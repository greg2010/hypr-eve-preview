use std::fmt;

use crate::config::MAX_OPACITY;

/// A request the control socket accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Lock,
    Unlock,
    Hide,
    Show,
    ToggleLock,
    ToggleHide,
    Snap,
    Unsnap,
    ToggleSnap,
    /// The base thumbnail opacity in percent, 0 to `MAX_OPACITY`.
    Opacity(u32),
}

/// The word that starts the value command, `Command::Opacity`; `parse` recognises it, not
/// `from_word`.
pub const OPACITY_WORD: &str = "opacity";

/// The commands that take no value, in usage order.
pub const COMMANDS: [Command; 9] = [
    Command::Lock,
    Command::Unlock,
    Command::Hide,
    Command::Show,
    Command::ToggleLock,
    Command::ToggleHide,
    Command::Snap,
    Command::Unsnap,
    Command::ToggleSnap,
];

impl Command {
    /// The word that names the command on the socket and the command line.
    pub fn word(self) -> &'static str {
        match self {
            Command::Lock => "lock",
            Command::Unlock => "unlock",
            Command::Hide => "hide",
            Command::Show => "show",
            Command::ToggleLock => "toggle-lock",
            Command::ToggleHide => "toggle-hide",
            Command::Snap => "snap",
            Command::Unsnap => "unsnap",
            Command::ToggleSnap => "toggle-snap",
            Command::Opacity(_) => OPACITY_WORD,
        }
    }

    /// The request line without its terminator: the word, then the value for `Opacity`.
    pub fn line(self) -> String {
        match self {
            Command::Opacity(n) => format!("{OPACITY_WORD} {n}"),
            _ => self.word().to_string(),
        }
    }

    /// The only mapping from a request line to a command. `opacity` takes one space and then
    /// its value.
    pub fn parse(line: &[u8]) -> Result<Command, Refusal> {
        if line.is_empty() {
            return Err(Refusal::Empty);
        }
        if let Some(command) = Command::from_word(line) {
            return Ok(command);
        }
        match line
            .strip_prefix(OPACITY_WORD.as_bytes())
            .and_then(|rest| rest.strip_prefix(b" "))
        {
            Some(value) => Command::opacity_value(value)
                .map(Command::Opacity)
                .ok_or(Refusal::BadValue),
            None => Err(Refusal::Unknown),
        }
    }

    /// One to three ASCII digits and nothing else, worth at most `MAX_OPACITY`.
    pub fn opacity_value(text: &[u8]) -> Option<u32> {
        if text.is_empty() || text.len() > 3 || !text.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let value = text
            .iter()
            .fold(0, |acc, digit| acc * 10 + u32::from(digit - b'0'));
        (value <= MAX_OPACITY).then_some(value)
    }

    /// The fieldless command in `COMMANDS` that a bare word names; `None` for `opacity`, which
    /// takes a value. Matching is exact and bytewise.
    pub fn from_word(word: &[u8]) -> Option<Command> {
        COMMANDS
            .into_iter()
            .find(|command| command.word().as_bytes() == word)
    }
}

/// The states the control commands change. `opacity` is a percent, 0 to `MAX_OPACITY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Toggles {
    pub locked: bool,
    pub hidden: bool,
    pub snapping: bool,
    pub opacity: u32,
}

/// The state after `command`. Returns `state` itself when `stopping`, so no command applies
/// after a stop.
pub fn apply(state: Toggles, command: Command, stopping: bool) -> Toggles {
    if stopping {
        return state;
    }
    match command {
        Command::Lock => Toggles {
            locked: true,
            ..state
        },
        Command::Unlock => Toggles {
            locked: false,
            ..state
        },
        Command::Hide => Toggles {
            hidden: true,
            ..state
        },
        Command::Show => Toggles {
            hidden: false,
            ..state
        },
        Command::ToggleLock => Toggles {
            locked: !state.locked,
            ..state
        },
        Command::ToggleHide => Toggles {
            hidden: !state.hidden,
            ..state
        },
        Command::Snap => Toggles {
            snapping: true,
            ..state
        },
        Command::Unsnap => Toggles {
            snapping: false,
            ..state
        },
        Command::ToggleSnap => Toggles {
            snapping: !state.snapping,
            ..state
        },
        Command::Opacity(n) => Toggles {
            opacity: n,
            ..state
        },
    }
}

