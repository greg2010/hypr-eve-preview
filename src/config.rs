use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The monitor new thumbnails start on. `None` means the focused monitor at start.
    pub output: Option<String>,
    pub thumbnail: Thumbnail,
    pub placement: Placement,
    pub border: Border,
    pub label: Label,
    pub resize: Resize,
}

/// Thumbnail sizing in pixels. `opacity` is a percentage, 0 to `MAX_OPACITY`, default 100.
/// `snap_distance` is in logical pixels, default 10; 0 disables snapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub width: u32,
    pub min_width: u32,
    pub max_width: u32,
    pub opacity: u32,
    pub snap_distance: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub x: u32,
    pub y: u32,
    pub gap: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Border {
    pub width: u32,
    pub color: Color,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub font: String,
    pub font_file: Option<PathBuf>,
    pub size: u32,
    pub color: Color,
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resize {
    pub step: u32,
}

#[derive(Debug)]
pub enum ConfigError {
    Read(io::Error),
    Syntax(String),
    Key { key: String, reason: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Read(e) => write!(f, "{e}"),
            ConfigError::Syntax(text) => f.write_str(text),
            ConfigError::Key { key, reason } => write!(f, "{key}: {reason}"),
        }
    }
}

impl std::error::Error for ConfigError {}

const DEFAULT_COLOR: Color = Color {
    r: 0x40,
    g: 0xFF,
    b: 0x00,
    a: 0xFF,
};

impl Default for Config {
    fn default() -> Self {
        Config {
            output: None,
            thumbnail: Thumbnail {
                width: 480,
                min_width: 160,
                max_width: 1280,
                opacity: 100,
                snap_distance: 10,
            },
            placement: Placement { x: 8, y: 8, gap: 8 },
            border: Border {
                width: 2,
                color: DEFAULT_COLOR,
            },
            label: Label {
                font: "Noto Sans Mono".to_string(),
                font_file: None,
                size: 15,
                color: DEFAULT_COLOR,
                x: 6,
                y: 6,
            },
            resize: Resize { step: 32 },
        }
    }
}

const U32_REASON: &str = "must be between 0 and 4294967295";
const COLOR_REASON: &str = "must be #RRGGBB or #RRGGBBAA";

struct Walked {
    config: Config,
    border_color: Option<String>,
    label_color: Option<String>,
}

enum Setter {
    Int(fn(&mut Walked, u32)),
    Str(fn(&mut Walked, String)),
}

