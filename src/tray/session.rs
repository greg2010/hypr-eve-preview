use std::os::fd::OwnedFd;
use std::time::Duration;

use dbus::channel::{BusType, Channel};
use dbus::message::MessageType;
use dbus::{Message, Path};
use dbus_crossroads::Crossroads;
use rustix::process::{PidfdFlags, PidfdGetfdFlags, getpid, pidfd_getfd, pidfd_open};

use super::icon::icon;
use super::menu::{MenuCommand, MenuState};
use super::objects::{SNI_PATH, Sni, menu_path, objects, state_signals};
use crate::config::Color;
use crate::control;

/// Timeout of each blocking bus call at startup.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(1);
/// Most passes the caller runs in one loop turn.
pub const MAX_DRAIN_PASSES: usize = 16;

const BUS_DOWN: &str = "session bus: disconnected";

const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const BUS_NAME: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
const DO_NOT_QUEUE: u32 = 4;
const PRIMARY_OWNER: u32 = 1;

/// What one pass learned, in order, with at most one command last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayEvent {
    Command(MenuCommand),
    Registered,
    Unavailable(String),
}

/// The result of one `Tray::drain` pass. `more` is true when the pass ended at a command or
/// flushed, so another pass may find queued messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pass {
    pub events: Vec<TrayEvent>,
    pub more: bool,
}

/// A duplicate of the bus connection's watch fd, owned by the caller's event source.
pub type TraySource = OwnedFd;

/// The status notifier item on the session bus. Single-threaded: every call runs on the
/// caller's thread.
#[derive(Debug)]
pub struct Tray {
    channel: Channel,
    crossroads: Crossroads,
    name: String,
    register_serial: Option<(u32, String)>,
    watch: OwnedFd,
}

fn match_rule() -> String {
    format!(
        "type='signal',sender='{BUS_NAME}',path='{BUS_PATH}',interface='{BUS_NAME}',\
         member='NameOwnerChanged',arg0='{WATCHER}'"
    )
}

fn bus_call(
    destination: &str,
    path: &str,
    interface: &str,
    member: &str,
) -> Result<Message, String> {
    Message::new_method_call(destination, path, interface, member)
}

impl Tray {
    /// Connects to the session bus, then runs [`Tray::with_channel`].
    ///
    /// `Err` means no tray: the connection failed or `with_channel` failed before the
    /// register call.
    pub fn start(
        pid: u32,
        toggles: control::Toggles,
        color: Color,
    ) -> Result<(Tray, Result<(), String>), String> {
        let channel = Channel::get_private(BusType::Session)
            .map_err(|error| format!("session bus: {error}"))?;
        Tray::with_channel(channel, pid, toggles, color)
    }

    /// Claims `org.kde.StatusNotifierItem-<pid>-1`, adds the watcher match rule, serves the
    /// objects and registers with the watcher, on a connected, registered `channel`. The inner
    /// result is the register call: the tray stays up on `Err` and registers when the watcher
    /// appears. `Err` means no tray: watch fd, name, match rule or `session bus: disconnected`.
    pub fn with_channel(
        mut channel: Channel,
        pid: u32,
        toggles: control::Toggles,
        color: Color,
    ) -> Result<(Tray, Result<(), String>), String> {
        channel.set_watch_enabled(true);
        if !channel.is_connected() {
            return Err(BUS_DOWN.to_string());
        }
        let own = pidfd_open(getpid(), PidfdFlags::empty())
            .map_err(|error| format!("watch: pidfd_open: {error}"))?;
        let watch = pidfd_getfd(&own, channel.watch().fd, PidfdGetfdFlags::empty())
            .map_err(|error| format!("watch: pidfd_getfd: {error}"))?;
        let name = format!("org.kde.StatusNotifierItem-{pid}-1");
        let named = |error: String| format!("name {name}: {error}");
        let request = bus_call(BUS_NAME, BUS_PATH, BUS_NAME, "RequestName")
            .map_err(named)?
            .append2(name.as_str(), DO_NOT_QUEUE);
        let reply = channel
            .send_with_reply_and_block(request, CALL_TIMEOUT)
            .map_err(|error| named(error.to_string()))?;
        let code = reply
            .read1::<u32>()
            .map_err(|error| named(error.to_string()))?;
        if code != PRIMARY_OWNER {
            return Err(named(format!("reply code {code}")));
        }

        let add_match = bus_call(BUS_NAME, BUS_PATH, BUS_NAME, "AddMatch")
            .map_err(|error| format!("match rule: {error}"))?
            .append1(match_rule());
        channel
            .send_with_reply_and_block(add_match, CALL_TIMEOUT)
            .map_err(|error| format!("match rule: {error}"))?;

        let state = MenuState {
            toggles,
            revision: 1,
            pending: vec![],
        };
        let tray = Tray {
            channel,
            crossroads: objects(state, icon(color)),
            name,
            register_serial: None,
            watch,
        };
        let registered = tray.register_blocking();
        Ok((tray, registered))
    }

    fn register_message(&self) -> Result<Message, String> {
        bus_call(
            WATCHER,
            "/StatusNotifierWatcher",
            WATCHER,
            "RegisterStatusNotifierItem",
        )
        .map(|message| message.append1(self.name.as_str()))
        .map_err(|error| format!("register: {error}"))
    }

    fn register_blocking(&self) -> Result<(), String> {
        let message = self.register_message()?;
        self.channel
            .send_with_reply_and_block(message, CALL_TIMEOUT)
            .map(|_| ())
            .map_err(|error| register_failure(error.name(), error.message()))
    }

