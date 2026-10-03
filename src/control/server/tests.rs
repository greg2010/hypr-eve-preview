use std::fs;

use super::super::client::ClientError;
use super::*;
use crate::control::fixtures::{admission_text, client, socket};
use crate::testutil::TempDir;

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
