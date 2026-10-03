use std::os::fd::AsFd;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{Interest, Mode, PostAction};

use super::startup::query;
use super::{App, ClientState};
use crate::capture::{Capture, Held, HeldFrame, Input, Teardown};
use crate::clients::Change;
use crate::exit::{SetupError, failure, loop_error};
use crate::geometry::Point;
use crate::input::PointerInput;
use crate::placement::{Origin, monitor_for};
use crate::report::Line;
use crate::surface::{SurfaceAction, SurfaceEvent, SurfaceState, surface_step};
use crate::{capture, hypr, ipc, layout};

const RETRY_AFTER: Duration = Duration::from_millis(100);

impl App {
    fn arm_retry(&mut self, address: u64) {
        let timer = Timer::from_duration(RETRY_AFTER);
        match self
            .loop_handle
            .insert_source(timer, move |_, _, app: &mut App| {
                app.retries.remove(&address);
                app.lookup(address);
                TimeoutAction::Drop
            }) {
            Ok(token) => {
                self.retries.insert(address, token);
            }
            Err(e) => self.stop(failure(loop_error(e))),
        }
    }

    pub(crate) fn apply_changes(&mut self, changes: Vec<Change>) {
        for change in changes {
            if self.stop.is_some() {
                return;
            }
            match change {
                Change::Lookup { address } => self.lookup(address),
                Change::Retry { address } => self.arm_retry(address),
                Change::Added { address } => self.add_client(address),
                Change::Removed { address, reason } => self.remove_client(address, reason),
                Change::Account { address, error } => {
                    self.emit(&Line::Account { address, error });
                }
                Change::Skipped {
                    address,
                    pid,
                    error,
                } => self.emit(&Line::WindowSkipped {
                    address,
                    pid,
                    error,
                }),
                Change::Title {
                    address,
                    label_changed,
                    key_changed,
                } => self.title_changed(address, label_changed, key_changed),
                Change::Workspace {
                    address,
                    label_changed,
                } => self.workspace_changed(address, label_changed),
                Change::RingOwner { address, previous } => self.ring_changed(address, previous),
            }
        }
        self.cancel_stale_retries();
    }

    fn cancel_stale_retries(&mut self) {
        let clients = &self.clients;
        let stale = stale_retries(self.retries.keys().copied(), |a| clients.is_pending(a));
        for address in stale {
            if let Some(token) = self.retries.remove(&address) {
                self.loop_handle.remove(token);
            }
        }
    }

    fn lookup(&mut self, address: u64) {
        let entries = match query(&self.requests, "j/clients", hypr::parse_clients) {
            Ok(entries) => entries,
            Err(reason) => {
                self.stop_with(1, reason);
                return;
            }
        };
        let changes = match entries.iter().find(|c| c.address == address) {
            Some(entry) => self.clients.add(entry),
            None => self.clients.lookup_missed(address),
        };
        self.apply_changes(changes);
    }

    fn add_client(&mut self, address: u64) {
        let Some(tracked) = self.clients.get(address) else {
            return;
        };
        let account = tracked.key();
        let line = Line::ClientAdded {
            address,
            pid: tracked.pid,
            workspace: tracked.workspace.clone(),
            slot: tracked.slot(),
            account: account.as_ref().map(ToString::to_string),
            label: tracked.label(),
        };
        let saved = account.and_then(|k| self.layout.entries.get(&k.to_string()).cloned());
        let configured = &self.config.thumbnail;
        let (origin, monitor, width, position) = match saved {
            Some(entry) => {
                let monitor = monitor_for(
                    &self.monitors,
                    entry.output.as_deref(),
                    self.default_monitor,
                );
                (
                    Origin::Saved,
                    monitor,
                    layout::effective_width(entry.width, configured, self.usable(monitor)),
                    Point {
                        x: entry.x,
                        y: entry.y,
                    },
                )
            }
            None => (
                Origin::Default,
                self.default_monitor,
                layout::effective_width(
                    configured.width,
                    configured,
                    self.usable(self.default_monitor),
                ),
                Point { x: 0, y: 0 },
            ),
        };
        let handle = hypr::capture_handle(address);
        let capture = (!self.hidden).then(|| Capture::new(self.mode, address, handle));
        self.records.insert(
            address,
            ClientState {
                capture,
                overlay: None,
                travellers: Vec::new(),
                surface_state: SurfaceState::Home,
                monitor,
                shown: None,
                buffers: Default::default(),
                frame: None,
                timer: None,
                position,
                width,
                origin,
                buffer_size: None,
                chromed: false,
            },
        );
        self.emit(&line);
        self.feed(address, Input::Start);
        self.recompute_placement();
    }

    fn remove_client(&mut self, address: u64, reason: String) {
        if let Some(mut record) = self.records.remove(&address) {
            let effects = self.gestures.handle(PointerInput::Removed { address });
            self.run_effects(None, effects);
            if self.press.is_some_and(|p| p.address == address) {
                self.press = None;
            }
            if self.hovered == Some(address) {
                self.hovered = None;
            }
            self.tear_down(address, &mut record);
        }
        self.emit(&Line::ClientRemoved { address, reason });
        self.recompute_placement();
    }

