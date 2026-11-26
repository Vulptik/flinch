//! The writing pass on every core, byte for byte the output of the one-thread reference.
//!
//! The decoder (the caller's thread, which owns Media Foundation) hands frames to a pool in
//! order. Error-diffusion dithering is serial within a frame and depends on the same row of the
//! previous frame, so frames overlap as a wavefront: a frame dithers row `y` once the previous
//! frame has published it. One collector plans the frames in order (cheap: the changed
//! rectangle), the pool LZW-compresses each rectangle, and the collector writes them in order.
//!
//! Memory is bounded by permits: a frame holds one from decoding until it is written or found
//! to add nothing. Every wait is timed, checks the shared failure flag and the pass deadline,
//! and the first error ends the pass on every thread.
use crate::gif::{Dither, Ditherer, GifEmitter, GifPlanner, GifReport, Palette, Rectangle};
use crate::{EngineError, ErrorKind, Result};
use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, mpsc};
use std::time::{Duration, Instant};

/// Decoded frames and their working copies kept in flight across the whole pipeline.
pub const PIPELINE_BYTES: usize = 256 << 20;
/// How long a blocked thread waits before rechecking for a failure or the deadline.
const POLL: Duration = Duration::from_millis(20);

/// One frame's indices, published a row at a time for the next frame's dithering.
struct Slot {
  rows: Vec<OnceLock<Box<[u8]>>>,
  published: Mutex<usize>,
  ready: Condvar,
}
impl Slot {
  fn new(height: usize) -> Self {
    Self {
      rows: (0..height).map(|_| OnceLock::new()).collect(),
      published: Mutex::new(0),
      ready: Condvar::new(),
    }
  }
  fn publish(&self, shared: &Shared, y: usize, row: Box<[u8]>) -> Result<()> {
    self
      .rows
      .get(y)
      .ok_or_else(|| EngineError::protocol("GIF row outside its frame"))?
      .set(row)
      .map_err(|_| EngineError::protocol("GIF row published twice"))?;
    *shared.lock(&self.published)? = y + 1;
    self.ready.notify_all();
    Ok(())
  }
  /// Row `y`, once published.
  fn row(&self, shared: &Shared, y: usize) -> Result<&[u8]> {
    let mut published = shared.lock(&self.published)?;
    while *published <= y {
      shared.alive()?;
      published = self
        .ready
        .wait_timeout(published, POLL)
        .map_err(|_| EngineError::protocol("GIF row lock poisoned"))?
        .0;
    }
    drop(published);
    self
      .rows
      .get(y)
      .and_then(OnceLock::get)
      .map(|row| &row[..])
      .ok_or_else(|| EngineError::protocol("GIF row missing after publication"))
  }
}

/// A decoded frame entering the pool, with the previous frame its dithering depends on.
struct Frame {
  number: u64,
  pts: u64,
  rgb: Arc<Vec<u8>>,
  slot: Arc<Slot>,
  previous: Option<(Arc<Vec<u8>>, Arc<Slot>)>,
}
enum Work {
  Dither(Frame),
  Compress(u64, Rectangle),
}
enum Done {
  Dithered(u64, u64, Vec<u8>),
  Compressed(u64, Rectangle),
  /// The decoder sent this many frames and will send no more.
  Decoded(u64),
}

struct Shared {
  queue: Mutex<VecDeque<Work>>,
  queued: Condvar,
  closed: AtomicBool,
  failed: AtomicBool,
  error: Mutex<Option<EngineError>>,
  permits: Mutex<usize>,
  released: Condvar,
  deadline: Instant,
}
impl Shared {
  fn lock<'a, T>(&self, mutex: &'a Mutex<T>) -> Result<MutexGuard<'a, T>> {
    mutex
      .lock()
      .map_err(|_| EngineError::protocol("GIF pipeline lock poisoned"))
  }
  /// Fails once another thread failed or the pass deadline passed.
  fn alive(&self) -> Result<()> {
    if self.failed.load(Ordering::Acquire) {
      return Err(EngineError::new(
        ErrorKind::Cancelled,
        "gif pipeline",
        "Another GIF stage failed",
      ));
    }
    if Instant::now() > self.deadline {
      return Err(EngineError::new(
        ErrorKind::Timeout,
        "gif decode",
        "Offline decode exceeded its duration budget",
      ));
    }
    Ok(())
  }
  /// Record the first real failure and wake every waiting thread.
  fn fail(&self, error: EngineError) {
    if let Ok(mut first) = self.error.lock()
      && first.is_none()
      && !self.failed.load(Ordering::Acquire)
    {
      *first = Some(error);
    }
    self.failed.store(true, Ordering::Release);
    self.queued.notify_all();
    self.released.notify_all();
  }
  fn push(&self, work: Work) -> Result<()> {
    self.lock(&self.queue)?.push_back(work);
    self.queued.notify_one();
    Ok(())
  }
  /// The next work in order, or `None` once the queue is closed and empty.
  fn take(&self) -> Result<Option<Work>> {
    let mut queue = self.lock(&self.queue)?;
    loop {
      if let Some(work) = queue.pop_front() {
        return Ok(Some(work));
      }
      if self.closed.load(Ordering::Acquire) {
        return Ok(None);
      }
      self.alive()?;
      queue = self
        .queued
        .wait_timeout(queue, POLL)
        .map_err(|_| EngineError::protocol("GIF queue lock poisoned"))?
        .0;
    }
  }
  fn acquire(&self) -> Result<()> {
    let mut permits = self.lock(&self.permits)?;
    while *permits == 0 {
      self.alive()?;
      permits = self
        .released
        .wait_timeout(permits, POLL)
        .map_err(|_| EngineError::protocol("GIF permit lock poisoned"))?
        .0;
    }
    *permits -= 1;
    Ok(())
  }
  fn release(&self) -> Result<()> {
    *self.lock(&self.permits)? += 1;
    self.released.notify_one();
    Ok(())
  }
}

