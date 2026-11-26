//! Reviewed Fluent UI System Icons for native overlay chrome.
//!
//! The SVGs in `resources/icons/fluent/` are embedded unmodified; `check:contracts` keeps
//! them matched to `references.json`. Each holds one path in a square view box, parsed
//! once per renderer into a filled Direct2D geometry with SVG's nonzero fill rule.

use anyhow::{Context, Result, bail, ensure};
use windows::Win32::Graphics::Direct2D::Common::{
  D2D_SIZE_F, D2D1_BEZIER_SEGMENT, D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_END_CLOSED,
  D2D1_FIGURE_END_OPEN, D2D1_FILL_MODE_WINDING,
};
use windows::Win32::Graphics::Direct2D::{
  D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_LARGE, D2D1_ARC_SIZE_SMALL, D2D1_QUADRATIC_BEZIER_SEGMENT,
  D2D1_SWEEP_DIRECTION_CLOCKWISE, D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE, ID2D1Factory,
  ID2D1PathGeometry,
};
use windows_numerics::Vector2;

macro_rules! fluent_icons {
  ($($name:ident => $file:literal,)*) => {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Icon {
      $($name,)*
    }

    impl Icon {
      pub const ALL: &'static [Icon] = &[$(Icon::$name,)*];

      /// The vendored file name, as listed in `references.json`.
      pub fn file(self) -> &'static str {
        match self {
          $(Icon::$name => $file,)*
        }
      }

      fn svg(self) -> &'static str {
        match self {
          $(Icon::$name => include_str!(concat!("../../resources/icons/fluent/", $file)),)*
        }
      }
    }
  };
}

fluent_icons! {
  Grip => "re_order_dots_vertical_20_regular.svg",
  AdjustArea => "crop_20_regular.svg",
  AdjustAreaSelected => "crop_20_filled.svg",
  SelectDrawing => "cursor_20_regular.svg",
  SelectDrawingSelected => "cursor_20_filled.svg",
  Pen => "pen_20_regular.svg",
  PenSelected => "pen_20_filled.svg",
  Highlighter => "highlight_20_regular.svg",
  HighlighterSelected => "highlight_20_filled.svg",
  Arrow => "arrow_up_right_20_regular.svg",
  ArrowSelected => "arrow_up_right_20_filled.svg",
  Rectangle => "square_20_regular.svg",
  RectangleSelected => "square_20_filled.svg",
  Ellipse => "circle_20_regular.svg",
  EllipseSelected => "circle_20_filled.svg",
  Blur => "blur_20_regular.svg",
  BlurSelected => "blur_20_filled.svg",
  Text => "text_t_20_regular.svg",
  TextSelected => "text_t_20_filled.svg",
  RedactText => "scan_text_20_regular.svg",
  StepMarker => "number_circle_1_20_regular.svg",
  StepMarkerSelected => "number_circle_1_20_filled.svg",
  Undo => "arrow_hook_up_left_20_regular.svg",
  Redo => "arrow_hook_up_right_20_regular.svg",
  ChevronUp => "chevron_up_12_regular.svg",
  ChevronDown => "chevron_down_12_regular.svg",
  Pause => "pause_20_filled.svg",
  Resume => "play_20_filled.svg",
  Stop => "stop_20_filled.svg",
  Microphone => "mic_20_filled.svg",
  MicrophoneMuted => "mic_off_20_filled.svg",
}

type Point = (f32, f32);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
  Begin(Point),
  Line(Point),
  Cubic(Point, Point, Point),
  Quadratic(Point, Point),
  Arc {
    end: Point,
    radii: Point,
    rotation: f32,
    large: bool,
    sweep: bool,
  },
  End {
    closed: bool,
  },
}

/// Upper bound on path operations; the largest vendored icon uses well under a hundred.
const MAX_OPS: usize = 4096;

/// Parses the vendored SVG shape: one `<path d="…">` inside a square `0 0 n n` view box.
fn parse_svg(svg: &str) -> Result<(f32, Vec<Op>)> {
  let attribute = |name: &str| -> Result<&str> {
    let marker = format!(" {name}=\"");
    let start = svg.find(&marker).context(format!("SVG has no {name}"))? + marker.len();
    let end = svg[start..]
      .find('"')
      .context("Unterminated SVG attribute")?;
    Ok(&svg[start..start + end])
  };
  ensure!(
    svg.starts_with("<svg ") && svg.ends_with("</svg>") && svg.matches("<path").count() == 1,
    "Icon must be one path inside an <svg> element"
  );
  ensure!(
    !svg.contains("transform") && !svg.contains("fill-rule"),
    "Icon uses unsupported SVG features"
  );
  let view_box: Vec<f32> = attribute("viewBox")?
    .split_whitespace()
    .map(str::parse)
    .collect::<std::result::Result<_, _>>()
    .context("Invalid SVG view box")?;
  ensure!(
    matches!(view_box.as_slice(), [0.0, 0.0, w, h] if w == h && *w > 0.0 && *w <= 64.0),
    "Icon view box must be square at the origin"
  );
  Ok((view_box[2], parse_path(attribute("d")?)?))
}

