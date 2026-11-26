//! Explicit application-owned services with statically constrained state access.
use crate::state::*;
use std::{
  ops::Deref,
  path::PathBuf,
  sync::{Arc, Mutex},
};
#[derive(Clone)]
pub struct AppHandle(Arc<Services>);
#[derive(Clone)]
pub struct WeakAppHandle(std::sync::Weak<Services>);
impl WeakAppHandle {
  pub fn upgrade(&self) -> Option<AppHandle> {
    self.0.upgrade().map(AppHandle)
  }
}
impl std::fmt::Debug for AppHandle {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("AppHandle")
      .field("resources", &self.0.resources)
      .finish_non_exhaustive()
  }
}
pub struct Services {
  #[cfg(test)]
  pub(crate) test_hotkeys: Option<crate::app::hotkeys::TestRegistry>,
  #[cfg(test)]
  test_config_path: Mutex<Option<PathBuf>>,
  pub config: ConfigState,
  pub clipboard: Option<ClipboardState>,
  pub recorder: VideoRecorderState,
  pub language: LanguageState,
  pub shortcuts: crate::shortcuts::ShortcutStateHolder,
  pub notifications: crate::overlay::notification::NotificationManager,
  pub activity: OverlayActivityState,
  pub hud: RecordingHudState,
  pub session: RecordingSessionState,
  pub exit: AppExitState,
  pub resources: PathBuf,
}
pub trait Service: Sized {
  fn get(services: &Services) -> Option<&Self>;
}
pub struct State<'a, T>(&'a T);
impl<T> Deref for State<'_, T> {
  type Target = T;
  fn deref(&self) -> &T {
    self.0
  }
}
impl<T> State<'_, T> {
  pub fn inner(&self) -> &T {
    self.0
  }
}
macro_rules! service {
  ($t:ty, $field:ident) => {
    impl Service for $t {
      fn get(s: &Services) -> Option<&Self> {
        Some(&s.$field)
      }
    }
  };
}
service!(ConfigState, config);
service!(VideoRecorderState, recorder);
service!(LanguageState, language);
service!(crate::shortcuts::ShortcutStateHolder, shortcuts);
service!(
  crate::overlay::notification::NotificationManager,
  notifications
);
service!(OverlayActivityState, activity);
service!(RecordingHudState, hud);
service!(RecordingSessionState, session);
service!(AppExitState, exit);
impl Service for ClipboardState {
  fn get(s: &Services) -> Option<&Self> {
    s.clipboard.as_ref()
  }
}
impl AppHandle {
  /// Isolated unit-test services: no clipboard or native registration.
  #[cfg(test)]
  pub(crate) fn isolated(config: crate::config::Config, resources: PathBuf) -> Self {
    let video_config = crate::video::config::VideoConfig::default();
    Self(Arc::new(Services {
      test_hotkeys: Some(crate::app::hotkeys::TestRegistry::default()),
      test_config_path: Mutex::new(None),
      language: LanguageState(Mutex::new(config.language.clone())),
      config: ConfigState::new(config),
      clipboard: None,
      recorder: VideoRecorderState::new(crate::video::controller::VideoRecorder::new(video_config)),
      shortcuts: crate::shortcuts::ShortcutStateHolder::new(),
      notifications: crate::overlay::notification::NotificationManager::isolated(),
      activity: OverlayActivityState::new(),
      hud: RecordingHudState::new(),
      session: RecordingSessionState::default(),
      exit: AppExitState::new(),
      resources,
    }))
  }

