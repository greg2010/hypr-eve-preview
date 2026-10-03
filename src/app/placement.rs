use std::time::Instant;

use calloop::timer::{TimeoutAction, Timer};

use super::{App, Spawn};
use crate::coords::{layout_point, point_of};
use crate::exit::{failure, loop_error};
use crate::geometry::{Offset, Point, Size};
use crate::layout;
use crate::layout::{KeyChange, SaveAction};
use crate::overlay::alpha_factor;
use crate::placement::{
    Origin, Settle, Trigger, capped_width, fitted, follows_row, placements, relocation, settled,
};
use crate::report::Line;
use crate::surface::{SurfaceEvent, SurfaceState, bound_monitor};

impl App {
    fn arm_save(&mut self, at: Instant) {
        let timer = Timer::from_deadline(at);
        match self
            .loop_handle
            .insert_source(timer, |_, _, app: &mut App| {
                app.save_timer = None;
                app.write_layout();
                TimeoutAction::Drop
            }) {
            Ok(token) => self.save_timer = Some(token),
            Err(e) => self.stop(failure(loop_error(e))),
        }
    }

    pub(super) fn write_layout(&mut self) {
        let line = match layout::save(&self.layout_path, &self.layout) {
            Ok(()) => Line::LayoutSaved {
                path: self.layout_path.clone(),
                entries: self.layout.entries.len(),
            },
            Err(e) => Line::LayoutError {
                path: self.layout_path.clone(),
                error: e.to_string(),
                renamed: None,
            },
        };
        self.schedule.wrote(Instant::now());
        self.emit(&line);
    }

    pub(super) fn request_save(&mut self) {
        match self.schedule.request(Instant::now()) {
            SaveAction::WriteNow => self.write_layout(),
            SaveAction::ArmAt(at) => self.arm_save(at),
            SaveAction::Joined => {}
        }
    }

    pub(super) fn layout_update(&mut self, address: u64) {
        let Some(record) = self.records.get(&address) else {
            return;
        };
        let Some(key) = self.clients.get(address).and_then(|t| t.key()) else {
            return;
        };
        let entry = record.entry(&self.monitors[record.monitor].name);
        self.layout.entries.insert(key.to_string(), entry);
        self.request_save();
    }

    pub(super) fn key_changed(&mut self, address: u64) {
        let Some(key) = self.clients.get(address).and_then(|t| t.key()) else {
            return;
        };
        let Some(record) = self.records.get(&address) else {
            return;
        };
        let current = record.entry(&self.monitors[record.monitor].name);
        let user_placed = record.origin == Origin::User;
        match layout::key_change(&mut self.layout, &key.to_string(), current, user_placed) {
            KeyChange::Saved => self.request_save(),
            KeyChange::Apply(entry) => {
                if let Some(record) = self.records.get_mut(&address) {
                    record.origin = Origin::Saved;
                }
                let target = self.key_target(address, entry.output.as_deref());
                self.relocate(
                    address,
                    target,
                    i64::from(entry.width),
                    Some((i64::from(entry.x), i64::from(entry.y))),
                );
                self.recompute_placement();
            }
            KeyChange::Default => {
                if let Some(record) = self.records.get_mut(&address) {
                    record.origin = Origin::Default;
                }
                let target = self.key_target(address, None);
                self.relocate(
                    address,
                    target,
                    i64::from(self.config.thumbnail.width),
                    None,
                );
                self.recompute_placement();
            }
        }
    }

    fn key_target(&self, address: u64, output: Option<&str>) -> Option<usize> {
        let record = self.records.get(&address)?;
        let trigger = Trigger::KeyChange {
            output,
            busy: self.pinned(address),
        };
        relocation(
            trigger,
            &self.monitors,
            record.origin,
            record.monitor,
            self.default_monitor,
        )
    }

    /// Applies a geometry request. With a `target` a surface there takes over at its first
    /// configure; `None` keeps the monitor.
    fn relocate(
        &mut self,
        address: u64,
        target: Option<usize>,
        requested: i64,
        desired: Option<(i64, i64)>,
    ) {
        let Some(monitor) = target else {
            self.set_geometry(address, requested, desired);
            return;
        };
        let Some(record) = self.records.get_mut(&address) else {
            return;
        };
        let Some(buffer_size) = record.buffer_size.filter(|_| record.overlay.is_some()) else {
            record.monitor = monitor;
            self.set_geometry(address, requested, desired);
            return;
        };
        let usable = self.usable(monitor);
        let (x, y) = desired.unwrap_or_else(|| self.row_slot(address));
        let (width, size, position) = fitted(
            requested,
            &self.config.thumbnail,
            buffer_size,
            usable,
            (x, y),
        );
        let spawn = Spawn {
            origin: layout_point(position, &self.monitors[monitor]),
            size,
            factor: alpha_factor(self.effective_opacity(address)),
        };
        if let Some(record) = self.records.get_mut(&address) {
            record.width = width;
        }
        let event = SurfaceEvent::ReleaseAt {
            monitor,
            configured: false,
        };
        self.step_surfaces(address, event, Some(spawn));
    }

