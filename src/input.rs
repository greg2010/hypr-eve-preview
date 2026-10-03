use std::collections::HashMap;

use smithay_client_toolkit::seat::pointer::BTN_LEFT;

pub const DRAG_THRESHOLD: f64 = 4.0;
pub const GRIP_SIZE: u32 = 16;

const WHEEL_STEP: i32 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    Default,
    Grabbing,
    SeResize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PointerInput {
    Enter {
        address: u64,
    },
    Leave {
        address: u64,
    },
    Press {
        address: u64,
        button: u32,
        in_grip: bool,
    },
    Release {
        button: u32,
    },
    Relative {
        dx: f64,
        dy: f64,
    },
    Motion {
        address: u64,
        in_grip: bool,
    },
    Axis {
        address: u64,
        value120: i32,
        discrete: i32,
    },
    FrameEnd,
    Removed {
        address: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Effect {
    Cursor(Cursor),
    Click { address: u64 },
    Drag { address: u64, offset: (f64, f64) },
    DragEnd { address: u64 },
    Resize { address: u64, steps: i32 },
    ResizeTo { address: u64, dx: f64 },
    ResizeEnd { address: u64 },
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
enum State {
    #[default]
    Idle,
    Pressed {
        address: u64,
        delta: (f64, f64),
        locked: bool,
    },
    Inert {
        address: u64,
    },
    Dragging {
        address: u64,
        delta: (f64, f64),
        sent: (f64, f64),
    },
    Resizing {
        address: u64,
        delta: (f64, f64),
        sent: (f64, f64),
    },
}

/// The gesture machine of the pointer. It holds the active gesture, the wheel remainder per
/// thumbnail and the cursor shape last requested. A locked machine keeps the cursor default,
/// lets an active gesture complete and applies the lock from the next press.
#[derive(Debug, Default)]
pub struct Gestures {
    state: State,
    wheel: HashMap<u64, i32>,
    cursor: Option<Cursor>,
    locked: bool,
}

impl Gestures {
    /// Sets the lock. It emits no effect and leaves the active gesture and the wheel remainders
    /// alone: the lock at the press decides a gesture, and the cursor shape follows the next
    /// pointer event.
    pub fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
    }

    /// Feeds one pointer input and returns the effects it causes, in order. Each effect names a
    /// client the machine has not been told was removed.
    pub fn handle(&mut self, input: PointerInput) -> Vec<Effect> {
        match input {
            PointerInput::Enter { .. } => {
                self.cursor = Some(Cursor::Default);
                vec![Effect::Cursor(Cursor::Default)]
            }
            PointerInput::Leave { address } => self.leave(address),
            PointerInput::Press {
                address,
                button,
                in_grip,
            } => self.press(address, button, in_grip),
            PointerInput::Release { button } => self.release(button),
            PointerInput::Relative { dx, dy } => self.relative(dx, dy),
            PointerInput::Motion { in_grip, .. } => self.motion(in_grip),
            PointerInput::Axis {
                address,
                value120,
                discrete,
            } => self.axis(address, value120, discrete),
            PointerInput::FrameEnd => self.frame_end(),
            PointerInput::Removed { address } => {
                self.wheel.remove(&address);
                if self.active() == Some(address) {
                    self.state = State::Idle;
                }
                vec![]
            }
        }
    }

    /// The address of the client of the current gesture, from the press to the release or
    /// leave. `None` while idle.
    pub fn active(&self) -> Option<u64> {
        match self.state {
            State::Idle => None,
            State::Pressed { address, .. }
            | State::Inert { address }
            | State::Dragging { address, .. }
            | State::Resizing { address, .. } => Some(address),
        }
    }

    fn set_cursor(&mut self, cursor: Cursor) -> Vec<Effect> {
        if self.cursor == Some(cursor) {
            return vec![];
        }
        self.cursor = Some(cursor);
        vec![Effect::Cursor(cursor)]
    }

    fn motion(&mut self, in_grip: bool) -> Vec<Effect> {
        if self.state != State::Idle {
            return vec![];
        }
        self.set_cursor(if in_grip && !self.locked {
            Cursor::SeResize
        } else {
            Cursor::Default
        })
    }

    fn leave(&mut self, address: u64) -> Vec<Effect> {
        if self.active() != Some(address) {
            return vec![];
        }
        let state = std::mem::take(&mut self.state);
        Self::end_gesture(state)
    }

    /// The effects that end a drag or resize: the last move or resize not yet sent, then the
    /// end effect. Empty for any other state.
    fn end_gesture(state: State) -> Vec<Effect> {
        match state {
            State::Dragging {
                address,
                delta,
                sent,
            } => {
                let mut effects = Vec::new();
                if delta != sent {
                    effects.push(Effect::Drag {
                        address,
                        offset: delta,
                    });
                }
                effects.push(Effect::DragEnd { address });
                effects
            }
            State::Resizing {
                address,
                delta,
                sent,
            } => {
                let mut effects = Vec::new();
                if delta.0 != sent.0 {
                    effects.push(Effect::ResizeTo {
                        address,
                        dx: delta.0,
                    });
                }
                effects.push(Effect::ResizeEnd { address });
                effects
            }
            State::Idle | State::Pressed { .. } | State::Inert { .. } => vec![],
        }
    }

    fn press(&mut self, address: u64, button: u32, in_grip: bool) -> Vec<Effect> {
        if button != BTN_LEFT || self.state != State::Idle {
            return vec![];
        }
        let delta = (0.0, 0.0);
        if in_grip && !self.locked {
            self.state = State::Resizing {
                address,
                delta,
                sent: delta,
            };
            self.set_cursor(Cursor::SeResize)
        } else {
            self.state = State::Pressed {
                address,
                delta,
                locked: self.locked,
            };
            vec![]
        }
    }

    fn release(&mut self, button: u32) -> Vec<Effect> {
        if button != BTN_LEFT {
            return vec![];
        }
        let state = std::mem::take(&mut self.state);
        let mut effects = match state {
            State::Idle | State::Inert { .. } => return vec![],
            State::Pressed { address, .. } => return vec![Effect::Click { address }],
            State::Dragging { .. } | State::Resizing { .. } => Self::end_gesture(state),
        };
        effects.extend(self.set_cursor(Cursor::Default));
        effects
    }

    fn relative(&mut self, dx: f64, dy: f64) -> Vec<Effect> {
        match &mut self.state {
            State::Idle | State::Inert { .. } => vec![],
            State::Pressed {
                address,
                delta,
                locked,
            } => {
                let (address, locked) = (*address, *locked);
                let delta = (delta.0 + dx, delta.1 + dy);
                if delta.0.hypot(delta.1) < DRAG_THRESHOLD {
                    self.state = State::Pressed {
                        address,
                        delta,
                        locked,
                    };
                    vec![]
                } else if locked {
                    self.state = State::Inert { address };
                    vec![]
                } else {
                    self.state = State::Dragging {
                        address,
                        delta,
                        sent: (0.0, 0.0),
                    };
                    self.set_cursor(Cursor::Grabbing)
                }
            }
            State::Dragging { delta, .. } | State::Resizing { delta, .. } => {
                delta.0 += dx;
                delta.1 += dy;
                vec![]
            }
        }
    }

    fn axis(&mut self, address: u64, value120: i32, discrete: i32) -> Vec<Effect> {
        if self.locked || self.state != State::Idle {
            return vec![];
        }
        let steps = if value120 != 0 {
            let total = self.wheel.entry(address).or_insert(0);
            *total += value120;
            let whole = *total / WHEEL_STEP;
            *total %= WHEEL_STEP;
            -whole
        } else {
            -discrete
        };
        if steps == 0 {
            vec![]
        } else {
            vec![Effect::Resize { address, steps }]
        }
    }

    fn frame_end(&mut self) -> Vec<Effect> {
        match &mut self.state {
            State::Dragging {
                address,
                delta,
                sent,
            } if delta != sent => {
                *sent = *delta;
                vec![Effect::Drag {
                    address: *address,
                    offset: *delta,
                }]
            }
            State::Resizing {
                address,
                delta,
                sent,
            } if delta.0 != sent.0 => {
                *sent = *delta;
                vec![Effect::ResizeTo {
                    address: *address,
                    dx: delta.0,
                }]
            }
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: u64 = 0x555512345678;
    const B: u64 = 0x55559abcdef0;
    const BTN_RIGHT: u32 = 0x111;

    type Script = Vec<(PointerInput, Vec<Effect>)>;

    fn enter(address: u64) -> PointerInput {
        PointerInput::Enter { address }
    }

    fn leave(address: u64) -> PointerInput {
        PointerInput::Leave { address }
    }

    fn press(address: u64, button: u32, in_grip: bool) -> PointerInput {
        PointerInput::Press {
            address,
            button,
            in_grip,
        }
    }

    fn release() -> PointerInput {
        PointerInput::Release { button: BTN_LEFT }
    }

    fn rel(dx: f64, dy: f64) -> PointerInput {
        PointerInput::Relative { dx, dy }
    }

    fn motion(in_grip: bool) -> PointerInput {
        PointerInput::Motion {
            address: A,
            in_grip,
        }
    }

    fn axis(value120: i32, discrete: i32) -> PointerInput {
        PointerInput::Axis {
            address: A,
            value120,
            discrete,
        }
    }

    fn cursor(c: Cursor) -> Effect {
        Effect::Cursor(c)
    }

    fn run(table: &str, cases: Vec<(&str, Script)>) {
        for (name, script) in cases {
            let mut gestures = Gestures::default();
            for (i, (input, want)) in script.into_iter().enumerate() {
                let got = gestures.handle(input);
                assert_eq!(got, want, "{table}/{name} step {i}: {input:?}");
            }
        }
    }

    #[test]
    fn gesture_cases() {
        let drag = || -> Script {
            vec![
                (press(A, BTN_LEFT, false), vec![]),
                (rel(3.0, 3.0), vec![cursor(Cursor::Grabbing)]),
                (
                    PointerInput::FrameEnd,
                    vec![Effect::Drag {
                        address: A,
                        offset: (3.0, 3.0),
                    }],
                ),
            ]
        };
        let cases: Vec<(&str, Script)> = vec![
            (
                "enter_sets_default",
                vec![(enter(A), vec![cursor(Cursor::Default)])],
            ),
            (
                "enter_again_sets_default_again",
                vec![
                    (enter(A), vec![cursor(Cursor::Default)]),
                    (enter(B), vec![cursor(Cursor::Default)]),
                ],
            ),
            (
                "click",
                vec![
                    (press(A, BTN_LEFT, false), vec![]),
                    (rel(2.0, 2.0), vec![]),
                    (release(), vec![Effect::Click { address: A }]),
                ],
            ),
            (
                "below_threshold_is_click",
                vec![
                    (press(A, BTN_LEFT, false), vec![]),
                    (rel(3.99, 0.0), vec![]),
                    (release(), vec![Effect::Click { address: A }]),
                ],
            ),
            (
                "threshold_exactly_starts_drag",
                vec![
                    (press(A, BTN_LEFT, false), vec![]),
                    (rel(4.0, 0.0), vec![cursor(Cursor::Grabbing)]),
                ],
            ),
            (
                "threshold_accumulates",
                vec![
                    (press(A, BTN_LEFT, false), vec![]),
                    (rel(2.0, 0.0), vec![]),
                    (rel(0.0, 2.0), vec![]),
                    (rel(1.5, 1.5), vec![cursor(Cursor::Grabbing)]),
                ],
            ),
            (
                "drag_moves_and_ends_on_release",
                [
                    drag(),
                    vec![
                        (rel(1.0, 0.0), vec![]),
                        (
                            PointerInput::FrameEnd,
                            vec![Effect::Drag {
                                address: A,
                                offset: (4.0, 3.0),
                            }],
                        ),
                        (
                            release(),
                            vec![Effect::DragEnd { address: A }, cursor(Cursor::Default)],
                        ),
                    ],
                ]
                .concat(),
            ),
            (
                "enter_without_leave_after_a_release_is_idle",
                [
                    drag(),
                    vec![
                        (
                            release(),
                            vec![Effect::DragEnd { address: A }, cursor(Cursor::Default)],
                        ),
                        (enter(A), vec![cursor(Cursor::Default)]),
                        (press(A, BTN_LEFT, false), vec![]),
                        (release(), vec![Effect::Click { address: A }]),
                    ],
                ]
                .concat(),
            ),
            (
                "release_sends_last_move",
                [
                    drag(),
                    vec![
                        (rel(1.0, 0.0), vec![]),
                        (
                            release(),
                            vec![
                                Effect::Drag {
                                    address: A,
                                    offset: (4.0, 3.0),
                                },
                                Effect::DragEnd { address: A },
                                cursor(Cursor::Default),
                            ],
                        ),
                    ],
                ]
                .concat(),
            ),
            (
                "release_sends_last_move_without_a_frame",
                vec![
                    (press(A, BTN_LEFT, false), vec![]),
                    (rel(5.0, 0.0), vec![cursor(Cursor::Grabbing)]),
                    (
                        release(),
                        vec![
                            Effect::Drag {
                                address: A,
                                offset: (5.0, 0.0),
                            },
                            Effect::DragEnd { address: A },
                            cursor(Cursor::Default),
                        ],
                    ),
                    (PointerInput::FrameEnd, vec![]),
                ],
            ),
            (
                "drag_frame_without_motion_is_silent",
                [
                    drag(),
                    vec![
                        (PointerInput::FrameEnd, vec![]),
                        (rel(0.0, 0.0), vec![]),
                        (PointerInput::FrameEnd, vec![]),
                    ],
                ]
                .concat(),
            ),
            (
                "drag_ends_on_leave_without_cursor",
                [
                    drag(),
                    vec![(leave(A), vec![Effect::DragEnd { address: A }])],
                ]
                .concat(),
            ),
            (
                "leave_sends_last_move",
                [
                    drag(),
                    vec![
                        (rel(1.0, 0.0), vec![]),
                        (
                            leave(A),
                            vec![
                                Effect::Drag {
                                    address: A,
                                    offset: (4.0, 3.0),
                                },
                                Effect::DragEnd { address: A },
                            ],
                        ),
                    ],
                ]
                .concat(),
            ),
            (
                "release_after_drag_leave_is_silent",
                [
                    drag(),
                    vec![
                        (leave(A), vec![Effect::DragEnd { address: A }]),
                        (release(), vec![]),
                    ],
                ]
                .concat(),
            ),
            (
                "press_then_leave_cancels",
                vec![
                    (press(A, BTN_LEFT, false), vec![]),
                    (leave(A), vec![]),
                    (release(), vec![]),
                ],
            ),
            (
                "right_button_press_ignored",
                vec![(press(A, BTN_RIGHT, false), vec![]), (release(), vec![])],
            ),
            (
                "right_button_release_keeps_gesture",
                vec![
                    (press(A, BTN_LEFT, false), vec![]),
                    (PointerInput::Release { button: BTN_RIGHT }, vec![]),
                    (release(), vec![Effect::Click { address: A }]),
                ],
            ),
            (
                "second_press_during_drag_ignored",
                [drag(), vec![(press(B, BTN_LEFT, false), vec![])]].concat(),
            ),
            (
                "second_press_during_press_ignored",
                vec![
                    (press(A, BTN_LEFT, false), vec![]),
                    (press(B, BTN_LEFT, false), vec![]),
                    (release(), vec![Effect::Click { address: A }]),
                ],
            ),
            (
                "removed_during_drag_ends_silently",
                [
                    drag(),
                    vec![
                        (PointerInput::Removed { address: A }, vec![]),
                        (release(), vec![]),
                    ],
                ]
                .concat(),
            ),
            (
                "removed_during_press_ends_silently",
                vec![
                    (press(A, BTN_LEFT, false), vec![]),
                    (PointerInput::Removed { address: A }, vec![]),
                    (release(), vec![]),
                ],
            ),
            (
                "removed_of_other_client_keeps_drag",
                [
                    drag(),
                    vec![
                        (PointerInput::Removed { address: B }, vec![]),
                        (
                            release(),
                            vec![Effect::DragEnd { address: A }, cursor(Cursor::Default)],
                        ),
                    ],
                ]
                .concat(),
            ),
            (
                "leave_of_other_client_keeps_drag",
                [
                    drag(),
                    vec![
                        (leave(B), vec![]),
                        (
                            release(),
                            vec![Effect::DragEnd { address: A }, cursor(Cursor::Default)],
                        ),
                    ],
                ]
                .concat(),
            ),
            (
                "relative_while_idle_ignored",
                vec![(rel(10.0, 10.0), vec![]), (PointerInput::FrameEnd, vec![])],
            ),
            ("release_while_idle_ignored", vec![(release(), vec![])]),
        ];
        run("gesture_cases", cases);
    }

    enum Lock {
        Set(bool),
        Input(PointerInput, Vec<Effect>),
    }

    #[test]
    fn lock_cases() {
        use Lock::{Input, Set};
        let grip_press = || press(A, BTN_LEFT, true);
        let body_press = || press(A, BTN_LEFT, false);
        let cases: Vec<(&str, Vec<Lock>)> = vec![
            (
                "locked enter",
                vec![Set(true), Input(enter(A), vec![cursor(Cursor::Default)])],
            ),
            (
                "locked grip motion",
                vec![
                    Set(true),
                    Input(motion(true), vec![cursor(Cursor::Default)]),
                ],
            ),
            (
                "locked grip press is a click",
                vec![
                    Set(true),
                    Input(grip_press(), vec![]),
                    Input(release(), vec![Effect::Click { address: A }]),
                ],
            ),
            (
                "locked press moved 6 px goes inert",
                vec![
                    Set(true),
                    Input(body_press(), vec![]),
                    Input(rel(6.0, 0.0), vec![]),
                    Input(PointerInput::FrameEnd, vec![]),
                    Input(release(), vec![]),
                ],
            ),
            (
                "locked grip press moved 6 px goes inert",
                vec![
                    Set(true),
                    Input(grip_press(), vec![]),
                    Input(rel(6.0, 0.0), vec![]),
                    Input(PointerInput::FrameEnd, vec![]),
                    Input(release(), vec![]),
                ],
            ),
            (
                "locked wheel keeps the remainder",
                vec![
                    Input(axis(60, 0), vec![]),
                    Set(true),
                    Input(axis(60, 0), vec![]),
                    Set(false),
                    Input(
                        axis(60, 0),
                        vec![Effect::Resize {
                            address: A,
                            steps: -1,
                        }],
                    ),
                ],
            ),
            (
                "drag started unlocked completes after lock",
                vec![
                    Input(body_press(), vec![]),
                    Input(rel(6.0, 0.0), vec![cursor(Cursor::Grabbing)]),
                    Set(true),
                    Input(rel(2.0, 0.0), vec![]),
                    Input(
                        PointerInput::FrameEnd,
                        vec![Effect::Drag {
                            address: A,
                            offset: (8.0, 0.0),
                        }],
                    ),
                    Input(
                        release(),
                        vec![Effect::DragEnd { address: A }, cursor(Cursor::Default)],
                    ),
                ],
            ),
            (
                "resize started unlocked completes after lock",
                vec![
                    Input(grip_press(), vec![cursor(Cursor::SeResize)]),
                    Set(true),
                    Input(rel(10.0, 0.0), vec![]),
                    Input(
                        PointerInput::FrameEnd,
                        vec![Effect::ResizeTo {
                            address: A,
                            dx: 10.0,
                        }],
                    ),
                    Input(
                        release(),
                        vec![Effect::ResizeEnd { address: A }, cursor(Cursor::Default)],
                    ),
                ],
            ),
            (
                "press taken locked stays inert after unlock",
                vec![
                    Set(true),
                    Input(body_press(), vec![]),
                    Set(false),
                    Input(rel(6.0, 0.0), vec![]),
                    Input(release(), vec![]),
                ],
            ),
            (
                "grip cursor turns default at the next motion",
                vec![
                    Input(motion(true), vec![cursor(Cursor::SeResize)]),
                    Set(true),
                    Input(motion(true), vec![cursor(Cursor::Default)]),
                ],
            ),
        ];
        for (name, script) in cases {
            let mut gestures = Gestures::default();
            for (i, step) in script.into_iter().enumerate() {
                match step {
                    Set(locked) => gestures.set_locked(locked),
                    Input(input, want) => {
                        let got = gestures.handle(input);
                        assert_eq!(got, want, "lock_cases/{name} step {i}: {input:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn active_cases() {
        let cases: Vec<(&str, bool, Vec<PointerInput>, Option<u64>)> = vec![
            ("idle", false, vec![], None),
            ("entered", false, vec![enter(A)], None),
            (
                "body_press_below_threshold",
                false,
                vec![press(A, BTN_LEFT, false), rel(1.0, 0.0)],
                Some(A),
            ),
            (
                "drag",
                false,
                vec![press(A, BTN_LEFT, false), rel(5.0, 0.0)],
                Some(A),
            ),
            (
                "resize_press",
                false,
                vec![press(B, BTN_LEFT, true)],
                Some(B),
            ),
            (
                "after_release",
                false,
                vec![press(A, BTN_LEFT, false), release()],
                None,
            ),
            (
                "after_leave",
                false,
                vec![press(A, BTN_LEFT, false), rel(5.0, 0.0), leave(A)],
                None,
            ),
            (
                "after_leave_from_press",
                false,
                vec![press(A, BTN_LEFT, false), leave(A)],
                None,
            ),
            (
                "after_leave_from_resize",
                false,
                vec![press(A, BTN_LEFT, true), rel(5.0, 0.0), leave(A)],
                None,
            ),
            (
                "locked press moved 6 px is active",
                true,
                vec![press(A, BTN_LEFT, false), rel(6.0, 0.0)],
                Some(A),
            ),
            (
                "inert gesture removed",
                true,
                vec![
                    press(A, BTN_LEFT, false),
                    rel(6.0, 0.0),
                    PointerInput::Removed { address: A },
                ],
                None,
            ),
        ];
        for (name, locked, inputs, want) in cases {
            let mut gestures = Gestures::default();
            gestures.set_locked(locked);
            for input in inputs {
                gestures.handle(input);
            }
            assert_eq!(gestures.active(), want, "active_cases/{name}");
        }
    }

    #[test]
    fn wheel_cases() {
        let resize = |steps| vec![Effect::Resize { address: A, steps }];
        let cases: Vec<(&str, Script)> = vec![
            ("one_step_up", vec![(axis(-120, 0), resize(1))]),
            (
                "half_steps_accumulate",
                vec![(axis(-60, 0), vec![]), (axis(-60, 0), resize(1))],
            ),
            ("two_steps_down", vec![(axis(240, 0), resize(-2))]),
            (
                "remainder_carries",
                vec![
                    (axis(-180, 0), resize(1)),
                    (axis(-60, 0), resize(1)),
                    (axis(-60, 0), vec![]),
                ],
            ),
            (
                "opposite_halves_cancel",
                vec![
                    (axis(-60, 0), vec![]),
                    (axis(60, 0), vec![]),
                    (axis(-60, 0), vec![]),
                ],
            ),
            ("discrete_fallback", vec![(axis(0, -1), resize(1))]),
            ("discrete_fallback_down", vec![(axis(0, 2), resize(-2))]),
            (
                "value120_wins_over_discrete",
                vec![(axis(-120, 5), resize(1))],
            ),
            ("no_value_no_step", vec![(axis(0, 0), vec![])]),
            (
                "accumulates_per_thumbnail",
                vec![
                    (axis(-60, 0), vec![]),
                    (
                        PointerInput::Axis {
                            address: B,
                            value120: -60,
                            discrete: 0,
                        },
                        vec![],
                    ),
                    (axis(-60, 0), resize(1)),
                    (
                        PointerInput::Axis {
                            address: B,
                            value120: -60,
                            discrete: 0,
                        },
                        vec![Effect::Resize {
                            address: B,
                            steps: 1,
                        }],
                    ),
                ],
            ),
            (
                "removed_clears_remainder",
                vec![
                    (axis(-60, 0), vec![]),
                    (PointerInput::Removed { address: A }, vec![]),
                    (axis(-60, 0), vec![]),
                ],
            ),
            (
                "ignored_while_pressed",
                vec![(press(A, BTN_LEFT, false), vec![]), (axis(-120, 0), vec![])],
            ),
            (
                "ignored_while_dragging",
                vec![
                    (press(A, BTN_LEFT, false), vec![]),
                    (rel(5.0, 0.0), vec![cursor(Cursor::Grabbing)]),
                    (axis(-120, 0), vec![]),
                ],
            ),
            (
                "ignored_while_resizing",
                vec![
                    (press(A, BTN_LEFT, true), vec![cursor(Cursor::SeResize)]),
                    (axis(-120, 0), vec![]),
                ],
            ),
        ];
        run("wheel_cases", cases);
    }

    #[test]
    fn grip_cases() {
        let resize_to = |dx| vec![Effect::ResizeTo { address: A, dx }];
        let cases: Vec<(&str, Script)> = vec![
            (
                "motion_into_and_out_of_grip",
                vec![
                    (enter(A), vec![cursor(Cursor::Default)]),
                    (motion(true), vec![cursor(Cursor::SeResize)]),
                    (motion(true), vec![]),
                    (motion(false), vec![cursor(Cursor::Default)]),
                    (motion(false), vec![]),
                ],
            ),
            (
                "motion_outside_grip_after_enter_is_silent",
                vec![
                    (enter(A), vec![cursor(Cursor::Default)]),
                    (motion(false), vec![]),
                ],
            ),
            (
                "grip_press_after_motion_resizes_without_cursor",
                vec![
                    (enter(A), vec![cursor(Cursor::Default)]),
                    (motion(true), vec![cursor(Cursor::SeResize)]),
                    (press(A, BTN_LEFT, true), vec![]),
                    (rel(10.0, 3.0), vec![]),
                    (PointerInput::FrameEnd, resize_to(10.0)),
                    (
                        release(),
                        vec![Effect::ResizeEnd { address: A }, cursor(Cursor::Default)],
                    ),
                ],
            ),
            (
                "release_sends_last_resize",
                vec![
                    (press(A, BTN_LEFT, true), vec![cursor(Cursor::SeResize)]),
                    (rel(6.0, 1.0), vec![]),
                    (PointerInput::FrameEnd, resize_to(6.0)),
                    (rel(2.0, 0.0), vec![]),
                    (
                        release(),
                        vec![
                            Effect::ResizeTo {
                                address: A,
                                dx: 8.0,
                            },
                            Effect::ResizeEnd { address: A },
                            cursor(Cursor::Default),
                        ],
                    ),
                ],
            ),
            (
                "release_sends_last_resize_without_a_frame",
                vec![
                    (press(A, BTN_LEFT, true), vec![cursor(Cursor::SeResize)]),
                    (rel(7.0, 0.0), vec![]),
                    (
                        release(),
                        vec![
                            Effect::ResizeTo {
                                address: A,
                                dx: 7.0,
                            },
                            Effect::ResizeEnd { address: A },
                            cursor(Cursor::Default),
                        ],
                    ),
                ],
            ),
            (
                "release_after_vertical_only_resize_sends_end_only",
                vec![
                    (press(A, BTN_LEFT, true), vec![cursor(Cursor::SeResize)]),
                    (rel(0.0, 5.0), vec![]),
                    (
                        release(),
                        vec![Effect::ResizeEnd { address: A }, cursor(Cursor::Default)],
                    ),
                ],
            ),
            (
                "grip_press_after_enter_sets_cursor_and_is_never_a_click",
                vec![
                    (enter(A), vec![cursor(Cursor::Default)]),
                    (press(A, BTN_LEFT, true), vec![cursor(Cursor::SeResize)]),
                    (
                        release(),
                        vec![Effect::ResizeEnd { address: A }, cursor(Cursor::Default)],
                    ),
                ],
            ),
            (
                "grip_press_then_leave_ends_without_cursor",
                vec![
                    (enter(A), vec![cursor(Cursor::Default)]),
                    (press(A, BTN_LEFT, true), vec![cursor(Cursor::SeResize)]),
                    (leave(A), vec![Effect::ResizeEnd { address: A }]),
                ],
            ),
            (
                "leave_sends_last_resize",
                vec![
                    (press(A, BTN_LEFT, true), vec![cursor(Cursor::SeResize)]),
                    (rel(6.0, 1.0), vec![]),
                    (PointerInput::FrameEnd, resize_to(6.0)),
                    (rel(2.0, 0.0), vec![]),
                    (
                        leave(A),
                        vec![
                            Effect::ResizeTo {
                                address: A,
                                dx: 8.0,
                            },
                            Effect::ResizeEnd { address: A },
                        ],
                    ),
                ],
            ),
            (
                "resize_to_accumulates_per_frame",
                vec![
                    (press(A, BTN_LEFT, true), vec![cursor(Cursor::SeResize)]),
                    (rel(6.0, 1.0), vec![]),
                    (rel(-2.0, 1.0), vec![]),
                    (PointerInput::FrameEnd, resize_to(4.0)),
                    (PointerInput::FrameEnd, vec![]),
                    (rel(0.0, 5.0), vec![]),
                    (PointerInput::FrameEnd, vec![]),
                    (rel(1.0, 0.0), vec![]),
                    (PointerInput::FrameEnd, resize_to(5.0)),
                ],
            ),
            (
                "motion_during_resize_ignored",
                vec![
                    (press(A, BTN_LEFT, true), vec![cursor(Cursor::SeResize)]),
                    (motion(false), vec![]),
                ],
            ),
            (
                "motion_in_grip_during_drag_ignored",
                vec![
                    (press(A, BTN_LEFT, false), vec![]),
                    (rel(5.0, 0.0), vec![cursor(Cursor::Grabbing)]),
                    (motion(true), vec![]),
                ],
            ),
            (
                "grip_press_with_right_button_ignored",
                vec![(press(A, BTN_RIGHT, true), vec![])],
            ),
            (
                "second_press_during_resize_ignored",
                vec![
                    (press(A, BTN_LEFT, true), vec![cursor(Cursor::SeResize)]),
                    (press(B, BTN_LEFT, false), vec![]),
                    (
                        release(),
                        vec![Effect::ResizeEnd { address: A }, cursor(Cursor::Default)],
                    ),
                ],
            ),
            (
                "removed_during_resize_ends_silently",
                vec![
                    (press(A, BTN_LEFT, true), vec![cursor(Cursor::SeResize)]),
                    (PointerInput::Removed { address: A }, vec![]),
                    (release(), vec![]),
                ],
            ),
        ];
        run("grip_cases", cases);
    }
}