/// Feeds decoded frames to the pipeline; returned by [`run`] to the decoding loop.
pub struct Feed<'a> {
  shared: &'a Shared,
  done: mpsc::Sender<Done>,
  width: usize,
  height: usize,
  next: u64,
  previous: Option<(Arc<Vec<u8>>, Arc<Slot>)>,
}
impl Feed<'_> {
  /// Queue one decoded frame (packed RGB) presented at `pts` (100 ns from the start).
  pub fn push(&mut self, rgb: Vec<u8>, pts: u64) -> Result<()> {
    if rgb.len() != self.width * self.height * 3 {
      return Err(EngineError::protocol(
        "Decoded picture size changed between GIF passes",
      ));
    }
    self.shared.alive()?;
    self.shared.acquire()?;
    let rgb = Arc::new(rgb);
    let slot = Arc::new(Slot::new(self.height));
    let frame = Frame {
      number: self.next,
      pts,
      rgb: rgb.clone(),
      slot: slot.clone(),
      previous: self.previous.replace((rgb, slot)),
    };
    self.next += 1;
    self.shared.push(Work::Dither(frame))
  }
}

/// How the writing pass runs.
pub struct Pass<'a> {
  pub palette: &'a Palette,
  pub dither: Dither,
  /// Threads that dither and compress (at least one).
  pub workers: usize,
  pub deadline: Instant,
  /// Memory for frames in flight (see [`PIPELINE_BYTES`]); at least three frames are allowed.
  pub budget: usize,
}

