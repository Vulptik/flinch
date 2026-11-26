//! The microphones Windows offers, for the settings picker, and a level meter that proves the
//! chosen one hears something before a recording depends on it.
use super::{AudioCapture, Endpoint};
use crate::{EngineError, Result};
use std::sync::{
  Arc,
  atomic::{AtomicBool, AtomicU32, Ordering},
};
use windows::{
  Win32::{
    Foundation::PROPERTYKEY,
    Media::Audio::*,
    System::{
      Com::{StructuredStorage::PropVariantClear, *},
      Variant::VT_LPWSTR,
    },
  },
  core::GUID,
};

/// `PKEY_Device_FriendlyName`: the name Windows shows in Sound settings.
const FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
  fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
  pid: 14,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Microphone {
  /// The endpoint id a recording opens.
  pub id: String,
  /// Its name in Windows' Sound settings, such as "Microphone (USB Audio)".
  pub name: String,
}

/// Connected microphones, Windows' default first, then by name. Runs on its own thread so any
/// caller (an STA UI thread included) can ask.
pub fn microphones() -> Result<Vec<Microphone>> {
  std::thread::Builder::new()
    .name("microphone list".into())
    .spawn(|| {
      let _com = super::super::Apartment::new()?;
      list()
    })
    .map_err(|e| EngineError::io("microphone list thread", e))?
    .join()
    .map_err(|_| EngineError::protocol("Microphone list thread panicked"))?
}

fn list() -> Result<Vec<Microphone>> {
  // SAFETY: this thread owns an initialized MTA; every interface stays on it.
  unsafe {
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
      .map_err(|e| EngineError::native("audio devices", e))?;
    // No microphone at all is not an error: the list is simply empty.
    let default = enumerator
      .GetDefaultAudioEndpoint(eCapture, eConsole)
      .ok()
      .and_then(|device| endpoint_id(&device).ok());
    let devices = enumerator
      .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
      .map_err(|e| EngineError::native("microphone list", e))?;
    let count = devices
      .GetCount()
      .map_err(|e| EngineError::native("microphone count", e))?;
    let mut found = Vec::with_capacity(count as usize);
    for index in 0..count.min(64) {
      let device = devices
        .Item(index)
        .map_err(|e| EngineError::native("microphone", e))?;
      let id = endpoint_id(&device)?;
      let name = friendly_name(&device).unwrap_or_else(|_| id.clone());
      found.push(Microphone { id, name });
    }
    Ok(ordered(found, default.as_deref()))
  }
}

/// The default microphone first, the rest by name (then id, so equal names keep one order).
fn ordered(mut found: Vec<Microphone>, default: Option<&str>) -> Vec<Microphone> {
  found.sort_by(|a, b| {
    let first = |mic: &Microphone| Some(mic.id.as_str()) != default;
    (first(a), a.name.to_lowercase(), &a.id).cmp(&(first(b), b.name.to_lowercase(), &b.id))
  });
  found
}

fn endpoint_id(device: &IMMDevice) -> Result<String> {
  // SAFETY: GetId returns a CoTaskMem string this function frees after copying it.
  unsafe {
    let id = device
      .GetId()
      .map_err(|e| EngineError::native("microphone id", e))?;
    let text = id.to_string();
    CoTaskMemFree(Some(id.0.cast()));
    text.map_err(|_| EngineError::protocol("Microphone id is not valid UTF-16"))
  }
}

fn friendly_name(device: &IMMDevice) -> Result<String> {
  // SAFETY: a read-only property store; the PROPVARIANT is cleared once below.
  unsafe {
    let store = device
      .OpenPropertyStore(STGM_READ)
      .map_err(|e| EngineError::native("microphone properties", e))?;
    let mut value = store
      .GetValue(&FRIENDLY_NAME)
      .map_err(|e| EngineError::native("microphone name", e))?;
    let fields = &value.Anonymous.Anonymous;
    let name = if fields.vt == VT_LPWSTR && !fields.Anonymous.pwszVal.is_null() {
      fields.Anonymous.pwszVal.to_string().ok()
    } else {
      None
    };
    // Clearing only frees the copy Windows made; a failure leaves nothing to recover.
    let _ = PropVariantClear(&mut value);
    name
      .filter(|name| !name.trim().is_empty())
      .ok_or_else(|| EngineError::protocol("Microphone has no name"))
  }
}

