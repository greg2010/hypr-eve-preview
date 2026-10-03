use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::hypr;

/// Read and write timeout of one request-socket exchange.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub enum IpcError {
    Env(&'static str),
    Connect { path: PathBuf, source: io::Error },
    Eof,
    Timeout,
    Io(io::Error),
    NotUtf8,
    BadEvent(&'static str),
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IpcError::Env(var) => write!(f, "{var} is unset or empty"),
            IpcError::Connect { path, source } => {
                write!(f, "connect {}: {source}", path.display())
            }
            IpcError::Eof => f.write_str("EOF"),
            IpcError::Timeout => write!(f, "timed out after {} s", REQUEST_TIMEOUT.as_secs()),
            IpcError::Io(source) => write!(f, "{source}"),
            IpcError::NotUtf8 => f.write_str("reply is not UTF-8"),
            IpcError::BadEvent(name) => write!(f, "malformed {name} data"),
        }
    }
}

impl std::error::Error for IpcError {}

/// Hyprland socket paths under `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/`.
/// `control` is this tool's own command socket, `.hypr-eve-preview.sock`, in the same directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sockets {
    pub events: PathBuf,
    pub requests: PathBuf,
    pub control: PathBuf,
}

pub fn sockets(
    runtime_dir: Option<&OsStr>,
    signature: Option<&OsStr>,
) -> Result<Sockets, IpcError> {
    let present = |value: Option<&'_ OsStr>, name: &'static str| match value {
        Some(v) if !v.is_empty() => Ok(v.to_owned()),
        _ => Err(IpcError::Env(name)),
    };
    let runtime_dir = present(runtime_dir, "XDG_RUNTIME_DIR")?;
    let signature = present(signature, "HYPRLAND_INSTANCE_SIGNATURE")?;
    let dir = Path::new(&runtime_dir).join("hypr").join(signature);
    Ok(Sockets {
        events: dir.join(".socket2.sock"),
        requests: dir.join(".socket.sock"),
        control: dir.join(".hypr-eve-preview.sock"),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    OpenWindow {
        address: u64,
        workspace: String,
        title: String,
    },
    CloseWindow {
        address: u64,
    },
    WindowTitle {
        address: u64,
        title: String,
    },
    MoveWindow {
        address: u64,
        workspace: String,
    },
    ActiveWindow {
        address: Option<u64>,
    },
}

/// Parses one event line. `Ok(None)` is a name the tool skips; `Err` is a handled name whose
/// data does not parse.
pub fn parse_event(line: &str) -> Result<Option<Event>, IpcError> {
    let (name, data) = line.split_once(">>").unwrap_or((line, ""));
    let name: &'static str = match name {
        "openwindow" => "openwindow",
        "closewindow" => "closewindow",
        "windowtitlev2" => "windowtitlev2",
        "movewindowv2" => "movewindowv2",
        "activewindowv2" => "activewindowv2",
        _ => return Ok(None),
    };
    let bad = || IpcError::BadEvent(name);
    let address = |s: &str| hypr::parse_event_address(s).map_err(|_| bad());
    let event = match name {
        "openwindow" => {
            let mut parts = data.splitn(4, ',');
            match (parts.next(), parts.next(), parts.next(), parts.next()) {
                (Some(a), Some(workspace), Some(_), Some(title)) => Event::OpenWindow {
                    address: address(a)?,
                    workspace: workspace.to_string(),
                    title: title.to_string(),
                },
                _ => return Err(bad()),
            }
        }
        "closewindow" => Event::CloseWindow {
            address: address(data)?,
        },
        "windowtitlev2" => {
            let (a, title) = data.split_once(',').ok_or_else(bad)?;
            Event::WindowTitle {
                address: address(a)?,
                title: title.to_string(),
            }
        }
        "movewindowv2" => {
            let mut parts = data.splitn(3, ',');
            match (parts.next(), parts.next(), parts.next()) {
                (Some(a), Some(_), Some(workspace)) => Event::MoveWindow {
                    address: address(a)?,
                    workspace: workspace.to_string(),
                },
                _ => return Err(bad()),
            }
        }
        _ => Event::ActiveWindow {
            address: if data.is_empty() {
                None
            } else {
                Some(address(data)?)
            },
        },
    };
    Ok(Some(event))
}

#[derive(Debug, Default)]
pub struct LineBuffer {
    pending: Vec<u8>,
}

impl LineBuffer {
    /// Returns the lines completed by `bytes`, newline removed, invalid UTF-8 replaced by
    /// U+FFFD. Bytes after the last newline wait for the next push.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let mut lines = Vec::new();
        let mut start = 0;
        while let Some(offset) = self.pending[start..].iter().position(|&b| b == b'\n') {
            let end = start + offset;
            lines.push(String::from_utf8_lossy(&self.pending[start..end]).into_owned());
            start = end + 1;
        }
        self.pending.drain(..start);
        lines
    }
}

