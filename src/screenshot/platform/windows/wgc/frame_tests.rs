use super::*;

fn state() -> Arc<(Mutex<FrameWaitState>, Condvar)> {
  Arc::new((
    Mutex::new(FrameWaitState {
      warmup: 3,
      requested: false,
      request_id: 0,
      result: None,
      buffer_to_use: None,
      last_timestamp: None,
    }),
    Condvar::new(),
  ))
}
fn complete(
  state: Arc<(Mutex<FrameWaitState>, Condvar)>,
  result: Result<CapturedFrame>,
) -> std::thread::JoinHandle<()> {
  std::thread::spawn(move || {
    let guard = state.0.lock().unwrap();
    let (mut guard, wait) = state
      .1
      .wait_timeout_while(guard, Duration::from_secs(1), |s| !s.requested)
      .unwrap();
    assert!(!wait.timed_out(), "test producer must receive a request");
    // A spurious notification must not be mistaken for completion.
    state.1.notify_all();
    let request_id = guard.request_id;
    guard.take_request_buffer(request_id).unwrap();
    assert!(guard.finish(request_id, result));
    state.1.notify_all();
  })
}

#[test]
fn frame_request_consumes_acknowledgment_and_never_returns_stale_or_wrong_sized_data() {
  for result in [
    Ok((2, 2, vec![7; 16])),
    Ok((3, 2, vec![7; 16])),
    Ok((2, 2, vec![7; 15])),
    Err(anyhow::anyhow!("device removed")),
  ] {
    let valid = result
      .as_ref()
      .is_ok_and(|f| f.0 == 2 && f.1 == 2 && f.2.len() == 16);
    let shared = state();
    shared.0.lock().unwrap().result = Some(Ok((2, 2, vec![1; 16])));
    let worker = complete(shared.clone(), result);
    let outcome = request_frame(
      &shared,
      &mut Some(vec![0; 32]),
      2,
      2,
      Duration::from_secs(1),
    );
    worker.join().expect("bounded producer");
    assert_eq!(outcome.is_ok(), valid);
    if let Ok(frame) = outcome {
      assert_eq!(frame.2, vec![7; 16]);
    }
    assert!(!shared.0.lock().unwrap().requested);
    assert!(shared.0.lock().unwrap().result.is_none());
  }
}

#[test]
fn frame_timeout_reclaims_buffer_clears_request_and_allows_next_capture() {
  let shared = state();
  let buffer = vec![42; 32];
  let pointer = buffer.as_ptr();
  let mut reusable = Some(buffer);
  let start = Instant::now();
  let error = request_frame(&shared, &mut reusable, 2, 2, Duration::from_millis(5)).unwrap_err();
  assert!(error.to_string().contains("timeout"));
  assert!(start.elapsed() < Duration::from_secs(1));
  assert!(!shared.0.lock().unwrap().requested);
  assert!(shared.0.lock().unwrap().buffer_to_use.is_none());
  assert_eq!(reusable.as_ref().unwrap().as_ptr(), pointer);
  assert_eq!(reusable.as_ref().unwrap().len(), 16);
  let worker = complete(shared.clone(), Ok((2, 2, vec![4; 16])));
  assert_eq!(
    request_frame(&shared, &mut reusable, 2, 2, Duration::from_secs(1))
      .unwrap()
      .2,
    vec![4; 16]
  );
  worker.join().unwrap();
}

#[test]
fn overlapping_empty_overflowed_and_poisoned_frame_requests_fail_explicitly() {
  let shared = state();
  shared.0.lock().unwrap().requested = true;
  assert!(
    request_frame(&shared, &mut None, 1, 1, Duration::ZERO)
      .unwrap_err()
      .to_string()
      .contains("already in progress")
  );
  for (w, h) in [(0, 2), (2, 0), (u32::MAX, u32::MAX)] {
    assert!(request_frame(&state(), &mut None, w, h, Duration::ZERO).is_err());
  }
  let poisoned = state();
  let worker = poisoned.clone();
  assert!(
    std::thread::spawn(move || {
      let _guard = worker.0.lock().unwrap();
      panic!("injected capture lock poison");
    })
    .join()
    .is_err()
  );
  assert!(
    request_frame(&poisoned, &mut None, 2, 2, Duration::ZERO)
      .unwrap_err()
      .to_string()
      .contains("mutex poisoned")
  );
}

#[test]
fn timed_out_frame_cannot_complete_a_newer_request_or_replace_its_failure() {
  let shared = state();
  assert!(request_frame(&shared, &mut None, 2, 2, Duration::ZERO).is_err());
  let mut guard = shared.0.lock().unwrap();
  let expired = guard.request_id;
  guard.begin_request(vec![8; 16]).unwrap();
  let current = guard.request_id;
  assert_ne!(expired, current);
  assert!(
    guard.take_request_buffer(expired).is_none(),
    "expired callback cannot steal the new buffer"
  );
  for result in [
    Ok((2, 2, vec![1; 16])),
    Err(anyhow::anyhow!("old device error")),
  ] {
    assert!(
      !guard.finish(expired, result),
      "expired callback must be rejected"
    );
    assert!(guard.requested);
    assert!(guard.result.is_none());
    assert_eq!(guard.buffer_to_use.as_deref(), Some(&[8; 16][..]));
  }
  assert!(guard.finish(current, Err(anyhow::anyhow!("current device error"))));
  assert!(
    !guard.finish(current, Ok((2, 2, vec![9; 16]))),
    "duplicate callback must not replace failure"
  );
  assert_eq!(
    guard.result.take().unwrap().unwrap_err().to_string(),
    "current device error"
  );
}

