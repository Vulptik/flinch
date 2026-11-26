//! Bounded native multiline text field. Only posts completion to the parent:
//! synchronous callbacks never reborrow the owning editor state.
use anyhow::{Result, ensure};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
  ANTIALIASED_QUALITY, BLACK_BRUSH, BitBlt, CreateFontIndirectW, CreatePatternBrush,
  CreateSolidBrush, DeleteObject, GetStockObject, HBRUSH, HDC, HFONT, LOGFONTW, OPAQUE, SRCCOPY,
  SetBkColor, SetBkMode, SetBrushOrgEx, SetTextColor, TRANSPARENT,
};
use windows::Win32::UI::Controls::{EM_GETFIRSTVISIBLELINE, EM_GETSEL, EM_SETLIMITTEXT, EM_SETSEL};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, SetFocus};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

pub const COMPLETE: u32 = WM_APP + 37;
pub const CHANGED: u32 = WM_APP + 38;
const BACKGROUND: windows::core::PCWSTR = w!("FlinchInlineTextBackground");
const FOREGROUND: windows::core::PCWSTR = w!("FlinchInlineTextForeground");
const BACKGROUND_COLOR: windows::core::PCWSTR = w!("FlinchInlineTextBackgroundColor");
const CAPTURE_BACKGROUND: windows::core::PCWSTR = w!("FlinchInlineTextCaptureBackground");
const MASK_PAINT: windows::core::PCWSTR = w!("FlinchInlineTextMaskPaint");
pub fn background(dc: HDC, child: HWND) -> Option<LRESULT> {
  // SAFETY: callback uses only child-owned HWND properties, avoiding any reentrant editor borrow.
  unsafe {
    if !GetPropW(child, MASK_PAINT).is_invalid() {
      if SetTextColor(dc, windows::Win32::Foundation::COLORREF(0x00ffffff)).0 == u32::MAX
        || SetBkColor(dc, windows::Win32::Foundation::COLORREF(0)).0 == u32::MAX
        || SetBkMode(dc, OPAQUE) == 0
      {
        tracing::warn!("Failed to set native text mask colors");
      }
      return Some(LRESULT(GetStockObject(BLACK_BRUSH).0 as isize));
    }
    let brush = GetPropW(child, BACKGROUND);
    if brush.is_invalid() {
      return None;
    }
    let color = GetPropW(child, FOREGROUND).0 as usize;
    let background = GetPropW(child, BACKGROUND_COLOR).0 as usize;
    let captured = !GetPropW(child, CAPTURE_BACKGROUND).is_invalid();
    if SetBkMode(dc, if captured { TRANSPARENT } else { OPAQUE }) == 0 {
      tracing::warn!("Failed to set inline text background mode");
    }
    if captured && !SetBrushOrgEx(dc, 0, 0, None).as_bool() {
      tracing::warn!("Failed to align inline screenshot text background");
    }
    if SetTextColor(
      dc,
      windows::Win32::Foundation::COLORREF(color.saturating_sub(1) as u32),
    )
    .0 == u32::MAX
      || SetBkColor(
        dc,
        windows::Win32::Foundation::COLORREF(background.saturating_sub(1) as u32),
      )
      .0 == u32::MAX
    {
      tracing::warn!("Failed to apply inline text colors");
    }
    Some(LRESULT(brush.0 as isize))
  }
}
pub struct TextField {
  pub hwnd: HWND,
  font: HFONT,
  line_height: i32,
  brush: HBRUSH,
  size: i32,
  color: [u8; 3],
  appearance: crate::screenshot::annotations::TextAppearance,
  backdrop_key: Option<BackdropKey>,
  suspended: bool,
}

