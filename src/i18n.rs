// src/i18n.rs
// Rust-side translations for system notifications.
//
// Covers native system strings and shared settings catalog access
// (notifications fired from background tasks, tray label toggle).
// Slint Copy and native settings share messages/*.json.

pub fn native_settings(language: &str, key: &str) -> Result<String, String> {
  use flinch_settings::{catalog::Catalog, model::LOCALES};
  static CATALOGS: std::sync::LazyLock<Result<Vec<Catalog>, String>> =
    std::sync::LazyLock::new(|| LOCALES.into_iter().map(Catalog::load).collect());
  let index = flinch_settings::catalog::locale_index(language)? as usize;
  let catalogs = CATALOGS.as_ref().map_err(Clone::clone)?;
  catalogs
    .get(index)
    .ok_or_else(|| "Missing native settings catalog".to_owned())?
    .text(key)
    .map(str::to_owned)
}

pub fn autostart_blocked(lang: &str) -> &'static str {
  match lang {
    "fr" => {
      "Windows a bloqué ce changement de démarrage. Vérifiez Flinch dans Paramètres > Applications > Démarrage. Une stratégie système peut empêcher ce changement."
    },
    "de" => {
      "Windows hat diese Autostart-Änderung blockiert. Prüfen Sie Flinch unter Einstellungen > Apps > Autostart. Eine Systemrichtlinie kann die Änderung verhindern."
    },
    "es" => {
      "Windows ha bloqueado este cambio de inicio. Comprueba Flinch en Configuración > Aplicaciones > Inicio. Una directiva del sistema puede impedir el cambio."
    },
    "it" => {
      "Windows ha bloccato questa modifica dell'avvio. Controlla Flinch in Impostazioni > App > Avvio. Un criterio di sistema può impedire la modifica."
    },
    _ => {
      "Windows blocked this startup change. Check Flinch in Settings > Apps > Startup. A system policy may prevent the change."
    },
  }
}

pub fn selection_too_small(lang: &str) -> &'static str {
  match lang {
    "fr" => "Trop petit. Faites glisser une zone plus grande.",
    "de" => "Zu klein. Ziehen Sie einen größeren Bereich auf.",
    "es" => "Demasiado pequeño. Arrastra un área más grande.",
    "it" => "Troppo piccola. Trascina un'area più grande.",
    _ => "Too small. Drag a larger area.",
  }
}

/// Muted line under the selection dimensions; keep it short enough for that panel.
pub fn selection_edit_hint(lang: &str) -> &'static str {
  match lang {
    "fr" => "Maintenez Alt pour modifier",
    "de" => "Alt halten zum Bearbeiten",
    "es" => "Mantén Alt para editar",
    "it" => "Tieni Alt per modificare",
    _ => "Hold Alt to edit",
  }
}

pub struct ConfigRecoveryLabels {
  pub kept_fields: &'static str,
  pub restored_backup: &'static str,
  pub restored: &'static str,
  pub failed: &'static str,
}

