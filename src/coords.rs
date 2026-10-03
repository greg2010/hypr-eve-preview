use crate::geometry::{Offset, Point, Rect, Size};
use crate::hypr::Monitor;
use crate::{hypr, layout};

/// The pointer in layout coordinates: the origin of the usable area it started in, the press
/// position of the thumbnail, the pointer's offset inside it at the press, and the drag offset.
pub(crate) fn pointer_global(
    origin: (i32, i32),
    start: Point,
    grab: (f64, f64),
    offset: (f64, f64),
) -> (f64, f64) {
    (
        f64::from(origin.0) + f64::from(start.x) + grab.0 + offset.0,
        f64::from(origin.1) + f64::from(start.y) + grab.1 + offset.1,
    )
}

/// A rectangle in layout coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Area {
    pub(crate) x: i64,
    pub(crate) y: i64,
    pub(crate) width: i64,
    pub(crate) height: i64,
}

impl Area {
    fn overlap(self, other: Area) -> i64 {
        let width = (self.x + self.width).min(other.x + other.width) - self.x.max(other.x);
        let height = (self.y + self.height).min(other.y + other.height) - self.y.max(other.y);
        if width > 0 && height > 0 {
            width * height
        } else {
            0
        }
    }
}

/// The monitor's usable area in layout coordinates.
fn usable_global(monitor: &Monitor) -> Area {
    let usable = monitor.usable_area();
    let origin = area_origin(monitor);
    Area {
        x: i64::from(origin.0),
        y: i64::from(origin.1),
        width: i64::from(usable.width),
        height: i64::from(usable.height),
    }
}

/// The top-left of a dragged thumbnail before snapping: the press position plus the rounded
/// offset, in layout coordinates.
pub(crate) fn desired_origin(origin: (i32, i32), start: Point, offset: (f64, f64)) -> (i64, i64) {
    (
        i64::from(origin.0) + i64::from(start.x) + offset.0.round() as i64,
        i64::from(origin.1) + i64::from(start.y) + offset.1.round() as i64,
    )
}

/// Whether the usable areas, which do not overlap each other, cover all of `rect`.
fn covered(rect: Area, areas: &[Area]) -> bool {
    let whole = rect.width * rect.height;
    whole > 0 && areas.iter().map(|a| rect.overlap(*a)).sum::<i64>() == whole
}

/// The top-left of the dragged rectangle in layout coordinates. It snaps in the usable area of
/// the monitor under the pointer, `under`, to its edges and the thumbnails `others` on it, and
/// rounds down to even there. The result is used as is when the usable areas of all monitors
/// cover it; otherwise the snapped position is clamped into the usable area of `under`.
pub(crate) fn dragged_origin(
    desired: (i64, i64),
    size: Size,
    under: usize,
    monitors: &[Monitor],
    others: &[Rect],
    distance: u32,
) -> (i64, i64) {
    let origin = area_origin(&monitors[under]);
    let (ox, oy) = (i64::from(origin.0), i64::from(origin.1));
    let usable = monitors[under].usable_area().size();
    let snapped = layout::snap(
        (desired.0 - ox, desired.1 - oy),
        size,
        others,
        usable,
        distance,
    );
    let even = (layout::even_down(snapped.0), layout::even_down(snapped.1));
    let rect = Area {
        x: even.0 + ox,
        y: even.1 + oy,
        width: i64::from(size.width),
        height: i64::from(size.height),
    };
    let areas: Vec<Area> = monitors.iter().map(usable_global).collect();
    if covered(rect, &areas) {
        return (rect.x, rect.y);
    }
    let clamped = layout::clamp_position(snapped.0, snapped.1, size, usable);
    (ox + i64::from(clamped.x), oy + i64::from(clamped.y))
}

/// The monitors other than `home` whose usable area the rectangle overlaps, ascending.
pub(crate) fn touched(monitors: &[Monitor], home: usize, rect: Area) -> Vec<usize> {
    monitors
        .iter()
        .enumerate()
        .filter(|(index, m)| *index != home && rect.overlap(usable_global(m)) > 0)
        .map(|(index, _)| index)
        .collect()
}

/// The monitor under `point` in layout coordinates. In a gap between monitors it is `last`.
pub(crate) fn under_pointer(monitors: &[Monitor], point: (f64, f64), last: usize) -> usize {
    hypr::monitor_at(monitors, point.0, point.1).unwrap_or(last)
}