#[derive(Debug)]
pub struct EventSocket {
    stream: UnixStream,
    buffer: LineBuffer,
}

impl AsFd for EventSocket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }
}

impl EventSocket {
    /// Connects to the event socket and makes it non-blocking.
    pub fn connect(path: &Path) -> Result<EventSocket, IpcError> {
        let stream = UnixStream::connect(path).map_err(|source| IpcError::Connect {
            path: path.to_path_buf(),
            source,
        })?;
        stream.set_nonblocking(true).map_err(IpcError::Io)?;
        Ok(EventSocket {
            stream,
            buffer: LineBuffer::default(),
        })
    }

    /// Reads until the socket would block and returns the complete lines. EOF is an error,
    /// and lines read before it are dropped with the connection.
    pub fn read_lines(&mut self) -> Result<Vec<String>, IpcError> {
        let mut lines = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(IpcError::Eof),
                Ok(n) => lines.extend(self.buffer.push(&chunk[..n])),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(lines),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(IpcError::Io(e)),
            }
        }
    }
}

/// Sends one request on its own connection and reads the reply to EOF. Each read and write
/// waits at most `REQUEST_TIMEOUT`.
pub fn request(path: &Path, text: &str) -> Result<String, IpcError> {
    let mut stream = UnixStream::connect(path).map_err(|source| IpcError::Connect {
        path: path.to_path_buf(),
        source,
    })?;
    stream
        .set_read_timeout(Some(REQUEST_TIMEOUT))
        .map_err(IpcError::Io)?;
    stream
        .set_write_timeout(Some(REQUEST_TIMEOUT))
        .map_err(IpcError::Io)?;
    let io_error = |e: io::Error| match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => IpcError::Timeout,
        _ => IpcError::Io(e),
    };
    stream.write_all(text.as_bytes()).map_err(io_error)?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).map_err(io_error)?;
    String::from_utf8(reply).map_err(|_| IpcError::NotUtf8)
}

pub fn workspace_dispatch(workspace: &str) -> String {
    format!("/dispatch workspace name:{workspace}")
}

