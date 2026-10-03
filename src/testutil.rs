use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::hypr::Monitor;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh empty directory under the system temp dir, removed on drop.
pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub(crate) fn new(label: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hypr-eve-preview-test-{label}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_dir_all(&self.path) {
            eprintln!("remove {}: {err}", self.path.display());
        }
    }
}

pub(crate) fn monitor(
    name: &str,
    x: i32,
    y: i32,
    width: u32,
    scale: f64,
    focused: bool,
) -> Monitor {
    Monitor {
        name: name.to_string(),
        x,
        y,
        width,
        height: width * 9 / 16,
        scale,
        transform: 0,
        reserved: [0, 34, 0, 0],
        focused,
    }
}

pub(crate) fn layout_monitors() -> Vec<Monitor> {
    vec![
        monitor("DP-1", 0, 0, 1920, 1.0, false),
        monitor("DP-3", 1920, 0, 3840, 1.5, true),
        monitor("HDMI-A-1", 5000, 0, 1920, 1.0, false),
    ]
}