/// Layout coordinates `at` relative to the top-left of the monitor's usable area.
pub(crate) fn usable_local(at: (i64, i64), monitor: &Monitor) -> (i64, i64) {
    let origin = area_origin(monitor);
    (
        at.0.saturating_sub(i64::from(origin.0)),
        at.1.saturating_sub(i64::from(origin.1)),
    )
}

/// The layout coordinates of `position`, a place relative to the monitor's usable area.
pub(crate) fn layout_point(position: Point, monitor: &Monitor) -> (i64, i64) {
    let origin = area_origin(monitor);
    (
        i64::from(origin.0) + i64::from(position.x),
        i64::from(origin.1) + i64::from(position.y),
    )
}

/// The margins of a surface on `monitor` that puts its top-left at layout coordinates `at`.
pub(crate) fn local_offset(at: (i64, i64), monitor: &Monitor) -> Offset {
    let (x, y) = usable_local(at, monitor);
    let fit = |value: i64| value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    Offset {
        x: fit(x),
        y: fit(y),
    }
}

/// The unsigned usable-relative position of a margin pair known to be clamped.
pub(crate) fn point_of(offset: Offset) -> Point {
    Point {
        x: u32::try_from(offset.x).unwrap_or(0),
        y: u32::try_from(offset.y).unwrap_or(0),
    }
}

