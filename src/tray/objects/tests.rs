use std::cell::RefCell;
use std::collections::HashMap;

use dbus::Message;
use dbus::arg::{AppendAll, Variant};

use super::*;
use crate::control::{Command, Toggles};
use crate::tray::fixtures::{
    Decoded, Entries, GREEN, Kind, PROPERTIES_INTERFACE, Val, click, decode, menu, menu_at, names,
    step_item,
};
use crate::tray::menu::{MenuCommand, OPACITY_ID, Prop};

const DBUS_ERROR_INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";

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
            want_heads.push("/StatusNotifierItem org.kde.StatusNotifierItem NewIcon".to_string());
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