    /// Releases everything the record holds, in the order `capture::teardown` plans.
    fn tear_down(&mut self, address: u64, record: &mut ClientState) {
        let held = Held {
            timer: record.timer.is_some(),
            frame: match &record.frame {
                None => HeldFrame::None,
                Some(frame) if frame.got_event => HeldFrame::Answered,
                Some(_) => HeldFrame::Unanswered,
            },
            overlay: record.overlay.is_some(),
            buffers: std::array::from_fn(|slot| record.buffers[slot].is_some()),
        };
        for step in capture::teardown(held) {
            match step {
                Teardown::CancelTimer => {
                    if let Some(token) = record.timer.take() {
                        self.loop_handle.remove(token);
                    }
                }
                Teardown::DestroyFrame => {
                    if let Some(frame) = record.frame.take() {
                        frame.proxy.destroy();
                    }
                }
                Teardown::OrphanFrame => {
                    if let Some(frame) = record.frame.take() {
                        self.orphans.push(frame);
                    }
                }
                Teardown::DestroyOverlay => {
                    let (next, actions) =
                        surface_step(&record.surface_state, SurfaceEvent::TearDown);
                    record.surface_state = next;
                    for action in actions {
                        if let SurfaceAction::Destroy(monitor) = action {
                            record.destroy_traveller(monitor);
                        }
                    }
                    if let Some(overlay) = record.overlay.take() {
                        overlay.destroy();
                    }
                }
                Teardown::DestroyBuffer { slot } => {
                    if let Some(buffer) = record.buffers[slot].take() {
                        buffer.destroy();
                    }
                }
                Teardown::ReleaseImports => self.imports.release_owner(address),
            }
        }
    }

    pub(super) fn hide(&mut self) {
        self.end_pointer_interaction();
        let addresses: Vec<u64> = self.records.keys().copied().collect();
        for address in addresses {
            let Some(mut record) = self.records.remove(&address) else {
                continue;
            };
            self.tear_down(address, &mut record);
            record.capture = None;
            record.buffer_size = None;
            record.shown = None;
            record.chromed = false;
            self.records.insert(address, record);
        }
        self.hidden = true;
    }

    pub(super) fn show(&mut self) {
        self.hidden = false;
        let addresses: Vec<u64> = self.records.keys().copied().collect();
        for address in addresses {
            let handle = hypr::capture_handle(address);
            if let Some(record) = self.records.get_mut(&address) {
                record.capture = Some(Capture::new(self.mode, address, handle));
            }
            self.feed(address, Input::Start);
        }
    }

    fn title_changed(&mut self, address: u64, label_changed: bool, key_changed: bool) {
        let Some(tracked) = self.clients.get(address) else {
            return;
        };
        let line = Line::Title {
            address,
            label: tracked.label(),
            account: tracked.key().map(|k| k.to_string()),
        };
        self.emit(&line);
        if key_changed {
            self.key_changed(address);
        }
        if label_changed {
            self.rerender_chrome(address);
        }
    }

    fn workspace_changed(&mut self, address: u64, label_changed: bool) {
        let Some(tracked) = self.clients.get(address) else {
            return;
        };
        let line = Line::Workspace {
            address,
            workspace: tracked.workspace.clone(),
            slot: tracked.slot(),
            label: tracked.label(),
        };
        self.emit(&line);
        if label_changed {
            self.rerender_chrome(address);
        }
        self.recompute_placement();
    }

    fn ring_changed(&mut self, address: Option<u64>, previous: Option<u64>) {
        if let Some(old) = previous {
            self.rerender_chrome(old);
        }
        if let Some(new) = address {
            self.rerender_chrome(new);
        }
        self.emit(&Line::Focus { address });
    }

    fn read_events(&mut self) -> PostAction {
        if self.stop.is_some() {
            return PostAction::Continue;
        }
        match self.events.read_lines() {
            Ok(lines) => {
                for line in lines {
                    if self.stop.is_some() {
                        break;
                    }
                    self.handle_line(line);
                }
                PostAction::Continue
            }
            Err(e) => {
                self.stop_with(1, format!("event socket: {e}"));
                PostAction::Disable
            }
        }
    }

    fn handle_line(&mut self, line: String) {
        match ipc::parse_event(&line) {
            Ok(Some(event)) => {
                let changes = self.clients.apply(&event);
                self.apply_changes(changes);
            }
            Ok(None) => {}
            Err(_) => self.emit(&Line::Ignored { line }),
        }
    }

    pub(crate) fn insert_event_source(&mut self) -> Result<(), SetupError> {
        let fd = self
            .events
            .as_fd()
            .try_clone_to_owned()
            .map_err(loop_error)?;
        let source = Generic::new(fd, Interest::READ, Mode::Level);
        let token = self
            .loop_handle
            .insert_source(source, |_, _, app: &mut App| Ok(app.read_events()))
            .map_err(loop_error)?;
        self.event_token = Some(token);
        Ok(())
    }
}

fn stale_retries(armed: impl Iterator<Item = u64>, is_pending: impl Fn(u64) -> bool) -> Vec<u64> {
    armed.filter(|a| !is_pending(*a)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_retries_cases() {
        type Case<'a> = (&'a str, &'a [u64], &'a [u64], Vec<u64>);
        let cases: [Case; 4] = [
            ("nothing armed", &[], &[7], vec![]),
            ("armed and pending", &[7, 8], &[7, 8], vec![]),
            ("pending set empty", &[7, 8], &[], vec![7, 8]),
            ("one still pending", &[7, 8], &[8], vec![7]),
        ];
        for (name, armed, pending, want) in cases {
            let got = stale_retries(armed.iter().copied(), |a| pending.contains(&a));
            assert_eq!(got, want, "{name}");
        }
    }
}
