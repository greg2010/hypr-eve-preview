use std::fmt;

use crate::control;

pub(super) const LOCK_ID: i32 = 1;
pub(super) const HIDE_ID: i32 = 2;
const QUIT_ID: i32 = 3;
pub(super) const OPACITY_ID: i32 = 4;
pub(super) const SNAP_ID: i32 = 5;
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

pub(super) fn step_id(percent: u32) -> Option<i32> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::Command;
    use crate::tray::fixtures::{menu, menu_at, names, node, step_item, step_nodes, toggle_item};

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

    fn quit_item() -> Node {
        node(3, vec![("label", Prop::Str("Quit"))], vec![])
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
}
