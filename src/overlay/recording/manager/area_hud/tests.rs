use super::*;

#[test]
fn prepared_area_hud_translates_negative_desktop_coordinates_before_offscreen_rendering() {
  let controls = Arc::new(crate::state::RecordingControls::new());
  let mut window = build_area_hud(
    (-640, -200, 640, 480),
    (-600., -160., -280., 40.),
    Instant::now(),
    controls.clone(),
    "fr".into(),
  )
  .unwrap();
  assert_eq!(
    (
      window.mode.sel_l,
      window.mode.sel_t,
      window.mode.sel_r,
      window.mode.sel_b
    ),
    (40, 40, 360, 240)
  );
  assert_eq!(
    (window.mode.bitmap.width, window.mode.bitmap.height),
    (640, 480)
  );
  assert!(Arc::ptr_eq(&window.mode.controls, &controls));
  assert!(window.shell.hwnd.is_invalid());
  AreaHudMode::render_with(&mut window, |bitmap, hwnd, x, y| {
    assert!(hwnd.is_invalid());
    assert_eq!((x, y), (-640, -200));
    assert_eq!((bitmap.width, bitmap.height), (640, 480));
    Ok(())
  });
  let pixels = window.mode.bitmap.pixels_mut();
  assert_eq!(&pixels[(100 * 640 + 100) * 4..][..4], &[0; 4]);
  assert!(pixels[(40 * 640 + 40) * 4 + 3] > 0);
  assert!(
    build_area_hud(
      (0, 0, 0, 0),
      (0., 0., 1., 1.),
      Instant::now(),
      controls,
      "en".into()
    )
    .is_err()
  );
}
use std::time::Duration;

#[test]
fn startup_pause_is_ignored_and_stop_failure_or_display_change_close_only_the_owned_hidden_hud() {
  use crate::overlay::gdi::window::{
    class_name_for_thread, create_hud_window, register_overlay_class, unregister_overlay_class,
  };
  use windows::Win32::{
    Foundation::{LPARAM, WPARAM},
    UI::WindowsAndMessaging::{DefWindowProcW, DestroyWindow, IsWindow, IsWindowVisible},
  };
  unsafe extern "system" fn hidden(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: the fixture has no application message state or visible surface.
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
  }
  for action in 0..4 {
    let class = format!("{}-hud-stop-{action}", class_name_for_thread());
    register_overlay_class(&class, Some(hidden)).unwrap();
    let hwnd = create_hud_window(
      &class,
      "Hidden HUD shutdown fixture",
      0,
      0,
      640,
      480,
      std::ptr::null_mut(),
    )
    .unwrap();
    let mut window = window();
    window.shell.hwnd = hwnd;
    let layout = toolbar_layout(window.shell.metrics, window.mode.tbar_l, window.mode.tbar_t);
    AreaHudMode::on_click(
      &mut window,
      hwnd,
      layout.pause_rect.left + 3,
      layout.pause_rect.top + 3,
    );
    assert!(!window.mode.controls.pause_pending.load(Ordering::SeqCst));
    assert_eq!(window.shell.phase, RecordingPhase::Starting);
    // SAFETY: inspect the test-owned window without showing it or changing global input.
    assert!(unsafe { IsWindow(Some(hwnd)) }.as_bool());
    assert!(!unsafe { IsWindowVisible(hwnd) }.as_bool());
    window.mode.controls.ready.store(true, Ordering::SeqCst);
    let mut receiver = window.mode.controls.subscribe();
    AreaHudMode::sync_controls(&mut window);
    for (acknowledged_paused, pending_phase) in [
      (false, RecordingPhase::Pausing),
      (true, RecordingPhase::Resuming),
    ] {
      window.mode.controls.sync_paused(acknowledged_paused);
      AreaHudMode::sync_controls(&mut window);
      let layout = toolbar_layout(window.shell.metrics, window.mode.tbar_l, window.mode.tbar_t);
      AreaHudMode::click_with(
        &mut window,
        hwnd,
        layout.pause_rect.left + 3,
        layout.pause_rect.top + 3,
        |window| AreaHudMode::render_with(window, |_, _, _, _| Ok(())),
      );
      assert_eq!(window.shell.phase, pending_phase);
      assert!(window.mode.controls.pause_pending.load(Ordering::SeqCst));
      assert_eq!(
        *receiver.borrow_and_update(),
        Some(crate::state::RecordingControlEvent::PauseRequested(
          !acknowledged_paused
        ))
      );
      assert_eq!(window.mode.controls.paused(), acknowledged_paused);
      // A second click before the worker acknowledges cannot reverse the request.
      let layout = toolbar_layout(window.shell.metrics, window.mode.tbar_l, window.mode.tbar_t);
      AreaHudMode::click_with(
        &mut window,
        hwnd,
        layout.pause_rect.left + 3,
        layout.pause_rect.top + 3,
        |_| panic!("pending pause must not redraw or dispatch twice"),
      );
      assert_eq!(window.shell.phase, pending_phase);
      assert!(!receiver.has_changed().unwrap());
      // SAFETY: rendering a hidden fixture must never expose it.
      assert!(!unsafe { IsWindowVisible(hwnd) }.as_bool());
    }
    let layout = toolbar_layout(window.shell.metrics, window.mode.tbar_l, window.mode.tbar_t);
    match action {
      0 => AreaHudMode::on_click(
        &mut window,
        hwnd,
        layout.stop_rect.left + 3,
        layout.stop_rect.top + 3,
      ),
      1 => {
        window.mode.controls.fail_pause();
        AreaHudMode::on_timer(&mut window, hwnd, HUD_TIMER_ID);
      },
      2 => {
        window.mode.controls.request_stop();
        AreaHudMode::on_timer(&mut window, hwnd, HUD_TIMER_ID);
      },
      _ => AreaHudMode::on_display_change(&mut window, hwnd),
    }
    assert!(window.mode.controls.stop_requested());
    // SAFETY: query only this fixture's HWND after the production shutdown callback.
    let alive = unsafe { IsWindow(Some(hwnd)) }.as_bool();
    if alive {
      // SAFETY: bounded cleanup of this fixture's owned HWND if a regression failed to close it.
      unsafe {
        DestroyWindow(hwnd).unwrap();
      }
    }
    unregister_overlay_class(&class);
    assert!(!alive, "HUD shutdown must destroy its own window");
  }
}

