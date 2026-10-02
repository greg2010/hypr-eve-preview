use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::dmabuf::fourcc_text;
use crate::geometry::{Rect, Size};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailReason {
    Failed,
    Stall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Default,
    Verbose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DamageMode {
    Recommit,
    IgnoreDamage,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    Start {
        output: String,
        scale: f64,
        usable: Rect,
        mode: DamageMode,
        config: Option<PathBuf>,
        layout: PathBuf,
        font: PathBuf,
    },
    ClientAdded {
        address: u64,
        pid: i32,
        workspace: String,
        slot: Option<u8>,
        account: Option<String>,
        label: String,
    },
    ClientRemoved {
        address: u64,
        reason: String,
    },
    Account {
        address: u64,
        error: String,
    },
    Title {
        address: u64,
        label: String,
        account: Option<String>,
    },
    Workspace {
        address: u64,
        workspace: String,
        slot: Option<u8>,
        label: String,
    },
    Focus {
        address: Option<u64>,
    },
    Dispatch {
        address: u64,
        request: String,
        result: Result<(), String>,
    },
    LayoutSaved {
        path: PathBuf,
        entries: usize,
    },
    LayoutError {
        path: PathBuf,
        error: String,
        renamed: Option<PathBuf>,
    },
    Ignored {
        line: String,
    },
    Failed {
        address: u64,
        seq: u64,
        reason: FailReason,
        consecutive: u32,
    },
    Format {
        address: u64,
        fourcc: u32,
        modifier: u64,
        size: Size,
        planes: u32,
    },
    Ready {
        address: u64,
        seq: u64,
        slot: usize,
        tv_sec: u64,
        tv_nsec: u32,
        copy_to_ready: Duration,
    },
    ReleaseWait {
        address: u64,
        slot: usize,
    },
    Released {
        address: u64,
        slot: usize,
        waited: Duration,
    },
    Usage {
        message: String,
    },
    Exit {
        code: u8,
        reason: String,
    },
}

const SYNTAX: &str = "hypr-eve-preview [--config <path>] [--log <path>] [--verbose] [--seconds <N>] [--ignore-damage]";

fn json(s: &str) -> Result<String, fmt::Error> {
    serde_json::to_string(s).map_err(|_| fmt::Error)
}

fn json_path(p: &Path) -> Result<String, fmt::Error> {
    json(&p.to_string_lossy())
}

fn json_or_dash(s: Option<&str>) -> Result<String, fmt::Error> {
    s.map_or_else(|| Ok("-".to_string()), json)
}

fn slot_text(slot: Option<u8>) -> String {
    slot.map_or_else(|| "-".to_string(), |n| n.to_string())
}

impl Line {
    /// Verbose for the per-frame lines `Format`, `Ready`, `ReleaseWait` and `Released`.
    pub fn level(&self) -> Level {
        match self {
            Line::Format { .. }
            | Line::Ready { .. }
            | Line::ReleaseWait { .. }
            | Line::Released { .. } => Level::Verbose,
            _ => Level::Default,
        }
    }
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Line::Start {
                output,
                scale,
                usable,
                mode,
                config,
                layout,
                font,
            } => {
                let mode = match mode {
                    DamageMode::Recommit => "recommit",
                    DamageMode::IgnoreDamage => "ignore-damage",
                };
                let config = match config {
                    Some(p) => json_path(p)?,
                    None => "-".to_string(),
                };
                write!(
                    f,
                    "start output={output} scale={scale} usable={},{},{}x{} mode={mode} \
                     config={config} layout={} font={}",
                    usable.x,
                    usable.y,
                    usable.width,
                    usable.height,
                    json_path(layout)?,
                    json_path(font)?
                )
            }
            Line::ClientAdded {
                address,
                pid,
                workspace,
                slot,
                account,
                label,
            } => write!(
                f,
                "client-added address=0x{address:x} pid={pid} workspace={} slot={} account={} \
                 label={}",
                json(workspace)?,
                slot_text(*slot),
                json_or_dash(account.as_deref())?,
                json(label)?
            ),
            Line::ClientRemoved { address, reason } => {
                write!(
                    f,
                    "client-removed address=0x{address:x} reason={}",
                    json(reason)?
                )
            }
            Line::Account { address, error } => {
                write!(f, "account address=0x{address:x} error={}", json(error)?)
            }
            Line::Title {
                address,
                label,
                account,
            } => write!(
                f,
                "title address=0x{address:x} label={} account={}",
                json(label)?,
                json_or_dash(account.as_deref())?
            ),
            Line::Workspace {
                address,
                workspace,
                slot,
                label,
            } => write!(
                f,
                "workspace address=0x{address:x} workspace={} slot={} label={}",
                json(workspace)?,
                slot_text(*slot),
                json(label)?
            ),
            Line::Focus { address } => match address {
                Some(a) => write!(f, "focus address=0x{a:x}"),
                None => write!(f, "focus address=-"),
            },
            Line::Dispatch {
                address,
                request,
                result,
            } => {
                write!(
                    f,
                    "dispatch address=0x{address:x} request={}",
                    json(request)?
                )?;
                match result {
                    Ok(()) => write!(f, " reply=\"ok\""),
                    Err(e) => write!(f, " error={}", json(e)?),
                }
            }
            Line::LayoutSaved { path, entries } => {
                write!(
                    f,
                    "layout-saved path={} entries={entries}",
                    json_path(path)?
                )
            }
            Line::LayoutError {
                path,
                error,
                renamed,
            } => {
                write!(
                    f,
                    "layout-error path={} error={}",
                    json_path(path)?,
                    json(error)?
                )?;
                match renamed {
                    Some(r) => write!(f, " renamed={}", json_path(r)?),
                    None => Ok(()),
                }
            }
            Line::Ignored { line } => write!(f, "ignored line={}", json(line)?),
            Line::Failed {
                address,
                seq,
                reason,
                consecutive,
            } => {
                let reason = match reason {
                    FailReason::Failed => "failed",
                    FailReason::Stall => "stall",
                };
                write!(
                    f,
                    "failed address=0x{address:x} seq={seq} reason={reason} \
                     consecutive={consecutive}"
                )
            }
            Line::Format {
                address,
                fourcc,
                modifier,
                size,
                planes,
            } => write!(
                f,
                "format address=0x{address:x} fourcc={} modifier=0x{modifier:016x} \
                 size={}x{} planes={planes}",
                fourcc_text(*fourcc),
                size.width,
                size.height
            ),
            Line::Ready {
                address,
                seq,
                slot,
                tv_sec,
                tv_nsec,
                copy_to_ready,
            } => write!(
                f,
                "ready address=0x{address:x} seq={seq} buffer={slot} tv={tv_sec}.{tv_nsec:09} \
                 copy_to_ready_us={}",
                copy_to_ready.as_micros()
            ),
            Line::ReleaseWait { address, slot } => {
                write!(f, "release-wait address=0x{address:x} buffer={slot}")
            }
            Line::Released {
                address,
                slot,
                waited,
            } => write!(
                f,
                "released address=0x{address:x} buffer={slot} waited_us={}",
                waited.as_micros()
            ),
            Line::Usage { message } => {
                write!(f, "usage message={} syntax=\"{SYNTAX}\"", json(message)?)
            }
            Line::Exit { code, reason } => {
                write!(f, "exit code={code} reason={}", json(reason)?)
            }
        }
    }
}