    /// The bus name this tray owns.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Hands out a duplicate of the watch fd taken at startup, for the caller's event loop.
    ///
    /// `Err` is `dup: <error>`.
    pub fn source(&self) -> Result<TraySource, String> {
        self.watch
            .try_clone()
            .map_err(|error| format!("dup: {error}"))
    }

    fn menu_state(&mut self) -> Option<&mut MenuState> {
        self.crossroads.data_mut::<MenuState>(&menu_path())
    }

    fn take_pending(&mut self) -> Option<MenuCommand> {
        self.menu_state()
            .filter(|state| !state.pending.is_empty())
            .map(|state| state.pending.remove(0))
    }

    /// Puts `command` first in the pending list, so the next pass returns it. Does no bus
    /// I/O.
    pub fn defer(&mut self, command: MenuCommand) {
        if let Some(state) = self.menu_state() {
            state.pending.insert(0, command);
        }
    }

    /// Runs one pass: returns the first pending command, or reads the connection, serves
    /// the method calls and watcher signals it holds, and stops at the first command.
    ///
    /// `Err` is the bus error: the connection is down and the tray is unusable.
    pub fn drain(&mut self) -> Result<Pass, String> {
        if let Some(command) = self.take_pending() {
            return Ok(Pass {
                events: vec![TrayEvent::Command(command)],
                more: true,
            });
        }
        if self.channel.read_write(Some(Duration::ZERO)).is_err() {
            return Err(BUS_DOWN.to_string());
        }
        let mut events = Vec::new();
        let mut sent = false;
        let mut at_command = false;
        while let Some(message) = self.channel.pop_message() {
            match message.msg_type() {
                MessageType::MethodCall => {
                    sent = true;
                    // Err only means the message was no method call, which the match rules out.
                    if self
                        .crossroads
                        .handle_message(message, &self.channel)
                        .is_err()
                    {
                        continue;
                    }
                    if self
                        .menu_state()
                        .is_some_and(|state| !state.pending.is_empty())
                    {
                        at_command = true;
                        break;
                    }
                }
                MessageType::Signal => sent |= self.on_signal(&message, &mut events)?,
                MessageType::MethodReturn | MessageType::Error => {
                    events.extend(self.on_reply(message));
                }
            }
        }
        if sent {
            self.channel.flush();
        }
        if !self.channel.is_connected() {
            return Err(BUS_DOWN.to_string());
        }
        if at_command {
            events.extend(self.take_pending().map(TrayEvent::Command));
        }
        Ok(Pass {
            events,
            more: at_command || sent,
        })
    }

    fn on_signal(
        &mut self,
        message: &Message,
        events: &mut Vec<TrayEvent>,
    ) -> Result<bool, String> {
        let owner_change = message.sender().as_deref() == Some(BUS_NAME)
            && message.interface().as_deref() == Some(BUS_NAME)
            && message.member().as_deref() == Some("NameOwnerChanged");
        if !owner_change {
            return Ok(false);
        }
        let Ok((subject, _, new_owner)) = message.read3::<String, String, String>() else {
            return Ok(false);
        };
        if subject != WATCHER {
            return Ok(false);
        }
        if new_owner.is_empty() {
            events.push(TrayEvent::Unavailable(
                "StatusNotifierWatcher left the bus".to_string(),
            ));
            return Ok(false);
        }
        match self.register_message() {
            Ok(register) => {
                let serial = self
                    .channel
                    .send(register)
                    .map_err(|()| BUS_DOWN.to_string())?;
                self.register_serial = Some((serial, new_owner));
                Ok(true)
            }
            Err(reason) => {
                events.push(TrayEvent::Unavailable(reason));
                Ok(false)
            }
        }
    }

    fn on_reply(&mut self, mut message: Message) -> Option<TrayEvent> {
        let (serial, owner) = self.register_serial.as_ref()?;
        let from_peer = message
            .sender()
            .is_some_and(|sender| &*sender == owner.as_str() || &*sender == BUS_NAME);
        if message.get_reply_serial() != Some(*serial) || !from_peer {
            return None;
        }
        self.register_serial = None;
        Some(match message.as_result() {
            Ok(_) => TrayEvent::Registered,
            Err(error) => TrayEvent::Unavailable(register_failure(error.name(), error.message())),
        })
    }

    /// Mirrors `toggles` into the menu and the icon, sends the change signals (the two menu
    /// signals, plus `NewIcon` when `hidden` changed), flushes and runs one pass.
    ///
    /// `Err` is the bus error, as for [`Tray::drain`].
    pub fn set_state(&mut self, toggles: control::Toggles) -> Result<Pass, String> {
        if let Some(sni) = self.crossroads.data_mut::<Sni>(&Path::from(SNI_PATH)) {
            sni.hidden = toggles.hidden;
        }
        let signals = self
            .menu_state()
            .map(|state| state_signals(state, toggles))
            .unwrap_or_default();
        for signal in signals {
            if self.channel.send(signal).is_err() {
                return Err(BUS_DOWN.to_string());
            }
        }
        self.channel.flush();
        if !self.channel.is_connected() {
            return Err(BUS_DOWN.to_string());
        }
        self.drain()
    }
}

fn register_failure(name: Option<&str>, message: Option<&str>) -> String {
    format!(
        "register: {}: {}",
        name.unwrap_or("unknown"),
        message.unwrap_or("")
    )
}

#[cfg(test)]
mod tests;
