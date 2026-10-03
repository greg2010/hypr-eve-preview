use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{Interest, Mode, PostAction};

use super::{App, ConnectionTokens};
use crate::exit::{SetupError, failure, loop_error};
use crate::overlay::alpha_factor;
use crate::report::{Line, Source};

impl App {
    /// Applies a lock, snap, visibility or opacity command. Returns what `Tray::set_state`
    /// returned: `Some` only when the state changed and a tray is up. It never drains the tray.
    pub(super) fn apply_command(
        &mut self,
        command: crate::control::Command,
        source: Source,
    ) -> Option<Result<crate::tray::Pass, String>> {
        let before = self.toggles();
        let after = crate::control::apply(before, command, self.stop.is_some());
        if after == before {
            return None;
        }
        if after.locked != before.locked {
            self.layout.locked = after.locked;
            self.gestures.set_locked(after.locked);
            self.emit(&Line::Lock {
                locked: after.locked,
                source,
            });
            self.request_save();
        }
        if after.snapping != before.snapping {
            self.layout.snapping = after.snapping;
            self.emit(&Line::Snap {
                snapping: after.snapping,
                source,
            });
            self.request_save();
        }
        if after.hidden != before.hidden {
            if after.hidden {
                self.hide();
            } else {
                self.show();
            }
            self.emit(&Line::Visibility {
                hidden: after.hidden,
                source,
            });
        }
        if after.opacity != before.opacity {
            self.layout.opacity = Some(after.opacity);
            let addresses: Vec<u64> = self
                .records
                .iter()
                .filter(|(address, record)| {
                    record.overlay.is_some() && self.hovered != Some(**address)
                })
                .map(|(address, _)| *address)
                .collect();
            for address in addresses {
                let factor = alpha_factor(self.effective_opacity(address));
                self.set_alpha(address, factor);
                self.rerender_chrome(address);
            }
            self.emit(&Line::Opacity {
                percent: after.opacity,
                source,
            });
            self.request_save();
        }
        self.tray.as_mut().map(|tray| tray.set_state(after))
    }

    fn control_error(&mut self, error: impl Into<String>) {
        self.emit(&Line::ControlError {
            error: error.into(),
        });
    }

    fn control_reply_error(&mut self, result: Result<(), crate::control::ReplyError>) {
        if let Err(e) = result {
            self.control_error(format!("reply: {e}"));
        }
    }

    fn control_shutdown_error(&mut self, result: std::io::Result<()>) {
        if let Err(e) = result {
            self.control_error(format!("shutdown: {e}"));
        }
    }

    pub(crate) fn insert_control_source(&mut self) -> Result<(), SetupError> {
        let Some(server) = self.control.as_ref() else {
            return Ok(());
        };
        let fd = server.listener_fd().map_err(loop_error)?;
        let source = Generic::new(fd, Interest::READ, Mode::Level);
        let token = self
            .loop_handle
            .insert_source(source, |_, _, app: &mut App| Ok(app.accept_connections()))
            .map_err(loop_error)?;
        self.control_loop.listener = Some(token);
        Ok(())
    }

    pub(crate) fn announce_control(&mut self) {
        let Some(path) = self.control.as_ref().map(|s| s.path().to_path_buf()) else {
            return;
        };
        self.emit(&Line::ControlListening { path });
    }

    fn accept_connections(&mut self) -> PostAction {
        if self.stop.is_some() {
            return PostAction::Continue;
        }
        let Some(server) = self.control.as_mut() else {
            return PostAction::Continue;
        };
        let (admissions, error) = server.accept();
        for admission in admissions {
            match admission {
                crate::control::Admission::Open(accepted) => self.watch_connection(accepted),
                crate::control::Admission::Busy(result) => {
                    self.control_error(crate::control::Refusal::Busy.to_string());
                    self.control_reply_error(result);
                }
            }
        }
        let Some(error) = error else {
            return PostAction::Continue;
        };
        self.control_error(format!("accept: {error}"));
        self.pause_accepting()
    }