/// Builds the `exit` line and returns its code with it. A teardown error is appended to the
/// reason and turns code 0 into 1.
pub fn exit_line(code: u8, reason: String, teardown: Option<String>) -> (u8, Line) {
    match teardown {
        Some(e) => {
            let code = code.max(1);
            let reason = format!("{reason}; teardown: {e}");
            (code, Line::Exit { code, reason })
        }
        None => (code, Line::Exit { code, reason }),
    }
}

/// Writes the prefixed line with a single `write_all`, so unbuffered stderr gets one write.
pub fn emit_to(w: &mut impl io::Write, line: &Line) -> io::Result<()> {
    w.write_all(format!("hypr-eve-preview: {line}\n").as_bytes())?;
    w.flush()
}

/// Writes lines to stderr and, when opened with a log path, to that file.
#[derive(Debug)]
pub struct Reporter {
    log: Option<File>,
    verbose: bool,
}

impl Reporter {
    /// Creates the missing parent directories of `log` and opens it for append.
    pub fn open(log: &Path, verbose: bool) -> io::Result<Reporter> {
        if let Some(parent) = log.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(log)?;
        Ok(Reporter {
            log: Some(file),
            verbose,
        })
    }

    /// A reporter that writes to stderr only.
    pub fn stderr(verbose: bool) -> Reporter {
        Reporter { log: None, verbose }
    }

