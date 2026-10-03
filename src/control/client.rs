use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::command::{Command, Refusal, reply};

/// Read and write timeout of the client.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);

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
    use std::io::{BufRead as _, BufReader};
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Instant;

    use super::super::server::{MAX_CONNECTIONS, Server};
    use super::*;
    use crate::control::fixtures::{admission_text, client, socket};
    use crate::testutil::TempDir;

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
