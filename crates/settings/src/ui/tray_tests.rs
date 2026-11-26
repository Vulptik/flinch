//! Tray menu windows on the shared headless platform: layout sizes, pointer and keyboard paths.
use super::*;
use slint::platform::Key;

fn render(window: &slint::Window, width: f32, height: f32, name: &str) -> Vec<slint::Rgb8Pixel> {
  use slint::Rgb8Pixel;
  let (width, height) = (width as u32, height as u32);
  window.request_redraw();
  let mut pixels = vec![Rgb8Pixel::default(); (width * height) as usize];
  SOFTWARE_WINDOW.with(|current| {
    current
      .borrow()
      .as_ref()
      .expect("headless renderer")
      .draw_if_needed(|renderer| {
        renderer.render(&mut pixels, width as usize);
      });
  });
  if let Some(directory) = std::env::var_os("FLINCH_SETTINGS_ERROR_PREVIEW_OUTPUT") {
    let directory = std::path::PathBuf::from(directory);
    std::fs::create_dir_all(&directory).expect("preview directory");
    let file = std::fs::File::create(directory.join(format!("{name}.png"))).expect("preview file");
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgb);
    let bytes = pixels
      .iter()
      .flat_map(|p| [p.r, p.g, p.b])
      .collect::<Vec<_>>();
    encoder
      .write_header()
      .expect("PNG header")
      .write_image_data(&bytes)
      .expect("PNG pixels");
  }
  pixels
}

fn key(window: &slint::Window, key: Key) {
  let text: slint::SharedString = key.into();
  window.dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.clone() });
  window.dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
}

fn hover(window: &slint::Window, x: f32, y: f32) {
  window.dispatch_event(slint::platform::WindowEvent::PointerMoved {
    position: slint::LogicalPosition::new(x, y),
  });
  slint::platform::update_timers_and_animations();
}

fn row(id: &str, label: &str, icon: &str, separator: bool, submenu: bool) -> TrayMenuRow {
  TrayMenuRow {
    id: id.into(),
    label: label.into(),
    detail: Default::default(),
    icon: icon.into(),
    separator,
    accent: false,
    submenu,
  }
}

