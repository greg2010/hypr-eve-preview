use std::ffi::OsString;
use std::fmt;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use ab_glyph::{Font as _, PxScale, ScaleFont as _, point};

use crate::config::{self, Color, MAX_OPACITY};
use crate::geometry::{BYTES_PER_PIXEL, Size};

/// A parsed label font, owning the file's bytes.
pub struct Font(ab_glyph::FontVec);

#[derive(Debug)]
pub enum FontError {
    Spawn(std::io::Error),
    Status(std::process::ExitStatus),
    Empty,
    Read(std::io::Error),
    Invalid,
}

impl fmt::Display for FontError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FontError::Spawn(e) => write!(f, "spawn: {e}"),
            FontError::Status(status) => write!(f, "{status}"),
            FontError::Empty => write!(f, "no font file printed"),
            FontError::Read(e) => write!(f, "read: {e}"),
            FontError::Invalid => write!(f, "not a font file"),
        }
    }
}

impl std::error::Error for FontError {}

/// Runs `fc-match -f '%{file}' <family>` and returns the file it prints. Fontconfig answers
/// an unknown family with its default match, so a success does not mean the family exists.
pub fn resolve_font(family: &str) -> Result<PathBuf, FontError> {
    let output = Command::new("fc-match")
        .arg("-f")
        .arg("%{file}")
        .arg(family)
        .output()
        .map_err(FontError::Spawn)?;
    if !output.status.success() {
        return Err(FontError::Status(output.status));
    }
    if output.stdout.is_empty() {
        return Err(FontError::Empty);
    }
    Ok(PathBuf::from(OsString::from_vec(output.stdout)))
}

impl Font {
    /// Reads the whole file and parses it. Never prints the file's contents.
    pub fn load(path: &Path) -> Result<Font, FontError> {
        let bytes = std::fs::read(path).map_err(FontError::Read)?;
        ab_glyph::FontVec::try_from_vec(bytes)
            .map(Font)
            .map_err(|_| FontError::Invalid)
    }
}

/// Premultiplied pixel bytes in memory order: B, G, R, A.
pub fn premultiply(color: Color) -> [u8; 4] {
    let scale = |channel: u8| ((u16::from(channel) * u16::from(color.a) + 127) / 255) as u8;
    [scale(color.b), scale(color.g), scale(color.r), color.a]
}

/// Chrome buffer size in pixels for a thumbnail of `logical` px at output `scale`.
pub fn buffer_size(logical: Size, scale: f64) -> Size {
    let axis = |v: u32| (f64::from(v) * scale).round() as u32;
    Size {
        width: axis(logical.width),
        height: axis(logical.height),
    }
}

/// What `render` draws: an optional ring of the given width and colour, and a label. `opacity`
/// is a percent, 0 to `MAX_OPACITY`, scaling the ring and label alpha before premultiplication.
pub struct Chrome<'a> {
    pub ring: Option<(u32, Color)>,
    pub label: &'a str,
    pub style: &'a config::Label,
    pub scale: f64,
    pub opacity: u32,
}

/// Draws the ring and the label into `canvas`, 4 bytes per pixel, `size.width` pixels per row.
/// Every byte of the canvas is overwritten. Both colours' alpha is scaled by `chrome.opacity`
/// percent, at most `MAX_OPACITY`.
pub fn render(canvas: &mut [u8], size: Size, chrome: &Chrome<'_>, font: &Font) {
    canvas.fill(0);
    if let Some((width, color)) = chrome.ring {
        draw_ring(
            canvas,
            size,
            width,
            with_opacity(color, chrome.opacity),
            chrome.scale,
        );
    }
    draw_label(canvas, size, chrome, font);
}

fn with_opacity(color: Color, percent: u32) -> Color {
    let a = (u32::from(color.a) * percent.min(MAX_OPACITY) + MAX_OPACITY / 2) / MAX_OPACITY;
    Color {
        a: a as u8,
        ..color
    }
}

fn draw_ring(canvas: &mut [u8], size: Size, width: u32, color: Color, scale: f64) {
    let r = (scale * f64::from(width) + 0.5).floor() as u32;
    if r == 0 {
        return;
    }
    let bytes = premultiply(color);
    for y in 0..size.height {
        for x in 0..size.width {
            let on_ring = x < r
                || x >= size.width.saturating_sub(r)
                || y < r
                || y >= size.height.saturating_sub(r);
            if on_ring && let Some(pixel) = pixel_mut(canvas, size, i64::from(x), i64::from(y)) {
                pixel.copy_from_slice(&bytes);
            }
        }
    }
}