#[derive(PartialEq, Eq)]
struct BackdropKey {
  rect: [i32; 4],
  revision: u64,
  appearance: crate::screenshot::annotations::TextAppearance,
  content: String,
  selection: usize,
  first_line: isize,
}
impl TextField {
  pub fn new(parent: HWND, rect: RECT, size: i32, content: &str) -> Result<Self> {
    let mut lf = LOGFONTW {
      lfHeight: -size,
      lfQuality: ANTIALIASED_QUALITY,
      ..Default::default()
    };
    for (to, from) in lf.lfFaceName.iter_mut().zip("Segoe UI".encode_utf16()) {
      *to = from;
    }
    // SAFETY: font descriptor is initialized and contains a null-terminated face name.
    let font = unsafe { CreateFontIndirectW(&lf) };
    ensure!(!font.is_invalid(), "Failed to create inline text font");
    let mut field = Self {
      hwnd: HWND::default(),
      font,
      line_height: size,
      brush: HBRUSH::default(),
      size,
      color: [240, 240, 245],
      appearance: crate::screenshot::annotations::TextAppearance::Plain,
      backdrop_key: None,
      suspended: false,
    };
    field.line_height = font_line_height(font)?;
    // SAFETY: the field exclusively owns the background brush through child destruction.
    field.brush = unsafe { CreateSolidBrush(windows::Win32::Foundation::COLORREF(0x00171414)) };
    ensure!(
      !field.brush.is_invalid(),
      "Failed to create inline text background"
    );
    let wide: Vec<u16> = content.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: parent belongs to this thread; class/content storage remain live during creation.
    field.hwnd = unsafe {
      CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("EDIT"),
        windows::core::PCWSTR(wide.as_ptr()),
        WS_CHILD
          | WINDOW_STYLE((ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN | ES_NOHIDESEL) as u32),
        rect.left,
        rect.top,
        (rect.right - rect.left).max(1),
        (rect.bottom - rect.top).max(1),
        Some(parent),
        None,
        None,
        None,
      )
    }?;
    // SAFETY: live child field; Windows stores its borrowed font until field destruction.
    unsafe {
      SetPropW(
        field.hwnd,
        BACKGROUND,
        Some(windows::Win32::Foundation::HANDLE(field.brush.0)),
      )?;
      SetPropW(
        field.hwnd,
        FOREGROUND,
        Some(windows::Win32::Foundation::HANDLE(
          0x00f5f0f1usize as *mut _,
        )),
      )?;
      SetPropW(
        field.hwnd,
        BACKGROUND_COLOR,
        Some(windows::Win32::Foundation::HANDLE(
          0x00171415usize as *mut _,
        )),
      )?;
      ensure!(
        SetWindowSubclass(field.hwnd, Some(edit_proc), 1, 0).as_bool(),
        "Failed to subclass inline text field"
      );
      SendMessageW(
        field.hwnd,
        WM_SETFONT,
        Some(WPARAM(font.0 as usize)),
        Some(LPARAM(1)),
      );
      SendMessageW(field.hwnd, EM_SETLIMITTEXT, Some(WPARAM(4096)), None);
      SendMessageW(field.hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
    }
    Ok(field)
  }
  pub fn style(
    &mut self,
    size: i32,
    color: [u8; 3],
    appearance: crate::screenshot::annotations::TextAppearance,
  ) -> Result<()> {
    if self.size == size && self.color == color && self.appearance == appearance {
      return Ok(());
    }
    let mut lf = LOGFONTW {
      lfHeight: -size,
      lfQuality: ANTIALIASED_QUALITY,
      ..Default::default()
    };
    for (to, from) in lf.lfFaceName.iter_mut().zip("Segoe UI".encode_utf16()) {
      *to = from;
    }
    // SAFETY: initialized descriptor; field and font belong exclusively to the overlay thread.
    unsafe {
      let font = CreateFontIndirectW(&lf);
      ensure!(!font.is_invalid(), "Failed to update inline text font");
      let line_height = match font_line_height(font) {
        Ok(height) => height,
        Err(error) => {
          if !DeleteObject(font.into()).as_bool() {
            tracing::warn!("Failed to release unused inline text font");
          }
          return Err(error);
        },
      };
      let rgb = color[0] as usize | (color[1] as usize) << 8 | (color[2] as usize) << 16;
      if let Err(error) = SetPropW(
        self.hwnd,
        FOREGROUND,
        Some(windows::Win32::Foundation::HANDLE((rgb + 1) as *mut _)),
      ) {
        if !DeleteObject(font.into()).as_bool() {
          tracing::warn!("Failed to release unused inline text font");
        }
        return Err(error.into());
      }
      let luminance = 299 * color[0] as u32 + 587 * color[1] as u32 + 114 * color[2] as u32;
      let background_color = if luminance < 60000 {
        0x00faf5f5
      } else {
        0x00171414
      };
      let brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(background_color));
      if brush.is_invalid() {
        if !DeleteObject(font.into()).as_bool() {
          tracing::warn!("Failed to release unused inline text font");
        }
        anyhow::bail!("Failed to update inline text background");
      }
      let properties = SetPropW(
        self.hwnd,
        BACKGROUND_COLOR,
        Some(windows::Win32::Foundation::HANDLE(
          (background_color as usize + 1) as *mut _,
        )),
      )
      .and_then(|()| {
        SetPropW(
          self.hwnd,
          BACKGROUND,
          Some(windows::Win32::Foundation::HANDLE(brush.0)),
        )
      });
      if let Err(error) = properties {
        if !DeleteObject(brush.into()).as_bool() {
          tracing::warn!("Failed to release unused inline text background");
        }
        if !DeleteObject(font.into()).as_bool() {
          tracing::warn!("Failed to release unused inline text font");
        }
        return Err(error.into());
      }
      let old_brush = std::mem::replace(&mut self.brush, brush);
      if !DeleteObject(old_brush.into()).as_bool() {
        tracing::warn!("Failed to release replaced inline text background");
      }
      SendMessageW(
        self.hwnd,
        WM_SETFONT,
        Some(WPARAM(font.0 as usize)),
        Some(LPARAM(1)),
      );
      let old = std::mem::replace(&mut self.font, font);
      self.line_height = line_height;
      if !DeleteObject(old.into()).as_bool() {
        tracing::warn!("Failed to release replaced inline text font");
      }
    }
    self.size = size;
    self.color = color;
    self.appearance = appearance;
    self.backdrop_key = None;
    Ok(())
  }
  pub fn backdrop(
    &mut self,
    dc: HDC,
    rect: RECT,
    revision: u64,
    appearance: crate::screenshot::annotations::TextAppearance,
  ) -> Result<()> {
    // SAFETY: native queries target this field and never mutate the parent/editor.
    let (selection, first_line) = unsafe {
      (
        SendMessageW(self.hwnd, EM_GETSEL, None, None).0 as usize,
        SendMessageW(self.hwnd, EM_GETFIRSTVISIBLELINE, None, None).0,
      )
    };
    let key = BackdropKey {
      rect: [rect.left, rect.top, rect.right, rect.bottom],
      revision,
      appearance,
      content: if appearance == crate::screenshot::annotations::TextAppearance::Plain {
        String::new()
      } else {
        self.content()?
      },
      selection: if appearance == crate::screenshot::annotations::TextAppearance::Plain {
        0
      } else {
        selection
      },
      first_line: if appearance == crate::screenshot::annotations::TextAppearance::Plain {
        0
      } else {
        first_line
      },
    };
    if self.backdrop_key.as_ref() == Some(&key) {
      return Ok(());
    }
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    ensure!(
      width > 0 && height > 0 && width as i64 * height as i64 * 4 <= 16 * 1024 * 1024,
      "Inline text background exceeds its memory budget"
    );
    let mut tile = crate::overlay::gdi::layered::LayeredBitmap::new(width, height)
      .map_err(anyhow::Error::msg)?;
    // SAFETY: source and destination DCs are thread-owned; tile copies only this bounded field region.
    unsafe {
      BitBlt(
        tile.dc,
        0,
        0,
        width,
        height,
        Some(dc),
        rect.left,
        rect.top,
        SRCCOPY,
      )
    }?;
    // Native selection takes precedence over readability decoration while text is selected.
    if appearance != crate::screenshot::annotations::TextAppearance::Plain
      && selection & 0xffff == selection >> 16
    {
      // Native line height bounds glyphs even when the editable field is much taller.
      // Include decoration padding; clipping still follows the actual child width/scroll state.
      let mask_height = height.min(self.content_height()?.saturating_add(8)).max(1);
      let mut mask = crate::overlay::gdi::layered::LayeredBitmap::new(width, mask_height)
        .map_err(anyhow::Error::msg)?;
      // SAFETY: marker changes only color callbacks for a synchronous, bounded child print.
      unsafe {
        SetPropW(
          self.hwnd,
          MASK_PAINT,
          Some(windows::Win32::Foundation::HANDLE(std::ptr::dangling_mut())),
        )
      }?;
      // SAFETY: EDIT paints its own font/wrapping/scroll position into the owned mask DC.
      unsafe {
        SendMessageW(
          self.hwnd,
          WM_PRINTCLIENT,
          Some(WPARAM(mask.dc.0 as usize)),
          Some(LPARAM((PRF_CLIENT | PRF_ERASEBKGND) as isize)),
        )
      };
      // SAFETY: remove before readback can fail so normal input cannot inherit mask colors.
      unsafe { RemovePropW(self.hwnd, MASK_PAINT) }?;
      // SAFETY: finish both this thread's GDI writes before direct DIB access.
      ensure!(
        unsafe { windows::Win32::Graphics::Gdi::GdiFlush() }.as_bool(),
        "Failed to synchronize inline text mask"
      );
      let luminance =
        299 * self.color[0] as u32 + 587 * self.color[1] as u32 + 114 * self.color[2] as u32;
      let contrast = if luminance > 145000 {
        [20, 20, 23]
      } else {
        [255, 255, 255]
      };
      apply_text_backing(
        &mut tile.pixels_mut()[..width as usize * mask_height as usize * 4],
        mask.pixels_mut(),
        width as usize,
        mask_height as usize,
        appearance,
        [contrast[2], contrast[1], contrast[0]],
      );
    }
    // SAFETY: brush copies a live DIB and is owned exclusively by this field.
    let brush = unsafe { CreatePatternBrush(tile.hbitmap) };
    ensure!(
      !brush.is_invalid(),
      "Failed to create inline screenshot background"
    );
    // SAFETY: callback reads only child properties; no parent/editor reference is published.
    if let Err(error) = unsafe {
      SetPropW(
        self.hwnd,
        BACKGROUND,
        Some(windows::Win32::Foundation::HANDLE(brush.0)),
      )
    } {
      // SAFETY: unused brush has not been installed or selected into any DC.
      if !unsafe { DeleteObject(brush.into()) }.as_bool() {
        tracing::warn!("Failed to release unused screenshot text brush");
      }
      return Err(error.into());
    }
    let old = std::mem::replace(&mut self.brush, brush);
    // SAFETY: property now references the replacement brush; the old brush is no longer used by the child.
    if !unsafe { DeleteObject(old.into()) }.as_bool() {
      tracing::warn!("Failed to release replaced screenshot text brush");
    }
    // SAFETY: marker property changes only native child painting; invalidation never reenters editor mutation.
    unsafe {
      SetPropW(
        self.hwnd,
        CAPTURE_BACKGROUND,
        Some(windows::Win32::Foundation::HANDLE(std::ptr::dangling_mut())),
      )
    }?;
    self.backdrop_key = Some(key);
    // SAFETY: reveal only after the screenshot brush is installed. Showing/focusing may
    // synchronously notify the parent, whose callbacks post actions without borrowing editor state.
    unsafe {
      if !IsWindowVisible(self.hwnd).as_bool() {
        // Return value reports previous visibility rather than success.
        let _ = ShowWindow(self.hwnd, SW_SHOWNA);
        if let Err(error) = SetFocus(Some(self.hwnd)) {
          ensure!(
            windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() == self.hwnd,
            "Failed to focus inline text field: {error}"
          );
        }
      }
    }
    // SAFETY: repaint this live child once after its backing changes; no timer or callback loop is introduced.
    if !unsafe { windows::Win32::Graphics::Gdi::InvalidateRect(Some(self.hwnd), None, true) }
      .as_bool()
    {
      tracing::warn!("Failed to repaint inline screenshot text");
    }
    Ok(())
  }
  pub fn suspend_for_move(&mut self) {
    if self.suspended {
      return;
    }
    self.suspended = true;
    // SAFETY: owned child on this thread; hide once, retaining native text/selection/IME state.
    unsafe {
      let _ = ShowWindow(self.hwnd, SW_HIDE);
    }
    // Force one fresh backdrop on release/cancellation before the child is revealed.
    self.backdrop_key = None;
  }
  pub fn synchronize_after_move(&mut self, rect: RECT) -> Result<()> {
    if self.suspended {
      self.layout(rect)?;
      self.suspended = false;
    }
    Ok(())
  }
  pub fn layout(&self, rect: RECT) -> Result<()> {
    // SAFETY: live child is owned by this thread; positioning does not replace content or selection.
    unsafe {
      SetWindowPos(
        self.hwnd,
        None,
        rect.left,
        rect.top,
        (rect.right - rect.left).max(1),
        (rect.bottom - rect.top).max(1),
        // Parent repaint restores the old rectangle and installs the new backdrop before
        // invalidating the child. Never copy old glyphs or paint with the old pattern brush.
        SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOREDRAW | SWP_NOCOPYBITS,
      )
    }?;
    Ok(())
  }
  pub fn content_height(&self) -> Result<i32> {
    // SAFETY: read the actual EDIT layout, including wrapping and trailing empty lines.
    let lines = unsafe {
      SendMessageW(
        self.hwnd,
        windows::Win32::UI::Controls::EM_GETLINECOUNT,
        None,
        None,
      )
      .0
    };
    ensure!(
      (1..=4096).contains(&lines),
      "Invalid native text line count"
    );
    Ok(lines as i32 * self.line_height)
  }
  pub fn focus(&self) {
    // A suspended editor is revealed/focused by backdrop after its new background is ready.
    // SAFETY: read-only query of this owned child on its overlay thread.
    if !unsafe { IsWindowVisible(self.hwnd) }.as_bool() {
      return;
    }
    // SAFETY: thread-owned field and input queue; native caret/selection are retained.
    if let Err(error) = unsafe { SetFocus(Some(self.hwnd)) }
      && unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() } != self.hwnd
    {
      tracing::warn!(%error, "Failed to return focus to text editor");
    }
  }
  pub fn content(&self) -> Result<String> {
    let mut buffer = vec![0u16; 4097];
    // SAFETY: thread-owned child must still exist before attempting its readback.
    ensure!(
      unsafe { IsWindow(Some(self.hwnd)) }.as_bool(),
      "Inline text field was disconnected"
    );
    // SAFETY: live native child and bounded mutable UTF-16 buffer.
    let count = unsafe {
      windows::Win32::Foundation::SetLastError(windows::Win32::Foundation::WIN32_ERROR(0));
      let count = GetWindowTextW(self.hwnd, &mut buffer);
      if count == 0 {
        ensure!(
          windows::Win32::Foundation::GetLastError().0 == 0,
          "Failed to read inline text field"
        );
      }
      count
    };
    ensure!((0..=4096).contains(&count), "Invalid inline text length");
    let content = String::from_utf16(&buffer[..count as usize])?;
    ensure!(
      content.chars().count() <= 2048,
      "Text exceeds 2048 characters"
    );
    Ok(content)
  }
}

