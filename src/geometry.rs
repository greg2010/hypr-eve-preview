#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

/// Thumbnail of the given width for a buffer, height rounded half up. Needs both widths at least 1.
pub fn thumbnail_size(width: u32, buffer: Size) -> Size {
    let w = u64::from(width);
    let bw = u64::from(buffer.width);
    let bh = u64::from(buffer.height);
    let height = ((2 * w * bh + bw) / (2 * bw)).max(1);
    Size {
        width,
        height: u32::try_from(height).unwrap_or(u32::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_size_cases() {
        let cases = [
            ("window size", 480, (2560, 1406), (480, 264)),
            ("buffer size", 480, (3840, 2109), (480, 264)),
            ("exact half rounds up", 1, (2, 1), (1, 1)),
            ("minimum height", 480, (3840, 1), (480, 1)),
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