type Table = (&'static str, &'static [(&'static str, Setter)]);

const TABLES: &[Table] = &[
    (
        "thumbnail",
        &[
            ("width", Setter::Int(|w, n| w.config.thumbnail.width = n)),
            (
                "min_width",
                Setter::Int(|w, n| w.config.thumbnail.min_width = n),
            ),
            (
                "max_width",
                Setter::Int(|w, n| w.config.thumbnail.max_width = n),
            ),
            (
                "opacity",
                Setter::Int(|w, n| w.config.thumbnail.opacity = n),
            ),
            (
                "snap_distance",
                Setter::Int(|w, n| w.config.thumbnail.snap_distance = n),
            ),
        ],
    ),
    (
        "placement",
        &[
            ("x", Setter::Int(|w, n| w.config.placement.x = n)),
            ("y", Setter::Int(|w, n| w.config.placement.y = n)),
            ("gap", Setter::Int(|w, n| w.config.placement.gap = n)),
        ],
    ),
    (
        "border",
        &[
            ("width", Setter::Int(|w, n| w.config.border.width = n)),
            ("color", Setter::Str(|w, s| w.border_color = Some(s))),
        ],
    ),
    (
        "label",
        &[
            ("font", Setter::Str(|w, s| w.config.label.font = s)),
            (
                "font_file",
                Setter::Str(|w, s| w.config.label.font_file = Some(PathBuf::from(s))),
            ),
            ("size", Setter::Int(|w, n| w.config.label.size = n)),
            ("color", Setter::Str(|w, s| w.label_color = Some(s))),
            ("x", Setter::Int(|w, n| w.config.label.x = n)),
            ("y", Setter::Int(|w, n| w.config.label.y = n)),
        ],
    ),
    (
        "resize",
        &[("step", Setter::Int(|w, n| w.config.resize.step = n))],
    ),
];

fn key_error(key: impl Into<String>, reason: impl Into<String>) -> ConfigError {
    ConfigError::Key {
        key: key.into(),
        reason: reason.into(),
    }
}

fn walk_table(
    name: &str,
    keys: &[(&str, Setter)],
    table: &toml::Table,
    walked: &mut Walked,
) -> Result<(), ConfigError> {
    for (key, value) in table {
        let dotted = format!("{name}.{key}");
        let Some((_, setter)) = keys.iter().find(|(k, _)| k == key) else {
            return Err(key_error(dotted, "unknown key"));
        };
        match setter {
            Setter::Int(set) => {
                let n = value
                    .as_integer()
                    .ok_or_else(|| key_error(&dotted, "expected integer"))?;
                let n = u32::try_from(n).map_err(|_| key_error(&dotted, U32_REASON))?;
                set(walked, n);
            }
            Setter::Str(set) => {
                let text = value
                    .as_str()
                    .ok_or_else(|| key_error(&dotted, "expected string"))?;
                set(walked, text.to_string());
            }
        }
    }
    Ok(())
}

fn walk(table: &toml::Table) -> Result<Walked, ConfigError> {
    let mut walked = Walked {
        config: Config::default(),
        border_color: None,
        label_color: None,
    };
    for (key, value) in table {
        if key == "output" {
            let text = value
                .as_str()
                .ok_or_else(|| key_error("output", "expected string"))?;
            walked.config.output = Some(text.to_string());
        } else if let Some((name, keys)) = TABLES.iter().find(|(n, _)| n == key) {
            let inner = value
                .as_table()
                .ok_or_else(|| key_error(*name, "expected table"))?;
            walk_table(name, keys, inner, &mut walked)?;
        } else {
            return Err(key_error(key, "unknown key"));
        }
    }
    Ok(walked)
}

fn even(key: &str, value: u32) -> Result<(), ConfigError> {
    if value.is_multiple_of(2) {
        Ok(())
    } else {
        Err(key_error(key, "must be even"))
    }
}

fn at_least(key: &str, value: u32, min: u32) -> Result<(), ConfigError> {
    if value >= min {
        Ok(())
    } else {
        Err(key_error(key, format!("must be at least {min}")))
    }
}

/// The one opacity bound and percent scale, shared so that every opacity check and
/// conversion agrees.
pub const MAX_OPACITY: u32 = 100;

fn between(key: &str, value: u32, min: u32, max: u32) -> Result<(), ConfigError> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(key_error(key, format!("must be between {min} and {max}")))
    }
}

fn non_empty(key: &str, value: &str) -> Result<(), ConfigError> {
    if value.is_empty() {
        Err(key_error(key, "must not be empty"))
    } else {
        Ok(())
    }
}

fn color(key: &str, text: Option<String>, default: Color) -> Result<Color, ConfigError> {
    match text {
        Some(text) => parse_color(&text).map_err(|reason| key_error(key, reason)),
        None => Ok(default),
    }
}

fn validate(walked: Walked) -> Result<Config, ConfigError> {
    let Walked {
        mut config,
        border_color,
        label_color,
    } = walked;
    let t = &config.thumbnail;
    if let Some(output) = &config.output {
        non_empty("output", output)?;
    }
    even("thumbnail.width", t.width)?;
    if t.width < t.min_width {
        return Err(key_error(
            "thumbnail.width",
            format!("must be at least thumbnail.min_width ({})", t.min_width),
        ));
    }
    if t.width > t.max_width {
        return Err(key_error(
            "thumbnail.width",
            format!("must be at most thumbnail.max_width ({})", t.max_width),
        ));
    }
    even("thumbnail.min_width", t.min_width)?;
    at_least("thumbnail.min_width", t.min_width, 2)?;
    even("thumbnail.max_width", t.max_width)?;
    between("thumbnail.opacity", t.opacity, 0, MAX_OPACITY)?;
    even("placement.x", config.placement.x)?;
    even("placement.y", config.placement.y)?;
    even("placement.gap", config.placement.gap)?;
    between("border.width", config.border.width, 0, 64)?;
    config.border.color = color("border.color", border_color, config.border.color)?;
    non_empty("label.font", &config.label.font)?;
    between("label.size", config.label.size, 1, 256)?;
    config.label.color = color("label.color", label_color, config.label.color)?;
    even("resize.step", config.resize.step)?;
    at_least("resize.step", config.resize.step, 2)?;
    Ok(config)
}

/// Parses a config document and checks every validation rule: key names, value types, ranges
/// and ordering relations. Missing keys keep their defaults.
pub fn parse(text: &str) -> Result<Config, ConfigError> {
    let table =
        toml::from_str::<toml::Table>(text).map_err(|e| ConfigError::Syntax(e.to_string()))?;
    validate(walk(&table)?)
}

/// Parses `#RRGGBB` or `#RRGGBBAA`, hex digits in either case. The error text is the reason
/// that `ConfigError::Key` carries.
pub fn parse_color(s: &str) -> Result<Color, String> {
    let digits = s
        .strip_prefix('#')
        .filter(|d| matches!(d.len(), 6 | 8) && d.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| COLOR_REASON.to_string())?;
    let byte =
        |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).map_err(|_| COLOR_REASON.to_string());
    Ok(Color {
        r: byte(0)?,
        g: byte(2)?,
        b: byte(4)?,
        a: if digits.len() == 8 { byte(6)? } else { 0xFF },
    })
}