fn font_line_height(font: HFONT) -> Result<i32> {
  use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, GetTextMetricsW, SelectObject, TEXTMETRICW,
  };
  // SAFETY: font is live on this thread; this scratch DC borrows it only for bounded measurement.
  unsafe {
    let dc = CreateCompatibleDC(None);
    ensure!(!dc.is_invalid(), "Failed to create native text metric DC");
    let old = SelectObject(dc, font.into());
    if old.is_invalid() {
      if !DeleteDC(dc).as_bool() {
        tracing::warn!("Failed to release native text metric DC");
      }
      anyhow::bail!("Failed to select native text metric font");
    }
    let mut metrics = TEXTMETRICW::default();
    let measured = GetTextMetricsW(dc, &mut metrics).as_bool();
    SelectObject(dc, old);
    if !DeleteDC(dc).as_bool() {
      tracing::warn!("Failed to release native text metric DC");
    }
    ensure!(
      measured && (1..=512).contains(&metrics.tmHeight),
      "Failed to read native text line height"
    );
    Ok(metrics.tmHeight)
  }
}
impl Drop for TextField {
  fn drop(&mut self) {
    // SAFETY: child/font belong to this thread; destroy the child before deleting its font.
    unsafe {
      if !self.hwnd.is_invalid()
        && IsWindow(Some(self.hwnd)).as_bool()
        && let Err(error) = DestroyWindow(self.hwnd)
      {
        tracing::warn!(%error,"Failed to destroy inline text field");
      }
      if !DeleteObject(self.font.into()).as_bool() {
        tracing::warn!("Failed to release inline text font");
      }
      if !self.brush.is_invalid() && !DeleteObject(self.brush.into()).as_bool() {
        tracing::warn!("Failed to release inline text background");
      }
    }
  }
}
unsafe extern "system" fn edit_proc(
  hwnd: HWND,
  msg: u32,
  wparam: WPARAM,
  lparam: LPARAM,
  id: usize,
  composing: usize,
) -> LRESULT {
  // SAFETY: subclass is attached only to a live thread-owned EDIT control; callbacks post
  // actions rather than calling into parent/editor mutation synchronously.
  unsafe {
    if (msg == WM_IME_STARTCOMPOSITION || msg == WM_IME_ENDCOMPOSITION)
      && !SetWindowSubclass(
        hwnd,
        Some(edit_proc),
        id,
        usize::from(msg == WM_IME_STARTCOMPOSITION),
      )
      .as_bool()
    {
      tracing::warn!("Failed to update text composition state");
    }
    let ctrl = GetKeyState(0x11) < 0;
    let shift = GetKeyState(0x10) < 0;
    if composing == 0 && msg == WM_KEYDOWN && wparam.0 == 0x0d && ctrl {
      return LRESULT(0);
    }
    if composing == 0 && msg == WM_KEYDOWN && (wparam.0 == 0x1b || (wparam.0 == 0x0d && !shift)) {
      match GetParent(hwnd) {
        Ok(parent) => {
          if let Err(error) = PostMessageW(
            Some(parent),
            COMPLETE,
            WPARAM(usize::from(wparam.0 == 0x0d)),
            LPARAM(hwnd.0 as isize),
          ) {
            tracing::error!(%error,"Failed to post inline text completion");
          }
        },
        Err(error) => tracing::error!(%error,"Inline text parent was disconnected"),
      }
      return LRESULT(0);
    }
    if composing == 0
      && msg == WM_CHAR
      && (wparam.0 == 0x1b || wparam.0 == 0x0a || (wparam.0 == 0x0d && (ctrl || !shift)))
    {
      return LRESULT(0);
    }
    if msg == WM_NCDESTROY && !RemoveWindowSubclass(hwnd, Some(edit_proc), id).as_bool() {
      tracing::warn!("Failed to remove inline text subclass");
    }
    let result = DefSubclassProc(hwnd, msg, wparam, lparam);
    if matches!(
      msg,
      WM_KEYUP | WM_LBUTTONUP | WM_MOUSEWHEEL | WM_IME_ENDCOMPOSITION
    ) && let Ok(parent) = GetParent(hwnd)
      && let Err(error) = PostMessageW(Some(parent), CHANGED, WPARAM(0), LPARAM(hwnd.0 as isize))
    {
      tracing::warn!(%error, "Failed to post native text selection update");
    }
    result
  }
}

