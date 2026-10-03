use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::server::Admission;
use crate::testutil::TempDir;

pub(super) fn socket(dir: &TempDir) -> PathBuf {
    dir.path().join("s")
}

pub(super) fn client(path: &Path) -> UnixStream {
    let stream = UnixStream::connect(path).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("read timeout");
    stream
}

pub(super) fn admission_text(admission: &Admission) -> &'static str {
    match admission {
        Admission::Open(_) => "open",
        Admission::Busy(Ok(())) => "busy",
        Admission::Busy(Err(_)) => "busy failed",
    }
}