pub fn config_recovery(lang: &str) -> ConfigRecoveryLabels {
  match lang {
    "fr" => ConfigRecoveryLabels {
      kept_fields: "Certains paramètres enregistrés sont invalides et ont repris leur valeur par défaut. Vos autres paramètres ont été conservés. Une copie du fichier original a été conservée ici :",
      restored_backup: "Les paramètres enregistrés sont invalides. Votre dernière sauvegarde a été restaurée. Une copie du fichier endommagé a été conservée ici :",
      restored: "Les paramètres enregistrés sont invalides. Les paramètres par défaut ont été restaurés. Une copie de récupération a été conservée ici :",
      failed: "Impossible de restaurer les paramètres enregistrés en toute sécurité. Le fichier original a été conservé. Flinch utilise les paramètres par défaut pour cette session. Vérifiez le fichier et les autorisations avant de réessayer :",
    },
    "de" => ConfigRecoveryLabels {
      kept_fields: "Einige gespeicherte Einstellungen sind ungültig und wurden auf die Standardwerte zurückgesetzt. Ihre übrigen Einstellungen wurden beibehalten. Eine Kopie der Originaldatei wurde hier gespeichert:",
      restored_backup: "Die gespeicherten Einstellungen sind ungültig. Ihre letzte Sicherung wurde wiederhergestellt. Eine Kopie der beschädigten Datei wurde hier gespeichert:",
      restored: "Die gespeicherten Einstellungen sind ungültig. Die Standardeinstellungen wurden wiederhergestellt. Eine Sicherung wurde hier gespeichert:",
      failed: "Die gespeicherten Einstellungen konnten nicht sicher wiederhergestellt werden. Die Originaldatei wurde beibehalten. Flinch verwendet für diese Sitzung Standardeinstellungen. Prüfen Sie die Datei und Zugriffsrechte, bevor Sie es erneut versuchen:",
    },
    "es" => ConfigRecoveryLabels {
      kept_fields: "Algunos ajustes guardados no son válidos y volvieron a sus valores predeterminados. Se mantuvieron tus demás ajustes. Se conservó una copia del archivo original aquí:",
      restored_backup: "Los ajustes guardados no son válidos. Se restauró tu última copia de seguridad. Se conservó una copia del archivo dañado aquí:",
      restored: "Los ajustes guardados no son válidos. Se restauraron los valores predeterminados. Se conservó una copia de recuperación aquí:",
      failed: "No se pudieron restaurar los ajustes guardados de forma segura. Se conservó el archivo original. Flinch utiliza los valores predeterminados para esta sesión. Comprueba el archivo y los permisos antes de volver a intentarlo:",
    },
    "it" => ConfigRecoveryLabels {
      kept_fields: "Alcune impostazioni salvate non sono valide e sono tornate ai valori predefiniti. Le altre impostazioni sono state mantenute. Una copia del file originale è stata conservata qui:",
      restored_backup: "Le impostazioni salvate non sono valide. È stato ripristinato il tuo ultimo backup. Una copia del file danneggiato è stata conservata qui:",
      restored: "Le impostazioni salvate non sono valide. Sono state ripristinate le impostazioni predefinite. Una copia di recupero è stata conservata qui:",
      failed: "Impossibile ripristinare in sicurezza le impostazioni salvate. Il file originale è stato conservato. Flinch usa le impostazioni predefinite per questa sessione. Controlla il file e le autorizzazioni prima di riprovare:",
    },
    _ => ConfigRecoveryLabels {
      kept_fields: "Some saved settings were invalid and went back to their defaults. Your other settings were kept. A copy of the original file was preserved here:",
      restored_backup: "The saved settings are invalid. Your latest backup was restored. A copy of the damaged file was preserved here:",
      restored: "The saved settings are invalid. Default settings were restored. A recovery copy was preserved here:",
      failed: "The saved settings could not be restored safely. The original file was preserved. Flinch is using defaults for this session. Check the file and permissions before retrying:",
    },
  }
}

/// Retrieve the active language code from app state.
pub fn get_lang(app: &crate::app::context::AppHandle) -> String {
  app
    .try_state::<crate::commands::LanguageState>()
    .and_then(|s| s.inner().0.lock().ok().map(|g| g.clone()))
    .unwrap_or_else(|| "en".to_string())
}

/// Tray menu item labels.
pub struct RecordingControlLabels {
  pub pause: &'static str,
  pub resume: &'static str,
  pub stop: &'static str,
}

pub fn recording_controls(lang: &str) -> RecordingControlLabels {
  match lang {
    "fr" => RecordingControlLabels {
      pause: "Pause",
      resume: "Reprendre",
      stop: "Arrêter",
    },
    "de" => RecordingControlLabels {
      pause: "Pause",
      resume: "Fortsetzen",
      stop: "Stopp",
    },
    "es" => RecordingControlLabels {
      pause: "Pausar",
      resume: "Reanudar",
      stop: "Detener",
    },
    "it" => RecordingControlLabels {
      pause: "Pausa",
      resume: "Riprendi",
      stop: "Ferma",
    },
    _ => RecordingControlLabels {
      pause: "Pause",
      resume: "Resume",
      stop: "Stop",
    },
  }
}

