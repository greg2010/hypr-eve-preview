use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::MAX_OPACITY;

/// Longest request line, terminator excluded.
pub const MAX_LINE: usize = 64;
/// How long a connection may stay open without completing its line.
pub const CONNECTION_DEADLINE: Duration = Duration::from_secs(1);
/// Most connections open at once.
pub const MAX_CONNECTIONS: usize = 8;
/// How long accepting pauses after an accept error.
pub const ACCEPT_PAUSE: Duration = Duration::from_millis(100);
/// Read and write timeout of the client.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);

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
    fn all() -> impl Iterator<Item = Refusal> {
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

/// Collects the bytes of one request line, holding at most `MAX_LINE + 1` of them.
#[derive(Debug, Default)]
pub struct LineReader {
    buf: Vec<u8>,
    done: bool,
}

fn classify(line: &[u8]) -> Result<Command, Refusal> {
    Command::parse(line)
}

impl LineReader {
    /// `Some` once the line is complete: at its `\n`, or at the byte that makes it too long.
    /// Bytes after that are ignored, and later pushes return `None`.
    pub fn push(&mut self, bytes: &[u8]) -> Option<Result<Command, Refusal>> {
        if self.done {
            return None;
        }
        for &byte in bytes {
            if byte == b'\n' {
                self.done = true;
                return Some(classify(&self.buf));
            }
            self.buf.push(byte);
            if self.buf.len() > MAX_LINE {
                self.done = true;
                return Some(Err(Refusal::TooLong));
            }
        }
        None
    }

    fn remaining(&self) -> usize {
        MAX_LINE + 1 - self.buf.len()
    }

    /// The result at EOF. `None` when no byte arrived or the line already completed.
    pub fn finish(self) -> Option<Result<Command, Refusal>> {
        if self.done || self.buf.is_empty() {
            return None;
        }
        Some(classify(&self.buf))
    }
}

/// Names one connection of a `Server`. Ids are never reused within a server.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ConnectionId(u64);

/// A connection the caller must watch.
#[derive(Debug)]
pub struct Accepted {
    pub id: ConnectionId,
    /// A duplicate of the connection's descriptor, for the caller's event source.
    pub fd: OwnedFd,
    /// The accept instant plus `CONNECTION_DEADLINE`.
    pub deadline: Instant,
}

/// A failed reply. At least one field is `Some`.
#[derive(Debug)]
pub struct ReplyError {
    pub write: Option<io::Error>,
    pub shutdown: Option<io::Error>,
}

impl fmt::Display for ReplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if let Some(e) = &self.write {
            parts.push(format!("write: {e}"));
        }
        if let Some(e) = &self.shutdown {
            parts.push(format!("shutdown: {e}"));
        }
        f.write_str(&parts.join("; "))
    }
}

impl std::error::Error for ReplyError {}

/// What `Server::accept` did with one connection. `Busy` carries the outcome of writing
/// `error: busy` and shutting the connection down.
#[derive(Debug)]
pub enum Admission {
    Open(Accepted),
    Busy(Result<(), ReplyError>),
}

/// The result of `Server::read`.
#[derive(Debug)]
pub enum ReadOutcome {
    /// No complete line yet, or the line was already returned and the connection awaits `reply`
    /// or `close`.
    Pending,
    /// A line, an overflow, or EOF after bytes. The connection waits for `reply` or `close`.
    Line(Result<Command, Refusal>),
    /// EOF before any byte, or an id that is not open. Carries the shutdown result.
    Closed(io::Result<()>),
    /// A read error. The server shut the connection down and closed it.
    Failed {
        read: io::Error,
        shutdown: io::Result<()>,
    },
}

#[derive(Debug)]
struct Connection {
    stream: UnixStream,
    reader: LineReader,
    complete: bool,
}

/// The control socket: the listener and every open connection. It never touches the event loop.
/// Every close shuts the socket down and returns that error.
#[derive(Debug)]
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
    next_id: u64,
    connections: BTreeMap<u64, Connection>,
}

fn write_once(mut stream: &UnixStream, bytes: &[u8]) -> io::Result<()> {
    loop {
        return match stream.write(bytes) {
            Ok(n) if n == bytes.len() => Ok(()),
            Ok(_) => Err(io::Error::new(io::ErrorKind::WriteZero, "short write")),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => Err(e),
        };
    }
}

fn answer(stream: &UnixStream, text: &str) -> Result<(), ReplyError> {
    let write = write_once(stream, text.as_bytes()).err();
    let shutdown = stream.shutdown(Shutdown::Both).err();
    if write.is_none() && shutdown.is_none() {
        Ok(())
    } else {
        Err(ReplyError { write, shutdown })
    }
}

