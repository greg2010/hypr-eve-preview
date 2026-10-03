use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::command::{Command, Refusal, reply};

/// Longest request line, terminator excluded.
pub const MAX_LINE: usize = 64;
/// How long a connection may stay open without completing its line.
pub const CONNECTION_DEADLINE: Duration = Duration::from_secs(1);
/// Most connections open at once.
pub const MAX_CONNECTIONS: usize = 8;
/// How long accepting pauses after an accept error.
pub const ACCEPT_PAUSE: Duration = Duration::from_millis(100);

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

#[cfg(test)]
mod tests;
