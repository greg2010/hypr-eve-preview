use smithay_client_toolkit::seat::pointer::{
    BTN_LEFT, CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec,
};
use smithay_client_toolkit::seat::relative_pointer::{RelativeMotionEvent, RelativePointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use wayland_client::protocol::wl_pointer::WlPointer;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, QueueHandle};
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1;

use super::render::opacity_change;
use super::{App, PressStart, SeatPointer, Spawn};
use crate::coords::{
    Area, area_origin, desired_origin, dragged_origin, local_offset, pointer_global, touched,
    under_pointer, usable_local,
};
use crate::geometry::{Rect, Size};
use crate::input::{Cursor, Effect, GRIP_SIZE, PointerInput};
use crate::overlay::{Overlay, alpha_factor};
use crate::report::Line;
use crate::surface::{SurfaceEvent, SurfaceState};
use crate::{config, ipc};

impl App {
    fn pointer_input(&mut self, pointer: &WlPointer, input: PointerInput) {
        if self.stop.is_some() {
            return;
        }
        self.route_input(Some(pointer), input);
    }

    fn route_input(&mut self, pointer: Option<&WlPointer>, input: PointerInput) {
        let before = self.gestures.active();
        let effects = self.gestures.handle(input);
        self.run_effects(pointer, effects);
        if gesture_ended(before, self.gestures.active()) {
            self.recompute_placement();
        }
    }

    pub(super) fn run_effects(&mut self, pointer: Option<&WlPointer>, effects: Vec<Effect>) {
        for effect in effects {
            if self.stop.is_some() {
                return;
            }
            match effect {
                Effect::Cursor(cursor) => {
                    // Only pointer events produce cursor effects, and they carry their pointer.
                    if let Some(pointer) = pointer {
                        self.set_cursor(pointer, cursor);
                    }
                }
                Effect::Click { address } => self.click(address),
                Effect::Drag { address, offset } => {
                    self.drag(address, offset);
                }
                Effect::DragEnd { address } => self.drag_end(address),
                Effect::ResizeEnd { address } => {
                    self.mark_user_placed(address);
                    self.layout_update(address);
                }
                Effect::Resize { address, steps } => {
                    let Some(width) = self.records.get(&address).map(|r| r.width) else {
                        continue;
                    };
                    let step = i64::from(self.config.resize.step);
                    self.set_geometry(address, i64::from(width) + i64::from(steps) * step, None);
                    self.mark_user_placed(address);
                    self.recompute_placement();
                    self.layout_update(address);
                }
                Effect::ResizeTo { address, dx } => {
                    let Some(start) = self.press.filter(|p| p.address == address) else {
                        continue;
                    };
                    let width = i64::from(start.width) + dx.round() as i64;
                    self.set_geometry(address, width, None);
                }
            }
        }
    }

    fn set_cursor(&mut self, pointer: &WlPointer, cursor: Cursor) {
        let icon = match cursor {
            Cursor::Default => CursorIcon::Default,
            Cursor::Grabbing => CursorIcon::Grabbing,
            Cursor::SeResize => CursorIcon::SeResize,
        };
        let result = self
            .pointers
            .iter()
            .find(|p| p.themed.pointer() == pointer)
            .map(|p| p.themed.set_cursor(&self.conn, icon));
        if let Some(Err(e)) = result {
            self.stop_with(1, format!("set_cursor: {e}"));
        }
    }

    fn click(&mut self, address: u64) {
        let Some(workspace) = self.clients.get(address).map(|t| t.workspace.clone()) else {
            return;
        };
        let request = ipc::workspace_dispatch(&workspace);
        let result = ipc::dispatch_result(ipc::request(&self.requests, &request));
        self.emit(&Line::Dispatch {
            address,
            request,
            result,
        });
    }

    fn others_on(&self, monitor: usize, except: u64) -> Vec<Rect> {
        self.records
            .iter()
            .filter(|(other, r)| **other != except && r.monitor == monitor)
            .filter_map(|(_, r)| {
                r.overlay.as_ref().map(|o| Rect {
                    x: r.position.x,
                    y: r.position.y,
                    width: o.size().width,
                    height: o.size().height,
                })
            })
            .collect()
    }