#[cfg(test)]
thread_local! { static REFERENCE_BACKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(test)]
fn dense_text_backing_reference(
  pixels: &mut [u8],
  mask: &[u8],
  width: usize,
  height: usize,
  appearance: crate::screenshot::annotations::TextAppearance,
  contrast: [u8; 3],
) {
  use crate::screenshot::annotations::TextAppearance;
  let coverage = |x: usize, y: usize| mask[(y * width + x) * 4];
  if appearance == TextAppearance::Background {
    let mut bounds = (width, height, 0, 0);
    for y in 0..height {
      for x in 0..width {
        if coverage(x, y) > 0 {
          bounds = (
            bounds.0.min(x),
            bounds.1.min(y),
            bounds.2.max(x + 1),
            bounds.3.max(y + 1),
          );
        }
      }
    }
    if bounds.0 < bounds.2 {
      for y in bounds.1.saturating_sub(4)..(bounds.3 + 4).min(height) {
        for x in bounds.0.saturating_sub(4)..(bounds.2 + 4).min(width) {
          pixels[(y * width + x) * 4..(y * width + x) * 4 + 3].copy_from_slice(&contrast);
        }
      }
    }
  } else if appearance == TextAppearance::Outline {
    for y in 0..height {
      for x in 0..width {
        let mut alpha = 0u32;
        for (dx, dy) in [
          (-2, -2),
          (0, -2),
          (2, -2),
          (-2, 0),
          (2, 0),
          (-2, 2),
          (0, 2),
          (2, 2),
        ] {
          let (nx, ny) = (x as i32 + dx, y as i32 + dy);
          if nx >= 0 && ny >= 0 && nx < width as i32 && ny < height as i32 {
            alpha = alpha.max(coverage(nx as usize, ny as usize) as u32);
          }
        }
        for (channel, value) in contrast.into_iter().enumerate() {
          let offset = (y * width + x) * 4 + channel;
          pixels[offset] =
            ((pixels[offset] as u32 * (255 - alpha) + value as u32 * alpha + 127) / 255) as u8;
        }
      }
    }
  }
}