/// Run the writing pass: `feed` decodes into the given [`Feed`] and returns the recording's
/// end (100 ns); `wait` runs on this thread while the pipeline drains and may cancel it.
/// Returns the emitter's output and report.
pub fn run<W: Write + Send>(
  emitter: GifEmitter<W>,
  planner: GifPlanner,
  pass: Pass<'_>,
  feed: impl FnOnce(&mut Feed<'_>) -> Result<u64>,
  mut wait: impl FnMut() -> Result<()>,
) -> Result<(W, GifReport)> {
  let Pass {
    palette,
    dither,
    workers,
    deadline,
    budget,
  } = pass;
  let (width, height) = planner.size();
  let frame_bytes = width.saturating_mul(height).saturating_mul(6).max(1);
  let workers = workers.max(1);
  let shared = Shared {
    queue: Mutex::new(VecDeque::new()),
    queued: Condvar::new(),
    closed: AtomicBool::new(false),
    failed: AtomicBool::new(false),
    error: Mutex::new(None),
    // At least three: one written frame awaiting its successor's start, one being planned and
    // one being decoded always make progress.
    permits: Mutex::new((budget / frame_bytes).clamp(3, (workers * 4).max(3))),
    released: Condvar::new(),
    deadline,
  };
  let (done_tx, done_rx) = mpsc::channel::<Done>();
  let (result_tx, result_rx) = mpsc::channel::<Result<(GifEmitter<W>, GifPlanner)>>();
  std::thread::scope(|scope| {
    for _ in 0..workers {
      let (shared, done) = (&shared, done_tx.clone());
      scope.spawn(move || {
        if let Err(error) = work(shared, palette, dither, width, height, &done) {
          shared.fail(error);
        }
      });
    }
    let shared_ref = &shared;
    scope.spawn(move || {
      let result = collect(shared_ref, emitter, planner, &done_rx);
      if let Err(error) = &result {
        shared_ref.fail(error.clone());
      }
      // Workers finish their queue and exit; the result is read once they are idle.
      shared_ref.closed.store(true, Ordering::Release);
      shared_ref.queued.notify_all();
      if result_tx.send(result).is_err() {
        eprintln!("GIF pipeline result receiver closed");
      }
    });
    let mut feeder = Feed {
      shared: &shared,
      done: done_tx,
      width,
      height,
      next: 0,
      previous: None,
    };
    let end = feed(&mut feeder).and_then(|end| {
      feeder
        .done
        .send(Done::Decoded(feeder.next))
        .map_err(|_| EngineError::protocol("GIF collector stopped early"))?;
      Ok(end)
    });
    let end = match end {
      Ok(end) => end,
      Err(error) => {
        shared.fail(error.clone());
        drop(feeder);
        return Err(first_error(&shared, error));
      },
    };
    drop(feeder);
    // Drain: the collector reports once every frame is written; cancellation stays live.
    let collected = loop {
      // Checked before every wait, so a cancellation is seen even when draining is quick.
      if let Err(error) = wait().and_then(|()| shared.alive()) {
        shared.fail(error.clone());
        return Err(first_error(&shared, error));
      }
      match result_rx.recv_timeout(POLL) {
        Ok(result) => break result,
        Err(mpsc::RecvTimeoutError::Timeout) => {},
        Err(mpsc::RecvTimeoutError::Disconnected) => {
          break Err(EngineError::protocol(
            "GIF collector ended without a result",
          ));
        },
      }
    };
    let (emitter, planner) = collected.map_err(|error| first_error(&shared, error))?;
    emitter.finish(end, planner)
  })
}

/// The first failure any thread recorded, else `fallback`: a stage that only saw the
/// cancellation of another reports the original cause, and a caller's own error stays as is.
fn first_error(shared: &Shared, fallback: EngineError) -> EngineError {
  shared
    .error
    .lock()
    .ok()
    .and_then(|mut first| first.take())
    .unwrap_or(fallback)
}

fn work(
  shared: &Shared,
  palette: &Palette,
  dither: Dither,
  width: usize,
  height: usize,
  done: &mpsc::Sender<Done>,
) -> Result<()> {
  let mut ditherer = Ditherer::new(dither, width)?;
  while let Some(work) = shared.take()? {
    let message = match work {
      Work::Dither(frame) => {
        ditherer.reset();
        let mut indices = Vec::with_capacity(width * height);
        for y in 0..height {
          shared.alive()?;
          let source = &frame.rgb[y * width * 3..(y + 1) * width * 3];
          let prior = match &frame.previous {
            Some((rgb, slot)) => Some((
              &rgb[y * width * 3..(y + 1) * width * 3],
              slot.row(shared, y)?,
            )),
            None => None,
          };
          let mut row = vec![0u8; width];
          ditherer.row(y, source, prior, palette, &mut row);
          indices.extend_from_slice(&row);
          frame.slot.publish(shared, y, row.into_boxed_slice())?;
        }
        Done::Dithered(frame.number, frame.pts, indices)
      },
      Work::Compress(order, mut rectangle) => {
        rectangle.compress();
        Done::Compressed(order, rectangle)
      },
    };
    done
      .send(message)
      .map_err(|_| EngineError::protocol("GIF collector stopped early"))?;
  }
  Ok(())
}

/// Plan dithered frames in order, have the pool compress each rectangle, and write them in
/// order. Returns once every decoded frame is planned and every rectangle written.
fn collect<W: Write>(
  shared: &Shared,
  mut emitter: GifEmitter<W>,
  mut planner: GifPlanner,
  done: &mpsc::Receiver<Done>,
) -> Result<(GifEmitter<W>, GifPlanner)> {
  let mut dithered = BTreeMap::new();
  let mut compressed = BTreeMap::new();
  let (mut planned, mut ordered, mut written) = (0u64, 0u64, 0u64);
  let mut total = None;
  // The emitter holds the last written rectangle until the next one fixes its delay.
  let mut holding = false;
  loop {
    if total == Some(planned) && written == ordered {
      return Ok((emitter, planner));
    }
    let message = match done.recv_timeout(POLL) {
      Ok(message) => message,
      Err(mpsc::RecvTimeoutError::Timeout) => {
        shared.alive()?;
        continue;
      },
      Err(mpsc::RecvTimeoutError::Disconnected) => {
        return Err(EngineError::protocol(
          "GIF stages ended before the collector",
        ));
      },
    };
    match message {
      Done::Decoded(count) => total = Some(count),
      Done::Dithered(number, pts, indices) => {
        dithered.insert(number, (pts, indices));
        while let Some((pts, indices)) = dithered.remove(&planned) {
          planned += 1;
          match planner.plan(&indices, pts)? {
            Some(rectangle) => {
              shared.push(Work::Compress(ordered, rectangle))?;
              ordered += 1;
            },
            None => shared.release()?,
          }
        }
      },
      Done::Compressed(order, rectangle) => {
        compressed.insert(order, rectangle);
        while let Some(rectangle) = compressed.remove(&written) {
          written += 1;
          emitter.push(rectangle)?;
          // Pushing wrote the previously held rectangle: that frame is finished.
          if holding {
            shared.release()?;
          }
          holding = true;
        }
      },
    }
  }
}