impl Server {
    /// Probes `path`, removes a stale file and binds a non-blocking listener there.
    pub fn bind(path: &Path) -> Result<Server, ControlError> {
        let path_buf = path.to_path_buf();
        match UnixStream::connect(path) {
            Ok(_) => return Err(ControlError::Answers { path: path_buf }),
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(source) => {
                        return Err(ControlError::RemoveStale {
                            path: path_buf,
                            source,
                        });
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(ControlError::Connect {
                    path: path_buf,
                    source,
                });
            }
        }
        let listener = UnixListener::bind(path).map_err(|source| ControlError::Bind {
            path: path_buf.clone(),
            source,
        })?;
        if let Err(e) = listener.set_nonblocking(true) {
            let source = match std::fs::remove_file(path) {
                Err(u) if u.kind() != io::ErrorKind::NotFound => {
                    io::Error::new(e.kind(), format!("set_nonblocking: {e}; unlink: {u}"))
                }
                _ => e,
            };
            return Err(ControlError::Bind {
                path: path_buf,
                source,
            });
        }
        Ok(Server {
            listener,
            path: path_buf,
            next_id: 0,
            connections: BTreeMap::new(),
        })
    }

    /// The socket path given to `bind`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A duplicate of the listener's descriptor, for the caller's event source.
    pub fn listener_fd(&self) -> io::Result<OwnedFd> {
        self.listener.try_clone().map(OwnedFd::from)
    }

    /// The number of open connections.
    pub fn open(&self) -> usize {
        self.connections.len()
    }

    /// Takes connections until `WouldBlock`. A connection that arrives while `MAX_CONNECTIONS`
    /// are open is answered `busy` and closed. Stops at the first error other than
    /// `Interrupted` and returns it with the admissions so far.
    pub fn accept(&mut self) -> (Vec<Admission>, Option<io::Error>) {
        let mut admissions = Vec::new();
        loop {
            let stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return (admissions, None),
                Err(e) => return (admissions, Some(e)),
            };
            let accepted = Instant::now();
            if let Err(e) = stream.set_nonblocking(true) {
                return (admissions, Some(e));
            }
            if self.open() >= MAX_CONNECTIONS {
                admissions.push(Admission::Busy(answer(&stream, reply(Err(Refusal::Busy)))));
                continue;
            }
            let fd = match stream.try_clone() {
                Ok(dup) => OwnedFd::from(dup),
                Err(e) => return (admissions, Some(e)),
            };
            let id = ConnectionId(self.next_id);
            self.next_id += 1;
            self.connections.insert(
                id.0,
                Connection {
                    stream,
                    reader: LineReader::default(),
                    complete: false,
                },
            );
            admissions.push(Admission::Open(Accepted {
                id,
                fd,
                deadline: accepted + CONNECTION_DEADLINE,
            }));
        }
    }

    /// Reads until `WouldBlock`, a complete line or EOF, never more than `MAX_LINE + 1` bytes.
    pub fn read(&mut self, id: ConnectionId) -> ReadOutcome {
        let Some(connection) = self.connections.get_mut(&id.0) else {
            return ReadOutcome::Closed(Ok(()));
        };
        if connection.complete {
            return ReadOutcome::Pending;
        }
        let mut buf = [0_u8; MAX_LINE + 1];
        loop {
            let want = connection.reader.remaining();
            match connection.stream.read(&mut buf[..want]) {
                Ok(0) => {
                    return match std::mem::take(&mut connection.reader).finish() {
                        Some(line) => {
                            connection.complete = true;
                            ReadOutcome::Line(line)
                        }
                        None => ReadOutcome::Closed(self.close(id)),
                    };
                }
                Ok(n) => {
                    if let Some(line) = connection.reader.push(&buf[..n]) {
                        connection.complete = true;
                        return ReadOutcome::Line(line);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return ReadOutcome::Pending,
                Err(read) => {
                    return ReadOutcome::Failed {
                        read,
                        shutdown: self.close(id),
                    };
                }
            }
        }
    }

    /// Writes the reply in one non-blocking write, then shuts the connection down and closes it.
    /// Both errors are kept. An id that is not open is `Ok(())` and writes nothing.
    pub fn reply(
        &mut self,
        id: ConnectionId,
        result: Result<(), Refusal>,
    ) -> Result<(), ReplyError> {
        match self.connections.remove(&id.0) {
            Some(connection) => answer(&connection.stream, reply(result)),
            None => Ok(()),
        }
    }

    /// Answers an open connection `error: timeout` and closes it. `None` when not open.
    pub fn timeout(&mut self, id: ConnectionId) -> Option<Result<(), ReplyError>> {
        if !self.connections.contains_key(&id.0) {
            return None;
        }
        Some(self.reply(id, Err(Refusal::Timeout)))
    }

    /// Shuts the connection down without a reply and closes it. An id that is not open is
    /// `Ok(())`.
    pub fn close(&mut self, id: ConnectionId) -> io::Result<()> {
        match self.connections.remove(&id.0) {
            Some(connection) => connection.stream.shutdown(Shutdown::Both),
            None => Ok(()),
        }
    }

    /// Closes every connection without a reply and unlinks the socket file. All shutdown errors
    /// and the unlink error (`NotFound` ignored) become one error of the first error's kind.
    pub fn remove(self) -> io::Result<()> {
        let mut errors = Vec::new();
        for connection in self.connections.into_values() {
            if let Err(e) = connection.stream.shutdown(Shutdown::Both) {
                errors.push(("shutdown", e));
            }
        }
        drop(self.listener);
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => errors.push(("unlink", e)),
            _ => {}
        }
        let Some((_, first)) = errors.first() else {
            return Ok(());
        };
        let kind = first.kind();
        let message = errors
            .iter()
            .map(|(part, e)| format!("{part}: {e}"))
            .collect::<Vec<_>>()
            .join("; ");
        Err(io::Error::new(kind, message))
    }
}