fn apply_text_backing(
  pixels: &mut [u8],
  mask: &[u8],
  width: usize,
  height: usize,
  appearance: crate::screenshot::annotations::TextAppearance,
  contrast: [u8; 3],
) {
  use crate::screenshot::annotations::TextAppearance;
  #[cfg(test)]
  if REFERENCE_BACKING.with(|value| value.get()) {
    dense_text_backing_reference(pixels, mask, width, height, appearance, contrast);
    return;
  }
  if appearance == TextAppearance::Background {
    let coverage = |x: usize, y: usize| mask[(y * width + x) * 4];
    let mut bounds = (width, height, 0, 0);
    for y in 0..height {
      for x in 0..width {
        if coverage(x, y) > 0 {
          bounds = (
            bounds.0.min(x),
            bounds.1.min(y),
            bounds.2.max(x + 1),
            bounds.3.max(y + 1),
          );
        }
      }
    }
    if bounds.0 < bounds.2 {
      for y in bounds.1.saturating_sub(4)..(bounds.3 + 4).min(height) {
        for x in bounds.0.saturating_sub(4)..(bounds.2 + 4).min(width) {
          pixels[(y * width + x) * 4..(y * width + x) * 4 + 3].copy_from_slice(&contrast);
        }
      }
    }
  } else if appearance == TextAppearance::Outline {
    // Scatter only nonzero glyph coverage. This is equivalent to the previous
    // eight-neighbour max, without eight mask reads for every empty field pixel.
    let mut outline = vec![0u8; width * height];
    for (index, pixel) in mask.as_chunks::<4>().0.iter().enumerate() {
      let alpha = pixel[0];
      if alpha == 0 {
        continue;
      }
      let (x, y) = ((index % width) as i32, (index / width) as i32);
      for (dx, dy) in [
        (-2, -2),
        (0, -2),
        (2, -2),
        (-2, 0),
        (2, 0),
        (-2, 2),
        (0, 2),
        (2, 2),
      ] {
        let (nx, ny) = (x + dx, y + dy);
        if nx >= 0 && ny >= 0 && nx < width as i32 && ny < height as i32 {
          let target = &mut outline[ny as usize * width + nx as usize];
          *target = (*target).max(alpha);
        }
      }
    }
    for (pixel, alpha) in pixels.as_chunks_mut::<4>().0.iter_mut().zip(outline) {
      if alpha == 0 {
        continue;
      }
      for (value, contrast) in pixel[..3].iter_mut().zip(contrast) {
        *value = ((u32::from(*value) * (255 - u32::from(alpha))
          + u32::from(contrast) * u32::from(alpha)
          + 127)
          / 255) as u8;
      }
    }
  }
}

