use std::fmt;
use std::io;
use std::time::Duration;

use crate::capture::DamageMode;
use crate::dmabuf::fourcc_text;
use crate::geometry::Size;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailReason {
    Failed,
    Stall,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Client {
        address: u64,
        handle: u32,
        title: String,
        workspace: String,
    },
    Mode {
        mode: DamageMode,
    },
    Format {
        fourcc: u32,
        modifier: u64,
        size: Size,
        planes: u32,
    },
    Ready {
        seq: u64,
        slot: usize,
        tv_sec: u64,
        tv_nsec: u32,
        copy_to_ready: Duration,
    },
    Failed {
        seq: u64,
        reason: FailReason,
        consecutive: u32,
    },
    ReleaseWait {
        slot: usize,
    },
    Released {
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

const SYNTAX: &str =
    "eve-preview [--address 0x<hex>] [--seconds <N>] [--width <W>] [--ignore-damage]";

fn json(s: &str) -> Result<String, fmt::Error> {
    serde_json::to_string(s).map_err(|_| fmt::Error)
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Line::Client {
                address,
                handle,
                title,
                workspace,
            } => write!(
                f,
                "client address=0x{address:x} handle={handle} title={} workspace={workspace}",
                json(title)?
            ),
            Line::Mode { mode } => {
                let name = match mode {
                    DamageMode::Recommit => "recommit",
                    DamageMode::IgnoreDamage => "ignore-damage",
                };
                write!(f, "mode name={name}")
            }
            Line::Format {
                fourcc,
                modifier,
                size,
                planes,
            } => {
                write!(
                    f,
                    "format fourcc={} modifier=0x{modifier:016x} size={}x{} planes={planes}",
                    fourcc_text(*fourcc),
                    size.width,
                    size.height
                )
            }
            Line::Ready {
                seq,
                slot,
                tv_sec,
                tv_nsec,
                copy_to_ready,
            } => write!(
                f,
                "ready seq={seq} buffer={slot} tv={tv_sec}.{tv_nsec:09} copy_to_ready_us={}",
                copy_to_ready.as_micros()
            ),
            Line::Failed {
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
                    "failed seq={seq} reason={reason} consecutive={consecutive}"
                )
            }
            Line::ReleaseWait { slot } => write!(f, "release-wait buffer={slot}"),
            Line::Released { slot, waited } => {
                write!(f, "released buffer={slot} waited_us={}", waited.as_micros())
            }
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
    w.write_all(format!("eve-preview: {line}\n").as_bytes())?;
    w.flush()
}

/// Writes the line to stderr.
pub fn emit(line: &Line) -> io::Result<()> {
    emit_to(&mut io::stderr().lock(), line)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BrokenPipe;

    impl io::Write for BrokenPipe {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn line_cases() {
        let cases: Vec<(&str, Line, &str)> = vec![
            (
                "client",
                Line::Client {
                    address: 0x5608ab929d00,
                    handle: 2878512384,
                    title: "EVE".into(),
                    workspace: "EVE2".into(),
                },
                "client address=0x5608ab929d00 handle=2878512384 title=\"EVE\" workspace=EVE2",
            ),
            (
                "client title with quote",
                Line::Client {
                    address: 0x1,
                    handle: 1,
                    title: "EVE - \"Bob\"".into(),
                    workspace: "EVE1".into(),
                },
                "client address=0x1 handle=1 title=\"EVE - \\\"Bob\\\"\" workspace=EVE1",
            ),
            (
                "mode recommit",
                Line::Mode {
                    mode: DamageMode::Recommit,
                },
                "mode name=recommit",
            ),
            (
                "mode ignore-damage",
                Line::Mode {
                    mode: DamageMode::IgnoreDamage,
                },
                "mode name=ignore-damage",
            ),
            (
                "format",
                Line::Format {
                    fourcc: 0x34325241,
                    modifier: 0x0300000000606015,
                    size: Size {
                        width: 3840,
                        height: 2109,
                    },
                    planes: 1,
                },
                "format fourcc=AR24 (0x34325241) modifier=0x0300000000606015 size=3840x2109 planes=1",
            ),
            (
                "ready",
                Line::Ready {
                    seq: 7,
                    slot: 1,
                    tv_sec: 123456,
                    tv_nsec: 789,
                    copy_to_ready: Duration::from_micros(4321),
                },
                "ready seq=7 buffer=1 tv=123456.000000789 copy_to_ready_us=4321",
            ),
            (
                "ready nsec 5",
                Line::Ready {
                    seq: 1,
                    slot: 0,
                    tv_sec: 9,
                    tv_nsec: 5,
                    copy_to_ready: Duration::from_micros(0),
                },
                "ready seq=1 buffer=0 tv=9.000000005 copy_to_ready_us=0",
            ),
            (
                "failed",
                Line::Failed {
                    seq: 3,
                    reason: FailReason::Failed,
                    consecutive: 2,
                },
                "failed seq=3 reason=failed consecutive=2",
            ),
            (
                "stall",
                Line::Failed {
                    seq: 4,
                    reason: FailReason::Stall,
                    consecutive: 1,
                },
                "failed seq=4 reason=stall consecutive=1",
            ),
            (
                "release-wait",
                Line::ReleaseWait { slot: 0 },
                "release-wait buffer=0",
            ),
            (
                "released",
                Line::Released {
                    slot: 1,
                    waited: Duration::from_millis(120),
                },
                "released buffer=1 waited_us=120000",
            ),
            (
                "usage",
                Line::Usage {
                    message: "unknown argument \"--help\"".into(),
                },
                "usage message=\"unknown argument \\\"--help\\\"\" syntax=\"eve-preview [--address 0x<hex>] [--seconds <N>] [--width <W>] [--ignore-damage]\"",
            ),
            (
                "exit",
                Line::Exit {
                    code: 0,
                    reason: "seconds elapsed".into(),
                },
                "exit code=0 reason=\"seconds elapsed\"",
            ),
        ];
        for (name, line, want) in cases {
            assert_eq!(line.to_string(), want, "{name}");
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
                Ok("eve-preview: exit code=0 reason=\"seconds elapsed\"\n".to_string()),
            ),
            ("broken pipe", true, Err(io::ErrorKind::BrokenPipe)),
        ];
        for (name, broken, want) in cases {
            let got = if broken {
                emit_to(&mut BrokenPipe, &line).map(|()| String::new())
            } else {
                let mut buf = Vec::new();
                emit_to(&mut buf, &line).map(|()| String::from_utf8(buf).unwrap())
            };
            assert_eq!(got.map_err(|e| e.kind()), want, "{name}");
        }
    }
}