/// Tray menu item labels.
pub struct TrayLabels {
  pub screenshot: &'static str,
  pub capture_edit: &'static str,
  pub copy_text: &'static str,
  pub color_picker: &'static str,
  pub last_region: &'static str,
  pub record_start: &'static str,
  pub record_stop: &'static str,
  pub record_pause: &'static str,
  pub record_resume: &'static str,
  pub hotkeys: &'static str,
  pub settings: &'static str,
  pub quit: &'static str,
  pub recent: &'static str,
  pub recent_empty: &'static str,
  pub recent_open: &'static str,
  pub recent_copy: &'static str,
  pub recent_show: &'static str,
  /// Pin to Screen: the Recent Captures button and pin notifications.
  pub pin: &'static str,
  pub recent_folder: &'static str,
  pub recent_copied: &'static str,
  /// The Capture Delay submenu row and its choices, matching `capture_delay::CHOICES`.
  pub delay: &'static str,
  pub delay_choices: [&'static str; 4],
  pub delay_failed: &'static str,
}

pub fn tray(lang: &str) -> &'static TrayLabels {
  match lang {
    "fr" => &TRAY_FR,
    "de" => &TRAY_DE,
    "es" => &TRAY_ES,
    "it" => &TRAY_IT,
    _ => &TRAY_EN,
  }
}

static TRAY_EN: TrayLabels = TrayLabels {
  screenshot: "Take Screenshot",
  capture_edit: "Capture and Edit",
  copy_text: "Copy Text",
  color_picker: "Color Picker",
  last_region: "Capture Last Region",
  record_start: "Start Recording",
  record_stop: "Stop Recording",
  record_pause: "Pause Recording",
  record_resume: "Resume Recording",
  hotkeys: "Hotkeys",
  settings: "Settings",
  quit: "Exit",
  recent: "Recent Captures",
  recent_empty: "No captures yet",
  recent_open: "Open",
  recent_copy: "Copy",
  recent_show: "Show in Folder",
  pin: "Pin to Screen",
  recent_folder: "Open Captures Folder",
  recent_copied: "Copied to clipboard",
  delay: "Capture Delay",
  delay_choices: ["Off", "3 seconds", "5 seconds", "10 seconds"],
  delay_failed: "Capture Delay couldn’t be changed",
};

static TRAY_FR: TrayLabels = TrayLabels {
  screenshot: "Prendre une capture",
  capture_edit: "Capturer et modifier",
  copy_text: "Copier le texte",
  color_picker: "Sélecteur de couleur",
  last_region: "Capturer la dernière zone",
  record_start: "Démarrer l'enregistrement",
  record_stop: "Arrêter l'enregistrement",
  record_pause: "Mettre en pause",
  record_resume: "Reprendre l'enregistrement",
  hotkeys: "Raccourcis",
  settings: "Paramètres",
  quit: "Quitter",
  recent: "Captures récentes",
  recent_empty: "Aucune capture pour l'instant",
  recent_open: "Ouvrir",
  recent_copy: "Copier",
  recent_show: "Afficher dans le dossier",
  pin: "Épingler à l’écran",
  recent_folder: "Ouvrir le dossier des captures",
  recent_copied: "Copié dans le presse-papiers",
  delay: "Délai de capture",
  delay_choices: ["Désactivé", "3 secondes", "5 secondes", "10 secondes"],
  delay_failed: "Impossible de modifier le délai de capture",
};

static TRAY_DE: TrayLabels = TrayLabels {
  screenshot: "Screenshot erstellen",
  capture_edit: "Erfassen und bearbeiten",
  copy_text: "Text kopieren",
  color_picker: "Farbwähler",
  last_region: "Letzten Bereich erfassen",
  record_start: "Aufnahme starten",
  record_stop: "Aufnahme stoppen",
  record_pause: "Aufnahme pausieren",
  record_resume: "Aufnahme fortsetzen",
  hotkeys: "Tastenkürzel",
  settings: "Einstellungen",
  quit: "Beenden",
  recent: "Letzte Aufnahmen",
  recent_empty: "Noch keine Aufnahmen",
  recent_open: "Öffnen",
  recent_copy: "Kopieren",
  recent_show: "Im Ordner anzeigen",
  pin: "Am Bildschirm anheften",
  recent_folder: "Aufnahmeordner öffnen",
  recent_copied: "In die Zwischenablage kopiert",
  delay: "Aufnahmeverzögerung",
  delay_choices: ["Aus", "3 Sekunden", "5 Sekunden", "10 Sekunden"],
  delay_failed: "Aufnahmeverzögerung konnte nicht geändert werden",
};

