use std::fmt;
use std::process::{Command, ExitStatus};
use std::str::Utf8Error;

use serde::Deserialize;
use serde::de::Error as _;

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub name: String,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Client {
    #[serde(deserialize_with = "deserialize_address")]
    pub address: u64,
    pub class: String,
    pub title: String,
    pub workspace: Workspace,
    pub mapped: bool,
}

fn deserialize_address<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let s = String::deserialize(d)?;
    parse_address(&s).map_err(D::Error::custom)
}

#[derive(Debug)]
pub enum HyprError {
    Spawn(std::io::Error),
    Status { status: ExitStatus, output: String },
    Utf8(Utf8Error),
    Json(serde_json::Error),
    InvalidAddress(String),
    NotFound(u64),
    NoCandidate,
}

impl fmt::Display for HyprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HyprError::Spawn(e) => write!(f, "cannot run hyprctl clients -j: {e}"),
            HyprError::Status { status, output } if output.is_empty() => {
                write!(f, "hyprctl clients -j failed: {status}")
            }
            HyprError::Status { status, output } => {
                write!(f, "hyprctl clients -j failed: {status}: {output}")
            }
            HyprError::Utf8(e) => write!(f, "hyprctl clients -j output is not UTF-8: {e}"),
            HyprError::Json(e) => write!(f, "hyprctl clients -j output is not valid: {e}"),
            HyprError::InvalidAddress(s) => write!(f, "invalid address {s:?}"),
            HyprError::NotFound(a) => write!(f, "no mapped client with address 0x{a:x}"),
            HyprError::NoCandidate => write!(f, "no mapped EVE game client found"),
        }
    }
}

impl std::error::Error for HyprError {}

/// Parses `0x` or `0X` followed by 1 to 16 hex digits.
pub fn parse_address(s: &str) -> Result<u64, HyprError> {
    let invalid = || HyprError::InvalidAddress(s.to_string());
    let digits = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .ok_or_else(invalid)?;
    if digits.is_empty() || digits.len() > 16 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    u64::from_str_radix(digits, 16).map_err(|_| invalid())
}

/// Parses the output of `hyprctl clients -j`. Unknown fields are ignored.
pub fn parse_clients(json: &str) -> Result<Vec<Client>, HyprError> {
    serde_json::from_str(json).map_err(HyprError::Json)
}

/// Runs `hyprctl clients -j` with the inherited environment. Its stderr is captured, not shown.
/// On failure the trimmed stdout is kept, because hyprctl prints its errors there.
pub fn query_clients() -> Result<Vec<Client>, HyprError> {
    let out = Command::new("hyprctl")
        .args(["clients", "-j"])
        .output()
        .map_err(HyprError::Spawn)?;
    if !out.status.success() {
        return Err(HyprError::Status {
            status: out.status,
            output: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        });
    }
    let text = std::str::from_utf8(&out.stdout).map_err(HyprError::Utf8)?;
    parse_clients(text)
}

pub fn is_game_title(title: &str) -> bool {
    title == "EVE" || title.starts_with("EVE - ")
}

/// The slot of an `EVE<n>` workspace name, n in 1 to 12 in decimal with no leading zero.
pub fn eve_slot(workspace_name: &str) -> Option<u8> {
    let digits = workspace_name.strip_prefix("EVE")?;
    if digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u8>().ok().filter(|n| (1..=12).contains(n))
}

/// Mapped `steam_app_8500` clients with an EVE title on an `EVE<n>` workspace, by (slot, address).
pub fn game_clients(clients: &[Client]) -> Vec<(u8, &Client)> {
    let mut found: Vec<(u8, &Client)> = clients
        .iter()
        .filter(|c| c.mapped && c.class == "steam_app_8500" && is_game_title(&c.title))
        .filter_map(|c| eve_slot(&c.workspace.name).map(|n| (n, c)))
        .collect();
    found.sort_by_key(|(n, c)| (*n, c.address));
    found
}

/// With an address, the mapped client at it. Without one, the first game client.
pub fn select(clients: &[Client], address: Option<u64>) -> Result<&Client, HyprError> {
    match address {
        Some(a) => clients
            .iter()
            .find(|c| c.address == a && c.mapped)
            .ok_or(HyprError::NotFound(a)),
        None => game_clients(clients)
            .first()
            .map(|(_, c)| *c)
            .ok_or(HyprError::NoCandidate),
    }
}

