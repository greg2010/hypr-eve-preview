/// Bytes per pixel of an ARGB8888 buffer.
pub const BYTES_PER_PIXEL: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: u32,
    pub y: u32,
}

/// A margin pair that may be negative, which places a layer surface partly or wholly outside
/// its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offset {
    pub x: i32,
    pub y: i32,
}

impl From<Point> for Offset {
    fn from(point: Point) -> Self {
        Offset {
            x: i32::try_from(point.x).unwrap_or(i32::MAX),
            y: i32::try_from(point.y).unwrap_or(i32::MAX),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn size(self) -> Size {
        Size {
            width: self.width,
            height: self.height,
        }
    }
}

/// Thumbnail of the given width for a buffer, with the exact height rounded to the nearest
/// even number, ties up. Needs both widths at least 1.
pub fn thumbnail_size(width: u32, buffer: Size) -> Size {
    let w = u64::from(width);
    let bw = u64::from(buffer.width);
    let bh = u64::from(buffer.height);
    let half = ((w * bh + bw) / (2 * bw)).max(1);
    Size {
        width,
        height: u32::try_from(2 * half).unwrap_or(u32::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_size_cases() {
        let cases = [
            ("offset ignored", (10, 34, 2560, 1406), (2560, 1406)),
            ("empty", (60, 60, 0, 0), (0, 0)),
        ];
        for (name, (x, y, width, height), (w, h)) in cases {
            let rect = Rect {
                x,
                y,
                width,
                height,
            };
            assert_eq!(
                rect.size(),
                Size {
                    width: w,
                    height: h
                },
                "{name}"
            );
        }
    }

    #[test]
    fn offset_from_point_cases() {
        let cases = [
            ("origin", (0, 0), (0, 0)),
            ("inside", (12, 34), (12, 34)),
            ("beyond i32", (u32::MAX, 5), (i32::MAX, 5)),
        ];
        for (name, (x, y), (want_x, want_y)) in cases {
            let want = Offset {
                x: want_x,
                y: want_y,
            };
            assert_eq!(Offset::from(Point { x, y }), want, "{name}");
        }
    }

    #[test]
    fn thumbnail_size_cases() {
        let cases = [
            ("window size", 480, (2560, 1406), (480, 264)),
            ("buffer size", 480, (3840, 2109), (480, 264)),
            ("sixteen to nine", 480, (3840, 2160), (480, 270)),
            ("width 500", 500, (3840, 2109), (500, 274)),
            ("width 544", 544, (3840, 2109), (544, 298)),
            ("tie rounds up", 2, (2, 1), (2, 2)),
            ("minimum height", 480, (3840, 1), (480, 2)),
        ];
        for (name, w, (bw, bh), (tw, th)) in cases {
            let got = thumbnail_size(
                w,
                Size {
                    width: bw,
                    height: bh,
                },
            );
            assert_eq!(
                got,
                Size {
                    width: tw,
                    height: th
                },
                "{name}"
            );
        }
    }
}
