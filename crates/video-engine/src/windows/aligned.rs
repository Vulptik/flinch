//! Write-time fragment alignment for the Media Foundation fragmented-MP4 sink.
//!
//! Windows players stop at a fragment whose `moof`/`mdat` headers straddle a 256 KiB read
//! (see [`crate::interop`]). This byte stream sits between the sink and the file. When the
//! sink writes a whole `moof` that would straddle, a `free` box is written first so the `moof`
//! starts exactly on the boundary. The sink keeps working in its own (virtual) positions; this
//! stream translates them to file positions and adds the inserted padding to the absolute
//! offsets the sink wrote (`tfhd` base data offsets, `tfra` entries). Stop then only scans.
//!
//! The sink's observed pattern: sequential `BeginWrite`s, each fragment as one
//! `moof` write, an 8-byte `mdat` header and its payload; at the end one seek back into
//! `moov` to patch `mvhd`, then `mfra`. Writes complete synchronously and are reported through
//! the sink's callback. A `moof` split across writes is not aligned here; finalization's
//! realignment still repairs that case.
use crate::interop;
use std::collections::HashMap;
use std::sync::Mutex;
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, E_POINTER, S_OK};
use windows::Win32::Media::MediaFoundation::*;
use windows::core::{Interface, Ref, implement};

/// Largest `mdat` header (64-bit size); the alignment window assumes it.
const MDAT_HEADER: u64 = 16;

#[derive(Default)]
struct State {
  /// Padding inserted before the fragment at each virtual position, in order.
  pads: Vec<(u64, u64)>,
  /// `moof` positions as (virtual, file) for `tfra` patching, in order.
  moofs: Vec<(u64, u64)>,
  /// Bytes reported by `EndWrite`, keyed by the async result each write completed with.
  completed: HashMap<usize, u32>,
}
impl State {
  fn padding(&self) -> u64 {
    self.pads.iter().map(|(_, pad)| pad).sum()
  }
  /// File position of a sink position: padding inserted at or before it moves it.
  fn real(&self, position: u64) -> u64 {
    position
      + self
        .pads
        .iter()
        .filter(|(at, _)| *at <= position)
        .map(|(_, pad)| pad)
        .sum::<u64>()
  }
  /// Sink position of a file position outside any padding.
  fn virtual_of(&self, real: u64) -> u64 {
    let mut shifted = 0;
    for (at, pad) in &self.pads {
      if at + shifted + pad <= real {
        shifted += pad;
      }
    }
    real - shifted
  }
}

#[implement(IMFByteStream)]
pub struct AlignedStream {
  inner: IMFByteStream,
  chunk: u64,
  state: Mutex<State>,
}
impl AlignedStream {
  pub fn wrap(inner: IMFByteStream, chunk: u64) -> IMFByteStream {
    Self {
      inner,
      chunk,
      state: Mutex::new(State::default()),
    }
    .into()
  }
  fn state(&self) -> windows::core::Result<std::sync::MutexGuard<'_, State>> {
    self
      .state
      .lock()
      .map_err(|_| windows::core::Error::new(E_FAIL, "aligned stream state poisoned"))
  }
  fn write_all(&self, bytes: &[u8]) -> windows::core::Result<()> {
    let mut at = 0;
    while at < bytes.len() {
      // SAFETY: synchronous write of a borrowed, initialized buffer to the owned file stream.
      let written = unsafe { self.inner.Write(&bytes[at..]) }? as usize;
      if written == 0 {
        return Err(windows::core::Error::new(
          E_FAIL,
          "file stream accepted no bytes",
        ));
      }
      at += written;
    }
    Ok(())
  }
  /// Write one sink buffer at the current position, aligning and patching as needed.
  fn write(&self, bytes: &[u8]) -> windows::core::Result<u32> {
    let length =
      u32::try_from(bytes.len()).map_err(|_| windows::core::Error::from(E_INVALIDARG))?;
    let mut state = self.state()?;
    // SAFETY: position query on the owned file stream.
    let mut real = unsafe { self.inner.GetCurrentPosition() }?;
    let position = state.virtual_of(real);
    match whole_box(bytes) {
      Some(kind) if &kind == b"moof" => {
        if interop::straddles_in(real, real + bytes.len() as u64 + MDAT_HEADER, self.chunk) {
          let pad = interop::boundary_after(real, self.chunk) - real;
          let mut free = vec![0u8; pad as usize];
          free[..4].copy_from_slice(&(pad as u32).to_be_bytes());
          free[4..8].copy_from_slice(b"free");
          self.write_all(&free)?;
          state.pads.push((position, pad));
          real += pad;
        }
        let mut moof = bytes.to_vec();
        interop::shift_moof(&mut moof, state.padding())
          .map_err(|error| windows::core::Error::new(E_FAIL, error.to_string()))?;
        state.moofs.push((position, real));
        self.write_all(&moof)?;
      },
      Some(kind) if &kind == b"mfra" && !state.pads.is_empty() => {
        let mut mfra = bytes.to_vec();
        interop::shift_mfra(&mut mfra, &state.moofs)
          .map_err(|error| windows::core::Error::new(E_FAIL, error.to_string()))?;
        self.write_all(&mfra)?;
      },
      _ => self.write_all(bytes)?,
    }
    Ok(length)
  }
}

/// The box type when `bytes` holds exactly one complete box with a 32-bit size.
fn whole_box(bytes: &[u8]) -> Option<[u8; 4]> {
  let size = u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
  (size == bytes.len()).then(|| bytes.get(4..8)?.try_into().ok())?
}