/// The low 32 bits of the address, which Hyprland matches against its window pointers.
pub fn capture_handle(address: u64) -> u32 {
    (address & 0xFFFF_FFFF) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_address_cases() {
        let cases: Vec<(&str, &str, Option<u64>)> = vec![
            ("lower", "0x5608ab929d00", Some(0x5608ab929d00)),
            (
                "upper prefix and digits",
                "0X5608AB929D00",
                Some(0x5608ab929d00),
            ),
            ("one digit", "0x1", Some(1)),
            ("sixteen digits", "0xffffffffffffffff", Some(u64::MAX)),
            ("empty", "", None),
            ("prefix only", "0x", None),
            ("no prefix", "5608ab929d00", None),
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
    fn status_display_cases() {
        use std::os::unix::process::ExitStatusExt;
        let cases = [
            ("no output", "", "hyprctl clients -j failed: exit status: 1"),
            (
                "output",
                "Couldn't connect",
                "hyprctl clients -j failed: exit status: 1: Couldn't connect",
            ),
        ];
        for (name, output, want) in cases {
            let err = HyprError::Status {
                status: ExitStatus::from_raw(1 << 8),
                output: output.to_string(),
            };
            assert_eq!(err.to_string(), want, "{name}");
        }
    }

    const FIXTURE: &str = include_str!("../testdata/hyprctl-clients.json");

    const EVE2: u64 = 0x5608ab929d00;
    const EVE3: u64 = 0x5608ab8fee80;
    const LAUNCHER: u64 = 0x5608ab8d7830;
    const HELPER: u64 = 0x5608ab796170;

    fn fixture() -> Vec<Client> {
        parse_clients(FIXTURE).unwrap()
    }

    fn addresses(clients: &[(u8, &Client)]) -> Vec<(u8, u64)> {
        clients.iter().map(|(n, c)| (*n, c.address)).collect()
    }

    #[test]
    fn capture_handle_cases() {
        let cases = [
            ("eve2", 0x5608ab929d00, 2878512384),
            ("doc example", 0xd161e7b0, 3512854448),
            ("eve3", 0x5608ab8fee80, 2878336640),
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
    fn parse_clients_cases() {
        let cases: [(&str, &str, Option<Vec<u64>>); 4] = [
            ("fixture", FIXTURE, Some(vec![EVE2, EVE3, LAUNCHER, HELPER])),
            ("not json", "not json", None),
            ("entry without address", "[{}]", None),
            ("not an array", "{}", None),
        ];
        for (name, input, want) in cases {
            match (parse_clients(input), want) {
                (Ok(clients), Some(want)) => {
                    let got: Vec<u64> = clients.iter().map(|c| c.address).collect();
                    assert_eq!(got, want, "{name}");
                }
                (Err(HyprError::Json(_)), None) => {}
                (got, want) => panic!("{name}: got {got:?}, want {want:?}"),
            }
        }
    }

    #[test]
    fn game_clients_fixture() {
        let mut unmapped = fixture();
        unmapped[1].mapped = false;
        let mut same_slot = fixture();
        same_slot[0].workspace.name = "EVE2".into();
        same_slot[1].workspace.name = "EVE2".into();
        let cases = [
            ("fixture", fixture(), vec![(2, EVE2), (3, EVE3)]),
            ("unmapped candidate dropped", unmapped, vec![(2, EVE2)]),
            (
                "same slot orders by address",
                same_slot,
                vec![(2, EVE3), (2, EVE2)],
            ),
        ];
        for (name, clients, want) in cases {
            assert_eq!(addresses(&game_clients(&clients)), want, "{name}");
        }
    }

    #[derive(Debug)]
    enum Want {
        Found(u64),
        NotFound,
        NoCandidate,
    }

    #[test]
    fn select_cases() {
        let mut eve2_unmapped = fixture();
        eve2_unmapped[0].mapped = false;
        let launcher_only = fixture().split_off(2);
        let cases = [
            ("first candidate", None, fixture(), Want::Found(EVE2)),
            (
                "explicit game client",
                Some(EVE3),
                fixture(),
                Want::Found(EVE3),
            ),
            (
                "explicit launcher skips the title rule",
                Some(LAUNCHER),
                fixture(),
                Want::Found(LAUNCHER),
            ),
            ("absent address", Some(0x1), fixture(), Want::NotFound),
            (
                "unmapped address",
                Some(EVE2),
                eve2_unmapped,
                Want::NotFound,
            ),
            ("no candidates", None, launcher_only, Want::NoCandidate),
        ];
        for (name, address, clients, want) in cases {
            match (select(&clients, address), want) {
                (Ok(c), Want::Found(a)) => assert_eq!(c.address, a, "{name}"),
                (Err(HyprError::NotFound(a)), Want::NotFound) => {
                    assert_eq!(Some(a), address, "{name}")
                }
                (Err(HyprError::NoCandidate), Want::NoCandidate) => {}
                (got, want) => panic!("{name}: got {got:?}, want {want:?}"),
            }
        }
    }
}