/// Row tops: 4px padding, 32px rows, 9px separators before Recent Captures and Settings.
pub(super) fn menu_states() {
  let window = TrayMenuWindow::new().expect("headless component");
  let rows = vec![
    row("screenshot", "Take Screenshot", "camera", false, false),
    row("capture-edit", "Capture and Edit", "pencil", false, false),
    row("copy-text", "Copy Text", "type", false, false),
    row("record", "Start Recording", "record", false, false),
    row("recent", "Recent Captures", "history", true, true),
    row("hotkeys", "Hotkeys", "keyboard", true, false),
    row("settings", "Settings", "settings", false, false),
    row("quit", "Exit", "power", false, false),
  ];
  // The host measures labels before the window exists, so measuring must not wait for a paint.
  window.set_measure_text("Take Screenshot".into());
  let label = window.get_measured_width();
  assert!(label > 60.0, "{label}");
  window.set_measure_text("Take Screenshot, but longer".into());
  assert!(
    window.get_measured_width() > label,
    "measuring follows the text"
  );
  window.set_rows(ModelRc::new(VecModel::from(rows)));
  let (width, height) = (300.0, 4.0 + 8.0 * 32.0 + 2.0 * 9.0 + 4.0);
  window.set_menu_width(width);
  window.set_menu_height(height);
  window
    .window()
    .set_size(slint::LogicalSize::new(width, height));
  window.show().expect("offscreen component");
  // Until the host reveals it, the window shows only its surface and border.
  window.set_slide_y(8.0);
  let hidden = render(window.window(), width, height, "tray-menu-hidden");
  let surface = slint::Rgb8Pixel {
    r: 0x1c,
    g: 0x1c,
    b: 0x1f,
  };
  let label_area = |pixels: &[slint::Rgb8Pixel]| {
    (8..28)
      .flat_map(|y| (40..140).map(move |x| (y, x)))
      .filter(|(y, x)| pixels[y * width as usize + x] != surface)
      .count()
  };
  assert_eq!(label_area(&hidden), 0, "no content before the reveal");
  window.set_shown(true);
  let activated = Rc::new(RefCell::new(Vec::new()));
  let received = activated.clone();
  window.on_activate(move |id| received.borrow_mut().push(id.to_string()));
  let hovered = Rc::new(RefCell::new(Vec::new()));
  let received = hovered.clone();
  window.on_hover_row(move |index| received.borrow_mut().push(index));
  let submenus = Rc::new(RefCell::new(Vec::new()));
  let received = submenus.clone();
  window.on_open_submenu(move |index, keyboard| received.borrow_mut().push((index, keyboard)));
  let dismissed = Rc::new(Cell::new(0));
  let received = dismissed.clone();
  window.on_dismiss(move || received.set(received.get() + 1));
  let revealed = render(window.window(), width, height, "tray-menu");
  assert!(
    label_area(&revealed) > 20,
    "the first label is drawn once revealed"
  );
  assert_eq!(
    window.get_natural_height(),
    height,
    "the rows need exactly the height the host gives them"
  );

  hover(window.window(), 60.0, 4.0 + 32.0 + 16.0);
  assert_eq!(window.get_highlight(), 1, "hover highlights its row");
  hover(window.window(), 60.0, 141.0 + 16.0);
  assert_eq!(window.get_highlight(), 4);
  assert_eq!(
    hovered.borrow().last(),
    Some(&4),
    "hovering Recent Captures reports it"
  );
  crate::interaction_check::click(window.window(), 60.0, 141.0 + 16.0);
  assert_eq!(
    submenus.borrow_mut().drain(..).collect::<Vec<_>>(),
    [(4, false)]
  );
  crate::interaction_check::click(window.window(), 60.0, 4.0 + 16.0);
  for row in 5..8 {
    crate::interaction_check::click(window.window(), 60.0, 4.0 + row as f32 * 32.0 + 18.0 + 16.0);
  }
  assert_eq!(
    activated.borrow_mut().drain(..).collect::<Vec<_>>(),
    ["screenshot", "hotkeys", "settings", "quit"]
  );
  crate::interaction_check::click(window.window(), 60.0, 4.0 + 4.0 * 32.0 + 4.0);
  assert!(
    activated.borrow().is_empty() && submenus.borrow().is_empty(),
    "separators do nothing"
  );

  window.set_highlight(-1);
  window.invoke_focus_menu();
  key(window.window(), Key::DownArrow);
  assert_eq!(window.get_highlight(), 0, "Down starts at the top");
  key(window.window(), Key::UpArrow);
  assert_eq!(window.get_highlight(), 7, "Up wraps to Exit");
  key(window.window(), Key::DownArrow);
  assert_eq!(window.get_highlight(), 0, "Down wraps to the top");
  key(window.window(), Key::Return);
  assert_eq!(
    activated.borrow_mut().drain(..).collect::<Vec<_>>(),
    ["screenshot"]
  );
  key(window.window(), Key::RightArrow);
  assert!(submenus.borrow().is_empty(), "Right only opens submenus");
  window.set_highlight(4);
  render(window.window(), width, height, "tray-menu-keyboard");
  key(window.window(), Key::RightArrow);
  key(window.window(), Key::Return);
  assert_eq!(
    submenus.borrow_mut().drain(..).collect::<Vec<_>>(),
    [(4, true), (4, true)]
  );
  assert!(
    activated.borrow().is_empty(),
    "Recent Captures is not an action"
  );
  key(window.window(), Key::Escape);
  assert_eq!(dismissed.get(), 1, "Escape");

  window.hide().expect("hide");
}