/// Sends `command` to the daemon at `path` and reads its reply. Each read and write waits at
/// most `CLIENT_TIMEOUT`.
pub fn send(path: &Path, command: Command) -> Result<(), ClientError> {
    let mut stream = UnixStream::connect(path).map_err(|source| ClientError::Connect {
        path: path.to_path_buf(),
        source,
    })?;
    stream
        .set_read_timeout(Some(CLIENT_TIMEOUT))
        .map_err(ClientError::Io)?;
    stream
        .set_write_timeout(Some(CLIENT_TIMEOUT))
        .map_err(ClientError::Io)?;
    let io_error = |e: io::Error| match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => ClientError::Timeout,
        _ => ClientError::Io(e),
    };
    // A busy daemon replies and closes without reading the request, so the write can hit
    // EPIPE and the read can end in ECONNRESET with the reply already buffered.
    let mut ignored = None;
    match stream.write_all(format!("{}\n", command.line()).as_bytes()) {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => return Err(io_error(e)),
        Err(e) => ignored = Some(e),
        Ok(()) => {}
    }
    let mut text = Vec::new();
    match stream.read_to_end(&mut text) {
        Err(e) if e.kind() != io::ErrorKind::ConnectionReset => return Err(io_error(e)),
        Err(e) => ignored = Some(e),
        Ok(_) => {}
    }
    if text == reply(Ok(())).as_bytes() {
        return Ok(());
    }
    match Refusal::all().find(|&r| text == reply(Err(r)).as_bytes()) {
        Some(r) => Err(ClientError::Refused(r.to_string())),
        None => Err(ignored.map_or(ClientError::BadReply, ClientError::Io)),
    }
}

/// Why `Server::bind` failed. `Display` is the exit reason.
#[derive(Debug)]
pub enum ControlError {
    Answers { path: PathBuf },
    Connect { path: PathBuf, source: io::Error },
    RemoveStale { path: PathBuf, source: io::Error },
    Bind { path: PathBuf, source: io::Error },
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ControlError::Answers { path } => write!(
                f,
                "control socket {}: another hypr-eve-preview answers",
                path.display()
            ),
            ControlError::Connect { path, source } => {
                write!(f, "control socket {}: connect: {source}", path.display())
            }
            ControlError::RemoveStale { path, source } => write!(
                f,
                "control socket {}: remove stale: {source}",
                path.display()
            ),
            ControlError::Bind { path, source } => {
                write!(f, "control socket {}: bind: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ControlError {}

/// Why `send` failed. `Display` is the exit reason.
#[derive(Debug)]
pub enum ClientError {
    Connect { path: PathBuf, source: io::Error },
    Timeout,
    Io(io::Error),
    Refused(String),
    BadReply,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Connect { path, source } => {
                write!(f, "connect {}: {source}", path.display())
            }
            ClientError::Timeout => {
                write!(f, "no reply within {} s", CLIENT_TIMEOUT.as_secs())
            }
            ClientError::Io(source) => write!(f, "{source}"),
            ClientError::Refused(reason) => f.write_str(reason),
            ClientError::BadReply => f.write_str("unexpected reply"),
        }
    }
}

impl std::error::Error for ClientError {}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{BufRead as _, BufReader};
    use std::os::unix::net::UnixListener;
    use std::thread;

    use super::*;
    use crate::testutil::TempDir;

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

    type Line = Option<Result<Command, Refusal>>;