static TRAY_ES: TrayLabels = TrayLabels {
  screenshot: "Tomar captura",
  capture_edit: "Capturar y editar",
  copy_text: "Copiar texto",
  color_picker: "Selector de color",
  last_region: "Capturar la última área",
  record_start: "Iniciar grabación",
  record_stop: "Detener grabación",
  record_pause: "Pausar grabación",
  record_resume: "Reanudar grabación",
  hotkeys: "Atajos",
  settings: "Ajustes",
  quit: "Salir",
  recent: "Capturas recientes",
  recent_empty: "Aún no hay capturas",
  recent_open: "Abrir",
  recent_copy: "Copiar",
  recent_show: "Mostrar en la carpeta",
  pin: "Fijar en pantalla",
  recent_folder: "Abrir carpeta de capturas",
  recent_copied: "Copiado al portapapeles",
  delay: "Retraso de captura",
  delay_choices: ["Desactivado", "3 segundos", "5 segundos", "10 segundos"],
  delay_failed: "No se pudo cambiar el retraso de captura",
};

static TRAY_IT: TrayLabels = TrayLabels {
  screenshot: "Fai uno screenshot",
  capture_edit: "Cattura e modifica",
  copy_text: "Copia testo",
  color_picker: "Selettore colore",
  last_region: "Cattura l’ultima area",
  record_start: "Avvia registrazione",
  record_stop: "Ferma registrazione",
  record_pause: "Metti in pausa",
  record_resume: "Riprendi registrazione",
  hotkeys: "Tasti rapidi",
  settings: "Impostazioni",
  quit: "Esci",
  recent: "Catture recenti",
  recent_empty: "Ancora nessuna cattura",
  recent_open: "Apri",
  recent_copy: "Copia",
  recent_show: "Mostra nella cartella",
  pin: "Fissa sullo schermo",
  recent_folder: "Apri cartella delle catture",
  recent_copied: "Copiato negli appunti",
  delay: "Ritardo di cattura",
  delay_choices: ["Disattivato", "3 secondi", "5 secondi", "10 secondi"],
  delay_failed: "Impossibile cambiare il ritardo di cattura",
};

/// All translatable notification titles emitted by Rust code.
pub struct NotifTitles {
  // Screenshot post-capture (subject = "screenshot")
  pub screenshot_copied: &'static str,
  pub screenshot_saved: &'static str,
  pub screenshot_save_failed: &'static str,

  // Capture-level error notifications (before handle_capture_result)
  pub capture_failed: &'static str,

  // Video
  pub video_saved: &'static str,
  pub video_save_failed: &'static str,

  // Shortcut conflicts
  pub shortcut_warning_title: &'static str,

  // ── Notification bodies (static) ────────────────────────────────────────
  pub body_image_copied: &'static str,
  /// Prefix for "Saved to: <path>" — append path after
  pub body_saved_to_prefix: &'static str,
  pub body_check_logs: &'static str,
  pub body_shortcut_conflict_warning: &'static str,
  pub shortcut_warning_open_settings: &'static str,
  pub shortcut_warning_dismiss: &'static str,
}

/// Titles used for screenshot capture notifications.
pub struct CaptureNotifTitles {
  pub copied: &'static str,
  pub saved: &'static str,
  pub save_failed: &'static str,
}

/// Titles used for video capture notifications.
pub struct VideoNotifTitles {
  pub saved: &'static str,
}

/// Resolve notification titles for screenshot captures.
pub fn screenshot_notif_titles(lang: &str) -> CaptureNotifTitles {
  let t = get(lang);
  CaptureNotifTitles {
    copied: t.screenshot_copied,
    saved: t.screenshot_saved,
    save_failed: t.screenshot_save_failed,
  }
}

/// Resolve notification titles for video captures.
pub fn video_notif_titles(lang: &str) -> VideoNotifTitles {
  let t = get(lang);
  VideoNotifTitles {
    saved: t.video_saved,
  }
}

pub fn get(lang: &str) -> &'static NotifTitles {
  match lang {
    "fr" => &FR,
    "de" => &DE,
    "es" => &ES,
    "it" => &IT,
    _ => &EN,
  }
}

// ── English ─────────────────────────────────────────────────────────────────

static EN: NotifTitles = NotifTitles {
  screenshot_copied: "Screenshot copied",
  screenshot_saved: "Screenshot saved",
  screenshot_save_failed: "Screenshot save failed",

  capture_failed: "Screenshot failed",

  video_saved: "Video saved",
  video_save_failed: "Video save failed",

  shortcut_warning_title: "Hotkeys need attention",

  body_image_copied: "Image copied to clipboard",
  body_saved_to_prefix: "Saved to: ",
  body_check_logs: "Something went wrong. Check the logs for details.",
  body_shortcut_conflict_warning: "Some hotkeys are unavailable because they are already being used by another application. Review your hotkeys in Settings.",
  shortcut_warning_open_settings: "Open Settings",
  shortcut_warning_dismiss: "Not Now",
};

