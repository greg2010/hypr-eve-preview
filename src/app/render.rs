use super::{App, Shown};
use crate::chrome::Chrome;
use crate::coords::point_of;
use crate::exit::failure;
use crate::geometry::{Offset, Size, thumbnail_size};
use crate::overlay::{Overlay, alpha_factor};
use crate::placement::capped_width;
use crate::{chrome, config};

impl App {
    pub(super) fn create_overlay(&mut self, address: u64, buffer_size: Size) {
        let Some((width, monitor)) = self.records.get(&address).map(|r| (r.width, r.monitor))
        else {
            return;
        };
        let width = capped_width(
            i64::from(width),
            &self.config.thumbnail,
            self.usable(monitor),
        );
        let size = thumbnail_size(width, buffer_size);
        let position = self.position_for(address, size);
        let factor = alpha_factor(self.effective_opacity(address));
        let Some(overlay) = self.new_overlay(monitor, Offset::from(position), size, factor) else {
            return;
        };
        if let Some(record) = self.records.get_mut(&address) {
            record.overlay = Some(overlay);
            record.buffer_size = Some(buffer_size);
            record.position = position;
            record.width = width;
        }
        let changes = self.clients.thumbnail_created(address);
        self.apply_changes(changes);
    }

    pub(super) fn base_opacity(&self) -> u32 {
        base_opacity(self.layout.opacity, self.config.thumbnail.opacity)
    }

    pub(super) fn toggles(&self) -> crate::control::Toggles {
        crate::control::Toggles {
            locked: self.layout.locked,
            hidden: self.hidden,
            snapping: self.layout.snapping,
            opacity: self.base_opacity(),
        }
    }

    pub(super) fn effective_opacity(&self, address: u64) -> u32 {
        effective_opacity(self.base_opacity(), self.hovered == Some(address))
    }

    pub(super) fn snap_distance(&self) -> u32 {
        snap_distance(self.layout.snapping, self.config.thumbnail.snap_distance)
    }

    pub(super) fn present(&mut self, address: u64, slot: usize, buffer_size: Size, y_invert: bool) {
        let ready = self
            .records
            .get(&address)
            .is_some_and(|r| r.overlay.is_some() && r.buffers[slot].is_some());
        let Some(record) = self.records.get(&address).filter(|_| ready) else {
            self.out_of_step("present");
            return;
        };
        let size = thumbnail_size(record.width, buffer_size);
        let redraw = record
            .surfaces()
            .any(|(overlay, _, chromed)| !chromed || overlay.size() != size);
        let home_position = self.position_for(address, size);
        let pinned = self.pinned(address);
        if redraw && !self.draw_chrome(address, size, None) {
            return;
        }
        let Some(record) = self.records.get_mut(&address) else {
            return;
        };
        record.buffer_size = Some(buffer_size);
        record.shown = Some(Shown { slot, y_invert });
        let Some(buffer) = record.buffers[slot].as_ref().map(|b| b.wl_buffer.clone()) else {
            return;
        };
        if let Some(home) = record.overlay.as_mut() {
            let at = if pinned {
                home.position()
            } else {
                Offset::from(home_position)
            };
            home.present(&buffer, buffer_size, size, at, y_invert);
            if !pinned {
                record.position = point_of(home.position());
            }
        }
        for traveller in &mut record.travellers {
            let overlay = &mut traveller.overlay;
            if overlay.is_configured() {
                let at = overlay.position();
                overlay.present(&buffer, buffer_size, size, at, y_invert);
            }
        }
    }