  pub fn downgrade(&self) -> WeakAppHandle {
    WeakAppHandle(Arc::downgrade(&self.0))
  }
  #[cfg(test)]
  pub(crate) fn test_hotkeys(&self) -> Option<&crate::app::hotkeys::TestRegistry> {
    self.0.test_hotkeys.as_ref()
  }
  #[cfg(test)]
  pub(crate) fn isolate_config_path(&self, path: PathBuf) {
    *self.0.test_config_path.lock().expect("test config path") = Some(path);
  }
  #[cfg(test)]
  pub(crate) fn test_config_path(&self) -> Option<PathBuf> {
    self
      .0
      .test_config_path
      .lock()
      .expect("test config path")
      .clone()
  }
  pub fn new(config: crate::config::Config, resources: PathBuf) -> Self {
    let video_config = crate::video::config::VideoConfig {
      videos_path: None,
      fps: config.video_fps,
      quality: config.video_quality,
      hardware_encoding: config.video_hardware_encoding,
      audio: crate::video::config::AudioConfig {
        enabled: config.record_audio,
      },
      // Each recording sets its own microphone from the settings when it starts.
      microphone: None,
      recording_profile: crate::video::config::RecordingProfile::Standard,
    };
    let clipboard = match arboard::Clipboard::new() {
      Ok(value) => Some(ClipboardState(Mutex::new(value))),
      Err(error) => {
        tracing::warn!(%error, "Clipboard initialization failed; clipboard operations will report unavailable");
        None
      },
    };
    Self(Arc::new(Services {
      #[cfg(test)]
      test_hotkeys: None,
      #[cfg(test)]
      test_config_path: Mutex::new(None),
      language: LanguageState(Mutex::new(config.language.clone())),
      config: ConfigState::new(config),
      clipboard,
      recorder: VideoRecorderState::new(crate::video::controller::VideoRecorder::new(video_config)),
      shortcuts: crate::shortcuts::ShortcutStateHolder::new(),
      notifications: crate::overlay::notification::NotificationManager::new(),
      activity: OverlayActivityState::new(),
      hud: RecordingHudState::new(),
      session: RecordingSessionState::default(),
      exit: AppExitState::new(),
      resources,
    }))
  }
  pub fn try_state<T: Service>(&self) -> Option<State<'_, T>> {
    T::get(&self.0).map(State)
  }
  pub fn state<T: RequiredService>(&self) -> State<'_, T> {
    State(T::required(&self.0))
  }
  pub fn resources(&self) -> &std::path::Path {
    &self.0.resources
  }
  pub fn run_on_main_thread(&self, task: impl FnOnce() + Send + 'static) -> Result<(), String> {
    flinch_settings::slint::invoke_from_event_loop(task).map_err(|error| error.to_string())
  }
  pub fn exit(&self, _code: i32) {
    if let Err(error) = flinch_settings::slint::quit_event_loop() {
      tracing::error!(%error, "Cannot stop native application loop");
    }
  }
}

pub trait RequiredService: Service {
  fn required(s: &Services) -> &Self;
}
impl RequiredService for VideoRecorderState {
  fn required(s: &Services) -> &Self {
    &s.recorder
  }
}
pub struct Image<'a> {
  rgba: Vec<u8>,
  width: u32,
  height: u32,
  _lifetime: std::marker::PhantomData<&'a ()>,
}
impl Image<'_> {
  pub fn new_owned(rgba: Vec<u8>, width: u32, height: u32) -> Self {
    Self {
      rgba,
      width,
      height,
      _lifetime: std::marker::PhantomData,
    }
  }
  pub fn into_tray(self) -> anyhow::Result<tray_icon::Icon> {
    Ok(tray_icon::Icon::from_rgba(
      self.rgba,
      self.width,
      self.height,
    )?)
  }
}

#[cfg(test)]
mod ownership_tests {
  use super::*;

  #[test]
  fn production_service_construction_preserves_preferences_resources_and_weak_ownership() {
    let folder = tempfile::tempdir().unwrap();
    let config = crate::config::Config {
      language: "fr".into(),
      video_fps: 60,
      record_audio: true,
      ..Default::default()
    };
    let app = AppHandle::new(config, folder.path().to_path_buf());
    assert_eq!(app.resources(), folder.path());
    assert_eq!(crate::config::Config::current(&app).video_fps, 60);
    assert_eq!(crate::i18n::get_lang(&app), "fr");
    assert!(app.try_state::<crate::state::RecordingHudState>().is_some());
    assert!(
      app
        .try_state::<crate::state::RecordingSessionState>()
        .unwrap()
        .snapshot()
        .is_none()
    );
    assert_eq!(
      app
        .state::<VideoRecorderState>()
        .inner()
        .0
        .lock()
        .unwrap()
        .status(),
      crate::video::controller::RecordingStatus::Idle
    );
    let weak = app.downgrade();
    assert!(weak.upgrade().is_some());
    let clone = app.clone();
    drop(app);
    assert!(weak.upgrade().is_some());
    drop(clone);
    assert!(
      weak.upgrade().is_none(),
      "services must not retain their own AppHandle"
    );
  }
}
