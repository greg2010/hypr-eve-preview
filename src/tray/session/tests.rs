use std::os::unix::fs::FileTypeExt;
use std::process::{Child, Command as Process, Stdio};
use std::thread::sleep;
use std::time::Instant;

use dbus::arg::{AppendAll, Variant};
use dbus::message::MessageType;
use dbus::strings::{Interface, Member};
use dbus::{Message, Path};

use super::*;
use crate::control::{Command, Toggles};
use crate::testutil::TempDir;
use crate::tray::fixtures::{Decoded, GREEN, Kind, PROPERTIES_INTERFACE, Val, click, decode};
use crate::tray::icon::HIDDEN_ICON_COLOR;
use crate::tray::menu::{HIDE_ID, LOCK_ID, SNAP_ID};
use crate::tray::objects::{MENU_INTERFACE, MENU_PATH, SNI_INTERFACE, wire_icon};

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
                let click = menu_event(tray, "Event", (LOCK_ID, "clicked", Variant(0i32), 0u32));
                tell(peer, click)?;
                Ok(drain_until_idle(tray))
            },
            vec![Ok(command_pass(lock)), Ok(idle())],
        ),
        (
            "snap click ends the pass",
            |_, tray, peer| {
                let click = menu_event(tray, "Event", (SNAP_ID, "clicked", Variant(0i32), 0u32));
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
                    let name = dbus::strings::ErrorName::new("org.freedesktop.DBus.Error.Failed")
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

fn ask(tray: &mut Tray, peer: &Channel, kind: Kind, message: Message) -> Result<Decoded, String> {
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