struct Lexer<'a> {
  bytes: &'a [u8],
  at: usize,
}

impl Lexer<'_> {
  fn skip_separators(&mut self) {
    while self
      .bytes
      .get(self.at)
      .is_some_and(|b| b.is_ascii_whitespace() || *b == b',')
    {
      self.at += 1;
    }
  }

  fn command(&mut self) -> Option<u8> {
    self.skip_separators();
    let byte = *self.bytes.get(self.at)?;
    byte.is_ascii_alphabetic().then(|| {
      self.at += 1;
      byte
    })
  }

  fn at_number(&mut self) -> bool {
    self.skip_separators();
    self
      .bytes
      .get(self.at)
      .is_some_and(|b| b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.'))
  }

  /// Reads one number; SVG allows `1.5.5` and `1-2` without separators.
  fn number(&mut self) -> Result<f32> {
    self.skip_separators();
    let start = self.at;
    let mut dot = false;
    let mut exponent = false;
    while let Some(&byte) = self.bytes.get(self.at) {
      let sign_allowed =
        self.at == start || (exponent && matches!(self.bytes[self.at - 1], b'e' | b'E'));
      match byte {
        b'+' | b'-' if sign_allowed => {},
        b'0'..=b'9' => {},
        b'.' if !dot && !exponent => dot = true,
        b'e' | b'E' if !exponent && self.at > start => exponent = true,
        _ => break,
      }
      self.at += 1;
    }
    let text = std::str::from_utf8(&self.bytes[start..self.at])?;
    let value: f32 = text
      .parse()
      .with_context(|| format!("Invalid path number at byte {start}"))?;
    ensure!(value.is_finite(), "Non-finite path number");
    Ok(value)
  }

  /// Arc flags are single `0`/`1` characters and may run into the next number.
  fn flag(&mut self) -> Result<bool> {
    self.skip_separators();
    let byte = self.bytes.get(self.at).copied();
    self.at += 1;
    match byte {
      Some(b'0') => Ok(false),
      Some(b'1') => Ok(true),
      _ => bail!("Invalid arc flag"),
    }
  }

  fn point(&mut self) -> Result<Point> {
    Ok((self.number()?, self.number()?))
  }
}

fn parse_path(data: &str) -> Result<Vec<Op>> {
  let mut lexer = Lexer {
    bytes: data.as_bytes(),
    at: 0,
  };
  let mut ops = Vec::new();
  let mut current = (0.0, 0.0);
  let mut start = (0.0, 0.0);
  let mut open = false;
  // Last control point, for reflecting into S/s (cubic) and T/t (quadratic).
  let mut last_cubic: Option<Point> = None;
  let mut last_quadratic: Option<Point> = None;
  let mut command = lexer.command().context("Path must start with a command")?;
  loop {
    let relative = command.is_ascii_lowercase();
    let offset = |p: Point, current: Point| {
      if relative {
        (p.0 + current.0, p.1 + current.1)
      } else {
        p
      }
    };
    let upper = command.to_ascii_uppercase();
    if upper != b'M' && upper != b'Z' && !open {
      // Drawing after Z continues from the closed subpath's start, in a new figure.
      ops.push(Op::Begin(current));
      open = true;
    }
    let mut cubic = None;
    let mut quadratic = None;
    match upper {
      b'M' => {
        if open {
          ops.push(Op::End { closed: false });
        }
        current = offset(lexer.point()?, current);
        start = current;
        ops.push(Op::Begin(current));
        open = true;
        // Further pairs after a moveto are implicit linetos.
        command = if relative { b'l' } else { b'L' };
        if !lexer.at_number() {
          command = match lexer.command() {
            Some(next) => next,
            None => break,
          };
        }
        last_cubic = None;
        last_quadratic = None;
        ensure!(ops.len() <= MAX_OPS, "Icon path is too long");
        continue;
      },
      b'Z' => {
        if open {
          ops.push(Op::End { closed: true });
          open = false;
        }
        current = start;
      },
      b'L' => {
        current = offset(lexer.point()?, current);
        ops.push(Op::Line(current));
      },
      b'H' => {
        let x = lexer.number()?;
        current.0 = if relative { current.0 + x } else { x };
        ops.push(Op::Line(current));
      },
      b'V' => {
        let y = lexer.number()?;
        current.1 = if relative { current.1 + y } else { y };
        ops.push(Op::Line(current));
      },
      b'C' | b'S' => {
        let first = if upper == b'C' {
          offset(lexer.point()?, current)
        } else {
          last_cubic.map_or(current, |c| (2.0 * current.0 - c.0, 2.0 * current.1 - c.1))
        };
        let second = offset(lexer.point()?, current);
        let end = offset(lexer.point()?, current);
        ops.push(Op::Cubic(first, second, end));
        cubic = Some(second);
        current = end;
      },
      b'Q' | b'T' => {
        let control = if upper == b'Q' {
          offset(lexer.point()?, current)
        } else {
          last_quadratic.map_or(current, |c| (2.0 * current.0 - c.0, 2.0 * current.1 - c.1))
        };
        let end = offset(lexer.point()?, current);
        ops.push(Op::Quadratic(control, end));
        quadratic = Some(control);
        current = end;
      },
      b'A' => {
        let radii = (lexer.number()?.abs(), lexer.number()?.abs());
        let rotation = lexer.number()?;
        let large = lexer.flag()?;
        let sweep = lexer.flag()?;
        let end = offset(lexer.point()?, current);
        // SVG treats a zero radius as a straight line.
        ops.push(if radii.0 == 0.0 || radii.1 == 0.0 {
          Op::Line(end)
        } else {
          Op::Arc {
            end,
            radii,
            rotation,
            large,
            sweep,
          }
        });
        current = end;
      },
      other => bail!("Unsupported path command {}", char::from(other)),
    }
    last_cubic = cubic;
    last_quadratic = quadratic;
    ensure!(ops.len() <= MAX_OPS, "Icon path is too long");
    if upper == b'Z' || !lexer.at_number() {
      command = match lexer.command() {
        Some(next) => next,
        None => break,
      };
    }
  }
  lexer.skip_separators();
  ensure!(lexer.at == lexer.bytes.len(), "Trailing path data");
  if open {
    ops.push(Op::End { closed: false });
  }
  Ok(ops)
}

