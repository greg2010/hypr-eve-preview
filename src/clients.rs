use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;

use crate::{hypr, ipc};

const LAUNCHER_ARG: &[u8] = b"/LauncherData=";
const LAUNCHER_PREFIX: &[u8] = b"eve-online:tranquility::";
const REMOVED_MISSED: &str = "not listed by j/clients";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountKey {
    User(u64),
    Character(String),
}

impl fmt::Display for AccountKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccountKey::User(id) => write!(f, "user:{id}"),
            AccountKey::Character(name) => write!(f, "character:{name}"),
        }
    }
}

/// Why no user id could be read. The texts name no part of the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountError {
    Unreadable(io::ErrorKind),
    Missing,
    Base64,
    Text,
}

impl fmt::Display for AccountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccountError::Unreadable(kind) => {
                write!(f, "cannot read the process command line: {kind}")
            }
            AccountError::Missing => f.write_str("no /LauncherData argument"),
            AccountError::Base64 => f.write_str("/LauncherData is not valid base64"),
            AccountError::Text => {
                f.write_str("/LauncherData does not decode to eve-online:tranquility::<id>:")
            }
        }
    }
}

impl std::error::Error for AccountError {}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// RFC 4648 section 4 with required padding.
fn decode_base64(text: &[u8]) -> Result<Vec<u8>, AccountError> {
    if text.is_empty() || !text.len().is_multiple_of(4) {
        return Err(AccountError::Base64);
    }
    let groups = text.len() / 4;
    let mut out = Vec::with_capacity(groups * 3);
    for (i, group) in text.chunks_exact(4).enumerate() {
        let padding = if i + 1 == groups {
            group.iter().rev().take_while(|&&b| b == b'=').count()
        } else {
            0
        };
        if padding > 2 {
            return Err(AccountError::Base64);
        }
        let mut bits = 0u32;
        for &byte in &group[..4 - padding] {
            let value = base64_value(byte).ok_or(AccountError::Base64)?;
            bits = bits << 6 | u32::from(value);
        }
        bits <<= 6 * padding;
        let bytes = bits.to_be_bytes();
        out.extend_from_slice(&bytes[1..4 - padding]);
    }
    Ok(out)
}

/// Decodes the user id of the first `/LauncherData=` argument of a NUL-separated command line.
pub fn user_id_from_cmdline(cmdline: &[u8]) -> Result<u64, AccountError> {
    let value = cmdline
        .split(|&b| b == 0)
        .find_map(|arg| arg.strip_prefix(LAUNCHER_ARG))
        .ok_or(AccountError::Missing)?;
    let decoded = decode_base64(value)?;
    let digits = decoded
        .strip_prefix(LAUNCHER_PREFIX)
        .and_then(|rest| rest.strip_suffix(b":"))
        .filter(|d| (1..=20).contains(&d.len()) && d.iter().all(u8::is_ascii_digit))
        .ok_or(AccountError::Text)?;
    std::str::from_utf8(digits)
        .ok()
        .and_then(|d| d.parse::<u64>().ok())
        .ok_or(AccountError::Text)
}

/// Reads `/proc/<pid>/cmdline` and nothing else under `/proc/<pid>`. A pid of 0 or below is
/// `Unreadable(NotFound)` without touching the filesystem.
pub fn read_user_id(pid: i32) -> Result<u64, AccountError> {
    if pid <= 0 {
        return Err(AccountError::Unreadable(io::ErrorKind::NotFound));
    }
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))
        .map_err(|e| AccountError::Unreadable(e.kind()))?;
    user_id_from_cmdline(&cmdline)
}

pub fn character_name(title: &str) -> Option<&str> {
    title
        .strip_prefix(hypr::GAME_TITLE_PREFIX)
        .filter(|name| !name.is_empty())
}