    fn pause_accepting(&mut self) -> PostAction {
        let timer = Timer::from_duration(crate::control::ACCEPT_PAUSE);
        match self
            .loop_handle
            .insert_source(timer, |_, _, app: &mut App| {
                app.control_loop.pause = None;
                app.resume_accepting();
                TimeoutAction::Drop
            }) {
            Ok(token) => {
                self.control_loop.pause = Some(token);
                PostAction::Disable
            }
            Err(e) => {
                self.stop(failure(loop_error(e)));
                PostAction::Continue
            }
        }
    }

    fn resume_accepting(&mut self) {
        let Some(token) = self.control_loop.listener else {
            return;
        };
        if let Err(e) = self.loop_handle.enable(&token) {
            self.stop(failure(loop_error(e)));
        }
    }

    fn watch_connection(&mut self, accepted: crate::control::Accepted) {
        let crate::control::Accepted { id, fd, deadline } = accepted;
        let source = Generic::new(fd, Interest::READ, Mode::Level);
        let source = match self
            .loop_handle
            .insert_source(source, move |_, _, app: &mut App| {
                Ok(app.connection_readable(id))
            }) {
            Ok(token) => token,
            Err(e) => {
                self.control_error(format!("accept: {e}"));
                self.close_connection(id);
                return;
            }
        };
        self.control_loop.connections.insert(
            id,
            ConnectionTokens {
                source,
                timer: None,
            },
        );
        let timer = Timer::from_deadline(deadline);
        match self
            .loop_handle
            .insert_source(timer, move |_, _, app: &mut App| {
                app.connection_deadline(id);
                TimeoutAction::Drop
            }) {
            Ok(token) => {
                if let Some(tokens) = self.control_loop.connections.get_mut(&id) {
                    tokens.timer = Some(token);
                }
            }
            Err(e) => {
                self.control_error(format!("accept: {e}"));
                self.forget_connection(id);
                self.close_connection(id);
            }
        }
    }

    fn close_connection(&mut self, id: crate::control::ConnectionId) {
        let result = match self.control.as_mut() {
            Some(server) => server.close(id),
            None => Ok(()),
        };
        self.control_shutdown_error(result);
    }

    fn reply_connection(
        &mut self,
        id: crate::control::ConnectionId,
        result: Result<(), crate::control::Refusal>,
    ) {
        let result = match self.control.as_mut() {
            Some(server) => server.reply(id, result),
            None => Ok(()),
        };
        self.control_reply_error(result);
    }

    pub(super) fn forget_connection(&mut self, id: crate::control::ConnectionId) {
        if let Some(tokens) = self.control_loop.connections.remove(&id) {
            self.loop_handle.remove(tokens.source);
            if let Some(timer) = tokens.timer {
                self.loop_handle.remove(timer);
            }
        }
    }

    fn connection_readable(&mut self, id: crate::control::ConnectionId) -> PostAction {
        let Some(server) = self.control.as_mut() else {
            return PostAction::Continue;
        };
        let stopping = self.stop.is_some();
        match server.read(id) {
            crate::control::ReadOutcome::Pending => return PostAction::Continue,
            crate::control::ReadOutcome::Line(_) if stopping => self.close_connection(id),
            crate::control::ReadOutcome::Line(Ok(command)) => {
                let pass = self.apply_command(command, Source::Socket);
                self.reply_connection(id, Ok(()));
                if let Some(first) = pass {
                    self.drain_loop(first, false);
                }
            }
            crate::control::ReadOutcome::Line(Err(refusal)) => {
                self.control_error(refusal.to_string());
                self.reply_connection(id, Err(refusal));
            }
            crate::control::ReadOutcome::Closed(result) => self.control_shutdown_error(result),
            crate::control::ReadOutcome::Failed { read, shutdown } => {
                self.control_error(format!("read: {read}"));
                self.control_shutdown_error(shutdown);
            }
        }
        self.forget_connection(id);
        PostAction::Continue
    }

    fn connection_deadline(&mut self, id: crate::control::ConnectionId) {
        if self.stop.is_some() {
            return;
        }
        if let Some(tokens) = self.control_loop.connections.get_mut(&id) {
            tokens.timer = None;
        }
        let timed_out = self.control.as_mut().and_then(|server| server.timeout(id));
        if let Some(result) = timed_out {
            self.control_error(crate::control::Refusal::Timeout.to_string());
            self.control_reply_error(result);
        }
        self.forget_connection(id);
    }
}