/// `Ok` exactly when the reply is `ok`; otherwise the request error or the reply text.
pub fn dispatch_result(reply: Result<String, IpcError>) -> Result<(), String> {
    match reply {
        Ok(text) if text == "ok" => Ok(()),
        Ok(text) => Err(text),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::thread;
    use std::time::Instant;

    use super::*;
    use crate::testutil::TempDir;

    const A: u64 = 0x555512345678;

    fn socket(dir: &TempDir) -> PathBuf {
        dir.path().join("s")
    }

    fn cut_title_data() -> Vec<u8> {
        let mut data = b"555512345678,EVE - ".to_vec();
        data.extend("é".repeat(600).as_bytes());
        data.truncate(1024);
        data
    }

    fn cut_line() -> String {
        let mut line = b"windowtitlev2>>".to_vec();
        line.extend(cut_title_data());
        String::from_utf8_lossy(&line).into_owned()
    }

    fn open_window(workspace: &str, title: &str) -> Event {
        Event::OpenWindow {
            address: A,
            workspace: workspace.to_string(),
            title: title.to_string(),
        }
    }

    #[test]
    fn parse_event_cases() {
        let cut_title = format!("EVE - {}\u{FFFD}", "é".repeat(502));
        type Row<'a> = (&'a str, String, Result<Option<Event>, &'a str>);
        let cases: Vec<Row> = vec![
            (
                "openwindow",
                "openwindow>>555512345678,EVE-new,kitty,EVE".into(),
                Ok(Some(open_window("EVE-new", "EVE"))),
            ),
            (
                "openwindow title with commas",
                "openwindow>>555512345678,EVE2,kitty,EVE - A, B, C".into(),
                Ok(Some(open_window("EVE2", "EVE - A, B, C"))),
            ),
            (
                "closewindow",
                "closewindow>>555512345678".into(),
                Ok(Some(Event::CloseWindow { address: A })),
            ),
            (
                "windowtitlev2 with comma",
                "windowtitlev2>>555512345678,EVE - Pilot, One".into(),
                Ok(Some(Event::WindowTitle {
                    address: A,
                    title: "EVE - Pilot, One".into(),
                })),
            ),
            (
                "movewindowv2",
                "movewindowv2>>555512345678,-1340,EVE2".into(),
                Ok(Some(Event::MoveWindow {
                    address: A,
                    workspace: "EVE2".into(),
                })),
            ),
            (
                "movewindowv2 workspace with comma",
                "movewindowv2>>555512345678,-1340,Odd,Name".into(),
                Ok(Some(Event::MoveWindow {
                    address: A,
                    workspace: "Odd,Name".into(),
                })),
            ),
            (
                "activewindowv2",
                "activewindowv2>>555512345678".into(),
                Ok(Some(Event::ActiveWindow { address: Some(A) })),
            ),
            (
                "activewindowv2 empty",
                "activewindowv2>>".into(),
                Ok(Some(Event::ActiveWindow { address: None })),
            ),
            (
                "title cut inside a character",
                cut_line(),
                Ok(Some(Event::WindowTitle {
                    address: A,
                    title: cut_title,
                })),
            ),
            ("workspacev2", "workspacev2>>-1340,EVE2".into(), Ok(None)),
            ("activewindow", "activewindow>>kitty,x".into(), Ok(None)),
            ("screencast", "screencast>>1,window,EVE".into(), Ok(None)),
            ("configreloaded", "configreloaded>>".into(), Ok(None)),
            (
                "handled name without separator",
                "closewindow".into(),
                Err("closewindow"),
            ),
            (
                "unhandled name without separator",
                "configreloaded".into(),
                Ok(None),
            ),
            (
                "closewindow bad address",
                "closewindow>>zz".into(),
                Err("closewindow"),
            ),
            (
                "closewindow empty",
                "closewindow>>".into(),
                Err("closewindow"),
            ),
            (
                "closewindow prefixed",
                "closewindow>>0x555512345678".into(),
                Err("closewindow"),
            ),
            (
                "openwindow too few fields",
                "openwindow>>555512345678,EVE2".into(),
                Err("openwindow"),
            ),
            (
                "windowtitlev2 no title",
                "windowtitlev2>>555512345678".into(),
                Err("windowtitlev2"),
            ),
            (
                "movewindowv2 no name",
                "movewindowv2>>555512345678,-1340".into(),
                Err("movewindowv2"),
            ),
            (
                "activewindowv2 bad address",
                "activewindowv2>>zz".into(),
                Err("activewindowv2"),
            ),
        ];
        for (name, line, want) in cases {
            let got = parse_event(&line).map_err(|e| match e {
                IpcError::BadEvent(n) => n,
                other => panic!("{name}: unexpected error {other}"),
            });
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn line_buffer_cases() {
        let mut cut = cut_title_data();
        cut.splice(0..0, b"windowtitlev2>>".iter().copied());
        cut.push(b'\n');
        let cut_want = vec![cut_line()];
        type Row<'a> = (&'a str, Vec<(Vec<u8>, Vec<String>)>);
        let cases: Vec<Row> = vec![
            (
                "split line",
                vec![
                    (
                        b"closewindow>>1\nactiv".to_vec(),
                        vec!["closewindow>>1".into()],
                    ),
                    (b"ewindowv2>>2\n".to_vec(), vec!["activewindowv2>>2".into()]),
                ],
            ),
            (
                "two lines in one push",
                vec![(b"a>>1\nb>>2\n".to_vec(), vec!["a>>1".into(), "b>>2".into()])],
            ),
            ("no newline", vec![(b"abc".to_vec(), vec![])]),
            ("empty push", vec![(vec![], vec![])]),
            ("empty line", vec![(b"\n".to_vec(), vec![String::new()])]),
            (
                "split mid character",
                vec![
                    (b"t>>\xC3".to_vec(), vec![]),
                    (b"\xA9\n".to_vec(), vec!["t>>é".into()]),
                ],
            ),
            ("cut inside a character", vec![(cut, cut_want)]),
        ];
        for (name, pushes) in cases {
            let mut buffer = LineBuffer::default();
            for (i, (bytes, want)) in pushes.into_iter().enumerate() {
                assert_eq!(buffer.push(&bytes), want, "{name} push {i}");
            }
        }
    }

    #[test]
    fn dispatch_result_cases() {
        type Row<'a> = (&'a str, Result<String, IpcError>, Result<(), String>);
        let cases: Vec<Row> = vec![
            ("ok", Ok("ok".into()), Ok(())),
            (
                "other reply",
                Ok("No such window found".into()),
                Err("No such window found".into()),
            ),
            (
                "bad workspace",
                Ok("Bad workspace".into()),
                Err("Bad workspace".into()),
            ),
            ("trailing newline", Ok("ok\n".into()), Err("ok\n".into())),
            ("empty", Ok(String::new()), Err(String::new())),
            (
                "timeout",
                Err(IpcError::Timeout),
                Err("timed out after 1 s".into()),
            ),
        ];
        for (name, reply, want) in cases {
            assert_eq!(dispatch_result(reply), want, "{name}");
        }
    }

    #[test]
    fn workspace_dispatch_cases() {
        let cases = [
            ("slot", "EVE2", "/dispatch workspace name:EVE2"),
            ("other name", "Games", "/dispatch workspace name:Games"),
            ("empty", "", "/dispatch workspace name:"),
        ];
        for (name, workspace, want) in cases {
            assert_eq!(workspace_dispatch(workspace), want, "{name}");
        }
    }

    #[test]
    fn error_display_cases() {
        let cases: Vec<(&str, IpcError, String)> = vec![
            (
                "env",
                IpcError::Env("XDG_RUNTIME_DIR"),
                "XDG_RUNTIME_DIR is unset or empty".into(),
            ),
            (
                "connect",
                IpcError::Connect {
                    path: PathBuf::from("/run/user/1000/hypr/sig/.socket.sock"),
                    source: io::Error::from_raw_os_error(2),
                },
                "connect /run/user/1000/hypr/sig/.socket.sock: No such file or directory (os error 2)"
                    .into(),
            ),
            ("eof", IpcError::Eof, "EOF".into()),
            ("timeout", IpcError::Timeout, "timed out after 1 s".into()),
            (
                "io",
                IpcError::Io(io::Error::from_raw_os_error(32)),
                "Broken pipe (os error 32)".into(),
            ),
            ("not utf-8", IpcError::NotUtf8, "reply is not UTF-8".into()),
            (
                "bad event",
                IpcError::BadEvent("openwindow"),
                "malformed openwindow data".into(),
            ),
        ];
        for (name, error, want) in cases {
            assert_eq!(error.to_string(), want, "{name}");
        }
    }

    #[test]
    fn sockets_cases() {
        let os = |s: &'static str| Some(OsStr::new(s));
        type Row<'a> = (
            &'a str,
            Option<&'a OsStr>,
            Option<&'a OsStr>,
            Result<Sockets, &'a str>,
        );
        let cases: Vec<Row> = vec![
            (
                "both set",
                os("/run/user/1000"),
                os("sig"),
                Ok(Sockets {
                    events: PathBuf::from("/run/user/1000/hypr/sig/.socket2.sock"),
                    requests: PathBuf::from("/run/user/1000/hypr/sig/.socket.sock"),
                    control: PathBuf::from("/run/user/1000/hypr/sig/.hypr-eve-preview.sock"),
                }),
            ),
            ("runtime dir unset", None, os("sig"), Err("XDG_RUNTIME_DIR")),
            (
                "runtime dir empty",
                os(""),
                os("sig"),
                Err("XDG_RUNTIME_DIR"),
            ),
            (
                "signature unset",
                os("/run/user/1000"),
                None,
                Err("HYPRLAND_INSTANCE_SIGNATURE"),
            ),
            (
                "signature empty",
                os("/run/user/1000"),
                os(""),
                Err("HYPRLAND_INSTANCE_SIGNATURE"),
            ),
            ("both unset", None, None, Err("XDG_RUNTIME_DIR")),
        ];
        for (name, runtime, signature, want) in cases {
            let got = sockets(runtime, signature).map_err(|e| match e {
                IpcError::Env(var) => var,
                other => panic!("{name}: unexpected error {other}"),
            });
            assert_eq!(got, want, "{name}");
        }
    }

    enum Server {
        Reply(Vec<Vec<u8>>),
        Hold,
        Missing,
    }

    #[derive(Debug, PartialEq)]
    enum Outcome {
        Reply(String),
        Timeout,
        NotUtf8,
        ConnectNotFound,
    }

    fn serve(listener: UnixListener, server: Server) -> thread::JoinHandle<String> {
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            let received = String::from_utf8_lossy(&buf[..n]).into_owned();
            match server {
                Server::Reply(chunks) => {
                    for chunk in chunks {
                        stream.write_all(&chunk).unwrap();
                        thread::sleep(Duration::from_millis(10));
                    }
                }
                Server::Hold => thread::sleep(Duration::from_secs(2)),
                Server::Missing => {}
            }
            received
        })
    }

    #[test]
    fn request_cases() {
        let big = vec![b'x'; 20_000];
        let big_chunks: Vec<Vec<u8>> = big.chunks(7_000).map(<[u8]>::to_vec).collect();
        let cases: Vec<(&str, Server, Outcome, Duration, Duration)> = vec![
            (
                "ok",
                Server::Reply(vec![b"ok".to_vec()]),
                Outcome::Reply("ok".into()),
                Duration::ZERO,
                Duration::from_secs(1),
            ),
            (
                "reply in pieces",
                Server::Reply(big_chunks),
                Outcome::Reply("x".repeat(20_000)),
                Duration::ZERO,
                Duration::from_secs(1),
            ),
            (
                "no reply",
                Server::Hold,
                Outcome::Timeout,
                Duration::from_secs(1),
                Duration::from_secs(2),
            ),
            (
                "not utf-8",
                Server::Reply(vec![b"ok\xFF".to_vec()]),
                Outcome::NotUtf8,
                Duration::ZERO,
                Duration::from_secs(1),
            ),
            (
                "missing socket",
                Server::Missing,
                Outcome::ConnectNotFound,
                Duration::ZERO,
                Duration::from_secs(1),
            ),
        ];
        for (i, (name, server, want, min, max)) in cases.into_iter().enumerate() {
            let dir = TempDir::new(&format!("ipc-rq{i}"));
            let path = socket(&dir);
            let handle = match server {
                Server::Missing => None,
                server => Some(serve(UnixListener::bind(&path).unwrap(), server)),
            };
            let start = Instant::now();
            let got = match request(&path, "/dispatch workspace name:EVE2") {
                Ok(reply) => Outcome::Reply(reply),
                Err(IpcError::Timeout) => Outcome::Timeout,
                Err(IpcError::NotUtf8) => Outcome::NotUtf8,
                Err(IpcError::Connect { path: got, source })
                    if got == path && source.kind() == io::ErrorKind::NotFound =>
                {
                    Outcome::ConnectNotFound
                }
                Err(other) => panic!("{name}: unexpected error {other}"),
            };
            let elapsed = start.elapsed();
            assert_eq!(got, want, "{name}");
            assert!(
                elapsed >= min && elapsed < max,
                "{name}: elapsed {elapsed:?}"
            );
            if let Some(handle) = handle {
                assert_eq!(
                    handle.join().unwrap(),
                    "/dispatch workspace name:EVE2",
                    "{name}"
                );
            }
        }
    }

    enum Step {
        Write(&'static [u8]),
        Read(&'static [&'static str]),
        Close,
        ReadEof,
    }

    #[test]
    fn event_socket_cases() {
        use Step::*;
        let cases: Vec<(&str, Vec<Step>)> = vec![
            (
                "burst of two lines",
                vec![
                    Write(b"closewindow>>1\nactivewindowv2>>2\n"),
                    Read(&["closewindow>>1", "activewindowv2>>2"]),
                ],
            ),
            (
                "line split across writes",
                vec![
                    Write(b"closewindow>>1\nactiv"),
                    Read(&["closewindow>>1"]),
                    Write(b"ewindowv2>>2\n"),
                    Read(&["activewindowv2>>2"]),
                ],
            ),
            ("nothing to read", vec![Read(&[])]),
            ("peer closes", vec![Close, ReadEof]),
        ];
        for (i, (name, steps)) in cases.into_iter().enumerate() {
            let dir = TempDir::new(&format!("ipc-ev{i}"));
            let listener = UnixListener::bind(socket(&dir)).unwrap();
            let mut socket = EventSocket::connect(&socket(&dir)).unwrap();
            let (stream, _) = listener.accept().unwrap();
            let mut peer: Option<UnixStream> = Some(stream);
            for (j, step) in steps.into_iter().enumerate() {
                match step {
                    Write(bytes) => peer.as_mut().unwrap().write_all(bytes).unwrap(),
                    Read(want) => {
                        let got = socket.read_lines().unwrap();
                        assert_eq!(got, want, "{name} step {j}");
                    }
                    Close => drop(peer.take()),
                    ReadEof => match socket.read_lines() {
                        Err(IpcError::Eof) => {}
                        other => panic!("{name} step {j}: got {other:?}"),
                    },
                }
            }
        }
    }

    #[test]
    fn event_socket_connect_cases() {
        let cases: [(&str, bool, Result<(), io::ErrorKind>); 2] = [
            ("listening socket", true, Ok(())),
            ("missing socket", false, Err(io::ErrorKind::NotFound)),
        ];
        for (i, (name, listen, want)) in cases.into_iter().enumerate() {
            let dir = TempDir::new(&format!("ipc-evc{i}"));
            let path = socket(&dir);
            let listener = listen.then(|| UnixListener::bind(&path).unwrap());
            let got = match EventSocket::connect(&path) {
                Ok(_) => Ok(()),
                Err(IpcError::Connect { path: got, source }) if got == path => Err(source.kind()),
                Err(other) => panic!("{name}: unexpected error {other}"),
            };
            assert_eq!(got, want, "{name}");
            drop(listener);
        }
    }
}
