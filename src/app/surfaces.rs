use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, QueueHandle};

use super::{App, Spawn, Traveller};
use crate::capture::Input;
use crate::coords::local_offset;
use crate::geometry::thumbnail_size;
use crate::input::PointerInput;
use crate::overlay::alpha_factor;
use crate::surface::{SurfaceAction, SurfaceEvent, SurfaceState, surface_step};

impl App {
    /// The record that owns `surface` and the monitor the surface is on.
    pub(super) fn surface_owner(&self, surface: &WlSurface) -> Option<(u64, usize)> {
        self.records.iter().find_map(|(address, r)| {
            r.surfaces()
                .find(|(o, _, _)| o.surface() == surface)
                .map(|(_, monitor, _)| (*address, monitor))
        })
    }

    /// Applies the surface state machine's answer to `event` and runs its actions. `spawn` is
    /// needed by the events that can create a surface.
    pub(super) fn step_surfaces(
        &mut self,
        address: u64,
        event: SurfaceEvent,
        spawn: Option<Spawn>,
    ) {
        let Some(record) = self.records.get_mut(&address) else {
            return;
        };
        let (next, actions) = surface_step(&record.surface_state, event);
        record.surface_state = next;
        for action in actions {
            if self.stop.is_some() {
                return;
            }
            match action {
                SurfaceAction::Create(monitor) => {
                    if let Some(spawn) = &spawn {
                        self.create_traveller(address, monitor, spawn);
                    }
                }
                SurfaceAction::Destroy(monitor) => {
                    if let Some(record) = self.records.get_mut(&address) {
                        record.destroy_traveller(monitor);
                    }
                }
                SurfaceAction::Present(monitor) => self.present_shown(address, monitor),
                SurfaceAction::Commit(monitor) => self.commit_traveller(address, monitor),
            }
        }
    }

    fn create_traveller(&mut self, address: u64, monitor: usize, spawn: &Spawn) {
        let position = local_offset(spawn.origin, &self.monitors[monitor]);
        let Some(overlay) = self.new_overlay(monitor, position, spawn.size, spawn.factor) else {
            return;
        };
        let Some(record) = self.records.get_mut(&address) else {
            overlay.destroy();
            return;
        };
        record.travellers.push(Traveller {
            overlay,
            monitor,
            chromed: false,
        });
        self.draw_chrome(address, spawn.size, Some(monitor));
    }

    /// Shows the last presented buffer on the surface on `monitor`, once it is configured, at the
    /// thumbnail size that buffer gives. Sends no position change.
    fn present_shown(&mut self, address: u64, monitor: usize) {
        let Some(record) = self.records.get_mut(&address) else {
            return;
        };
        let (Some(shown), Some(buffer_size)) = (record.shown, record.buffer_size) else {
            return;
        };
        let Some(buffer) = record.buffers[shown.slot]
            .as_ref()
            .map(|b| b.wl_buffer.clone())
        else {
            return;
        };
        let size = thumbnail_size(record.width, buffer_size);
        let Some(overlay) = record.surface_on(monitor).filter(|o| o.is_configured()) else {
            return;
        };
        let at = overlay.position();
        overlay.present(&buffer, buffer_size, size, at, shown.y_invert);
    }

    /// The surface on `monitor` becomes the record's overlay, with the width capped and the
    /// position clamped to that monitor. The old home surface goes without a `leave`.
    fn commit_traveller(&mut self, address: u64, monitor: usize) {
        self.cancel_gesture(address);
        let Some(record) = self.records.get_mut(&address) else {
            return;
        };
        let Some(at) = record.travellers.iter().position(|t| t.monitor == monitor) else {
            return;
        };
        let traveller = record.travellers.remove(at);
        let landed = traveller.overlay.position();
        let width = i64::from(record.width);
        record.monitor = monitor;
        record.chromed = traveller.chromed;
        if let Some(home) = record.overlay.replace(traveller.overlay) {
            home.destroy();
        }
        if self.hovered == Some(address) {
            self.hovered = None;
        }
        let rectangle = (i64::from(landed.x), i64::from(landed.y));
        self.set_geometry(address, width, Some(rectangle));
        let factor = alpha_factor(self.effective_opacity(address));
        self.set_alpha(address, factor);
        self.rerender_chrome(address);
        self.recompute_placement();
    }

    /// Ends a gesture on the record whose surface is about to be destroyed: the compositor sends
    /// the release to the destroyed surface, where `pointer_frame` cannot route it.
    fn cancel_gesture(&mut self, address: u64) {
        if self.gestures.active() == Some(address) && self.stop.is_none() {
            self.press = None;
            // The leave's effects are dropped: a cancelled gesture saves and moves nothing.
            self.gestures.handle(PointerInput::Leave { address });
        }
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.stop_with(1, "overlay closed by compositor");
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        let Some((address, monitor)) = self.surface_owner(layer.wl_surface()) else {
            return;
        };
        let Some(record) = self.records.get_mut(&address) else {
            return;
        };
        let home = monitor == record.monitor;
        let Some(overlay) = record.surface_on(monitor).filter(|o| !o.is_configured()) else {
            return;
        };
        overlay.set_configured();
        if home {
            self.feed(address, Input::OverlayConfigured);
            return;
        }
        let landing = matches!(record.surface_state, SurfaceState::Landing { .. });
        self.step_surfaces(address, SurfaceEvent::Configured(monitor), None);
        let committed = landing
            && self
                .records
                .get(&address)
                .is_some_and(|r| r.surface_state == SurfaceState::Home);
        if committed {
            self.settle(address);
        }
    }
}
