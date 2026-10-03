use crate::geometry::{Point, Size, thumbnail_size};
use crate::hypr::Monitor;
use crate::{config, layout};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    Default,
    Saved,
    User,
}

/// The width a request gets on a monitor with `usable`: negative requests count as zero.
pub(crate) fn capped_width(requested: i64, thumbnail: &config::Thumbnail, usable: Size) -> u32 {
    let requested = u32::try_from(requested.max(0)).unwrap_or(u32::MAX);
    layout::effective_width(requested, thumbnail, usable)
}

/// The width a request gets on a monitor with `usable`, the size that width gives for a frame
/// of `buffer`, and `at` (usable-relative) clamped.
pub(crate) fn fitted(
    width: i64,
    thumbnail: &config::Thumbnail,
    buffer: Size,
    usable: Size,
    at: (i64, i64),
) -> (u32, Size, Point) {
    let width = capped_width(width, thumbnail, usable);
    let size = thumbnail_size(width, buffer);
    (
        width,
        size,
        layout::clamp_position(at.0, at.1, size, usable),
    )
}

/// What `settle` does after `relocation` chose `target`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Settle {
    Relocate {
        target: Option<usize>,
        width: i64,
        at: Option<(i64, i64)>,
    },
    Save,
    Stay,
}

/// A saved record with an entry always applies the entry's geometry, so a second key change
/// inside a move's window leaves the record where the layout holds it. `width` is the record's.
pub(crate) fn settled(
    target: Option<usize>,
    origin: Origin,
    entry: Option<&layout::Entry>,
    width: u32,
) -> Settle {
    match (entry, target) {
        (Some(entry), _) => Settle::Relocate {
            target,
            width: i64::from(entry.width),
            at: Some((i64::from(entry.x), i64::from(entry.y))),
        },
        (None, Some(_)) => Settle::Relocate {
            target,
            width: i64::from(width),
            at: None,
        },
        (None, None) if origin == Origin::User => Settle::Save,
        (None, None) => Settle::Stay,
    }
}

/// What asks where a record lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trigger<'a> {
    /// `output` is the monitor the new key's entry names; `busy` is a drag or a pending landing.
    KeyChange { output: Option<&'a str>, busy: bool },
    /// A drop or a commit left the record on its landing monitor. `entry` is the current key's
    /// saved entry, `None` without one, and the monitor it names.
    Settled { entry: Option<Option<&'a str>> },
}

/// The monitor a record moves to, or `None` to stay on `monitor`. A key change goes to its
/// entry's monitor or `default`, never while busy. After a settle a user-placed record stays,
/// a default-placed one goes to `default`, a saved one to its entry's monitor or `default`, or
/// stays without an entry.
pub(crate) fn relocation(
    trigger: Trigger,
    monitors: &[Monitor],
    origin: Origin,
    monitor: usize,
    default: usize,
) -> Option<usize> {
    let target = match trigger {
        Trigger::KeyChange { busy: true, .. } => return None,
        Trigger::KeyChange { output, .. } => monitor_for(monitors, output, default),
        Trigger::Settled { .. } if origin == Origin::Default => default,
        Trigger::Settled {
            entry: Some(output),
        } if origin == Origin::Saved => monitor_for(monitors, output, default),
        Trigger::Settled { .. } => return None,
    };
    (target != monitor).then_some(target)
}

/// Whether a client sits at its default-row slot. The client of an active gesture keeps the
/// geometry the gesture gave it: nothing re-places it until the gesture ends.
pub(crate) fn follows_row(origin: Origin, active: Option<u64>, address: u64) -> bool {
    origin == Origin::Default && active != Some(address)
}

/// The default-row slots to apply. The client of an active gesture keeps its slot in the row but
/// is not moved. Its re-placement is deferred until the gesture ends.
pub(crate) fn placements(order: &[u64], gesture: Option<u64>) -> Vec<(usize, u64)> {
    order
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, address)| Some(*address) != gesture)
        .collect()
}