/// The character name; without one, the workspace name when it gives a slot, else `EVE`.
pub fn label(title: &str, workspace: &str) -> String {
    match character_name(title) {
        Some(name) => name.to_string(),
        None if hypr::eve_slot(workspace).is_some() => workspace.to_string(),
        None => "EVE".to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracked {
    pub address: u64,
    pub pid: i32,
    pub title: String,
    pub workspace: String,
    pub user_id: Option<u64>,
    pub character: Option<String>,
}

impl Tracked {
    pub fn slot(&self) -> Option<u8> {
        hypr::eve_slot(&self.workspace)
    }

    pub fn label(&self) -> String {
        label(&self.title, &self.workspace)
    }

    pub fn key(&self) -> Option<AccountKey> {
        match (self.user_id, &self.character) {
            (Some(id), _) => Some(AccountKey::User(id)),
            (None, Some(name)) => Some(AccountKey::Character(name.clone())),
            (None, None) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Lookup {
        address: u64,
    },
    Retry {
        address: u64,
    },
    Added {
        address: u64,
    },
    Removed {
        address: u64,
        reason: String,
    },
    Title {
        address: u64,
        label_changed: bool,
        key_changed: bool,
    },
    Workspace {
        address: u64,
        label_changed: bool,
    },
    RingOwner {
        address: Option<u64>,
        previous: Option<u64>,
    },
}

#[derive(Debug)]
pub struct Clients {
    tracked: BTreeMap<u64, Tracked>,
    // The value is true once the 100 ms retry has been used.
    pending: BTreeMap<u64, bool>,
    thumbnails: BTreeSet<u64>,
    active: Option<u64>,
    ring: Option<u64>,
}

impl Clients {
    pub fn new(active: Option<u64>) -> Clients {
        Clients {
            tracked: BTreeMap::new(),
            pending: BTreeMap::new(),
            thumbnails: BTreeSet::new(),
            active,
            ring: None,
        }
    }

    /// Tracks a game-client entry. Any other entry changes nothing but drops its pending lookup.
    pub fn add(&mut self, entry: &hypr::Client, user_id: Option<u64>) -> Vec<Change> {
        self.pending.remove(&entry.address);
        if !hypr::is_game_client(&entry.class, &entry.title)
            || self.tracked.contains_key(&entry.address)
        {
            return Vec::new();
        }
        self.tracked.insert(
            entry.address,
            Tracked {
                address: entry.address,
                pid: entry.pid,
                title: entry.title.clone(),
                workspace: entry.workspace.name.clone(),
                user_id,
                character: character_name(&entry.title).map(str::to_string),
            },
        );
        vec![Change::Added {
            address: entry.address,
        }]
    }

    /// First miss asks for the retry, the second removes the pending address.
    pub fn lookup_missed(&mut self, address: u64) -> Vec<Change> {
        match self.pending.get_mut(&address) {
            Some(retried) if !*retried => {
                *retried = true;
                vec![Change::Retry { address }]
            }
            Some(_) => {
                self.pending.remove(&address);
                vec![Change::Removed {
                    address,
                    reason: REMOVED_MISSED.to_string(),
                }]
            }
            None => Vec::new(),
        }
    }

    pub fn apply(&mut self, event: &ipc::Event) -> Vec<Change> {
        match event {
            ipc::Event::OpenWindow {
                address,
                class,
                title,
                ..
            } => {
                if self.tracked.contains_key(address)
                    || self.pending.contains_key(address)
                    || !hypr::is_game_client(class, title)
                {
                    return Vec::new();
                }
                self.pending.insert(*address, false);
                vec![Change::Lookup { address: *address }]
            }
            ipc::Event::CloseWindow { address } => self.remove(*address, "closed".to_string()),
            ipc::Event::WindowTitle { address, title } => self.retitle(*address, title),
            ipc::Event::MoveWindow { address, workspace } => {
                let Some(tracked) = self.tracked.get_mut(address) else {
                    return Vec::new();
                };
                let before = tracked.label();
                tracked.workspace.clone_from(workspace);
                vec![Change::Workspace {
                    address: *address,
                    label_changed: tracked.label() != before,
                }]
            }
            ipc::Event::ActiveWindow { address } => {
                self.active = *address;
                let mut changes = Vec::new();
                self.refresh_ring(&mut changes);
                changes
            }
        }
    }

    fn retitle(&mut self, address: u64, title: &str) -> Vec<Change> {
        let Some(tracked) = self.tracked.get_mut(&address) else {
            return Vec::new();
        };
        if !hypr::is_game_title(title) {
            let quoted = serde_json::Value::String(title.to_string());
            return self.remove(address, format!("title {quoted}"));
        }
        let (label_before, key_before) = (tracked.label(), tracked.key());
        tracked.title = title.to_string();
        if let Some(name) = character_name(title) {
            tracked.character = Some(name.to_string());
        }
        vec![Change::Title {
            address,
            label_changed: tracked.label() != label_before,
            key_changed: tracked.key() != key_before,
        }]
    }

    /// Removes a tracked or pending address; any other address changes nothing.
    pub fn remove(&mut self, address: u64, reason: String) -> Vec<Change> {
        let was_pending = self.pending.remove(&address).is_some();
        if self.tracked.remove(&address).is_none() {
            return if was_pending {
                vec![Change::Removed { address, reason }]
            } else {
                Vec::new()
            };
        }
        self.thumbnails.remove(&address);
        let mut changes = vec![Change::Removed { address, reason }];
        self.refresh_ring(&mut changes);
        changes
    }

    /// Records that the client's thumbnail exists, which can make it the ring owner.
    pub fn thumbnail_created(&mut self, address: u64) -> Vec<Change> {
        let mut changes = Vec::new();
        if self.tracked.contains_key(&address) {
            self.thumbnails.insert(address);
            self.refresh_ring(&mut changes);
        }
        changes
    }

    fn refresh_ring(&mut self, changes: &mut Vec<Change>) {
        let owner = self.active.filter(|a| self.thumbnails.contains(a));
        if owner != self.ring {
            let previous = std::mem::replace(&mut self.ring, owner);
            changes.push(Change::RingOwner {
                address: owner,
                previous,
            });
        }
    }

    pub fn get(&self, address: u64) -> Option<&Tracked> {
        self.tracked.get(&address)
    }

    /// Whether a lookup for the address is outstanding.
    pub fn is_pending(&self, address: u64) -> bool {
        self.pending.contains_key(&address)
    }

    pub fn ring_owner(&self) -> Option<u64> {
        self.ring
    }

    /// Tracked addresses by slot ascending, clients without a slot last, then by address.
    pub fn ordered(&self) -> Vec<u64> {
        let mut all: Vec<&Tracked> = self.tracked.values().collect();
        all.sort_by_key(|t| (t.slot().is_none(), t.slot(), t.address));
        all.into_iter().map(|t| t.address).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../testdata/hyprctl-clients.json");

    const A: u64 = 0x555512345678;
    const B: u64 = 0x55559abcdef0;
    const C: u64 = 0x555500003000;
    const D: u64 = 0x555500000010;
    const E: u64 = 0xffff00000000;
    const X: u64 = 0x555500009999;

    const GAME: &str = "steam_app_8500";

    fn entry(address: u64, class: &str, workspace: &str, title: &str) -> hypr::Client {
        hypr::Client {
            address,
            class: class.to_string(),
            title: title.to_string(),
            workspace: hypr::Workspace {
                name: workspace.to_string(),
            },
            pid: 1000,
        }
    }

    fn game(address: u64, workspace: &str, title: &str) -> hypr::Client {
        entry(address, GAME, workspace, title)
    }

    fn open(address: u64, class: &str, workspace: &str, title: &str) -> ipc::Event {
        ipc::Event::OpenWindow {
            address,
            workspace: workspace.to_string(),
            class: class.to_string(),
            title: title.to_string(),
        }
    }

    fn title_event(address: u64, title: &str) -> ipc::Event {
        ipc::Event::WindowTitle {
            address,
            title: title.to_string(),
        }
    }

    fn move_event(address: u64, workspace: &str) -> ipc::Event {
        ipc::Event::MoveWindow {
            address,
            workspace: workspace.to_string(),
        }
    }

    fn active(address: Option<u64>) -> ipc::Event {
        ipc::Event::ActiveWindow { address }
    }

    enum Op {
        Apply(ipc::Event),
        Add(hypr::Client, Option<u64>),
        Missed(u64),
        Thumbnail(u64),
        Remove(u64, &'static str),
    }

    type Script = Vec<(Op, Vec<Change>)>;

    fn base(active: Option<u64>) -> Clients {
        let mut clients = Clients::new(active);
        clients.add(&game(A, "EVE2", "EVE - Pilot One"), Some(1000001));
        clients.add(&game(B, "EVE3", "EVE"), None);
        clients
    }

    fn play(name: &str, clients: &mut Clients, script: Script) {
        for (i, (op, want)) in script.into_iter().enumerate() {
            let got = match op {
                Op::Apply(event) => clients.apply(&event),
                Op::Add(client, user_id) => clients.add(&client, user_id),
                Op::Missed(address) => clients.lookup_missed(address),
                Op::Thumbnail(address) => clients.thumbnail_created(address),
                Op::Remove(address, reason) => clients.remove(address, reason.to_string()),
            };
            assert_eq!(got, want, "{name} step {i}");
        }
    }

    fn run(name: &str, active: Option<u64>, script: Script, want_ordered: Vec<u64>) {
        let mut clients = base(active);
        play(name, &mut clients, script);
        assert_eq!(clients.ordered(), want_ordered, "{name} ordered");
    }

    fn removed(address: u64, reason: &str) -> Change {
        Change::Removed {
            address,
            reason: reason.to_string(),
        }
    }

    fn owner(address: Option<u64>, previous: Option<u64>) -> Change {
        Change::RingOwner { address, previous }
    }

    #[test]
    fn snapshot_fixture() {
        let cases = [(
            "fixture game clients",
            FIXTURE,
            vec![Change::Added { address: A }, Change::Added { address: B }],
            vec![A, B],
            Some(Tracked {
                address: A,
                pid: 1001,
                title: "EVE - Pilot One".to_string(),
                workspace: "EVE2".to_string(),
                user_id: None,
                character: Some("Pilot One".to_string()),
            }),
        )];
        for (name, json, want_changes, want_ordered, want_a) in cases {
            let all = hypr::parse_clients(json).unwrap();
            let mut clients = Clients::new(None);
            let mut changes = Vec::new();
            for client in all
                .iter()
                .filter(|c| hypr::is_game_client(&c.class, &c.title))
            {
                changes.extend(clients.add(client, None));
            }
            assert_eq!(changes, want_changes, "{name} changes");
            assert_eq!(clients.ordered(), want_ordered, "{name} ordered");
            assert_eq!(clients.get(A), want_a.as_ref(), "{name} A");
        }
    }

    fn cmdline(launcher: &str) -> Vec<u8> {
        format!("C:\\x\\exefile.exe\0/ssoToken=synthetic.token.value\0{launcher}\0/language=en\0")
            .into_bytes()
    }

    #[test]
    fn user_id_cases() {
        let cases: Vec<(&str, Vec<u8>, Result<u64, AccountError>)> = vec![
            (
                "tranquility",
                cmdline("/LauncherData=ZXZlLW9ubGluZTp0cmFucXVpbGl0eTo6MTAwMDAwMTo="),
                Ok(1000001),
            ),
            (
                "no padding",
                cmdline("/LauncherData=ZXZlLW9ubGluZTp0cmFucXVpbGl0eTo6MTAwMDAwMDE6"),
                Ok(10000001),
            ),
            (
                "two padding characters",
                cmdline("/LauncherData=ZXZlLW9ubGluZTp0cmFucXVpbGl0eTo6MTAwMDAwMDAxOg=="),
                Ok(100000001),
            ),
            (
                "u64 maximum",
                cmdline(
                    "/LauncherData=ZXZlLW9ubGluZTp0cmFucXVpbGl0eTo6MTg0NDY3NDQwNzM3MDk1NTE2MTU6",
                ),
                Ok(u64::MAX),
            ),
            (
                "first argument wins",
                b"/LauncherData=ZXZlLW9ubGluZTp0cmFucXVpbGl0eTo6MTAwMDAwMTo=\0/LauncherData=@@@@\0"
                    .to_vec(),
                Ok(1000001),
            ),
            ("empty", Vec::new(), Err(AccountError::Missing)),
            (
                "no launcher argument",
                b"C:\\x\\exefile.exe\0/language=en\0".to_vec(),
                Err(AccountError::Missing),
            ),
            (
                "prefix inside another argument",
                b"x/LauncherData=ZXZlLW9ubGluZTp0cmFucXVpbGl0eTo6MTAwMDAwMTo=\0".to_vec(),
                Err(AccountError::Missing),
            ),
            (
                "bad alphabet",
                cmdline("/LauncherData=@@@@"),
                Err(AccountError::Base64),
            ),
            (
                "empty value",
                cmdline("/LauncherData="),
                Err(AccountError::Base64),
            ),
            (
                "missing padding",
                cmdline("/LauncherData=ZXZlLW9ubGluZTp0cmFucXVpbGl0eTo6MTAwMDAwMTo"),
                Err(AccountError::Base64),
            ),
            (
                "padding in the middle",
                cmdline("/LauncherData=ZXZl=W9ubGluZTp0cmFucXVpbGl0eTo6MTAwMDAwMTo="),
                Err(AccountError::Base64),
            ),
            (
                "padding at the start of the last group",
                cmdline("/LauncherData=ZXZlLW9ubGluZTp0cmFucXVpbGl0eTo6MTAwMDAw=A=="),
                Err(AccountError::Base64),
            ),
            (
                "three padding characters",
                cmdline("/LauncherData=ZXZlLW9ubGluZTp0cmFucXVpbGl0eTo6MTAwMDAwM==="),
                Err(AccountError::Base64),
            ),
            (
                "wrong prefix",
                cmdline("/LauncherData=ZXZlLW9ubGluZTpzaW5ndWxhcml0eTo6MTAwMDAwMTo="),
                Err(AccountError::Text),
            ),
            (
                "twenty-one digits",
                cmdline(
                    "/LauncherData=ZXZlLW9ubGluZTp0cmFucXVpbGl0eTo6MTAwMDAwMDAwMDAwMDAwMDAwMDAwOg==",
                ),
                Err(AccountError::Text),
            ),
            (
                "twenty digits above u64",
                cmdline(
                    "/LauncherData=ZXZlLW9ubGluZTp0cmFucXVpbGl0eTo6OTk5OTk5OTk5OTk5OTk5OTk5OTk6",
                ),
                Err(AccountError::Text),
            ),
            (
                "no digits",
                cmdline("/LauncherData=ZXZlLW9ubGluZTp0cmFucXVpbGl0eTo6Og=="),
                Err(AccountError::Text),
            ),
            (
                "non-digit id",
                cmdline("/LauncherData=ZXZlLW9ubGluZTp0cmFucXVpbGl0eTo6MTBhOg=="),
                Err(AccountError::Text),
            ),
            (
                "no closing colon",
                cmdline("/LauncherData=ZXZlLW9ubGluZTp0cmFucXVpbGl0eTo6MTAwMDAwMQ=="),
                Err(AccountError::Text),
            ),
            (
                "trailing text",
                cmdline("/LauncherData=ZXZlLW9ubGluZTp0cmFucXVpbGl0eTo6MTAwMDAwMTp4"),
                Err(AccountError::Text),
            ),
        ];
        for (name, input, want) in cases {
            assert_eq!(user_id_from_cmdline(&input), want, "{name}");
        }
    }

    #[test]
    fn read_user_id_cases() {
        let cases = [
            (
                "negative pid",
                -1,
                Err(AccountError::Unreadable(io::ErrorKind::NotFound)),
            ),
            (
                "zero pid",
                0,
                Err(AccountError::Unreadable(io::ErrorKind::NotFound)),
            ),
        ];
        for (name, pid, want) in cases {
            assert_eq!(read_user_id(pid), want, "{name}");
        }
    }

    #[test]
    fn account_error_display_cases() {
        let cases = [
            (
                "unreadable",
                AccountError::Unreadable(io::ErrorKind::NotFound),
                "cannot read the process command line: entity not found",
            ),
            (
                "missing",
                AccountError::Missing,
                "no /LauncherData argument",
            ),
            (
                "base64",
                AccountError::Base64,
                "/LauncherData is not valid base64",
            ),
            (
                "text",
                AccountError::Text,
                "/LauncherData does not decode to eve-online:tranquility::<id>:",
            ),
        ];
        for (name, error, want) in cases {
            assert_eq!(error.to_string(), want, "{name}");
        }
    }

    #[test]
    fn key_cases() {
        type Row<'a> = (
            &'a str,
            Option<u64>,
            Vec<&'a str>,
            Option<AccountKey>,
            &'a str,
        );
        let cases: Vec<Row> = vec![
            (
                "user id",
                Some(1000001),
                vec!["EVE - Pilot One"],
                Some(AccountKey::User(1000001)),
                "user:1000001",
            ),
            (
                "user id wins over a name",
                Some(1000001),
                vec!["EVE", "EVE - Pilot One"],
                Some(AccountKey::User(1000001)),
                "user:1000001",
            ),
            (
                "character",
                None,
                vec!["EVE - Pilot One"],
                Some(AccountKey::Character("Pilot One".to_string())),
                "character:Pilot One",
            ),
            (
                "character kept after a bare title",
                None,
                vec!["EVE - Pilot One", "EVE"],
                Some(AccountKey::Character("Pilot One".to_string())),
                "character:Pilot One",
            ),
            (
                "character follows the latest name",
                None,
                vec!["EVE - Pilot One", "EVE - Pilot Two"],
                Some(AccountKey::Character("Pilot Two".to_string())),
                "character:Pilot Two",
            ),
            ("bare title only", None, vec!["EVE"], None, ""),
        ];
        for (name, user_id, titles, want, text) in cases {
            let mut clients = Clients::new(None);
            clients.add(&game(B, "EVE3", titles[0]), user_id);
            for title in &titles[1..] {
                clients.apply(&title_event(B, title));
            }
            let got = clients.get(B).and_then(Tracked::key);
            assert_eq!(got, want, "{name}");
            assert_eq!(
                got.map(|k| k.to_string()).unwrap_or_default(),
                text,
                "{name}"
            );
        }
    }

    #[test]
    fn character_name_cases() {
        let cases = [
            ("EVE - Pilot One", Some("Pilot One")),
            ("EVE - Pilot, One", Some("Pilot, One")),
            ("EVE - ", None),
            ("EVE", None),
            ("EVE Launcher", None),
            ("", None),
        ];
        for (title, want) in cases {
            assert_eq!(character_name(title), want, "{title:?}");
        }
    }

    #[test]
    fn label_cases() {
        let cases = [
            ("EVE - Pilot One", "EVE2", "Pilot One"),
            ("EVE", "EVE3", "EVE3"),
            ("EVE", "EVE-new", "EVE"),
            ("EVE - ", "EVE2", "EVE2"),
            ("EVE - ", "Games", "EVE"),
            ("EVE - Pilot, One", "Games", "Pilot, One"),
            ("EVE - Pilot One", "EVE-new", "Pilot One"),
        ];
        for (title, workspace, want) in cases {
            assert_eq!(label(title, workspace), want, "{title:?} {workspace:?}");
        }
    }

    #[test]
    fn tracked_cases() {
        let tracked = |title: &str, workspace: &str| Tracked {
            address: A,
            pid: 1001,
            title: title.to_string(),
            workspace: workspace.to_string(),
            user_id: None,
            character: None,
        };
        let cases = [
            (tracked("EVE - Pilot One", "EVE2"), Some(2), "Pilot One"),
            (tracked("EVE", "EVE12"), Some(12), "EVE12"),
            (tracked("EVE", "EVE-new"), None, "EVE"),
            (tracked("EVE", "Games"), None, "EVE"),
        ];
        for (t, slot, want_label) in cases {
            assert_eq!(t.slot(), slot, "{t:?}");
            assert_eq!(t.label(), want_label, "{t:?}");
        }
    }

    #[test]
    fn apply_cases() {
        use Op::*;
        let cases: Vec<(&str, Script, Vec<u64>)> = vec![
            (
                "openwindow of a new game client",
                vec![(
                    Apply(open(C, GAME, "EVE-new", "EVE")),
                    vec![Change::Lookup { address: C }],
                )],
                vec![A, B],
            ),
            (
                "openwindow of the launcher title",
                vec![(Apply(open(C, GAME, "EVE-new", "EVE Launcher")), vec![])],
                vec![A, B],
            ),
            (
                "openwindow of another class",
                vec![(Apply(open(C, "kitty", "1", "EVE")), vec![])],
                vec![A, B],
            ),
            (
                "openwindow of a tracked address",
                vec![(Apply(open(A, GAME, "EVE2", "EVE")), vec![])],
                vec![A, B],
            ),
            (
                "closewindow",
                vec![(
                    Apply(ipc::Event::CloseWindow { address: A }),
                    vec![removed(A, "closed")],
                )],
                vec![B],
            ),
            (
                "login title",
                vec![(
                    Apply(title_event(B, "EVE - Pilot Two")),
                    vec![Change::Title {
                        address: B,
                        label_changed: true,
                        key_changed: true,
                    }],
                )],
                vec![A, B],
            ),
            (
                "name change of a client with a user id",
                vec![(
                    Apply(title_event(A, "EVE - Other")),
                    vec![Change::Title {
                        address: A,
                        label_changed: true,
                        key_changed: false,
                    }],
                )],
                vec![A, B],
            ),
            (
                "logout title",
                vec![
                    (
                        Apply(title_event(B, "EVE - Pilot Two")),
                        vec![Change::Title {
                            address: B,
                            label_changed: true,
                            key_changed: true,
                        }],
                    ),
                    (
                        Apply(title_event(B, "EVE")),
                        vec![Change::Title {
                            address: B,
                            label_changed: true,
                            key_changed: false,
                        }],
                    ),
                ],
                vec![A, B],
            ),
            (
                "same title again",
                vec![(
                    Apply(title_event(A, "EVE - Pilot One")),
                    vec![Change::Title {
                        address: A,
                        label_changed: false,
                        key_changed: false,
                    }],
                )],
                vec![A, B],
            ),
            (
                "title that stops being a game title",
                vec![(Apply(title_event(A, "")), vec![removed(A, "title \"\"")])],
                vec![B],
            ),
            (
                "title with a quote stops being a game title",
                vec![(
                    Apply(title_event(A, "EVE Launcher \"x\"")),
                    vec![removed(A, "title \"EVE Launcher \\\"x\\\"\"")],
                )],
                vec![B],
            ),
            (
                "move of a client without a name",
                vec![(
                    Apply(move_event(B, "EVE5")),
                    vec![Change::Workspace {
                        address: B,
                        label_changed: true,
                    }],
                )],
                vec![A, B],
            ),
            (
                "move of a logged-in client",
                vec![(
                    Apply(move_event(A, "EVE5")),
                    vec![Change::Workspace {
                        address: A,
                        label_changed: false,
                    }],
                )],
                vec![B, A],
            ),
            (
                "events for an unknown address",
                vec![
                    (Apply(ipc::Event::CloseWindow { address: X }), vec![]),
                    (Apply(title_event(X, "EVE - Nobody")), vec![]),
                    (Apply(move_event(X, "EVE5")), vec![]),
                ],
                vec![A, B],
            ),
        ];
        for (name, script, want_ordered) in cases {
            run(name, None, script, want_ordered);
        }
    }

    #[test]
    fn lookup_cases() {
        use Op::*;
        let lookup = |address| vec![Change::Lookup { address }];
        let cases: Vec<(&str, Script, Vec<u64>)> = vec![
            (
                "miss twice",
                vec![
                    (Apply(open(C, GAME, "EVE-new", "EVE")), lookup(C)),
                    (Missed(C), vec![Change::Retry { address: C }]),
                    (Missed(C), vec![removed(C, "not listed by j/clients")]),
                    (Missed(C), vec![]),
                ],
                vec![A, B],
            ),
            (
                "miss of an unknown address",
                vec![(Missed(C), vec![])],
                vec![A, B],
            ),
            (
                "second openwindow while pending",
                vec![
                    (Apply(open(C, GAME, "EVE-new", "EVE")), lookup(C)),
                    (Apply(open(C, GAME, "EVE-new", "EVE")), vec![]),
                ],
                vec![A, B],
            ),
            (
                "hit is the launcher",
                vec![
                    (Apply(open(C, GAME, "EVE-new", "EVE")), lookup(C)),
                    (Add(game(C, "Games", "EVE Launcher"), None), vec![]),
                    (Apply(ipc::Event::CloseWindow { address: C }), vec![]),
                    (Missed(C), vec![]),
                ],
                vec![A, B],
            ),
            (
                "hit is a game client",
                vec![
                    (Apply(open(C, GAME, "EVE-new", "EVE")), lookup(C)),
                    (
                        Add(game(C, "EVE-new", "EVE"), Some(1000002)),
                        vec![Change::Added { address: C }],
                    ),
                ],
                vec![A, B, C],
            ),
            (
                "hit after a retry",
                vec![
                    (Apply(open(C, GAME, "EVE-new", "EVE")), lookup(C)),
                    (Missed(C), vec![Change::Retry { address: C }]),
                    (
                        Add(game(C, "EVE-new", "EVE"), None),
                        vec![Change::Added { address: C }],
                    ),
                    (Missed(C), vec![]),
                ],
                vec![A, B, C],
            ),
            (
                "add of a tracked address",
                vec![(Add(game(A, "EVE5", "EVE"), None), vec![])],
                vec![A, B],
            ),
        ];
        for (name, script, want_ordered) in cases {
            run(name, None, script, want_ordered);
        }
    }

    #[test]
    fn ring_owner_cases() {
        use Op::*;
        let cases: Vec<(&str, Option<u64>, Script, Option<u64>)> = vec![
            (
                "owner follows the active address",
                None,
                vec![
                    (Thumbnail(A), vec![]),
                    (Apply(active(Some(A))), vec![owner(Some(A), None)]),
                    (Apply(active(None)), vec![owner(None, Some(A))]),
                ],
                None,
            ),
            (
                "active before the thumbnail",
                None,
                vec![
                    (Apply(active(Some(A))), vec![]),
                    (Thumbnail(A), vec![owner(Some(A), None)]),
                ],
                Some(A),
            ),
            (
                "initial active address",
                Some(A),
                vec![(Thumbnail(A), vec![owner(Some(A), None)])],
                Some(A),
            ),
            (
                "untracked active address",
                None,
                vec![
                    (Thumbnail(A), vec![]),
                    (Apply(active(Some(A))), vec![owner(Some(A), None)]),
                    (Apply(active(Some(X))), vec![owner(None, Some(A))]),
                ],
                None,
            ),
            (
                "pending client",
                None,
                vec![
                    (
                        Apply(open(C, GAME, "EVE-new", "EVE")),
                        vec![Change::Lookup { address: C }],
                    ),
                    (Apply(active(Some(C))), vec![]),
                    (
                        Add(game(C, "EVE-new", "EVE"), None),
                        vec![Change::Added { address: C }],
                    ),
                    (Thumbnail(C), vec![owner(Some(C), None)]),
                ],
                Some(C),
            ),
            (
                "owner moves between thumbnails",
                None,
                vec![
                    (Thumbnail(A), vec![]),
                    (Thumbnail(B), vec![]),
                    (Apply(active(Some(A))), vec![owner(Some(A), None)]),
                    (Apply(active(Some(B))), vec![owner(Some(B), Some(A))]),
                ],
                Some(B),
            ),
            (
                "same active address again",
                None,
                vec![
                    (Thumbnail(A), vec![]),
                    (Apply(active(Some(A))), vec![owner(Some(A), None)]),
                    (Apply(active(Some(A))), vec![]),
                ],
                Some(A),
            ),
            (
                "thumbnail of an unknown address",
                Some(X),
                vec![(Thumbnail(X), vec![])],
                None,
            ),
        ];
        for (name, initial, script, want_owner) in cases {
            let mut clients = base(initial);
            play(name, &mut clients, script);
            assert_eq!(clients.ring_owner(), want_owner, "{name} ring owner");
        }
    }

    #[test]
    fn ordered_cases() {
        use Op::*;
        let added = |address| vec![Change::Added { address }];
        let cases: Vec<(&str, Script, Vec<u64>)> = vec![
            (
                "slots then no slot",
                vec![(Add(game(C, "EVE-new", "EVE"), None), added(C))],
                vec![A, B, C],
            ),
            (
                "same slot by address",
                vec![(Add(game(D, "EVE2", "EVE"), None), added(D))],
                vec![D, A, B],
            ),
            (
                "no slot by address",
                vec![
                    (Add(game(C, "EVE-new", "EVE"), None), added(C)),
                    (Add(game(D, "Games", "EVE"), None), added(D)),
                ],
                vec![A, B, D, C],
            ),
            (
                "slot beats address",
                vec![(Add(game(E, "EVE1", "EVE"), None), added(E))],
                vec![E, A, B],
            ),
            (
                "move changes the order",
                vec![(
                    Apply(move_event(A, "EVE9")),
                    vec![Change::Workspace {
                        address: A,
                        label_changed: false,
                    }],
                )],
                vec![B, A],
            ),
        ];
        for (name, script, want_ordered) in cases {
            run(name, None, script, want_ordered);
        }
    }

    struct PendingCase {
        name: &'static str,
        script: Script,
        want_ordered: Vec<u64>,
        want_pending: Vec<u64>,
        want_owner: Option<u64>,
    }

    fn run_pending(cases: Vec<PendingCase>) {
        for case in cases {
            let name = case.name;
            let mut clients = base(None);
            play(name, &mut clients, case.script);
            assert_eq!(clients.ordered(), case.want_ordered, "{name} ordered");
            let pending: Vec<u64> = clients.pending.keys().copied().collect();
            assert_eq!(pending, case.want_pending, "{name} pending");
            let is_pending: Vec<u64> = [A, B, C, D, X]
                .into_iter()
                .filter(|a| clients.is_pending(*a))
                .collect();
            let mut want_is_pending = case.want_pending.clone();
            want_is_pending.sort_by_key(|a| [A, B, C, D, X].iter().position(|k| k == a));
            assert_eq!(is_pending, want_is_pending, "{name} is_pending");
            assert_eq!(clients.ring_owner(), case.want_owner, "{name} ring owner");
        }
    }

    #[test]
    fn retitle_untracked_cases() {
        use Op::*;
        let lookup = vec![Change::Lookup { address: C }];
        run_pending(vec![
            PendingCase {
                name: "pending address, non-game title",
                script: vec![
                    (Apply(open(C, GAME, "EVE-new", "EVE")), lookup),
                    (Apply(title_event(C, "EVE Launcher")), vec![]),
                ],
                want_ordered: vec![A, B],
                want_pending: vec![C],
                want_owner: None,
            },
            PendingCase {
                name: "unknown address, non-game title",
                script: vec![(Apply(title_event(X, "EVE Launcher")), vec![])],
                want_ordered: vec![A, B],
                want_pending: vec![],
                want_owner: None,
            },
            PendingCase {
                name: "unknown address, game title",
                script: vec![(Apply(title_event(X, "EVE - Pilot Three")), vec![])],
                want_ordered: vec![A, B],
                want_pending: vec![],
                want_owner: None,
            },
        ]);
    }

    #[test]
    fn remove_pending_cases() {
        use Op::*;
        let open_c = || {
            (
                Apply(open(C, GAME, "EVE-new", "EVE")),
                vec![Change::Lookup { address: C }],
            )
        };
        let open_d = || {
            (
                Apply(open(D, GAME, "EVE-new", "EVE")),
                vec![Change::Lookup { address: D }],
            )
        };
        run_pending(vec![
            PendingCase {
                name: "only pending address",
                script: vec![
                    open_c(),
                    (Remove(C, "x"), vec![removed(C, "x")]),
                    (Missed(C), vec![]),
                ],
                want_ordered: vec![A, B],
                want_pending: vec![],
                want_owner: None,
            },
            PendingCase {
                name: "another pending remains",
                script: vec![open_c(), open_d(), (Remove(C, "x"), vec![removed(C, "x")])],
                want_ordered: vec![A, B],
                want_pending: vec![D],
                want_owner: None,
            },
            PendingCase {
                name: "tracked address while a pending remains",
                script: vec![open_c(), (Remove(A, "x"), vec![removed(A, "x")])],
                want_ordered: vec![B],
                want_pending: vec![C],
                want_owner: None,
            },
            PendingCase {
                name: "closewindow of a pending address",
                script: vec![
                    open_c(),
                    (
                        Apply(ipc::Event::CloseWindow { address: C }),
                        vec![removed(C, "closed")],
                    ),
                ],
                want_ordered: vec![A, B],
                want_pending: vec![],
                want_owner: None,
            },
        ]);
    }

    #[test]
    fn remove_cases() {
        use Op::*;
        let cases: Vec<(&str, Script, Vec<u64>)> = vec![
            (
                "owner removed",
                vec![
                    (Thumbnail(A), vec![]),
                    (Apply(active(Some(A))), vec![owner(Some(A), None)]),
                    (
                        Remove(A, "5 consecutive capture failures"),
                        vec![
                            removed(A, "5 consecutive capture failures"),
                            owner(None, Some(A)),
                        ],
                    ),
                ],
                vec![B],
            ),
            (
                "non-owner removed",
                vec![
                    (Thumbnail(A), vec![]),
                    (Apply(active(Some(A))), vec![owner(Some(A), None)]),
                    (Remove(B, "x"), vec![removed(B, "x")]),
                ],
                vec![A],
            ),
            (
                "unknown address",
                vec![(Remove(X, "x"), vec![])],
                vec![A, B],
            ),
            (
                "removed twice",
                vec![
                    (Remove(A, "x"), vec![removed(A, "x")]),
                    (Remove(A, "x"), vec![]),
                ],
                vec![B],
            ),
            (
                "closewindow of a pending address",
                vec![
                    (
                        Apply(open(C, GAME, "EVE-new", "EVE")),
                        vec![Change::Lookup { address: C }],
                    ),
                    (
                        Apply(ipc::Event::CloseWindow { address: C }),
                        vec![removed(C, "closed")],
                    ),
                    (Missed(C), vec![]),
                ],
                vec![A, B],
            ),
            (
                "re-added client has no thumbnail",
                vec![
                    (Thumbnail(A), vec![]),
                    (Apply(active(Some(A))), vec![owner(Some(A), None)]),
                    (Remove(A, "x"), vec![removed(A, "x"), owner(None, Some(A))]),
                    (
                        Add(game(A, "EVE2", "EVE"), None),
                        vec![Change::Added { address: A }],
                    ),
                    (Thumbnail(A), vec![owner(Some(A), None)]),
                ],
                vec![A, B],
            ),
        ];
        for (name, script, want_ordered) in cases {
            run(name, None, script, want_ordered);
        }
    }
}