// COM vtable methods receive raw buffers by contract; each use checks null and documents why the
// caller's buffer is valid. The trait signatures are fixed by the interface.
#[allow(non_snake_case, clippy::not_unsafe_ptr_arg_deref)]
impl IMFByteStream_Impl for AlignedStream_Impl {
  fn GetCapabilities(&self) -> windows::core::Result<u32> {
    // SAFETY: delegated query on the owned file stream.
    unsafe { self.inner.GetCapabilities() }
  }
  fn GetLength(&self) -> windows::core::Result<u64> {
    // SAFETY: delegated query on the owned file stream.
    let real = unsafe { self.inner.GetLength() }?;
    Ok(self.state()?.virtual_of(real))
  }
  fn SetLength(&self, length: u64) -> windows::core::Result<()> {
    let real = self.state()?.real(length);
    // SAFETY: delegated call on the owned file stream.
    unsafe { self.inner.SetLength(real) }
  }
  fn GetCurrentPosition(&self) -> windows::core::Result<u64> {
    // SAFETY: delegated query on the owned file stream.
    let real = unsafe { self.inner.GetCurrentPosition() }?;
    Ok(self.state()?.virtual_of(real))
  }
  fn SetCurrentPosition(&self, position: u64) -> windows::core::Result<()> {
    let real = self.state()?.real(position);
    // SAFETY: delegated call on the owned file stream.
    unsafe { self.inner.SetCurrentPosition(real) }
  }
  fn IsEndOfStream(&self) -> windows::core::Result<windows::core::BOOL> {
    // SAFETY: delegated query on the owned file stream.
    unsafe { self.inner.IsEndOfStream() }
  }
  fn Read(&self, _: *mut u8, _: u32, _: *mut u32) -> windows::core::Result<()> {
    Err(windows::core::Error::new(
      E_FAIL,
      "the aligned MP4 stream is write-only",
    ))
  }
  fn BeginRead(
    &self,
    _: *mut u8,
    _: u32,
    _: Ref<IMFAsyncCallback>,
    _: Ref<windows::core::IUnknown>,
  ) -> windows::core::Result<()> {
    Err(windows::core::Error::new(
      E_FAIL,
      "the aligned MP4 stream is write-only",
    ))
  }
  fn EndRead(&self, _: Ref<IMFAsyncResult>) -> windows::core::Result<u32> {
    Err(windows::core::Error::new(
      E_FAIL,
      "the aligned MP4 stream is write-only",
    ))
  }
  fn Write(&self, buffer: *const u8, length: u32) -> windows::core::Result<u32> {
    if buffer.is_null() {
      return Err(E_POINTER.into());
    }
    // SAFETY: the caller guarantees `length` readable bytes for the duration of the call.
    self.write(unsafe { std::slice::from_raw_parts(buffer, length as usize) })
  }
  fn BeginWrite(
    &self,
    buffer: *const u8,
    length: u32,
    callback: Ref<IMFAsyncCallback>,
    state: Ref<windows::core::IUnknown>,
  ) -> windows::core::Result<()> {
    if buffer.is_null() {
      return Err(E_POINTER.into());
    }
    // SAFETY: the buffer stays valid until EndWrite; it is fully consumed before returning.
    let written = self.write(unsafe { std::slice::from_raw_parts(buffer, length as usize) })?;
    // SAFETY: a new async result owned here, completed once through the caller's callback.
    let result = unsafe { MFCreateAsyncResult(None, callback.as_ref(), state.as_ref()) }?;
    // SAFETY: status set on the live result before it is delivered.
    unsafe { result.SetStatus(S_OK) }?;
    self
      .state()?
      .completed
      .insert(result.as_raw() as usize, written);
    // SAFETY: delivers the completed result to the caller's callback on a work queue.
    let invoked = unsafe { MFInvokeCallback(&result) };
    if invoked.is_err() {
      self.state()?.completed.remove(&(result.as_raw() as usize));
    }
    invoked
  }
  fn EndWrite(&self, result: Ref<IMFAsyncResult>) -> windows::core::Result<u32> {
    let result = result.ok()?;
    // SAFETY: status query on the caller's live async result.
    unsafe { result.GetStatus() }?;
    self
      .state()?
      .completed
      .remove(&(result.as_raw() as usize))
      .ok_or_else(|| windows::core::Error::new(E_INVALIDARG, "unknown write result"))
  }
  fn Seek(
    &self,
    origin: MFBYTESTREAM_SEEK_ORIGIN,
    offset: i64,
    _flags: u32,
  ) -> windows::core::Result<u64> {
    let current = self.GetCurrentPosition()?;
    let target = if origin == msoBegin {
      offset
    } else if origin == msoCurrent {
      i64::try_from(current)
        .map_err(|_| windows::core::Error::from(E_INVALIDARG))?
        .checked_add(offset)
        .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?
    } else {
      return Err(E_INVALIDARG.into());
    };
    let target = u64::try_from(target).map_err(|_| windows::core::Error::from(E_INVALIDARG))?;
    self.SetCurrentPosition(target)?;
    Ok(target)
  }
  fn Flush(&self) -> windows::core::Result<()> {
    // SAFETY: delegated call on the owned file stream.
    unsafe { self.inner.Flush() }
  }
  fn Close(&self) -> windows::core::Result<()> {
    // SAFETY: delegated call on the owned file stream.
    unsafe { self.inner.Close() }
  }
}

#[cfg(test)]
#[path = "aligned/tests.rs"]
mod tests;