    #[test]
    fn line_cases() {
        let long = [b'a'; MAX_LINE + 1];
        let mut at_limit = vec![b'a'; MAX_LINE];
        at_limit.push(b'\n');
        type Row<'a> = (&'a str, Vec<&'a [u8]>, Vec<Line>, Line);
        let cases: Vec<Row> = vec![
            (
                "lock newline",
                vec![b"lock\n"],
                vec![Some(Ok(Command::Lock))],
                None,
            ),
            (
                "unlock newline",
                vec![b"unlock\n"],
                vec![Some(Ok(Command::Unlock))],
                None,
            ),
            (
                "hide newline",
                vec![b"hide\n"],
                vec![Some(Ok(Command::Hide))],
                None,
            ),
            (
                "show newline",
                vec![b"show\n"],
                vec![Some(Ok(Command::Show))],
                None,
            ),
            (
                "toggle-lock newline",
                vec![b"toggle-lock\n"],
                vec![Some(Ok(Command::ToggleLock))],
                None,
            ),
            (
                "toggle-hide newline",
                vec![b"toggle-hide\n"],
                vec![Some(Ok(Command::ToggleHide))],
                None,
            ),
            (
                "toggle-snap newline",
                vec![b"toggle-snap\n"],
                vec![Some(Ok(Command::ToggleSnap))],
                None,
            ),
            (
                "opacity newline",
                vec![b"opacity 50\n"],
                vec![Some(Ok(Command::Opacity(50)))],
                None,
            ),
            (
                "opacity then eof",
                vec![b"opacity 50"],
                vec![None],
                Some(Ok(Command::Opacity(50))),
            ),
            (
                "lock then eof",
                vec![b"lock"],
                vec![None],
                Some(Ok(Command::Lock)),
            ),
            (
                "unlock then eof",
                vec![b"unlock"],
                vec![None],
                Some(Ok(Command::Unlock)),
            ),
            (
                "hide then eof",
                vec![b"hide"],
                vec![None],
                Some(Ok(Command::Hide)),
            ),
            (
                "show then eof",
                vec![b"show"],
                vec![None],
                Some(Ok(Command::Show)),
            ),
            (
                "toggle-lock then eof",
                vec![b"toggle-lock"],
                vec![None],
                Some(Ok(Command::ToggleLock)),
            ),
            (
                "toggle-hide then eof",
                vec![b"toggle-hide"],
                vec![None],
                Some(Ok(Command::ToggleHide)),
            ),
            (
                "empty line",
                vec![b"\n"],
                vec![Some(Err(Refusal::Empty))],
                None,
            ),
            (
                "trailing space",
                vec![b"lock \n"],
                vec![Some(Err(Refusal::Unknown))],
                None,
            ),
            (
                "upper case",
                vec![b"LOCK\n"],
                vec![Some(Err(Refusal::Unknown))],
                None,
            ),
            (
                "carriage return",
                vec![b"lock\r\n"],
                vec![Some(Err(Refusal::Unknown))],
                None,
            ),
            (
                "unknown word",
                vec![b"quit\n"],
                vec![Some(Err(Refusal::Unknown))],
                None,
            ),
            (
                "too long at the 65th byte",
                vec![&long[..MAX_LINE], &long[MAX_LINE..]],
                vec![None, Some(Err(Refusal::TooLong))],
                None,
            ),
            (
                "too long in one push",
                vec![&long],
                vec![Some(Err(Refusal::TooLong))],
                None,
            ),
            (
                "64 bytes and newline",
                vec![&at_limit],
                vec![Some(Err(Refusal::Unknown))],
                None,
            ),
            ("nothing then eof", vec![], vec![], None),
            ("empty push then eof", vec![b""], vec![None], None),
            (
                "word split over two pushes",
                vec![b"toggle-", b"hide\n"],
                vec![None, Some(Ok(Command::ToggleHide))],
                None,
            ),
            (
                "bytes after the newline are ignored",
                vec![b"lock\nhide\n"],
                vec![Some(Ok(Command::Lock))],
                None,
            ),
            (
                "a push after the line is ignored",
                vec![b"lock\n", b"hide\n"],
                vec![Some(Ok(Command::Lock)), None],
                None,
            ),
        ];
        for (name, pushes, want_pushes, want_finish) in cases {
            let mut reader = LineReader::default();
            let got: Vec<Line> = pushes.iter().map(|bytes| reader.push(bytes)).collect();
            assert_eq!(got, want_pushes, "{name}: pushes");
            assert_eq!(reader.finish(), want_finish, "{name}: finish");
        }
    }

    fn socket(dir: &TempDir) -> PathBuf {
        dir.path().join("s")
    }