/// Returns `var` when it is set, non-empty and absolute, else `home` joined with `fallback`
/// when `home` is set and non-empty.
pub fn xdg_base(var: Option<&OsStr>, home: Option<&OsStr>, fallback: &str) -> Option<PathBuf> {
    if let Some(var) = var.filter(|v| !v.is_empty() && Path::new(v).is_absolute()) {
        return Some(PathBuf::from(var));
    }
    home.filter(|h| !h.is_empty())
        .map(|h| Path::new(h).join(fallback))
}

pub const APP_DIR: &str = "hypr-eve-preview";

pub fn default_path(config_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    xdg_base(config_home, home, ".config").map(|base| base.join(APP_DIR).join("config.toml"))
}

/// Reads `explicit` when given (it must exist), else `default`, where a missing file means
/// the defaults. The second value is the path that was read.
pub fn load(
    explicit: Option<&Path>,
    default: &Path,
) -> Result<(Config, Option<PathBuf>), ConfigError> {
    let (path, required) = match explicit {
        Some(path) => (path, true),
        None => (default, false),
    };
    match std::fs::read_to_string(path) {
        Ok(text) => Ok((parse(&text)?, Some(path.to_path_buf()))),
        Err(e) if !required && e.kind() == io::ErrorKind::NotFound => Ok((Config::default(), None)),
        Err(e) => Err(ConfigError::Read(e)),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::testutil::TempDir;

    const EXAMPLE: &str = r##"output = "DP-3"

[thumbnail]
width = 480
min_width = 160
max_width = 1280
opacity = 100
snap_distance = 10

[placement]
x = 8
y = 8
gap = 8

[border]
width = 2
color = "#40FF00"

[label]
font = "Noto Sans Mono"
# font_file = "/usr/share/fonts/noto/NotoSansMono-Regular.ttf"
size = 15
color = "#40FF00"
x = 6
y = 6

[resize]
step = 32
"##;

    const EVERY_KEY: &str = r##"output = "HDMI-A-1"

[thumbnail]
width = 400
min_width = 100
max_width = 800
opacity = 50
snap_distance = 0

[placement]
x = 10
y = 12
gap = 4

[border]
width = 3
color = "#11223344"

[label]
font = "Mono"
font_file = "/tmp/font.ttf"
size = 20
color = "#aabbcc"
x = 2
y = 4

[resize]
step = 16
"##;

    #[derive(Debug, PartialEq, Eq)]
    enum Shape {
        Ok(Config),
        Key(String, String),
        Syntax(String),
        Read(io::ErrorKind),
    }

    fn shape(result: Result<Config, ConfigError>) -> Shape {
        match result {
            Ok(c) => Shape::Ok(c),
            Err(ConfigError::Key { key, reason }) => Shape::Key(key, reason),
            Err(ConfigError::Syntax(text)) => Shape::Syntax(text),
            Err(ConfigError::Read(e)) => Shape::Read(e.kind()),
        }
    }

    fn key(key: &str, reason: &str) -> Shape {
        Shape::Key(key.to_string(), reason.to_string())
    }

    type LoadCase<'a> = (&'a str, Option<&'a Path>, &'a Path, Shape, Option<PathBuf>);

    #[test]
    fn parse_cases() {
        let every_key = Config {
            output: Some("HDMI-A-1".to_string()),
            thumbnail: Thumbnail {
                width: 400,
                min_width: 100,
                max_width: 800,
                opacity: 50,
                snap_distance: 0,
            },
            placement: Placement {
                x: 10,
                y: 12,
                gap: 4,
            },
            border: Border {
                width: 3,
                color: Color {
                    r: 0x11,
                    g: 0x22,
                    b: 0x33,
                    a: 0x44,
                },
            },
            label: Label {
                font: "Mono".to_string(),
                font_file: Some(PathBuf::from("/tmp/font.ttf")),
                size: 20,
                color: Color {
                    r: 0xaa,
                    g: 0xbb,
                    b: 0xcc,
                    a: 0xff,
                },
                x: 2,
                y: 4,
            },
            resize: Resize { step: 16 },
        };
        let defaults = Config {
            output: None,
            thumbnail: Thumbnail {
                width: 480,
                min_width: 160,
                max_width: 1280,
                opacity: 100,
                snap_distance: 10,
            },
            placement: Placement { x: 8, y: 8, gap: 8 },
            border: Border {
                width: 2,
                color: Color {
                    r: 0x40,
                    g: 0xFF,
                    b: 0x00,
                    a: 0xFF,
                },
            },
            label: Label {
                font: "Noto Sans Mono".to_string(),
                font_file: None,
                size: 15,
                color: Color {
                    r: 0x40,
                    g: 0xFF,
                    b: 0x00,
                    a: 0xFF,
                },
                x: 6,
                y: 6,
            },
            resize: Resize { step: 32 },
        };
        let cases = [
            ("empty", "", defaults.clone()),
            (
                "example",
                EXAMPLE,
                Config {
                    output: Some("DP-3".to_string()),
                    ..defaults
                },
            ),
            ("every key", EVERY_KEY, every_key),
            (
                "one key keeps the rest",
                "[resize]\nstep = 2\n",
                Config {
                    resize: Resize { step: 2 },
                    ..Config::default()
                },
            ),
            (
                "bounds at the limits",
                "[border]\nwidth = 64\n[label]\nsize = 256\n",
                Config {
                    border: Border {
                        width: 64,
                        color: DEFAULT_COLOR,
                    },
                    label: Label {
                        size: 256,
                        ..Config::default().label
                    },
                    ..Config::default()
                },
            ),
            (
                "zero border width",
                "[border]\nwidth = 0\n",
                Config {
                    border: Border {
                        width: 0,
                        color: DEFAULT_COLOR,
                    },
                    ..Config::default()
                },
            ),
        ];
        for (name, text, want) in cases {
            assert_eq!(shape(parse(text)), Shape::Ok(want), "{name}");
        }
    }

    #[test]
    fn error_cases() {
        let cases = [
            (
                "syntax error",
                "output = \"DP-3\"\n[thumbnail",
                Shape::Syntax(
                    "TOML parse error at line 2, column 11\n  |\n2 | [thumbnail\n  |           ^\n\
                     unclosed table, expected `]`\n"
                        .to_string(),
                ),
            ),
            (
                "unknown key",
                "[thumbnail]\nhieght = 1\n",
                key("thumbnail.hieght", "unknown key"),
            ),
            (
                "unknown top key",
                "colour = 1\n",
                key("colour", "unknown key"),
            ),
            (
                "table not a table",
                "thumbnail = 5\n",
                key("thumbnail", "expected table"),
            ),
            (
                "integer as string",
                "[thumbnail]\nwidth = \"480\"\n",
                key("thumbnail.width", "expected integer"),
            ),
            (
                "output as integer",
                "output = 3\n",
                key("output", "expected string"),
            ),
            (
                "color as integer",
                "[border]\ncolor = 3\n",
                key("border.color", "expected string"),
            ),
            (
                "negative",
                "[placement]\nx = -2\n",
                key("placement.x", "must be between 0 and 4294967295"),
            ),
            (
                "above u32",
                "[placement]\ny = 4294967296\n",
                key("placement.y", "must be between 0 and 4294967295"),
            ),
            (
                "odd width",
                "[thumbnail]\nwidth = 481\n",
                key("thumbnail.width", "must be even"),
            ),
            (
                "width below min, first failing row",
                "[thumbnail]\nmin_width = 600\nmax_width = 400\n",
                key(
                    "thumbnail.width",
                    "must be at least thumbnail.min_width (600)",
                ),
            ),
            (
                "width above max",
                "[thumbnail]\nwidth = 2000\n",
                key(
                    "thumbnail.width",
                    "must be at most thumbnail.max_width (1280)",
                ),
            ),
            (
                "odd min width",
                "[thumbnail]\nmin_width = 101\n",
                key("thumbnail.min_width", "must be even"),
            ),
            (
                "min width zero",
                "[thumbnail]\nwidth = 0\nmin_width = 0\n",
                key("thumbnail.min_width", "must be at least 2"),
            ),
            (
                "odd max width",
                "[thumbnail]\nmax_width = 1281\n",
                key("thumbnail.max_width", "must be even"),
            ),
            (
                "opacity above 100",
                "[thumbnail]\nopacity = 101\n",
                key("thumbnail.opacity", "must be between 0 and 100"),
            ),
            (
                "opacity as float",
                "[thumbnail]\nopacity = 0.5\n",
                key("thumbnail.opacity", "expected integer"),
            ),
            (
                "negative snap distance",
                "[thumbnail]\nsnap_distance = -1\n",
                key(
                    "thumbnail.snap_distance",
                    "must be between 0 and 4294967295",
                ),
            ),
            (
                "odd max width is reported before opacity",
                "[thumbnail]\nmax_width = 1281\nopacity = 101\n",
                key("thumbnail.max_width", "must be even"),
            ),
            (
                "odd placement x",
                "[placement]\nx = 7\n",
                key("placement.x", "must be even"),
            ),
            (
                "odd placement y",
                "[placement]\ny = 7\n",
                key("placement.y", "must be even"),
            ),
            (
                "odd placement gap",
                "[placement]\ngap = 3\n",
                key("placement.gap", "must be even"),
            ),
            (
                "border width",
                "[border]\nwidth = 65\n",
                key("border.width", "must be between 0 and 64"),
            ),
            (
                "short color",
                "[border]\ncolor = \"#40FF0\"\n",
                key("border.color", "must be #RRGGBB or #RRGGBBAA"),
            ),
            (
                "color without hash",
                "[label]\ncolor = \"40FF00\"\n",
                key("label.color", "must be #RRGGBB or #RRGGBBAA"),
            ),
            (
                "color not hex",
                "[border]\ncolor = \"#GG0000\"\n",
                key("border.color", "must be #RRGGBB or #RRGGBBAA"),
            ),
            (
                "empty output",
                "output = \"\"\n",
                key("output", "must not be empty"),
            ),
            (
                "empty font",
                "[label]\nfont = \"\"\n",
                key("label.font", "must not be empty"),
            ),
            (
                "label size zero",
                "[label]\nsize = 0\n",
                key("label.size", "must be between 1 and 256"),
            ),
            (
                "label size above",
                "[label]\nsize = 257\n",
                key("label.size", "must be between 1 and 256"),
            ),
            (
                "odd step",
                "[resize]\nstep = 3\n",
                key("resize.step", "must be even"),
            ),
            (
                "step zero",
                "[resize]\nstep = 0\n",
                key("resize.step", "must be at least 2"),
            ),
            (
                "walk error before rule error",
                "[thumbnail]\nwidth = 481\n[resize]\nbogus = 1\n",
                key("resize.bogus", "unknown key"),
            ),
        ];
        for (name, text, want) in cases {
            assert_eq!(shape(parse(text)), want, "{name}");
        }
    }

    #[test]
    fn display_cases() {
        let cases = [
            (
                ConfigError::Key {
                    key: "thumbnail.width".to_string(),
                    reason: "must be even".to_string(),
                },
                "thumbnail.width: must be even".to_string(),
            ),
            (ConfigError::Syntax("bad".to_string()), "bad".to_string()),
            (
                ConfigError::Read(io::Error::new(io::ErrorKind::NotFound, "gone")),
                "gone".to_string(),
            ),
        ];
        for (err, want) in cases {
            assert_eq!(err.to_string(), want);
        }
    }

    #[test]
    fn color_cases() {
        let color = |r, g, b, a| Ok(Color { r, g, b, a });
        let bad = || Err("must be #RRGGBB or #RRGGBBAA".to_string());
        let cases = [
            ("rgb", "#40FF00", color(0x40, 0xFF, 0x00, 0xFF)),
            (
                "rgba lower case",
                "#40ff0080",
                color(0x40, 0xFF, 0x00, 0x80),
            ),
            ("mixed case", "#aBcDeF", color(0xAB, 0xCD, 0xEF, 0xFF)),
            ("black transparent", "#00000000", color(0, 0, 0, 0)),
            ("empty", "", bad()),
            ("hash only", "#", bad()),
            ("five digits", "#40FF0", bad()),
            ("seven digits", "#40FF008", bad()),
            ("nine digits", "#40FF00801", bad()),
            ("no hash", "40FF00", bad()),
            ("not hex", "#GG0000", bad()),
            ("sign", "#+0FF00", bad()),
            ("multibyte", "#40FF0\u{e9}", bad()),
        ];
        for (name, text, want) in cases {
            assert_eq!(parse_color(text), want, "{name}");
        }
    }

    #[test]
    fn xdg_base_cases() {
        let os = |s: &'static str| Some(OsStr::new(s));
        let path = |s: &str| Some(PathBuf::from(s));
        let cases = [
            ("var absolute", os("/x"), os("/h"), ".config", path("/x")),
            ("var empty", os(""), os("/h"), ".config", path("/h/.config")),
            (
                "var relative",
                os("rel"),
                os("/h"),
                ".config",
                path("/h/.config"),
            ),
            ("var unset", None, os("/h"), ".config", path("/h/.config")),
            ("var set, home unset", os("/x"), None, ".config", path("/x")),
            ("relative var, home unset", os("rel"), None, ".config", None),
            ("home empty", None, os(""), ".config", None),
            ("both unset", None, None, ".config", None),
            (
                "nested fallback",
                None,
                os("/h"),
                ".local/state",
                path("/h/.local/state"),
            ),
        ];
        for (name, var, home, fallback, want) in cases {
            assert_eq!(xdg_base(var, home, fallback), want, "{name}");
        }
    }

    #[test]
    fn default_path_cases() {
        let os = |s: &'static str| Some(OsStr::new(s));
        let path = |s: &str| Some(PathBuf::from(s));
        let cases = [
            (os("/x"), os("/h"), path("/x/hypr-eve-preview/config.toml")),
            (
                None,
                os("/h"),
                path("/h/.config/hypr-eve-preview/config.toml"),
            ),
            (
                os("rel"),
                os("/h"),
                path("/h/.config/hypr-eve-preview/config.toml"),
            ),
            (None, None, None),
        ];
        for (config_home, home, want) in cases {
            assert_eq!(default_path(config_home, home), want);
        }
    }

    #[test]
    fn load_cases() {
        let temp = TempDir::new("config-load");
        let dir = temp.path().to_path_buf();
        let explicit = dir.join("explicit.toml");
        let default = dir.join("default.toml");
        let missing = dir.join("missing.toml");
        let broken = dir.join("broken.toml");
        std::fs::write(&explicit, "[resize]\nstep = 2\n").expect("write explicit");
        std::fs::write(&default, "output = \"HDMI-A-1\"\n").expect("write default");
        std::fs::write(&broken, "output = 1\n").expect("write broken");
        let stepped = Config {
            resize: Resize { step: 2 },
            ..Config::default()
        };
        let named = Config {
            output: Some("HDMI-A-1".to_string()),
            ..Config::default()
        };
        let cases: Vec<LoadCase> = vec![
            (
                "missing default",
                None,
                &missing,
                Shape::Ok(Config::default()),
                None,
            ),
            (
                "present default",
                None,
                &default,
                Shape::Ok(named),
                Some(default.clone()),
            ),
            (
                "explicit wins",
                Some(&explicit),
                &default,
                Shape::Ok(stepped),
                Some(explicit.clone()),
            ),
            (
                "missing explicit",
                Some(&missing),
                &default,
                Shape::Read(io::ErrorKind::NotFound),
                None,
            ),
            (
                "default is a directory",
                None,
                &dir,
                Shape::Read(io::ErrorKind::IsADirectory),
                None,
            ),
            (
                "broken explicit",
                Some(&broken),
                &default,
                key("output", "expected string"),
                None,
            ),
        ];
        for (name, explicit, default, want, want_path) in cases {
            let (got, got_path) = match load(explicit, default) {
                Ok((config, path)) => (Shape::Ok(config), path),
                Err(e) => (shape(Err(e)), None),
            };
            assert_eq!((got, got_path), (want, want_path), "{name}");
        }
    }
}
