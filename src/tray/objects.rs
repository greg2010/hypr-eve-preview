use std::collections::HashMap;

use dbus::arg::{RefArg, Variant};
use dbus::strings::{Interface, Member};
use dbus::{Message, MethodErr, Path};
use dbus_crossroads::{Crossroads, IfaceBuilder};

use super::icon::{HIDDEN_ICON_COLOR, Pixmap, icon};
use super::menu::{
    HIDE_ID, LOCK_ID, MenuError, MenuState, Node, Prop, SNAP_ID, event, layout, step_id,
};
use crate::control;

pub(super) const SNI_PATH: &str = "/StatusNotifierItem";
pub(super) const MENU_PATH: &str = "/MenuBar";
pub(super) const MENU_INTERFACE: &str = "com.canonical.dbusmenu";

pub(super) const SNI_INTERFACE: &str = "org.kde.StatusNotifierItem";

pub(super) type Props = HashMap<String, Variant<Box<dyn RefArg>>>;
pub(super) type Wire = (i32, Props, Vec<Variant<Box<dyn RefArg>>>);
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

pub(super) type WireIcon = Vec<(i32, i32, Vec<u8>)>;

pub(super) struct Sni {
    shown: WireIcon,
    hidden_icon: WireIcon,
    pub(super) hidden: bool,
}

pub(super) fn wire_icon(icon: Vec<Pixmap>) -> WireIcon {
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

pub(super) fn menu_path() -> Path<'static> {
    Path::from(MENU_PATH)
}

#[cfg(test)]
mod tests;
