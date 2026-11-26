//! Software-only Windows media fixtures. Never selected by a recording session.
#![allow(clippy::expect_used, clippy::panic)] // This entire module is compiled only as a test fixture.
use super::*;
use crate::{time, types::*};
use std::mem::ManuallyDrop;
use windows::Win32::{
  Media::MediaFoundation::*,
  System::{Com::*, Variant::VARIANT},
};
use windows::core::Interface;
pub struct MediaFixture {
  pub video: Vec<EncodedPacket>,
  pub audio: Vec<EncodedPacket>,
  pub video_extra: Vec<u8>,
  pub audio_extra: Vec<u8>,
}
unsafe fn pull(transform: &IMFTransform, packets: &mut Vec<EncodedPacket>) {
  for _ in 0..1024 {
    let info = unsafe { transform.GetOutputStreamInfo(0) }.expect("software H264 output info");
    assert!(info.cbSize < 16 * 1024 * 1024);
    let sample = unsafe { MFCreateSample() }.expect("output sample");
    let buffer = unsafe { MFCreateMemoryBuffer(info.cbSize.max(4096)) }.expect("output buffer");
    unsafe { sample.AddBuffer(&buffer) }.expect("output attach");
    let mut outputs = [MFT_OUTPUT_DATA_BUFFER {
      dwStreamID: 0,
      pSample: ManuallyDrop::new(Some(sample)),
      dwStatus: 0,
      pEvents: ManuallyDrop::new(None),
    }];
    let mut status = 0;
    let result = unsafe { transform.ProcessOutput(0, &mut outputs, &mut status) };
    let sample = unsafe { ManuallyDrop::take(&mut outputs[0].pSample) };
    drop(unsafe { ManuallyDrop::take(&mut outputs[0].pEvents) });
    if result
      .as_ref()
      .is_err_and(|e| e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT)
    {
      return;
    }
    result.expect("software H264 encode");
    let sample = sample.expect("software H264 sample");
    let pts = unsafe { sample.GetSampleTime() }.expect("encoded PTS");
    let duration = unsafe { sample.GetSampleDuration() }.expect("encoded duration");
    packets.push(EncodedPacket {
      track: Track::Video,
      pts,
      dts: pts,
      duration: duration as u64,
      keyframe: unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0,
      data: mux::sample_bytes(&sample).expect("encoded bytes"),
    });
  }
  panic!("software fixture encoder did not quiesce");
}
pub fn media(seconds: u32) -> MediaFixture {
  media_sized(seconds, 512, 64)
}
/// Numbered 60 FPS fixture at an explicit even size; heights off a 16-row macroblock exercise padding.
pub fn media_sized(seconds: u32, width: usize, height: usize) -> MediaFixture {
  assert!((1..=8).contains(&seconds));
  assert!(width >= 512 && height >= 16 && width.is_multiple_of(2) && height.is_multiple_of(2));
  let transform: IMFTransform =
    unsafe { CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER) }
      .expect("Windows software H264 fixture encoder");
  let codec: ICodecAPI = transform.cast().expect("software codec controls");
  unsafe { codec.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32)) }
    .expect("B frames disabled");
  let output = mux::video_type(width as u32, height as u32, 60, &[]).expect("H264 type");
  unsafe { output.SetUINT32(&MF_MT_AVG_BITRATE, 2_000_000) }.expect("fixture bitrate");
  unsafe { transform.SetOutputType(0, &output, 0) }.expect("H264 output type");
  let input = mux::media_type(&MFMediaType_Video, &MFVideoFormat_NV12).expect("NV12 input type");
  unsafe {
    input
      .SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)
      .expect("size");
    input
      .SetUINT64(&MF_MT_FRAME_RATE, (60u64 << 32) | 1)
      .expect("rate");
    input
      .SetUINT32(&MF_MT_INTERLACE_MODE, 2)
      .expect("progressive");
    transform.SetInputType(0, &input, 0).expect("NV12 input");
    transform
      .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
      .expect("begin");
    transform
      .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
      .expect("start");
  }
  let mut video = Vec::new();
  let mut image = vec![128u8; width * height * 3 / 2];
  for index in 0..seconds * 60 {
    let bright = index % 60 == 30;
    image[..width * height].fill(if bright { 200 } else { 42 });
    for bit in 0..32 {
      for y in 0..16 {
        let x = bit * 16;
        image[y * width + x..y * width + x + 16].fill(if index & (1 << bit) != 0 {
          235
        } else {
          16
        });
      }
    }
    let pts = time::frame_time(u64::from(index), 60).expect("frame time");
    let end = time::frame_time(u64::from(index + 1), 60).expect("frame end");
    let sample = mux::sample(&image, pts as i64, end - pts).expect("raw fixture sample");
    unsafe { transform.ProcessInput(0, &sample, 0) }.expect("fixture input");
    unsafe {
      pull(&transform, &mut video);
    }
  }
  unsafe {
    transform
      .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
      .expect("end");
    transform
      .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
      .expect("drain");
    pull(&transform, &mut video);
  }
  assert_eq!(video.len(), seconds as usize * 60);
  let video_extra = mft::parameter_sets(&video[0].data).expect("fixture SPS/PPS");
  let mut encoder = audio::AacEncoder::new().expect("AAC fixture");
  let audio_extra = encoder.extra.clone();
  let mut audio = Vec::new();
  let mut pcm = vec![0i16; 960];
  for block in 0..seconds * 100 {
    for i in 0..480 {
      let at = u64::from(block) * 480 + i as u64;
      let phase = at % 48000;
      let v = if (24000..24960).contains(&phase) {
        ((at as f64 * std::f64::consts::TAU * 1000.0 / 48000.0).sin() * 16000.0) as i16
      } else {
        0
      };
      pcm[i * 2] = v;
      pcm[i * 2 + 1] = v;
    }
    encoder
      .input(&pcm, u64::from(block) * 480)
      .expect("AAC input");
    while let Some(packet) = encoder.output().expect("AAC packet") {
      audio.push(packet);
    }
  }
  encoder.drain().expect("AAC drain");
  while let Some(packet) = encoder.output().expect("AAC tail") {
    audio.push(packet);
  }
  MediaFixture {
    video,
    audio,
    video_extra,
    audio_extra,
  }
}
