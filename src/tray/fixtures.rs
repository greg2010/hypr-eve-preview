use dbus::arg::{RefArg, Variant};
use dbus::message::MessageType;
use dbus::{Message, Path};

use super::menu::{MenuState, Node, Prop};
use super::objects::{Props, Wire, WireIcon};
use crate::config::Color;
use crate::control::Toggles;

pub(super) const GREEN: Color = Color {
    r: 0x40,
    g: 0xFF,
    b: 0x00,
    a: 0xFF,
};
pub(super) const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
pub(super) fn menu_at(locked: bool, hidden: bool, snapping: bool, opacity: u32) -> MenuState {
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

pub(super) fn menu(locked: bool, hidden: bool, snapping: bool) -> MenuState {
    menu_at(locked, hidden, snapping, 100)
}

pub(super) fn step_nodes(checked: Option<i32>, label_only: bool) -> Vec<Node> {
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

pub(super) fn step_item(id: i32, on: bool) -> Node {
    step_nodes(on.then_some(id), false)
        .into_iter()
        .find(|item| item.id == id)
        .unwrap_or_else(|| panic!("{id} is not a step id"))
}

pub(super) fn node(id: i32, props: Vec<(&'static str, Prop)>, children: Vec<Node>) -> Node {
    Node {
        id,
        props,
        children,
    }
}

pub(super) fn toggle_item(id: i32, label: &'static str, on: bool) -> Node {
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

pub(super) fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_string()).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Val {
    Str(String),
    Path(String),
    I32(i32),
    Other(String),
}

pub(super) type Entries = Vec<(String, Val)>;

#[derive(Debug, PartialEq)]
pub(super) enum Decoded {
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
pub(super) enum Kind {
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

pub(super) fn decode(kind: Kind, reply: &mut Message) -> Result<Decoded, String> {
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

pub(super) fn click(id: i32, event_id: &str) -> (i32, String, Variant<i32>, u32) {
    (id, event_id.to_string(), Variant(0), 0)
}