/// The top-left of the monitor's usable area in layout coordinates.
pub(crate) fn area_origin(monitor: &Monitor) -> (i32, i32) {
    let usable = monitor.usable_area();
    (
        monitor.x.saturating_add_unsigned(usable.x),
        monitor.y.saturating_add_unsigned(usable.y),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{layout_monitors, monitor};

    #[test]
    fn area_origin_cases() {
        let at = |x, y, reserved| Monitor {
            x,
            y,
            reserved,
            ..monitor("M", 0, 0, 1920, 1.0, false)
        };
        let cases = [
            ("top bar", at(0, 0, [0, 34, 0, 0]), (0, 34), (1920, 1046)),
            (
                "scaled",
                layout_monitors()[1].clone(),
                (1920, 34),
                (2560, 1406),
            ),
            (
                "negative position",
                at(-1920, -200, [0, 34, 0, 0]),
                (-1920, -166),
                (1920, 1046),
            ),
            (
                "left reserve",
                at(0, 0, [48, 0, 0, 0]),
                (48, 0),
                (1872, 1080),
            ),
            (
                "left reserve at a position",
                at(1920, 0, [48, 0, 0, 0]),
                (1968, 0),
                (1872, 1080),
            ),
        ];
        for (name, m, origin, size) in cases {
            assert_eq!(area_origin(&m), origin, "{name}");
            let want = Size {
                width: size.0,
                height: size.1,
            };
            assert_eq!(m.usable_area().size(), want, "{name}");
        }
    }

    #[test]
    fn pointer_global_cases() {
        let cases = [
            (
                "no movement",
                (0, 34),
                (100, 50),
                (10.0, 20.0),
                (0.0, 0.0),
                (110.0, 104.0),
            ),
            (
                "offset moves it",
                (1920, 34),
                (100, 50),
                (10.0, 20.0),
                (-5.5, 7.25),
                (2024.5, 111.25),
            ),
        ];
        for (name, origin, start, grab, offset, want) in cases {
            let start = Point {
                x: start.0,
                y: start.1,
            };
            assert_eq!(pointer_global(origin, start, grab, offset), want, "{name}");
        }
    }

    #[test]
    fn desired_origin_cases() {
        let cases = [
            ("no movement", (0, 34), (100, 50), (0.0, 0.0), (100, 84)),
            (
                "offset rounds per axis",
                (1920, 34),
                (100, 50),
                (-5.5, 7.25),
                (2014, 91),
            ),
            (
                "half rounds away from zero",
                (0, 0),
                (1000, 500),
                (3.5, -2.5),
                (1004, 497),
            ),
            (
                "left of the layout",
                (0, 0),
                (10, 10),
                (-40.0, -40.0),
                (-30, -30),
            ),
        ];
        for (name, origin, start, offset, want) in cases {
            let start = Point {
                x: start.0,
                y: start.1,
            };
            assert_eq!(desired_origin(origin, start, offset), want, "{name}");
        }
    }

    fn usable_areas(monitors: &[Monitor]) -> Vec<Area> {
        monitors.iter().map(usable_global).collect()
    }

    #[test]
    fn covered_cases() {
        let areas = usable_areas(&layout_monitors());
        let rect = |x, y| Area {
            x,
            y,
            width: 480,
            height: 264,
        };
        let cases = [
            ("inside one", &areas[..], rect(100, 134), true),
            ("straddles two monitors", &areas[..], rect(1700, 134), true),
            ("over a bar region", &areas[..], rect(100, 0), false),
            ("past the last monitor", &areas[..], rect(4300, 100), false),
            (
                "below the shorter monitor",
                &areas[..],
                rect(1800, 1000),
                false,
            ),
            (
                "in the gap between monitors",
                &areas[..],
                rect(4500, 100),
                false,
            ),
            ("no monitors", &[][..], rect(0, 0), false),
            (
                "zero-area rectangle inside a monitor",
                &areas[..],
                Area {
                    width: 0,
                    ..rect(100, 134)
                },
                false,
            ),
        ];
        for (name, areas, rect, want) in cases {
            assert_eq!(covered(rect, areas), want, "{name}");
        }
    }

    #[test]
    fn dragged_origin_cases() {
        let monitors = layout_monitors();
        let size = Size {
            width: 480,
            height: 264,
        };
        let neighbour = Rect {
            x: 1000,
            y: 400,
            width: 320,
            height: 176,
        };
        type Case<'a> = (
            &'a str,
            &'a [Monitor],
            (i64, i64),
            usize,
            &'a [Rect],
            (i64, i64),
        );
        let cases: [Case; 8] = [
            (
                "single monitor, inside",
                &monitors[..1],
                (100, 134),
                0,
                &[],
                (100, 134),
            ),
            (
                "single monitor, clamped",
                &monitors[..1],
                (-50, -50),
                0,
                &[],
                (0, 34),
            ),
            (
                "straddles two monitors, used as is",
                &monitors[..2],
                (1700, 134),
                1,
                &[],
                (1700, 134),
            ),
            (
                "straddling, snapped to the usable edge of the pointer's monitor",
                &monitors[..2],
                (1925, 134),
                1,
                &[],
                (1920, 134),
            ),
            (
                "over a bar region, clamped",
                &monitors[..2],
                (100, 10),
                0,
                &[],
                (100, 34),
            ),
            (
                "past the outer edge, clamped into the pointer's monitor",
                &monitors[..2],
                (4400, 134),
                1,
                &[],
                (4000, 134),
            ),
            (
                "snapped to a thumbnail",
                &monitors[..2],
                (1315, 84),
                0,
                &[neighbour],
                (1320, 84),
            ),
            (
                "odd position rounds down to even",
                &monitors[..1],
                (101, 135),
                0,
                &[],
                (100, 134),
            ),
        ];
        for (name, monitors, desired, under, others, want) in cases {
            let got = dragged_origin(desired, size, under, monitors, others, 10);
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn dragged_origin_usable_width_cases() {
        let size = Size {
            width: 480,
            height: 264,
        };
        let cases = [
            (
                "even width, right-edge snap",
                2560,
                (2075, 134),
                (2080, 134),
            ),
            (
                "odd width rounds the right-edge snap down",
                2561,
                (2075, 134),
                (2080, 134),
            ),
            (
                "snap past the edge is clamped back",
                2560,
                (2600, 134),
                (2080, 134),
            ),
        ];
        for (name, width, desired, want) in cases {
            let monitors = [Monitor {
                height: 1440,
                ..monitor("M", 0, 0, width, 1.0, true)
            }];
            let got = dragged_origin(desired, size, 0, &monitors, &[], 10);
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn touched_cases() {
        let monitors = layout_monitors();
        let flat = |name, x, y| Monitor {
            reserved: [0; 4],
            ..monitor(name, x, y, 1920, 1.0, false)
        };
        let corner = [flat("A", 0, 0), flat("B", 1920, 0), flat("C", 0, 1080)];
        let rect = |x, y| Area {
            x,
            y,
            width: 480,
            height: 264,
        };
        let cases = [
            ("inside home", &monitors[..], 0, rect(100, 134), vec![]),
            (
                "scale 1 into scale 1.5",
                &monitors[..],
                0,
                rect(1700, 134),
                vec![1],
            ),
            (
                "scale 1.5 into scale 1",
                &monitors[..],
                1,
                rect(1700, 134),
                vec![0],
            ),
            (
                "bar region of the neighbour",
                &monitors[..],
                0,
                Area {
                    x: 1700,
                    y: 0,
                    width: 480,
                    height: 30,
                },
                vec![],
            ),
            (
                "edge contact only",
                &monitors[..],
                0,
                rect(1440, 134),
                vec![],
            ),
            ("third monitor", &monitors[..], 0, rect(4900, 134), vec![2]),
            (
                "corner of three monitors",
                &corner[..],
                0,
                rect(1800, 1000),
                vec![1, 2],
            ),
        ];
        for (name, monitors, home, rect, want) in cases {
            assert_eq!(touched(monitors, home, rect), want, "{name}");
        }
    }

    #[test]
    fn under_pointer_cases() {
        let monitors = layout_monitors();
        let cases = [
            ("on the first monitor", (100.0, 100.0), 1, 0),
            ("on the second monitor", (2000.0, 100.0), 0, 1),
            (
                "at the shared edge, the right monitor",
                (1920.0, 100.0),
                0,
                1,
            ),
            ("in a gap keeps the last monitor", (4600.0, 100.0), 1, 1),
            ("in a gap keeps the first monitor", (4600.0, 100.0), 0, 0),
            ("below every monitor", (100.0, 5000.0), 2, 2),
        ];
        for (name, point, last, want) in cases {
            assert_eq!(under_pointer(&monitors, point, last), want, "{name}");
        }
    }

    fn placed(x: i32, y: i32, reserved: [u32; 4]) -> Monitor {
        Monitor {
            x,
            y,
            reserved,
            ..monitor("M", 0, 0, 1920, 1.0, false)
        }
    }

    #[test]
    fn local_offset_cases() {
        let scaled = layout_monitors()[1].clone();
        let flat = placed(0, 0, [0; 4]);
        let cases = [
            (
                "scale 1.0",
                placed(0, 0, [0, 34, 0, 0]),
                (100, 134),
                (100, 100),
                (100, 100),
            ),
            (
                "scale 1.5",
                scaled.clone(),
                (2000, 134),
                (80, 100),
                (80, 100),
            ),
            (
                "monitor at a negative position",
                placed(-1920, -200, [0, 34, 0, 0]),
                (-1900, -100),
                (20, 66),
                (20, 66),
            ),
            (
                "left reserve, a negative local coordinate",
                placed(0, 0, [48, 0, 0, 0]),
                (40, 10),
                (-8, 10),
                (-8, 10),
            ),
            (
                "a point past the surface size",
                scaled,
                (6000, 3000),
                (4080, 2966),
                (4080, 2966),
            ),
            (
                "beyond the i32 range",
                flat,
                (i64::MAX, i64::MIN),
                (i64::MAX, i64::MIN),
                (i32::MAX, i32::MIN),
            ),
        ];
        for (name, monitor, at, relative, (x, y)) in cases {
            assert_eq!(usable_local(at, &monitor), relative, "{name} relative");
            assert_eq!(local_offset(at, &monitor), Offset { x, y }, "{name} offset");
        }
    }

    #[test]
    fn layout_point_cases() {
        let cases = [
            (
                "scale 1.0",
                placed(0, 0, [0, 34, 0, 0]),
                (100, 100),
                (100, 134),
            ),
            (
                "scale 1.5",
                layout_monitors()[1].clone(),
                (80, 100),
                (2000, 134),
            ),
            (
                "monitor at a negative position",
                placed(-1920, -200, [0, 34, 0, 0]),
                (20, 66),
                (-1900, -100),
            ),
            (
                "left reserve",
                placed(0, 0, [48, 0, 0, 0]),
                (0, 10),
                (48, 10),
            ),
        ];
        for (name, monitor, (x, y), want) in cases {
            assert_eq!(layout_point(Point { x, y }, &monitor), want, "{name}");
            assert_eq!(
                usable_local(want, &monitor),
                (i64::from(x), i64::from(y)),
                "{name} inverse"
            );
        }
    }

    #[test]
    fn point_of_cases() {
        let cases = [
            ("inside", (80, 100), (80, 100)),
            ("origin", (0, 0), (0, 0)),
            ("negative x", (-8, 10), (0, 10)),
            ("negative y", (10, -8), (10, 0)),
            (
                "largest",
                (i32::MAX, i32::MAX),
                (2_147_483_647, 2_147_483_647),
            ),
        ];
        for (name, (x, y), (want_x, want_y)) in cases {
            let want = Point {
                x: want_x,
                y: want_y,
            };
            assert_eq!(point_of(Offset { x, y }), want, "{name}");
        }
    }
}
