use std::fmt;

use serde::Deserialize;
use serde::de::Error as _;

use crate::geometry::{Rect, Size};

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub name: String,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Client {
    #[serde(deserialize_with = "deserialize_address")]
    pub address: u64,
    pub title: String,
    pub workspace: Workspace,
    pub pid: i32,
}

fn deserialize_address<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let s = String::deserialize(d)?;
    parse_address(&s).map_err(D::Error::custom)
}

#[derive(Debug)]
pub enum HyprError {
    Json(serde_json::Error),
    InvalidAddress(String),
}

impl fmt::Display for HyprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HyprError::Json(e) => write!(f, "reply is not valid JSON of the expected shape: {e}"),
            HyprError::InvalidAddress(s) => write!(f, "invalid address {s:?}"),
        }
    }
}

impl std::error::Error for HyprError {}

/// Parses `0x` or `0X` followed by 1 to 16 hex digits.
pub fn parse_address(s: &str) -> Result<u64, HyprError> {
    let digits = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .ok_or_else(|| HyprError::InvalidAddress(s.to_string()))?;
    parse_event_address(digits).map_err(|_| HyprError::InvalidAddress(s.to_string()))
}

/// Parses the output of `hyprctl clients -j`. Unknown fields are ignored.
pub fn parse_clients(json: &str) -> Result<Vec<Client>, HyprError> {
    serde_json::from_str(json).map_err(HyprError::Json)
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct Monitor {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub transform: u32,
    pub reserved: [u32; 4],
    pub focused: bool,
}

/// Parses the event form of an address: 1 to 16 hex digits with no prefix.
pub fn parse_event_address(s: &str) -> Result<u64, HyprError> {
    if s.is_empty() || s.len() > 16 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(HyprError::InvalidAddress(s.to_string()));
    }
    u64::from_str_radix(s, 16).map_err(|_| HyprError::InvalidAddress(s.to_string()))
}

pub fn parse_monitors(json: &str) -> Result<Vec<Monitor>, HyprError> {
    serde_json::from_str(json).map_err(HyprError::Json)
}

/// Parses `j/activewindow`: `{}` means no active window.
pub fn parse_active_window(json: &str) -> Result<Option<u64>, HyprError> {
    let window: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(json).map_err(HyprError::Json)?;
    if window.is_empty() {
        return Ok(None);
    }
    let address = window
        .get("address")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            HyprError::Json(serde_json::Error::custom("missing string field `address`"))
        })?;
    parse_address(address).map(Some)
}

impl Monitor {
    /// The output's size in logical px: the mode divided by the scale and rounded, with width and
    /// height swapped for an odd transform.
    pub fn logical_size(&self) -> Size {
        let (width, height) = if self.transform % 2 == 1 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        };
        let logical = |px: u32| (f64::from(px) / self.scale).round() as u32;
        Size {
            width: logical(width),
            height: logical(height),
        }
    }

    /// The output's logical area minus the reserved edges, in logical px.
    pub fn usable_area(&self) -> Rect {
        let size = self.logical_size();
        let [left, top, right, bottom] = self.reserved;
        Rect {
            x: left,
            y: top,
            width: size.width.saturating_sub(left).saturating_sub(right),
            height: size.height.saturating_sub(top).saturating_sub(bottom),
        }
    }
}

/// The index of the first monitor whose logical rectangle contains the layout point. The left and
/// top edges belong to a monitor, the right and bottom edges to its neighbour.
pub fn monitor_at(monitors: &[Monitor], x: f64, y: f64) -> Option<usize> {
    monitors.iter().position(|m| {
        let size = m.logical_size();
        x >= f64::from(m.x)
            && x < f64::from(m.x) + f64::from(size.width)
            && y >= f64::from(m.y)
            && y < f64::from(m.y) + f64::from(size.height)
    })
}

pub const GAME_TITLE_PREFIX: &str = "EVE - ";

/// The command-line argument only the EVE launcher passes to a game client.
pub const LAUNCHER_ARG: &str = "/LauncherData=";

pub fn is_game_title(title: &str) -> bool {
    title == "EVE" || title.starts_with(GAME_TITLE_PREFIX)
}

