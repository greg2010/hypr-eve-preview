use std::time::Instant;

use calloop::timer::{TimeoutAction, Timer};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1;
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_v1::WpAlphaModifierV1;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use super::{App, Frame};
use crate::capture::{Action, DmabufInfo, Input};
use crate::exit::{failure, loop_error};
use crate::geometry::Size;
use crate::hypr;
use crate::protocol::hyprland_toplevel_export_frame_v1::{
    self, Flags, HyprlandToplevelExportFrameV1,
};
use crate::protocol::hyprland_toplevel_export_manager_v1::HyprlandToplevelExportManagerV1;

impl App {
    pub(super) fn feed(&mut self, address: u64, input: Input) {
        if self.stop.is_some() {
            return;
        }
        let Some(capture) = self
            .records
            .get_mut(&address)
            .and_then(|r| r.capture.as_mut())
        else {
            return;
        };
        let actions = capture.handle(input, Instant::now());
        self.execute(address, actions);
    }

    fn execute(&mut self, address: u64, actions: Vec<Action>) {
        for action in actions {
            if self.stop.is_some() {
                return;
            }
            match action {
                Action::RequestFrame => self.request_frame(address),
                Action::CreateOverlay { buffer_size } => self.create_overlay(address, buffer_size),
                Action::Allocate { slot, fourcc, size } => {
                    if let Err(stop) = self.allocate(address, slot, fourcc, size) {
                        self.stop(stop);
                    }
                }
                Action::Copy {
                    slot,
                    ignore_damage,
                } => {
                    let record = self.records.get(&address);
                    match (
                        record.and_then(|r| r.frame.as_ref()),
                        record.and_then(|r| r.buffers[slot].as_ref()),
                    ) {
                        (Some(frame), Some(buffer)) => {
                            frame
                                .proxy
                                .copy(&buffer.wl_buffer, i32::from(ignore_damage));
                        }
                        _ => self.out_of_step("copy"),
                    }
                }
                Action::Recommit { slot, buffer_size } => match self.records.get_mut(&address) {
                    Some(record) if record.buffers[slot].is_some() => {
                        let buffer = record.buffers[slot].as_ref().map(|b| b.wl_buffer.clone());
                        if let Some(buffer) = buffer {
                            for (overlay, _, _) in record.surfaces_mut() {
                                if overlay.has_buffer() {
                                    overlay.recommit(&buffer, buffer_size);
                                }
                            }
                        }
                    }
                    _ => self.out_of_step("recommit"),
                },
                Action::Present {
                    slot,
                    buffer_size,
                    y_invert,
                } => self.present(address, slot, buffer_size, y_invert),
                Action::DestroyFrame => {
                    if let Some(frame) = self.records.get_mut(&address).and_then(|r| r.frame.take())
                    {
                        frame.proxy.destroy();
                    }
                }
                Action::WakeAt(at) => self.arm_timer(address, at),
                Action::Report(line) => self.emit(&line),
                Action::Remove { reason } => {
                    let changes = self.clients.remove(address, reason);
                    self.apply_changes(changes);
                }
                Action::Exit { code, reason } => self.stop_with(code, reason),
            }
        }
    }

    fn request_frame(&mut self, address: u64) {
        self.last_sync_id += 1;
        let sync_id = self.last_sync_id;
        let Some(record) = self.records.get_mut(&address) else {
            return;
        };
        let handle = hypr::capture_handle(address);
        let proxy = self
            .export_manager
            .capture_toplevel(0, handle, &self.qh, address);
        self.conn.display().sync(&self.qh, (address, sync_id));
        record.frame = Some(Frame {
            proxy,
            sync_id,
            got_event: false,
            dmabuf: None,
        });
    }

    fn arm_timer(&mut self, address: u64, at: Instant) {
        let Some(record) = self.records.get_mut(&address) else {
            return;
        };
        if let Some(token) = record.timer.take() {
            self.loop_handle.remove(token);
        }
        let timer = Timer::from_deadline(at);
        match self
            .loop_handle
            .insert_source(timer, move |_, _, app: &mut App| {
                if let Some(record) = app.records.get_mut(&address) {
                    record.timer = None;
                }
                app.feed(address, Input::Wake);
                TimeoutAction::Drop
            }) {
            Ok(token) => {
                if let Some(record) = self.records.get_mut(&address) {
                    record.timer = Some(token);
                }
            }
            Err(e) => self.stop(failure(loop_error(e))),
        }
    }