#[test]
fn request_ownership_rejects_exhaustion_and_duplicate_producers_without_losing_buffers() {
  let shared = state();
  let mut guard = shared.0.lock().unwrap();
  guard.request_id = u64::MAX;
  assert!(
    guard
      .begin_request(vec![1; 16])
      .unwrap_err()
      .to_string()
      .contains("exhausted")
  );
  assert!(!guard.requested && guard.buffer_to_use.is_none());
  guard.request_id = 0;
  guard.begin_request(vec![2; 16]).unwrap();
  let id = guard.request_id;
  assert!(guard.begin_request(vec![3; 16]).is_err());
  assert_eq!(guard.request_id, id);
  assert_eq!(guard.take_request_buffer(id).unwrap(), [2; 16]);
  assert!(guard.take_request_buffer(id).is_none());
  assert!(guard.finish(id, Ok((2, 2, vec![4; 16]))));
  assert!(guard.take_request_buffer(id).is_none());
  assert!(!guard.finish(id, Err(anyhow::anyhow!("late failure"))));
  assert_eq!(guard.result.take().unwrap().unwrap().2, [4; 16]);
}

#[test]
fn warmup_preserves_native_failure_instead_of_waiting_for_a_generic_timeout() {
  let shared = state();
  {
    let mut guard = shared.0.lock().unwrap();
    guard.warmup = 0;
    let id = guard.request_id;
    assert!(guard.finish(id, Err(anyhow::anyhow!("GPU fence signal rejected"))));
  }
  let started = Instant::now();
  let error = wait_for_warmup(&shared, Duration::from_secs(1)).unwrap_err();
  assert!(started.elapsed() < Duration::from_millis(500));
  assert!(format!("{error:#}").contains("GPU fence signal rejected"));
  assert!(format!("{error:#}").contains("WGC warmup failed"));
}

#[test]
fn warmup_requires_all_frames_and_rejects_unexpected_data() {
  let shared = state();
  for warmup in 0..=WARMUP_FRAMES {
    shared.0.lock().unwrap().warmup = warmup;
    assert_eq!(
      wait_for_warmup(&shared, Duration::ZERO).is_ok(),
      warmup == WARMUP_FRAMES
    );
  }
  shared.0.lock().unwrap().result = Some(Ok((2, 2, vec![4; 16])));
  assert!(
    wait_for_warmup(&shared, Duration::ZERO)
      .unwrap_err()
      .to_string()
      .contains("before warmup")
  );
  let poisoned = state();
  let worker = poisoned.clone();
  assert!(
    std::thread::spawn(move || {
      let _guard = worker.0.lock().unwrap();
      panic!("injected warmup poison");
    })
    .join()
    .is_err()
  );
  assert!(
    wait_for_warmup(&poisoned, Duration::ZERO)
      .unwrap_err()
      .to_string()
      .contains("mutex poisoned")
  );
}

#[test]
fn delayed_producer_cannot_wake_the_next_capture_with_expired_pixels() {
  let shared = state();
  let (claimed_tx, claimed_rx) = std::sync::mpsc::sync_channel(1);
  let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
  let producer_state = shared.clone();
  let producer = std::thread::spawn(move || {
    let guard = producer_state.0.lock().unwrap();
    let (mut guard, wait) = producer_state
      .1
      .wait_timeout_while(guard, Duration::from_secs(1), |s| !s.requested)
      .unwrap();
    assert!(!wait.timed_out());
    let expired = guard.request_id;
    let _old_buffer = guard.take_request_buffer(expired).unwrap();
    drop(guard);
    claimed_tx.send(expired).unwrap();
    release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let mut guard = producer_state.0.lock().unwrap();
    assert!(!guard.finish(expired, Ok((2, 2, vec![1; 16]))));
    assert!(guard.requested && guard.result.is_none());
    let current = guard.request_id;
    assert_ne!(current, expired);
    guard.take_request_buffer(current).unwrap();
    assert!(guard.finish(current, Ok((2, 2, vec![9; 16]))));
    producer_state.1.notify_all();
  });
  let first_state = shared.clone();
  let first = std::thread::spawn(move || {
    request_frame(&first_state, &mut None, 2, 2, Duration::from_millis(200))
  });
  let expired = claimed_rx.recv_timeout(Duration::from_secs(1)).unwrap();
  assert!(
    first
      .join()
      .unwrap()
      .unwrap_err()
      .to_string()
      .contains("timeout")
  );
  let second_state = shared.clone();
  let second = std::thread::spawn(move || {
    request_frame(&second_state, &mut None, 2, 2, Duration::from_secs(1))
  });
  let guard = shared.0.lock().unwrap();
  let (guard, wait) = shared
    .1
    .wait_timeout_while(guard, Duration::from_secs(1), |s| {
      !s.requested || s.request_id == expired
    })
    .unwrap();
  assert!(!wait.timed_out());
  assert!(guard.buffer_to_use.is_some());
  drop(guard);
  release_tx.send(()).unwrap();
  assert_eq!(second.join().unwrap().unwrap().2, [9; 16]);
  producer.join().unwrap();
}