pub(super) fn recent_states() {
  let window = TrayRecentWindow::new().expect("headless component");
  window.set_empty_label("No captures yet".into());
  window.set_copy_label("Copy".into());
  window.set_show_label("Show in Folder".into());
  window.set_folder_label("Open Captures Folder".into());
  let mut pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(4, 4);
  for pixel in pixels.make_mut_slice() {
    *pixel = slint::Rgba8Pixel {
      r: 217,
      g: 47,
      b: 47,
      a: 255,
    };
  }
  let rows = vec![
    TrayRecentRow {
      name: "2026-11-08_14-32-05_Discord_screenshot.png".into(),
      time: "14:32".into(),
      kind: "image".into(),
      thumbnail: slint::Image::from_rgba8(pixels),
      has_thumbnail: true,
    },
    TrayRecentRow {
      name: "2026-11-08_14-20-11_Chrome_recording.mp4".into(),
      time: "14:20".into(),
      kind: "video".into(),
      thumbnail: slint::Image::default(),
      has_thumbnail: false,
    },
  ];
  window.set_rows(ModelRc::new(VecModel::from(rows)));
  let (width, height) = (window.get_menu_width(), 4.0 + 2.0 * 52.0 + 9.0 + 32.0 + 4.0);
  window.set_menu_height(height);
  window
    .window()
    .set_size(slint::LogicalSize::new(width, height));
  window.show().expect("offscreen component");
  window.set_shown(true);
  let acts = Rc::new(RefCell::new(Vec::new()));
  let received = acts.clone();
  window.on_act(move |index, action| received.borrow_mut().push((index, action.to_string())));
  let folders = Rc::new(Cell::new(0));
  let received = folders.clone();
  window.on_folder(move || received.set(received.get() + 1));
  let backs = Rc::new(Cell::new(0));
  let received = backs.clone();
  window.on_back(move || received.set(received.get() + 1));
  let hovers = Rc::new(RefCell::new(Vec::new()));
  let received = hovers.clone();
  window.on_hovered(move |hovered| received.borrow_mut().push(hovered));
  render(window.window(), width, height, "tray-recent");
  assert_eq!(window.get_natural_height(), height, "capture rows");

  hover(window.window(), 120.0, 4.0 + 52.0 + 26.0);
  assert_eq!(window.get_highlight(), 1);
  assert!(
    hovers.borrow().contains(&true),
    "reaching a row reports the submenu as hovered"
  );
  render(window.window(), width, height, "tray-recent-hover");
  let copy_x = 4.0 + (width - 8.0) - 66.0 + 15.0; // the copy button
  crate::interaction_check::click(window.window(), copy_x, 4.0 + 52.0 + 26.0);
  crate::interaction_check::click(window.window(), copy_x + 32.0, 4.0 + 52.0 + 26.0);
  // The video row has no pin button: its slot does nothing.
  crate::interaction_check::click(window.window(), copy_x - 32.0, 4.0 + 52.0 + 26.0);
  crate::interaction_check::click(window.window(), 120.0, 4.0 + 26.0);
  assert_eq!(
    acts.borrow_mut().drain(..).collect::<Vec<_>>(),
    [
      (1, "copy".to_string()),
      (1, "show".into()),
      (1, "open".into()),
      (0, "open".into())
    ]
  );
  // The screenshot row pins.
  hover(window.window(), 120.0, 4.0 + 26.0);
  render(window.window(), width, height, "tray-recent-pin");
  crate::interaction_check::click(window.window(), copy_x - 32.0, 4.0 + 26.0);
  assert_eq!(
    acts.borrow_mut().drain(..).collect::<Vec<_>>(),
    [(0, "pin".to_string())]
  );
  crate::interaction_check::click(window.window(), 120.0, 4.0 + 104.0 + 9.0 + 16.0);
  assert_eq!(folders.get(), 1, "folder row");

  window.set_highlight(-1);
  window.invoke_focus_menu();
  key(window.window(), Key::DownArrow);
  for _ in 0..4 {
    key(window.window(), Key::Tab);
    key(window.window(), Key::Return);
  }
  assert_eq!(
    acts.borrow_mut().drain(..).collect::<Vec<_>>(),
    [
      (0, "pin".to_string()),
      (0, "copy".into()),
      (0, "show".into()),
      (0, "open".into())
    ],
    "Tab cycles open, pin, copy and show on a highlighted screenshot"
  );
  key(window.window(), Key::DownArrow);
  for _ in 0..3 {
    key(window.window(), Key::Tab);
    key(window.window(), Key::Return);
  }
  key(window.window(), Key::Tab);
  key(window.window(), Key::Tab);
  key(window.window(), Key::Backtab);
  key(window.window(), Key::Return);
  key(window.window(), Key::Backtab);
  key(window.window(), Key::Return);
  assert_eq!(
    acts.borrow_mut().drain(..).collect::<Vec<_>>(),
    [
      (1, "copy".to_string()),
      (1, "show".into()),
      (1, "open".into()),
      (1, "copy".into()),
      (1, "open".into())
    ],
    "a recording has no pin part, forward or back"
  );
  key(window.window(), Key::UpArrow);
  key(window.window(), Key::UpArrow);
  assert_eq!(window.get_highlight(), 2, "Up wraps to the folder row");
  assert_eq!(window.get_part(), 0, "moving rows returns to open");
  key(window.window(), Key::Tab);
  assert_eq!(window.get_part(), 0, "the folder row has no actions");
  key(window.window(), Key::Return);
  assert_eq!(folders.get(), 2);
  key(window.window(), Key::LeftArrow);
  key(window.window(), Key::Escape);
  assert_eq!(backs.get(), 2, "Left and Escape return to the menu");

  window.set_rows(ModelRc::new(VecModel::from(Vec::<TrayRecentRow>::new())));
  let empty = 4.0 + 36.0 + 9.0 + 32.0 + 4.0;
  window.set_menu_height(empty);
  window.set_highlight(-1);
  key(window.window(), Key::DownArrow);
  assert_eq!(
    window.get_highlight(),
    0,
    "with no captures only the folder row is selectable"
  );
  key(window.window(), Key::Return);
  assert_eq!(folders.get(), 3);
  assert!(acts.borrow().is_empty());
  window
    .window()
    .set_size(slint::LogicalSize::new(width, empty));
  render(window.window(), width, empty, "tray-recent-empty");
  assert_eq!(window.get_natural_height(), empty, "the empty line");
  window.hide().expect("hide");
}