// ── French ───────────────────────────────────────────────────────────────────

static FR: NotifTitles = NotifTitles {
  screenshot_copied: "Capture copiée",
  screenshot_saved: "Capture sauvegardée",
  screenshot_save_failed: "Échec de la sauvegarde",

  capture_failed: "Échec de la capture",

  video_saved: "Vidéo sauvegardée",
  video_save_failed: "Échec de la sauvegarde vidéo",

  shortcut_warning_title: "Raccourcis à vérifier",

  body_image_copied: "Image copiée dans le presse-papiers",
  body_saved_to_prefix: "Sauvegardé dans : ",
  body_check_logs: "Une erreur est survenue. Consultez les journaux pour plus de détails.",
  body_shortcut_conflict_warning: "Certains raccourcis sont indisponibles car ils sont déjà utilisés par une autre application. Vérifiez vos raccourcis dans les paramètres.",
  shortcut_warning_open_settings: "Ouvrir les paramètres",
  shortcut_warning_dismiss: "Plus tard",
};

// ── German ───────────────────────────────────────────────────────────────────

static DE: NotifTitles = NotifTitles {
  screenshot_copied: "Screenshot kopiert",
  screenshot_saved: "Screenshot gespeichert",
  screenshot_save_failed: "Speichern fehlgeschlagen",

  capture_failed: "Screenshot fehlgeschlagen",

  video_saved: "Video gespeichert",
  video_save_failed: "Video konnte nicht gespeichert werden",

  shortcut_warning_title: "Tastenkürzel prüfen",

  body_image_copied: "Bild in die Zwischenablage kopiert",
  body_saved_to_prefix: "Gespeichert in: ",
  body_check_logs: "Etwas ist schiefgelaufen. Weitere Details finden Sie in den Protokollen.",
  body_shortcut_conflict_warning: "Einige Tastenkürzel sind nicht verfügbar, weil sie bereits von einer anderen Anwendung verwendet werden. Überprüfen Sie Ihre Tastenkürzel in den Einstellungen.",
  shortcut_warning_open_settings: "Einstellungen öffnen",
  shortcut_warning_dismiss: "Später",
};

// ── Spanish ──────────────────────────────────────────────────────────────────

static ES: NotifTitles = NotifTitles {
  screenshot_copied: "Captura copiada",
  screenshot_saved: "Captura guardada",
  screenshot_save_failed: "Error al guardar",

  capture_failed: "Error de captura",

  video_saved: "Vídeo guardado",
  video_save_failed: "Error al guardar el vídeo",

  shortcut_warning_title: "Hay atajos que revisar",

  body_image_copied: "Imagen copiada al portapapeles",
  body_saved_to_prefix: "Guardado en: ",
  body_check_logs: "Algo salió mal. Revisa los registros para más detalles.",
  body_shortcut_conflict_warning: "Algunos atajos no están disponibles porque otra aplicación ya los está usando. Revisa tus atajos en Ajustes.",
  shortcut_warning_open_settings: "Abrir ajustes",
  shortcut_warning_dismiss: "Ahora no",
};

// ── Italian ──────────────────────────────────────────────────────────────────

static IT: NotifTitles = NotifTitles {
  screenshot_copied: "Screenshot copiato",
  screenshot_saved: "Screenshot salvato",
  screenshot_save_failed: "Salvataggio fallito",

  capture_failed: "Screenshot fallito",

  video_saved: "Video salvato",
  video_save_failed: "Salvataggio video fallito",

  shortcut_warning_title: "Controlla i tasti rapidi",

  body_image_copied: "Immagine copiata negli appunti",
  body_saved_to_prefix: "Salvato in: ",
  body_check_logs: "Si è verificato un errore. Controlla i log per i dettagli.",
  body_shortcut_conflict_warning: "Alcuni tasti rapidi non sono disponibili perché sono già usati da un'altra applicazione. Controllali nelle impostazioni.",
  shortcut_warning_open_settings: "Apri impostazioni",
  shortcut_warning_dismiss: "Non ora",
};

