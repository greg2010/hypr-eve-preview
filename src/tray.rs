use std::collections::HashMap;
use std::fmt;
use std::os::fd::OwnedFd;
use std::time::Duration;

use dbus::arg::{RefArg, Variant};
use dbus::channel::{BusType, Channel};
use dbus::message::MessageType;
use dbus::strings::{Interface, Member};
use dbus::{Message, MethodErr, Path};
use dbus_crossroads::{Crossroads, IfaceBuilder};
use rustix::process::{PidfdFlags, PidfdGetfdFlags, getpid, pidfd_getfd, pidfd_open};

use crate::config::Color;
use crate::control;

/// Timeout of each blocking bus call at startup.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(1);
/// Most passes the caller runs in one loop turn.
pub const MAX_DRAIN_PASSES: usize = 16;

const BUS_DOWN: &str = "session bus: disconnected";
const SNI_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";
const MENU_INTERFACE: &str = "com.canonical.dbusmenu";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const BUS_NAME: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
const DO_NOT_QUEUE: u32 = 4;
const PRIMARY_OWNER: u32 = 1;
const LOCK_ID: i32 = 1;
const HIDE_ID: i32 = 2;
const QUIT_ID: i32 = 3;
const OPACITY_ID: i32 = 4;
const SNAP_ID: i32 = 5;
const ITEM_IDS: [i32; 5] = [LOCK_ID, HIDE_ID, SNAP_ID, OPACITY_ID, QUIT_ID];
const STEPS: [(i32, u32, &str); 10] = [
    (11, 10, "10%"),
    (12, 20, "20%"),
    (13, 30, "30%"),
    (14, 40, "40%"),
    (15, 50, "50%"),
    (16, 60, "60%"),
    (17, 70, "70%"),
    (18, 80, "80%"),
    (19, 90, "90%"),
    (20, 100, "100%"),
];
const SNI_INTERFACE: &str = "org.kde.StatusNotifierItem";
const HIDDEN_ICON_COLOR: Color = Color {
    r: 0xFF,
    g: 0x00,
    b: 0x00,
    a: 0xFF,
};

/// A command the menu queued for the caller to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuCommand {
    Toggle(control::Command),
    Quit,
}

/// The menu model. `pending` holds the commands that clicks queued and that no pass has
/// returned yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuState {
    pub toggles: control::Toggles,
    pub revision: u32,
    pub pending: Vec<MenuCommand>,
}

/// An icon image: ARGB bytes, row by row, not premultiplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pixmap {
    pub width: i32,
    pub height: i32,
    pub argb: Vec<u8>,
}

/// The ring icon at 24 and 48 px in `color`.
pub fn icon(color: Color) -> Vec<Pixmap> {
    [24, 48]
        .into_iter()
        .map(|size| Pixmap {
            width: size,
            height: size,
            argb: ring(size, color),
        })
        .collect()
}

fn ring(size: i32, color: Color) -> Vec<u8> {
    let side = f64::from(size);
    let centre = side / 2.0;
    let (inner, outer) = (0.30 * side, 0.45 * side);
    let on = [color.a, color.r, color.g, color.b];
    (0..size)
        .flat_map(|y| (0..size).map(move |x| (x, y)))
        .flat_map(|(x, y)| {
            let distance = (f64::from(x) + 0.5 - centre).hypot(f64::from(y) + 0.5 - centre);
            if (inner..=outer).contains(&distance) {
                on
            } else {
                [0; 4]
            }
        })
        .collect()
}

/// One dbusmenu item with its properties and children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: i32,
    pub props: Vec<(&'static str, Prop)>,
    pub children: Vec<Node>,
}

/// A dbusmenu property value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prop {
    Str(&'static str),
    Int(i32),
}

/// A menu request that names an id or property the menu does not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuError {
    UnknownId(i32),
    UnknownProperty(String),
}

impl fmt::Display for MenuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MenuError::UnknownId(id) => write!(f, "unknown menu item {id}"),
            MenuError::UnknownProperty(name) => write!(f, "unknown property {name}"),
        }
    }
}

impl std::error::Error for MenuError {}

fn step(id: i32) -> Option<(u32, &'static str)> {
    STEPS
        .iter()
        .find_map(|&(step, percent, label)| (step == id).then_some((percent, label)))
}

fn step_id(percent: u32) -> Option<i32> {
    STEPS
        .iter()
        .find_map(|&(id, step, _)| (step == percent).then_some(id))
}

fn item_props(state: &MenuState, id: i32) -> Option<Vec<(&'static str, Prop)>> {
    let toggle = |label, on: bool| {
        vec![
            ("label", Prop::Str(label)),
            ("toggle-type", Prop::Str("checkmark")),
            ("toggle-state", Prop::Int(i32::from(on))),
        ]
    };
    match id {
        0 => Some(vec![("children-display", Prop::Str("submenu"))]),
        LOCK_ID => Some(toggle("Lock thumbnails", state.toggles.locked)),
        HIDE_ID => Some(toggle("Hide thumbnails", state.toggles.hidden)),
        SNAP_ID => Some(toggle("Snap thumbnails", state.toggles.snapping)),
        OPACITY_ID => Some(vec![
            ("label", Prop::Str("Opacity")),
            ("children-display", Prop::Str("submenu")),
        ]),
        QUIT_ID => Some(vec![("label", Prop::Str("Quit"))]),
        _ => step(id).map(|(percent, label)| toggle(label, state.toggles.opacity == percent)),
    }
}

fn child_ids(id: i32) -> Vec<i32> {
    match id {
        0 => ITEM_IDS.to_vec(),
        OPACITY_ID => STEPS.iter().map(|&(id, _, _)| id).collect(),
        _ => vec![],
    }
}

/// The subtree at `parent`. The root and the opacity item have children. `depth` follows
/// dbusmenu `recursionDepth`: -1 is unlimited, 0 withholds children, n keeps n levels. An
/// empty `names` keeps every property, otherwise only the listed ones.
pub fn layout(
    state: &MenuState,
    parent: i32,
    depth: i32,
    names: &[String],
) -> Result<Node, MenuError> {
    let mut props = item_props(state, parent).ok_or(MenuError::UnknownId(parent))?;
    if !names.is_empty() {
        props.retain(|(key, _)| names.iter().any(|name| name == key));
    }
    let children = if depth == 0 {
        vec![]
    } else {
        let below = if depth < 0 { depth } else { depth - 1 };
        child_ids(parent)
            .into_iter()
            .map(|id| layout(state, id, below, names))
            .collect::<Result<Vec<_>, _>>()?
    };
    Ok(Node {
        id: parent,
        props,
        children,
    })
}

/// Queues the command of item `id` when `event_id` is `clicked`. Every other event on a
/// known item is accepted and ignored, and so is every event on the opacity submenu item.
pub fn event(state: &mut MenuState, id: i32, event_id: &str) -> Result<(), MenuError> {
    let command = match id {
        LOCK_ID => Some(MenuCommand::Toggle(control::Command::ToggleLock)),
        HIDE_ID => Some(MenuCommand::Toggle(control::Command::ToggleHide)),
        SNAP_ID => Some(MenuCommand::Toggle(control::Command::ToggleSnap)),
        QUIT_ID => Some(MenuCommand::Quit),
        OPACITY_ID => None,
        _ => {
            let (percent, _) = step(id).ok_or(MenuError::UnknownId(id))?;
            Some(MenuCommand::Toggle(control::Command::Opacity(percent)))
        }
    };
    if event_id == "clicked"
        && let Some(command) = command
    {
        state.pending.push(command);
    }
    Ok(())
}

type Props = HashMap<String, Variant<Box<dyn RefArg>>>;
type Wire = (i32, Props, Vec<Variant<Box<dyn RefArg>>>);
type Click = (i32, String, Variant<Box<dyn RefArg>>, u32);

fn prop_value(prop: &Prop) -> Variant<Box<dyn RefArg>> {
    match prop {
        Prop::Str(text) => Variant(Box::new((*text).to_string())),
        Prop::Int(number) => Variant(Box::new(*number)),
    }
}