pub(super) fn choice_states() {
  let window = TrayChoiceWindow::new().expect("headless component");
  let choices = ["Off", "3 seconds", "5 seconds", "10 seconds"];
  window.set_choices(ModelRc::new(VecModel::from(
    choices
      .iter()
      .map(|choice| slint::SharedString::from(*choice))
      .collect::<Vec<_>>(),
  )));
  window.set_selected(2);
  let (width, height) = (180.0, 4.0 + 4.0 * 32.0 + 4.0);
  window.set_menu_width(width);
  window.set_menu_height(height);
  window
    .window()
    .set_size(slint::LogicalSize::new(width, height));
  window.show().expect("offscreen component");
  window.set_shown(true);
  let chosen = Rc::new(RefCell::new(Vec::new()));
  let received = chosen.clone();
  window.on_choose(move |index| received.borrow_mut().push(index));
  let backs = Rc::new(Cell::new(0));
  let received = backs.clone();
  window.on_back(move || received.set(received.get() + 1));
  let hovers = Rc::new(RefCell::new(Vec::new()));
  let received = hovers.clone();
  window.on_hovered(move |hovered| received.borrow_mut().push(hovered));
  render(window.window(), width, height, "tray-delay");
  assert_eq!(window.get_natural_height(), height, "one row per choice");

  hover(window.window(), 60.0, 4.0 + 32.0 + 16.0);
  assert_eq!(window.get_highlight(), 1);
  assert!(
    hovers.borrow().contains(&true),
    "reaching a row keeps the submenu"
  );
  render(window.window(), width, height, "tray-delay-hover");
  crate::interaction_check::click(window.window(), 60.0, 4.0 + 3.0 * 32.0 + 16.0);
  assert_eq!(chosen.borrow_mut().drain(..).collect::<Vec<_>>(), [3]);

  window.set_highlight(-1);
  window.invoke_focus_menu();
  key(window.window(), Key::UpArrow);
  assert_eq!(
    window.get_highlight(),
    3,
    "Up from nothing starts at the last choice"
  );
  key(window.window(), Key::DownArrow);
  assert_eq!(window.get_highlight(), 0, "Down wraps");
  key(window.window(), Key::Return);
  key(window.window(), Key::DownArrow);
  key(window.window(), Key::Space);
  assert_eq!(chosen.borrow_mut().drain(..).collect::<Vec<_>>(), [0, 1]);
  key(window.window(), Key::LeftArrow);
  key(window.window(), Key::Escape);
  assert_eq!(backs.get(), 2, "Left and Escape return to the menu");
  window.hide().expect("hide");
}

/// The Capture Delay row shows its current choice between the label and the chevron.
pub(super) fn delay_row_states() {
  let window = TrayMenuWindow::new().expect("headless component");
  let mut delay = row("delay", "Capture Delay", "timer", true, true);
  delay.detail = "3 seconds".into();
  window.set_rows(ModelRc::new(VecModel::from(vec![
    row("screenshot", "Take Screenshot", "camera", false, false),
    delay,
    row("recent", "Recent Captures", "history", false, true),
  ])));
  let (width, height) = (300.0, 4.0 + 3.0 * 32.0 + 9.0 + 4.0);
  window.set_menu_width(width);
  window.set_menu_height(height);
  window
    .window()
    .set_size(slint::LogicalSize::new(width, height));
  window.show().expect("offscreen component");
  window.set_shown(true);
  window.set_submenu_row(1);
  let pixels = render(window.window(), width, height, "tray-menu-delay");
  assert_eq!(window.get_natural_height(), height);
  let surface = slint::Rgb8Pixel {
    r: 0x1c,
    g: 0x1c,
    b: 0x1f,
  };
  // The detail sits right of the label, before the chevron, on the open row's highlight.
  let row_top = (4.0 + 32.0 + 9.0) as usize;
  let detail = (row_top + 10..row_top + 22)
    .flat_map(|y| (180..260).map(move |x| (y, x)))
    .filter(|(y, x)| {
      let pixel = pixels[y * width as usize + x];
      pixel != surface && pixel.r > 0x60
    })
    .count();
  assert!(detail > 10, "the current delay is drawn, got {detail}");
  window.hide().expect("hide");
}
