use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

use crate::config;
use crate::control;

const MAX_SECONDS: u64 = 86_400;

/// The daemon invocation: every option is optional on the command line. `None` leaves the
/// default (config path, log destination, run until stopped), `false` leaves the flag off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub config: Option<PathBuf>,
    pub log: Option<PathBuf>,
    pub verbose: bool,
    pub seconds: Option<u64>,
    pub ignore_damage: bool,
}

/// What the command line asks for: `Daemon` carries the parsed options, `Command` a control
/// command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    Daemon(Args),
    Command(control::Command),
}

/// A command-line mistake. `main` prints the `usage` line (its `Display` text and
/// `report::syntax()`) and exits with code 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageError {
    Unknown(String),
    MissingValue(&'static str),
    Duplicate(&'static str),
    InvalidSeconds(String),
    NotUtf8(usize),
    InvalidOpacity(String),
    CommandArgument {
        command: &'static str,
        argument: String,
    },
}

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UsageError::Unknown(a) => write!(f, "unknown argument {a:?}"),
            UsageError::MissingValue(flag) => write!(f, "{flag} needs a value"),
            UsageError::Duplicate(flag) => write!(f, "{flag} given more than once"),
            UsageError::InvalidSeconds(v) => {
                write!(f, "invalid --seconds {v:?}: want 1 to {MAX_SECONDS}")
            }
            UsageError::NotUtf8(n) => write!(f, "argument {n} is not UTF-8"),
            UsageError::InvalidOpacity(v) => {
                write!(
                    f,
                    "invalid opacity {v:?}: want 0 to {}",
                    config::MAX_OPACITY
                )
            }
            UsageError::CommandArgument { command, argument } => {
                write!(f, "{command}: unexpected argument {argument:?}")
            }
        }
    }
}

impl std::error::Error for UsageError {}

fn once<T>(slot: &mut Option<T>, flag: &'static str, value: T) -> Result<(), UsageError> {
    if slot.is_some() {
        return Err(UsageError::Duplicate(flag));
    }
    *slot = Some(value);
    Ok(())
}

/// Like `parse`, for raw arguments. A non-UTF-8 argument is a usage error naming its 1-based
/// position.
pub fn parse_os(args: impl IntoIterator<Item = OsString>) -> Result<Invocation, UsageError> {
    let args = args
        .into_iter()
        .enumerate()
        .map(|(i, a)| a.into_string().map_err(|_| UsageError::NotUtf8(i + 1)))
        .collect::<Result<Vec<_>, _>>()?;
    parse(args)
}

