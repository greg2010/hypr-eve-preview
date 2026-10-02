use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config;
use crate::geometry::{Point, Size};

pub const SAVE_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub x: u32,
    pub y: u32,
    pub width: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(transparent)]
pub struct LayoutFile {
    pub entries: BTreeMap<String, Entry>,
}

#[derive(Debug)]
pub enum LayoutError {
    Read(std::io::Error),
    Json(serde_json::Error),
    Write(std::io::Error),
    Rename(std::io::Error),
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutError::Read(e) | LayoutError::Write(e) | LayoutError::Rename(e) => {
                write!(f, "{e}")
            }
            LayoutError::Json(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LayoutError {}

const FILE_NAME: &str = "layout.json";
const BAD_NAME: &str = "layout.json.bad";
const TMP_NAME: &str = "layout.json.tmp";

pub fn default_path(state_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    config::xdg_base(state_home, home, ".local/state")
        .map(|base| base.join(config::APP_DIR).join(FILE_NAME))
}

/// A missing file is an empty layout.
pub fn load(path: &Path) -> Result<LayoutFile, LayoutError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(LayoutFile::default()),
        Err(e) => return Err(LayoutError::Read(e)),
    };
    serde_json::from_str(&text).map_err(LayoutError::Json)
}

/// Renames `path` to `layout.json.bad` in the same directory, replacing an older one, and
/// returns the new path.
pub fn set_aside(path: &Path) -> Result<PathBuf, LayoutError> {
    let bad = path.with_file_name(BAD_NAME);
    fs::rename(path, &bad).map_err(LayoutError::Rename)?;
    Ok(bad)
}

/// Writes `layout.json.tmp` beside `path`, syncs it and renames it over `path`. Creates the
/// directory with mode 0700 when missing. Every I/O failure is `Write`.
pub fn save(path: &Path, file: &LayoutFile) -> Result<(), LayoutError> {
    let bytes = serde_json::to_vec_pretty(file).map_err(LayoutError::Json)?;
    let write = || -> io::Result<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let tmp = path.with_file_name(TMP_NAME);
        let mut out = File::create(&tmp)?;
        out.write_all(&bytes)?;
        out.sync_all()?;
        fs::rename(&tmp, path)
    };
    write().map_err(LayoutError::Write)
}

pub fn even_down(value: u32) -> u32 {
    value & !1
}

/// Rounds down to even, then clamps to `min_width` and the smaller of `max_width` and the
/// even-rounded usable width. `min_width` wins when the bounds cross.
pub fn effective_width(width: u32, config: &config::Thumbnail, usable: Size) -> u32 {
    let max = config.max_width.min(even_down(usable.width));
    even_down(width).min(max).max(config.min_width)
}

pub fn clamp_position(x: i64, y: i64, size: Size, usable: Size) -> Point {
    let fit = |value: i64, extent: u32, bound: u32| {
        let max = i64::from(bound.saturating_sub(extent));
        even_down(u32::try_from(value.clamp(0, max)).unwrap_or(0))
    };
    Point {
        x: fit(x, size.width, usable.width),
        y: fit(y, size.height, usable.height),
    }
}