/// Builds the icon's filled geometry in its own view-box units, plus that box's size.
pub fn geometry(factory: &ID2D1Factory, icon: Icon) -> Result<(ID2D1PathGeometry, f32)> {
  let (size, ops) = parse_svg(icon.svg()).with_context(|| icon.file())?;
  let vector = |p: Point| Vector2 { X: p.0, Y: p.1 };
  // SAFETY: factory, geometry and sink are live on the owning thread; every figure ends
  // before Close, and point data is copied by each Add call.
  unsafe {
    let geometry = factory.CreatePathGeometry()?;
    let sink = geometry.Open()?;
    sink.SetFillMode(D2D1_FILL_MODE_WINDING);
    for op in ops {
      match op {
        Op::Begin(p) => sink.BeginFigure(vector(p), D2D1_FIGURE_BEGIN_FILLED),
        Op::Line(p) => sink.AddLine(vector(p)),
        Op::Cubic(a, b, c) => sink.AddBezier(&D2D1_BEZIER_SEGMENT {
          point1: vector(a),
          point2: vector(b),
          point3: vector(c),
        }),
        Op::Quadratic(a, b) => sink.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT {
          point1: vector(a),
          point2: vector(b),
        }),
        Op::Arc {
          end,
          radii,
          rotation,
          large,
          sweep,
        } => sink.AddArc(&D2D1_ARC_SEGMENT {
          point: vector(end),
          size: D2D_SIZE_F {
            width: radii.0,
            height: radii.1,
          },
          rotationAngle: rotation,
          // SVG's positive-angle sweep is clockwise in y-down coordinates.
          sweepDirection: if sweep {
            D2D1_SWEEP_DIRECTION_CLOCKWISE
          } else {
            D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE
          },
          arcSize: if large {
            D2D1_ARC_SIZE_LARGE
          } else {
            D2D1_ARC_SIZE_SMALL
          },
        }),
        Op::End { closed } => sink.EndFigure(if closed {
          D2D1_FIGURE_END_CLOSED
        } else {
          D2D1_FIGURE_END_OPEN
        }),
      }
    }
    sink.Close()?;
    Ok((geometry, size))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_relative_implicit_and_compact_path_syntax() {
    let ops =
      parse_path("M1 2l3 4h-1v2H0V1zm1.5.5 1-1a1 1 0 0 1 2 0q1 1 2 0t2 0c1 0 1 1 2 1s1 1 2 1Z")
        .expect("path");
    assert_eq!(
      ops,
      vec![
        Op::Begin((1.0, 2.0)),
        Op::Line((4.0, 6.0)),
        Op::Line((3.0, 6.0)),
        Op::Line((3.0, 8.0)),
        Op::Line((0.0, 8.0)),
        Op::Line((0.0, 1.0)),
        Op::End { closed: true },
        // `m` after Z is relative to the closed subpath's start (1, 2).
        Op::Begin((2.5, 2.5)),
        Op::Line((3.5, 1.5)),
        Op::Arc {
          end: (5.5, 1.5),
          radii: (1.0, 1.0),
          rotation: 0.0,
          large: false,
          sweep: true,
        },
        Op::Quadratic((6.5, 2.5), (7.5, 1.5)),
        // T reflects the previous quadratic control point.
        Op::Quadratic((8.5, 0.5), (9.5, 1.5)),
        Op::Cubic((10.5, 1.5), (10.5, 2.5), (11.5, 2.5)),
        // S reflects the previous cubic's second control point.
        Op::Cubic((12.5, 2.5), (12.5, 3.5), (13.5, 3.5)),
        Op::End { closed: true },
      ]
    );
  }

  #[test]
  fn arc_flags_may_touch_the_next_number_and_zero_radii_draw_lines() {
    let ops = parse_path("M0 0a.5.5 0 001 1A0 3 0 1 1 4 4").expect("path");
    assert_eq!(
      ops[1],
      Op::Arc {
        end: (1.0, 1.0),
        radii: (0.5, 0.5),
        rotation: 0.0,
        large: false,
        sweep: false,
      }
    );
    assert_eq!(ops[2], Op::Line((4.0, 4.0)));
    assert_eq!(ops[3], Op::End { closed: false });
  }

  #[test]
  fn malformed_paths_and_unsupported_svg_fail_explicitly() {
    for bad in [
      "",
      "1 2",
      "M0",
      "M0 0 X1 1",
      "M0 0a1 1 0 2 0 1 1",
      "M0 0L1e999 0",
      "M0 0 L1 1 #",
    ] {
      assert!(parse_path(bad).is_err(), "{bad:?}");
    }
    let path = r#"<path d="M0 0h1v1z"/>"#;
    for bad in [
      format!(r#"<svg viewBox="0 0 20 20">{path}{path}</svg>"#),
      format!(r#"<svg viewBox="0 0 20 16">{path}</svg>"#),
      format!(r#"<svg viewBox="1 0 20 20">{path}</svg>"#),
      format!(r#"<svg viewBox="0 0 20 20" transform="scale(2)">{path}</svg>"#),
      r#"<svg viewBox="0 0 20 20"></svg>"#.to_string(),
    ] {
      assert!(parse_svg(&bad).is_err(), "{bad}");
    }
    let huge = format!("M0 0{}", "h1".repeat(MAX_OPS + 1));
    assert!(parse_path(&huge).is_err());
  }

  #[test]
  fn every_vendored_icon_parses_into_closed_filled_figures_inside_its_box() {
    for icon in Icon::ALL {
      let (size, ops) =
        parse_svg(icon.svg()).unwrap_or_else(|error| panic!("{}: {error}", icon.file()));
      let expected = if icon.file().contains("_12_") {
        12.0
      } else {
        20.0
      };
      assert_eq!(size, expected, "{}", icon.file());
      assert!(matches!(ops.first(), Some(Op::Begin(_))), "{}", icon.file());
      let ends = ops.iter().filter(|op| matches!(op, Op::End { .. })).count();
      let begins = ops.iter().filter(|op| matches!(op, Op::Begin(_))).count();
      assert_eq!(begins, ends, "{} figures are balanced", icon.file());
      for op in &ops {
        let points: Vec<Point> = match *op {
          Op::Begin(p) | Op::Line(p) => vec![p],
          Op::Cubic(a, b, c) => vec![a, b, c],
          Op::Quadratic(a, b) => vec![a, b],
          Op::Arc { end, .. } => vec![end],
          Op::End { .. } => vec![],
        };
        for (x, y) in points {
          assert!(
            (-0.5..=size + 0.5).contains(&x) && (-0.5..=size + 0.5).contains(&y),
            "{} point ({x}, {y}) leaves its view box",
            icon.file()
          );
        }
      }
    }
  }

  #[test]
  fn every_vendored_file_is_listed_once_and_used() {
    let references: serde_json::Value =
      serde_json::from_str(include_str!("../../resources/icons/fluent/references.json"))
        .expect("references");
    let listed = references["sha1"].as_object().expect("fingerprints");
    let used: std::collections::BTreeSet<&str> = Icon::ALL.iter().map(|icon| icon.file()).collect();
    assert_eq!(used.len(), Icon::ALL.len(), "each icon has its own file");
    let listed: std::collections::BTreeSet<&str> = listed.keys().map(String::as_str).collect();
    assert_eq!(
      used, listed,
      "references.json lists exactly the embedded icons"
    );
  }
}
