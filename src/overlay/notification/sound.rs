use std::io::Cursor;

/// Embedded notification sound (~28 KB MP3).
const NOTIFICATION_MP3: &[u8] = include_bytes!("../../../resources/notification.mp3");

/// Volume level for the notification sound (0.0–1.0).
/// Gentle, so it's audible without being startling.
const VOLUME: f32 = 0.3;

/// Play the notification sound on a background thread. Fire-and-forget.
/// Silently does nothing if audio output is unavailable.
pub fn play() {
  std::thread::Builder::new()
    .name("notification-sound".into())
    .spawn(|| {
      let Ok(mut device_sink) = rodio::DeviceSinkBuilder::open_default_sink() else {
        return;
      };
      device_sink.log_on_drop(false);
      let player = rodio::Player::connect_new(device_sink.mixer());
      let Ok(source) = rodio::Decoder::try_from(Cursor::new(NOTIFICATION_MP3)) else {
        return;
      };
      player.set_volume(VOLUME);
      player.append(source);
      player.sleep_until_end();
    })
    .ok();
}