fn props_map(props: &[(&'static str, Prop)]) -> Props {
    props
        .iter()
        .map(|(key, prop)| ((*key).to_string(), prop_value(prop)))
        .collect()
}

fn wire(node: &Node) -> Wire {
    let children = node
        .children
        .iter()
        .map(|child| Variant(Box::new(wire(child)) as Box<dyn RefArg>))
        .collect();
    (node.id, props_map(&node.props), children)
}

fn flatten(node: &Node, out: &mut Vec<(i32, Props)>) {
    out.push((node.id, props_map(&node.props)));
    for child in &node.children {
        flatten(child, out);
    }
}

fn invalid(error: MenuError) -> MethodErr {
    MethodErr::invalid_arg(&error.to_string())
}

fn refused(reason: &str) -> MethodErr {
    MethodErr::invalid_arg(reason)
}

type WireIcon = Vec<(i32, i32, Vec<u8>)>;

struct Sni {
    shown: WireIcon,
    hidden_icon: WireIcon,
    hidden: bool,
}

fn wire_icon(icon: Vec<Pixmap>) -> WireIcon {
    icon.into_iter()
        .map(|pixmap| (pixmap.width, pixmap.height, pixmap.argb))
        .collect()
}

fn register_item(crossroads: &mut Crossroads) -> dbus_crossroads::IfaceToken<Sni> {
    crossroads.register(SNI_INTERFACE, |b: &mut IfaceBuilder<Sni>| {
        let text = |value: &'static str| move |_: &mut _, _: &mut Sni| Ok(value.to_string());
        b.property::<String, _>("Category")
            .get(text("ApplicationStatus"));
        b.property::<String, _>("Id").get(text("hypr-eve-preview"));
        b.property::<String, _>("Title")
            .get(text("hypr-eve-preview"));
        b.property::<String, _>("Status").get(text("Active"));
        b.property::<WireIcon, _>("IconPixmap").get(|_, sni| {
            Ok(if sni.hidden {
                sni.hidden_icon.clone()
            } else {
                sni.shown.clone()
            })
        });
        b.signal::<(), _>("NewIcon", ());
        b.property::<Path<'static>, _>("Menu")
            .get(|_, _| Ok(menu_path()));
        b.property::<bool, _>("ItemIsMenu").get(|_, _| Ok(true));
        b.method("Activate", ("x", "y"), (), |_, _, _: (i32, i32)| Ok(()));
        b.method(
            "SecondaryActivate",
            ("x", "y"),
            (),
            |_, _, _: (i32, i32)| Ok(()),
        );
        b.method("ContextMenu", ("x", "y"), (), |_, _, _: (i32, i32)| Ok(()));
        b.method(
            "Scroll",
            ("delta", "orientation"),
            (),
            |_, _, _: (i32, String)| Ok(()),
        );
    })
}

fn register_menu(crossroads: &mut Crossroads) -> dbus_crossroads::IfaceToken<MenuState> {
    crossroads.register(MENU_INTERFACE, |b: &mut IfaceBuilder<MenuState>| {
        b.property::<u32, _>("Version").get(|_, _| Ok(3));
        b.property::<String, _>("Status")
            .get(|_, _| Ok("normal".to_string()));
        b.method(
            "GetLayout",
            ("parentId", "recursionDepth", "propertyNames"),
            ("revision", "layout"),
            |_, state: &mut MenuState, (parent, depth, names): (i32, i32, Vec<String>)| {
                let node = layout(state, parent, depth, &names).map_err(invalid)?;
                Ok((state.revision, wire(&node)))
            },
        );
        b.method(
            "GetGroupProperties",
            ("ids", "propertyNames"),
            ("properties",),
            |_, state: &mut MenuState, (ids, names): (Vec<i32>, Vec<String>)| {
                let mut out = Vec::new();
                if ids.is_empty() {
                    let root = layout(state, 0, -1, &names).map_err(invalid)?;
                    flatten(&root, &mut out);
                } else {
                    for id in ids {
                        if let Ok(node) = layout(state, id, 0, &names) {
                            flatten(&node, &mut out);
                        }
                    }
                }
                Ok((out,))
            },
        );
        b.method(
            "GetProperty",
            ("id", "name"),
            ("value",),
            |_, state: &mut MenuState, (id, name): (i32, String)| {
                let node = layout(state, id, 0, &[]).map_err(invalid)?;
                node.props
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, prop)| (prop_value(prop),))
                    .ok_or_else(|| invalid(MenuError::UnknownProperty(name)))
            },
        );
        b.method(
            "Event",
            ("id", "eventId", "data", "timestamp"),
            (),
            |_, state: &mut MenuState, (id, event_id, _, _): Click| {
                event(state, id, &event_id).map_err(invalid)
            },
        );
        b.method(
            "EventGroup",
            ("events",),
            ("idErrors",),
            |_, state: &mut MenuState, (events,): (Vec<Click>,)| {
                let total = events.len();
                let refused_ids: Vec<i32> = events
                    .into_iter()
                    .filter_map(|(id, event_id, _, _)| {
                        event(state, id, &event_id).err().map(|_| id)
                    })
                    .collect();
                if refused_ids.len() == total {
                    return Err(refused("no event accepted"));
                }
                Ok((refused_ids,))
            },
        );
        b.method(
            "AboutToShow",
            ("id",),
            ("needUpdate",),
            |_, state: &mut MenuState, (id,): (i32,)| {
                layout(state, id, 0, &[]).map_err(invalid)?;
                Ok((false,))
            },
        );
        b.method(
            "AboutToShowGroup",
            ("ids",),
            ("updatesNeeded", "idErrors"),
            |_, state: &mut MenuState, (ids,): (Vec<i32>,)| {
                let unknown: Vec<i32> = ids
                    .iter()
                    .copied()
                    .filter(|id| layout(state, *id, 0, &[]).is_err())
                    .collect();
                if !ids.is_empty() && unknown.len() == ids.len() {
                    return Err(refused("no known item"));
                }
                Ok((Vec::<i32>::new(), unknown))
            },
        );
        b.signal::<(
            Vec<(i32, HashMap<String, Variant<i32>>)>,
            Vec<(i32, Vec<String>)>,
        ), _>("ItemsPropertiesUpdated", ("updatedProps", "removedProps"));
        b.signal::<(u32, i32), _>("LayoutUpdated", ("revision", "parent"));
    })
}

/// Both object paths with their interfaces and handlers. Needs no bus. `Event` and
/// `EventGroup` only queue commands in `state.pending`. `icon` is the icon served while
/// thumbnails are shown, the red icon is served while `state.toggles.hidden` is true.
pub fn objects(state: MenuState, icon: Vec<Pixmap>) -> Crossroads {
    let mut crossroads = Crossroads::new();
    let item = register_item(&mut crossroads);
    let menu = register_menu(&mut crossroads);
    let sni = Sni {
        shown: wire_icon(icon),
        hidden_icon: wire_icon(self::icon(HIDDEN_ICON_COLOR)),
        hidden: state.toggles.hidden,
    };
    crossroads.insert(SNI_PATH, &[item], sni);
    crossroads.insert(MENU_PATH, &[menu], state);
    crossroads
}

/// Applies `toggles` to `state`, bumps the revision and returns the signals that announce
/// it: `ItemsPropertiesUpdated` with one entry per changed item, then `LayoutUpdated`, then
/// `NewIcon` when `hidden` changed. Sends nothing.
pub fn state_signals(state: &mut MenuState, toggles: control::Toggles) -> Vec<Message> {
    let changed = |id: i32, on: bool| {
        (
            id,
            HashMap::from([("toggle-state".to_string(), Variant(i32::from(on)))]),
        )
    };
    let mut updated: Vec<(i32, HashMap<String, Variant<i32>>)> = Vec::new();
    if toggles.locked != state.toggles.locked {
        updated.push(changed(LOCK_ID, toggles.locked));
    }
    if toggles.hidden != state.toggles.hidden {
        updated.push(changed(HIDE_ID, toggles.hidden));
    }
    if toggles.snapping != state.toggles.snapping {
        updated.push(changed(SNAP_ID, toggles.snapping));
    }
    if toggles.opacity != state.toggles.opacity {
        if let Some(id) = step_id(state.toggles.opacity) {
            updated.push(changed(id, false));
        }
        if let Some(id) = step_id(toggles.opacity) {
            updated.push(changed(id, true));
        }
    }
    let icon_changed = toggles.hidden != state.toggles.hidden;
    state.toggles = toggles;
    state.revision = state.revision.wrapping_add(1);
    let path = menu_path();
    let interface = Interface::from(MENU_INTERFACE);
    let mut signals = vec![
        Message::signal(&path, &interface, &Member::from("ItemsPropertiesUpdated"))
            .append2(updated, Vec::<(i32, Vec<String>)>::new()),
        Message::signal(&path, &interface, &Member::from("LayoutUpdated"))
            .append2(state.revision, 0i32),
    ];
    if icon_changed {
        signals.push(Message::signal(
            &Path::from(SNI_PATH),
            &Interface::from(SNI_INTERFACE),
            &Member::from("NewIcon"),
        ));
    }
    signals
}