pub fn default_position(index: usize, placement: &config::Placement, width: u32) -> (i64, i64) {
    let step = i64::from(width) + i64::from(placement.gap);
    let index = i64::try_from(index).unwrap_or(i64::MAX);
    (
        i64::from(placement.x).saturating_add(index.saturating_mul(step)),
        i64::from(placement.y),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyChange {
    Saved,
    Apply(Entry),
    Default,
}

/// A user-placed client keeps `current`, which becomes the key's entry (`Saved`, the caller
/// requests a save). Otherwise the key's saved entry applies, or the default placement.
pub fn key_change(
    file: &mut LayoutFile,
    key: &str,
    current: Entry,
    user_placed: bool,
) -> KeyChange {
    if user_placed {
        file.entries.insert(key.to_string(), current);
        return KeyChange::Saved;
    }
    match file.entries.get(key) {
        Some(entry) => KeyChange::Apply(*entry),
        None => KeyChange::Default,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveAction {
    WriteNow,
    ArmAt(Instant),
    Joined,
}

#[derive(Debug, Default)]
pub struct SaveSchedule {
    last_write: Option<Instant>,
    armed: bool,
}

impl SaveSchedule {
    pub fn request(&mut self, now: Instant) -> SaveAction {
        if self.armed {
            return SaveAction::Joined;
        }
        match self.last_write {
            Some(last) if now.saturating_duration_since(last) < SAVE_INTERVAL => {
                self.armed = true;
                SaveAction::ArmAt(last + SAVE_INTERVAL)
            }
            _ => SaveAction::WriteNow,
        }
    }

    /// Call after every write attempt.
    pub fn wrote(&mut self, now: Instant) {
        self.last_write = Some(now);
        self.armed = false;
    }

    /// True while a timer is armed.
    pub fn pending(&self) -> bool {
        self.armed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::os::unix::fs::PermissionsExt;

    fn entry(x: u32, y: u32, width: u32) -> Entry {
        Entry { x, y, width }
    }

    fn file(entries: &[(&str, Entry)]) -> LayoutFile {
        LayoutFile {
            entries: entries
                .iter()
                .map(|(k, e)| ((*k).to_string(), *e))
                .collect(),
        }
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn placement() -> config::Placement {
        config::Placement { x: 8, y: 8, gap: 8 }
    }

    fn thumbnail() -> config::Thumbnail {
        config::Thumbnail {
            width: 480,
            min_width: 160,
            max_width: 1280,
        }
    }

    const USABLE: Size = Size {
        width: 2560,
        height: 1406,
    };

    #[test]
    fn default_position_cases() {
        let cases = [(0, (8, 8)), (1, (496, 8)), (5, (2448, 8))];
        for (index, want) in cases {
            assert_eq!(default_position(index, &placement(), 480), want, "{index}");
        }
    }

    #[test]
    fn clamp_position_cases() {
        let thumb = Size {
            width: 480,
            height: 264,
        };
        let wide = Size {
            width: 2600,
            height: 100,
        };
        let cases = [
            ("right edge", 2448, 8, thumb, Point { x: 2080, y: 8 }),
            ("negative", -10, -10, thumb, Point { x: 0, y: 0 }),
            ("odd rounds down", 9, 11, thumb, Point { x: 8, y: 10 }),
            ("bottom edge", 100, 1300, thumb, Point { x: 100, y: 1142 }),
            ("wider than U", 0, 0, wide, Point { x: 0, y: 0 }),
        ];
        for (name, x, y, size, want) in cases {
            assert_eq!(clamp_position(x, y, size, USABLE), want, "{name}");
        }
    }

    #[test]
    fn effective_width_cases() {
        let narrow = Size {
            width: 1001,
            height: 1406,
        };
        let cases = [
            ("odd rounds down", 481, USABLE, 480),
            ("below min", 100, USABLE, 160),
            ("above max", 1300, USABLE, 1280),
            ("capped by U", 1280, narrow, 1000),
        ];
        for (name, width, usable, want) in cases {
            assert_eq!(effective_width(width, &thumbnail(), usable), want, "{name}");
        }
    }

    #[test]
    fn file_cases() {
        let tmp = TempDir::new("file");
        let dir = tmp.path().to_path_buf();
        let ok = file(&[("user:1000001", entry(8, 8, 480))]);
        let cases: [(&str, Option<&str>, Result<LayoutFile, &str>); 7] = [
            ("missing", None, Ok(LayoutFile::default())),
            (
                "valid",
                Some(r#"{"user:1000001":{"x":8,"y":8,"width":480}}"#),
                Ok(ok),
            ),
            ("not json", Some("not json"), Err("json")),
            ("array", Some("[]"), Err("json")),
            ("missing fields", Some(r#"{"user:1":{"x":2}}"#), Err("json")),
            (
                "unknown field",
                Some(r#"{"user:1":{"x":2,"y":2,"width":2,"h":2}}"#),
                Err("json"),
            ),
            ("directory", None, Err("read IsADirectory")),
        ];
        for (name, text, want) in cases {
            let path = dir.join(format!("{}.json", name.replace(' ', "-")));
            if let Some(text) = text {
                std::fs::write(&path, text).unwrap();
            }
            if name == "directory" {
                std::fs::create_dir(&path).unwrap();
            }
            let got = load(&path).map_err(|e| match e {
                LayoutError::Json(_) => "json".to_string(),
                LayoutError::Read(e) => format!("read {:?}", e.kind()),
                LayoutError::Write(_) => "write".to_string(),
                LayoutError::Rename(_) => "rename".to_string(),
            });
            assert_eq!(got, want.map_err(String::from), "{name}");
        }

        let paths = [
            (
                "state home",
                Some("/x"),
                Some("/h"),
                Some("/x/hypr-eve-preview/layout.json"),
            ),
            (
                "unset",
                None,
                Some("/h"),
                Some("/h/.local/state/hypr-eve-preview/layout.json"),
            ),
            (
                "empty",
                Some(""),
                Some("/h"),
                Some("/h/.local/state/hypr-eve-preview/layout.json"),
            ),
            (
                "relative",
                Some("rel"),
                Some("/h"),
                Some("/h/.local/state/hypr-eve-preview/layout.json"),
            ),
            ("nothing", None, None, None),
        ];
        for (name, state, home, want) in paths {
            let got = default_path(state.map(OsStr::new), home.map(OsStr::new));
            assert_eq!(got, want.map(PathBuf::from), "{name}");
        }
    }

    enum SaveStep {
        Save(&'static str, LayoutFile),
        Expect {
            path: &'static str,
            listing_of: &'static str,
            listing: Vec<&'static str>,
            load: LayoutFile,
        },
        DirMode(&'static str, u32),
        Text(&'static str, &'static str),
        WriteFile(&'static str, &'static str),
        WriteFails(&'static str, LayoutFile),
    }

    fn both() -> LayoutFile {
        file(&[
            ("user:1000001", entry(8, 8, 480)),
            ("character:Pilot Two", entry(496, 8, 480)),
        ])
    }

    #[test]
    fn save_cases() {
        let want_text = "{\n  \"character:Pilot Two\": {\n    \"x\": 496,\n    \"y\": 8,\n    \
                         \"width\": 480\n  },\n  \"user:1000001\": {\n    \"x\": 8,\n    \"y\": 8,\n    \
                         \"width\": 480\n  }\n}";
        let again = file(&[("user:1000001", entry(10, 12, 544))]);
        let cases = [
            (
                "first save creates the directory with mode 0700",
                vec![
                    SaveStep::Save("hypr-eve-preview/layout.json", both()),
                    SaveStep::DirMode("hypr-eve-preview", 0o700),
                    SaveStep::Expect {
                        path: "hypr-eve-preview/layout.json",
                        listing_of: "hypr-eve-preview",
                        listing: vec!["layout.json"],
                        load: both(),
                    },
                    SaveStep::Text("hypr-eve-preview/layout.json", want_text),
                ],
            ),
            (
                "second save replaces the file and leaves no temp file",
                vec![
                    SaveStep::Save("hypr-eve-preview/layout.json", both()),
                    SaveStep::Save("hypr-eve-preview/layout.json", again.clone()),
                    SaveStep::Expect {
                        path: "hypr-eve-preview/layout.json",
                        listing_of: "hypr-eve-preview",
                        listing: vec!["layout.json"],
                        load: again,
                    },
                ],
            ),
            (
                "a file in place of the directory is a write error",
                vec![
                    SaveStep::WriteFile("blocker", "x"),
                    SaveStep::WriteFails("blocker/hypr-eve-preview/layout.json", both()),
                ],
            ),
        ];
        for (name, steps) in cases {
            let tmp = TempDir::new("save");
            let dir = tmp.path();
            for step in steps {
                match step {
                    SaveStep::Save(rel, contents) => {
                        save(&dir.join(rel), &contents).unwrap();
                    }
                    SaveStep::Expect {
                        path,
                        listing_of,
                        listing: want_listing,
                        load: want_load,
                    } => {
                        assert_eq!(listing(&dir.join(listing_of)), want_listing, "{name}");
                        assert_eq!(load(&dir.join(path)).unwrap(), want_load, "{name}");
                    }
                    SaveStep::DirMode(rel, want) => {
                        let mode = std::fs::metadata(dir.join(rel))
                            .unwrap()
                            .permissions()
                            .mode();
                        assert_eq!(mode & 0o777, want, "{name}");
                    }
                    SaveStep::WriteFile(rel, text) => std::fs::write(dir.join(rel), text).unwrap(),
                    SaveStep::WriteFails(rel, contents) => {
                        let got = save(&dir.join(rel), &contents)
                            .map_err(|e| matches!(e, LayoutError::Write(_)));
                        assert_eq!(got, Err(true), "{name}");
                    }
                    SaveStep::Text(rel, want) => {
                        assert_eq!(
                            std::fs::read_to_string(dir.join(rel)).unwrap(),
                            want,
                            "{name}"
                        );
                    }
                }
            }
        }
    }

    enum Want {
        WriteNow,
        ArmAtMs(u64),
        Joined,
    }

    enum ScheduleStep {
        Request(u64, Want),
        Wrote(u64),
        Pending(bool),
    }

    #[test]
    fn schedule_cases() {
        use ScheduleStep::{Pending, Request, Wrote};
        use Want::{ArmAtMs, Joined, WriteNow};
        let cases = [
            (
                "arm, join, write at the timer, then write after the interval",
                vec![
                    Request(0, WriteNow),
                    Pending(false),
                    Wrote(0),
                    Request(100, ArmAtMs(500)),
                    Pending(true),
                    Request(200, Joined),
                    Pending(true),
                    Wrote(500),
                    Pending(false),
                    Request(1100, WriteNow),
                ],
            ),
            (
                "an earlier write disarms the timer",
                vec![
                    Wrote(0),
                    Request(100, ArmAtMs(500)),
                    Pending(true),
                    Wrote(300),
                    Pending(false),
                    Request(900, WriteNow),
                ],
            ),
        ];
        for (name, steps) in cases {
            let t0 = Instant::now();
            let at = |n: u64| t0 + Duration::from_millis(n);
            let mut s = SaveSchedule::default();
            for (i, step) in steps.into_iter().enumerate() {
                match step {
                    Request(ms, want) => {
                        let want = match want {
                            WriteNow => SaveAction::WriteNow,
                            ArmAtMs(n) => SaveAction::ArmAt(at(n)),
                            Joined => SaveAction::Joined,
                        };
                        assert_eq!(s.request(at(ms)), want, "{name} step {i}");
                    }
                    Wrote(ms) => s.wrote(at(ms)),
                    Pending(want) => assert_eq!(s.pending(), want, "{name} step {i}"),
                }
            }
        }
    }

    enum AsideSetup {
        Plain,
        BadIsNonEmptyDir,
    }

    #[test]
    fn set_aside_cases() {
        let cases = [
            (
                "replaces an older bad file",
                AsideSetup::Plain,
                Ok("layout.json.bad"),
                vec!["layout.json.bad"],
                Some(("layout.json.bad", "not json")),
            ),
            (
                "rename blocked by a directory",
                AsideSetup::BadIsNonEmptyDir,
                Err(true),
                vec!["layout.json", "layout.json.bad"],
                Some(("layout.json", "not json")),
            ),
        ];
        for (name, setup, want, want_listing, want_content) in cases {
            let tmp = TempDir::new("aside");
            let dir = tmp.path();
            let path = dir.join("layout.json");
            std::fs::write(&path, "not json").unwrap();
            match setup {
                AsideSetup::Plain => {
                    std::fs::write(dir.join("layout.json.bad"), "old").unwrap();
                }
                AsideSetup::BadIsNonEmptyDir => {
                    std::fs::create_dir(dir.join("layout.json.bad")).unwrap();
                    std::fs::write(dir.join("layout.json.bad/keep"), "x").unwrap();
                }
            }
            let got = set_aside(&path)
                .map(|p| p.strip_prefix(dir).unwrap().to_path_buf())
                .map_err(|e| matches!(e, LayoutError::Rename(_)));
            assert_eq!(got, want.map(PathBuf::from), "{name}");
            assert_eq!(listing(dir), want_listing, "{name}");
            if let Some((rel, text)) = want_content {
                assert_eq!(
                    std::fs::read_to_string(dir.join(rel)).unwrap(),
                    text,
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn apply_cases() {
        let both = [
            ("user:1000001", entry(8, 8, 480)),
            ("character:Pilot Two", entry(496, 8, 480)),
        ];
        let only_user = [("user:1000001", entry(8, 8, 480))];
        let placed = entry(100, 200, 544);
        let moved = [
            ("user:1000001", entry(8, 8, 480)),
            ("character:Pilot Two", placed),
        ];
        let cases = [
            (
                "saved entry applies",
                &both[..],
                false,
                KeyChange::Apply(entry(496, 8, 480)),
                &both[..],
            ),
            (
                "user placed wins over a saved entry",
                &both[..],
                true,
                KeyChange::Saved,
                &moved[..],
            ),
            (
                "no entry is default",
                &only_user[..],
                false,
                KeyChange::Default,
                &only_user[..],
            ),
            (
                "user placed creates the entry",
                &only_user[..],
                true,
                KeyChange::Saved,
                &moved[..],
            ),
        ];
        for (name, start, user_placed, want, want_file) in cases {
            let mut f = file(start);
            let got = key_change(&mut f, "character:Pilot Two", placed, user_placed);
            assert_eq!(got, want, "{name}");
            assert_eq!(f, file(want_file), "{name}");
        }
    }
}
