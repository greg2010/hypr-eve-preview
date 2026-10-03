use calloop::generic::Generic;
use calloop::ping::make_ping;
use calloop::{Interest, Mode, PostAction};

use super::App;
use crate::report::{Line, Source};

impl App {
    pub(crate) fn start_tray(&mut self) {
        let toggles = self.toggles();
        match crate::tray::Tray::start(std::process::id(), toggles, self.config.border.color) {
            Err(reason) => self.emit(&Line::TrayUnavailable { reason }),
            Ok((tray, registered)) => {
                let line = match registered {
                    Ok(()) => Line::TrayRegistered {
                        name: tray.name().to_string(),
                    },
                    Err(reason) => Line::TrayUnavailable { reason },
                };
                self.tray = Some(tray);
                self.emit(&line);
            }
        }
    }

    /// Inserts the tray source and the drain ping source. A failure ends the tray.
    pub(crate) fn watch_tray(&mut self) {
        let Some(source) = self.tray.as_ref().map(crate::tray::Tray::source) else {
            return;
        };
        if let Err(e) = self.insert_tray_sources(source) {
            self.end_tray(format!("watch: {e}"), false);
        }
    }

    fn insert_tray_sources(
        &mut self,
        source: Result<crate::tray::TraySource, String>,
    ) -> Result<(), String> {
        let source = Generic::new(source?, Interest::READ, Mode::Level);
        let token = self
            .loop_handle
            .insert_source(source, |_, _, app: &mut App| Ok(app.tray_readable()))
            .map_err(|e| e.to_string())?;
        self.tray_token = Some(token);
        let (ping, source) = make_ping().map_err(|e| e.to_string())?;
        let token = self
            .loop_handle
            .insert_source(source, |_, _, app: &mut App| app.drain_tray(false))
            .map_err(|e| e.to_string())?;
        self.ping_token = Some(token);
        self.ping = Some(ping);
        Ok(())
    }

    fn tray_readable(&mut self) -> PostAction {
        self.drain_tray(true);
        if self.tray.is_some() {
            PostAction::Continue
        } else {
            PostAction::Remove
        }
    }

    /// Runs the drain loop from a fresh `Tray::drain` pass. Does nothing without a tray.
    pub(crate) fn drain_tray(&mut self, in_tray_callback: bool) {
        if let Some(first) = self.tray.as_mut().map(crate::tray::Tray::drain) {
            self.drain_loop(first, in_tray_callback);
        }
    }

    /// Applies the tray's passes in order, at most `MAX_DRAIN_PASSES` per call. An `Err` pass is
    /// the bus error.
    pub(super) fn drain_loop(
        &mut self,
        first: Result<crate::tray::Pass, String>,
        in_tray_callback: bool,
    ) {
        let mut next = first;
        for number in 1..=crate::tray::MAX_DRAIN_PASSES {
            let crate::tray::Pass { events, more } = match next {
                Ok(pass) => pass,
                Err(reason) => {
                    self.end_tray(reason, in_tray_callback);
                    return;
                }
            };
            let bound = number == crate::tray::MAX_DRAIN_PASSES;
            let mut set_state = None;
            let mut command = false;
            for event in events {
                match event {
                    crate::tray::TrayEvent::Registered => {
                        if let Some(name) = self.tray.as_ref().map(|t| t.name().to_string()) {
                            self.emit(&Line::TrayRegistered { name });
                        }
                    }
                    crate::tray::TrayEvent::Unavailable(reason) => {
                        self.emit(&Line::TrayUnavailable { reason });
                    }
                    crate::tray::TrayEvent::Command(menu) => {
                        command = true;
                        if bound {
                            if let Some(tray) = self.tray.as_mut() {
                                tray.defer(menu);
                            }
                        } else {
                            match menu {
                                crate::tray::MenuCommand::Quit => {
                                    self.stop_with(0, "tray quit");
                                    return;
                                }
                                crate::tray::MenuCommand::Toggle(toggle) => {
                                    set_state = self.apply_command(toggle, Source::Tray);
                                }
                            }
                        }
                    }
                }
            }
            if bound {
                if command || more {
                    self.emit(&Line::TrayUnavailable {
                        reason: format!("drain: {} passes", crate::tray::MAX_DRAIN_PASSES),
                    });
                    if let Some(ping) = &self.ping {
                        ping.ping();
                    }
                }
                return;
            }
            next = match set_state {
                Some(pass) => pass,
                None if more => match self.tray.as_mut() {
                    Some(tray) => tray.drain(),
                    None => return,
                },
                None => return,
            };
        }
    }

    /// Reports the tray as unavailable and ends it: its sources leave the loop and an idle
    /// callback drops it, after every source callback of this dispatch has returned.
    fn end_tray(&mut self, reason: String, in_tray_callback: bool) {
        self.emit(&Line::TrayUnavailable { reason });
        if let Some(token) = self.tray_token.take()
            && !in_tray_callback
        {
            self.loop_handle.remove(token);
        }
        if let Some(token) = self.ping_token.take() {
            self.loop_handle.remove(token);
        }
        self.ping = None;
        if let Some(tray) = self.tray.take() {
            self.loop_handle.insert_idle(move |_: &mut App| drop(tray));
        }
    }
}