    fn drag(&mut self, address: u64, offset: (f64, f64)) {
        let Some(start) = self.press.filter(|p| p.address == address) else {
            return;
        };
        let Some(record) = self.records.get(&address) else {
            return;
        };
        if matches!(record.surface_state, SurfaceState::Landing { .. }) {
            return;
        }
        let Some(size) = record.overlay.as_ref().map(Overlay::size) else {
            return;
        };
        let home = record.monitor;
        let origin = area_origin(&self.monitors[home]);
        let pointer = pointer_global(origin, start.position, start.grab, offset);
        let under = under_pointer(&self.monitors, pointer, start.monitor);
        let others = self.others_on(under, address);
        let at = dragged_origin(
            desired_origin(origin, start.position, offset),
            size,
            under,
            &self.monitors,
            &others,
            self.snap_distance(),
        );
        if let Some(press) = self.press.as_mut() {
            press.monitor = under;
            press.at = Some(at);
        }
        let rect = Area {
            x: at.0,
            y: at.1,
            width: i64::from(size.width),
            height: i64::from(size.height),
        };
        let spawn = Spawn {
            origin: at,
            size,
            factor: alpha_factor(config::MAX_OPACITY),
        };
        let touching = touched(&self.monitors, home, rect);
        self.step_surfaces(address, SurfaceEvent::Touch(touching), Some(spawn));
        let monitors = &self.monitors;
        if let Some(record) = self.records.get_mut(&address) {
            for (overlay, monitor, _) in record.surfaces_mut() {
                let to = local_offset(at, &monitors[monitor]);
                if overlay.position() != to {
                    overlay.move_to(to);
                }
            }
        }
    }

    /// Ends a drag: on home the rectangle is clamped and the width capped. Elsewhere the landing
    /// surface takes over at its configure, where the commit clamps and caps.
    fn drag_end(&mut self, address: u64) {
        self.mark_user_placed(address);
        let end = self
            .press
            .filter(|p| p.address == address)
            .and_then(|p| p.at.map(|at| (p.monitor, at)));
        if let Some(press) = self.press.as_mut().filter(|p| p.address == address) {
            press.at = None;
        }
        let Some((landing, at)) = end else {
            self.layout_update(address);
            return;
        };
        let Some(record) = self.records.get_mut(&address) else {
            return;
        };
        let Some(size) = record.overlay.as_ref().map(Overlay::size) else {
            return;
        };
        let (home, width) = (record.monitor, record.width);
        if landing == home {
            self.step_surfaces(address, SurfaceEvent::ReleaseHome, None);
            let relative = usable_local(at, &self.monitors[home]);
            self.set_geometry(address, i64::from(width), Some(relative));
            self.settle(address);
            return;
        }
        let configured = record
            .surface_on(landing)
            .is_some_and(|o| o.is_configured());
        let spawn = Spawn {
            origin: at,
            size,
            factor: alpha_factor(config::MAX_OPACITY),
        };
        let event = SurfaceEvent::ReleaseAt {
            monitor: landing,
            configured,
        };
        self.step_surfaces(address, event, Some(spawn));
        let pending = self
            .records
            .get(&address)
            .is_some_and(|r| matches!(r.surface_state, SurfaceState::Landing { .. }));
        if !pending {
            self.settle(address);
        }
    }

    /// A missing leave, for example a pointer capability loss, would leave a stale 100 %.
    fn set_hovered(&mut self, next: Option<u64>) {
        if self.stop.is_some() {
            return;
        }
        let previous = self.hovered;
        if previous == next {
            return;
        }
        self.hovered = next;
        if let Some(address) = previous {
            self.hover_changed(address, true, false);
        }
        if let Some(address) = next {
            self.hover_changed(address, false, true);
        }
    }

    fn hover_changed(&mut self, address: u64, before: bool, after: bool) {
        let Some(percent) = opacity_change(self.base_opacity(), before, after) else {
            return;
        };
        self.set_alpha(address, alpha_factor(percent));
        self.rerender_chrome(address);
    }

    fn begin_press(&mut self, address: u64, grab: (f64, f64)) {
        if let Some(record) = self.records.get(&address) {
            self.press = Some(PressStart {
                address,
                position: record.position,
                grab,
                width: record.width,
                monitor: record.monitor,
                at: None,
            });
        }
    }

    fn in_grip_at(&self, address: u64, position: (f64, f64)) -> bool {
        self.records
            .get(&address)
            .and_then(|r| r.overlay.as_ref())
            .is_some_and(|o| in_grip(position, o.size()))
    }

