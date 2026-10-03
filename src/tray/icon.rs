use crate::config::Color;

pub(super) const HIDDEN_ICON_COLOR: Color = Color {
    r: 0xFF,
    g: 0x00,
    b: 0x00,
    a: 0xFF,
};

/// An icon image: ARGB bytes, row by row, not premultiplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pixmap {
    pub width: i32,
    pub height: i32,
    pub argb: Vec<u8>,
}

/// The ring icon at 24 and 48 px in `color`.
pub fn icon(color: Color) -> Vec<Pixmap> {
    [24, 48]
        .into_iter()
        .map(|size| Pixmap {
            width: size,
            height: size,
            argb: ring(size, color),
        })
        .collect()
}

fn ring(size: i32, color: Color) -> Vec<u8> {
    let side = f64::from(size);
    let centre = side / 2.0;
    let (inner, outer) = (0.30 * side, 0.45 * side);
    let on = [color.a, color.r, color.g, color.b];
    (0..size)
        .flat_map(|y| (0..size).map(move |x| (x, y)))
        .flat_map(|(x, y)| {
            let distance = (f64::from(x) + 0.5 - centre).hypot(f64::from(y) + 0.5 - centre);
            if (inner..=outer).contains(&distance) {
                on
            } else {
                [0; 4]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tray::fixtures::GREEN;

    #[test]
    fn icon_cases() {
        let clear = [0, 0, 0, 0];
        let cases = [
            ("green", GREEN, [0xFF, 0x40, 0xFF, 0x00]),
            ("hidden red", HIDDEN_ICON_COLOR, [0xFF, 0xFF, 0x00, 0x00]),
        ];
        for (name, color, ring) in cases {
            let pixmaps = icon(color);
            let sizes: Vec<(i32, i32, usize)> = pixmaps
                .iter()
                .map(|pixmap| (pixmap.width, pixmap.height, pixmap.argb.len()))
                .collect();
            assert_eq!(sizes, vec![(24, 24, 2_304), (48, 48, 9_216)], "{name}");

            let pixels = [
                ("outer ring row", 12, 1, ring),
                ("inner ring row", 12, 4, ring),
                ("inside the ring", 12, 5, clear),
                ("centre", 12, 12, clear),
                ("above the ring", 12, 0, clear),
                ("corner", 0, 0, clear),
            ];
            for (what, x, y, want) in pixels {
                let at = (y * 24 + x) * 4;
                assert_eq!(pixmaps[0].argb[at..at + 4], want, "{name} {what}");
            }
            let at = (2 * 48 + 24) * 4;
            assert_eq!(pixmaps[1].argb[at..at + 4], ring, "{name} 48 px ring");
        }
    }
}
