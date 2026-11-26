//! Synthetic AAC round trip on Windows software MFTs, without an audio endpoint.
use super::{audio::AacEncoder, mux};
use crate::{EngineError, Result, time, types::EncodedPacket};
use serde::{Deserialize, Serialize};
use std::mem::ManuallyDrop;
use windows::Win32::{Media::MediaFoundation::*, System::Com::*};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Calibration {
  pub input_frames: u64,
  pub decoded_frames: u64,
  pub encoded_packets: u64,
  pub first_packet_pts: i64,
  pub decoded_onset_frame: i64,
  pub onset_error_frames: i64,
  pub decoded_padding_frames: i64,
  pub timestamp_adjustment_frames: i64,
}
fn decoded(transform: &IMFTransform, frames: &mut u64, onset: &mut Option<i64>) -> Result<()> {
  for _ in 0..128 {
    // SAFETY: synchronous software decoder; bounded output buffer follows its stream contract.
    unsafe {
      let info = transform
        .GetOutputStreamInfo(0)
        .map_err(|e| EngineError::native("AAC calibration output info", e))?;
      if info.cbSize > 1024 * 1024 {
        return Err(EngineError::protocol("AAC calibration output too large"));
      }
      let sample =
        MFCreateSample().map_err(|e| EngineError::native("AAC calibration sample", e))?;
      let buffer = MFCreateMemoryBuffer(info.cbSize.max(8192))
        .map_err(|e| EngineError::native("AAC calibration buffer", e))?;
      sample
        .AddBuffer(&buffer)
        .map_err(|e| EngineError::native("AAC calibration attach", e))?;
      let mut outputs = [MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: 0,
        pSample: ManuallyDrop::new(Some(sample)),
        dwStatus: 0,
        pEvents: ManuallyDrop::new(None),
      }];
      let mut status = 0;
      let result = transform.ProcessOutput(0, &mut outputs, &mut status);
      let sample = ManuallyDrop::take(&mut outputs[0].pSample);
      drop(ManuallyDrop::take(&mut outputs[0].pEvents));
      match result {
        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
        Err(e) => return Err(EngineError::native("AAC calibration decode", e)),
        Ok(()) => {},
      }
      let sample = sample.ok_or_else(|| EngineError::protocol("AAC calibration missing sample"))?;
      let pts = sample
        .GetSampleTime()
        .map_err(|e| EngineError::native("AAC calibration time", e))?;
      let bytes = mux::sample_bytes(&sample)?;
      if !bytes.len().is_multiple_of(4) {
        return Err(EngineError::protocol("AAC calibration PCM not aligned"));
      }
      for (i, frame) in bytes.as_chunks::<4>().0.iter().enumerate() {
        if onset.is_none() && i16::from_le_bytes([frame[0], frame[1]]).unsigned_abs() > 3000 {
          *onset = Some((i128::from(pts) * 48000 / i128::from(time::SECOND)) as i64 + i as i64);
        }
      }
      *frames += (bytes.len() / 4) as u64;
    }
  }
  Err(EngineError::protocol(
    "AAC calibration output did not quiesce",
  ))
}
fn decode_packet(
  decoder: &IMFTransform,
  packet: EncodedPacket,
  frames: &mut u64,
  onset: &mut Option<i64>,
) -> Result<()> {
  let sample = mux::sample(&packet.data, packet.pts, packet.duration)?;
  // SAFETY: compressed AAC sample matches the decoder's negotiated ASC and stays alive during the call.
  unsafe { decoder.ProcessInput(0, &sample, 0) }
    .map_err(|e| EngineError::native("AAC calibration input", e))?;
  decoded(decoder, frames, onset)
}
pub fn calibrate() -> Result<Calibration> {
  let _com = super::Apartment::new()?;
  let _mf = super::MediaRuntime::new()?;
  let mut encoder = AacEncoder::new()?;
  // SAFETY: caller owns MTA/MF; both software transforms remain on this thread, with no rendering sink.
  let decoder: IMFTransform =
    unsafe { CoCreateInstance(&CLSID_MSAACDecMFT, None, CLSCTX_INPROC_SERVER) }
      .map_err(|e| EngineError::native("AAC calibration decoder", e))?;
  let input = mux::audio_type(true)?;
  unsafe {
    input
      .SetBlob(&MF_MT_USER_DATA, &encoder.extra)
      .map_err(|e| EngineError::native("AAC calibration ASC", e))?;
    decoder
      .SetInputType(0, &input, 0)
      .map_err(|e| EngineError::native("AAC calibration compressed type", e))?;
    decoder
      .SetOutputType(0, &mux::audio_type(false)?, 0)
      .map_err(|e| EngineError::native("AAC calibration PCM type", e))?;
    decoder
      .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
      .map_err(|e| EngineError::native("AAC calibration begin", e))?;
    decoder
      .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
      .map_err(|e| EngineError::native("AAC calibration start", e))?;
  }
  let mut pcm = vec![0; 960];
  let mut frames = 0;
  let mut onset = None;
  let mut packets = 0;
  let mut first_pts = None;
  for block in 0..150u64 {
    for i in 0..480 {
      let at = block * 480 + i as u64;
      let value = if (24000..24960).contains(&at) {
        ((at as f64 * std::f64::consts::TAU * 1000.0 / 48000.0).sin() * 16000.0) as i16
      } else {
        0
      };
      pcm[i * 2] = value;
      pcm[i * 2 + 1] = value;
    }
    encoder.input(&pcm, block * 480)?;
    while let Some(packet) = encoder.output()? {
      first_pts.get_or_insert(packet.pts);
      packets += 1;
      decode_packet(&decoder, packet, &mut frames, &mut onset)?;
    }
  }
  encoder.drain()?;
  while let Some(packet) = encoder.output()? {
    first_pts.get_or_insert(packet.pts);
    packets += 1;
    decode_packet(&decoder, packet, &mut frames, &mut onset)?;
  }
  // SAFETY: input ended; drain only the same privately owned software decoder.
  unsafe {
    decoder
      .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
      .and_then(|_| decoder.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0))
  }
  .map_err(|e| EngineError::native("AAC calibration drain", e))?;
  decoded(&decoder, &mut frames, &mut onset)?;
  let onset = onset.ok_or_else(|| EngineError::protocol("AAC calibration tone missing"))?;
  let error = onset - 24000;
  if error.abs() > 48 {
    return Err(EngineError::protocol(format!(
      "AAC calibration delay {error} frames exceeds one millisecond; no guessed correction applied"
    )));
  }
  Ok(Calibration {
    input_frames: 72000,
    decoded_frames: frames,
    encoded_packets: packets,
    first_packet_pts: first_pts
      .ok_or_else(|| EngineError::protocol("No AAC calibration packets"))?,
    decoded_onset_frame: onset,
    onset_error_frames: error,
    decoded_padding_frames: frames as i64 - 72000,
    timestamp_adjustment_frames: 0,
  })
}
#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  #[test]
  fn native_aac_round_trip_has_measured_delay_without_playback() {
    let c = calibrate().expect("Windows software AAC calibration");
    assert!(c.encoded_packets > 60);
    assert!(c.onset_error_frames.abs() <= 48);
    assert_eq!(c.timestamp_adjustment_frames, 0);
    assert!(c.decoded_padding_frames.abs() < 2048);
  }
}