/// Whether a window is a game client: a game title and an EVE launcher command line.
pub fn is_game_client(title: &str, cmdline: &str) -> bool {
    is_game_title(title) && cmdline.contains(LAUNCHER_ARG)
}

/// The slot of an `EVE<n>` workspace name, n in 1 to 12 in decimal with no leading zero.
pub fn eve_slot(workspace_name: &str) -> Option<u8> {
    let digits = workspace_name.strip_prefix("EVE")?;
    if digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u8>().ok().filter(|n| (1..=12).contains(n))
}

/// The low 32 bits of the address, which Hyprland matches against its window pointers.
pub fn capture_handle(address: u64) -> u32 {
    (address & 0xFFFF_FFFF) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../testdata/hyprctl-clients.json");

    const A: u64 = 0x555512345678;
    const B: u64 = 0x55559abcdef0;
    const LAUNCHER: u64 = 0x555500001000;
    const HELPER: u64 = 0x555500002000;

    fn entry_a() -> String {
        let all: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        all[0].to_string()
    }

    #[test]
    fn parse_address_cases() {
        let cases: Vec<(&str, &str, Option<u64>)> = vec![
            ("lower", "0x555512345678", Some(A)),
            ("upper prefix", "0X555512345678", Some(A)),
            ("upper digits", "0x55559ABCDEF0", Some(B)),
            ("one digit", "0x1", Some(1)),
            ("sixteen digits", "0xffffffffffffffff", Some(u64::MAX)),
            ("empty", "", None),
            ("prefix only", "0x", None),
            ("no prefix", "555512345678", None),
            ("non-hex", "0xg1", None),
            ("seventeen digits", "0x10000000000000000", None),
            ("plus sign", "0x+1", None),
        ];
        for (name, input, want) in cases {
            match (parse_address(input), want) {
                (Ok(got), Some(want)) => assert_eq!(got, want, "{name}"),
                (Err(HyprError::InvalidAddress(s)), None) => assert_eq!(s, input, "{name}"),
                (got, want) => panic!("{name}: got {got:?}, want {want:?}"),
            }
        }
    }

    #[test]
    fn parse_event_address_cases() {
        let cases: Vec<(&str, &str, Option<u64>)> = vec![
            ("lower", "555512345678", Some(A)),
            ("upper", "55559ABCDEF0", Some(B)),
            ("one digit", "1", Some(1)),
            ("sixteen digits", "ffffffffffffffff", Some(u64::MAX)),
            ("empty", "", None),
            ("prefixed", "0x555512345678", None),
            ("non-hex", "g1", None),
            ("seventeen digits", "10000000000000000", None),
            ("plus sign", "+1", None),
        ];
        for (name, input, want) in cases {
            match (parse_event_address(input), want) {
                (Ok(got), Some(want)) => assert_eq!(got, want, "{name}"),
                (Err(HyprError::InvalidAddress(s)), None) => assert_eq!(s, input, "{name}"),
                (got, want) => panic!("{name}: got {got:?}, want {want:?}"),
            }
        }
    }

    #[test]
    fn capture_handle_cases() {
        let cases = [
            ("a", A, 305419896),
            ("b", B, 2596069104),
            ("doc example", 0xd161e7b0, 3512854448),
        ];
        for (name, address, want) in cases {
            assert_eq!(capture_handle(address), want, "{name}");
        }
    }

    #[test]
    fn eve_slot_cases() {
        let cases = [
            ("EVE1", Some(1)),
            ("EVE9", Some(9)),
            ("EVE12", Some(12)),
            ("EVE", None),
            ("EVE-new", None),
            ("EVE0", None),
            ("EVE01", None),
            ("EVE13", None),
            ("EVE+1", None),
            ("Games", None),
        ];
        for (name, want) in cases {
            assert_eq!(eve_slot(name), want, "{name}");
        }
    }

    #[test]
    fn is_game_title_cases() {
        let cases = [
            ("EVE", true),
            ("EVE - Char Name", true),
            ("EVE - ", true),
            ("EVE Launcher", false),
            ("", false),
            ("EVE -", false),
            ("eve", false),
        ];
        for (title, want) in cases {
            assert_eq!(is_game_title(title), want, "{title:?}");
        }
    }

    #[test]
    fn is_game_client_cases() {
        let launched = format!("exefile.exe /ssoToken=x {LAUNCHER_ARG}abc /language=en");
        let launched = launched.as_str();
        let cases = [
            ("EVE", launched, true),
            ("EVE - Zentiv", launched, true),
            ("EVE - Foo - Google Chrome", "chrome --new-window", false),
            ("EVE Launcher", launched, false),
            ("", launched, false),
            ("eve - x", launched, false),
            ("EVE - X", "exefile.exe /language=en", false),
        ];
        for (title, cmdline, want) in cases {
            assert_eq!(
                is_game_client(title, cmdline),
                want,
                "{title:?} {cmdline:?}"
            );
        }
    }

    #[test]
    fn parse_clients_cases() {
        type Row = (u64, i32, String, String);
        let row = |address, pid, workspace: &str, title: &str| -> Row {
            (address, pid, workspace.to_string(), title.to_string())
        };
        let fixture = vec![
            row(A, 1001, "EVE2", "EVE - Pilot One"),
            row(B, 1002, "EVE3", "EVE"),
            row(LAUNCHER, 1003, "Games", "EVE Launcher"),
            row(HELPER, 1004, "Games", ""),
        ];
        let cases: [(&str, &str, Option<Vec<Row>>); 4] = [
            ("fixture", FIXTURE, Some(fixture)),
            ("not json", "not json", None),
            ("entry without address", "[{}]", None),
            ("not an array", "{}", None),
        ];
        for (name, input, want) in cases {
            match (parse_clients(input), want) {
                (Ok(clients), Some(want)) => {
                    let got: Vec<Row> = clients
                        .into_iter()
                        .map(|c| (c.address, c.pid, c.workspace.name, c.title))
                        .collect();
                    assert_eq!(got, want, "{name}");
                }
                (Err(HyprError::Json(_)), None) => {}
                (got, want) => panic!("{name}: got {got:?}, want {want:?}"),
            }
        }
    }

    #[test]
    fn parse_active_window_cases() {
        let a = entry_a();
        let cases: Vec<(&str, &str, Option<Option<u64>>)> = vec![
            ("empty object", "{}", Some(None)),
            ("fixture entry a", &a, Some(Some(A))),
            ("array", "[]", None),
            ("not json", "not json", None),
        ];
        for (name, input, want) in cases {
            match (parse_active_window(input), want) {
                (Ok(got), Some(want)) => assert_eq!(got, want, "{name}"),
                (Err(HyprError::Json(_)), None) => {}
                (got, want) => panic!("{name}: got {got:?}, want {want:?}"),
            }
        }
    }

    const DP3: &str = r#"[{"id": 2, "name": "DP-3", "description": "synthetic", "make": "x",
        "width": 3840, "height": 2160, "refreshRate": 60.0, "x": 0, "y": 0,
        "activeWorkspace": {"id": 4, "name": "Games"}, "reserved": [0, 34, 0, 0],
        "scale": 1.5, "transform": 0, "focused": true}]"#;

    fn monitor(width: u32, height: u32, scale: f64, transform: u32, reserved: [u32; 4]) -> Monitor {
        Monitor {
            name: "DP-3".to_string(),
            x: 0,
            y: 0,
            focused: true,
            width,
            height,
            scale,
            transform,
            reserved,
        }
    }

    #[test]
    fn parse_monitors_cases() {
        let cases: Vec<(&str, &str, Option<Vec<Monitor>>)> = vec![
            (
                "dp-3",
                DP3,
                Some(vec![monitor(3840, 2160, 1.5, 0, [0, 34, 0, 0])]),
            ),
            (
                "position and focus",
                r#"[{"name": "DP-3", "width": 1920, "height": 1080, "x": 2560, "y": -40,
                    "reserved": [0, 0, 0, 0], "scale": 1.0, "transform": 0, "focused": false}]"#,
                Some(vec![Monitor {
                    x: 2560,
                    y: -40,
                    focused: false,
                    ..monitor(1920, 1080, 1.0, 0, [0, 0, 0, 0])
                }]),
            ),
            ("empty array", "[]", Some(vec![])),
            ("object", "{}", None),
            ("not json", "not json", None),
        ];
        for (name, input, want) in cases {
            match (parse_monitors(input), want) {
                (Ok(got), Some(want)) => assert_eq!(got, want, "{name}"),
                (Err(HyprError::Json(_)), None) => {}
                (got, want) => panic!("{name}: got {got:?}, want {want:?}"),
            }
        }
    }

    #[test]
    fn logical_size_cases() {
        let size = |width, height| Size { width, height };
        let cases = [
            (
                "scale 1",
                monitor(1920, 1080, 1.0, 0, [0; 4]),
                size(1920, 1080),
            ),
            (
                "scale 1.5",
                monitor(3840, 2160, 1.5, 0, [0; 4]),
                size(2560, 1440),
            ),
            (
                "rounds",
                monitor(1000, 1000, 1.5, 0, [0; 4]),
                size(667, 667),
            ),
            (
                "transform 1 swaps",
                monitor(2160, 3840, 1.5, 1, [0; 4]),
                size(2560, 1440),
            ),
            (
                "transform 2 keeps",
                monitor(3840, 2160, 1.5, 2, [0; 4]),
                size(2560, 1440),
            ),
            (
                "transform 3 swaps",
                monitor(3840, 2160, 1.5, 3, [0; 4]),
                size(1440, 2560),
            ),
        ];
        for (name, m, want) in cases {
            assert_eq!(m.logical_size(), want, "{name}");
        }
    }

    #[test]
    fn monitor_at_cases() {
        let at = |x, y, mut m: Monitor| {
            m.x = x;
            m.y = y;
            m
        };
        let left = at(0, 0, monitor(1920, 1080, 1.0, 0, [0; 4]));
        let right = at(1920, 0, monitor(3840, 2160, 1.5, 0, [0; 4]));
        let gap = at(5000, 0, monitor(1920, 1080, 1.0, 0, [0; 4]));
        let pair = vec![left.clone(), right];
        let split = [left, gap];
        type Case<'a> = (&'a str, &'a [Monitor], (f64, f64), Option<usize>);
        let cases: Vec<Case> = vec![
            ("inside the left", &pair, (100.0, 100.0), Some(0)),
            ("inside the right", &pair, (3000.0, 1000.0), Some(1)),
            (
                "shared edge belongs to the right",
                &pair,
                (1920.0, 500.0),
                Some(1),
            ),
            ("last column of the left", &pair, (1919.5, 500.0), Some(0)),
            ("right monitor's far edge", &pair, (4480.0, 500.0), None),
            ("below the left", &pair, (100.0, 1080.0), None),
            ("below both", &pair, (2000.0, 1500.0), None),
            ("above", &pair, (100.0, -1.0), None),
            ("gap", &split, (3000.0, 100.0), None),
            ("empty list", &[], (0.0, 0.0), None),
        ];
        for (name, monitors, (x, y), want) in cases {
            assert_eq!(monitor_at(monitors, x, y), want, "{name}");
        }
    }

    #[test]
    fn usable_area_cases() {
        let rect = |x, y, width, height| Rect {
            x,
            y,
            width,
            height,
        };
        let cases = [
            (
                "dp-3",
                monitor(3840, 2160, 1.5, 0, [0, 34, 0, 0]),
                rect(0, 34, 2560, 1406),
            ),
            (
                "odd transform",
                monitor(2160, 3840, 1.5, 1, [0, 34, 0, 0]),
                rect(0, 34, 2560, 1406),
            ),
            (
                "transform 2",
                monitor(3840, 2160, 1.5, 2, [0, 34, 0, 0]),
                rect(0, 34, 2560, 1406),
            ),
            (
                "scale 1",
                monitor(1920, 1080, 1.0, 0, [10, 20, 30, 40]),
                rect(10, 20, 1880, 1020),
            ),
            (
                "rounds",
                monitor(1000, 1000, 1.5, 0, [0, 0, 0, 0]),
                rect(0, 0, 667, 667),
            ),
            (
                "reserved exceeds",
                monitor(100, 100, 1.0, 0, [60, 60, 60, 60]),
                rect(60, 60, 0, 0),
            ),
        ];
        for (name, m, want) in cases {
            assert_eq!(m.usable_area(), want, "{name}");
        }
    }
}