    fn names(dir: &TempDir) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir.path())
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[derive(Clone, Copy)]
    enum PathState {
        Missing,
        StaleSocket,
        RegularFile,
        LiveListener,
        Directory,
        BelowRegularFile,
        MissingParent,
    }

    fn prepare(state: PathState, base: &Path) -> (PathBuf, Option<UnixListener>) {
        match state {
            PathState::Missing => (base.to_path_buf(), None),
            PathState::StaleSocket => {
                drop(UnixListener::bind(base).expect("bind stale"));
                (base.to_path_buf(), None)
            }
            PathState::RegularFile => {
                fs::write(base, b"x").expect("write file");
                (base.to_path_buf(), None)
            }
            PathState::LiveListener => (
                base.to_path_buf(),
                Some(UnixListener::bind(base).expect("bind live")),
            ),
            PathState::Directory => {
                fs::create_dir(base).expect("mkdir");
                (base.to_path_buf(), None)
            }
            PathState::BelowRegularFile => {
                fs::write(base, b"x").expect("write file");
                (base.join("x"), None)
            }
            PathState::MissingParent => (base.join("x").join("y"), None),
        }
    }

    #[test]
    fn bind_cases() {
        type Row<'a> = (&'a str, PathState, Result<(), &'a str>, Vec<&'a str>);
        let cases: Vec<Row> = vec![
            (
                "live listener",
                PathState::LiveListener,
                Err("answers"),
                vec!["s"],
            ),
            ("stale socket file", PathState::StaleSocket, Ok(()), vec![]),
            ("regular file", PathState::RegularFile, Ok(()), vec![]),
            ("missing file", PathState::Missing, Ok(()), vec![]),
            (
                "directory at the path",
                PathState::Directory,
                Err("remove stale"),
                vec!["s"],
            ),
            (
                "path below a regular file",
                PathState::BelowRegularFile,
                Err("connect"),
                vec!["s"],
            ),
            (
                "parent directory missing",
                PathState::MissingParent,
                Err("bind"),
                vec![],
            ),
        ];
        for (name, state, want, want_entries) in cases {
            let dir = TempDir::new("control-bind");
            let (path, _live) = prepare(state, &socket(&dir));
            let got = match Server::bind(&path) {
                Ok(server) => {
                    assert_eq!(server.path(), path, "{name}: path");
                    server.remove().map_err(|_| "remove")
                }
                Err(ControlError::Answers { .. }) => Err("answers"),
                Err(ControlError::Connect { .. }) => Err("connect"),
                Err(ControlError::RemoveStale { .. }) => Err("remove stale"),
                Err(ControlError::Bind { .. }) => Err("bind"),
            };
            assert_eq!(got, want, "{name}");
            assert_eq!(names(&dir), want_entries, "{name}: entries");
        }
    }

    fn client(path: &Path) -> UnixStream {
        let stream = UnixStream::connect(path).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("read timeout");
        stream
    }

    fn read_all(mut stream: UnixStream) -> Vec<u8> {
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).expect("read to end");
        bytes
    }

    fn only(admissions: Vec<Admission>) -> Accepted {
        let mut open: Vec<Accepted> = admissions
            .into_iter()
            .filter_map(|a| match a {
                Admission::Open(accepted) => Some(accepted),
                Admission::Busy(_) => None,
            })
            .collect();
        assert_eq!(open.len(), 1, "open admissions");
        open.remove(0)
    }

    #[derive(Debug, PartialEq)]
    enum Seen {
        Pending,
        Line(Result<Command, Refusal>),
        Closed(Result<(), io::ErrorKind>),
        Failed,
    }

    fn seen(outcome: ReadOutcome) -> Seen {
        match outcome {
            ReadOutcome::Pending => Seen::Pending,
            ReadOutcome::Line(line) => Seen::Line(line),
            ReadOutcome::Closed(result) => Seen::Closed(result.map_err(|e| e.kind())),
            ReadOutcome::Failed { .. } => Seen::Failed,
        }
    }

    fn accept_one(server: &mut Server) -> Accepted {
        let (admissions, error) = server.accept();
        assert!(error.is_none(), "accept error: {error:?}");
        only(admissions)
    }

    fn finish(server: Server) {
        server.remove().expect("remove");
    }

    fn admission_text(admission: &Admission) -> &'static str {
        match admission {
            Admission::Open(_) => "open",
            Admission::Busy(Ok(())) => "busy",
            Admission::Busy(Err(_)) => "busy failed",
        }
    }

    #[test]
    fn server_cases() {
        type Row = fn(Server, &TempDir);
        let cases: Vec<(&str, Row)> = vec![
            ("line then reply", |mut server, dir| {
                let mut peer = client(&socket(dir));
                peer.write_all(b"lock\n").expect("write");
                let a = accept_one(&mut server);
                assert_eq!(seen(server.read(a.id)), Seen::Line(Ok(Command::Lock)));
                assert_eq!(seen(server.read(a.id)), Seen::Pending, "read after line");
                assert!(server.reply(a.id, Ok(())).is_ok());
                assert_eq!(server.open(), 0);
                assert_eq!(read_all(peer), b"ok\n");
                finish(server);
            }),
            ("refused line then reply", |mut server, dir| {
                let mut peer = client(&socket(dir));
                peer.write_all(b"\n").expect("write");
                let a = accept_one(&mut server);
                assert_eq!(seen(server.read(a.id)), Seen::Line(Err(Refusal::Empty)));
                assert!(server.reply(a.id, Err(Refusal::Empty)).is_ok());
                assert_eq!(read_all(peer), b"error: empty line\n");
                finish(server);
            }),
            ("overflow", |mut server, dir| {
                let mut peer = client(&socket(dir));
                peer.write_all(&[b'a'; 200]).expect("write");
                let a = accept_one(&mut server);
                assert_eq!(seen(server.read(a.id)), Seen::Line(Err(Refusal::TooLong)));
                assert!(server.reply(a.id, Err(Refusal::TooLong)).is_ok());
                assert_eq!(read_all(peer), b"error: line too long\n");
                finish(server);
            }),
            ("eof after bytes is a line", |mut server, dir| {
                let mut peer = client(&socket(dir));
                peer.write_all(b"hide").expect("write");
                peer.shutdown(Shutdown::Write).expect("shutdown write");
                let a = accept_one(&mut server);
                assert_eq!(seen(server.read(a.id)), Seen::Line(Ok(Command::Hide)));
                assert_eq!(server.open(), 1);
                assert!(server.reply(a.id, Ok(())).is_ok());
                assert_eq!(read_all(peer), b"ok\n");
                finish(server);
            }),
            ("eof before any byte", |mut server, dir| {
                let peer = client(&socket(dir));
                peer.shutdown(Shutdown::Write).expect("shutdown write");
                let a = accept_one(&mut server);
                assert_eq!(seen(server.read(a.id)), Seen::Closed(Ok(())));
                assert_eq!(server.open(), 0);
                assert_eq!(read_all(peer), b"");
                finish(server);
            }),
            ("read of an id that is not open", |mut server, _| {
                assert_eq!(seen(server.read(ConnectionId(99))), Seen::Closed(Ok(())));
                finish(server);
            }),
            ("partial word pends", |mut server, dir| {
                let mut peer = client(&socket(dir));
                peer.write_all(b"lo").expect("write");
                let a = accept_one(&mut server);
                assert_eq!(seen(server.read(a.id)), Seen::Pending);
                peer.write_all(b"ck\n").expect("write");
                assert_eq!(seen(server.read(a.id)), Seen::Line(Ok(Command::Lock)));
                finish(server);
            }),
            ("eight open, the ninth is busy", |mut server, dir| {
                let mut peers: Vec<UnixStream> = (0..9).map(|_| client(&socket(dir))).collect();
                let (admissions, error) = server.accept();
                assert!(error.is_none(), "accept error: {error:?}");
                let shape: Vec<&str> = admissions.iter().map(admission_text).collect();
                let mut want = vec!["open"; 8];
                want.push("busy");
                assert_eq!(shape, want);
                assert_eq!(server.open(), 8);
                assert_eq!(read_all(peers.remove(8)), b"error: busy\n");
                finish(server);
            }),
            ("partial word then timeout", |mut server, dir| {
                let mut peer = client(&socket(dir));
                peer.write_all(b"loc").expect("write");
                let a = accept_one(&mut server);
                assert_eq!(seen(server.read(a.id)), Seen::Pending);
                assert!(matches!(server.timeout(a.id), Some(Ok(()))));
                assert_eq!(server.open(), 0);
                assert_eq!(read_all(peer), b"error: timeout\n");
                finish(server);
            }),
            ("timeout of a closed id", |mut server, dir| {
                let peer = client(&socket(dir));
                let a = accept_one(&mut server);
                assert!(server.close(a.id).is_ok());
                assert!(server.timeout(a.id).is_none());
                assert_eq!(read_all(peer), b"");
                finish(server);
            }),
            ("close of an open id", |mut server, dir| {
                let peer = client(&socket(dir));
                let a = accept_one(&mut server);
                assert_eq!(server.open(), 1);
                assert!(server.close(a.id).is_ok());
                assert_eq!(server.open(), 0);
                assert_eq!(read_all(peer), b"");
                finish(server);
            }),
            ("reply on a closed id", |mut server, dir| {
                let peer = client(&socket(dir));
                let a = accept_one(&mut server);
                assert!(server.close(a.id).is_ok());
                assert!(server.reply(a.id, Ok(())).is_ok());
                assert_eq!(read_all(peer), b"");
                finish(server);
            }),
            (
                "deadline lies one second after the accept",
                |mut server, dir| {
                    let _peer = client(&socket(dir));
                    let before = Instant::now();
                    let a = accept_one(&mut server);
                    let after = Instant::now();
                    assert!(
                        a.deadline >= before + CONNECTION_DEADLINE
                            && a.deadline <= after + CONNECTION_DEADLINE,
                        "deadline outside the accept window"
                    );
                    finish(server);
                },
            ),
            ("ids are distinct and in accept order", |mut server, dir| {
                let _p1 = client(&socket(dir));
                let _p2 = client(&socket(dir));
                let (admissions, _) = server.accept();
                let ids: Vec<ConnectionId> = admissions
                    .iter()
                    .filter_map(|a| match a {
                        Admission::Open(accepted) => Some(accepted.id),
                        Admission::Busy(_) => None,
                    })
                    .collect();
                assert_eq!(ids, vec![ConnectionId(0), ConnectionId(1)]);
                finish(server);
            }),
            ("peer gone before the reply", |mut server, dir| {
                let mut peer = client(&socket(dir));
                peer.write_all(b"lock\n").expect("write");
                drop(peer);
                let a = accept_one(&mut server);
                assert_eq!(seen(server.read(a.id)), Seen::Line(Ok(Command::Lock)));
                let got = server.reply(a.id, Ok(()));
                let shape = got
                    .as_ref()
                    .err()
                    .map(|e| (e.write.as_ref().map(io::Error::kind), e.shutdown.is_none()));
                assert_eq!(shape, Some((Some(io::ErrorKind::BrokenPipe), true)));
                finish(server);
            }),
            ("remove leaves the pre-bind entries", |mut server, dir| {
                let _peer = client(&socket(dir));
                let _a = accept_one(&mut server);
                assert_eq!(names(dir), vec!["s"]);
                finish(server);
                assert_eq!(names(dir), Vec::<String>::new());
            }),
        ];
        for (name, row) in cases {
            let dir = TempDir::new("control-server");
            let server = Server::bind(&socket(&dir))
                .map_err(|e| format!("{name}: {e}"))
                .expect("bind");
            row(server, &dir);
        }
    }

    #[test]
    fn reply_error_display_cases() {
        let w = || io::Error::other("w");
        let s = || io::Error::other("s");
        let cases = [
            (
                ReplyError {
                    write: Some(w()),
                    shutdown: None,
                },
                "write: w",
            ),
            (
                ReplyError {
                    write: None,
                    shutdown: Some(s()),
                },
                "shutdown: s",
            ),
            (
                ReplyError {
                    write: Some(w()),
                    shutdown: Some(s()),
                },
                "write: w; shutdown: s",
            ),
        ];
        for (err, want) in cases {
            assert_eq!(err.to_string(), want);
        }
    }

    #[test]
    fn error_display_cases() {
        let p = || PathBuf::from("/x/s");
        let e = || io::Error::other("boom");
        let cases: Vec<(String, &str)> = vec![
            (
                ControlError::Answers { path: p() }.to_string(),
                "control socket /x/s: another hypr-eve-preview answers",
            ),
            (
                ControlError::Connect {
                    path: p(),
                    source: e(),
                }
                .to_string(),
                "control socket /x/s: connect: boom",
            ),
            (
                ControlError::RemoveStale {
                    path: p(),
                    source: e(),
                }
                .to_string(),
                "control socket /x/s: remove stale: boom",
            ),
            (
                ControlError::Bind {
                    path: p(),
                    source: e(),
                }
                .to_string(),
                "control socket /x/s: bind: boom",
            ),
            (
                ClientError::Connect {
                    path: p(),
                    source: e(),
                }
                .to_string(),
                "connect /x/s: boom",
            ),
            (ClientError::Timeout.to_string(), "no reply within 2 s"),
            (ClientError::Io(e()).to_string(), "boom"),
            (ClientError::Refused("busy".into()).to_string(), "busy"),
            (ClientError::BadReply.to_string(), "unexpected reply"),
        ];
        for (got, want) in cases {
            assert_eq!(got, want);
        }
    }

    #[test]
    fn remove_error_cases() {
        type Setup = fn(&Path);
        type Row<'a> = (&'a str, Setup, Result<(), (io::ErrorKind, &'a str)>);
        let cases: Vec<Row> = vec![
            (
                "socket file already gone",
                |p| fs::remove_file(p).expect("unlink first"),
                Ok(()),
            ),
            (
                "unlink fails",
                |p| {
                    fs::remove_file(p).expect("unlink first");
                    fs::create_dir(p).expect("mkdir");
                    fs::write(p.join("f"), b"x").expect("write inside");
                },
                Err((
                    io::ErrorKind::IsADirectory,
                    "unlink: Is a directory (os error 21)",
                )),
            ),
        ];
        for (name, setup, want) in cases {
            let dir = TempDir::new("control-remove");
            let path = socket(&dir);
            let server = Server::bind(&path).expect("bind");
            setup(&path);
            let got = server.remove().map_err(|e| (e.kind(), e.to_string()));
            let want = want.map_err(|(kind, text)| (kind, text.to_string()));
            assert_eq!(got, want, "{name}");
        }
    }

    #[derive(Debug, PartialEq)]
    enum Sent {
        Ok,
        Refused(String),
        BadReply,
        Timeout,
        Io(String),
        Connect(bool),
    }

    #[derive(Debug, PartialEq)]
    enum Observed {
        Lines(Vec<String>),
        Admissions(Vec<&'static str>),
        Queued(usize),
    }

    #[derive(Clone, Copy)]
    enum Peer {
        Reply(&'static [u8]),
        Hold,
        Missing,
        FullServer,
        ResetUnread(&'static [u8]),
    }

    #[test]
    fn send_request_cases() {
        let cases = [
            (Command::Lock, "lock\n"),
            (Command::ToggleHide, "toggle-hide\n"),
            (Command::Opacity(50), "opacity 50\n"),
        ];
        for (command, want) in cases {
            let dir = TempDir::new("control-send-request");
            let path = socket(&dir);
            let listener = UnixListener::bind(&path).expect("bind");
            let peer = thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut line = String::new();
                BufReader::new(&stream)
                    .read_line(&mut line)
                    .expect("read request");
                stream.write_all(b"ok\n").expect("write reply");
                line
            });
            assert!(send(&path, command).is_ok(), "{command:?}");
            assert_eq!(peer.join().expect("peer thread"), want, "{command:?}");
        }
    }

    #[test]
    fn send_cases() {
        const REQUEST: &str = "toggle-hide\n";
        let cases = [
            ("ok", Peer::Reply(b"ok\n"), Sent::Ok),
            (
                "busy",
                Peer::Reply(b"error: busy\n"),
                Sent::Refused("busy".into()),
            ),
            (
                "empty line",
                Peer::Reply(b"error: empty line\n"),
                Sent::Refused("empty line".into()),
            ),
            (
                "busy from a real server",
                Peer::FullServer,
                Sent::Refused("busy".into()),
            ),
            ("garbage", Peer::Reply(b"nonsense"), Sent::BadReply),
            (
                "unknown reason",
                Peer::Reply(b"error: why\n"),
                Sent::BadReply,
            ),
            ("no file", Peer::Missing, Sent::Connect(true)),
            ("never replies", Peer::Hold, Sent::Timeout),
            (
                "closed with the request unread, no reply",
                Peer::ResetUnread(b""),
                Sent::Io("Connection reset by peer (os error 104)".into()),
            ),
            (
                "reply cut off by a reset",
                Peer::ResetUnread(b"error: bu"),
                Sent::Io("Connection reset by peer (os error 104)".into()),
            ),
            (
                "busy then a reset",
                Peer::ResetUnread(b"error: busy\n"),
                Sent::Refused("busy".into()),
            ),
        ];
        for (name, peer, want) in cases {
            let dir = TempDir::new("control-send");
            let path = socket(&dir);
            let server = match peer {
                Peer::Missing => None,
                Peer::FullServer => {
                    let mut server = Server::bind(&path).expect("bind");
                    let held: Vec<UnixStream> =
                        (0..MAX_CONNECTIONS).map(|_| client(&path)).collect();
                    let (admissions, error) = server.accept();
                    assert!(error.is_none(), "accept error: {error:?}");
                    assert_eq!(admissions.len(), MAX_CONNECTIONS);
                    Some(thread::spawn(move || {
                        let _held = held;
                        // The request must be in the socket unread when the refusal closes it.
                        thread::sleep(Duration::from_millis(100));
                        loop {
                            let (admissions, error) = server.accept();
                            assert!(error.is_none(), "accept error: {error:?}");
                            if !admissions.is_empty() {
                                return Observed::Admissions(
                                    admissions.iter().map(admission_text).collect(),
                                );
                            }
                            thread::sleep(Duration::from_millis(5));
                        }
                    }))
                }
                Peer::ResetUnread(bytes) => {
                    let listener = UnixListener::bind(&path).expect("bind");
                    Some(thread::spawn(move || {
                        let (mut stream, _) = listener.accept().expect("accept");
                        // Dropping the stream with the request unread makes the kernel reset.
                        let deadline = Instant::now() + 2 * CLIENT_TIMEOUT;
                        let mut queued = 0;
                        while queued == 0 && Instant::now() < deadline {
                            queued = rustix::io::ioctl_fionread(&stream).expect("fionread");
                            thread::sleep(Duration::from_millis(1));
                        }
                        stream.write_all(bytes).expect("write reply");
                        Observed::Queued(usize::try_from(queued).expect("queued bytes"))
                    }))
                }
                _ => {
                    let listener = UnixListener::bind(&path).expect("bind");
                    Some(thread::spawn(move || {
                        let (mut stream, _) = listener.accept().expect("accept");
                        let mut line = String::new();
                        BufReader::new(&stream)
                            .read_line(&mut line)
                            .expect("read request");
                        match peer {
                            Peer::Reply(bytes) => stream.write_all(bytes).expect("write reply"),
                            _ => thread::sleep(Duration::from_secs(3)),
                        }
                        Observed::Lines(vec![line])
                    }))
                }
            };
            let start = Instant::now();
            let got = send(&path, Command::ToggleHide);
            let elapsed = start.elapsed();
            let seen = server.map(|handle| handle.join().expect("server thread"));
            let got = match got {
                Ok(()) => Sent::Ok,
                Err(ClientError::Refused(r)) => Sent::Refused(r),
                Err(ClientError::BadReply) => Sent::BadReply,
                Err(ClientError::Timeout) => Sent::Timeout,
                Err(e @ ClientError::Connect { .. }) => Sent::Connect(
                    e.to_string()
                        .starts_with(&format!("connect {}: ", path.display())),
                ),
                Err(ClientError::Io(e)) => Sent::Io(e.to_string()),
            };
            assert_eq!(got, want, "{name}");
            let want_seen = match (&want, peer) {
                (Sent::Connect(_), _) => None,
                (_, Peer::FullServer) => Some(Observed::Admissions(vec!["busy"])),
                (_, Peer::ResetUnread(_)) => Some(Observed::Queued(REQUEST.len())),
                _ => Some(Observed::Lines(vec![REQUEST.to_string()])),
            };
            assert_eq!(seen, want_seen, "{name}: seen");
            if want == Sent::Timeout {
                assert!(
                    elapsed >= CLIENT_TIMEOUT && elapsed < Duration::from_secs(3),
                    "{name}: elapsed {elapsed:?}"
                );
            }
        }
    }
}
