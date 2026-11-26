//! Vendor encoders prove themselves before Prepare commits to them: a few frames must encode into
//! well-formed H.264 within a bounded time. Any failure is a capability result, so automatic
//! selection moves on (to the software encoder) instead of starting a recording that could fail
//! midway.
//!
//! Costs one extra encoder open and three frames during Prepare, for every vendor encoder (AMF,
//! NVENC, QSV); the software encoder is the last tier and has nothing to fall back to. The
//! self-test stays even on tested hardware: a test covers one GPU generation and driver, and a
//! later driver or untested GPU must still fall back to software rather than
//! fail a recording.
use super::{
  encoding::Codec,
  mft::{self, NativeEncoder},
};
use crate::{EngineError, ErrorKind, Result, time, types::*};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::{
  Direct3D11::*,
  Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC},
};

pub(super) const FRAMES: u32 = 3;
const DEADLINE: Duration = Duration::from_secs(2);

/// Every vendor encoder automatic selection can pick ahead of software; kept permanently.
pub(super) fn required(encoder: EncoderKind) -> bool {
  matches!(
    encoder,
    EncoderKind::Amf | EncoderKind::Nvenc | EncoderKind::Qsv
  )
}

/// Opens a throwaway instance and encodes [`FRAMES`] frames of a blank surface.
pub(super) fn run(
  device: &ID3D11Device,
  luid: i64,
  width: u32,
  height: u32,
  spec: &SessionSpec,
) -> Result<()> {
  let texture = surface(device, width, height)?;
  let mut codec = NativeEncoder::open(device, luid, width, height, spec, ConversionKind::Gpu)?;
  exercise(&mut codec, &texture, spec.fps, DEADLINE)
    .map(|_| ())
    .map_err(|error| refuse(spec.encoder, error))
}

/// A self-test failure keeps its cause but always lets selection fall back.
fn refuse(encoder: EncoderKind, error: EngineError) -> EngineError {
  EngineError {
    native_code: error.native_code,
    ..EngineError::new(
      ErrorKind::Capability,
      "encoder self-test",
      format!(
        "{encoder:?} failed its self-test: {}: {}",
        error.stage, error.reason
      ),
    )
  }
}

/// The same format and binding as the session's GPU surfaces.
fn surface(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11Texture2D> {
  let desc = D3D11_TEXTURE2D_DESC {
    Width: width,
    Height: height,
    MipLevels: 1,
    ArraySize: 1,
    Format: DXGI_FORMAT_NV12,
    SampleDesc: DXGI_SAMPLE_DESC {
      Count: 1,
      Quality: 0,
    },
    Usage: D3D11_USAGE_DEFAULT,
    BindFlags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_VIDEO_ENCODER).0 as u32,
    ..Default::default()
  };
  let mut texture = None;
  // SAFETY: plain default-usage texture on the caller's device; no initial data.
  unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
    .map_err(|e| EngineError::native("self-test surface", e))?;
  texture.ok_or_else(|| EngineError::protocol("No self-test surface"))
}

fn nal_types(bytes: &[u8]) -> Vec<u8> {
  mft::nal_units(bytes)
    .iter()
    .filter_map(|unit| unit.first().map(|header| header & 0x1f))
    .collect()
}

fn expired(started: Instant, deadline: Duration, step: &str) -> Result<()> {
  if started.elapsed() > deadline {
    return Err(EngineError::new(
      ErrorKind::Timeout,
      "encoder self-test",
      format!(
        "No progress within {} ms while {step}",
        deadline.as_millis()
      ),
    ));
  }
  Ok(())
}

/// Encodes frames one at a time through slot 0 (a texture is registered once per slot), then
/// drains. Returns the packets' total size.
pub(super) fn exercise<C: Codec>(
  codec: &mut C,
  texture: &ID3D11Texture2D,
  fps: u32,
  deadline: Duration,
) -> Result<usize> {
  let headers = nal_types(codec.extra());
  if !headers.contains(&7) || !headers.contains(&8) {
    return Err(EngineError::protocol(
      "Parameter sets lack an SPS and a PPS",
    ));
  }
  let started = Instant::now();
  let duration = time::frame_time(1, fps)?;
  let mut packets = Vec::new();
  let wait = || std::thread::sleep(Duration::from_millis(1));
  for frame in 0..FRAMES {
    let pts = time::frame_time(u64::from(frame), fps)?;
    while codec.busy(0) {
      expired(started, deadline, "waiting for the surface")?;
      if let Some(packet) = codec.poll()? {
        packets.push(packet);
      }
      wait();
    }
    while !codec.submit(texture, 0, pts, duration, frame == 0)? {
      expired(started, deadline, "submitting")?;
      if let Some(packet) = codec.poll()? {
        packets.push(packet);
      }
      wait();
    }
    while packets.len() <= frame as usize {
      expired(started, deadline, "waiting for output")?;
      match codec.poll()? {
        Some(packet) => packets.push(packet),
        None => wait(),
      }
    }
  }
  while !codec.drain()? {
    expired(started, deadline, "draining")?;
    if let Some(packet) = codec.poll()? {
      packets.push(packet);
    }
    wait();
  }
  while !codec.drained() {
    expired(started, deadline, "finishing")?;
    match codec.poll()? {
      Some(packet) => packets.push(packet),
      None => wait(),
    }
  }
  if packets.len() != FRAMES as usize {
    return Err(EngineError::protocol(format!(
      "{} packets for {FRAMES} frames",
      packets.len()
    )));
  }
  let mut size = 0;
  for (index, packet) in packets.iter().enumerate() {
    let expected = i64::try_from(time::frame_time(index as u64, fps)?)
      .map_err(|_| EngineError::protocol("Self-test timestamp overflow"))?;
    if packet.pts != expected || packet.dts != packet.pts || packet.duration != duration {
      return Err(EngineError::protocol(format!(
        "Packet {index} is timed {}+{} (DTS {}), expected {expected}+{duration}",
        packet.pts, packet.duration, packet.dts
      )));
    }
    let types = nal_types(&packet.data);
    let picture = if index == 0 { 5 } else { 1 };
    // Frame 0 must be an IDR; later frames may be IDR or non-IDR slices.
    if !types.contains(&picture) && !types.contains(&5) {
      return Err(EngineError::protocol(format!(
        "Packet {index} has no picture slice (NAL types {types:?})"
      )));
    }
    if index == 0 && !packet.keyframe {
      return Err(EngineError::protocol("First packet is not a keyframe"));
    }
    size += packet.data.len();
  }
  Ok(size)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests;
