use smithay_client_toolkit::error::GlobalError;

use crate::control;
use crate::dmabuf::DmabufError;
use crate::report;
use crate::report::{Line, Reporter};

pub(crate) enum Stop {
    Exit { code: u8, reason: String },
    StderrFailed,
}

#[derive(Debug)]
pub(crate) enum SetupError {
    Global { name: &'static str, reason: String },
    NoOutput(String),
    Wayland(String),
    Loop(String),
    DmabufGlobal(GlobalError),
    Allocator(DmabufError),
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupError::Global { name, reason } => write!(f, "{name}: {reason}"),
            SetupError::NoOutput(name) => write!(f, "no wl_output named {name}"),
            SetupError::Wayland(e) => write!(f, "{e}"),
            SetupError::Loop(e) => write!(f, "event loop: {e}"),
            SetupError::DmabufGlobal(e) => write!(f, "{e}"),
            SetupError::Allocator(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SetupError {}

/// The exit reason of a failed dispatch: the Wayland error's own text when the connection has one.
pub(crate) fn dispatch_reason(wayland: Option<String>, dispatch: String) -> String {
    wayland.unwrap_or_else(|| SetupError::Loop(dispatch).to_string())
}

pub(crate) fn failure(e: impl std::fmt::Display) -> Stop {
    Stop::Exit {
        code: 1,
        reason: e.to_string(),
    }
}

pub(crate) fn wayland(e: impl std::fmt::Display) -> SetupError {
    SetupError::Wayland(e.to_string())
}

pub(crate) fn loop_error(e: impl std::fmt::Display) -> SetupError {
    SetupError::Loop(e.to_string())
}

pub(crate) fn out_of_step_reason(action: &str) -> String {
    format!("{action}: no frame, overlay or buffer for the slot")
}

/// Prints the `exit` line and exits.
pub(crate) fn finish(reporter: &mut Reporter, stop: Stop, teardown: Option<String>) -> ! {
    let code = match stop {
        Stop::StderrFailed => 1,
        Stop::Exit { code, reason } => {
            let (code, line) = report::exit_line(code, reason, teardown);
            match reporter.emit(&line) {
                Ok(()) => code,
                Err(_) => 1,
            }
        }
    };
    std::process::exit(i32::from(code))
}

/// Exits with `code` after the `exit` line. `teardown` is the text of a failed cleanup.
pub(crate) fn fail(
    reporter: &mut Reporter,
    code: u8,
    reason: impl std::fmt::Display,
    teardown: Option<String>,
) -> ! {
    finish(
        reporter,
        Stop::Exit {
            code,
            reason: reason.to_string(),
        },
        teardown,
    )
}

/// Removes the control socket file and the connections. The error is a `teardown` text for
/// `finish`.
pub(crate) fn remove_control(server: control::Server) -> Option<String> {
    let path = server.path().to_path_buf();
    server
        .remove()
        .err()
        .map(|e| format!("control socket {}: remove: {e}", path.display()))
}

pub(crate) fn emit_or_finish(reporter: &mut Reporter, line: &Line) {
    if reporter.emit(line).is_err() {
        finish(reporter, Stop::StderrFailed, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_reason_cases() {
        let cases = [
            (
                "wayland error",
                Some("broken pipe"),
                "poll failed",
                "broken pipe",
            ),
            (
                "no wayland error",
                None,
                "poll failed",
                "event loop: poll failed",
            ),
        ];
        for (name, wayland, dispatch, want) in cases {
            let got = dispatch_reason(wayland.map(String::from), dispatch.to_string());
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn setup_error_display_cases() {
        let cases = [
            (
                "global",
                SetupError::Global {
                    name: "wl_compositor",
                    reason: "missing".to_string(),
                },
                "wl_compositor: missing",
            ),
            (
                "relative pointer global",
                SetupError::Global {
                    name: "zwp_relative_pointer_manager_v1",
                    reason: "not advertised by the compositor".to_string(),
                },
                "zwp_relative_pointer_manager_v1: not advertised by the compositor",
            ),
            (
                "alpha modifier global",
                SetupError::Global {
                    name: "wp_alpha_modifier_v1",
                    reason: "missing".to_string(),
                },
                "wp_alpha_modifier_v1: missing",
            ),
            (
                "no output",
                SetupError::NoOutput("HDMI-A-1".to_string()),
                "no wl_output named HDMI-A-1",
            ),
            (
                "wayland",
                SetupError::Wayland("broken pipe".to_string()),
                "broken pipe",
            ),
            (
                "loop",
                SetupError::Loop("poll failed".to_string()),
                "event loop: poll failed",
            ),
            (
                "dmabuf global missing",
                SetupError::DmabufGlobal(GlobalError::MissingGlobal("zwp_linux_dmabuf_v1")),
                "the 'zwp_linux_dmabuf_v1' global was not available",
            ),
            (
                "dmabuf global version",
                SetupError::DmabufGlobal(GlobalError::InvalidVersion {
                    name: "zwp_linux_dmabuf_v1",
                    required: 4,
                    available: 3,
                }),
                "the 'zwp_linux_dmabuf_v1' global does not support interface version 4 (using version 3)",
            ),
            (
                "allocator",
                SetupError::Allocator(DmabufError::NoRenderNode(0x2a)),
                "no /dev/dri/renderD* node with device number 0x2a",
            ),
        ];
        for (name, err, want) in cases {
            assert_eq!(err.to_string(), want, "{name}");
        }
    }

    #[test]
    fn out_of_step_reason_cases() {
        let cases = [
            ("copy", "copy: no frame, overlay or buffer for the slot"),
            (
                "recommit",
                "recommit: no frame, overlay or buffer for the slot",
            ),
            (
                "present",
                "present: no frame, overlay or buffer for the slot",
            ),
        ];
        for (action, want) in cases {
            assert_eq!(out_of_step_reason(action), want, "{action}");
        }
    }
}