/// Why the socket refuses a request. `Display` is the reason text of the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Empty,
    Unknown,
    TooLong,
    Timeout,
    Busy,
    BadValue,
}

impl Refusal {
    fn next(self) -> Option<Refusal> {
        match self {
            Refusal::Empty => Some(Refusal::Unknown),
            Refusal::Unknown => Some(Refusal::TooLong),
            Refusal::TooLong => Some(Refusal::Timeout),
            Refusal::Timeout => Some(Refusal::Busy),
            Refusal::Busy => Some(Refusal::BadValue),
            Refusal::BadValue => None,
        }
    }

    /// Every variant in declaration order. A new variant must be the `Some(..)` result of the arm
    /// before it: when appended, the last arm becomes `Some(X)` and `X`'s arm `None`.
    pub(super) fn all() -> impl Iterator<Item = Refusal> {
        std::iter::successors(Some(Refusal::Empty), |r| r.next())
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let line = reply(Err(*self));
        f.write_str(line.trim_start_matches("error: ").trim_end_matches('\n'))
    }
}

/// The reply text: `ok\n` or `error: <reason>\n`.
pub fn reply(result: Result<(), Refusal>) -> &'static str {
    match result {
        Ok(()) => "ok\n",
        Err(Refusal::Empty) => "error: empty line\n",
        Err(Refusal::Unknown) => "error: unknown command\n",
        Err(Refusal::TooLong) => "error: line too long\n",
        Err(Refusal::Timeout) => "error: timeout\n",
        Err(Refusal::Busy) => "error: busy\n",
        Err(Refusal::BadValue) => "error: bad value\n",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORDS: [(Command, &str); 10] = [
        (Command::Lock, "lock"),
        (Command::Unlock, "unlock"),
        (Command::Hide, "hide"),
        (Command::Show, "show"),
        (Command::ToggleLock, "toggle-lock"),
        (Command::ToggleHide, "toggle-hide"),
        (Command::Snap, "snap"),
        (Command::Unsnap, "unsnap"),
        (Command::ToggleSnap, "toggle-snap"),
        (Command::Opacity(50), "opacity"),
    ];

    #[test]
    fn word_cases() {
        for (command, word) in WORDS {
            assert_eq!(command.word(), word, "word of {command:?}");
        }
        let fieldless: Vec<(Command, &str)> = WORDS
            .into_iter()
            .filter(|(command, _)| !matches!(command, Command::Opacity(_)))
            .collect();
        assert_eq!(
            COMMANDS.to_vec(),
            fieldless
                .iter()
                .map(|&(command, _)| command)
                .collect::<Vec<_>>()
        );
        for (command, word) in fieldless {
            assert_eq!(Command::from_word(word.as_bytes()), Some(command), "{word}");
        }
    }

    #[test]
    fn from_word_unknown_cases() {
        let unknown: [&[u8]; 6] = [b"Lock", b"lock ", b"quit", b"opacity", b"opacity 50", b""];
        for word in unknown {
            assert_eq!(Command::from_word(word), None, "{word:?}");
        }
    }

    #[test]
    fn apply_cases() {
        let t = |locked, hidden, snapping, opacity| Toggles {
            locked,
            hidden,
            snapping,
            opacity,
        };
        let mut cases = Vec::new();
        for (locked, hidden, snapping) in [
            (false, false, false),
            (false, false, true),
            (false, true, false),
            (false, true, true),
            (true, false, false),
            (true, false, true),
            (true, true, false),
            (true, true, true),
        ] {
            let s = t(locked, hidden, snapping, 100);
            cases.push((Command::Lock, s, t(true, hidden, snapping, 100)));
            cases.push((Command::Unlock, s, t(false, hidden, snapping, 100)));
            cases.push((Command::Hide, s, t(locked, true, snapping, 100)));
            cases.push((Command::Show, s, t(locked, false, snapping, 100)));
            cases.push((Command::ToggleLock, s, t(!locked, hidden, snapping, 100)));
            cases.push((Command::ToggleHide, s, t(locked, !hidden, snapping, 100)));
            cases.push((Command::Snap, s, t(locked, hidden, true, 100)));
            cases.push((Command::Unsnap, s, t(locked, hidden, false, 100)));
            cases.push((Command::ToggleSnap, s, t(locked, hidden, !snapping, 100)));
            cases.push((Command::Opacity(50), s, t(locked, hidden, snapping, 50)));
            cases.push((Command::Opacity(100), s, s));
            cases.push((
                Command::Opacity(0),
                t(locked, hidden, snapping, 30),
                t(locked, hidden, snapping, 0),
            ));
        }
        for (command, state, want) in cases {
            assert_eq!(
                apply(state, command, false),
                want,
                "{command:?} on {state:?}"
            );
            assert_eq!(
                apply(state, command, true),
                state,
                "{command:?} on {state:?} while stopping"
            );
        }
    }

    #[test]
    fn parse_cases() {
        let cases: Vec<(&str, &[u8], Result<Command, Refusal>)> = vec![
            ("lock", b"lock", Ok(Command::Lock)),
            ("unlock", b"unlock", Ok(Command::Unlock)),
            ("hide", b"hide", Ok(Command::Hide)),
            ("show", b"show", Ok(Command::Show)),
            ("toggle-lock", b"toggle-lock", Ok(Command::ToggleLock)),
            ("toggle-hide", b"toggle-hide", Ok(Command::ToggleHide)),
            ("snap", b"snap", Ok(Command::Snap)),
            ("unsnap", b"unsnap", Ok(Command::Unsnap)),
            ("toggle-snap", b"toggle-snap", Ok(Command::ToggleSnap)),
            ("opacity 50", b"opacity 50", Ok(Command::Opacity(50))),
            ("opacity 0", b"opacity 0", Ok(Command::Opacity(0))),
            ("opacity 100", b"opacity 100", Ok(Command::Opacity(100))),
            ("opacity 007", b"opacity 007", Ok(Command::Opacity(7))),
            ("opacity 101", b"opacity 101", Err(Refusal::BadValue)),
            ("opacity 1000", b"opacity 1000", Err(Refusal::BadValue)),
            ("opacity -1", b"opacity -1", Err(Refusal::BadValue)),
            ("opacity 5a", b"opacity 5a", Err(Refusal::BadValue)),
            ("opacity empty value", b"opacity ", Err(Refusal::BadValue)),
            ("opacity two spaces", b"opacity  50", Err(Refusal::BadValue)),
            (
                "opacity trailing space",
                b"opacity 50 ",
                Err(Refusal::BadValue),
            ),
            ("opacity alone", b"opacity", Err(Refusal::Unknown)),
            ("snap trailing space", b"snap ", Err(Refusal::Unknown)),
            ("opacity upper case", b"Opacity 50", Err(Refusal::Unknown)),
            ("empty line", b"", Err(Refusal::Empty)),
        ];
        for (name, line, want) in cases {
            assert_eq!(Command::parse(line), want, "{name}");
        }
    }

    #[test]
    fn line_text_cases() {
        let mut cases: Vec<(Command, String)> = WORDS
            .iter()
            .filter(|(command, _)| !matches!(command, Command::Opacity(_)))
            .map(|&(command, word)| (command, word.to_string()))
            .collect();
        cases.push((Command::Opacity(50), "opacity 50".to_string()));
        for (command, want) in cases {
            assert_eq!(command.line(), want, "{command:?}");
        }
    }

    #[test]
    fn reply_cases() {
        let cases = [
            (Ok(()), "ok\n"),
            (Err(Refusal::Empty), "error: empty line\n"),
            (Err(Refusal::Unknown), "error: unknown command\n"),
            (Err(Refusal::TooLong), "error: line too long\n"),
            (Err(Refusal::Timeout), "error: timeout\n"),
            (Err(Refusal::Busy), "error: busy\n"),
            (Err(Refusal::BadValue), "error: bad value\n"),
        ];
        for (result, want) in cases {
            assert_eq!(reply(result), want, "{result:?}");
        }
        assert_eq!(
            Refusal::all().collect::<Vec<_>>(),
            [
                Refusal::Empty,
                Refusal::Unknown,
                Refusal::TooLong,
                Refusal::Timeout,
                Refusal::Busy,
                Refusal::BadValue
            ]
        );
        for r in Refusal::all() {
            assert_eq!(reply(Err(r)), format!("error: {r}\n"), "{r:?}");
        }
    }
}
