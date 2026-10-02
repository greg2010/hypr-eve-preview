use std::time::{Duration, Instant};

use crate::geometry::Size;
use crate::report::{DamageMode, FailReason, Line};

/// Keeps the copy rate at 30 per second or less.
pub const MIN_COPY_INTERVAL: Duration = Duration::from_nanos(33_333_334);
pub const RETRY_DELAY: Duration = Duration::from_millis(500);
pub const STALL_TIMEOUT: Duration = Duration::from_millis(1000);
pub const MAX_CONSECUTIVE_FAILURES: u32 = 5;
/// Number of buffers the thumbnail alternates between.
pub const SLOTS: usize = 2;

/// The screencopy frame a client holds. `Answered` has delivered its event, so `teardown`
/// destroys it; `Unanswered` has not, so `teardown` orphans it instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldFrame {
    None,
    Answered,
    Unanswered,
}

/// The capture resources one client holds when it is torn down. `teardown` turns the record
/// into a plan; `buffers[slot]` is true while that slot's buffer exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Held {
    pub timer: bool,
    pub frame: HeldFrame,
    pub overlay: bool,
    pub buffers: [bool; SLOTS],
}

/// One destruction step. The caller executes the steps in the order `teardown` returns them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Teardown {
    CancelTimer,
    DestroyFrame,
    OrphanFrame,
    DestroyOverlay,
    DestroyBuffer { slot: usize },
    ReleaseImports,
}

