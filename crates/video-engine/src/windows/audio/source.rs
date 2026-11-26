//! Fixed endpoint identity and physical/mix/client format provenance, queried without starting audio.
use super::*;
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
  #[default]
  PcmInteger,
  PcmFloat,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AudioSourceInfo {
  pub device_id: String,
  pub device_rate: u32,
  pub mix_rate: u32,
  pub mix_channels: u16,
  pub mix_bits: u16,
  pub mix_valid_bits: u16,
  pub mix_format_tag: u16,
  pub mix_channel_mask: u32,
  pub mix_encoding: Encoding,
  pub client_rate: u32,
  pub client_channels: u16,
  pub client_bits: u16,
  pub packet_capacity_frames: u32,
  #[serde(default)]
  pub timestamp_mode: String,
  #[serde(default)]
  pub packet_position_rate: u32,
}
fn parse(format: WAVEFORMATEX, extended: Option<WAVEFORMATEXTENSIBLE>) -> Result<AudioSourceInfo> {
  let channels = format.nChannels;
  let bits = format.wBitsPerSample;
  let rate = format.nSamplesPerSec;
  if !(1..=2).contains(&channels) {
    return Err(EngineError::new(
      ErrorKind::Capability,
      "audio format",
      "Prototype accepts mono or stereo render endpoints",
    ));
  }
  let mut mask = 0;
  let mut valid_bits = bits;
  let tag = if format.wFormatTag == 0xfffe {
    let extended =
      extended.ok_or_else(|| EngineError::protocol("Truncated extensible mix format"))?;
    mask = extended.dwChannelMask;
    // SAFETY: WAVEFORMATEXTENSIBLE supplies this valid-bits union for its PCM/float subtype.
    valid_bits = unsafe { extended.Samples.wValidBitsPerSample };
    let subtype = extended.SubFormat;
    if subtype == MFAudioFormat_PCM {
      1
    } else if subtype == MFAudioFormat_Float {
      3
    } else {
      0
    }
  } else {
    format.wFormatTag
  };
  let encoding = match tag {
    1 if matches!(bits, 8 | 16 | 24 | 32) => Encoding::PcmInteger,
    3 if matches!(bits, 32 | 64) => Encoding::PcmFloat,
    _ => {
      return Err(EngineError::new(
        ErrorKind::Capability,
        "audio format",
        "Unsupported render mix PCM representation",
      ));
    },
  };
  if !(8000..=384000).contains(&rate)
    || valid_bits > bits
    || format.nBlockAlign != channels * (bits / 8)
    || format.nAvgBytesPerSec != format.nSamplesPerSec * u32::from(format.nBlockAlign)
  {
    return Err(EngineError::protocol("Invalid render mix PCM extent"));
  }
  if mask != 0 && mask != if channels == 1 { 4 } else { 3 } {
    return Err(EngineError::new(
      ErrorKind::Capability,
      "audio layout",
      "Only mono center and stereo front-left/front-right layouts are supported",
    ));
  }
  Ok(AudioSourceInfo {
    mix_rate: format.nSamplesPerSec,
    mix_channels: channels,
    mix_bits: bits,
    mix_valid_bits: valid_bits,
    mix_format_tag: format.wFormatTag,
    mix_channel_mask: mask,
    mix_encoding: encoding,
    client_rate: 48000,
    client_channels: 2,
    client_bits: 16,
    timestamp_mode: "wasapi_capture_packet_qpc_v1".into(),
    // GetBuffer positions count the shared engine's mix frames, not the 48 kHz client stream:
    // a 44.1 kHz VirtualBox endpoint advanced 447 positions per 487-frame packet.
    packet_position_rate: format.nSamplesPerSec,
    ..Default::default()
  })
}
impl AudioSourceInfo {
  pub(super) fn query(device: &IMMDevice, client: &IAudioClient, device_rate: u32) -> Result<Self> {
    // SAFETY: the client and endpoint remain on this apartment; the returned mix allocation is freed once below.
    let mix = unsafe { client.GetMixFormat() }
      .map_err(|error| EngineError::native("audio mix format", error))?;
    if mix.is_null() {
      return Err(EngineError::protocol("Null audio mix format"));
    }
    // SAFETY: a successful GetMixFormat owns at least its WAVEFORMATEX header.
    let format = unsafe { mix.read_unaligned() };
    let extended = if format.wFormatTag == 0xfffe && format.cbSize >= 22 {
      // SAFETY: the API-provided extension length contains a complete WAVEFORMATEXTENSIBLE.
      Some(unsafe { mix.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() })
    } else {
      None
    };
    let parsed = parse(format, extended);
    // SAFETY: no references into the mix allocation escape the copied format parse.
    unsafe { CoTaskMemFree(Some(mix.cast())) };
    let mut info = parsed?;
    // SAFETY: GetId transfers a COM-task allocation containing a terminated endpoint identifier.
    let id = unsafe { device.GetId() }
      .map_err(|error| EngineError::native("audio endpoint identity", error))?;
    if id.is_null() {
      return Err(EngineError::protocol("Null endpoint identifier"));
    }
    let result = (|| {
      for count in 0..4096 {
        // SAFETY: native GetId guarantees a valid terminated UTF-16 string; the loop bounds retained data.
        if unsafe { *id.0.add(count) } == 0 {
          // SAFETY: the prefix has been read and remains in the live native identifier allocation.
          return String::from_utf16(unsafe { std::slice::from_raw_parts(id.0, count) }).map_err(
            |error| EngineError::protocol(format!("Invalid endpoint identifier: {error}")),
          );
        }
      }
      Err(EngineError::protocol(
        "Endpoint identifier exceeds 4096 UTF-16 units",
      ))
    })();
    // SAFETY: GetId's unique owning allocation is released after the owned string has been copied.
    unsafe { CoTaskMemFree(Some(id.0.cast())) };
    info.device_id = result?;
    if info.device_id.is_empty() {
      return Err(EngineError::protocol("Empty endpoint identifier"));
    }
    info.device_rate = device_rate;
    Ok(info)
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  fn format() -> WAVEFORMATEX {
    WAVEFORMATEX {
      wFormatTag: 1,
      nChannels: 2,
      nSamplesPerSec: 48000,
      nAvgBytesPerSec: 192000,
      nBlockAlign: 4,
      wBitsPerSample: 16,
      cbSize: 0,
    }
  }
  #[test]
  fn malformed_and_unknown_mix_formats_are_explicit_and_cannot_change_the_client_format() {
    let original = format();
    assert_eq!(parse(original, None).expect("integer mix").client_bits, 16);
    let mut sample = original;
    sample.nBlockAlign = 3;
    assert!(parse(sample, None).is_err());
    sample = original;
    sample.nSamplesPerSec = 0;
    assert!(parse(sample, None).is_err());
    sample = original;
    sample.wFormatTag = 0xfffe;
    sample.cbSize = 22;
    assert!(parse(sample, None).is_err());
    let mut extended = WAVEFORMATEXTENSIBLE {
      Format: sample,
      Samples: WAVEFORMATEXTENSIBLE_0 {
        wValidBitsPerSample: 17,
      },
      dwChannelMask: 3,
      SubFormat: MFAudioFormat_PCM,
    };
    assert!(parse(sample, Some(extended)).is_err());
    extended.Samples = WAVEFORMATEXTENSIBLE_0 {
      wValidBitsPerSample: 16,
    };
    extended.SubFormat = windows::core::GUID::zeroed();
    assert_eq!(
      parse(sample, Some(extended))
        .expect_err("compressed/unknown mix")
        .kind,
      ErrorKind::Capability
    );
    sample = original;
    sample.wFormatTag = 7;
    assert!(parse(sample, None).is_err());
  }
  #[test]
  fn packet_positions_follow_the_mix_rate_not_the_client_rate() {
    // Regression: a 44.1 kHz endpoint was reported (and dated) with 48 kHz positions.
    let mut mix = format();
    mix.nSamplesPerSec = 44100;
    mix.nAvgBytesPerSec = 44100 * 4;
    let info = parse(mix, None).expect("44.1 kHz mix");
    assert_eq!(
      (info.mix_rate, info.packet_position_rate, info.client_rate),
      (44100, 44100, 48000)
    );
  }
}