/// The index of the monitor new thumbnails start on: the one named `output`, or the focused one
/// when `output` is unset. The error is the `exit` reason.
pub(crate) fn default_monitor(monitors: &[Monitor], output: Option<&str>) -> Result<usize, String> {
    match output {
        Some(name) => monitors
            .iter()
            .position(|m| m.name == name)
            .ok_or_else(|| format!("no monitor named {name} in j/monitors")),
        None => monitors
            .iter()
            .position(|m| m.focused)
            .ok_or_else(|| "no focused monitor in j/monitors".to_string()),
    }
}

/// The monitor a saved entry names, or `default` when it names none or one that is absent.
pub(crate) fn monitor_for(monitors: &[Monitor], output: Option<&str>, default: usize) -> usize {
    output
        .and_then(|name| monitors.iter().position(|m| m.name == name))
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::layout_monitors;

    #[test]
    fn placements_cases() {
        type Case<'a> = (&'a str, &'a [u64], Option<u64>, Vec<(usize, u64)>);
        let cases: [Case; 4] = [
            ("empty", &[], None, vec![]),
            (
                "gesture on a client outside the row",
                &[7, 9],
                Some(8),
                vec![(0, 7), (1, 9)],
            ),
            ("no gesture", &[7, 8, 9], None, vec![(0, 7), (1, 8), (2, 9)]),
            (
                "gesture on the middle",
                &[7, 8, 9],
                Some(8),
                vec![(0, 7), (2, 9)],
            ),
        ];
        for (name, order, gesture, want) in cases {
            assert_eq!(placements(order, gesture), want, "{name}");
        }
    }

    #[test]
    fn follows_row_cases() {
        let placed = Origin::User;
        let cases = [
            ("default, idle", Origin::Default, None, 7, true),
            ("default, other gesture", Origin::Default, Some(8), 7, true),
            ("default, own gesture", Origin::Default, Some(7), 7, false),
            ("saved, idle", Origin::Saved, None, 7, false),
            ("placed, idle", placed, None, 7, false),
            ("placed, own gesture", placed, Some(7), 7, false),
        ];
        for (name, origin, active, address, want) in cases {
            assert_eq!(follows_row(origin, active, address), want, "{name}");
        }
    }

    #[test]
    fn default_monitor_cases() {
        let monitors = layout_monitors();
        let unfocused: Vec<Monitor> = monitors
            .iter()
            .cloned()
            .map(|m| Monitor {
                focused: false,
                ..m
            })
            .collect();
        type Case<'a> = (
            &'a str,
            &'a [Monitor],
            Option<&'a str>,
            Result<usize, &'a str>,
        );
        let two_focused: Vec<Monitor> = monitors
            .iter()
            .cloned()
            .map(|m| Monitor {
                focused: m.name != "DP-1",
                ..m
            })
            .collect();
        let cases: [Case; 8] = [
            ("named and present", &monitors, Some("HDMI-A-1"), Ok(2)),
            (
                "named and absent",
                &monitors,
                Some("DP-9"),
                Err("no monitor named DP-9 in j/monitors"),
            ),
            ("unset with one focused", &monitors, None, Ok(1)),
            (
                "unset with none focused",
                &unfocused,
                None,
                Err("no focused monitor in j/monitors"),
            ),
            ("named wins over focus", &monitors, Some("DP-1"), Ok(0)),
            ("two focused, the first wins", &two_focused, None, Ok(1)),
            (
                "named, empty list",
                &[],
                Some("DP-1"),
                Err("no monitor named DP-1 in j/monitors"),
            ),
            (
                "unset, empty list",
                &[],
                None,
                Err("no focused monitor in j/monitors"),
            ),
        ];
        for (name, monitors, output, want) in cases {
            let want = want.map_err(String::from);
            assert_eq!(default_monitor(monitors, output), want, "{name}");
        }
    }

    #[test]
    fn monitor_for_cases() {
        let monitors = layout_monitors();
        let cases = [
            ("names a monitor", Some("HDMI-A-1"), 2),
            ("names an absent monitor", Some("DP-9"), 1),
            ("names none", None, 1),
        ];
        for (name, output, want) in cases {
            assert_eq!(monitor_for(&monitors, output, 1), want, "{name}");
        }
    }

    #[test]
    fn fitted_cases() {
        let thumbnail = config::Thumbnail {
            width: 480,
            min_width: 160,
            max_width: 1600,
            opacity: 100,
            snap_distance: 10,
        };
        let buffer = Size {
            width: 2560,
            height: 1406,
        };
        let usable = |width, height| Size { width, height };
        let size = |width, height| Size { width, height };
        let at = |x, y| Point { x, y };
        let cases = [
            (
                "fits",
                480,
                usable(2560, 1000),
                (100, 134),
                (480, size(480, 264), at(100, 134)),
            ),
            (
                "landing without a resize is unchanged",
                480,
                usable(2560, 1000),
                (80, 100),
                (480, size(480, 264), at(80, 100)),
            ),
            (
                "wider than the usable width",
                1280,
                usable(1080, 1000),
                (0, 0),
                (1080, size(1080, 594), at(0, 0)),
            ),
            (
                "width grown past the landing monitor is capped and the position clamped",
                1280,
                usable(1080, 1000),
                (80, 100),
                (1080, size(1080, 594), at(0, 100)),
            ),
            (
                "landing monitor's width on a narrower monitor",
                1128,
                usable(1080, 1000),
                (0, 0),
                (1080, size(1080, 594), at(0, 0)),
            ),
            (
                "grown past the old monitor's usable width",
                1120,
                usable(2560, 1000),
                (0, 0),
                (1120, size(1120, 616), at(0, 0)),
            ),
            (
                "negative request",
                -5,
                usable(2560, 1000),
                (0, 0),
                (160, size(160, 88), at(0, 0)),
            ),
            (
                "request above u32::MAX",
                5_000_000_000,
                usable(2560, 1000),
                (0, 0),
                (1600, size(1600, 878), at(0, 0)),
            ),
            (
                "odd request rounds down",
                481,
                usable(2560, 1000),
                (0, 0),
                (480, size(480, 264), at(0, 0)),
            ),
            (
                "below the minimum",
                100,
                usable(2560, 1000),
                (0, 0),
                (160, size(160, 88), at(0, 0)),
            ),
            (
                "above the configured maximum",
                2000,
                usable(2560, 1000),
                (0, 0),
                (1600, size(1600, 878), at(0, 0)),
            ),
            (
                "left of the area",
                480,
                usable(2560, 1000),
                (-220, 100),
                (480, size(480, 264), at(0, 100)),
            ),
            (
                "above the area",
                480,
                usable(2560, 1000),
                (80, -34),
                (480, size(480, 264), at(80, 0)),
            ),
            (
                "beyond the right edge",
                480,
                usable(2560, 1000),
                (2480, 100),
                (480, size(480, 264), at(2080, 100)),
            ),
            (
                "beyond the bottom edge",
                480,
                usable(1920, 1046),
                (100, 966),
                (480, size(480, 264), at(100, 782)),
            ),
            (
                "odd position rounds down to even",
                480,
                usable(2560, 1000),
                (101, 135),
                (480, size(480, 264), at(100, 134)),
            ),
        ];
        for (name, width, usable, position, want) in cases {
            assert_eq!(
                fitted(width, &thumbnail, buffer, usable, position),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn relocation_cases() {
        let monitors = layout_monitors();
        let key = |output, busy| Trigger::KeyChange { output, busy };
        let cases = [
            (
                "key change to a present monitor",
                key(Some("HDMI-A-1"), false),
                Origin::Saved,
                0,
                1,
                Some(2),
            ),
            (
                "key change to the record's own monitor",
                key(Some("DP-1"), false),
                Origin::Saved,
                0,
                1,
                None,
            ),
            (
                "entry without output goes to the default monitor",
                key(None, false),
                Origin::Saved,
                2,
                1,
                Some(1),
            ),
            (
                "entry naming an absent monitor goes to the default monitor",
                key(Some("DP-9"), false),
                Origin::Saved,
                2,
                1,
                Some(1),
            ),
            (
                "default placement on the default monitor",
                key(None, false),
                Origin::Default,
                1,
                1,
                None,
            ),
            (
                "default placement away from the default monitor",
                key(None, false),
                Origin::Default,
                0,
                1,
                Some(1),
            ),
            (
                "key change during a drag or a landing, present monitor",
                key(Some("HDMI-A-1"), true),
                Origin::Saved,
                0,
                1,
                None,
            ),
            (
                "key change during a drag or a landing, default",
                key(None, true),
                Origin::Default,
                0,
                1,
                None,
            ),
            (
                "drop of a user placement stays",
                Trigger::Settled { entry: None },
                Origin::User,
                2,
                1,
                None,
            ),
            (
                "drop of a default placement returns to the default monitor",
                Trigger::Settled { entry: None },
                Origin::Default,
                2,
                1,
                Some(1),
            ),
            (
                "drop of a default placement on the default monitor",
                Trigger::Settled { entry: None },
                Origin::Default,
                1,
                1,
                None,
            ),
            (
                "saved placement on its entry's monitor",
                Trigger::Settled {
                    entry: Some(Some("HDMI-A-1")),
                },
                Origin::Saved,
                2,
                1,
                None,
            ),
            (
                "saved placement, entry names another present monitor",
                Trigger::Settled {
                    entry: Some(Some("HDMI-A-1")),
                },
                Origin::Saved,
                0,
                1,
                Some(2),
            ),
            (
                "saved placement, entry without output, on the default monitor",
                Trigger::Settled { entry: Some(None) },
                Origin::Saved,
                1,
                1,
                None,
            ),
            (
                "saved placement, entry without output, off the default monitor",
                Trigger::Settled { entry: Some(None) },
                Origin::Saved,
                2,
                1,
                Some(1),
            ),
            (
                "saved placement, entry names an absent monitor",
                Trigger::Settled {
                    entry: Some(Some("DP-9")),
                },
                Origin::Saved,
                2,
                1,
                Some(1),
            ),
            (
                "saved placement without an entry on the default monitor",
                Trigger::Settled { entry: None },
                Origin::Saved,
                1,
                1,
                None,
            ),
            (
                "saved placement without an entry off the default monitor",
                Trigger::Settled { entry: None },
                Origin::Saved,
                2,
                1,
                None,
            ),
            (
                "user placement ignores the entry's monitor",
                Trigger::Settled {
                    entry: Some(Some("HDMI-A-1")),
                },
                Origin::User,
                0,
                1,
                None,
            ),
        ];
        for (name, trigger, origin, monitor, default, want) in cases {
            let got = relocation(trigger, &monitors, origin, monitor, default);
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn settled_cases() {
        let entry = |width, x, y| layout::Entry {
            x,
            y,
            width,
            output: None,
        };
        let relocate = |target, width, at| Settle::Relocate { target, width, at };
        let cases = [
            (
                "saved, entry on the same monitor, geometry applied",
                None,
                Origin::Saved,
                Some(entry(400, 30, 40)),
                relocate(None, 400, Some((30, 40))),
            ),
            (
                "saved, entry on another monitor, moves with the entry's geometry",
                Some(2),
                Origin::Saved,
                Some(entry(400, 30, 40)),
                relocate(Some(2), 400, Some((30, 40))),
            ),
            (
                "saved without an entry stays",
                None,
                Origin::Saved,
                None,
                Settle::Stay,
            ),
            (
                "default placement moves to its row",
                Some(1),
                Origin::Default,
                None,
                relocate(Some(1), 480, None),
            ),
            (
                "default placement on the default monitor stays",
                None,
                Origin::Default,
                None,
                Settle::Stay,
            ),
            (
                "user placement is saved",
                None,
                Origin::User,
                None,
                Settle::Save,
            ),
        ];
        for (name, target, origin, entry, want) in cases {
            assert_eq!(settled(target, origin, entry.as_ref(), 480), want, "{name}");
        }
    }
}
