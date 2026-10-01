use std::ffi::OsString;
use std::fmt;

use crate::hypr;

const MAX_SECONDS: u64 = 86_400;
const MAX_WIDTH: u32 = 2544;
const DEFAULT_WIDTH: u32 = 480;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub address: Option<u64>,
    pub seconds: Option<u64>,
    pub width: u32,
    pub ignore_damage: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageError {
    Unknown(String),
    MissingValue(&'static str),
    Duplicate(&'static str),
    InvalidAddress(String),
    InvalidSeconds(String),
    InvalidWidth(String),
    NotUtf8(usize),
}

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UsageError::Unknown(a) => write!(f, "unknown argument {a:?}"),
            UsageError::MissingValue(flag) => write!(f, "{flag} needs a value"),
            UsageError::Duplicate(flag) => write!(f, "{flag} given more than once"),
            UsageError::InvalidAddress(v) => write!(f, "invalid --address {v:?}"),
            UsageError::InvalidSeconds(v) => {
                write!(f, "invalid --seconds {v:?}: want 1 to {MAX_SECONDS}")
            }
            UsageError::InvalidWidth(v) => {
                write!(f, "invalid --width {v:?}: want 1 to {MAX_WIDTH}")
            }
            UsageError::NotUtf8(n) => write!(f, "argument {n} is not UTF-8"),
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
pub fn parse_os(args: impl IntoIterator<Item = OsString>) -> Result<Args, UsageError> {
    let args = args
        .into_iter()
        .enumerate()
        .map(|(i, a)| a.into_string().map_err(|_| UsageError::NotUtf8(i + 1)))
        .collect::<Result<Vec<_>, _>>()?;
    parse(args)
}

/// Parses the arguments after argv[0]. Does no I/O.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, UsageError> {
    let mut it = args.into_iter();
    let mut address = None;
    let mut seconds = None;
    let mut width = None;
    let mut ignore_damage = false;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--address" => {
                let v = it.next().ok_or(UsageError::MissingValue("--address"))?;
                let a = hypr::parse_address(&v).map_err(|_| UsageError::InvalidAddress(v))?;
                once(&mut address, "--address", a)?;
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
            "--width" => {
                let v = it.next().ok_or(UsageError::MissingValue("--width"))?;
                let n = v
                    .parse::<u64>()
                    .ok()
                    .and_then(|n| u32::try_from(n).ok())
                    .filter(|n| (1..=MAX_WIDTH).contains(n))
                    .ok_or(UsageError::InvalidWidth(v))?;
                once(&mut width, "--width", n)?;
            }
            "--ignore-damage" => {
                if ignore_damage {
                    return Err(UsageError::Duplicate("--ignore-damage"));
                }
                ignore_damage = true;
            }
            _ => return Err(UsageError::Unknown(arg)),
        }
    }
    Ok(Args {
        address,
        seconds,
        width: width.unwrap_or(DEFAULT_WIDTH),
        ignore_damage,
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;

    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn ok(
        address: Option<u64>,
        seconds: Option<u64>,
        width: u32,
        ignore_damage: bool,
    ) -> Result<Args, UsageError> {
        Ok(Args {
            address,
            seconds,
            width,
            ignore_damage,
        })
    }

    #[test]
    fn parse_cases() {
        let unknown = |s: &str| Err(UsageError::Unknown(s.to_string()));
        let missing = |s: &'static str| Err(UsageError::MissingValue(s));
        let dup = |s: &'static str| Err(UsageError::Duplicate(s));
        let bad_addr = |s: &str| Err(UsageError::InvalidAddress(s.to_string()));
        let bad_secs = |s: &str| Err(UsageError::InvalidSeconds(s.to_string()));
        let bad_width = |s: &str| Err(UsageError::InvalidWidth(s.to_string()));
        let cases: Vec<(&str, Vec<&str>, Result<Args, UsageError>)> = vec![
            ("defaults", vec![], ok(None, None, 480, false)),
            (
                "all value flags",
                vec![
                    "--address",
                    "0x5608ab929d00",
                    "--seconds",
                    "6",
                    "--width",
                    "480",
                ],
                ok(Some(0x5608ab929d00), Some(6), 480, false),
            ),
            (
                "ignore damage",
                vec!["--ignore-damage"],
                ok(None, None, 480, true),
            ),
            (
                "seconds and ignore damage",
                vec!["--seconds", "6", "--ignore-damage"],
                ok(None, Some(6), 480, true),
            ),
            (
                "seconds max",
                vec!["--seconds", "86400"],
                ok(None, Some(86400), 480, false),
            ),
            (
                "width max",
                vec!["--width", "2544"],
                ok(None, None, 2544, false),
            ),
            ("width min", vec!["--width", "1"], ok(None, None, 1, false)),
            (
                "leading plus",
                vec!["--seconds", "+6"],
                ok(None, Some(6), 480, false),
            ),
            (
                "leading zeros",
                vec!["--width", "0480"],
                ok(None, None, 480, false),
            ),
            ("unknown flag", vec!["--bogus"], unknown("--bogus")),
            ("help", vec!["--help"], unknown("--help")),
            ("positional", vec!["x"], unknown("x")),
            ("missing value", vec!["--seconds"], missing("--seconds")),
            (
                "missing address value",
                vec!["--address"],
                missing("--address"),
            ),
            ("missing width value", vec!["--width"], missing("--width")),
            ("equals form", vec!["--width=480"], unknown("--width=480")),
            (
                "address without 0x",
                vec!["--address", "5608ab929d00"],
                bad_addr("5608ab929d00"),
            ),
            (
                "address 17 digits",
                vec!["--address", "0x10000000000000000"],
                bad_addr("0x10000000000000000"),
            ),
            (
                "address non-hex",
                vec!["--address", "0xzz"],
                bad_addr("0xzz"),
            ),
            ("seconds zero", vec!["--seconds", "0"], bad_secs("0")),
            (
                "seconds too large",
                vec!["--seconds", "86401"],
                bad_secs("86401"),
            ),
            (
                "seconds not a number",
                vec!["--seconds", "abc"],
                bad_secs("abc"),
            ),
            ("width zero", vec!["--width", "0"], bad_width("0")),
            (
                "width too large",
                vec!["--width", "2545"],
                bad_width("2545"),
            ),
            ("width negative", vec!["--width", "-1"], bad_width("-1")),
            (
                "duplicate seconds",
                vec!["--seconds", "1", "--seconds", "2"],
                dup("--seconds"),
            ),
            (
                "duplicate ignore damage",
                vec!["--ignore-damage", "--ignore-damage"],
                dup("--ignore-damage"),
            ),
            (
                "ignore damage equals",
                vec!["--ignore-damage=1"],
                unknown("--ignore-damage=1"),
            ),
            (
                "ignore damage positional",
                vec!["--ignore-damage", "1"],
                unknown("1"),
            ),
        ];
        for (name, input, want) in cases {
            assert_eq!(parse(args(&input)), want, "{name}");
        }
    }

    #[test]
    fn parse_os_cases() {
        let bad = || OsString::from_vec(vec![0xff]);
        let cases: Vec<(&str, Vec<OsString>, Result<Args, UsageError>)> = vec![
            (
                "valid",
                vec!["--seconds".into(), "6".into()],
                ok(None, Some(6), 480, false),
            ),
            ("non-utf8 flag", vec![bad()], Err(UsageError::NotUtf8(1))),
            (
                "non-utf8 address value",
                vec!["--address".into(), bad()],
                Err(UsageError::NotUtf8(2)),
            ),
        ];
        for (name, input, want) in cases {
            assert_eq!(parse_os(input), want, "{name}");
        }
    }
}