/// The destruction plan of one client's capture resources, in the order that leaves no server
/// reference to a destroyed buffer: timer, frame, overlay, buffers, imports.
/// `ReleaseImports` is always present.
pub fn teardown(held: Held) -> Vec<Teardown> {
    let mut plan = Vec::new();
    if held.timer {
        plan.push(Teardown::CancelTimer);
    }
    match held.frame {
        HeldFrame::None => {}
        HeldFrame::Answered => plan.push(Teardown::DestroyFrame),
        HeldFrame::Unanswered => plan.push(Teardown::OrphanFrame),
    }
    if held.overlay {
        plan.push(Teardown::DestroyOverlay);
    }
    plan.extend(
        held.buffers
            .iter()
            .enumerate()
            .filter(|(_, held)| **held)
            .map(|(slot, _)| Teardown::DestroyBuffer { slot }),
    );
    plan.push(Teardown::ReleaseImports);
    plan
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmabufInfo {
    pub fourcc: u32,
    pub size: Size,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Start,
    FrameDescribed { dmabuf: Option<DmabufInfo> },
    FrameMissing,
    OverlayConfigured,
    Imported { slot: usize },
    Flags { y_invert: bool },
    Ready { tv_sec: u64, tv_nsec: u32 },
    Failed,
    Released { slot: usize },
    Wake,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    RequestFrame,
    CreateOverlay {
        buffer_size: Size,
    },
    Allocate {
        slot: usize,
        fourcc: u32,
        size: Size,
    },
    Copy {
        slot: usize,
        ignore_damage: bool,
    },
    Recommit {
        slot: usize,
        buffer_size: Size,
    },
    Present {
        slot: usize,
        buffer_size: Size,
        y_invert: bool,
    },
    DestroyFrame,
    WakeAt(Instant),
    Report(Line),
    Remove {
        reason: String,
    },
    Exit {
        code: u8,
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Free,
    Copying,
    OnScreen,
    Displaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BufferRecord {
    size: Size,
    fourcc: u32,
    imported: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Frame {
    Idle,
    Requested,
    Described(DmabufInfo),
    Copying {
        slot: usize,
        at: Instant,
        size: Size,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayState {
    Absent,
    Created {
        configured: bool,
        buffers: [BufferRecord; SLOTS],
    },
}

/// The frame and buffer state machine. It does no I/O: the caller feeds inputs with the current
/// time and executes the returned actions in order.
pub struct Capture {
    mode: DamageMode,
    address: u64,
    handle: u32,
    slots: [SlotState; SLOTS],
    frame: Frame,
    overlay: OverlayState,
    seq: u64,
    failures: u32,
    last_copy: Option<Instant>,
    last_failure: Option<Instant>,
    wait: Option<(usize, Instant)>,
    y_invert: bool,
}

impl Capture {
    /// `handle` only appears in the removal reason of a missing window.
    pub fn new(mode: DamageMode, address: u64, handle: u32) -> Capture {
        Capture {
            mode,
            address,
            handle,
            slots: [SlotState::Free; SLOTS],
            frame: Frame::Idle,
            overlay: OverlayState::Absent,
            seq: 0,
            failures: 0,
            last_copy: None,
            last_failure: None,
            wait: None,
            y_invert: false,
        }
    }

    pub fn handle(&mut self, input: Input, now: Instant) -> Vec<Action> {
        match input {
            Input::Start => self.gate_if_idle(now),
            Input::Wake => match self.frame {
                Frame::Idle => self.gate_if_idle(now),
                Frame::Copying { at, .. } => {
                    let stall_at = at + STALL_TIMEOUT;
                    if now >= stall_at {
                        self.fail(FailReason::Stall, now)
                    } else {
                        vec![Action::WakeAt(stall_at)]
                    }
                }
                _ => vec![],
            },
            Input::FrameDescribed { dmabuf } => {
                if self.frame != Frame::Requested {
                    return vec![];
                }
                let Some(info) = dmabuf else {
                    return vec![Action::Exit {
                        code: 1,
                        reason: "no linux_dmabuf event from hyprland_toplevel_export_frame_v1"
                            .to_string(),
                    }];
                };
                if info.size.width == 0 || info.size.height == 0 {
                    return vec![Action::Remove {
                        reason: format!(
                            "frame size {}x{} has a zero dimension",
                            info.size.width, info.size.height
                        ),
                    }];
                }
                self.frame = Frame::Described(info);
                self.describe(info, now)
            }
            Input::FrameMissing => {
                if self.frame != Frame::Requested {
                    return vec![];
                }
                vec![Action::Remove {
                    reason: format!(
                        "window 0x{:x} not found by hyprland_toplevel_export_manager_v1 (handle {})",
                        self.address, self.handle
                    ),
                }]
            }
            Input::OverlayConfigured => {
                if let OverlayState::Created { configured, .. } = &mut self.overlay {
                    *configured = true;
                }
                self.try_copy(now)
            }
            Input::Imported { slot } => {
                let OverlayState::Created { buffers, .. } = &mut self.overlay else {
                    return vec![];
                };
                let Some(record) = buffers.get_mut(slot) else {
                    return vec![];
                };
                record.imported = true;
                if slot == self.target() {
                    self.try_copy(now)
                } else {
                    vec![]
                }
            }
            Input::Flags { y_invert } => {
                self.y_invert = y_invert;
                vec![]
            }
            Input::Ready { tv_sec, tv_nsec } => self.ready(tv_sec, tv_nsec, now),
            Input::Failed => match self.frame {
                Frame::Requested | Frame::Described(_) | Frame::Copying { .. } => {
                    self.fail(FailReason::Failed, now)
                }
                Frame::Idle => vec![],
            },
            Input::Released { slot } => self.released(slot, now),
        }
    }

    fn on_screen(&self) -> Option<usize> {
        self.slots.iter().position(|s| *s == SlotState::OnScreen)
    }

    fn target(&self) -> usize {
        match self.on_screen() {
            Some(slot) => (slot + 1) % SLOTS,
            None => 0,
        }
    }

    fn earliest(&self) -> Option<Instant> {
        let copy = self.last_copy.map(|t| t + MIN_COPY_INTERVAL);
        let retry = self.last_failure.map(|t| t + RETRY_DELAY);
        copy.max(retry)
    }

    fn gate_if_idle(&mut self, now: Instant) -> Vec<Action> {
        if self.frame == Frame::Idle {
            self.gate(now)
        } else {
            vec![]
        }
    }

    fn gate(&mut self, now: Instant) -> Vec<Action> {
        if self.wait.is_some() {
            return vec![];
        }
        if let Some(earliest) = self.earliest()
            && now < earliest
        {
            return vec![Action::WakeAt(earliest)];
        }
        let target = self.target();
        if self.slots[target] == SlotState::Displaced {
            self.wait = Some((target, now));
            return vec![Action::Report(Line::ReleaseWait {
                address: self.address,
                slot: target,
            })];
        }
        self.seq += 1;
        self.y_invert = false;
        self.frame = Frame::Requested;
        vec![Action::RequestFrame]
    }

    fn describe(&mut self, info: DmabufInfo, now: Instant) -> Vec<Action> {
        let record = BufferRecord {
            size: info.size,
            fourcc: info.fourcc,
            imported: false,
        };
        let allocate = |slot| Action::Allocate {
            slot,
            fourcc: info.fourcc,
            size: info.size,
        };
        let target = self.target();
        let OverlayState::Created { buffers, .. } = &mut self.overlay else {
            self.overlay = OverlayState::Created {
                configured: false,
                buffers: [record; SLOTS],
            };
            let mut actions = vec![Action::CreateOverlay {
                buffer_size: info.size,
            }];
            actions.extend((0..SLOTS).map(allocate));
            return actions;
        };
        let current = buffers[target];
        if current.size != info.size || current.fourcc != info.fourcc {
            buffers[target] = record;
            return vec![allocate(target)];
        }
        self.try_copy(now)
    }

    fn try_copy(&mut self, now: Instant) -> Vec<Action> {
        let Frame::Described(info) = self.frame else {
            return vec![];
        };
        let OverlayState::Created {
            configured: true,
            buffers,
        } = &self.overlay
        else {
            return vec![];
        };
        let target = self.target();
        let b = buffers[target];
        let ready = b.imported && b.size == info.size && b.fourcc == info.fourcc;
        if !ready || self.slots[target] != SlotState::Free {
            return vec![];
        }
        let on_screen = self.on_screen();
        let ignore_damage = self.mode == DamageMode::IgnoreDamage || on_screen.is_none();
        let mut actions = vec![Action::Copy {
            slot: target,
            ignore_damage,
        }];
        if self.mode == DamageMode::Recommit
            && let Some(slot) = on_screen
        {
            actions.push(Action::Recommit {
                slot,
                buffer_size: buffers[slot].size,
            });
        }
        actions.push(Action::WakeAt(now + STALL_TIMEOUT));
        self.last_copy = Some(now);
        self.slots[target] = SlotState::Copying;
        self.frame = Frame::Copying {
            slot: target,
            at: now,
            size: info.size,
        };
        actions
    }

    fn ready(&mut self, tv_sec: u64, tv_nsec: u32, now: Instant) -> Vec<Action> {
        let Frame::Copying { slot, at, size } = self.frame else {
            return vec![];
        };
        if let Some(previous) = self.on_screen() {
            self.slots[previous] = SlotState::Displaced;
        }
        self.slots[slot] = SlotState::OnScreen;
        self.failures = 0;
        self.frame = Frame::Idle;
        let earliest = self.earliest().unwrap_or(now);
        vec![
            Action::Report(Line::Ready {
                address: self.address,
                seq: self.seq,
                slot,
                tv_sec,
                tv_nsec,
                copy_to_ready: now.saturating_duration_since(at),
            }),
            Action::Present {
                slot,
                buffer_size: size,
                y_invert: self.y_invert,
            },
            Action::DestroyFrame,
            Action::WakeAt(earliest),
        ]
    }

    fn fail(&mut self, reason: FailReason, now: Instant) -> Vec<Action> {
        if let Frame::Copying { slot, .. } = self.frame {
            self.slots[slot] = SlotState::Free;
        }
        self.frame = Frame::Idle;
        self.failures += 1;
        self.last_failure = Some(now);
        let mut actions = vec![
            Action::DestroyFrame,
            Action::Report(Line::Failed {
                address: self.address,
                seq: self.seq,
                reason,
                consecutive: self.failures,
            }),
        ];
        if self.failures >= MAX_CONSECUTIVE_FAILURES {
            actions.push(Action::Remove {
                reason: format!("{MAX_CONSECUTIVE_FAILURES} consecutive capture failures"),
            });
        } else {
            actions.push(Action::WakeAt(self.earliest().unwrap_or(now)));
        }
        actions
    }

    fn released(&mut self, slot: usize, now: Instant) -> Vec<Action> {
        let Some(s) = self.slots.get_mut(slot) else {
            return vec![];
        };
        if *s != SlotState::Displaced {
            return vec![];
        }
        *s = SlotState::Free;
        let Some((waited_slot, start)) = self.wait else {
            return vec![];
        };
        if waited_slot != slot {
            return vec![];
        }
        self.wait = None;
        let mut actions = vec![Action::Report(Line::Released {
            address: self.address,
            slot,
            waited: now.saturating_duration_since(start),
        })];
        actions.extend(self.gate(now));
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDRESS: u64 = 0x555512345678;
    const HANDLE: u32 = 305419896;
    const AR24: u32 = 0x34325241;
    const SIZE: Size = Size {
        width: 3840,
        height: 2109,
    };
    const MODES: [DamageMode; 2] = [DamageMode::Recommit, DamageMode::IgnoreDamage];

    struct Step {
        at: Duration,
        input: Input,
        want: Vec<Action>,
    }

    fn step(at: Duration, input: Input, want: Vec<Action>) -> Step {
        Step { at, input, want }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn run(name: &str, mode: DamageMode, t0: Instant, steps: Vec<Step>) -> Capture {
        let mut cap = Capture::new(mode, ADDRESS, HANDLE);
        for (i, s) in steps.into_iter().enumerate() {
            let got = cap.handle(s.input.clone(), t0 + s.at);
            assert_eq!(got, s.want, "{name} ({mode:?}) step {i}: {:?}", s.input);
        }
        cap
    }

    fn slot_states(cap: &Capture) -> [SlotState; SLOTS] {
        cap.slots
    }

    fn describe(size: Size) -> Input {
        Input::FrameDescribed {
            dmabuf: Some(DmabufInfo { fourcc: AR24, size }),
        }
    }

    fn allocate(slot: usize, size: Size) -> Action {
        Action::Allocate {
            slot,
            fourcc: AR24,
            size,
        }
    }

    fn at(t0: Instant, d: Duration) -> Action {
        Action::WakeAt(t0 + d)
    }

    fn ready_line(seq: u64, slot: usize, copy_to_ready: Duration) -> Action {
        Action::Report(Line::Ready {
            address: ADDRESS,
            seq,
            slot,
            tv_sec: 100,
            tv_nsec: 5,
            copy_to_ready,
        })
    }

    fn ready_input() -> Input {
        Input::Ready {
            tv_sec: 100,
            tv_nsec: 5,
        }
    }

    fn present(slot: usize, buffer_size: Size, y_invert: bool) -> Action {
        Action::Present {
            slot,
            buffer_size,
            y_invert,
        }
    }

    /// Start through the first Ready: copy at 4 ms, ready at 14 ms, slot 0 on screen.
    fn first_cycle(t0: Instant) -> Vec<Step> {
        vec![
            step(ms(0), Input::Start, vec![Action::RequestFrame]),
            step(
                ms(1),
                describe(SIZE),
                vec![
                    Action::CreateOverlay { buffer_size: SIZE },
                    allocate(0, SIZE),
                    allocate(1, SIZE),
                ],
            ),
            step(ms(2), Input::OverlayConfigured, vec![]),
            step(ms(3), Input::Imported { slot: 1 }, vec![]),
            step(
                ms(4),
                Input::Imported { slot: 0 },
                vec![
                    Action::Copy {
                        slot: 0,
                        ignore_damage: true,
                    },
                    at(t0, ms(4) + STALL_TIMEOUT),
                ],
            ),
            step(ms(10), Input::Flags { y_invert: false }, vec![]),
            step(
                ms(14),
                ready_input(),
                vec![
                    ready_line(1, 0, ms(10)),
                    present(0, SIZE, false),
                    Action::DestroyFrame,
                    at(t0, ms(4) + MIN_COPY_INTERVAL),
                ],
            ),
        ]
    }

    fn copy_actions(
        mode: DamageMode,
        t0: Instant,
        slot: usize,
        on_screen: usize,
        size: Size,
        copy_at: Duration,
    ) -> Vec<Action> {
        let mut v = match mode {
            DamageMode::Recommit => vec![
                Action::Copy {
                    slot,
                    ignore_damage: false,
                },
                Action::Recommit {
                    slot: on_screen,
                    buffer_size: size,
                },
            ],
            DamageMode::IgnoreDamage => vec![Action::Copy {
                slot,
                ignore_damage: true,
            }],
        };
        v.push(at(t0, copy_at + STALL_TIMEOUT));
        v
    }

    /// Second cycle: wake at the copy interval, describe 1 ms later, ready 10 ms after that.
    fn second_cycle(mode: DamageMode, t0: Instant) -> Vec<Step> {
        let wake = ms(4) + MIN_COPY_INTERVAL;
        let copy = wake + ms(1);
        vec![
            step(wake, Input::Wake, vec![Action::RequestFrame]),
            step(
                copy,
                describe(SIZE),
                copy_actions(mode, t0, 1, 0, SIZE, copy),
            ),
            step(
                copy + ms(10),
                ready_input(),
                vec![
                    ready_line(2, 1, ms(10)),
                    present(1, SIZE, false),
                    Action::DestroyFrame,
                    at(t0, copy + MIN_COPY_INTERVAL),
                ],
            ),
        ]
    }

    fn second_copy_at() -> Duration {
        ms(5) + MIN_COPY_INTERVAL
    }

    /// The Wake that finds slot 0 displaced, and the release 120 ms later.
    fn wait_cycle() -> Vec<Step> {
        let wake = second_copy_at() + MIN_COPY_INTERVAL;
        vec![
            step(
                wake,
                Input::Wake,
                vec![Action::Report(Line::ReleaseWait {
                    address: ADDRESS,
                    slot: 0,
                })],
            ),
            step(
                wake + ms(120),
                Input::Released { slot: 0 },
                vec![
                    Action::Report(Line::Released {
                        address: ADDRESS,
                        slot: 0,
                        waited: ms(120),
                    }),
                    Action::RequestFrame,
                ],
            ),
        ]
    }

    fn join(parts: Vec<Vec<Step>>) -> Vec<Step> {
        parts.into_iter().flatten().collect()
    }

    #[test]
    fn cycle_cases() {
        for mode in MODES {
            let t0 = Instant::now();
            let first_ready_wake = ms(4) + MIN_COPY_INTERVAL;
            let third_copy = second_copy_at() + MIN_COPY_INTERVAL + ms(120) + ms(1);
            let cases: Vec<(&str, Vec<Step>)> = vec![
                (
                    "start_requests_frame",
                    vec![step(ms(0), Input::Start, vec![Action::RequestFrame])],
                ),
                (
                    "first_described_creates_overlay_and_allocates",
                    first_cycle(t0).into_iter().take(2).collect(),
                ),
                (
                    "copy_waits_for_configure_then_import",
                    first_cycle(t0).into_iter().take(5).collect(),
                ),
                (
                    "copy_waits_for_import_then_configure",
                    vec![
                        step(ms(0), Input::Start, vec![Action::RequestFrame]),
                        step(
                            ms(1),
                            describe(SIZE),
                            vec![
                                Action::CreateOverlay { buffer_size: SIZE },
                                allocate(0, SIZE),
                                allocate(1, SIZE),
                            ],
                        ),
                        step(ms(2), Input::Imported { slot: 1 }, vec![]),
                        step(ms(3), Input::Imported { slot: 0 }, vec![]),
                        step(
                            ms(4),
                            Input::OverlayConfigured,
                            vec![
                                Action::Copy {
                                    slot: 0,
                                    ignore_damage: true,
                                },
                                at(t0, ms(4) + STALL_TIMEOUT),
                            ],
                        ),
                    ],
                ),
                ("ready_presents_and_paces", first_cycle(t0)),
                (
                    "wake_before_interval_rearms",
                    join(vec![
                        first_cycle(t0),
                        vec![step(
                            ms(15),
                            Input::Wake,
                            vec![at(t0, first_ready_wake)],
                        )],
                    ]),
                ),
                (
                    "wake_after_interval_requests",
                    join(vec![
                        first_cycle(t0),
                        vec![step(first_ready_wake, Input::Wake, vec![Action::RequestFrame])],
                    ]),
                ),
                (
                    "slots_alternate_0_1_0",
                    join(vec![
                        first_cycle(t0),
                        second_cycle(mode, t0),
                        wait_cycle(),
                        vec![step(
                            third_copy,
                            describe(SIZE),
                            copy_actions(mode, t0, 0, 1, SIZE, third_copy),
                        )],
                    ]),
                ),
                (
                    "frame_missing_removes",
                    vec![
                        step(ms(0), Input::Start, vec![Action::RequestFrame]),
                        step(
                            ms(1),
                            Input::FrameMissing,
                            vec![Action::Remove {
                                reason: "window 0x555512345678 not found by hyprland_toplevel_export_manager_v1 (handle 305419896)".to_string(),
                            }],
                        ),
                    ],
                ),
                (
                    "described_without_dmabuf_exits",
                    vec![
                        step(ms(0), Input::Start, vec![Action::RequestFrame]),
                        step(
                            ms(1),
                            Input::FrameDescribed { dmabuf: None },
                            vec![Action::Exit {
                                code: 1,
                                reason: "no linux_dmabuf event from hyprland_toplevel_export_frame_v1".to_string(),
                            }],
                        ),
                    ],
                ),
                (
                    "zero_width_frame_removes",
                    zero_frame_script(0, 1406),
                ),
                (
                    "zero_height_frame_removes",
                    zero_frame_script(3840, 0),
                ),
                (
                    "retry_before_first_ready_ignores_damage",
                    join(vec![
                        first_cycle(t0).into_iter().take(5).collect(),
                        vec![
                        step(
                            ms(20),
                            Input::Failed,
                            vec![
                                Action::DestroyFrame,
                                Action::Report(Line::Failed {
                                    address: ADDRESS,
                                    seq: 1,
                                    reason: FailReason::Failed,
                                    consecutive: 1,
                                }),
                                at(t0, ms(20) + RETRY_DELAY),
                            ],
                        ),
                        step(ms(20) + RETRY_DELAY, Input::Wake, vec![Action::RequestFrame]),
                        step(
                            ms(521),
                            describe(SIZE),
                            vec![
                                Action::Copy {
                                    slot: 0,
                                    ignore_damage: true,
                                },
                                at(t0, ms(521) + STALL_TIMEOUT),
                            ],
                        ),
                        ],
                    ]),
                ),
                (
                    "y_invert_true_presents_flipped",
                    flag_script(t0, true),
                ),
                (
                    "y_invert_false_presents_normal",
                    flag_script(t0, false),
                ),
            ];
            for (name, steps) in cases {
                run(name, mode, t0, steps);
            }
        }
    }

    fn zero_frame_script(width: u32, height: u32) -> Vec<Step> {
        vec![
            step(ms(0), Input::Start, vec![Action::RequestFrame]),
            step(
                ms(1),
                describe(Size { width, height }),
                vec![Action::Remove {
                    reason: format!("frame size {width}x{height} has a zero dimension"),
                }],
            ),
        ]
    }

    fn flag_script(t0: Instant, y_invert: bool) -> Vec<Step> {
        let mut steps: Vec<Step> = first_cycle(t0).into_iter().take(5).collect();
        steps.push(step(ms(10), Input::Flags { y_invert }, vec![]));
        steps.push(step(
            ms(14),
            ready_input(),
            vec![
                ready_line(1, 0, ms(10)),
                present(0, SIZE, y_invert),
                Action::DestroyFrame,
                at(t0, ms(4) + MIN_COPY_INTERVAL),
            ],
        ));
        steps
    }

    #[test]
    fn teardown_cases() {
        let held = |timer, frame, overlay, buffers| Held {
            timer,
            frame,
            overlay,
            buffers,
        };
        let buffer = |slot| Teardown::DestroyBuffer { slot };
        let cases: Vec<(&str, Held, Vec<Teardown>)> = vec![
            (
                "nothing held",
                held(false, HeldFrame::None, false, [false; SLOTS]),
                vec![Teardown::ReleaseImports],
            ),
            (
                "everything held, answered frame",
                held(true, HeldFrame::Answered, true, [true; SLOTS]),
                vec![
                    Teardown::CancelTimer,
                    Teardown::DestroyFrame,
                    Teardown::DestroyOverlay,
                    buffer(0),
                    buffer(1),
                    Teardown::ReleaseImports,
                ],
            ),
            (
                "everything held, unanswered frame",
                held(true, HeldFrame::Unanswered, true, [true; SLOTS]),
                vec![
                    Teardown::CancelTimer,
                    Teardown::OrphanFrame,
                    Teardown::DestroyOverlay,
                    buffer(0),
                    buffer(1),
                    Teardown::ReleaseImports,
                ],
            ),
            (
                "only slot 1 held",
                held(false, HeldFrame::None, false, [false, true]),
                vec![buffer(1), Teardown::ReleaseImports],
            ),
            (
                "timer and unanswered frame only",
                held(true, HeldFrame::Unanswered, false, [false; SLOTS]),
                vec![
                    Teardown::CancelTimer,
                    Teardown::OrphanFrame,
                    Teardown::ReleaseImports,
                ],
            ),
        ];
        for (name, held, want) in cases {
            assert_eq!(teardown(held), want, "teardown_cases/{name}");
        }
    }

    #[test]
    fn release_cases() {
        for mode in MODES {
            let t0 = Instant::now();
            let free_wake = second_copy_at() + MIN_COPY_INTERVAL;
            let free_copy = free_wake + ms(1);
            let cases: Vec<(&str, Vec<Step>, [SlotState; SLOTS])> = vec![
                (
                    "release_outside_wait_frees_slot",
                    join(vec![
                        first_cycle(t0),
                        second_cycle(mode, t0),
                        vec![
                            step(
                                second_copy_at() + ms(20),
                                Input::Released { slot: 0 },
                                vec![],
                            ),
                            step(free_wake, Input::Wake, vec![Action::RequestFrame]),
                            step(
                                free_copy,
                                describe(SIZE),
                                copy_actions(mode, t0, 0, 1, SIZE, free_copy),
                            ),
                        ],
                    ]),
                    [SlotState::Copying, SlotState::OnScreen],
                ),
                (
                    "release_wait_then_release",
                    join(vec![first_cycle(t0), second_cycle(mode, t0), wait_cycle()]),
                    [SlotState::Free, SlotState::OnScreen],
                ),
                (
                    "release_on_free_slot_ignored",
                    join(vec![
                        first_cycle(t0),
                        vec![step(ms(20), Input::Released { slot: 1 }, vec![])],
                    ]),
                    [SlotState::OnScreen, SlotState::Free],
                ),
                (
                    "release_on_copying_slot_ignored",
                    first_cycle(t0)
                        .into_iter()
                        .take(5)
                        .chain([step(ms(5), Input::Released { slot: 0 }, vec![])])
                        .collect(),
                    [SlotState::Copying, SlotState::Free],
                ),
                (
                    "release_on_onscreen_slot_ignored",
                    join(vec![
                        first_cycle(t0),
                        vec![step(ms(20), Input::Released { slot: 0 }, vec![])],
                    ]),
                    [SlotState::OnScreen, SlotState::Free],
                ),
            ];
            for (name, steps, want) in cases {
                let cap = run(name, mode, t0, steps);
                assert_eq!(slot_states(&cap), want, "{name} ({mode:?}) slot states");
            }
        }
    }

    #[test]
    fn realloc_cases() {
        let new_size = Size {
            width: 3840,
            height: 2000,
        };
        for mode in MODES {
            let t0 = Instant::now();
            let wake = ms(4) + MIN_COPY_INTERVAL;
            let copy = wake + ms(1);
            let imported = copy + ms(2);
            let ready = imported + ms(10);
            let wake2 = imported + MIN_COPY_INTERVAL;
            let copy2 = wake2 + ms(120) + ms(1);
            let imported2 = copy2 + ms(2);
            let realloc_slot_1 = || {
                vec![
                    step(wake, Input::Wake, vec![Action::RequestFrame]),
                    step(copy, describe(new_size), vec![allocate(1, new_size)]),
                    step(
                        imported,
                        Input::Imported { slot: 1 },
                        copy_actions(mode, t0, 1, 0, SIZE, imported),
                    ),
                    step(
                        ready,
                        ready_input(),
                        vec![
                            ready_line(2, 1, ms(10)),
                            present(1, new_size, false),
                            Action::DestroyFrame,
                            at(t0, imported + MIN_COPY_INTERVAL),
                        ],
                    ),
                ]
            };
            let cases: Vec<(&str, Vec<Step>)> = vec![
                (
                    "size_change_reallocates_free_slot_now",
                    join(vec![first_cycle(t0), realloc_slot_1()]),
                ),
                (
                    "size_change_reallocates_other_slot_when_target",
                    join(vec![
                        first_cycle(t0),
                        realloc_slot_1(),
                        vec![
                            step(
                                wake2,
                                Input::Wake,
                                vec![Action::Report(Line::ReleaseWait {
                                    address: ADDRESS,
                                    slot: 0,
                                })],
                            ),
                            step(
                                wake2 + ms(120),
                                Input::Released { slot: 0 },
                                vec![
                                    Action::Report(Line::Released {
                                        address: ADDRESS,
                                        slot: 0,
                                        waited: ms(120),
                                    }),
                                    Action::RequestFrame,
                                ],
                            ),
                            step(copy2, describe(new_size), vec![allocate(0, new_size)]),
                            step(
                                imported2,
                                Input::Imported { slot: 0 },
                                copy_actions(mode, t0, 0, 1, new_size, imported2),
                            ),
                        ],
                    ]),
                ),
            ];
            for (name, steps) in cases {
                run(name, mode, t0, steps);
            }
        }
    }

    fn failed_line(seq: u64, consecutive: u32, reason: FailReason) -> Action {
        Action::Report(Line::Failed {
            address: ADDRESS,
            seq,
            reason,
            consecutive,
        })
    }

    fn fail_wake(n: u64) -> Duration {
        ms(1000 * n)
    }

    /// Four Failed cycles, one second apart, from Start.
    fn four_failures(t0: Instant) -> Vec<Step> {
        let mut steps = Vec::new();
        for n in 0..4u64 {
            let wake = fail_wake(n);
            steps.push(step(
                wake,
                if n == 0 { Input::Start } else { Input::Wake },
                vec![Action::RequestFrame],
            ));
            steps.push(step(
                wake + ms(1),
                Input::Failed,
                vec![
                    Action::DestroyFrame,
                    failed_line(n + 1, u32::try_from(n + 1).unwrap(), FailReason::Failed),
                    at(t0, wake + ms(1) + RETRY_DELAY),
                ],
            ));
        }
        steps
    }

    #[test]
    fn failure_cases() {
        for mode in MODES {
            let t0 = Instant::now();
            let wake = fail_wake(4);
            let cases: Vec<(&str, Vec<Step>, [SlotState; SLOTS])> = vec![
                (
                    "failed_reports_and_retries",
                    first_cycle(t0)
                        .into_iter()
                        .take(5)
                        .chain([step(
                            ms(20),
                            Input::Failed,
                            vec![
                                Action::DestroyFrame,
                                failed_line(1, 1, FailReason::Failed),
                                at(t0, ms(20) + RETRY_DELAY),
                            ],
                        )])
                        .collect(),
                    [SlotState::Free, SlotState::Free],
                ),
                (
                    "four_failures_then_ready_resets",
                    join(vec![
                        four_failures(t0),
                        vec![step(wake, Input::Wake, vec![Action::RequestFrame])],
                        vec![
                            step(
                                wake + ms(1),
                                describe(SIZE),
                                vec![
                                    Action::CreateOverlay { buffer_size: SIZE },
                                    allocate(0, SIZE),
                                    allocate(1, SIZE),
                                ],
                            ),
                            step(wake + ms(2), Input::OverlayConfigured, vec![]),
                            step(wake + ms(3), Input::Imported { slot: 1 }, vec![]),
                            step(
                                wake + ms(4),
                                Input::Imported { slot: 0 },
                                vec![
                                    Action::Copy {
                                        slot: 0,
                                        ignore_damage: true,
                                    },
                                    at(t0, wake + ms(4) + STALL_TIMEOUT),
                                ],
                            ),
                        ],
                        vec![
                            step(
                                wake + ms(14),
                                ready_input(),
                                vec![
                                    ready_line(5, 0, ms(10)),
                                    present(0, SIZE, false),
                                    Action::DestroyFrame,
                                    at(t0, wake + ms(4) + MIN_COPY_INTERVAL),
                                ],
                            ),
                            step(wake + ms(100), Input::Wake, vec![Action::RequestFrame]),
                            step(
                                wake + ms(101),
                                Input::Failed,
                                vec![
                                    Action::DestroyFrame,
                                    failed_line(6, 1, FailReason::Failed),
                                    at(t0, wake + ms(101) + RETRY_DELAY),
                                ],
                            ),
                        ],
                    ]),
                    [SlotState::OnScreen, SlotState::Free],
                ),
                (
                    "five_failures_remove",
                    join(vec![
                        four_failures(t0),
                        vec![
                            step(wake, Input::Wake, vec![Action::RequestFrame]),
                            step(
                                wake + ms(1),
                                Input::Failed,
                                vec![
                                    Action::DestroyFrame,
                                    failed_line(5, 5, FailReason::Failed),
                                    Action::Remove {
                                        reason: "5 consecutive capture failures".to_string(),
                                    },
                                ],
                            ),
                        ],
                    ]),
                    [SlotState::Free, SlotState::Free],
                ),
                (
                    "stall_after_1000ms",
                    first_cycle(t0)
                        .into_iter()
                        .take(5)
                        .chain([
                            step(
                                ms(4) + ms(999),
                                Input::Wake,
                                vec![at(t0, ms(4) + STALL_TIMEOUT)],
                            ),
                            step(
                                ms(4) + STALL_TIMEOUT,
                                Input::Wake,
                                vec![
                                    Action::DestroyFrame,
                                    failed_line(1, 1, FailReason::Stall),
                                    at(t0, ms(4) + STALL_TIMEOUT + RETRY_DELAY),
                                ],
                            ),
                        ])
                        .collect(),
                    [SlotState::Free, SlotState::Free],
                ),
                (
                    "failed_before_buffer_done_counts",
                    vec![
                        step(ms(0), Input::Start, vec![Action::RequestFrame]),
                        step(
                            ms(1),
                            Input::Failed,
                            vec![
                                Action::DestroyFrame,
                                failed_line(1, 1, FailReason::Failed),
                                at(t0, ms(1) + RETRY_DELAY),
                            ],
                        ),
                    ],
                    [SlotState::Free, SlotState::Free],
                ),
            ];
            for (name, steps, want) in cases {
                let cap = run(name, mode, t0, steps);
                assert_eq!(slot_states(&cap), want, "{name} ({mode:?}) slot states");
            }
        }
    }
}