    /// Writes stderr first, then the log. Verbose lines are skipped unless `verbose` was set.
    pub fn emit(&mut self, line: &Line) -> io::Result<()> {
        self.emit_with(&mut io::stderr().lock(), line)
    }

    fn emit_with(&mut self, stderr: &mut impl io::Write, line: &Line) -> io::Result<()> {
        if line.level() == Level::Verbose && !self.verbose {
            return Ok(());
        }
        emit_to(stderr, line)?;
        match &mut self.log {
            Some(file) => emit_to(file, line),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn lines() -> Vec<(&'static str, Line, Level, &'static str)> {
        use Level::{Default, Verbose};
        vec![
            (
                "start",
                Line::Start {
                    output: "DP-3".into(),
                    scale: 1.5,
                    usable: Rect {
                        x: 0,
                        y: 34,
                        width: 2560,
                        height: 1406,
                    },
                    mode: DamageMode::Recommit,
                    config: None,
                    layout: "/s/hypr-eve-preview/layout.json".into(),
                    font: "/usr/share/fonts/noto/NotoSansMono-Regular.ttf".into(),
                },
                Default,
                "start output=DP-3 scale=1.5 usable=0,34,2560x1406 mode=recommit config=- layout=\"/s/hypr-eve-preview/layout.json\" font=\"/usr/share/fonts/noto/NotoSansMono-Regular.ttf\"",
            ),
            (
                "start with config and ignore-damage",
                Line::Start {
                    output: "HDMI-A-1".into(),
                    scale: 2.0,
                    usable: Rect {
                        x: 10,
                        y: 20,
                        width: 1880,
                        height: 1020,
                    },
                    mode: DamageMode::IgnoreDamage,
                    config: Some("/c/config.toml".into()),
                    layout: "/s/l.json".into(),
                    font: "/f.ttf".into(),
                },
                Default,
                "start output=HDMI-A-1 scale=2 usable=10,20,1880x1020 mode=ignore-damage config=\"/c/config.toml\" layout=\"/s/l.json\" font=\"/f.ttf\"",
            ),
            (
                "client-added with slot and account",
                Line::ClientAdded {
                    address: 0x555512345678,
                    pid: 1001,
                    workspace: "EVE2".into(),
                    slot: Some(2),
                    account: Some("user:1000001".into()),
                    label: "Pilot One".into(),
                },
                Default,
                "client-added address=0x555512345678 pid=1001 workspace=\"EVE2\" slot=2 account=\"user:1000001\" label=\"Pilot One\"",
            ),
            (
                "client-added without slot or account",
                Line::ClientAdded {
                    address: 0x55559abcdef0,
                    pid: 1002,
                    workspace: "EVE-new".into(),
                    slot: None,
                    account: None,
                    label: "EVE".into(),
                },
                Default,
                "client-added address=0x55559abcdef0 pid=1002 workspace=\"EVE-new\" slot=- account=- label=\"EVE\"",
            ),
            (
                "client-removed",
                Line::ClientRemoved {
                    address: 0x555512345678,
                    reason: "closed".into(),
                },
                Default,
                "client-removed address=0x555512345678 reason=\"closed\"",
            ),
            (
                "account error",
                Line::Account {
                    address: 0x555512345678,
                    error: "no user id".into(),
                },
                Default,
                "account address=0x555512345678 error=\"no user id\"",
            ),
            (
                "account error escapes quotes",
                Line::Account {
                    address: 0xabc,
                    error: "bad \"x\"".into(),
                },
                Default,
                "account address=0xabc error=\"bad \\\"x\\\"\"",
            ),
            (
                "title",
                Line::Title {
                    address: 0x555512345678,
                    label: "Pilot One".into(),
                    account: Some("character:Pilot One".into()),
                },
                Default,
                "title address=0x555512345678 label=\"Pilot One\" account=\"character:Pilot One\"",
            ),
            (
                "title without account",
                Line::Title {
                    address: 0x555512345678,
                    label: "EVE2".into(),
                    account: None,
                },
                Default,
                "title address=0x555512345678 label=\"EVE2\" account=-",
            ),
            (
                "workspace",
                Line::Workspace {
                    address: 0x555512345678,
                    workspace: "EVE5".into(),
                    slot: Some(5),
                    label: "Pilot One".into(),
                },
                Default,
                "workspace address=0x555512345678 workspace=\"EVE5\" slot=5 label=\"Pilot One\"",
            ),
            (
                "focus some",
                Line::Focus {
                    address: Some(0x555512345678),
                },
                Default,
                "focus address=0x555512345678",
            ),
            (
                "focus none",
                Line::Focus { address: None },
                Default,
                "focus address=-",
            ),
            (
                "dispatch ok",
                Line::Dispatch {
                    address: 0x555512345678,
                    request: "/dispatch workspace name:EVE2".into(),
                    result: Ok(()),
                },
                Default,
                "dispatch address=0x555512345678 request=\"/dispatch workspace name:EVE2\" reply=\"ok\"",
            ),
            (
                "dispatch error",
                Line::Dispatch {
                    address: 0x555512345678,
                    request: "/dispatch workspace name:EVE2".into(),
                    result: Err("Bad workspace".into()),
                },
                Default,
                "dispatch address=0x555512345678 request=\"/dispatch workspace name:EVE2\" error=\"Bad workspace\"",
            ),
            (
                "layout-saved",
                Line::LayoutSaved {
                    path: "/s/hypr-eve-preview/layout.json".into(),
                    entries: 2,
                },
                Default,
                "layout-saved path=\"/s/hypr-eve-preview/layout.json\" entries=2",
            ),
            (
                "layout-error without rename",
                Line::LayoutError {
                    path: "/s/hypr-eve-preview/layout.json".into(),
                    error: "No space left on device (os error 28)".into(),
                    renamed: None,
                },
                Default,
                "layout-error path=\"/s/hypr-eve-preview/layout.json\" error=\"No space left on device (os error 28)\"",
            ),
            (
                "layout-error with rename",
                Line::LayoutError {
                    path: "/s/hypr-eve-preview/layout.json".into(),
                    error: "expected value at line 1 column 1".into(),
                    renamed: Some("/s/hypr-eve-preview/layout.json.bad".into()),
                },
                Default,
                "layout-error path=\"/s/hypr-eve-preview/layout.json\" error=\"expected value at line 1 column 1\" renamed=\"/s/hypr-eve-preview/layout.json.bad\"",
            ),
            (
                "ignored escapes quotes",
                Line::Ignored {
                    line: "closewindow>>\"zz".into(),
                },
                Default,
                "ignored line=\"closewindow>>\\\"zz\"",
            ),
            (
                "failed",
                Line::Failed {
                    address: 0x555512345678,
                    seq: 7,
                    reason: FailReason::Stall,
                    consecutive: 2,
                },
                Default,
                "failed address=0x555512345678 seq=7 reason=stall consecutive=2",
            ),
            (
                "failed reason failed",
                Line::Failed {
                    address: 0x555512345678,
                    seq: 1,
                    reason: FailReason::Failed,
                    consecutive: 1,
                },
                Default,
                "failed address=0x555512345678 seq=1 reason=failed consecutive=1",
            ),
            (
                "format",
                Line::Format {
                    address: 0x555512345678,
                    fourcc: 0x3432_5241,
                    modifier: 0x0300_0000_0000_0012,
                    size: Size {
                        width: 3840,
                        height: 2109,
                    },
                    planes: 1,
                },
                Verbose,
                "format address=0x555512345678 fourcc=AR24 (0x34325241) modifier=0x0300000000000012 size=3840x2109 planes=1",
            ),
            (
                "ready",
                Line::Ready {
                    address: 0x555512345678,
                    seq: 3,
                    slot: 0,
                    tv_sec: 12,
                    tv_nsec: 5,
                    copy_to_ready: Duration::from_micros(900),
                },
                Verbose,
                "ready address=0x555512345678 seq=3 buffer=0 tv=12.000000005 copy_to_ready_us=900",
            ),
            (
                "release-wait",
                Line::ReleaseWait {
                    address: 0x555512345678,
                    slot: 1,
                },
                Verbose,
                "release-wait address=0x555512345678 buffer=1",
            ),
            (
                "released",
                Line::Released {
                    address: 0x555512345678,
                    slot: 1,
                    waited: Duration::from_millis(120),
                },
                Verbose,
                "released address=0x555512345678 buffer=1 waited_us=120000",
            ),
            (
                "usage",
                Line::Usage {
                    message: "unknown flag --help".into(),
                },
                Default,
                "usage message=\"unknown flag --help\" syntax=\"hypr-eve-preview [--config <path>] [--log <path>] [--verbose] [--seconds <N>] [--ignore-damage]\"",
            ),
            (
                "exit",
                Line::Exit {
                    code: 0,
                    reason: "seconds elapsed".into(),
                },
                Default,
                "exit code=0 reason=\"seconds elapsed\"",
            ),
        ]
    }

    #[test]
    fn line_cases() {
        for (name, line, _, want) in lines() {
            assert_eq!(line.to_string(), want, "{name}");
        }
    }

    #[test]
    fn level_cases() {
        for (name, line, want, _) in lines() {
            assert_eq!(line.level(), want, "{name}");
        }
    }

    struct Stderr {
        buf: Vec<u8>,
        broken: bool,
    }

    impl io::Write for Stderr {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.broken {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            self.buf.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    enum Target {
        Stderr,
        Log(&'static str),
        Existing(&'static str),
        DevFull,
    }

    struct ReporterCase {
        name: &'static str,
        target: Target,
        verbose: bool,
        broken_stderr: bool,
        lines: Vec<Line>,
        want_results: Vec<Result<(), io::ErrorKind>>,
        want_stderr: String,
        want_log: Option<String>,
    }

    fn ready() -> Line {
        Line::Ready {
            address: 0x555512345678,
            seq: 3,
            slot: 0,
            tv_sec: 12,
            tv_nsec: 5,
            copy_to_ready: Duration::from_micros(900),
        }
    }

    const FOCUS_TEXT: &str = "hypr-eve-preview: focus address=-\n";
    const READY_TEXT: &str = "hypr-eve-preview: ready address=0x555512345678 seq=3 buffer=0 tv=12.000000005 copy_to_ready_us=900\n";

    #[test]
    fn reporter_cases() {
        let focus = || Line::Focus { address: None };
        let cases = vec![
            ReporterCase {
                name: "quiet skips verbose lines, creates parents",
                target: Target::Log("quiet/a/b/eve.log"),
                verbose: false,
                broken_stderr: false,
                lines: vec![ready(), focus()],
                want_results: vec![Ok(()), Ok(())],
                want_stderr: FOCUS_TEXT.to_string(),
                want_log: Some(FOCUS_TEXT.to_string()),
            },
            ReporterCase {
                name: "verbose writes both lines to both sinks",
                target: Target::Log("loud/eve.log"),
                verbose: true,
                broken_stderr: false,
                lines: vec![ready(), focus()],
                want_results: vec![Ok(()), Ok(())],
                want_stderr: format!("{READY_TEXT}{FOCUS_TEXT}"),
                want_log: Some(format!("{READY_TEXT}{FOCUS_TEXT}")),
            },
            ReporterCase {
                name: "log appends to an existing file",
                target: Target::Existing("append.log"),
                verbose: false,
                broken_stderr: false,
                lines: vec![focus()],
                want_results: vec![Ok(())],
                want_stderr: FOCUS_TEXT.to_string(),
                want_log: Some(format!("old\n{FOCUS_TEXT}")),
            },
            ReporterCase {
                name: "stderr only",
                target: Target::Stderr,
                verbose: false,
                broken_stderr: false,
                lines: vec![focus()],
                want_results: vec![Ok(())],
                want_stderr: FOCUS_TEXT.to_string(),
                want_log: None,
            },
            ReporterCase {
                name: "full log reports the write error after stderr",
                target: Target::DevFull,
                verbose: false,
                broken_stderr: false,
                lines: vec![focus()],
                want_results: vec![Err(io::ErrorKind::StorageFull)],
                want_stderr: FOCUS_TEXT.to_string(),
                want_log: None,
            },
            ReporterCase {
                name: "broken stderr",
                target: Target::Stderr,
                verbose: false,
                broken_stderr: true,
                lines: vec![focus()],
                want_results: vec![Err(io::ErrorKind::BrokenPipe)],
                want_stderr: String::new(),
                want_log: None,
            },
        ];
        let dir = TempDir::new("reporter");
        for case in cases {
            let log = match case.target {
                Target::Stderr => None,
                Target::Log(rel) => Some(dir.path().join(rel)),
                Target::Existing(rel) => {
                    let path = dir.path().join(rel);
                    std::fs::write(&path, "old\n").unwrap();
                    Some(path)
                }
                Target::DevFull => Some(PathBuf::from("/dev/full")),
            };
            let mut reporter = match &log {
                Some(path) => Reporter::open(path, case.verbose).unwrap(),
                None => Reporter::stderr(case.verbose),
            };
            let mut stderr = Stderr {
                buf: Vec::new(),
                broken: case.broken_stderr,
            };
            let results: Vec<_> = case
                .lines
                .iter()
                .map(|line| reporter.emit_with(&mut stderr, line).map_err(|e| e.kind()))
                .collect();
            assert_eq!(results, case.want_results, "{} results", case.name);
            assert_eq!(
                String::from_utf8(stderr.buf).unwrap(),
                case.want_stderr,
                "{} stderr",
                case.name
            );
            let got_log = match (&log, &case.want_log) {
                (Some(path), Some(_)) => Some(std::fs::read_to_string(path).unwrap()),
                _ => None,
            };
            assert_eq!(got_log, case.want_log, "{} log", case.name);
        }
    }

    #[test]
    fn open_cases() {
        let dir = TempDir::new("open");
        let cases = [
            ("writable", dir.path().join("a/eve.log"), Ok(())),
            (
                "path is a directory",
                dir.path().to_path_buf(),
                Err(io::ErrorKind::IsADirectory),
            ),
        ];
        for (name, path, want) in cases {
            let got = Reporter::open(&path, false)
                .map(|_| ())
                .map_err(|e| e.kind());
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn exit_line_cases() {
        let cases = [
            (
                "plain",
                0,
                "seconds elapsed",
                None,
                0,
                "exit code=0 reason=\"seconds elapsed\"",
            ),
            (
                "teardown error turns 0 into 1",
                0,
                "seconds elapsed",
                Some("flush: Broken pipe (os error 32)"),
                1,
                "exit code=1 reason=\"seconds elapsed; teardown: flush: Broken pipe (os error 32)\"",
            ),
            (
                "teardown error keeps 1",
                1,
                "overlay closed by compositor",
                Some("x"),
                1,
                "exit code=1 reason=\"overlay closed by compositor; teardown: x\"",
            ),
            ("usage", 2, "usage", None, 2, "exit code=2 reason=\"usage\""),
            (
                "pointer call reason",
                1,
                "set_cursor: no enter serial",
                None,
                1,
                "exit code=1 reason=\"set_cursor: no enter serial\"",
            ),
        ];
        for (name, code, reason, teardown, want_code, want) in cases {
            let (got_code, line) = exit_line(code, reason.to_string(), teardown.map(String::from));
            assert_eq!(got_code, want_code, "{name}");
            assert_eq!(line.to_string(), want, "{name}");
        }
    }

    #[test]
    fn emit_to_cases() {
        let line = Line::Exit {
            code: 0,
            reason: "seconds elapsed".into(),
        };
        let cases = [
            (
                "writes the prefixed line",
                false,
                Ok("hypr-eve-preview: exit code=0 reason=\"seconds elapsed\"\n".to_string()),
            ),
            ("broken pipe", true, Err(io::ErrorKind::BrokenPipe)),
        ];
        for (name, broken, want) in cases {
            let mut out = Stderr {
                buf: Vec::new(),
                broken,
            };
            let got = emit_to(&mut out, &line).map(|()| String::from_utf8(out.buf).unwrap());
            assert_eq!(got.map_err(|e| e.kind()), want, "{name}");
        }
    }
}