    /// Draws the chrome on every surface of the record, or only the one on `only`, each at its
    /// monitor's scale. Returns false after stopping on an error.
    pub(super) fn draw_chrome(&mut self, address: u64, logical: Size, only: Option<usize>) -> bool {
        let Some(label) = self.clients.get(address).map(|t| t.label()) else {
            return true;
        };
        let border = &self.config.border;
        let ring = (self.clients.ring_owner() == Some(address) && border.width > 0)
            .then_some((border.width, border.color));
        let opacity = self.effective_opacity(address);
        let Some(record) = self.records.get_mut(&address) else {
            return true;
        };
        let mut failed = None;
        for (overlay, monitor, chromed) in record.surfaces_mut() {
            if only.is_some_and(|m| m != monitor) {
                continue;
            }
            let scale = self.monitors[monitor].scale;
            let chrome = Chrome {
                ring,
                label: &label,
                style: &self.config.label,
                scale,
                opacity,
            };
            let font = &self.font;
            let buffer = chrome::buffer_size(logical, scale);
            match overlay.draw_chrome(logical, buffer, |canvas, size| {
                chrome::render(canvas, size, &chrome, font);
            }) {
                Ok(()) => *chromed = true,
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        match failed {
            None => true,
            Some(e) => {
                self.stop(failure(e));
                false
            }
        }
    }

    pub(super) fn rerender_chrome(&mut self, address: u64) {
        let Some(size) = self
            .records
            .get(&address)
            .filter(|r| r.chromed)
            .and_then(|r| r.overlay.as_ref())
            .map(Overlay::size)
        else {
            return;
        };
        if !self.draw_chrome(address, size, None) {
            return;
        }
        if let Some(record) = self.records.get_mut(&address) {
            for (overlay, _, _) in record.surfaces_mut() {
                if overlay.is_configured() {
                    overlay.commit();
                }
            }
        }
    }

    pub(super) fn set_alpha(&mut self, address: u64, factor: u32) {
        if let Some(record) = self.records.get_mut(&address) {
            for (overlay, _, _) in record.surfaces_mut() {
                overlay.set_alpha(factor);
            }
        }
    }
}

/// The base opacity in percent: the layout file's value when it has one, else the config's.
fn base_opacity(layout: Option<u32>, config: u32) -> u32 {
    layout.unwrap_or(config)
}

/// The drag snap distance: `configured` while snapping is on, else 0, which never snaps.
fn snap_distance(snapping: bool, configured: u32) -> u32 {
    if snapping { configured } else { 0 }
}

/// The opacity a thumbnail shows: `config::MAX_OPACITY` while hovered, otherwise `base`, from
/// `base_opacity`.
fn effective_opacity(base: u32, hovered: bool) -> u32 {
    if hovered { config::MAX_OPACITY } else { base }
}

/// The new effective opacity in percent, only when a hover change alters it.
pub(super) fn opacity_change(base: u32, hovered_before: bool, hovered_after: bool) -> Option<u32> {
    let before = effective_opacity(base, hovered_before);
    let after = effective_opacity(base, hovered_after);
    (before != after).then_some(after)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_distance_cases() {
        let cases = [(true, 10, 10), (false, 10, 0), (true, 0, 0)];
        for (snapping, configured, want) in cases {
            assert_eq!(
                snap_distance(snapping, configured),
                want,
                "{snapping} {configured}"
            );
        }
    }

    #[test]
    fn base_opacity_cases() {
        let cases = [
            ("no saved value", None, 100, 100),
            ("saved value wins", Some(50), 100, 50),
            ("saved zero wins", Some(0), 100, 0),
        ];
        for (name, layout, config, want) in cases {
            assert_eq!(base_opacity(layout, config), want, "{name}");
        }
    }

    #[test]
    fn opacity_change_cases() {
        let cases = [
            ("base 100 enter", 100, false, true, None),
            ("base 100 leave", 100, true, false, None),
            ("base 50 enter", 50, false, true, Some(100)),
            ("base 50 leave", 50, true, false, Some(50)),
            ("base 50 enter while already hovered", 50, true, true, None),
            ("base 50 no hover before or after", 50, false, false, None),
            ("base 0 enter", 0, false, true, Some(100)),
            ("base 0 leave", 0, true, false, Some(0)),
        ];
        for (name, base, before, after, want) in cases {
            assert_eq!(opacity_change(base, before, after), want, "{name}");
        }
    }
}