/// A live input level for one microphone, read while the settings page shows it. It opens the
/// microphone like a recording would (Windows shows its microphone-in-use icon meanwhile) and
/// closes it when dropped.
pub struct LevelMeter {
  stop: Arc<AtomicBool>,
  /// The peak since the last read, as f32 bits.
  peak: Arc<AtomicU32>,
  failure: Arc<std::sync::Mutex<Option<EngineError>>>,
  thread: Option<std::thread::JoinHandle<()>>,
}
impl LevelMeter {
  /// Opens `id` (`None`: the Windows default microphone). Fails when it cannot be opened, with the
  /// stages `MICROPHONE_BLOCKED_STAGE` or `MICROPHONE_MISSING_STAGE` where they apply.
  pub fn open(id: Option<String>) -> Result<Self> {
    let capture = AudioCapture::start(Endpoint::Microphone, id)?;
    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicU32::new(0));
    let failure = Arc::new(std::sync::Mutex::new(None));
    let (stopping, level, failed) = (stop.clone(), peak.clone(), failure.clone());
    let thread = std::thread::Builder::new()
      .name("microphone level".into())
      .spawn(move || {
        while !stopping.load(Ordering::Acquire) {
          match capture.try_packet() {
            Ok(Some(packet)) => {
              let loudest = packet
                .data
                .iter()
                .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
              level.fetch_max(loudest.min(1.0).to_bits(), Ordering::AcqRel);
            },
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(error) => {
              if let Ok(mut slot) = failed.lock() {
                *slot = Some(error);
              }
              break;
            },
          }
        }
        if let Err(error) = capture.finish() {
          eprintln!("Microphone level meter: {error}");
        }
      })
      .map_err(|e| EngineError::io("microphone level thread", e))?;
    Ok(Self {
      stop,
      peak,
      failure,
      thread: Some(thread),
    })
  }
  /// The loudest sample since the last call, 0 to 1. Non-negative f32 bits order like the values,
  /// so the thread can keep the maximum with an integer `fetch_max`.
  pub fn take_peak(&self) -> f32 {
    f32::from_bits(self.peak.swap(0, Ordering::AcqRel))
  }
  /// Why the meter stopped, once: the microphone was unplugged or failed.
  pub fn failure(&self) -> Option<EngineError> {
    self.failure.lock().ok().and_then(|mut slot| slot.take())
  }
}
impl Drop for LevelMeter {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Release);
    // The loop checks the flag at least every 10 ms and the capture shutdown is bounded.
    if let Some(thread) = self.thread.take()
      && thread.join().is_err()
    {
      eprintln!("Microphone level thread panicked");
    }
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;

  fn mic(id: &str, name: &str) -> Microphone {
    Microphone {
      id: id.into(),
      name: name.into(),
    }
  }

  #[test]
  fn the_default_microphone_comes_first_and_the_rest_by_name() {
    let list = ordered(
      vec![
        mic("c", "Zeta"),
        mic("a", "alpha"),
        mic("d", "Headset"),
        mic("b", "Beta"),
      ],
      Some("d"),
    );
    let names: Vec<_> = list.iter().map(|mic| mic.name.as_str()).collect();
    assert_eq!(names, ["Headset", "alpha", "Beta", "Zeta"]);
    let none = ordered(vec![mic("b", "Same"), mic("a", "Same")], None);
    assert_eq!(none[0].id, "a", "equal names keep a stable order");
  }

  #[test]
  fn peak_bits_order_like_levels() {
    let mut previous = 0u32;
    for step in 0..=100 {
      let bits = (step as f32 / 100.0).to_bits();
      assert!(bits >= previous);
      previous = bits;
    }
  }

  /// Reads this machine's microphones, so what it reaches depends on what is plugged in: kept
  /// out of quiet runs, where coverage must match on every machine.
  #[test]
  #[ignore = "Explicit read of this machine's microphone list; no audio capture"]
  fn listing_microphones_never_fails_on_this_machine() {
    // A machine without a microphone lists none; every listed one has a name and an id.
    let list = microphones().expect("list");
    assert!(
      list
        .iter()
        .all(|mic| !mic.id.is_empty() && !mic.name.is_empty())
    );
  }
}