fn window() -> RecordingOverlayWindow<AreaHudMode> {
  RecordingOverlayWindow {
    shell: RecordingOverlayShell::new(Instant::now(), "en".into()),
    mode: AreaHudMode {
      bitmap: LayeredBitmap::new(640, 480).unwrap(),
      win_x: 0,
      win_y: 0,
      win_w: 640,
      win_h: 480,
      sel_l: 40,
      sel_t: 40,
      sel_r: 500,
      sel_b: 350,
      tbar_l: 60,
      tbar_t: 360,
      controls: Arc::new(crate::state::RecordingControls::new()),
    },
  }
}

#[test]
fn area_hud_uses_acknowledged_phase_and_elapsed_and_renders_offscreen() {
  let mut window = window();
  for (ready, pending, paused, phase) in [
    (false, false, false, RecordingPhase::Starting),
    (true, false, false, RecordingPhase::Recording),
    (true, true, false, RecordingPhase::Pausing),
    (true, false, true, RecordingPhase::Paused),
    (true, true, true, RecordingPhase::Resuming),
    (true, false, false, RecordingPhase::Recording),
  ] {
    window.mode.controls.ready.store(ready, Ordering::SeqCst);
    window.mode.controls.sync_paused(paused);
    window
      .mode
      .controls
      .pause_pending
      .store(pending, Ordering::SeqCst);
    window
      .mode
      .controls
      .elapsed_ms
      .store(65_321, Ordering::SeqCst);
    AreaHudMode::sync_controls(&mut window);
    assert_eq!(window.shell.phase, phase);
    assert_eq!(
      window.shell.confirmed_elapsed,
      Some(Duration::from_millis(65_321))
    );
    assert_eq!(window.shell.is_paused(), paused);
    let mut presented = false;
    AreaHudMode::render_with(&mut window, |bitmap, hwnd, x, y| {
      assert_eq!((bitmap.width, bitmap.height), (640, 480));
      assert_eq!((x, y), (0, 0));
      assert!(hwnd.is_invalid());
      presented = true;
      Ok(())
    });
    assert!(presented);
    let toolbar_y = window.mode.tbar_t;
    let pixels = window.mode.bitmap.pixels_mut();
    assert!(
      pixels[(40 * 640 + 40) * 4 + 3] > 0,
      "selection border must be visible"
    );
    assert_eq!(
      &pixels[(100 * 640 + 100) * 4..][..4],
      &[0, 0, 0, 0],
      "capture interior must stay transparent"
    );
    assert!(
      pixels[(toolbar_y as usize * 640 * 4)..]
        .as_chunks::<4>()
        .0
        .iter()
        .any(|p| p[3] != 0),
      "toolbar must be painted"
    );
  }
  window.mode.win_x = -640;
  window.mode.win_y = -120;
  assert_eq!(window.mode.viewport(), (-640, -120, 640, 480));
  window.mode.set_toolbar_position(80, 380);
  assert_eq!(window.mode.toolbar_position(), (80, 380));
  assert_eq!(AreaHudMode::hit_test(&window, -630, -110), LRESULT(-1));
  let layout = toolbar_layout(window.shell.metrics, 80, 380);
  assert_eq!(
    AreaHudMode::hover_target(&window, layout.stop_rect.left + 3, layout.stop_rect.top + 3),
    HoverTarget::Stop
  );
  assert_eq!(
    AreaHudMode::hit_test(
      &window,
      layout.stop_rect.left + 3 - 640,
      layout.stop_rect.top + 3 - 120
    ),
    LRESULT(1)
  );
}