/// Parses the arguments after argv[0]. Does no I/O. A command word is the whole invocation:
/// only the first argument can be one, `opacity` takes one value and the others nothing. Any
/// further argument is `CommandArgument`.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Invocation, UsageError> {
    let mut it = args.into_iter().peekable();
    let command = if it.peek().map(String::as_str) == Some(control::OPACITY_WORD) {
        it.next();
        let value = it
            .next()
            .ok_or(UsageError::MissingValue(control::OPACITY_WORD))?;
        let percent = control::Command::opacity_value(value.as_bytes())
            .ok_or(UsageError::InvalidOpacity(value))?;
        Some(control::Command::Opacity(percent))
    } else {
        let command = it
            .peek()
            .and_then(|first| control::Command::from_word(first.as_bytes()));
        if command.is_some() {
            it.next();
        }
        command
    };
    if let Some(command) = command {
        return match it.next() {
            None => Ok(Invocation::Command(command)),
            Some(argument) => Err(UsageError::CommandArgument {
                command: command.word(),
                argument,
            }),
        };
    }
    let mut config = None;
    let mut log = None;
    let mut seconds = None;
    let mut verbose = None;
    let mut ignore_damage = None;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => {
                let v = it.next().ok_or(UsageError::MissingValue("--config"))?;
                once(&mut config, "--config", PathBuf::from(v))?;
            }
            "--log" => {
                let v = it.next().ok_or(UsageError::MissingValue("--log"))?;
                once(&mut log, "--log", PathBuf::from(v))?;
            }
            "--seconds" => {
                let v = it.next().ok_or(UsageError::MissingValue("--seconds"))?;
                let n = v
                    .parse::<u64>()
                    .ok()
                    .filter(|n| (1..=MAX_SECONDS).contains(n))
                    .ok_or(UsageError::InvalidSeconds(v))?;
                once(&mut seconds, "--seconds", n)?;
            }
            "--verbose" => once(&mut verbose, "--verbose", ())?,
            "--ignore-damage" => once(&mut ignore_damage, "--ignore-damage", ())?,
            _ => return Err(UsageError::Unknown(arg)),
        }
    }
    Ok(Invocation::Daemon(Args {
        config,
        log,
        verbose: verbose.is_some(),
        seconds,
        ignore_damage: ignore_damage.is_some(),
    }))
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;

    use super::*;
    use crate::control::Command;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn ok(
        config: Option<&str>,
        log: Option<&str>,
        verbose: bool,
        seconds: Option<u64>,
        ignore_damage: bool,
    ) -> Result<Invocation, UsageError> {
        Ok(Invocation::Daemon(Args {
            config: config.map(PathBuf::from),
            log: log.map(PathBuf::from),
            verbose,
            seconds,
            ignore_damage,
        }))
    }

    #[test]
    fn parse_cases() {
        let unknown = |s: &str| Err(UsageError::Unknown(s.to_string()));
        let missing = |s: &'static str| Err(UsageError::MissingValue(s));
        let dup = |s: &'static str| Err(UsageError::Duplicate(s));
        let bad_opacity = |s: &str| Err(UsageError::InvalidOpacity(s.to_string()));
        let bad_secs = |s: &str| Err(UsageError::InvalidSeconds(s.to_string()));
        let cases: Vec<(&str, Vec<&str>, Result<Invocation, UsageError>)> = vec![
            ("defaults", vec![], ok(None, None, false, None, false)),
            ("lock", vec!["lock"], Ok(Invocation::Command(Command::Lock))),
            (
                "unlock",
                vec!["unlock"],
                Ok(Invocation::Command(Command::Unlock)),
            ),
            ("hide", vec!["hide"], Ok(Invocation::Command(Command::Hide))),
            ("snap", vec!["snap"], Ok(Invocation::Command(Command::Snap))),
            (
                "unsnap",
                vec!["unsnap"],
                Ok(Invocation::Command(Command::Unsnap)),
            ),
            (
                "toggle-snap",
                vec!["toggle-snap"],
                Ok(Invocation::Command(Command::ToggleSnap)),
            ),
            (
                "toggle-snap then argument",
                vec!["toggle-snap", "x"],
                Err(UsageError::CommandArgument {
                    command: "toggle-snap",
                    argument: "x".to_string(),
                }),
            ),
            ("flag then snap", vec!["--verbose", "snap"], unknown("snap")),
            ("show", vec!["show"], Ok(Invocation::Command(Command::Show))),
            (
                "toggle-lock",
                vec!["toggle-lock"],
                Ok(Invocation::Command(Command::ToggleLock)),
            ),
            (
                "toggle-hide",
                vec!["toggle-hide"],
                Ok(Invocation::Command(Command::ToggleHide)),
            ),
            (
                "opacity 50",
                vec!["opacity", "50"],
                Ok(Invocation::Command(Command::Opacity(50))),
            ),
            (
                "opacity 0",
                vec!["opacity", "0"],
                Ok(Invocation::Command(Command::Opacity(0))),
            ),
            (
                "opacity 100",
                vec!["opacity", "100"],
                Ok(Invocation::Command(Command::Opacity(100))),
            ),
            ("opacity missing", vec!["opacity"], missing("opacity")),
            ("opacity 101", vec!["opacity", "101"], bad_opacity("101")),
            ("opacity -1", vec!["opacity", "-1"], bad_opacity("-1")),
            ("opacity text", vec!["opacity", "abc"], bad_opacity("abc")),
            ("opacity 1000", vec!["opacity", "1000"], bad_opacity("1000")),
            (
                "opacity then argument",
                vec!["opacity", "50", "x"],
                Err(UsageError::CommandArgument {
                    command: "opacity",
                    argument: "x".to_string(),
                }),
            ),
            (
                "flag then opacity",
                vec!["--verbose", "opacity", "50"],
                unknown("opacity"),
            ),
            (
                "command then flag",
                vec!["hide", "--verbose"],
                Err(UsageError::CommandArgument {
                    command: "hide",
                    argument: "--verbose".to_string(),
                }),
            ),
            (
                "command twice",
                vec!["lock", "lock"],
                Err(UsageError::CommandArgument {
                    command: "lock",
                    argument: "lock".to_string(),
                }),
            ),
            (
                "flag then command",
                vec!["--verbose", "hide"],
                unknown("hide"),
            ),
            ("command in upper case", vec!["Lock"], unknown("Lock")),
            (
                "every flag",
                vec![
                    "--config",
                    "/tmp/c",
                    "--log",
                    "/tmp/l",
                    "--verbose",
                    "--seconds",
                    "6",
                    "--ignore-damage",
                ],
                ok(Some("/tmp/c"), Some("/tmp/l"), true, Some(6), true),
            ),
            (
                "config only",
                vec!["--config", "/tmp/c"],
                ok(Some("/tmp/c"), None, false, None, false),
            ),
            (
                "log only",
                vec!["--log", "/tmp/l"],
                ok(None, Some("/tmp/l"), false, None, false),
            ),
            (
                "verbose only",
                vec!["--verbose"],
                ok(None, None, true, None, false),
            ),
            (
                "ignore damage only",
                vec!["--ignore-damage"],
                ok(None, None, false, None, true),
            ),
            (
                "order does not matter",
                vec!["--ignore-damage", "--seconds", "6", "--verbose"],
                ok(None, None, true, Some(6), true),
            ),
            (
                "config value may look like a flag",
                vec!["--config", "--log"],
                ok(Some("--log"), None, false, None, false),
            ),
            (
                "seconds plus sign",
                vec!["--seconds", "+6"],
                ok(None, None, false, Some(6), false),
            ),
            (
                "seconds leading zero",
                vec!["--seconds", "06"],
                ok(None, None, false, Some(6), false),
            ),
            (
                "seconds min",
                vec!["--seconds", "1"],
                ok(None, None, false, Some(1), false),
            ),
            (
                "seconds max",
                vec!["--seconds", "86400"],
                ok(None, None, false, Some(86_400), false),
            ),
            ("seconds zero", vec!["--seconds", "0"], bad_secs("0")),
            (
                "seconds above max",
                vec!["--seconds", "86401"],
                bad_secs("86401"),
            ),
            ("seconds text", vec!["--seconds", "abc"], bad_secs("abc")),
            ("seconds negative", vec!["--seconds", "-1"], bad_secs("-1")),
            ("seconds missing", vec!["--seconds"], missing("--seconds")),
            ("config missing", vec!["--config"], missing("--config")),
            ("log missing", vec!["--log"], missing("--log")),
            ("unknown flag", vec!["--bogus"], unknown("--bogus")),
            ("help", vec!["--help"], unknown("--help")),
            ("address", vec!["--address", "0x1"], unknown("--address")),
            ("width", vec!["--width", "480"], unknown("--width")),
            ("equals form", vec!["--config=x"], unknown("--config=x")),
            ("positional", vec!["x"], unknown("x")),
            (
                "verbose takes no value",
                vec!["--verbose", "1"],
                unknown("1"),
            ),
            (
                "config twice",
                vec!["--config", "a", "--config", "b"],
                dup("--config"),
            ),
            ("log twice", vec!["--log", "a", "--log", "b"], dup("--log")),
            (
                "seconds twice",
                vec!["--seconds", "1", "--seconds", "2"],
                dup("--seconds"),
            ),
            (
                "verbose twice",
                vec!["--verbose", "--verbose"],
                dup("--verbose"),
            ),
            (
                "ignore damage twice",
                vec!["--ignore-damage", "--ignore-damage"],
                dup("--ignore-damage"),
            ),
        ];
        for (name, input, want) in cases {
            assert_eq!(parse(args(&input)), want, "{name}");
        }
    }

    #[test]
    fn parse_os_cases() {
        let os = |v: &[u8]| OsString::from_vec(v.to_vec());
        let cases: Vec<(&str, Vec<OsString>, Result<Invocation, UsageError>)> = vec![
            ("empty", vec![], ok(None, None, false, None, false)),
            (
                "utf8",
                vec![os(b"--config"), os(b"/tmp/c"), os(b"--seconds"), os(b"6")],
                ok(Some("/tmp/c"), None, false, Some(6), false),
            ),
            (
                "command",
                vec![os(b"toggle-hide")],
                Ok(Invocation::Command(Command::ToggleHide)),
            ),
            (
                "first argument not utf8",
                vec![os(b"\xff")],
                Err(UsageError::NotUtf8(1)),
            ),
            (
                "third argument not utf8",
                vec![os(b"--verbose"), os(b"--config"), os(b"/tmp/\xfe")],
                Err(UsageError::NotUtf8(3)),
            ),
        ];
        for (name, input, want) in cases {
            assert_eq!(parse_os(input), want, "{name}");
        }
    }

    #[test]
    fn usage_error_display_cases() {
        let cases = [
            (
                UsageError::Unknown("--x".into()),
                "unknown argument \"--x\"",
            ),
            (UsageError::MissingValue("--log"), "--log needs a value"),
            (UsageError::Duplicate("--log"), "--log given more than once"),
            (
                UsageError::InvalidSeconds("0".into()),
                "invalid --seconds \"0\": want 1 to 86400",
            ),
            (UsageError::NotUtf8(2), "argument 2 is not UTF-8"),
            (
                UsageError::InvalidOpacity("101".into()),
                "invalid opacity \"101\": want 0 to 100",
            ),
            (
                UsageError::CommandArgument {
                    command: "hide",
                    argument: "--verbose".into(),
                },
                "hide: unexpected argument \"--verbose\"",
            ),
        ];
        for (err, want) in cases {
            assert_eq!(err.to_string(), want);
        }
    }
}