fn draw_label(canvas: &mut [u8], size: Size, chrome: &Chrome<'_>, font: &Font) {
    let style = chrome.style;
    let scaled = font
        .0
        .as_scaled(PxScale::from((chrome.scale * f64::from(style.size)) as f32));
    let source = premultiply(with_opacity(style.color, chrome.opacity));
    let mut pen_x = (chrome.scale * f64::from(style.x)) as f32;
    let baseline = (chrome.scale * f64::from(style.y)) as f32 + scaled.ascent();
    let mut previous = None;
    for c in chrome.label.chars() {
        let id = scaled.glyph_id(c);
        if let Some(previous) = previous {
            pen_x += scaled.kern(previous, id);
        }
        let glyph = id.with_scale_and_position(scaled.scale(), point(pen_x, baseline));
        if let Some(outlined) = scaled.outline_glyph(glyph) {
            let origin = outlined.px_bounds().min;
            let (left, top) = (origin.x.floor() as i64, origin.y.floor() as i64);
            outlined.draw(|gx, gy, coverage| {
                let x = left + i64::from(gx);
                let y = top + i64::from(gy);
                if let Some(pixel) = pixel_mut(canvas, size, x, y) {
                    blend(pixel, source, coverage.min(1.0));
                }
            });
        }
        pen_x += scaled.h_advance(id);
        previous = Some(id);
    }
}

fn pixel_mut(canvas: &mut [u8], size: Size, x: i64, y: i64) -> Option<&mut [u8]> {
    let (w, h) = (i64::from(size.width), i64::from(size.height));
    if x < 0 || y < 0 || x >= w || y >= h {
        return None;
    }
    let start = usize::try_from((y * w + x) * BYTES_PER_PIXEL as i64).ok()?;
    canvas.get_mut(start..start + BYTES_PER_PIXEL)
}