    /// What follows a drop or a commit: a default-placed or saved record off the monitor it
    /// belongs on starts its move there, a saved record applies its entry's geometry, a
    /// user-placed record is saved.
    pub(super) fn settle(&mut self, address: u64) {
        let Some(record) = self.records.get(&address) else {
            return;
        };
        let entry = (record.origin == Origin::Saved)
            .then(|| {
                let key = self.clients.get(address)?.key()?;
                self.layout.entries.get(&key.to_string()).cloned()
            })
            .flatten();
        let target = relocation(
            Trigger::Settled {
                entry: entry.as_ref().map(|e| e.output.as_deref()),
            },
            &self.monitors,
            record.origin,
            record.monitor,
            self.default_monitor,
        );
        match settled(target, record.origin, entry.as_ref(), record.width) {
            Settle::Relocate { target, width, at } => self.relocate(address, target, width, at),
            Settle::Save => self.layout_update(address),
            Settle::Stay => {}
        }
    }

    fn default_order(&self) -> Vec<u64> {
        self.clients
            .ordered()
            .into_iter()
            .filter(|a| {
                self.records
                    .get(a)
                    .is_some_and(|r| r.origin == Origin::Default)
            })
            .collect()
    }

    fn row_slot(&self, address: u64) -> (i64, i64) {
        let index = self
            .default_order()
            .iter()
            .position(|a| *a == address)
            .unwrap_or(0);
        self.default_slot(index)
    }

    pub(super) fn position_for(&self, address: u64, size: Size) -> Point {
        let Some(record) = self.records.get(&address) else {
            return Point { x: 0, y: 0 };
        };
        let (x, y) = if follows_row(record.origin, self.gestures.active(), address) {
            self.row_slot(address)
        } else {
            (i64::from(record.position.x), i64::from(record.position.y))
        };
        layout::clamp_position(x, y, size, self.usable(record.monitor))
    }

    fn default_slot(&self, index: usize) -> (i64, i64) {
        layout::default_position(index, &self.config.placement, self.config.thumbnail.width)
    }

    pub(super) fn recompute_placement(&mut self) {
        for (index, address) in placements(&self.default_order(), self.gestures.active()) {
            let (x, y) = self.default_slot(index);
            self.move_clamped(address, x, y);
        }
    }

    /// Whether a drag of the record has produced a rectangle and not yet ended.
    fn dragging(&self, address: u64) -> bool {
        self.press
            .is_some_and(|p| p.address == address && p.at.is_some())
    }

    /// Whether the record's surfaces, not `position`, say where it is: a drag is in progress or
    /// a surface on another monitor exists.
    pub(super) fn pinned(&self, address: u64) -> bool {
        self.dragging(address)
            || self
                .records
                .get(&address)
                .is_some_and(|r| r.surface_state != SurfaceState::Home)
    }

    fn move_clamped(&mut self, address: u64, x: i64, y: i64) {
        if self.pinned(address) {
            return;
        }
        let Some(usable) = self.records.get(&address).map(|r| self.usable(r.monitor)) else {
            return;
        };
        let Some(record) = self.records.get_mut(&address) else {
            return;
        };
        let Some(overlay) = record.overlay.as_mut() else {
            return;
        };
        let position = layout::clamp_position(x, y, overlay.size(), usable);
        if position != record.position {
            overlay.move_to(Offset::from(position));
            record.position = position;
        }
    }

    /// Gives every surface `size`, with the chrome redrawn at each monitor's scale when any
    /// surface changes size. The home surface also moves to `home_at` when given. Other
    /// surfaces keep their margins.
    fn apply_geometry(&mut self, address: u64, size: Size, home_at: Option<Offset>) {
        let Some(record) = self.records.get(&address) else {
            return;
        };
        let resized = record
            .surfaces()
            .any(|(overlay, _, _)| overlay.size() != size);
        if resized && !self.draw_chrome(address, size, None) {
            return;
        }
        let Some(record) = self.records.get_mut(&address) else {
            return;
        };
        let home = record.monitor;
        for (overlay, monitor, _) in record.surfaces_mut() {
            let at = match home_at {
                Some(at) if monitor == home => at,
                _ => overlay.position(),
            };
            if resized {
                overlay.resize(size, at);
            } else if at != overlay.position() {
                overlay.move_to(at);
            }
        }
        if let Some(at) = home_at {
            record.position = point_of(at);
        }
    }

    pub(super) fn set_geometry(
        &mut self,
        address: u64,
        requested: i64,
        desired: Option<(i64, i64)>,
    ) {
        let Some(usable) = self
            .records
            .get(&address)
            .map(|r| self.usable(bound_monitor(&r.surface_state, r.monitor)))
        else {
            return;
        };
        let pinned = self.pinned(address);
        let thumbnail = &self.config.thumbnail;
        let Some(record) = self.records.get_mut(&address) else {
            return;
        };
        let (x, y) =
            desired.unwrap_or((i64::from(record.position.x), i64::from(record.position.y)));
        let Some(buffer_size) = record.buffer_size.filter(|_| record.overlay.is_some()) else {
            record.width = capped_width(requested, thumbnail, usable);
            record.position = Point {
                x: u32::try_from(x.max(0)).unwrap_or(u32::MAX),
                y: u32::try_from(y.max(0)).unwrap_or(u32::MAX),
            };
            return;
        };
        let (width, size, position) = fitted(requested, thumbnail, buffer_size, usable, (x, y));
        record.width = width;
        self.apply_geometry(address, size, (!pinned).then_some(Offset::from(position)));
    }

    pub(super) fn mark_user_placed(&mut self, address: u64) {
        if let Some(record) = self.records.get_mut(&address) {
            record.origin = Origin::User;
        }
    }
}