    fn frame_event(
        &mut self,
        address: u64,
        proxy: &HyprlandToplevelExportFrameV1,
        event: hyprland_toplevel_export_frame_v1::Event,
    ) {
        if let Some(at) = self.orphans.iter().position(|f| f.proxy == *proxy) {
            self.orphans.remove(at).proxy.destroy();
            return;
        }
        let Some(frame) = self
            .records
            .get_mut(&address)
            .and_then(|r| r.frame.as_mut())
            .filter(|f| f.proxy == *proxy)
        else {
            return;
        };
        frame.got_event = true;
        if self.stop.is_some() {
            return;
        }
        let input = match event {
            hyprland_toplevel_export_frame_v1::Event::LinuxDmabuf {
                format,
                width,
                height,
            } => {
                frame.dmabuf = Some(DmabufInfo {
                    fourcc: format,
                    size: Size { width, height },
                });
                None
            }
            hyprland_toplevel_export_frame_v1::Event::BufferDone => Some(Input::FrameDescribed {
                dmabuf: frame.dmabuf.take(),
            }),
            hyprland_toplevel_export_frame_v1::Event::Flags { flags } => Some(Input::Flags {
                y_invert: y_invert(flags),
            }),
            hyprland_toplevel_export_frame_v1::Event::Ready {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => Some(Input::Ready {
                tv_sec: ready_tv_sec(tv_sec_hi, tv_sec_lo),
                tv_nsec,
            }),
            hyprland_toplevel_export_frame_v1::Event::Failed => Some(Input::Failed),
            _ => None,
        };
        if let Some(input) = input {
            self.feed(address, input);
        }
    }
}

wayland_client::delegate_noop!(App: ignore HyprlandToplevelExportManagerV1);

impl Dispatch<HyprlandToplevelExportFrameV1, u64> for App {
    fn event(
        state: &mut App,
        proxy: &HyprlandToplevelExportFrameV1,
        event: hyprland_toplevel_export_frame_v1::Event,
        address: &u64,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        state.frame_event(*address, proxy, event);
    }
}

wayland_client::delegate_noop!(App: ignore WpViewporter);

wayland_client::delegate_noop!(App: ignore WpViewport);

wayland_client::delegate_noop!(App: ignore WpAlphaModifierV1);

wayland_client::delegate_noop!(App: ignore WpAlphaModifierSurfaceV1);

impl Dispatch<WlCallback, (u64, u64)> for App {
    fn event(
        state: &mut App,
        _: &WlCallback,
        event: wl_callback::Event,
        (address, id): &(u64, u64),
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        let wl_callback::Event::Done { .. } = event else {
            return;
        };
        if let Some(at) = state.orphans.iter().position(|f| f.sync_id == *id) {
            state.orphans.remove(at);
            return;
        }
        let missing = state.records.get_mut(address).is_some_and(|r| {
            r.frame
                .take_if(|f| eventless_sync(f.sync_id, f.got_event, *id))
                .is_some()
        });
        if missing {
            state.feed(*address, Input::FrameMissing);
        }
    }
}

/// Whether a sync `done` of id `id` finds a frame that has had no event, which the compositor
/// answers with a missing-frame outcome only.
fn eventless_sync(sync_id: u64, got_event: bool, id: u64) -> bool {
    sync_id == id && !got_event
}

fn y_invert(flags: WEnum<Flags>) -> bool {
    match flags {
        WEnum::Value(f) => f.contains(Flags::YInvert),
        WEnum::Unknown(v) => Flags::from_bits_truncate(v).contains(Flags::YInvert),
    }
}

fn ready_tv_sec(hi: u32, lo: u32) -> u64 {
    (u64::from(hi) << 32) | u64::from(lo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eventless_sync_cases() {
        let cases = [
            ("matches, no event", 5, false, 5, true),
            ("matches, event seen", 5, true, 5, false),
            ("other id, no event", 5, false, 6, false),
            ("other id, event seen", 5, true, 6, false),
        ];
        for (name, sync_id, got_event, id, want) in cases {
            assert_eq!(eventless_sync(sync_id, got_event, id), want, "{name}");
        }
    }

    #[test]
    fn ready_tv_sec_cases() {
        let cases = [
            ("low word only", 0, 5, 5),
            ("high word only", 1, 0, 4_294_967_296),
            ("both words", 0x1234, 0x5678_9abc, 0x1234_5678_9abc),
            ("all ones", u32::MAX, u32::MAX, u64::MAX),
        ];
        for (name, hi, lo, want) in cases {
            assert_eq!(ready_tv_sec(hi, lo), want, "{name}");
        }
    }

    #[test]
    fn y_invert_cases() {
        let cases = [
            ("empty", WEnum::Value(Flags::empty()), false),
            ("y invert", WEnum::Value(Flags::YInvert), true),
            ("unknown bit only", WEnum::Unknown(0b10), false),
            ("unknown bit with y invert", WEnum::Unknown(0b11), true),
        ];
        for (name, flags, want) in cases {
            assert_eq!(y_invert(flags), want, "{name}");
        }
    }
}