#[cfg(test)]
mod text_tests {
  use super::*;

  #[test]
  fn native_backdrop_styles_cache_selection_and_move_lifecycle_preserve_hidden_text() {
    use crate::overlay::gdi::window::{
      class_name_for_thread, register_overlay_class, unregister_overlay_class,
    };
    use crate::screenshot::annotations::TextAppearance;
    use windows::Win32::Graphics::Gdi::GetTextColor;
    unsafe extern "system" fn colors(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
      if msg == WM_CTLCOLOREDIT
        && let Some(value) = background(HDC(w.0 as *mut _), HWND(l.0 as *mut _))
      {
        return value;
      }
      // SAFETY: the hidden parent has no application/editor pointer or borrowed state.
      unsafe { DefWindowProcW(hwnd, msg, w, l) }
    }
    struct Fixture(HWND, String);
    impl Drop for Fixture {
      fn drop(&mut self) {
        // SAFETY: this fixture owns its hidden parent and registered window class.
        unsafe {
          DestroyWindow(self.0).unwrap();
        }
        unregister_overlay_class(&self.1);
      }
    }
    let class = format!("{}-text-backdrop", class_name_for_thread());
    register_overlay_class(&class, Some(colors)).unwrap();
    let name: Vec<u16> = class.encode_utf16().chain(Some(0)).collect();
    // SAFETY: a hidden thread-owned parent that is never activated or shown.
    let parent = Fixture(
      unsafe {
        CreateWindowExW(
          WINDOW_EX_STYLE::default(),
          windows::core::PCWSTR(name.as_ptr()),
          w!(""),
          WS_POPUP,
          0,
          0,
          640,
          400,
          None,
          None,
          None,
          None,
        )
      }
      .unwrap(),
      class,
    );
    let rect = RECT {
      left: 20,
      top: 20,
      right: 420,
      bottom: 180,
    };
    let scene = crate::overlay::gdi::bitmap::ScreenshotBitmap::from_rgba(
      &[110, 90, 160, 255].repeat(640 * 400),
      640,
      400,
    )
    .unwrap();
    let mut field = TextField::new(parent.0, rect, 24, "Café 日本語\r\nsecond line").unwrap();
    for appearance in [
      TextAppearance::Plain,
      TextAppearance::Outline,
      TextAppearance::Background,
    ] {
      for color in [[250, 250, 250], [20, 30, 40]] {
        field.style(24, color, appearance).unwrap();
        // SAFETY: alter selection only in the fixture's own hidden EDIT control.
        unsafe {
          SendMessageW(field.hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(0)));
        }
        field.backdrop(scene.dc, rect, 0, appearance).unwrap();
        let brush = field.brush;
        field.backdrop(scene.dc, rect, 0, appearance).unwrap();
        assert_eq!(
          field.brush, brush,
          "unchanged backing must reuse its pattern brush"
        );
        assert!(background(scene.dc, field.hwnd).is_some());
        // SAFETY: inspect only the field's properties and this fixture's drawing DC.
        unsafe {
          assert!(GetPropW(field.hwnd, MASK_PAINT).is_invalid());
          assert_eq!(
            GetTextColor(scene.dc).0,
            u32::from(color[0]) | (u32::from(color[1]) << 8) | (u32::from(color[2]) << 16)
          );
          SendMessageW(field.hwnd, EM_SETSEL, Some(WPARAM(2)), Some(LPARAM(4)));
        }
        field.backdrop(scene.dc, rect, 0, appearance).unwrap();
        assert_eq!(field.content().unwrap(), "Café 日本語\r\nsecond line");
      }
    }
    field.suspend_for_move();
    field.suspend_for_move();
    let moved = RECT {
      left: 30,
      top: 40,
      ..rect
    };
    field.synchronize_after_move(moved).unwrap();
    field
      .backdrop(scene.dc, moved, 0, TextAppearance::Plain)
      .unwrap();
    assert!(field.content_height().unwrap() > 24);
    // SAFETY: a child may be prepared, but its parent must stay hidden throughout.
    assert!(!unsafe { IsWindowVisible(parent.0) }.as_bool());
    assert!(background(scene.dc, HWND::default()).is_none());
    assert!(
      field
        .backdrop(
          scene.dc,
          RECT {
            left: 0,
            top: 0,
            right: 9000,
            bottom: 9000
          },
          0,
          TextAppearance::Background
        )
        .is_err()
    );
  }
  struct Parent(HWND);
  impl Drop for Parent {
    fn drop(&mut self) {
      // SAFETY: hidden test parent is owned by this test thread.
      if let Err(error) = unsafe { DestroyWindow(self.0) } {
        tracing::warn!(%error,"Test parent cleanup failed");
      }
    }
  }
  #[test]
  #[ignore = "native keyboard/focus depends on the interactive input queue; test:capture:desktop"]
  fn native_field_reads_unicode_and_posts_cancel_without_orphaning_control() {
    // SAFETY: built-in class, hidden thread-owned parent; no visible window or input injection.
    let parent = Parent(
      unsafe {
        CreateWindowExW(
          WINDOW_EX_STYLE::default(),
          w!("STATIC"),
          w!(""),
          WS_POPUP,
          0,
          0,
          400,
          300,
          None,
          None,
          None,
          None,
        )
      }
      .expect("hidden parent"),
    );
    let mut field = TextField::new(
      parent.0,
      RECT {
        left: 10,
        top: 10,
        right: 310,
        bottom: 170,
      },
      24,
      "Café 日本語 🙂\r\nSecond line",
    )
    .expect("native field");
    // SAFETY: inspect the test-owned child's style even though its parent remains hidden.
    assert_eq!(
      unsafe { GetWindowLongW(field.hwnd, GWL_STYLE) } as u32 & WS_VISIBLE.0,
      0,
      "creation must not expose an unprepared native field"
    );
    let scene =
      crate::overlay::gdi::bitmap::ScreenshotBitmap::from_rgba(&vec![80; 400 * 300 * 4], 400, 300)
        .expect("frozen scene");
    field
      .backdrop(
        scene.dc,
        RECT {
          left: 10,
          top: 10,
          right: 310,
          bottom: 170,
        },
        0,
        crate::screenshot::annotations::TextAppearance::Plain,
      )
      .expect("prepare field");
    // SAFETY: same thread-owned child; visibility is enabled only after its backing exists.
    assert_ne!(
      unsafe { GetWindowLongW(field.hwnd, GWL_STYLE) } as u32 & WS_VISIBLE.0,
      0
    );
    assert_eq!(
      field.content().expect("Unicode read"),
      "Café 日本語 🙂\r\nSecond line"
    );
    let hwnd = field.hwnd;
    // SAFETY: sends Escape to this test's own control; the callback posts to its hidden parent.
    unsafe { SendMessageW(hwnd, WM_KEYDOWN, Some(WPARAM(0x1b)), None) };
    let mut message = MSG::default();
    // SAFETY: read only the bounded custom message range for this test parent.
    assert!(
      unsafe { PeekMessageW(&mut message, Some(parent.0), COMPLETE, COMPLETE, PM_REMOVE) }
        .as_bool()
    );
    assert_eq!(message.wParam.0, 0);
    assert_eq!(message.lParam.0, hwnd.0 as isize);
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardState, SetKeyboardState};
    struct Keys([u8; 256]);
    impl Drop for Keys {
      fn drop(&mut self) {
        // SAFETY: restores only this test thread's input-queue keyboard state.
        if let Err(error) = unsafe { SetKeyboardState(&self.0) } {
          tracing::warn!(%error,"Failed to restore test keyboard state");
        }
      }
    }
    let mut original = [0u8; 256];
    // SAFETY: reads thread-local input state into a correctly sized buffer.
    unsafe { GetKeyboardState(&mut original) }.expect("keyboard snapshot");
    let _keys = Keys(original);
    // Do not inherit physical modifiers held by the user during a quiet test.
    let mut ctrl = [0u8; 256];
    ctrl[0x11] = 0x80;
    // SAFETY: alters only this test thread's queue state; no SendInput or physical keyboard event.
    unsafe { SetKeyboardState(&ctrl) }.expect("test control modifier");
    let before = field.content().expect("before commit");
    // SAFETY: synthetic messages target only the test-owned hidden child.
    unsafe {
      SendMessageW(hwnd, WM_KEYDOWN, Some(WPARAM(0x0d)), None);
      SendMessageW(hwnd, WM_CHAR, Some(WPARAM(0x0a)), None);
    }
    // SAFETY: completion is bounded to the test parent's one custom message type.
    assert!(
      !unsafe { PeekMessageW(&mut message, Some(parent.0), COMPLETE, COMPLETE, PM_REMOVE) }
        .as_bool()
    );
    assert_eq!(
      field.content().expect("after commit key"),
      before,
      "Ctrl+Enter must not insert a newline"
    );
    let mut shift = [0u8; 256];
    shift[0x10] = 0x80;
    shift[0x11] = 0;
    // SAFETY: synthetic modifier state and messages target only this hidden test child.
    unsafe {
      SetKeyboardState(&shift).expect("shift modifier");
      SendMessageW(hwnd, EM_SETSEL, Some(WPARAM(usize::MAX)), Some(LPARAM(-1)));
      SendMessageW(hwnd, WM_KEYDOWN, Some(WPARAM(0x0d)), None);
      SendMessageW(hwnd, WM_CHAR, Some(WPARAM(0x0d)), None);
    }
    assert_eq!(field.content().expect("newline"), format!("{before}\r\n"));
    // SAFETY: restore modifiers before testing unmodified Enter on the same child.
    unsafe {
      SetKeyboardState(&[0u8; 256]).expect("plain modifier");
      SendMessageW(hwnd, WM_KEYDOWN, Some(WPARAM(0x0d)), None);
      SendMessageW(hwnd, WM_CHAR, Some(WPARAM(0x0d)), None);
    }
    // SAFETY: only consume the test parent's bounded completion message.
    assert!(
      unsafe { PeekMessageW(&mut message, Some(parent.0), COMPLETE, COMPLETE, PM_REMOVE) }
        .as_bool()
    );
    assert_eq!(message.wParam.0, 1);
    assert_eq!(
      field.content().expect("committed content"),
      format!("{before}\r\n")
    );
    drop(field);
    // SAFETY: querying a disposed HWND has no mutation and should report false.
    assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
  }

  #[test]
  fn sparse_decoration_matches_dense_reference_at_edges_and_partial_coverage() {
    let (width, height) = (31usize, 23usize);
    let mut mask = vec![0; width * height * 4];
    for (index, pixel) in mask.as_chunks_mut::<4>().0.iter_mut().enumerate() {
      if index % 11 == 0 || index % 37 == 0 {
        pixel[0] = (index % 255 + 1) as u8;
      }
    }
    let initial = vec![97; width * height * 4];
    let mut expected = initial.clone();
    for y in 0..height {
      for x in 0..width {
        let mut alpha = 0u32;
        for (dx, dy) in [
          (-2, -2),
          (0, -2),
          (2, -2),
          (-2, 0),
          (2, 0),
          (-2, 2),
          (0, 2),
          (2, 2),
        ] {
          let (nx, ny) = (x as i32 + dx, y as i32 + dy);
          if nx >= 0 && ny >= 0 && nx < width as i32 && ny < height as i32 {
            alpha = alpha.max(mask[(ny as usize * width + nx as usize) * 4] as u32);
          }
        }
        for (channel, color) in [20u32, 30, 40].into_iter().enumerate() {
          expected[(y * width + x) * 4 + channel] =
            ((97 * (255 - alpha) + color * alpha + 127) / 255) as u8;
        }
      }
    }
    let mut actual = initial;
    apply_text_backing(
      &mut actual,
      &mask,
      width,
      height,
      crate::screenshot::annotations::TextAppearance::Outline,
      [20, 30, 40],
    );
    assert_eq!(actual, expected);
  }
  unsafe extern "system" fn probe_parent(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _: usize,
  ) -> LRESULT {
    if msg == WM_CTLCOLOREDIT
      && let Some(result) = background(HDC(wparam.0 as *mut _), HWND(lparam.0 as *mut _))
    {
      return result;
    }
    // SAFETY: subclass owns only this hidden parent, and chains native messages normally.
    unsafe {
      if msg == WM_NCDESTROY {
        assert!(RemoveWindowSubclass(hwnd, Some(probe_parent), id).as_bool());
      }
      DefSubclassProc(hwnd, msg, wparam, lparam)
    }
  }
  #[test]
  #[ignore = "explicit native text interaction cost probe"]
  fn text_interaction_cost_probe() {
    // SAFETY: hidden parent and children are created/destroyed on this test thread.
    let parent = Parent(
      unsafe {
        CreateWindowExW(
          WINDOW_EX_STYLE::default(),
          w!("STATIC"),
          w!(""),
          WS_POPUP,
          0,
          0,
          2300,
          800,
          None,
          None,
          None,
          None,
        )
      }
      .expect("hidden parent"),
    );
    // SAFETY: reproduce production native color/mask callbacks on the hidden test parent.
    assert!(unsafe { SetWindowSubclass(parent.0, Some(probe_parent), 42, 0) }.as_bool());
    let source = crate::overlay::gdi::bitmap::ScreenshotBitmap::from_rgba(
      &vec![80; 2300 * 800 * 4],
      2300,
      800,
    )
    .expect("controlled DIB");
    let mut results = Vec::new();
    for reference in [true, false] {
      REFERENCE_BACKING.with(|value| value.set(reference));
      for (width, height) in [(800, 160), (2000, 500)] {
        for appearance in [
          crate::screenshot::annotations::TextAppearance::Plain,
          crate::screenshot::annotations::TextAppearance::Outline,
          crate::screenshot::annotations::TextAppearance::Background,
        ] {
          let rect = RECT {
            left: 20,
            top: 20,
            right: 20 + width,
            bottom: 20 + height,
          };
          let mut field = TextField::new(parent.0, rect, 24, "Flinch text movement\r\nUnicode éà 日本語 and a third line\r\nA short annotation on a large text field.").expect("native field");
          field.style(24, [240, 240, 245], appearance).expect("style");
          // SAFETY: clear select-all so decoration costs are included.
          unsafe {
            SendMessageW(field.hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(0)));
          }
          let mut samples = Vec::new();
          for step in 0..12 {
            let shifted = RECT {
              left: rect.left + step,
              right: rect.right + step,
              ..rect
            };
            let started = std::time::Instant::now();
            field.layout(shifted).expect("move field");
            field
              .backdrop(source.dc, shifted, 0, appearance)
              .expect("backdrop");
            samples.push(started.elapsed().as_micros() as u64);
          }
          let cold_us = samples.remove(0);
          samples.sort_unstable();
          results.push(serde_json::json!({"reference":reference,"width":width,"height":height,"appearance":format!("{appearance:?}"),"cold_us":cold_us,"warm_p50_us":samples[5],"warm_max_us":samples[10]}));
        }
      }
    }
    REFERENCE_BACKING.with(|value| value.set(false));
    let mut measure = Vec::new();
    for index in 0..12 {
      let started = std::time::Instant::now();
      crate::screenshot::annotations::text_data(
        format!("Text typing measurement {index} éà 日本語"),
        24,
        800,
      )
      .expect("layout");
      measure.push(started.elapsed().as_micros() as u64);
    }
    measure.sort_unstable();
    println!(
      "{}",
      serde_json::json!({"profile":if cfg!(debug_assertions) {"debug"} else {"release"},"native_field_move_and_backdrop":results,"text_measure_p50_us":measure[6],"text_measure_p95_us":measure[11]})
    );
  }
}