fn blend(pixel: &mut [u8], source: [u8; 4], coverage: f32) {
    let inverse = 1.0 - f32::from(source[3]) / 255.0 * coverage;
    for (dst, src) in pixel.iter_mut().zip(source) {
        let out = f32::from(src) * coverage + f32::from(*dst) * inverse;
        *dst = out.round().clamp(0.0, 255.0) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FONT_FILE: &str = "/usr/share/fonts/noto/NotoSansMono-Regular.ttf";
    const GREEN: Color = Color {
        r: 0x40,
        g: 0xFF,
        b: 0x00,
        a: 0xFF,
    };

    fn style() -> config::Label {
        config::Label {
            font: "Noto Sans Mono".to_string(),
            font_file: None,
            size: 15,
            color: GREEN,
            x: 6,
            y: 6,
        }
    }

    fn noto() -> Font {
        Font::load(Path::new(FONT_FILE)).unwrap()
    }

    fn pixel(canvas: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * width + x) as usize) * BYTES_PER_PIXEL;
        [canvas[i], canvas[i + 1], canvas[i + 2], canvas[i + 3]]
    }

    #[test]
    fn buffer_size_cases() {
        let cases = [
            ("default thumbnail", (480, 264), 1.5, (720, 396)),
            ("minimum thumbnail", (160, 88), 1.5, (240, 132)),
            ("scale one", (480, 264), 1.0, (480, 264)),
            ("half rounds away from zero", (3, 3), 1.5, (5, 5)),
        ];
        for (name, (w, h), scale, (bw, bh)) in cases {
            let got = buffer_size(
                Size {
                    width: w,
                    height: h,
                },
                scale,
            );
            assert_eq!(
                got,
                Size {
                    width: bw,
                    height: bh
                },
                "{name}"
            );
        }
    }

    #[test]
    fn premultiply_cases() {
        let cases = [
            ("opaque", (0x40, 0xFF, 0x00, 0xFF), [0x00, 0xFF, 0x40, 0xFF]),
            (
                "half alpha",
                (0x40, 0xFF, 0x00, 0x80),
                [0x00, 0x80, 0x20, 0x80],
            ),
            ("transparent", (0, 0, 0, 0), [0, 0, 0, 0]),
            (
                "white half",
                (0xFF, 0xFF, 0xFF, 0x80),
                [0x80, 0x80, 0x80, 0x80],
            ),
        ];
        for (name, (r, g, b, a), want) in cases {
            assert_eq!(premultiply(Color { r, g, b, a }), want, "{name}");
        }
    }

    #[test]
    fn ring_cases() {
        let size = Size {
            width: 720,
            height: 396,
        };
        let style = style();
        let green = [0x00, 0xFF, 0x40, 0xFF];
        let clear = [0; 4];
        let cases = [
            (
                "ring width 2 at 1.5",
                Some((2, GREEN)),
                100,
                false,
                vec![
                    ((0, 0), green),
                    ((2, 200), green),
                    ((717, 200), green),
                    ((719, 395), green),
                    ((360, 393), green),
                    ((3, 200), clear),
                    ((716, 200), clear),
                    ((360, 392), clear),
                    ((360, 198), clear),
                ],
            ),
            ("no ring", None, 100, true, vec![]),
            ("ring width 0", Some((0, GREEN)), 100, true, vec![]),
            (
                "ring at opacity 50",
                Some((2, GREEN)),
                50,
                false,
                vec![((2, 200), [0x00, 0x80, 0x20, 0x80])],
            ),
            (
                "ring at opacity 150",
                Some((2, GREEN)),
                150,
                false,
                vec![((2, 200), green)],
            ),
            (
                "ring at opacity 0",
                Some((2, GREEN)),
                0,
                true,
                vec![((2, 200), clear)],
            ),
        ];
        let font = noto();
        for (name, ring, opacity, all_zero, pixels) in cases {
            let chrome = Chrome {
                ring,
                label: "",
                style: &style,
                scale: 1.5,
                opacity,
            };
            let mut canvas = vec![0xAA; 720 * 396 * BYTES_PER_PIXEL];
            render(&mut canvas, size, &chrome, &font);
            if all_zero {
                assert_eq!(canvas, vec![0u8; 720 * 396 * BYTES_PER_PIXEL], "{name}");
            }
            for ((x, y), want) in pixels {
                assert_eq!(pixel(&canvas, 720, x, y), want, "{name} ({x}, {y})");
            }
        }
    }

    #[test]
    fn label_cases() {
        let size = Size {
            width: 720,
            height: 396,
        };
        let style = style();
        let font = noto();
        let cases = [
            ("label", "Pilot One", 100, Some(0xFF)),
            ("label W at opacity 50", "W", 50, Some(0x80)),
            ("label at opacity 150", "Pilot One", 150, Some(0xFF)),
            ("outline-less glyphs", "   ", 100, None),
            ("empty label", "", 100, None),
        ];
        for (name, label, opacity, alpha_max) in cases {
            let chrome = Chrome {
                ring: None,
                label,
                style: &style,
                scale: 1.5,
                opacity,
            };
            let mut canvas = vec![0xAA; 720 * 396 * BYTES_PER_PIXEL];
            render(&mut canvas, size, &chrome, &font);
            let mut max_alpha = 0;
            let mut bounds: Option<(u32, u32, u32, u32)> = None;
            let mut blue = Vec::new();
            for y in 0..396 {
                for x in 0..720 {
                    let p = pixel(&canvas, 720, x, y);
                    if p[3] == 0 {
                        continue;
                    }
                    blue.push(p[0]);
                    max_alpha = max_alpha.max(p[3]);
                    bounds = Some(match bounds {
                        None => (x, y, x, y),
                        Some((l, t, r, b)) => (l.min(x), t.min(y), r.max(x), b.max(y)),
                    });
                }
            }
            if let Some(alpha_max) = alpha_max {
                let (l, t, r, b) = bounds.unwrap();
                assert!(
                    l >= 9 && t >= 9 && r < 720 && b < 54,
                    "{name}: {l},{t},{r},{b}"
                );
                assert!(blue.iter().all(|&v| v == 0), "{name}: blue channel");
                assert_eq!(max_alpha, alpha_max, "{name}: max alpha");
            } else {
                assert_eq!(canvas, vec![0u8; 720 * 396 * BYTES_PER_PIXEL], "{name}");
            }
        }
    }

    #[test]
    fn font_cases() {
        let cases = [
            ("a font", FONT_FILE, "ok"),
            (
                "not a font",
                concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
                "invalid",
            ),
            ("missing", "/nonexistent/hypr-eve-preview-font.ttf", "read"),
        ];
        for (name, path, want) in cases {
            let got = match Font::load(Path::new(path)) {
                Ok(_) => "ok",
                Err(FontError::Invalid) => "invalid",
                Err(FontError::Read(_)) => "read",
                Err(_) => "other",
            };
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn error_display_cases() {
        let cases = [
            ("empty", FontError::Empty, "no font file printed"),
            ("invalid", FontError::Invalid, "not a font file"),
            (
                "read",
                FontError::Read(std::io::Error::from(std::io::ErrorKind::NotFound)),
                "read: entity not found",
            ),
        ];
        for (name, error, want) in cases {
            assert_eq!(error.to_string(), want, "{name}");
        }
    }

    #[test]
    fn resolve_font_cases() {
        let cases = [("noto sans mono", "Noto Sans Mono", FONT_FILE)];
        for (name, family, want) in cases {
            assert_eq!(resolve_font(family).unwrap(), PathBuf::from(want), "{name}");
        }
    }
}