/// Labels for the native screenshot drawing toolbar.
pub struct AnnotationLabels {
  pub text_appearances: [&'static str; 3],
  pub text_style: &'static str,
  pub width_options: [&'static str; 3],
  /// The same three choices for the Highlighter, which paints them three times as thick.
  pub highlighter_width_options: [&'static str; 3],
}

pub fn annotations(lang: &str) -> &'static AnnotationLabels {
  match lang {
    "fr" => &ANNOTATIONS_FR,
    "de" => &ANNOTATIONS_DE,
    "es" => &ANNOTATIONS_ES,
    "it" => &ANNOTATIONS_IT,
    _ => &ANNOTATIONS_EN,
  }
}

static ANNOTATIONS_EN: AnnotationLabels = AnnotationLabels {
  text_style: "Text style ›",
  text_appearances: ["Plain", "Outline", "Background"],
  width_options: ["3 px", "6 px", "10 px"],
  highlighter_width_options: ["9 px", "18 px", "30 px"],
};

static ANNOTATIONS_FR: AnnotationLabels = AnnotationLabels {
  text_style: "Style du texte ›",
  text_appearances: ["Simple", "Contour", "Fond"],
  width_options: ["3 px", "6 px", "10 px"],
  highlighter_width_options: ["9 px", "18 px", "30 px"],
};

static ANNOTATIONS_DE: AnnotationLabels = AnnotationLabels {
  text_style: "Textstil ›",
  text_appearances: ["Normal", "Kontur", "Hintergrund"],
  width_options: ["3 px", "6 px", "10 px"],
  highlighter_width_options: ["9 px", "18 px", "30 px"],
};

static ANNOTATIONS_ES: AnnotationLabels = AnnotationLabels {
  text_style: "Estilo de texto ›",
  text_appearances: ["Simple", "Contorno", "Fondo"],
  width_options: ["3 px", "6 px", "10 px"],
  highlighter_width_options: ["9 px", "18 px", "30 px"],
};

static ANNOTATIONS_IT: AnnotationLabels = AnnotationLabels {
  text_style: "Stile del testo ›",
  text_appearances: ["Semplice", "Contorno", "Sfondo"],
  width_options: ["3 px", "6 px", "10 px"],
  highlighter_width_options: ["9 px", "18 px", "30 px"],
};

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn tray_falls_back_to_english_for_unknown_language() {
    let english = tray("en");
    let unknown = tray("jp");

    assert_eq!(unknown.screenshot, english.screenshot);
    assert_eq!(unknown.record_start, english.record_start);
    assert_eq!(unknown.record_stop, english.record_stop);
    assert_eq!(unknown.record_pause, english.record_pause);
    assert_eq!(unknown.record_resume, english.record_resume);
    assert_eq!(unknown.settings, english.settings);
    assert_eq!(unknown.quit, english.quit);
  }

  #[test]
  fn selection_edit_hint_names_alt_in_every_language_and_stays_short() {
    assert_eq!(selection_edit_hint("jp"), selection_edit_hint("en"));
    for lang in ["en", "fr", "de", "es", "it"] {
      let hint = selection_edit_hint(lang);
      assert!(hint.contains("Alt"), "{lang}: {hint}");
      // One muted line in the dimensions panel; longer copy would widen or clip it.
      assert!(hint.chars().count() <= 28, "{lang}: {hint}");
    }
  }

  #[test]
  fn notification_titles_fall_back_to_english_for_unknown_language() {
    let english = get("en");
    let unknown = get("jp");

    assert_eq!(unknown.screenshot_copied, english.screenshot_copied);
    assert_eq!(unknown.video_save_failed, english.video_save_failed);
    assert_eq!(unknown.body_check_logs, english.body_check_logs);
  }

  #[test]
  fn screenshot_notification_titles_use_generic_screenshot_copy() {
    let titles = screenshot_notif_titles("en");

    assert_eq!(titles.copied, "Screenshot copied");
    assert_eq!(titles.saved, "Screenshot saved");
    assert_eq!(titles.save_failed, "Screenshot save failed");
  }

  #[test]
  fn video_notification_titles_use_generic_video_copy() {
    let titles = video_notif_titles("en");

    assert_eq!(titles.saved, "Video saved");
  }

  #[test]
  fn generic_error_bodies_do_not_embed_runtime_details() {
    let titles = get("en");

    assert_eq!(
      titles.body_check_logs,
      "Something went wrong. Check the logs for details."
    );
  }
}