fn menu_path() -> Path<'static> {
    Path::from(MENU_PATH)
}

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
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::os::unix::fs::FileTypeExt;
    use std::process::{Child, Command as Process, Stdio};
    use std::thread::sleep;
    use std::time::Instant;

    use dbus::Path;
    use dbus::arg::{AppendAll, RefArg, Variant};
    use dbus::message::MessageType;

    use super::*;
    use crate::control::{Command, Toggles};
    use crate::testutil::TempDir;

    const GREEN: Color = Color {
        r: 0x40,
        g: 0xFF,
        b: 0x00,
        a: 0xFF,
    };
    const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
    const DBUS_ERROR_INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";

    fn menu_at(locked: bool, hidden: bool, snapping: bool, opacity: u32) -> MenuState {
        MenuState {
            toggles: Toggles {
                locked,
                hidden,
                snapping,
                opacity,
            },
            revision: 1,
            pending: vec![],
        }
    }

    fn menu(locked: bool, hidden: bool, snapping: bool) -> MenuState {
        menu_at(locked, hidden, snapping, 100)
    }

    fn step_nodes(checked: Option<i32>, label_only: bool) -> Vec<Node> {
        let labels: [(i32, &'static str); 10] = [
            (11, "10%"),
            (12, "20%"),
            (13, "30%"),
            (14, "40%"),
            (15, "50%"),
            (16, "60%"),
            (17, "70%"),
            (18, "80%"),
            (19, "90%"),
            (20, "100%"),
        ];
        labels
            .into_iter()
            .map(|(id, label)| {
                let mut item = toggle_item(id, label, checked == Some(id));
                if label_only {
                    item.props.truncate(1);
                }
                item
            })
            .collect()
    }

    fn step_item(id: i32, on: bool) -> Node {
        step_nodes(on.then_some(id), false)
            .into_iter()
            .find(|item| item.id == id)
            .unwrap_or_else(|| panic!("{id} is not a step id"))
    }

    fn steps(checked: Option<i32>) -> Vec<Node> {
        step_nodes(checked, false)
    }

    fn opacity_props() -> Vec<(&'static str, Prop)> {
        vec![
            ("label", Prop::Str("Opacity")),
            ("children-display", Prop::Str("submenu")),
        ]
    }

    fn opacity_item(checked: Option<i32>) -> Node {
        node(4, opacity_props(), steps(checked))
    }

    fn opacity_leaf() -> Node {
        node(4, opacity_props(), vec![])
    }

    fn node(id: i32, props: Vec<(&'static str, Prop)>, children: Vec<Node>) -> Node {
        Node {
            id,
            props,
            children,
        }
    }

    fn toggle_item(id: i32, label: &'static str, on: bool) -> Node {
        node(
            id,
            vec![
                ("label", Prop::Str(label)),
                ("toggle-type", Prop::Str("checkmark")),
                ("toggle-state", Prop::Int(i32::from(on))),
            ],
            vec![],
        )
    }

    fn quit_item() -> Node {
        node(3, vec![("label", Prop::Str("Quit"))], vec![])
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn layout_cases() {
        let root_props = || vec![("children-display", Prop::Str("submenu"))];
        let cases = vec![
            (
                "full tree",
                menu(true, false, true),
                0,
                -1,
                names(&[]),
                Ok(node(
                    0,
                    root_props(),
                    vec![
                        toggle_item(1, "Lock thumbnails", true),
                        toggle_item(2, "Hide thumbnails", false),
                        toggle_item(5, "Snap thumbnails", true),
                        opacity_item(Some(20)),
                        quit_item(),
                    ],
                )),
            ),
            (
                "depth 1 has the children",
                menu(false, true, false),
                0,
                1,
                names(&[]),
                Ok(node(
                    0,
                    root_props(),
                    vec![
                        toggle_item(1, "Lock thumbnails", false),
                        toggle_item(2, "Hide thumbnails", true),
                        toggle_item(5, "Snap thumbnails", false),
                        opacity_leaf(),
                        quit_item(),
                    ],
                )),
            ),
            (
                "depth 2 reaches the steps",
                menu_at(false, false, true, 30),
                0,
                2,
                names(&["label"]),
                Ok(node(
                    0,
                    vec![],
                    vec![
                        node(1, vec![("label", Prop::Str("Lock thumbnails"))], vec![]),
                        node(2, vec![("label", Prop::Str("Hide thumbnails"))], vec![]),
                        node(5, vec![("label", Prop::Str("Snap thumbnails"))], vec![]),
                        node(
                            4,
                            vec![("label", Prop::Str("Opacity"))],
                            step_nodes(None, true),
                        ),
                        node(3, vec![("label", Prop::Str("Quit"))], vec![]),
                    ],
                )),
            ),
            (
                "depth 0 has no children",
                menu(true, false, true),
                0,
                0,
                names(&[]),
                Ok(node(0, root_props(), vec![])),
            ),
            (
                "label filter",
                menu(true, false, true),
                0,
                -1,
                names(&["label"]),
                Ok(node(
                    0,
                    vec![],
                    vec![
                        node(1, vec![("label", Prop::Str("Lock thumbnails"))], vec![]),
                        node(2, vec![("label", Prop::Str("Hide thumbnails"))], vec![]),
                        node(5, vec![("label", Prop::Str("Snap thumbnails"))], vec![]),
                        node(
                            4,
                            vec![("label", Prop::Str("Opacity"))],
                            step_nodes(None, true),
                        ),
                        node(3, vec![("label", Prop::Str("Quit"))], vec![]),
                    ],
                )),
            ),
            (
                "an item as the parent",
                menu(false, true, true),
                2,
                -1,
                names(&[]),
                Ok(toggle_item(2, "Hide thumbnails", true)),
            ),
            (
                "snap item as parent",
                menu(false, false, true),
                5,
                -1,
                names(&[]),
                Ok(toggle_item(5, "Snap thumbnails", true)),
            ),
            (
                "snap item at depth 0",
                menu(false, false, false),
                5,
                0,
                names(&[]),
                Ok(toggle_item(5, "Snap thumbnails", false)),
            ),
            (
                "the opacity item as the parent",
                menu_at(false, false, true, 50),
                4,
                -1,
                names(&[]),
                Ok(opacity_item(Some(15))),
            ),
            (
                "the opacity item at depth 0",
                menu(false, false, true),
                4,
                0,
                names(&[]),
                Ok(opacity_leaf()),
            ),
            (
                "a value that is not a step checks nothing",
                menu_at(false, false, true, 55),
                4,
                -1,
                names(&[]),
                Ok(opacity_item(None)),
            ),
            (
                "a step as the parent",
                menu_at(false, false, true, 30),
                13,
                -1,
                names(&[]),
                Ok(step_item(13, true)),
            ),
            (
                "unknown parent",
                menu(true, false, true),
                7,
                -1,
                names(&[]),
                Err(MenuError::UnknownId(7)),
            ),
        ];
        for (name, state, parent, depth, filter, want) in cases {
            assert_eq!(layout(&state, parent, depth, &filter), want, "{name}");
        }
    }

    #[test]
    fn event_cases() {
        let cases = vec![
            (
                "lock clicked",
                1,
                "clicked",
                Ok(()),
                vec![MenuCommand::Toggle(Command::ToggleLock)],
            ),
            (
                "hide clicked",
                2,
                "clicked",
                Ok(()),
                vec![MenuCommand::Toggle(Command::ToggleHide)],
            ),
            (
                "quit clicked",
                3,
                "clicked",
                Ok(()),
                vec![MenuCommand::Quit],
            ),
            (
                "step clicked",
                13,
                "clicked",
                Ok(()),
                vec![MenuCommand::Toggle(Command::Opacity(30))],
            ),
            (
                "last step clicked",
                20,
                "clicked",
                Ok(()),
                vec![MenuCommand::Toggle(Command::Opacity(100))],
            ),
            (
                "snap clicked",
                5,
                "clicked",
                Ok(()),
                vec![MenuCommand::Toggle(Command::ToggleSnap)],
            ),
            ("snap hovered", 5, "hovered", Ok(()), vec![]),
            ("step hovered", 13, "hovered", Ok(()), vec![]),
            ("opacity clicked", 4, "clicked", Ok(()), vec![]),
            ("opacity opened", 4, "opened", Ok(()), vec![]),
            ("opacity hovered", 4, "hovered", Ok(()), vec![]),
            ("opacity closed", 4, "closed", Ok(()), vec![]),
            ("lock hovered", 1, "hovered", Ok(()), vec![]),
            ("quit opened", 3, "opened", Ok(()), vec![]),
            (
                "root clicked",
                0,
                "clicked",
                Err(MenuError::UnknownId(0)),
                vec![],
            ),
            (
                "unknown id clicked",
                9,
                "clicked",
                Err(MenuError::UnknownId(9)),
                vec![],
            ),
            (
                "unknown id 7 clicked",
                7,
                "clicked",
                Err(MenuError::UnknownId(7)),
                vec![],
            ),
        ];
        for (name, id, event_id, want, pending) in cases {
            let mut state = menu(true, false, true);
            assert_eq!(event(&mut state, id, event_id), want, "{name}");
            assert_eq!(state.pending, pending, "{name}");
        }
    }

    #[test]
    fn icon_cases() {
        let clear = [0, 0, 0, 0];
        let cases = [
            ("green", GREEN, [0xFF, 0x40, 0xFF, 0x00]),
            ("hidden red", HIDDEN_ICON_COLOR, [0xFF, 0xFF, 0x00, 0x00]),
        ];
        for (name, color, ring) in cases {
            let pixmaps = icon(color);
            let sizes: Vec<(i32, i32, usize)> = pixmaps
                .iter()
                .map(|pixmap| (pixmap.width, pixmap.height, pixmap.argb.len()))
                .collect();
            assert_eq!(sizes, vec![(24, 24, 2_304), (48, 48, 9_216)], "{name}");

            let pixels = [
                ("outer ring row", 12, 1, ring),
                ("inner ring row", 12, 4, ring),
                ("inside the ring", 12, 5, clear),
                ("centre", 12, 12, clear),
                ("above the ring", 12, 0, clear),
                ("corner", 0, 0, clear),
            ];
            for (what, x, y, want) in pixels {
                let at = (y * 24 + x) * 4;
                assert_eq!(pixmaps[0].argb[at..at + 4], want, "{name} {what}");
            }
            let at = (2 * 48 + 24) * 4;
            assert_eq!(pixmaps[1].argb[at..at + 4], ring, "{name} 48 px ring");
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Val {
        Str(String),
        Path(String),
        I32(i32),
        Other(String),
    }

    type Entries = Vec<(String, Val)>;

    #[derive(Debug, PartialEq)]
    enum Decoded {
        Failed(String, String),
        Signature(String),
        Layout {
            revision: u32,
            id: i32,
            props: Entries,
            children: Vec<(i32, Entries, usize)>,
        },
        Groups(Vec<(i32, Entries)>),
        Value(Val),
        Bool(bool),
        Ints(Vec<i32>),
        Pair(Vec<i32>, Vec<i32>),
        Icon(WireIcon),
    }

    #[derive(Clone, Copy)]
    enum Kind {
        Signature,
        Layout,
        Groups,
        Value,
        Bool,
        Ints,
        Pair,
        Icon,
        Text,
    }

    fn val(arg: &(dyn RefArg + 'static)) -> Val {
        let any = arg.as_any();
        if let Some(text) = any.downcast_ref::<String>() {
            Val::Str(text.clone())
        } else if let Some(path) = any.downcast_ref::<Path<'static>>() {
            Val::Path(path.to_string())
        } else if let Some(number) = any.downcast_ref::<i32>() {
            Val::I32(*number)
        } else {
            Val::Other(arg.signature().to_string())
        }
    }

    fn entries(props: Props) -> Entries {
        let mut list: Entries = props
            .into_iter()
            .map(|(key, Variant(value))| (key, val(&*value)))
            .collect();
        list.sort_by(|a, b| a.0.cmp(&b.0));
        list
    }

    fn want_entries(list: &[(&str, Val)]) -> Entries {
        let mut out: Entries = list
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn text(value: &str) -> Val {
        Val::Str(value.to_string())
    }

    fn step_entries(id: i32, on: bool) -> Entries {
        let vals: Vec<(&str, Val)> = step_item(id, on)
            .props
            .into_iter()
            .map(|(key, prop)| {
                let val = match prop {
                    Prop::Str(label) => text(label),
                    Prop::Int(number) => Val::I32(number),
                };
                (key, val)
            })
            .collect();
        want_entries(&vals)
    }

    fn opacity_entries() -> Entries {
        want_entries(&[
            ("label", text("Opacity")),
            ("children-display", text("submenu")),
        ])
    }

    fn signature(reply: &Message) -> String {
        let mut out = String::new();
        let mut iter = reply.iter_init();
        while iter.arg_type() != dbus::arg::ArgType::Invalid {
            out.push_str(&iter.signature());
            if !iter.next() {
                break;
            }
        }
        out
    }

    fn decode(kind: Kind, reply: &mut Message) -> Result<Decoded, String> {
        if reply.msg_type() == MessageType::Error {
            let failure = reply.as_result().err().map(|error| {
                (
                    error.name().unwrap_or_default().to_string(),
                    error.message().unwrap_or_default().to_string(),
                )
            });
            let (name, message) = failure.unwrap_or_default();
            return Ok(Decoded::Failed(name, message));
        }
        let mismatch = |error: dbus::arg::TypeMismatchError| error.to_string();
        Ok(match kind {
            Kind::Signature => Decoded::Signature(signature(reply)),
            Kind::Layout => {
                let (revision, (id, props, children)) = reply
                    .read2::<u32, (i32, Props, Vec<Variant<Wire>>)>()
                    .map_err(mismatch)?;
                Decoded::Layout {
                    revision,
                    id,
                    props: entries(props),
                    children: children
                        .into_iter()
                        .map(|Variant((id, props, below))| (id, entries(props), below.len()))
                        .collect(),
                }
            }
            Kind::Groups => Decoded::Groups(
                reply
                    .read1::<Vec<(i32, Props)>>()
                    .map_err(mismatch)?
                    .into_iter()
                    .map(|(id, props)| (id, entries(props)))
                    .collect(),
            ),
            Kind::Value => {
                let Variant(value) = reply
                    .read1::<Variant<Box<dyn RefArg>>>()
                    .map_err(mismatch)?;
                Decoded::Value(val(&*value))
            }
            Kind::Bool => Decoded::Bool(reply.read1::<bool>().map_err(mismatch)?),
            Kind::Ints => Decoded::Ints(reply.read1::<Vec<i32>>().map_err(mismatch)?),
            Kind::Text => Decoded::Value(Val::Str(reply.read1::<String>().map_err(mismatch)?)),
            Kind::Icon => {
                let Variant(icon) = reply.read1::<Variant<WireIcon>>().map_err(mismatch)?;
                Decoded::Icon(icon)
            }
            Kind::Pair => {
                let (first, second) = reply.read2::<Vec<i32>, Vec<i32>>().map_err(mismatch)?;
                Decoded::Pair(first, second)
            }
        })
    }

    fn call<A: AppendAll>(path: &str, interface: &str, member: &str, args: A) -> Message {
        Message::call_with_args(
            "org.kde.StatusNotifierItem-1-1",
            path,
            interface,
            member,
            args,
        )
    }

    fn invalid_args(reason: &str) -> Decoded {
        Decoded::Failed(
            DBUS_ERROR_INVALID_ARGS.to_string(),
            format!("Invalid argument {reason:?}"),
        )
    }

    fn click(id: i32, event_id: &str) -> (i32, String, Variant<i32>, u32) {
        (id, event_id.to_string(), Variant(0), 0)
    }

    fn menu_call<A: AppendAll>(member: &str, args: A) -> Message {
        call(MENU_PATH, MENU_INTERFACE, member, args)
    }

    #[test]
    fn bus_cases() -> Result<(), String> {
        let none = || names(&[]);
        let label = || names(&["label"]);
        let lock = || vec![MenuCommand::Toggle(Command::ToggleLock)];
        let cases = vec![
            (
                "GetLayout signature",
                menu_call("GetLayout", (0, -1, none())),
                Kind::Signature,
                Decoded::Signature("u(ia{sv}av)".to_string()),
                vec![],
            ),
            (
                "GetLayout tree",
                menu_call("GetLayout", (0, -1, none())),
                Kind::Layout,
                Decoded::Layout {
                    revision: 1,
                    id: 0,
                    props: want_entries(&[("children-display", text("submenu"))]),
                    children: vec![
                        (
                            1,
                            want_entries(&[
                                ("label", text("Lock thumbnails")),
                                ("toggle-type", text("checkmark")),
                                ("toggle-state", Val::I32(1)),
                            ]),
                            0,
                        ),
                        (
                            2,
                            want_entries(&[
                                ("label", text("Hide thumbnails")),
                                ("toggle-type", text("checkmark")),
                                ("toggle-state", Val::I32(0)),
                            ]),
                            0,
                        ),
                        (
                            5,
                            want_entries(&[
                                ("label", text("Snap thumbnails")),
                                ("toggle-type", text("checkmark")),
                                ("toggle-state", Val::I32(1)),
                            ]),
                            0,
                        ),
                        (4, opacity_entries(), 10),
                        (3, want_entries(&[("label", text("Quit"))]), 0),
                    ],
                },
                vec![],
            ),
            (
                "GetLayout of the opacity item",
                menu_call("GetLayout", (4, -1, none())),
                Kind::Layout,
                Decoded::Layout {
                    revision: 1,
                    id: 4,
                    props: opacity_entries(),
                    children: (11..=20)
                        .map(|id| (id, step_entries(id, id == 20), 0))
                        .collect(),
                },
                vec![],
            ),
            (
                "GetLayout unknown parent",
                menu_call("GetLayout", (7, -1, none())),
                Kind::Signature,
                invalid_args("unknown menu item 7"),
                vec![],
            ),
            (
                "Menu property",
                call(
                    SNI_PATH,
                    PROPERTIES_INTERFACE,
                    "Get",
                    (SNI_INTERFACE, "Menu"),
                ),
                Kind::Value,
                Decoded::Value(Val::Path("/MenuBar".to_string())),
                vec![],
            ),
            (
                "GetGroupProperties of everything",
                menu_call("GetGroupProperties", (Vec::<i32>::new(), none())),
                Kind::Groups,
                Decoded::Groups(vec![
                    (0, want_entries(&[("children-display", text("submenu"))])),
                    (
                        1,
                        want_entries(&[
                            ("label", text("Lock thumbnails")),
                            ("toggle-type", text("checkmark")),
                            ("toggle-state", Val::I32(1)),
                        ]),
                    ),
                    (
                        2,
                        want_entries(&[
                            ("label", text("Hide thumbnails")),
                            ("toggle-type", text("checkmark")),
                            ("toggle-state", Val::I32(0)),
                        ]),
                    ),
                    (
                        5,
                        want_entries(&[
                            ("label", text("Snap thumbnails")),
                            ("toggle-type", text("checkmark")),
                            ("toggle-state", Val::I32(1)),
                        ]),
                    ),
                    (4, opacity_entries()),
                    (11, step_entries(11, false)),
                    (12, step_entries(12, false)),
                    (13, step_entries(13, false)),
                    (14, step_entries(14, false)),
                    (15, step_entries(15, false)),
                    (16, step_entries(16, false)),
                    (17, step_entries(17, false)),
                    (18, step_entries(18, false)),
                    (19, step_entries(19, false)),
                    (20, step_entries(20, true)),
                    (3, want_entries(&[("label", text("Quit"))])),
                ]),
                vec![],
            ),
            (
                "GetGroupProperties of steps",
                menu_call("GetGroupProperties", (vec![14, 20], none())),
                Kind::Groups,
                Decoded::Groups(vec![
                    (14, step_entries(14, false)),
                    (20, step_entries(20, true)),
                ]),
                vec![],
            ),
            (
                "GetGroupProperties of listed ids",
                menu_call("GetGroupProperties", (vec![HIDE_ID, 7, LOCK_ID], label())),
                Kind::Groups,
                Decoded::Groups(vec![
                    (2, want_entries(&[("label", text("Hide thumbnails"))])),
                    (1, want_entries(&[("label", text("Lock thumbnails"))])),
                ]),
                vec![],
            ),
            (
                "GetProperty toggle-state",
                menu_call("GetProperty", (LOCK_ID, "toggle-state")),
                Kind::Value,
                Decoded::Value(Val::I32(1)),
                vec![],
            ),
            (
                "GetProperty toggle-state of snap",
                menu_call("GetProperty", (SNAP_ID, "toggle-state")),
                Kind::Value,
                Decoded::Value(Val::I32(1)),
                vec![],
            ),
            (
                "GetProperty toggle-state of the checked step",
                menu_call("GetProperty", (20, "toggle-state")),
                Kind::Value,
                Decoded::Value(Val::I32(1)),
                vec![],
            ),
            (
                "GetProperty toggle-state of an unchecked step",
                menu_call("GetProperty", (14, "toggle-state")),
                Kind::Value,
                Decoded::Value(Val::I32(0)),
                vec![],
            ),
            (
                "GetProperty unknown name",
                menu_call("GetProperty", (LOCK_ID, "x")),
                Kind::Value,
                invalid_args("unknown property x"),
                vec![],
            ),
            (
                "GetProperty unknown id",
                menu_call("GetProperty", (7, "label")),
                Kind::Value,
                invalid_args("unknown menu item 7"),
                vec![],
            ),
            (
                "Event clicked",
                menu_call("Event", (LOCK_ID, "clicked", Variant(0i32), 0u32)),
                Kind::Signature,
                Decoded::Signature(String::new()),
                lock(),
            ),
            (
                "Event clicked on snap",
                menu_call("Event", (SNAP_ID, "clicked", Variant(0i32), 0u32)),
                Kind::Signature,
                Decoded::Signature(String::new()),
                vec![MenuCommand::Toggle(Command::ToggleSnap)],
            ),
            (
                "Event clicked on a step",
                menu_call("Event", (15, "clicked", Variant(0i32), 0u32)),
                Kind::Signature,
                Decoded::Signature(String::new()),
                vec![MenuCommand::Toggle(Command::Opacity(50))],
            ),
            (
                "Event clicked on the opacity item",
                menu_call("Event", (OPACITY_ID, "clicked", Variant(0i32), 0u32)),
                Kind::Signature,
                Decoded::Signature(String::new()),
                vec![],
            ),
            (
                "Event hovered",
                menu_call("Event", (LOCK_ID, "hovered", Variant(0i32), 0u32)),
                Kind::Signature,
                Decoded::Signature(String::new()),
                vec![],
            ),
            (
                "Event unknown id",
                menu_call("Event", (9, "clicked", Variant(0i32), 0u32)),
                Kind::Signature,
                invalid_args("unknown menu item 9"),
                vec![],
            ),
            (
                "EventGroup partly refused",
                menu_call(
                    "EventGroup",
                    (vec![click(LOCK_ID, "clicked"), click(9, "clicked")],),
                ),
                Kind::Ints,
                Decoded::Ints(vec![9]),
                lock(),
            ),
            (
                "EventGroup all refused",
                menu_call("EventGroup", (vec![click(9, "clicked")],)),
                Kind::Ints,
                invalid_args("no event accepted"),
                vec![],
            ),
            (
                "EventGroup empty",
                menu_call(
                    "EventGroup",
                    (Vec::<(i32, String, Variant<i32>, u32)>::new(),),
                ),
                Kind::Ints,
                invalid_args("no event accepted"),
                vec![],
            ),
            (
                "AboutToShow known",
                menu_call("AboutToShow", (LOCK_ID,)),
                Kind::Bool,
                Decoded::Bool(false),
                vec![],
            ),
            (
                "AboutToShow opacity",
                menu_call("AboutToShow", (OPACITY_ID,)),
                Kind::Bool,
                Decoded::Bool(false),
                vec![],
            ),
            (
                "AboutToShow unknown",
                menu_call("AboutToShow", (7,)),
                Kind::Bool,
                invalid_args("unknown menu item 7"),
                vec![],
            ),
            (
                "AboutToShowGroup mixed",
                menu_call("AboutToShowGroup", (vec![LOCK_ID, 7],)),
                Kind::Pair,
                Decoded::Pair(vec![], vec![7]),
                vec![],
            ),
            (
                "AboutToShowGroup unknown only",
                menu_call("AboutToShowGroup", (vec![7],)),
                Kind::Pair,
                invalid_args("no known item"),
                vec![],
            ),
        ];
        for (name, mut message, kind, want, pending) in cases {
            let mut crossroads = objects(menu(true, false, true), icon(GREEN));
            let sink = RefCell::new(Vec::new());
            message.set_serial(57);
            assert_eq!(crossroads.handle_message(message, &sink), Ok(()), "{name}");
            let mut replies = sink.into_inner();
            let got = replies
                .iter_mut()
                .map(|reply| decode(kind, reply))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("{name}: {error}"))?;
            assert_eq!(got, vec![want], "{name}");
            let queued = crossroads
                .data_mut::<MenuState>(&menu_path())
                .map(|state| state.pending.clone());
            assert_eq!(queued, Some(pending), "{name}");
        }
        Ok(())
    }

    #[test]
    fn icon_property_cases() -> Result<(), String> {
        let cases = [
            ("shown", menu_at(false, false, true, 100), GREEN),
            ("hidden", menu_at(false, true, true, 100), HIDDEN_ICON_COLOR),
        ];
        for (name, state, color) in cases {
            let mut crossroads = objects(state, icon(GREEN));
            let sink = RefCell::new(Vec::new());
            let mut message = call(
                SNI_PATH,
                PROPERTIES_INTERFACE,
                "Get",
                (SNI_INTERFACE, "IconPixmap"),
            );
            message.set_serial(57);
            assert_eq!(crossroads.handle_message(message, &sink), Ok(()), "{name}");
            let mut replies = sink.into_inner();
            let got = replies
                .iter_mut()
                .map(|reply| decode(Kind::Icon, reply))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("{name}: {error}"))?;
            assert_eq!(got, vec![Decoded::Icon(wire_icon(icon(color)))], "{name}");
        }
        Ok(())
    }

    #[test]
    fn state_signals_cases() -> Result<(), String> {
        let state_at =
            |locked: bool, hidden: bool, snapping: bool, opacity: u32, revision: u32| MenuState {
                revision,
                ..menu_at(locked, hidden, snapping, opacity)
            };
        let toggles = |locked: bool, hidden: bool, snapping: bool, opacity: u32| Toggles {
            locked,
            hidden,
            snapping,
            opacity,
        };
        let on = |id: i32| (id, vec![("toggle-state".to_string(), 1)]);
        let off = |id: i32| (id, vec![("toggle-state".to_string(), 0)]);
        let cases = vec![
            (
                "hide on",
                state_at(true, false, true, 100, 1),
                toggles(true, true, true, 100),
                2,
                vec![on(2)],
                true,
            ),
            (
                "lock off",
                state_at(true, false, true, 100, 5),
                toggles(false, false, true, 100),
                6,
                vec![off(1)],
                false,
            ),
            (
                "both change",
                state_at(false, true, true, 100, 1),
                toggles(true, false, true, 100),
                2,
                vec![on(1), off(2)],
                true,
            ),
            (
                "opacity 100 to 50",
                state_at(false, false, true, 100, 1),
                toggles(false, false, true, 50),
                2,
                vec![off(20), on(15)],
                false,
            ),
            (
                "opacity 50 to 55",
                state_at(false, false, true, 50, 1),
                toggles(false, false, true, 55),
                2,
                vec![off(15)],
                false,
            ),
            (
                "opacity 55 to 60",
                state_at(false, false, true, 55, 1),
                toggles(false, false, true, 60),
                2,
                vec![on(16)],
                false,
            ),
            (
                "opacity 55 to 57",
                state_at(false, false, true, 55, 1),
                toggles(false, false, true, 57),
                2,
                vec![],
                false,
            ),
            (
                "lock and opacity together",
                state_at(false, false, true, 100, 1),
                toggles(true, false, true, 50),
                2,
                vec![on(1), off(20), on(15)],
                false,
            ),
            (
                "lock, hide and opacity together",
                state_at(false, false, true, 100, 1),
                toggles(true, true, true, 10),
                2,
                vec![on(1), on(2), off(20), on(11)],
                true,
            ),
            (
                "snap off",
                state_at(false, false, true, 100, 1),
                toggles(false, false, false, 100),
                2,
                vec![(5, vec![("toggle-state".to_string(), 0)])],
                false,
            ),
            (
                "snap on",
                state_at(false, false, false, 100, 3),
                toggles(false, false, true, 100),
                4,
                vec![on(5)],
                false,
            ),
            (
                "lock and snap together",
                state_at(false, false, false, 100, 1),
                toggles(true, false, true, 100),
                2,
                vec![on(1), on(5)],
                false,
            ),
            (
                "snap and opacity together",
                state_at(false, false, false, 100, 1),
                toggles(false, false, true, 50),
                2,
                vec![on(5), off(20), on(15)],
                false,
            ),
            (
                "unchanged",
                state_at(true, false, true, 100, 7),
                toggles(true, false, true, 100),
                8,
                vec![],
                false,
            ),
        ];
        for (name, mut state, toggles, revision, updated, new_icon) in cases {
            let messages = state_signals(&mut state, toggles);
            assert_eq!(state.revision, revision, "{name}");
            assert_eq!(state.toggles, toggles, "{name}");
            let heads: Vec<String> = messages
                .iter()
                .map(|message| {
                    format!(
                        "{} {} {}",
                        message.path().as_deref().unwrap_or(""),
                        message.interface().as_deref().unwrap_or(""),
                        message.member().as_deref().unwrap_or("")
                    )
                })
                .collect();
            let mut want_heads = vec![
                "/MenuBar com.canonical.dbusmenu ItemsPropertiesUpdated".to_string(),
                "/MenuBar com.canonical.dbusmenu LayoutUpdated".to_string(),
            ];
            if new_icon {
                want_heads
                    .push("/StatusNotifierItem org.kde.StatusNotifierItem NewIcon".to_string());
            }
            assert_eq!(heads, want_heads, "{name}");
            let (items, removed) = messages[0]
                .read2::<Vec<(i32, HashMap<String, Variant<i32>>)>, Vec<(i32, Vec<String>)>>()
                .map_err(|error| format!("{name}: {error}"))?;
            let items: Vec<(i32, Vec<(String, i32)>)> = items
                .into_iter()
                .map(|(id, props)| {
                    (
                        id,
                        props.into_iter().map(|(k, Variant(v))| (k, v)).collect(),
                    )
                })
                .collect();
            assert_eq!(items, updated, "{name}");
            assert_eq!(removed, Vec::<(i32, Vec<String>)>::new(), "{name}");
            let layout_args = messages[1]
                .read2::<u32, i32>()
                .map_err(|error| format!("{name}: {error}"))?;
            assert_eq!(layout_args, (revision, 0), "{name}");
        }
        Ok(())
    }

    struct PrivateBus {
        child: Child,
        running: bool,
        address: String,
        _dir: TempDir,
    }

    impl PrivateBus {
        fn start(label: &str) -> Result<PrivateBus, String> {
            let dir = TempDir::new(label);
            let address = format!("unix:path={}", dir.path().join("bus").display());
            let child = Process::new("/usr/bin/dbus-daemon")
                .args(["--session", "--nofork", "--nopidfile"])
                .arg(format!("--address={address}"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .map_err(|error| format!("spawn dbus-daemon: {error}"))?;
            Ok(PrivateBus {
                child,
                running: true,
                address,
                _dir: dir,
            })
        }

        fn channel(&mut self) -> Result<Channel, String> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match Channel::open_private(&self.address) {
                    Ok(mut channel) => {
                        channel
                            .register()
                            .map_err(|error| format!("register: {error}"))?;
                        return Ok(channel);
                    }
                    Err(error) if Instant::now() >= deadline => {
                        return Err(format!("connect {}: {error}", self.address));
                    }
                    Err(_) => sleep(Duration::from_millis(10)),
                }
                if let Some(status) = self
                    .child
                    .try_wait()
                    .map_err(|error| format!("wait dbus-daemon: {error}"))?
                {
                    return Err(format!("dbus-daemon exited early: {status}"));
                }
            }
        }

        fn stop(&mut self) -> Result<(), String> {
            self.running = false;
            self.child
                .kill()
                .map_err(|error| format!("kill dbus-daemon: {error}"))?;
            self.child
                .wait()
                .map(|_| ())
                .map_err(|error| format!("wait dbus-daemon: {error}"))
        }
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            if self.running
                && let Err(error) = self.stop()
            {
                eprintln!("{error}");
            }
        }
    }

    fn start_tray(channel: Channel) -> Result<(Tray, Result<(), String>), String> {
        let toggles = Toggles {
            locked: false,
            hidden: false,
            snapping: false,
            opacity: 100,
        };
        Tray::with_channel(channel, std::process::id(), toggles, GREEN)
    }

    fn private_tray(bus: &mut PrivateBus) -> Result<Tray, String> {
        let (tray, registered) = start_tray(bus.channel()?)?;
        assert_eq!(
            tray.name(),
            format!("org.kde.StatusNotifierItem-{}-1", std::process::id())
        );
        assert_eq!(
            registered,
            Err(
                "register: org.freedesktop.DBus.Error.ServiceUnknown: The name \
                org.kde.StatusNotifierWatcher was not provided by any .service files"
                    .to_string()
            ),
            "the private bus has no watcher"
        );
        Ok(tray)
    }

    fn is_socket(fd: OwnedFd) -> Result<bool, String> {
        let kind = std::fs::File::from(fd)
            .metadata()
            .map_err(|error| format!("metadata: {error}"))?
            .file_type();
        Ok(kind.is_socket())
    }

    #[derive(Clone, Copy)]
    enum Stop {
        Never,
        AfterStart,
        BeforeStart,
        GoneNoticed,
    }

    #[test]
    fn bus_error_cases() -> Result<(), String> {
        let cases = vec![
            (
                "daemon alive",
                Stop::Never,
                (
                    Ok(Pass {
                        events: vec![],
                        more: false,
                    }),
                    Some(true),
                ),
            ),
            (
                "daemon killed",
                Stop::AfterStart,
                (Err("session bus: disconnected".to_string()), Some(true)),
            ),
            (
                "bus gone before with_channel, noticed",
                Stop::GoneNoticed,
                (Err("session bus: disconnected".to_string()), None),
            ),
            (
                "daemon gone before with_channel",
                Stop::BeforeStart,
                (
                    Err(format!(
                        "name org.kde.StatusNotifierItem-{}-1: Connection was disconnected before \
                         a reply was received",
                        std::process::id()
                    )),
                    None,
                ),
            ),
        ];
        for (name, stop, want) in cases {
            let mut bus = PrivateBus::start("tray-bus-error")?;
            let got = match stop {
                Stop::BeforeStart => {
                    let channel = bus.channel()?;
                    // The round trip leaves NameAcquired queued in libdbus, so the first blocking
                    // call after the daemon dies reports the disconnect, not a NoReply timeout.
                    sync(&channel)?;
                    bus.stop()?;
                    let started = start_tray(channel).and_then(|_| Err("tray started".to_string()));
                    (started, None)
                }
                Stop::GoneNoticed => {
                    let channel = bus.channel()?;
                    bus.stop()?;
                    assert_eq!(channel.read_write(Some(Duration::ZERO)), Err(()), "{name}");
                    let started = start_tray(channel).and_then(|_| Err("tray started".to_string()));
                    (started, None)
                }
                Stop::Never | Stop::AfterStart => {
                    let mut tray = private_tray(&mut bus)?;
                    if matches!(stop, Stop::AfterStart) {
                        bus.stop()?;
                    }
                    let drained = tray.drain();
                    (drained, Some(is_socket(tray.source()?)?))
                }
            };
            assert_eq!(got, want, "{name}");
        }
        Ok(())
    }

    const WAIT: Duration = Duration::from_secs(5);

    type Passes = Vec<Result<Pass, String>>;
    type Scenario = fn(&mut PrivateBus, &mut Tray, &Channel) -> Result<Passes, String>;
    type Scenario2 = fn(&mut Tray, &Channel) -> Result<Vec<Decoded>, String>;

    fn idle() -> Pass {
        Pass {
            events: vec![],
            more: false,
        }
    }

    fn command_pass(command: MenuCommand) -> Pass {
        Pass {
            events: vec![TrayEvent::Command(command)],
            more: true,
        }
    }

    fn sync(peer: &Channel) -> Result<(), String> {
        let ping = bus_call(BUS_NAME, BUS_PATH, BUS_NAME, "GetId")?;
        peer.send_with_reply_and_block(ping, WAIT)
            .map(|_| ())
            .map_err(|error| format!("sync: {error}"))
    }

    fn tell(peer: &Channel, message: Message) -> Result<u32, String> {
        let serial = peer
            .send(message)
            .map_err(|()| "peer send failed".to_string())?;
        peer.flush();
        sync(peer)?;
        Ok(serial)
    }

    fn next_message<T>(
        peer: &Channel,
        wait: Duration,
        timeout_error: &str,
        mut accept: impl FnMut(Message) -> Option<T>,
    ) -> Result<T, String> {
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            let popped = peer
                .blocking_pop_message(left)
                .map_err(|error| format!("peer pop: {error}"))?;
            if let Some(found) = popped.and_then(&mut accept) {
                return Ok(found);
            }
        }
        Err(timeout_error.to_string())
    }

    fn next_call(peer: &Channel, wait: Duration) -> Result<Message, String> {
        next_message(peer, wait, "no register call reached the peer", |message| {
            (message.msg_type() == MessageType::MethodCall).then_some(message)
        })
    }

    fn drain_until_idle(tray: &mut Tray) -> Passes {
        let mut passes = Vec::new();
        while passes.len() < MAX_DRAIN_PASSES {
            let pass = tray.drain();
            let more = matches!(&pass, Ok(pass) if pass.more);
            passes.push(pass);
            if !more {
                break;
            }
        }
        passes
    }

    fn menu_event(tray: &Tray, member: &str, args: impl AppendAll) -> Message {
        Message::call_with_args(tray.name(), MENU_PATH, MENU_INTERFACE, member, args)
    }

    fn assert_register_call(call: &Message) {
        let text = |value: Option<String>| value.unwrap_or_default();
        let fields = (
            text(call.destination().map(|name| name.to_string())),
            text(call.path().map(|path| path.to_string())),
            text(call.interface().map(|interface| interface.to_string())),
            text(call.member().map(|member| member.to_string())),
            call.read1::<String>(),
        );
        assert_eq!(
            fields,
            (
                "org.kde.StatusNotifierWatcher".to_string(),
                "/StatusNotifierWatcher".to_string(),
                "org.kde.StatusNotifierWatcher".to_string(),
                "RegisterStatusNotifierItem".to_string(),
                Ok(format!(
                    "org.kde.StatusNotifierItem-{}-1",
                    std::process::id()
                )),
            )
        );
    }

    fn acquire_watcher(tray: &mut Tray, peer: &Channel) -> Result<(Pass, Message), String> {
        let request =
            bus_call(BUS_NAME, BUS_PATH, BUS_NAME, "RequestName")?.append2(WATCHER, DO_NOT_QUEUE);
        peer.send_with_reply_and_block(request, WAIT)
            .map_err(|error| format!("watcher name: {error}"))?;
        sync(peer)?;
        let pass = tray.drain()?;
        let call = next_call(peer, WAIT)?;
        assert_register_call(&call);
        Ok((pass, call))
    }

    fn serve_register(
        tray: &mut Tray,
        peer: &Channel,
        answer: fn(&Message) -> Result<Message, String>,
    ) -> Result<Passes, String> {
        let (first, call) = acquire_watcher(tray, peer)?;
        tell(peer, answer(&call)?)?;
        Ok(vec![Ok(first), tray.drain()])
    }

    fn forged_owner_change(tray: &Tray, peer: &Channel) -> Result<Message, String> {
        let owner = peer.unique_name().ok_or("peer has no unique name")?;
        let unique = tray
            .channel
            .unique_name()
            .ok_or("tray has no unique name")?;
        let mut signal = Message::signal(
            &Path::from(BUS_PATH),
            &Interface::from(BUS_NAME),
            &Member::from("NameOwnerChanged"),
        )
        .append3(WATCHER, "", owner);
        signal.set_destination(Some(
            dbus::strings::BusName::new(unique).map_err(|error| format!("destination: {error}"))?,
        ));
        Ok(signal)
    }

    #[test]
    fn pass_cases() -> Result<(), String> {
        let lock = MenuCommand::Toggle(Command::ToggleLock);
        let hide = MenuCommand::Toggle(Command::ToggleHide);
        let step = MenuCommand::Toggle(Command::Opacity(30));
        let snap = MenuCommand::Toggle(Command::ToggleSnap);
        let cases: Vec<(&str, Scenario, Passes)> = vec![
            (
                "a click ends the pass",
                |_, tray, peer| {
                    let click =
                        menu_event(tray, "Event", (LOCK_ID, "clicked", Variant(0i32), 0u32));
                    tell(peer, click)?;
                    Ok(drain_until_idle(tray))
                },
                vec![Ok(command_pass(lock)), Ok(idle())],
            ),
            (
                "snap click ends the pass",
                |_, tray, peer| {
                    let click =
                        menu_event(tray, "Event", (SNAP_ID, "clicked", Variant(0i32), 0u32));
                    tell(peer, click)?;
                    Ok(drain_until_idle(tray))
                },
                vec![Ok(command_pass(snap)), Ok(idle())],
            ),
            (
                "a step click ends the pass",
                |_, tray, peer| {
                    let click = menu_event(tray, "Event", (13, "clicked", Variant(0i32), 0u32));
                    tell(peer, click)?;
                    Ok(drain_until_idle(tray))
                },
                vec![Ok(command_pass(step)), Ok(idle())],
            ),
            (
                "an EventGroup with two clicks gives two passes",
                |_, tray, peer| {
                    let group = menu_event(
                        tray,
                        "EventGroup",
                        (vec![click(LOCK_ID, "clicked"), click(HIDE_ID, "clicked")],),
                    );
                    tell(peer, group)?;
                    Ok(drain_until_idle(tray))
                },
                vec![Ok(command_pass(lock)), Ok(command_pass(hide)), Ok(idle())],
            ),
            (
                "a watcher appearing registers",
                |_, tray, peer| {
                    serve_register(tray, peer, |call| {
                        Message::new_method_return(call).ok_or("no method return".to_string())
                    })
                },
                vec![
                    Ok(Pass {
                        events: vec![],
                        more: true,
                    }),
                    Ok(Pass {
                        events: vec![TrayEvent::Registered],
                        more: false,
                    }),
                ],
            ),
            (
                "a watcher's register error gives Unavailable",
                |_, tray, peer| {
                    serve_register(tray, peer, |call| {
                        let name =
                            dbus::strings::ErrorName::new("org.freedesktop.DBus.Error.Failed")
                                .map_err(|error| format!("error name: {error}"))?;
                        Ok(call.error(&name, c"nope"))
                    })
                },
                vec![
                    Ok(Pass {
                        events: vec![],
                        more: true,
                    }),
                    Ok(Pass {
                        events: vec![TrayEvent::Unavailable(
                            "register: org.freedesktop.DBus.Error.Failed: nope".to_string(),
                        )],
                        more: false,
                    }),
                ],
            ),
            (
                "a forged NameOwnerChanged is ignored",
                |_, tray, peer| {
                    tell(peer, forged_owner_change(tray, peer)?)?;
                    let first = tray.drain();
                    sync(peer)?;
                    Ok(vec![first, tray.drain()])
                },
                vec![Ok(idle()), Ok(idle())],
            ),
            (
                "a forged register reply is ignored",
                |bus, tray, peer| {
                    let (first, call) = acquire_watcher(tray, peer)?;
                    let mut passes = vec![Ok(first)];
                    let third = bus.channel()?;
                    let name = dbus::strings::ErrorName::new("org.freedesktop.DBus.Error.Failed")
                        .map_err(|error| format!("error name: {error}"))?;
                    tell(&third, call.error(&name, c"forged"))?;
                    passes.push(tray.drain());
                    tell(
                        peer,
                        Message::new_method_return(&call).ok_or("no method return".to_string())?,
                    )?;
                    passes.push(tray.drain());
                    Ok(passes)
                },
                vec![
                    Ok(Pass {
                        events: vec![],
                        more: true,
                    }),
                    Ok(idle()),
                    Ok(Pass {
                        events: vec![TrayEvent::Registered],
                        more: false,
                    }),
                ],
            ),
            (
                "set_state after the bus is gone is the bus error",
                |bus, tray, _| {
                    bus.stop()?;
                    Ok(vec![tray.set_state(Toggles {
                        locked: true,
                        hidden: false,
                        snapping: false,
                        opacity: 100,
                    })])
                },
                vec![Err("session bus: disconnected".to_string())],
            ),
            (
                "set_state with an opacity change is idle",
                |_, tray, _| {
                    Ok(vec![tray.set_state(Toggles {
                        locked: false,
                        hidden: false,
                        snapping: false,
                        opacity: 50,
                    })])
                },
                vec![Ok(idle())],
            ),
            (
                "set_state that hides is idle",
                |_, tray, _| {
                    Ok(vec![tray.set_state(Toggles {
                        locked: false,
                        hidden: true,
                        snapping: false,
                        opacity: 100,
                    })])
                },
                vec![Ok(idle())],
            ),
        ];
        for (name, scenario, want) in cases {
            let mut bus = PrivateBus::start("tray-pass")?;
            let mut tray = private_tray(&mut bus)?;
            let peer = bus.channel()?;
            assert_eq!(scenario(&mut bus, &mut tray, &peer)?, want, "{name}");
        }
        Ok(())
    }

    fn ask(
        tray: &mut Tray,
        peer: &Channel,
        kind: Kind,
        message: Message,
    ) -> Result<Decoded, String> {
        let serial = tell(peer, message)?;
        tray.drain()?;
        let mut reply = next_message(peer, WAIT, "no reply reached the peer", |reply| {
            (reply.get_reply_serial() == Some(serial)
                && matches!(
                    reply.msg_type(),
                    MessageType::MethodReturn | MessageType::Error
                ))
            .then_some(reply)
        })?;
        decode(kind, &mut reply)
    }

    fn read_icon(tray: &mut Tray, peer: &Channel) -> Result<Decoded, String> {
        let get = Message::call_with_args(
            tray.name(),
            SNI_PATH,
            PROPERTIES_INTERFACE,
            "Get",
            (SNI_INTERFACE, "IconPixmap"),
        );
        ask(tray, peer, Kind::Icon, get)
    }

    #[test]
    fn sni_cases() -> Result<(), String> {
        let shown = Decoded::Icon(wire_icon(icon(GREEN)));
        let hidden = Decoded::Icon(wire_icon(icon(HIDDEN_ICON_COLOR)));
        let cases: Vec<(&str, Scenario2, Vec<Decoded>)> = vec![
            (
                "set_state that hides changes the served icon",
                |tray, peer| {
                    let before = read_icon(tray, peer)?;
                    tray.set_state(Toggles {
                        locked: false,
                        hidden: true,
                        snapping: false,
                        opacity: 100,
                    })?;
                    Ok(vec![before, read_icon(tray, peer)?])
                },
                vec![shown, hidden],
            ),
            (
                "introspection declares NewIcon",
                |tray, peer| {
                    let introspect = Message::call_with_args(
                        tray.name(),
                        SNI_PATH,
                        "org.freedesktop.DBus.Introspectable",
                        "Introspect",
                        (),
                    );
                    let xml = ask(tray, peer, Kind::Text, introspect)?;
                    let Decoded::Value(Val::Str(xml)) = xml else {
                        return Err(format!("introspect: {xml:?}"));
                    };
                    Ok(vec![Decoded::Bool(
                        xml.contains("<signal name=\"NewIcon\""),
                    )])
                },
                vec![Decoded::Bool(true)],
            ),
        ];
        for (name, scenario, want) in cases {
            let mut bus = PrivateBus::start("tray-sni")?;
            let mut tray = private_tray(&mut bus)?;
            let peer = bus.channel()?;
            assert_eq!(scenario(&mut tray, &peer)?, want, "{name}");
        }
        Ok(())
    }

    #[test]
    fn defer_cases() -> Result<(), String> {
        let hide = MenuCommand::Toggle(Command::ToggleHide);
        let lock = MenuCommand::Toggle(Command::ToggleLock);
        let pass = |command: MenuCommand| Pass {
            events: vec![TrayEvent::Command(command)],
            more: true,
        };
        let idle = || Pass {
            events: vec![],
            more: false,
        };
        let cases = vec![
            (
                "the later deferral is first",
                vec![hide, lock],
                vec![pass(lock), pass(hide), idle()],
            ),
            (
                "quit alone",
                vec![MenuCommand::Quit],
                vec![pass(MenuCommand::Quit), idle()],
            ),
        ];
        for (name, deferred, want) in cases {
            let mut bus = PrivateBus::start("tray-defer")?;
            let mut tray = private_tray(&mut bus)?;
            for command in deferred {
                tray.defer(command);
            }
            let got = want
                .iter()
                .map(|_| tray.drain())
                .collect::<Result<Vec<_>, _>>()?;
            assert_eq!(got, want, "{name}");
        }
        Ok(())
    }
}