    pub(super) fn end_pointer_interaction(&mut self) {
        if let Some(address) = self.gestures.active()
            && self.stop.is_none()
        {
            self.route_input(None, PointerInput::Leave { address });
        }
        self.press = None;
        self.set_hovered(None);
    }

    fn drop_pointers(&mut self, seat: &WlSeat) {
        self.end_pointer_interaction();
        for pointer in std::mem::take(&mut self.pointers) {
            if pointer.seat == *seat {
                pointer.relative.destroy();
                drop(pointer.themed);
            } else {
                self.pointers.push(pointer);
            }
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        if capability != Capability::Pointer {
            return;
        }
        let surface = self.compositor.create_surface(qh);
        let themed = match self.seat_state.get_pointer_with_theme::<App, ()>(
            qh,
            &seat,
            self.shm.wl_shm(),
            surface,
            ThemeSpec::System,
        ) {
            Ok(themed) => themed,
            Err(e) => {
                self.stop_with(1, format!("get_pointer_with_theme: {e}"));
                return;
            }
        };
        match self
            .relative_pointer_state
            .get_relative_pointer(themed.pointer(), qh)
        {
            Ok(relative) => self.pointers.push(SeatPointer {
                seat,
                themed,
                relative,
            }),
            Err(e) => self.stop_with(1, format!("get_relative_pointer: {e}")),
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            self.drop_pointers(&seat);
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: WlSeat) {
        self.drop_pointers(&seat);
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let Some((address, _)) = self.surface_owner(&event.surface) else {
                continue;
            };
            let input = match &event.kind {
                PointerEventKind::Enter { .. } => {
                    self.set_hovered(Some(address));
                    PointerInput::Enter { address }
                }
                PointerEventKind::Leave { .. } => {
                    self.set_hovered(None);
                    PointerInput::Leave { address }
                }
                PointerEventKind::Motion { .. } => PointerInput::Motion {
                    address,
                    in_grip: self.in_grip_at(address, event.position),
                },
                PointerEventKind::Press { button, .. } => {
                    if *button == BTN_LEFT {
                        self.begin_press(address, event.position);
                    }
                    PointerInput::Press {
                        address,
                        button: *button,
                        in_grip: self.in_grip_at(address, event.position),
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    PointerInput::Release { button: *button }
                }
                PointerEventKind::Axis { vertical, .. } => PointerInput::Axis {
                    address,
                    value120: vertical.value120,
                    discrete: vertical.discrete,
                },
            };
            self.pointer_input(pointer, input);
        }
        self.pointer_input(pointer, PointerInput::FrameEnd);
    }
}

impl RelativePointerHandler for App {
    fn relative_pointer_motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpRelativePointerV1,
        pointer: &WlPointer,
        event: RelativeMotionEvent,
    ) {
        self.pointer_input(
            pointer,
            PointerInput::Relative {
                dx: event.delta.0,
                dy: event.delta.1,
            },
        );
    }
}

/// Whether the gesture that was active before a pointer event is over after it, which frees a
/// client the gesture kept out of the default row, so it is re-placed.
fn gesture_ended(before: Option<u64>, after: Option<u64>) -> bool {
    before.is_some() && after.is_none()
}

fn in_grip(position: (f64, f64), size: Size) -> bool {
    let grip = f64::from(GRIP_SIZE);
    position.0 >= f64::from(size.width) - grip && position.1 >= f64::from(size.height) - grip
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gesture_ended_cases() {
        let cases = [
            ("idle to idle", None, None, false),
            ("same gesture", Some(7), Some(7), false),
            ("gesture over", Some(7), None, true),
            ("gesture begins", None, Some(7), false),
        ];
        for (name, before, after, want) in cases {
            assert_eq!(gesture_ended(before, after), want, "{name}");
        }
    }

    #[test]
    fn in_grip_cases() {
        let size = Size {
            width: 480,
            height: 264,
        };
        let cases = [
            ("corner of the grip", (464.0, 248.0), true),
            ("one px left of the grip", (463.0, 248.0), false),
            ("one px above the grip", (464.0, 247.0), false),
            ("far corner", (479.9, 263.9), true),
            ("origin", (0.0, 0.0), false),
        ];
        for (name, position, want) in cases {
            assert_eq!(in_grip(position, size), want, "{name}");
        }
    }
}
