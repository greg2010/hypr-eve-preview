#![deny(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod capture;
mod chrome;
mod cli;
mod clients;
mod config;
mod control;
mod dmabuf;
mod geometry;
mod hypr;
mod input;
mod ipc;
mod layout;
mod overlay;
mod protocol;
mod report;
#[cfg(test)]
mod testutil;
mod tray;

use std::collections::{BTreeMap, HashMap};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use calloop::generic::Generic;
use calloop::ping::{Ping, make_ping};
use calloop::signals::{Signal, Signals};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::dmabuf::{DmabufFeedback, DmabufHandler, DmabufState};
use smithay_client_toolkit::error::GlobalError;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::seat::pointer::{
    BTN_LEFT, CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer,
};
use smithay_client_toolkit::seat::relative_pointer::{
    RelativeMotionEvent, RelativePointerHandler, RelativePointerState,
};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::subcompositor::SubcompositorState;
use wayland_client::globals::{GlobalList, registry_queue_init};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_pointer::WlPointer;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1;
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_v1::WpAlphaModifierV1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1;
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use capture::{Action, Capture, DmabufInfo, Held, HeldFrame, Input, SLOTS, Teardown};
use chrome::{Chrome, Font};
use clients::{Change, Clients};
use config::Config;
use dmabuf::{Allocator, DmaBuffer, DmabufError, FormatModifier};
use geometry::{Offset, Point, Rect, Size, thumbnail_size};
use hypr::Monitor;
use input::{Cursor, Effect, GRIP_SIZE, Gestures, PointerInput};
use ipc::EventSocket;
use layout::{Entry, KeyChange, LayoutFile, SaveAction, SaveSchedule};
use overlay::{Overlay, alpha_factor};
use protocol::hyprland_toplevel_export_frame_v1::{self, Flags, HyprlandToplevelExportFrameV1};
use protocol::hyprland_toplevel_export_manager_v1::HyprlandToplevelExportManagerV1;
use report::{DamageMode, Line, Reporter, Source};

const RETRY_AFTER: Duration = Duration::from_millis(100);

enum Stop {
    Exit { code: u8, reason: String },
    StderrFailed,
}

#[derive(Debug)]
enum SetupError {
    Global { name: &'static str, reason: String },
    NoOutput(String),
    Wayland(String),
    Loop(String),
    DmabufGlobal(GlobalError),
    Allocator(DmabufError),
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupError::Global { name, reason } => write!(f, "{name}: {reason}"),
            SetupError::NoOutput(name) => write!(f, "no wl_output named {name}"),
            SetupError::Wayland(e) => write!(f, "{e}"),
            SetupError::Loop(e) => write!(f, "event loop: {e}"),
            SetupError::DmabufGlobal(e) => write!(f, "{e}"),
            SetupError::Allocator(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SetupError {}

struct Feedback {
    table: Vec<FormatModifier>,
    tranches: Vec<Vec<u16>>,
}

struct PendingImport {
    bo: gbm::BufferObject<()>,
    fourcc: u32,
    modifiers: Vec<u64>,
}

/// Outcome of a `created` or `failed` event for a params object.
#[derive(Debug, PartialEq, Eq)]
enum Resolved<T> {
    Current { owner: u64, slot: usize, import: T },
    Superseded(T),
}

struct Pending<K, T> {
    owner: u64,
    slot: usize,
    key: K,
    import: T,
    current: bool,
}

/// Pending imports of every client. A replaced import, and every import of a removed client, stays
/// listed until its own event arrives.
struct Imports<K, T> {
    entries: Vec<Pending<K, T>>,
}

impl<K: PartialEq, T> Imports<K, T> {
    fn new() -> Self {
        Imports {
            entries: Vec::new(),
        }
    }

    fn begin(&mut self, owner: u64, slot: usize, key: K, import: T) {
        for entry in &mut self.entries {
            if entry.owner == owner && entry.slot == slot {
                entry.current = false;
            }
        }
        self.entries.push(Pending {
            owner,
            slot,
            key,
            import,
            current: true,
        });
    }

    /// Makes every import of `owner` resolve as superseded.
    fn release_owner(&mut self, owner: u64) {
        for entry in &mut self.entries {
            if entry.owner == owner {
                entry.current = false;
            }
        }
    }

    /// Removes `key` from the list and returns its payload. `None` means the key is not listed.
    fn resolve(&mut self, key: &K) -> Option<Resolved<T>> {
        let at = self.entries.iter().position(|e| e.key == *key)?;
        let entry = self.entries.remove(at);
        Some(if entry.current {
            Resolved::Current {
                owner: entry.owner,
                slot: entry.slot,
                import: entry.import,
            }
        } else {
            Resolved::Superseded(entry.import)
        })
    }

    /// Empties the list and returns every entry, current and superseded.
    fn drain(&mut self) -> Vec<(K, T)> {
        self.entries.drain(..).map(|e| (e.key, e.import)).collect()
    }
}

struct Frame {
    proxy: HyprlandToplevelExportFrameV1,
    sync_id: u64,
    got_event: bool,
    dmabuf: Option<DmabufInfo>,
}

/// State of the short-lived feedback queue.
struct Probe {
    dmabuf_state: DmabufState,
    feedback: Option<(u64, Feedback)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Default,
    Saved,
    User,
}

/// The frame buffer last presented, which a surface that starts after it shows at its configure.
#[derive(Debug, Clone, Copy)]
struct Shown {
    slot: usize,
    y_invert: bool,
}

/// A layer surface of a dragged or moving thumbnail on a monitor other than the record's own.
struct Traveller {
    overlay: Overlay,
    monitor: usize,
    chromed: bool,
}

/// Which surfaces a record has. The home surface always exists on the record's monitor; the
/// travellers are on other monitors.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SurfaceState {
    Home,
    /// Travellers on these monitors, ascending.
    Straddling(Vec<usize>),
    /// The surface on `monitor` becomes the record's overlay at its first configure. The
    /// travellers on `others` go with the home surface.
    Landing {
        monitor: usize,
        others: Vec<usize>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SurfaceEvent {
    /// The monitors other than home that the dragged rectangle touches, ascending.
    Touch(Vec<usize>),
    Configured(usize),
    ReleaseHome,
    /// The record lands on `monitor`: a drag ended there, or a key change moves a record that
    /// has only its home surface.
    ReleaseAt {
        monitor: usize,
        configured: bool,
    },
    TearDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SurfaceAction {
    Create(usize),
    Destroy(usize),
    Present(usize),
    Commit(usize),
}

/// Where and how large new travellers are created.
struct Spawn {
    origin: (i64, i64),
    size: Size,
    factor: u32,
}

struct ClientState {
    capture: Option<Capture>,
    overlay: Option<Overlay>,
    travellers: Vec<Traveller>,
    surface_state: SurfaceState,
    monitor: usize,
    shown: Option<Shown>,
    buffers: [Option<DmaBuffer>; SLOTS],
    frame: Option<Frame>,
    timer: Option<RegistrationToken>,
    position: Point,
    width: u32,
    origin: Origin,
    buffer_size: Option<Size>,
    chromed: bool,
}

impl ClientState {
    fn entry(&self, output: &str) -> Entry {
        Entry {
            x: self.position.x,
            y: self.position.y,
            width: self.width,
            output: Some(output.to_string()),
        }
    }

    /// The home surface, then the travellers: each with its monitor and whether it has chrome.
    fn surfaces(&self) -> impl Iterator<Item = (&Overlay, usize, bool)> {
        let home = self
            .overlay
            .as_ref()
            .map(|o| (o, self.monitor, self.chromed));
        home.into_iter().chain(
            self.travellers
                .iter()
                .map(|t| (&t.overlay, t.monitor, t.chromed)),
        )
    }

    fn surfaces_mut(&mut self) -> impl Iterator<Item = (&mut Overlay, usize, &mut bool)> {
        let home = self
            .overlay
            .as_mut()
            .map(|o| (o, self.monitor, &mut self.chromed));
        home.into_iter().chain(
            self.travellers
                .iter_mut()
                .map(|t| (&mut t.overlay, t.monitor, &mut t.chromed)),
        )
    }

    /// The surface on `monitor`: the home surface or a traveller.
    fn surface_on(&mut self, monitor: usize) -> Option<&mut Overlay> {
        if monitor == self.monitor {
            self.overlay.as_mut()
        } else {
            self.travellers
                .iter_mut()
                .find(|t| t.monitor == monitor)
                .map(|t| &mut t.overlay)
        }
    }

    fn destroy_traveller(&mut self, monitor: usize) {
        if let Some(at) = self.travellers.iter().position(|t| t.monitor == monitor) {
            self.travellers.remove(at).overlay.destroy();
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PressStart {
    address: u64,
    position: Point,
    grab: (f64, f64),
    width: u32,
    /// The monitor last under the pointer during a drag; the home monitor at the press.
    monitor: usize,
    /// The dragged rectangle's top-left in layout coordinates, from the first drag event.
    at: Option<(i64, i64)>,
}

struct SeatPointer {
    seat: WlSeat,
    themed: ThemedPointer,
    relative: ZwpRelativePointerV1,
}

struct App {
    conn: Connection,
    qh: QueueHandle<App>,
    loop_handle: LoopHandle<'static, App>,
    registry_state: RegistryState,
    output_state: OutputState,
    compositor: CompositorState,
    subcompositor: SubcompositorState,
    shm: Shm,
    layer_shell: LayerShell,
    seat_state: SeatState,
    relative_pointer_state: RelativePointerState,
    dmabuf_state: DmabufState,
    viewporter: WpViewporter,
    export_manager: HyprlandToplevelExportManagerV1,
    alpha: WpAlphaModifierV1,
    feedback: Feedback,
    allocator: Allocator,
    pointers: Vec<SeatPointer>,
    gestures: Gestures,
    press: Option<PressStart>,
    hidden: bool,
    hovered: Option<u64>,
    clients: Clients,
    records: BTreeMap<u64, ClientState>,
    imports: Imports<ZwpLinuxBufferParamsV1, PendingImport>,
    orphans: Vec<Frame>,
    retries: BTreeMap<u64, RegistrationToken>,
    last_sync_id: u64,
    layout_path: PathBuf,
    layout: LayoutFile,
    schedule: SaveSchedule,
    save_timer: Option<RegistrationToken>,
    config: Config,
    font: Font,
    monitors: Vec<Monitor>,
    default_monitor: usize,
    mode: DamageMode,
    reporter: Reporter,
    requests: PathBuf,
    events: EventSocket,
    event_token: Option<RegistrationToken>,
    control: Option<control::Server>,
    control_loop: ControlLoop,
    tray: Option<tray::Tray>,
    tray_token: Option<RegistrationToken>,
    ping: Option<Ping>,
    ping_token: Option<RegistrationToken>,
    stop: Option<Stop>,
}

/// The loop registrations of the control socket. `App::control` holds the server itself, which
/// is `None` only after shutdown has removed it.
#[derive(Default)]
struct ControlLoop {
    listener: Option<RegistrationToken>,
    pause: Option<RegistrationToken>,
    connections: HashMap<control::ConnectionId, ConnectionTokens>,
}

struct ConnectionTokens {
    source: RegistrationToken,
    timer: Option<RegistrationToken>,
}

impl App {
    fn stop(&mut self, stop: Stop) {
        if self.stop.is_none() {
            self.stop = Some(stop);
        }
    }

    fn stop_with(&mut self, code: u8, reason: impl Into<String>) {
        self.stop(Stop::Exit {
            code,
            reason: reason.into(),
        });
    }

    fn emit(&mut self, line: &Line) {
        // A failed report write leaves no channel to report on; the exit code is the signal.
        if self.reporter.emit(line).is_err() {
            self.stop = Some(Stop::StderrFailed);
        }
    }

    fn feed(&mut self, address: u64, input: Input) {
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

    fn out_of_step(&mut self, action: &str) {
        self.stop(failure(out_of_step_reason(action)));
    }

    fn wl_output(&self, name: &str) -> Result<WlOutput, SetupError> {
        self.output_state
            .outputs()
            .find(|o| {
                self.output_state
                    .info(o)
                    .is_some_and(|i| i.name.as_deref() == Some(name))
            })
            .ok_or_else(|| SetupError::NoOutput(name.to_string()))
    }

    fn globals<'a>(&'a self, output: &'a WlOutput) -> overlay::Globals<'a> {
        overlay::Globals {
            compositor: &self.compositor,
            subcompositor: &self.subcompositor,
            layer_shell: &self.layer_shell,
            viewporter: &self.viewporter,
            shm: &self.shm,
            output,
            alpha: &self.alpha,
        }
    }

    fn usable(&self, monitor: usize) -> Size {
        self.monitors[monitor].usable_area().size()
    }

    /// A new layer surface on `monitor`. A failure stops the daemon and returns `None`.
    fn new_overlay(
        &mut self,
        monitor: usize,
        position: Offset,
        size: Size,
        factor: u32,
    ) -> Option<Overlay> {
        let output = match self.wl_output(&self.monitors[monitor].name) {
            Ok(output) => output,
            Err(e) => {
                self.stop(failure(e));
                return None;
            }
        };
        let globals = self.globals(&output);
        match Overlay::new(&self.qh, &globals, position, size, factor) {
            Ok(overlay) => Some(overlay),
            Err(e) => {
                self.stop(failure(e));
                None
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

    fn create_overlay(&mut self, address: u64, buffer_size: Size) {
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

    fn base_opacity(&self) -> u32 {
        base_opacity(self.layout.opacity, self.config.thumbnail.opacity)
    }

    fn toggles(&self) -> control::Toggles {
        control::Toggles {
            locked: self.layout.locked,
            hidden: self.hidden,
            snapping: self.layout.snapping,
            opacity: self.base_opacity(),
        }
    }

    fn effective_opacity(&self, address: u64) -> u32 {
        effective_opacity(self.base_opacity(), self.hovered == Some(address))
    }

    fn snap_distance(&self) -> u32 {
        snap_distance(self.layout.snapping, self.config.thumbnail.snap_distance)
    }

    fn allocate(&mut self, address: u64, slot: usize, fourcc: u32, size: Size) -> Result<(), Stop> {
        let Some(record) = self.records.get_mut(&address) else {
            return Ok(());
        };
        if let Some(old) = record.buffers[slot].take() {
            old.destroy();
        }
        let modifiers =
            dmabuf::modifiers_for(&self.feedback.table, &self.feedback.tranches, fourcc);
        let bo = self
            .allocator
            .allocate(size, fourcc, &modifiers)
            .map_err(failure)?;
        self.emit(&Line::Format {
            address,
            fourcc,
            modifier: u64::from(bo.modifier()),
            size,
            planes: bo.plane_count(),
        });
        if self.stop.is_some() {
            return Ok(());
        }
        let params = self.dmabuf_state.create_params(&self.qh).map_err(failure)?;
        let params = dmabuf::import(&bo, params).map_err(failure)?;
        self.imports.begin(
            address,
            slot,
            params,
            PendingImport {
                bo,
                fourcc,
                modifiers,
            },
        );
        Ok(())
    }

    fn present(&mut self, address: u64, slot: usize, buffer_size: Size, y_invert: bool) {
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
    fn draw_chrome(&mut self, address: u64, logical: Size, only: Option<usize>) -> bool {
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

    fn rerender_chrome(&mut self, address: u64) {
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

    fn arm_retry(&mut self, address: u64) {
        let timer = Timer::from_duration(RETRY_AFTER);
        match self
            .loop_handle
            .insert_source(timer, move |_, _, app: &mut App| {
                app.retries.remove(&address);
                app.lookup(address);
                TimeoutAction::Drop
            }) {
            Ok(token) => {
                self.retries.insert(address, token);
            }
            Err(e) => self.stop(failure(loop_error(e))),
        }
    }

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

    fn write_layout(&mut self) {
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

    fn request_save(&mut self) {
        match self.schedule.request(Instant::now()) {
            SaveAction::WriteNow => self.write_layout(),
            SaveAction::ArmAt(at) => self.arm_save(at),
            SaveAction::Joined => {}
        }
    }

    fn layout_update(&mut self, address: u64) {
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

    fn apply_changes(&mut self, changes: Vec<Change>) {
        for change in changes {
            if self.stop.is_some() {
                return;
            }
            match change {
                Change::Lookup { address } => self.lookup(address),
                Change::Retry { address } => self.arm_retry(address),
                Change::Added { address } => self.add_client(address),
                Change::Removed { address, reason } => self.remove_client(address, reason),
                Change::Account { address, error } => {
                    self.emit(&Line::Account { address, error });
                }
                Change::Skipped {
                    address,
                    pid,
                    error,
                } => self.emit(&Line::WindowSkipped {
                    address,
                    pid,
                    error,
                }),
                Change::Title {
                    address,
                    label_changed,
                    key_changed,
                } => self.title_changed(address, label_changed, key_changed),
                Change::Workspace {
                    address,
                    label_changed,
                } => self.workspace_changed(address, label_changed),
                Change::RingOwner { address, previous } => self.ring_changed(address, previous),
            }
        }
        self.cancel_stale_retries();
    }

    fn cancel_stale_retries(&mut self) {
        let clients = &self.clients;
        let stale = stale_retries(self.retries.keys().copied(), |a| clients.is_pending(a));
        for address in stale {
            if let Some(token) = self.retries.remove(&address) {
                self.loop_handle.remove(token);
            }
        }
    }

    fn lookup(&mut self, address: u64) {
        let entries = match query(&self.requests, "j/clients", hypr::parse_clients) {
            Ok(entries) => entries,
            Err(reason) => {
                self.stop_with(1, reason);
                return;
            }
        };
        let changes = match entries.iter().find(|c| c.address == address) {
            Some(entry) => self.clients.add(entry),
            None => self.clients.lookup_missed(address),
        };
        self.apply_changes(changes);
    }

    fn add_client(&mut self, address: u64) {
        let Some(tracked) = self.clients.get(address) else {
            return;
        };
        let account = tracked.key();
        let line = Line::ClientAdded {
            address,
            pid: tracked.pid,
            workspace: tracked.workspace.clone(),
            slot: tracked.slot(),
            account: account.as_ref().map(ToString::to_string),
            label: tracked.label(),
        };
        let saved = account.and_then(|k| self.layout.entries.get(&k.to_string()).cloned());
        let configured = &self.config.thumbnail;
        let (origin, monitor, width, position) = match saved {
            Some(entry) => {
                let monitor = monitor_for(
                    &self.monitors,
                    entry.output.as_deref(),
                    self.default_monitor,
                );
                (
                    Origin::Saved,
                    monitor,
                    layout::effective_width(entry.width, configured, self.usable(monitor)),
                    Point {
                        x: entry.x,
                        y: entry.y,
                    },
                )
            }
            None => (
                Origin::Default,
                self.default_monitor,
                layout::effective_width(
                    configured.width,
                    configured,
                    self.usable(self.default_monitor),
                ),
                Point { x: 0, y: 0 },
            ),
        };
        let handle = hypr::capture_handle(address);
        let capture = (!self.hidden).then(|| Capture::new(self.mode, address, handle));
        self.records.insert(
            address,
            ClientState {
                capture,
                overlay: None,
                travellers: Vec::new(),
                surface_state: SurfaceState::Home,
                monitor,
                shown: None,
                buffers: Default::default(),
                frame: None,
                timer: None,
                position,
                width,
                origin,
                buffer_size: None,
                chromed: false,
            },
        );
        self.emit(&line);
        self.feed(address, Input::Start);
        self.recompute_placement();
    }

    fn remove_client(&mut self, address: u64, reason: String) {
        if let Some(mut record) = self.records.remove(&address) {
            let effects = self.gestures.handle(PointerInput::Removed { address });
            self.run_effects(None, effects);
            if self.press.is_some_and(|p| p.address == address) {
                self.press = None;
            }
            if self.hovered == Some(address) {
                self.hovered = None;
            }
            self.tear_down(address, &mut record);
        }
        self.emit(&Line::ClientRemoved { address, reason });
        self.recompute_placement();
    }

    /// Releases everything the record holds, in the order `capture::teardown` plans.
    fn tear_down(&mut self, address: u64, record: &mut ClientState) {
        let held = Held {
            timer: record.timer.is_some(),
            frame: match &record.frame {
                None => HeldFrame::None,
                Some(frame) if frame.got_event => HeldFrame::Answered,
                Some(_) => HeldFrame::Unanswered,
            },
            overlay: record.overlay.is_some(),
            buffers: std::array::from_fn(|slot| record.buffers[slot].is_some()),
        };
        for step in capture::teardown(held) {
            match step {
                Teardown::CancelTimer => {
                    if let Some(token) = record.timer.take() {
                        self.loop_handle.remove(token);
                    }
                }
                Teardown::DestroyFrame => {
                    if let Some(frame) = record.frame.take() {
                        frame.proxy.destroy();
                    }
                }
                Teardown::OrphanFrame => {
                    if let Some(frame) = record.frame.take() {
                        self.orphans.push(frame);
                    }
                }
                Teardown::DestroyOverlay => {
                    let (next, actions) =
                        surface_step(&record.surface_state, SurfaceEvent::TearDown);
                    record.surface_state = next;
                    for action in actions {
                        if let SurfaceAction::Destroy(monitor) = action {
                            record.destroy_traveller(monitor);
                        }
                    }
                    if let Some(overlay) = record.overlay.take() {
                        overlay.destroy();
                    }
                }
                Teardown::DestroyBuffer { slot } => {
                    if let Some(buffer) = record.buffers[slot].take() {
                        buffer.destroy();
                    }
                }
                Teardown::ReleaseImports => self.imports.release_owner(address),
            }
        }
    }

    fn hide(&mut self) {
        self.end_pointer_interaction();
        let addresses: Vec<u64> = self.records.keys().copied().collect();
        for address in addresses {
            let Some(mut record) = self.records.remove(&address) else {
                continue;
            };
            self.tear_down(address, &mut record);
            record.capture = None;
            record.buffer_size = None;
            record.shown = None;
            record.chromed = false;
            self.records.insert(address, record);
        }
        self.hidden = true;
    }

    fn show(&mut self) {
        self.hidden = false;
        let addresses: Vec<u64> = self.records.keys().copied().collect();
        for address in addresses {
            let handle = hypr::capture_handle(address);
            if let Some(record) = self.records.get_mut(&address) {
                record.capture = Some(Capture::new(self.mode, address, handle));
            }
            self.feed(address, Input::Start);
        }
    }

    /// Applies a lock, snap, visibility or opacity command. Returns what `Tray::set_state`
    /// returned: `Some` only when the state changed and a tray is up. It never drains the tray.
    fn apply_command(
        &mut self,
        command: control::Command,
        source: Source,
    ) -> Option<Result<tray::Pass, String>> {
        let before = self.toggles();
        let after = control::apply(before, command, self.stop.is_some());
        if after == before {
            return None;
        }
        if after.locked != before.locked {
            self.layout.locked = after.locked;
            self.gestures.set_locked(after.locked);
            self.emit(&Line::Lock {
                locked: after.locked,
                source,
            });
            self.request_save();
        }
        if after.snapping != before.snapping {
            self.layout.snapping = after.snapping;
            self.emit(&Line::Snap {
                snapping: after.snapping,
                source,
            });
            self.request_save();
        }
        if after.hidden != before.hidden {
            if after.hidden {
                self.hide();
            } else {
                self.show();
            }
            self.emit(&Line::Visibility {
                hidden: after.hidden,
                source,
            });
        }
        if after.opacity != before.opacity {
            self.layout.opacity = Some(after.opacity);
            let addresses: Vec<u64> = self
                .records
                .iter()
                .filter(|(address, record)| {
                    record.overlay.is_some() && self.hovered != Some(**address)
                })
                .map(|(address, _)| *address)
                .collect();
            for address in addresses {
                let factor = alpha_factor(self.effective_opacity(address));
                self.set_alpha(address, factor);
                self.rerender_chrome(address);
            }
            self.emit(&Line::Opacity {
                percent: after.opacity,
                source,
            });
            self.request_save();
        }
        self.tray.as_mut().map(|tray| tray.set_state(after))
    }

    fn title_changed(&mut self, address: u64, label_changed: bool, key_changed: bool) {
        let Some(tracked) = self.clients.get(address) else {
            return;
        };
        let line = Line::Title {
            address,
            label: tracked.label(),
            account: tracked.key().map(|k| k.to_string()),
        };
        self.emit(&line);
        if key_changed {
            self.key_changed(address);
        }
        if label_changed {
            self.rerender_chrome(address);
        }
    }

    fn workspace_changed(&mut self, address: u64, label_changed: bool) {
        let Some(tracked) = self.clients.get(address) else {
            return;
        };
        let line = Line::Workspace {
            address,
            workspace: tracked.workspace.clone(),
            slot: tracked.slot(),
            label: tracked.label(),
        };
        self.emit(&line);
        if label_changed {
            self.rerender_chrome(address);
        }
        self.recompute_placement();
    }

    fn ring_changed(&mut self, address: Option<u64>, previous: Option<u64>) {
        if let Some(old) = previous {
            self.rerender_chrome(old);
        }
        if let Some(new) = address {
            self.rerender_chrome(new);
        }
        self.emit(&Line::Focus { address });
    }

    fn key_changed(&mut self, address: u64) {
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
    fn settle(&mut self, address: u64) {
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

    fn position_for(&self, address: u64, size: Size) -> Point {
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

    fn recompute_placement(&mut self) {
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
    fn pinned(&self, address: u64) -> bool {
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

    fn set_geometry(&mut self, address: u64, requested: i64, desired: Option<(i64, i64)>) {
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

    fn mark_user_placed(&mut self, address: u64) {
        if let Some(record) = self.records.get_mut(&address) {
            record.origin = Origin::User;
        }
    }

    /// The record that owns `surface` and the monitor the surface is on.
    fn surface_owner(&self, surface: &WlSurface) -> Option<(u64, usize)> {
        self.records.iter().find_map(|(address, r)| {
            r.surfaces()
                .find(|(o, _, _)| o.surface() == surface)
                .map(|(_, monitor, _)| (*address, monitor))
        })
    }

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

    fn run_effects(&mut self, pointer: Option<&WlPointer>, effects: Vec<Effect>) {
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

    /// Applies the surface state machine's answer to `event` and runs its actions. `spawn` is
    /// needed by the events that can create a surface.
    fn step_surfaces(&mut self, address: u64, event: SurfaceEvent, spawn: Option<Spawn>) {
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

    fn set_alpha(&mut self, address: u64, factor: u32) {
        if let Some(record) = self.records.get_mut(&address) {
            for (overlay, _, _) in record.surfaces_mut() {
                overlay.set_alpha(factor);
            }
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

    fn end_pointer_interaction(&mut self) {
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

    fn read_events(&mut self) -> PostAction {
        if self.stop.is_some() {
            return PostAction::Continue;
        }
        match self.events.read_lines() {
            Ok(lines) => {
                for line in lines {
                    if self.stop.is_some() {
                        break;
                    }
                    self.handle_line(line);
                }
                PostAction::Continue
            }
            Err(e) => {
                self.stop_with(1, format!("event socket: {e}"));
                PostAction::Disable
            }
        }
    }

    fn handle_line(&mut self, line: String) {
        match ipc::parse_event(&line) {
            Ok(Some(event)) => {
                let changes = self.clients.apply(&event);
                self.apply_changes(changes);
            }
            Ok(None) => {}
            Err(_) => self.emit(&Line::Ignored { line }),
        }
    }

    fn insert_event_source(&mut self) -> Result<(), SetupError> {
        let fd = self
            .events
            .as_fd()
            .try_clone_to_owned()
            .map_err(loop_error)?;
        let source = Generic::new(fd, Interest::READ, Mode::Level);
        let token = self
            .loop_handle
            .insert_source(source, |_, _, app: &mut App| Ok(app.read_events()))
            .map_err(loop_error)?;
        self.event_token = Some(token);
        Ok(())
    }

    fn control_error(&mut self, error: impl Into<String>) {
        self.emit(&Line::ControlError {
            error: error.into(),
        });
    }

    fn control_reply_error(&mut self, result: Result<(), control::ReplyError>) {
        if let Err(e) = result {
            self.control_error(format!("reply: {e}"));
        }
    }

    fn control_shutdown_error(&mut self, result: std::io::Result<()>) {
        if let Err(e) = result {
            self.control_error(format!("shutdown: {e}"));
        }
    }

    fn insert_control_source(&mut self) -> Result<(), SetupError> {
        let Some(server) = self.control.as_ref() else {
            return Ok(());
        };
        let fd = server.listener_fd().map_err(loop_error)?;
        let source = Generic::new(fd, Interest::READ, Mode::Level);
        let token = self
            .loop_handle
            .insert_source(source, |_, _, app: &mut App| Ok(app.accept_connections()))
            .map_err(loop_error)?;
        self.control_loop.listener = Some(token);
        Ok(())
    }

    fn announce_control(&mut self) {
        let Some(path) = self.control.as_ref().map(|s| s.path().to_path_buf()) else {
            return;
        };
        self.emit(&Line::ControlListening { path });
    }

    fn accept_connections(&mut self) -> PostAction {
        if self.stop.is_some() {
            return PostAction::Continue;
        }
        let Some(server) = self.control.as_mut() else {
            return PostAction::Continue;
        };
        let (admissions, error) = server.accept();
        for admission in admissions {
            match admission {
                control::Admission::Open(accepted) => self.watch_connection(accepted),
                control::Admission::Busy(result) => {
                    self.control_error(control::Refusal::Busy.to_string());
                    self.control_reply_error(result);
                }
            }
        }
        let Some(error) = error else {
            return PostAction::Continue;
        };
        self.control_error(format!("accept: {error}"));
        self.pause_accepting()
    }

    fn pause_accepting(&mut self) -> PostAction {
        let timer = Timer::from_duration(control::ACCEPT_PAUSE);
        match self
            .loop_handle
            .insert_source(timer, |_, _, app: &mut App| {
                app.control_loop.pause = None;
                app.resume_accepting();
                TimeoutAction::Drop
            }) {
            Ok(token) => {
                self.control_loop.pause = Some(token);
                PostAction::Disable
            }
            Err(e) => {
                self.stop(failure(loop_error(e)));
                PostAction::Continue
            }
        }
    }

    fn resume_accepting(&mut self) {
        let Some(token) = self.control_loop.listener else {
            return;
        };
        if let Err(e) = self.loop_handle.enable(&token) {
            self.stop(failure(loop_error(e)));
        }
    }

    fn watch_connection(&mut self, accepted: control::Accepted) {
        let control::Accepted { id, fd, deadline } = accepted;
        let source = Generic::new(fd, Interest::READ, Mode::Level);
        let source = match self
            .loop_handle
            .insert_source(source, move |_, _, app: &mut App| {
                Ok(app.connection_readable(id))
            }) {
            Ok(token) => token,
            Err(e) => {
                self.control_error(format!("accept: {e}"));
                self.close_connection(id);
                return;
            }
        };
        self.control_loop.connections.insert(
            id,
            ConnectionTokens {
                source,
                timer: None,
            },
        );
        let timer = Timer::from_deadline(deadline);
        match self
            .loop_handle
            .insert_source(timer, move |_, _, app: &mut App| {
                app.connection_deadline(id);
                TimeoutAction::Drop
            }) {
            Ok(token) => {
                if let Some(tokens) = self.control_loop.connections.get_mut(&id) {
                    tokens.timer = Some(token);
                }
            }
            Err(e) => {
                self.control_error(format!("accept: {e}"));
                self.forget_connection(id);
                self.close_connection(id);
            }
        }
    }

    fn close_connection(&mut self, id: control::ConnectionId) {
        let result = match self.control.as_mut() {
            Some(server) => server.close(id),
            None => Ok(()),
        };
        self.control_shutdown_error(result);
    }

    fn reply_connection(
        &mut self,
        id: control::ConnectionId,
        result: Result<(), control::Refusal>,
    ) {
        let result = match self.control.as_mut() {
            Some(server) => server.reply(id, result),
            None => Ok(()),
        };
        self.control_reply_error(result);
    }

    fn forget_connection(&mut self, id: control::ConnectionId) {
        if let Some(tokens) = self.control_loop.connections.remove(&id) {
            self.loop_handle.remove(tokens.source);
            if let Some(timer) = tokens.timer {
                self.loop_handle.remove(timer);
            }
        }
    }

    fn connection_readable(&mut self, id: control::ConnectionId) -> PostAction {
        let Some(server) = self.control.as_mut() else {
            return PostAction::Continue;
        };
        let stopping = self.stop.is_some();
        match server.read(id) {
            control::ReadOutcome::Pending => return PostAction::Continue,
            control::ReadOutcome::Line(_) if stopping => self.close_connection(id),
            control::ReadOutcome::Line(Ok(command)) => {
                let pass = self.apply_command(command, Source::Socket);
                self.reply_connection(id, Ok(()));
                if let Some(first) = pass {
                    self.drain_loop(first, false);
                }
            }
            control::ReadOutcome::Line(Err(refusal)) => {
                self.control_error(refusal.to_string());
                self.reply_connection(id, Err(refusal));
            }
            control::ReadOutcome::Closed(result) => self.control_shutdown_error(result),
            control::ReadOutcome::Failed { read, shutdown } => {
                self.control_error(format!("read: {read}"));
                self.control_shutdown_error(shutdown);
            }
        }
        self.forget_connection(id);
        PostAction::Continue
    }

    fn connection_deadline(&mut self, id: control::ConnectionId) {
        if self.stop.is_some() {
            return;
        }
        if let Some(tokens) = self.control_loop.connections.get_mut(&id) {
            tokens.timer = None;
        }
        let timed_out = self.control.as_mut().and_then(|server| server.timeout(id));
        if let Some(result) = timed_out {
            self.control_error(control::Refusal::Timeout.to_string());
            self.control_reply_error(result);
        }
        self.forget_connection(id);
    }

    fn start_tray(&mut self) {
        let toggles = self.toggles();
        match tray::Tray::start(std::process::id(), toggles, self.config.border.color) {
            Err(reason) => self.emit(&Line::TrayUnavailable { reason }),
            Ok((tray, registered)) => {
                let line = match registered {
                    Ok(()) => Line::TrayRegistered {
                        name: tray.name().to_string(),
                    },
                    Err(reason) => Line::TrayUnavailable { reason },
                };
                self.tray = Some(tray);
                self.emit(&line);
            }
        }
    }

    /// Inserts the tray source and the drain ping source. A failure ends the tray.
    fn watch_tray(&mut self) {
        let Some(source) = self.tray.as_ref().map(tray::Tray::source) else {
            return;
        };
        if let Err(e) = self.insert_tray_sources(source) {
            self.end_tray(format!("watch: {e}"), false);
        }
    }

    fn insert_tray_sources(
        &mut self,
        source: Result<tray::TraySource, String>,
    ) -> Result<(), String> {
        let source = Generic::new(source?, Interest::READ, Mode::Level);
        let token = self
            .loop_handle
            .insert_source(source, |_, _, app: &mut App| Ok(app.tray_readable()))
            .map_err(|e| e.to_string())?;
        self.tray_token = Some(token);
        let (ping, source) = make_ping().map_err(|e| e.to_string())?;
        let token = self
            .loop_handle
            .insert_source(source, |_, _, app: &mut App| app.drain_tray(false))
            .map_err(|e| e.to_string())?;
        self.ping_token = Some(token);
        self.ping = Some(ping);
        Ok(())
    }

    fn tray_readable(&mut self) -> PostAction {
        self.drain_tray(true);
        if self.tray.is_some() {
            PostAction::Continue
        } else {
            PostAction::Remove
        }
    }

    /// Runs the drain loop from a fresh `Tray::drain` pass. Does nothing without a tray.
    fn drain_tray(&mut self, in_tray_callback: bool) {
        if let Some(first) = self.tray.as_mut().map(tray::Tray::drain) {
            self.drain_loop(first, in_tray_callback);
        }
    }

    /// Applies the tray's passes in order, at most `MAX_DRAIN_PASSES` per call. An `Err` pass is
    /// the bus error.
    fn drain_loop(&mut self, first: Result<tray::Pass, String>, in_tray_callback: bool) {
        let mut next = first;
        for number in 1..=tray::MAX_DRAIN_PASSES {
            let tray::Pass { events, more } = match next {
                Ok(pass) => pass,
                Err(reason) => {
                    self.end_tray(reason, in_tray_callback);
                    return;
                }
            };
            let bound = number == tray::MAX_DRAIN_PASSES;
            let mut set_state = None;
            let mut command = false;
            for event in events {
                match event {
                    tray::TrayEvent::Registered => {
                        if let Some(name) = self.tray.as_ref().map(|t| t.name().to_string()) {
                            self.emit(&Line::TrayRegistered { name });
                        }
                    }
                    tray::TrayEvent::Unavailable(reason) => {
                        self.emit(&Line::TrayUnavailable { reason });
                    }
                    tray::TrayEvent::Command(menu) => {
                        command = true;
                        if bound {
                            if let Some(tray) = self.tray.as_mut() {
                                tray.defer(menu);
                            }
                        } else {
                            match menu {
                                tray::MenuCommand::Quit => {
                                    self.stop_with(0, "tray quit");
                                    return;
                                }
                                tray::MenuCommand::Toggle(toggle) => {
                                    set_state = self.apply_command(toggle, Source::Tray);
                                }
                            }
                        }
                    }
                }
            }
            if bound {
                if command || more {
                    self.emit(&Line::TrayUnavailable {
                        reason: format!("drain: {} passes", tray::MAX_DRAIN_PASSES),
                    });
                    if let Some(ping) = &self.ping {
                        ping.ping();
                    }
                }
                return;
            }
            next = match set_state {
                Some(pass) => pass,
                None if more => match self.tray.as_mut() {
                    Some(tray) => tray.drain(),
                    None => return,
                },
                None => return,
            };
        }
    }

    /// Reports the tray as unavailable and ends it: its sources leave the loop and an idle
    /// callback drops it, after every source callback of this dispatch has returned.
    fn end_tray(&mut self, reason: String, in_tray_callback: bool) {
        self.emit(&Line::TrayUnavailable { reason });
        if let Some(token) = self.tray_token.take()
            && !in_tray_callback
        {
            self.loop_handle.remove(token);
        }
        if let Some(token) = self.ping_token.take() {
            self.loop_handle.remove(token);
        }
        self.ping = None;
        if let Some(tray) = self.tray.take() {
            self.loop_handle.insert_idle(move |_: &mut App| drop(tray));
        }
    }

    /// Removes the control sources and the socket, then drops the tray and the ping, and tears
    /// down timers, the pending layout save and Wayland objects. Prints `exit` and exits.
    /// Callers keep the `Signals` source installed until exit, so a late signal stays pending.
    fn shutdown(mut self, stop: Stop) -> ! {
        let mut teardown = Vec::new();
        for token in [
            self.control_loop.listener.take(),
            self.control_loop.pause.take(),
        ]
        .into_iter()
        .flatten()
        {
            self.loop_handle.remove(token);
        }
        let ids: Vec<_> = self.control_loop.connections.keys().copied().collect();
        for id in ids {
            self.forget_connection(id);
        }
        if let Some(server) = self.control.take() {
            teardown.extend(remove_control(server));
        }
        for token in [self.tray_token.take(), self.ping_token.take()]
            .into_iter()
            .flatten()
        {
            self.loop_handle.remove(token);
        }
        self.ping = None;
        drop(self.tray.take());
        for record in self.records.values_mut() {
            if let Some(token) = record.timer.take() {
                self.loop_handle.remove(token);
            }
        }
        if let Some(token) = self.save_timer.take() {
            self.loop_handle.remove(token);
        }
        for (_, token) in std::mem::take(&mut self.retries) {
            self.loop_handle.remove(token);
        }
        if self.schedule.pending() {
            self.write_layout();
        }
        let mut objects = Vec::new();
        for record in std::mem::take(&mut self.records).into_values() {
            if let Some(frame) = record.frame
                && frame.got_event
            {
                frame.proxy.destroy();
            }
            for traveller in record.travellers {
                traveller.overlay.destroy();
            }
            if let Some(overlay) = record.overlay {
                overlay.destroy();
            }
            for buffer in record.buffers.into_iter().flatten() {
                buffer.wl_buffer.destroy();
                objects.push(buffer.bo);
            }
        }
        let mut pending = Vec::new();
        for (params, import) in self.imports.drain() {
            params.destroy();
            pending.push(import);
        }
        for pointer in std::mem::take(&mut self.pointers) {
            pointer.relative.destroy();
            drop(pointer.themed);
        }
        self.export_manager.destroy();
        self.alpha.destroy();
        teardown.extend(self.conn.flush().err().map(|e| format!("flush: {e}")));
        let error = (!teardown.is_empty()).then(|| teardown.join("; "));
        drop(objects);
        drop(pending);
        drop(self.allocator);
        if let Some(token) = self.event_token.take() {
            self.loop_handle.remove(token);
        }
        drop(self.events);
        let stop = if matches!(self.stop, Some(Stop::StderrFailed)) {
            Stop::StderrFailed
        } else {
            stop
        };
        finish(&mut self.reporter, stop, error)
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: u32) {}

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
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

impl DmabufHandler for App {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_feedback(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpLinuxDmabufFeedbackV1,
        _: DmabufFeedback,
    ) {
    }

    fn created(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        params: &ZwpLinuxBufferParamsV1,
        buffer: WlBuffer,
    ) {
        match self.imports.resolve(params) {
            Some(Resolved::Current {
                owner,
                slot,
                import,
            }) => {
                params.destroy();
                match self.records.get_mut(&owner) {
                    Some(record) => {
                        record.buffers[slot] = Some(DmaBuffer {
                            bo: import.bo,
                            wl_buffer: buffer,
                        });
                        self.feed(owner, Input::Imported { slot });
                    }
                    None => {
                        buffer.destroy();
                        drop(import);
                    }
                }
            }
            Some(Resolved::Superseded(import)) => {
                buffer.destroy();
                params.destroy();
                drop(import);
            }
            None => {
                buffer.destroy();
                params.destroy();
            }
        }
    }

    fn failed(&mut self, _: &Connection, _: &QueueHandle<Self>, params: &ZwpLinuxBufferParamsV1) {
        let resolved = self.imports.resolve(params);
        params.destroy();
        if let Some(Resolved::Current { import, .. }) = resolved {
            let error = DmabufError::ImportFailed {
                fourcc: import.fourcc,
                modifiers: import.modifiers,
            };
            self.stop(failure(error));
        }
    }

    fn released(&mut self, _: &Connection, _: &QueueHandle<Self>, buffer: &WlBuffer) {
        let owner = self.records.iter().find_map(|(address, record)| {
            record
                .buffers
                .iter()
                .position(|b| b.as_ref().is_some_and(|b| b.wl_buffer == *buffer))
                .map(|slot| (*address, slot))
        });
        if let Some((address, slot)) = owner {
            self.feed(address, Input::Released { slot });
        }
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers!(OutputState, SeatState);
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
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

impl DmabufHandler for Probe {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_feedback(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpLinuxDmabufFeedbackV1,
        feedback: DmabufFeedback,
    ) {
        if self.feedback.is_some() {
            return;
        }
        let table = feedback
            .format_table()
            .iter()
            .map(|f| FormatModifier {
                format: f.format,
                modifier: f.modifier,
            })
            .collect();
        let tranches = feedback
            .tranches()
            .iter()
            .map(|t| t.formats.clone())
            .collect();
        self.feedback = Some((feedback.main_device(), Feedback { table, tranches }));
    }

    fn created(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpLinuxBufferParamsV1,
        _: WlBuffer,
    ) {
    }

    fn failed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &ZwpLinuxBufferParamsV1) {}

    fn released(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlBuffer) {}
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
fn opacity_change(base: u32, hovered_before: bool, hovered_after: bool) -> Option<u32> {
    let before = effective_opacity(base, hovered_before);
    let after = effective_opacity(base, hovered_after);
    (before != after).then_some(after)
}

/// The pointer in layout coordinates: the origin of the usable area it started in, the press
/// position of the thumbnail, the pointer's offset inside it at the press, and the drag offset.
fn pointer_global(
    origin: (i32, i32),
    start: Point,
    grab: (f64, f64),
    offset: (f64, f64),
) -> (f64, f64) {
    (
        f64::from(origin.0) + f64::from(start.x) + grab.0 + offset.0,
        f64::from(origin.1) + f64::from(start.y) + grab.1 + offset.1,
    )
}

/// A rectangle in layout coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Area {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
}

impl Area {
    fn overlap(self, other: Area) -> i64 {
        let width = (self.x + self.width).min(other.x + other.width) - self.x.max(other.x);
        let height = (self.y + self.height).min(other.y + other.height) - self.y.max(other.y);
        if width > 0 && height > 0 {
            width * height
        } else {
            0
        }
    }
}

/// The monitor's usable area in layout coordinates.
fn usable_global(monitor: &Monitor) -> Area {
    let usable = monitor.usable_area();
    let origin = area_origin(monitor);
    Area {
        x: i64::from(origin.0),
        y: i64::from(origin.1),
        width: i64::from(usable.width),
        height: i64::from(usable.height),
    }
}

/// The top-left of a dragged thumbnail before snapping: the press position plus the rounded
/// offset, in layout coordinates.
fn desired_origin(origin: (i32, i32), start: Point, offset: (f64, f64)) -> (i64, i64) {
    (
        i64::from(origin.0) + i64::from(start.x) + offset.0.round() as i64,
        i64::from(origin.1) + i64::from(start.y) + offset.1.round() as i64,
    )
}

/// Whether the usable areas, which do not overlap each other, cover all of `rect`.
fn covered(rect: Area, areas: &[Area]) -> bool {
    let whole = rect.width * rect.height;
    whole > 0 && areas.iter().map(|a| rect.overlap(*a)).sum::<i64>() == whole
}

/// The top-left of the dragged rectangle in layout coordinates. It snaps in the usable area of
/// the monitor under the pointer, `under`, to its edges and the thumbnails `others` on it, and
/// rounds down to even there. The result is used as is when the usable areas of all monitors
/// cover it; otherwise the snapped position is clamped into the usable area of `under`.
fn dragged_origin(
    desired: (i64, i64),
    size: Size,
    under: usize,
    monitors: &[Monitor],
    others: &[Rect],
    distance: u32,
) -> (i64, i64) {
    let origin = area_origin(&monitors[under]);
    let (ox, oy) = (i64::from(origin.0), i64::from(origin.1));
    let usable = monitors[under].usable_area().size();
    let snapped = layout::snap(
        (desired.0 - ox, desired.1 - oy),
        size,
        others,
        usable,
        distance,
    );
    let even = (layout::even_down(snapped.0), layout::even_down(snapped.1));
    let rect = Area {
        x: even.0 + ox,
        y: even.1 + oy,
        width: i64::from(size.width),
        height: i64::from(size.height),
    };
    let areas: Vec<Area> = monitors.iter().map(usable_global).collect();
    if covered(rect, &areas) {
        return (rect.x, rect.y);
    }
    let clamped = layout::clamp_position(snapped.0, snapped.1, size, usable);
    (ox + i64::from(clamped.x), oy + i64::from(clamped.y))
}

/// The monitors other than `home` whose usable area the rectangle overlaps, ascending.
fn touched(monitors: &[Monitor], home: usize, rect: Area) -> Vec<usize> {
    monitors
        .iter()
        .enumerate()
        .filter(|(index, m)| *index != home && rect.overlap(usable_global(m)) > 0)
        .map(|(index, _)| index)
        .collect()
}

/// The width a request gets on a monitor with `usable`: negative requests count as zero.
fn capped_width(requested: i64, thumbnail: &config::Thumbnail, usable: Size) -> u32 {
    let requested = u32::try_from(requested.max(0)).unwrap_or(u32::MAX);
    layout::effective_width(requested, thumbnail, usable)
}

/// The width a request gets on a monitor with `usable`, the size that width gives for a frame
/// of `buffer`, and `at` (usable-relative) clamped.
fn fitted(
    width: i64,
    thumbnail: &config::Thumbnail,
    buffer: Size,
    usable: Size,
    at: (i64, i64),
) -> (u32, Size, Point) {
    let width = capped_width(width, thumbnail, usable);
    let size = thumbnail_size(width, buffer);
    (
        width,
        size,
        layout::clamp_position(at.0, at.1, size, usable),
    )
}

/// The monitor under `point` in layout coordinates. In a gap between monitors it is `last`.
fn under_pointer(monitors: &[Monitor], point: (f64, f64), last: usize) -> usize {
    hypr::monitor_at(monitors, point.0, point.1).unwrap_or(last)
}

/// Layout coordinates `at` relative to the top-left of the monitor's usable area.
fn usable_local(at: (i64, i64), monitor: &Monitor) -> (i64, i64) {
    let origin = area_origin(monitor);
    (
        at.0.saturating_sub(i64::from(origin.0)),
        at.1.saturating_sub(i64::from(origin.1)),
    )
}

/// The layout coordinates of `position`, a place relative to the monitor's usable area.
fn layout_point(position: Point, monitor: &Monitor) -> (i64, i64) {
    let origin = area_origin(monitor);
    (
        i64::from(origin.0) + i64::from(position.x),
        i64::from(origin.1) + i64::from(position.y),
    )
}

/// The margins of a surface on `monitor` that puts its top-left at layout coordinates `at`.
fn local_offset(at: (i64, i64), monitor: &Monitor) -> Offset {
    let (x, y) = usable_local(at, monitor);
    let fit = |value: i64| value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    Offset {
        x: fit(x),
        y: fit(y),
    }
}

/// The unsigned usable-relative position of a margin pair known to be clamped.
fn point_of(offset: Offset) -> Point {
    Point {
        x: u32::try_from(offset.x).unwrap_or(0),
        y: u32::try_from(offset.y).unwrap_or(0),
    }
}

/// The surfaces of one record after `event`, and the actions that take them there. Drag
/// events during a pending landing change nothing.
fn surface_step(state: &SurfaceState, event: SurfaceEvent) -> (SurfaceState, Vec<SurfaceAction>) {
    use SurfaceAction::{Commit, Create, Destroy, Present};
    let travellers: Vec<usize> = match state {
        SurfaceState::Home => Vec::new(),
        SurfaceState::Straddling(set) => set.clone(),
        SurfaceState::Landing { monitor, others } => std::iter::once(*monitor)
            .chain(others.iter().copied())
            .collect(),
    };
    let destroy_all = || travellers.iter().map(|m| Destroy(*m)).collect();
    let straddle = |set: Vec<usize>| {
        if set.is_empty() {
            SurfaceState::Home
        } else {
            SurfaceState::Straddling(set)
        }
    };
    match (state, event) {
        (SurfaceState::Landing { .. }, SurfaceEvent::Touch(_) | SurfaceEvent::ReleaseHome) => {
            (state.clone(), Vec::new())
        }
        (SurfaceState::Landing { .. }, SurfaceEvent::ReleaseAt { .. }) => {
            (state.clone(), Vec::new())
        }
        (_, SurfaceEvent::Touch(next)) => {
            let mut actions: Vec<SurfaceAction> = travellers
                .iter()
                .filter(|m| !next.contains(m))
                .map(|m| Destroy(*m))
                .collect();
            actions.extend(
                next.iter()
                    .filter(|m| !travellers.contains(m))
                    .map(|m| Create(*m)),
            );
            (straddle(next), actions)
        }
        (_, SurfaceEvent::ReleaseHome) => (SurfaceState::Home, destroy_all()),
        (
            _,
            SurfaceEvent::ReleaseAt {
                monitor,
                configured,
            },
        ) => {
            let exists = travellers.contains(&monitor);
            let others: Vec<usize> = travellers
                .iter()
                .copied()
                .filter(|m| *m != monitor)
                .collect();
            if exists && configured {
                let mut actions: Vec<SurfaceAction> = others.iter().map(|m| Destroy(*m)).collect();
                actions.push(Commit(monitor));
                (SurfaceState::Home, actions)
            } else {
                let actions = if exists {
                    Vec::new()
                } else {
                    vec![Create(monitor)]
                };
                (SurfaceState::Landing { monitor, others }, actions)
            }
        }
        (SurfaceState::Landing { monitor, others }, SurfaceEvent::Configured(at)) => {
            if at == *monitor {
                let mut actions = vec![Present(at)];
                actions.extend(others.iter().map(|m| Destroy(*m)));
                actions.push(Commit(at));
                (SurfaceState::Home, actions)
            } else if others.contains(&at) {
                (state.clone(), vec![Present(at)])
            } else {
                (state.clone(), Vec::new())
            }
        }
        (_, SurfaceEvent::Configured(at)) => {
            let actions = if travellers.contains(&at) {
                vec![Present(at)]
            } else {
                Vec::new()
            };
            (state.clone(), actions)
        }
        (_, SurfaceEvent::TearDown) => (SurfaceState::Home, destroy_all()),
    }
}

/// What `settle` does after `relocation` chose `target`.
#[derive(Debug, PartialEq, Eq)]
enum Settle {
    Relocate {
        target: Option<usize>,
        width: i64,
        at: Option<(i64, i64)>,
    },
    Save,
    Stay,
}

/// A saved record with an entry always applies the entry's geometry, so a second key change
/// inside a move's window leaves the record where the layout holds it. `width` is the record's.
fn settled(
    target: Option<usize>,
    origin: Origin,
    entry: Option<&layout::Entry>,
    width: u32,
) -> Settle {
    match (entry, target) {
        (Some(entry), _) => Settle::Relocate {
            target,
            width: i64::from(entry.width),
            at: Some((i64::from(entry.x), i64::from(entry.y))),
        },
        (None, Some(_)) => Settle::Relocate {
            target,
            width: i64::from(width),
            at: None,
        },
        (None, None) if origin == Origin::User => Settle::Save,
        (None, None) => Settle::Stay,
    }
}

/// The monitor whose usable area bounds a record's geometry: the landing monitor while a
/// landing is pending, else the record's own.
fn bound_monitor(state: &SurfaceState, monitor: usize) -> usize {
    match state {
        SurfaceState::Landing { monitor, .. } => *monitor,
        SurfaceState::Home | SurfaceState::Straddling(_) => monitor,
    }
}

/// What asks where a record lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger<'a> {
    /// `output` is the monitor the new key's entry names; `busy` is a drag or a pending landing.
    KeyChange { output: Option<&'a str>, busy: bool },
    /// A drop or a commit left the record on its landing monitor. `entry` is the current key's
    /// saved entry, `None` without one, and the monitor it names.
    Settled { entry: Option<Option<&'a str>> },
}

/// The monitor a record moves to, or `None` to stay on `monitor`. A key change goes to its
/// entry's monitor or `default`, never while busy. After a settle a user-placed record stays,
/// a default-placed one goes to `default`, a saved one to its entry's monitor or `default`, or
/// stays without an entry.
fn relocation(
    trigger: Trigger,
    monitors: &[Monitor],
    origin: Origin,
    monitor: usize,
    default: usize,
) -> Option<usize> {
    let target = match trigger {
        Trigger::KeyChange { busy: true, .. } => return None,
        Trigger::KeyChange { output, .. } => monitor_for(monitors, output, default),
        Trigger::Settled { .. } if origin == Origin::Default => default,
        Trigger::Settled {
            entry: Some(output),
        } if origin == Origin::Saved => monitor_for(monitors, output, default),
        Trigger::Settled { .. } => return None,
    };
    (target != monitor).then_some(target)
}

/// Whether a client sits at its default-row slot. The client of an active gesture keeps the
/// geometry the gesture gave it: nothing re-places it until the gesture ends.
fn follows_row(origin: Origin, active: Option<u64>, address: u64) -> bool {
    origin == Origin::Default && active != Some(address)
}

/// The exit reason of a failed dispatch: the Wayland error's own text when the connection has one.
fn dispatch_reason(wayland: Option<String>, dispatch: String) -> String {
    wayland.unwrap_or_else(|| SetupError::Loop(dispatch).to_string())
}

/// The default-row slots to apply. The client of an active gesture keeps its slot in the row but
/// is not moved. Its re-placement is deferred until the gesture ends.
fn placements(order: &[u64], gesture: Option<u64>) -> Vec<(usize, u64)> {
    order
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, address)| Some(*address) != gesture)
        .collect()
}

/// Whether the gesture that was active before a pointer event is over after it, which frees a
/// client the gesture kept out of the default row, so it is re-placed.
fn gesture_ended(before: Option<u64>, after: Option<u64>) -> bool {
    before.is_some() && after.is_none()
}

fn stale_retries(armed: impl Iterator<Item = u64>, is_pending: impl Fn(u64) -> bool) -> Vec<u64> {
    armed.filter(|a| !is_pending(*a)).collect()
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

fn in_grip(position: (f64, f64), size: Size) -> bool {
    let grip = f64::from(GRIP_SIZE);
    position.0 >= f64::from(size.width) - grip && position.1 >= f64::from(size.height) - grip
}

smithay_client_toolkit::delegate_registry!(App);
smithay_client_toolkit::delegate_dispatch2!(App);
smithay_client_toolkit::delegate_dispatch2!(Probe);

fn failure(e: impl std::fmt::Display) -> Stop {
    Stop::Exit {
        code: 1,
        reason: e.to_string(),
    }
}

fn wayland(e: impl std::fmt::Display) -> SetupError {
    SetupError::Wayland(e.to_string())
}

fn loop_error(e: impl std::fmt::Display) -> SetupError {
    SetupError::Loop(e.to_string())
}

fn out_of_step_reason(action: &str) -> String {
    format!("{action}: no frame, overlay or buffer for the slot")
}

/// Prints the `exit` line and exits.
fn finish(reporter: &mut Reporter, stop: Stop, teardown: Option<String>) -> ! {
    let code = match stop {
        Stop::StderrFailed => 1,
        Stop::Exit { code, reason } => {
            let (code, line) = report::exit_line(code, reason, teardown);
            match reporter.emit(&line) {
                Ok(()) => code,
                Err(_) => 1,
            }
        }
    };
    std::process::exit(i32::from(code))
}

/// Exits with `code` after the `exit` line. `teardown` is the text of a failed cleanup.
fn fail(
    reporter: &mut Reporter,
    code: u8,
    reason: impl std::fmt::Display,
    teardown: Option<String>,
) -> ! {
    finish(
        reporter,
        Stop::Exit {
            code,
            reason: reason.to_string(),
        },
        teardown,
    )
}

/// Removes the control socket file and the connections. The error is a `teardown` text for
/// `finish`.
fn remove_control(server: control::Server) -> Option<String> {
    let path = server.path().to_path_buf();
    server
        .remove()
        .err()
        .map(|e| format!("control socket {}: remove: {e}", path.display()))
}

fn emit_or_finish(reporter: &mut Reporter, line: &Line) {
    if reporter.emit(line).is_err() {
        finish(reporter, Stop::StderrFailed, None);
    }
}

/// Sends a Hyprland request and parses the reply. The error is the `exit` reason.
fn query<T>(
    path: &Path,
    request: &str,
    parse: impl FnOnce(&str) -> Result<T, hypr::HyprError>,
) -> Result<T, String> {
    let reply = ipc::request(path, request).map_err(|e| format!("request {request}: {e}"))?;
    parse(&reply).map_err(|e| format!("request {request}: {e}"))
}

/// Reads the default dmabuf feedback on a queue of its own and opens the GBM device. The
/// probe-bound `zwp_linux_dmabuf_v1` stays until the connection closes and gets no events at v4+.
fn probe_feedback(
    conn: &Connection,
    globals: &GlobalList,
) -> Result<(Allocator, Feedback), SetupError> {
    let mut queue = conn.new_event_queue::<Probe>();
    let qh = queue.handle();
    let mut probe = Probe {
        dmabuf_state: DmabufState::new(globals, &qh),
        feedback: None,
    };
    let feedback_proxy = probe
        .dmabuf_state
        .get_default_feedback(&qh)
        .map_err(SetupError::DmabufGlobal)?;
    let (main_device, feedback) = loop {
        if let Some(found) = probe.feedback.take() {
            break found;
        }
        queue.blocking_dispatch(&mut probe).map_err(wayland)?;
    };
    feedback_proxy.destroy();
    let allocator = Allocator::open(main_device).map_err(SetupError::Allocator)?;
    Ok((allocator, feedback))
}

struct Parts {
    conn: Connection,
    queue: EventQueue<App>,
    globals: GlobalList,
    allocator: Allocator,
    feedback: Feedback,
    compositor: CompositorState,
    subcompositor: SubcompositorState,
    shm: Shm,
    layer_shell: LayerShell,
    viewporter: WpViewporter,
    export_manager: HyprlandToplevelExportManagerV1,
    alpha: WpAlphaModifierV1,
    seat_state: SeatState,
    relative_pointer_state: RelativePointerState,
}

fn bind_wayland() -> Result<Parts, SetupError> {
    let conn = Connection::connect_to_env().map_err(wayland)?;
    let (globals, queue) = registry_queue_init::<App>(&conn).map_err(wayland)?;
    let qh = queue.handle();
    let (allocator, feedback) = probe_feedback(&conn, &globals)?;
    let global = |name: &'static str| {
        move |e: wayland_client::globals::BindError| SetupError::Global {
            name,
            reason: e.to_string(),
        }
    };
    let compositor = CompositorState::bind(&globals, &qh).map_err(global("wl_compositor"))?;
    let subcompositor = SubcompositorState::bind(compositor.wl_compositor().clone(), &globals, &qh)
        .map_err(global("wl_subcompositor"))?;
    let shm = Shm::bind(&globals, &qh).map_err(global("wl_shm"))?;
    let layer_shell = LayerShell::bind(&globals, &qh).map_err(global("zwlr_layer_shell_v1"))?;
    let viewporter = globals
        .bind::<WpViewporter, _, _>(&qh, 1..=1, ())
        .map_err(global("wp_viewporter"))?;
    let export_manager = globals
        .bind::<HyprlandToplevelExportManagerV1, _, _>(&qh, 1..=2, ())
        .map_err(global("hyprland_toplevel_export_manager_v1"))?;
    let alpha = globals
        .bind::<WpAlphaModifierV1, _, _>(&qh, 1..=1, ())
        .map_err(global("wp_alpha_modifier_v1"))?;
    let advertised = globals.contents().with_list(|list| {
        list.iter()
            .any(|g| g.interface == "zwp_relative_pointer_manager_v1")
    });
    if !advertised {
        return Err(SetupError::Global {
            name: "zwp_relative_pointer_manager_v1",
            reason: "not advertised by the compositor".to_string(),
        });
    }
    let relative_pointer_state = RelativePointerState::bind(&globals, &qh);
    let seat_state = SeatState::new(&globals, &qh);
    Ok(Parts {
        conn,
        queue,
        globals,
        allocator,
        feedback,
        compositor,
        subcompositor,
        shm,
        layer_shell,
        viewporter,
        export_manager,
        alpha,
        seat_state,
        relative_pointer_state,
    })
}

struct Startup {
    reporter: Reporter,
    config: Config,
    font: Font,
    layout_path: PathBuf,
    layout: LayoutFile,
    requests: PathBuf,
    events: EventSocket,
    control: control::Server,
    clients: Clients,
    monitors: Vec<Monitor>,
    default_monitor: usize,
    mode: DamageMode,
}

/// Binds the Wayland globals and output and opens the GBM device. A failure prints the `exit` line
/// and exits.
fn setup(startup: Startup, loop_handle: LoopHandle<'static, App>) -> App {
    let Startup {
        mut reporter,
        config,
        font,
        layout_path,
        layout,
        requests,
        events,
        control,
        clients,
        monitors,
        default_monitor,
        mode,
    } = startup;
    let parts = match bind_wayland() {
        Ok(parts) => parts,
        Err(e) => fail(&mut reporter, 1, e, remove_control(control)),
    };
    let Parts {
        conn,
        mut queue,
        globals,
        allocator,
        feedback,
        compositor,
        subcompositor,
        shm,
        layer_shell,
        viewporter,
        export_manager,
        alpha,
        seat_state,
        relative_pointer_state,
    } = parts;
    let qh = queue.handle();
    let mut gestures = Gestures::default();
    gestures.set_locked(layout.locked);
    let mut app = App {
        conn: conn.clone(),
        qh: qh.clone(),
        loop_handle: loop_handle.clone(),
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        subcompositor,
        shm,
        layer_shell,
        seat_state,
        relative_pointer_state,
        dmabuf_state: DmabufState::new(&globals, &qh),
        viewporter,
        export_manager,
        alpha,
        feedback,
        allocator,
        pointers: Vec::new(),
        gestures,
        press: None,
        hidden: false,
        hovered: None,
        clients,
        records: BTreeMap::new(),
        imports: Imports::new(),
        orphans: Vec::new(),
        retries: BTreeMap::new(),
        last_sync_id: 0,
        layout_path,
        layout,
        schedule: SaveSchedule::default(),
        save_timer: None,
        config,
        font,
        monitors,
        default_monitor,
        mode,
        reporter,
        requests,
        events,
        event_token: None,
        control: Some(control),
        control_loop: ControlLoop::default(),
        tray: None,
        tray_token: None,
        ping: None,
        ping_token: None,
        stop: None,
    };
    // The wl_output binds from OutputState::new are answered up to `done` before the sync returns.
    let settled = queue
        .roundtrip(&mut app)
        .map_err(wayland)
        .and_then(|_| {
            app.wl_output(&app.monitors[app.default_monitor].name)
                .map(|_| ())
        })
        .and_then(|()| {
            WaylandSource::new(conn, queue)
                .insert(loop_handle)
                .map(|_| ())
                .map_err(loop_error)
        });
    if let Err(e) = settled {
        app.shutdown(failure(e));
    }
    app
}

struct HyprState {
    monitors: Vec<Monitor>,
    default_monitor: usize,
    snapshot: Vec<hypr::Client>,
    active: Option<u64>,
}

/// The index of the monitor new thumbnails start on: the one named `output`, or the focused one
/// when `output` is unset. The error is the `exit` reason.
fn default_monitor(monitors: &[Monitor], output: Option<&str>) -> Result<usize, String> {
    match output {
        Some(name) => monitors
            .iter()
            .position(|m| m.name == name)
            .ok_or_else(|| format!("no monitor named {name} in j/monitors")),
        None => monitors
            .iter()
            .position(|m| m.focused)
            .ok_or_else(|| "no focused monitor in j/monitors".to_string()),
    }
}

/// The monitor a saved entry names, or `default` when it names none or one that is absent.
fn monitor_for(monitors: &[Monitor], output: Option<&str>, default: usize) -> usize {
    output
        .and_then(|name| monitors.iter().position(|m| m.name == name))
        .unwrap_or(default)
}

/// The top-left of the monitor's usable area in layout coordinates.
fn area_origin(monitor: &Monitor) -> (i32, i32) {
    let usable = monitor.usable_area();
    (
        monitor.x.saturating_add_unsigned(usable.x),
        monitor.y.saturating_add_unsigned(usable.y),
    )
}

/// Reads the monitors, the client list and the active window. The error is the `exit` reason.
fn read_hyprland(requests: &Path, output: Option<&str>) -> Result<HyprState, String> {
    let monitors = query(requests, "j/monitors", hypr::parse_monitors)?;
    let default_monitor = default_monitor(&monitors, output)?;
    let snapshot = query(requests, "j/clients", hypr::parse_clients)?;
    let active = query(requests, "j/activewindow", hypr::parse_active_window)?;
    Ok(HyprState {
        monitors,
        default_monitor,
        snapshot,
        active,
    })
}

/// Resolves the Hyprland sockets from the environment or exits with code 1.
fn sockets_from_env(reporter: &mut Reporter) -> ipc::Sockets {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
    let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE");
    match ipc::sockets(runtime_dir.as_deref(), signature.as_deref()) {
        Ok(sockets) => sockets,
        Err(e) => fail(reporter, 1, e, None),
    }
}

/// Runs startup from the config to the `start` line. A failure prints the `exit` line and exits,
/// or exits 1 when stderr fails.
fn prepare(args: &cli::Args, mut reporter: Reporter) -> (Startup, Vec<hypr::Client>) {
    let home = std::env::var_os("HOME");
    let config_path = match &args.config {
        Some(path) => path.clone(),
        None => {
            let config_home = std::env::var_os("XDG_CONFIG_HOME");
            match config::default_path(config_home.as_deref(), home.as_deref()) {
                Some(path) => path,
                None => fail(
                    &mut reporter,
                    1,
                    "no config path: XDG_CONFIG_HOME and HOME are unset or unusable",
                    None,
                ),
            }
        }
    };
    let (config, config_read) = match config::load(args.config.as_deref(), &config_path) {
        Ok(loaded) => loaded,
        Err(e) => fail(
            &mut reporter,
            2,
            format!("config {}: {e}", config_path.display()),
            None,
        ),
    };
    let (font, font_path) = match &config.label.font_file {
        Some(path) => match Font::load(path) {
            Ok(font) => (font, path.clone()),
            Err(e) => fail(
                &mut reporter,
                2,
                format!("label.font_file {}: {e}", path.display()),
                None,
            ),
        },
        None => {
            let family = &config.label.font;
            let loaded = chrome::resolve_font(family)
                .and_then(|path| Font::load(&path).map(|font| (font, path)));
            match loaded {
                Ok(loaded) => loaded,
                Err(e) => fail(&mut reporter, 1, format!("fc-match {family}: {e}"), None),
            }
        }
    };
    let state_home = std::env::var_os("XDG_STATE_HOME");
    let Some(layout_path) = layout::default_path(state_home.as_deref(), home.as_deref()) else {
        fail(
            &mut reporter,
            1,
            "no layout path: XDG_STATE_HOME and HOME are unset or unusable",
            None,
        )
    };
    let layout = match layout::load(&layout_path) {
        Ok(file) => file,
        Err(e) => match layout::set_aside(&layout_path) {
            Ok(renamed) => {
                let line = Line::LayoutError {
                    path: layout_path.clone(),
                    error: e.to_string(),
                    renamed: Some(renamed),
                };
                emit_or_finish(&mut reporter, &line);
                LayoutFile::default()
            }
            Err(rename) => fail(
                &mut reporter,
                1,
                format!("layout {}: {e}; rename: {rename}", layout_path.display()),
                None,
            ),
        },
    };
    let sockets = sockets_from_env(&mut reporter);
    let events = match EventSocket::connect(&sockets.events) {
        Ok(events) => events,
        Err(e) => fail(&mut reporter, 1, format!("event socket: {e}"), None),
    };
    let control = match control::Server::bind(&sockets.control) {
        Ok(server) => server,
        Err(e) => fail(&mut reporter, 1, e, None),
    };
    let HyprState {
        monitors,
        default_monitor,
        snapshot,
        active,
    } = match read_hyprland(&sockets.requests, config.output.as_deref()) {
        Ok(state) => state,
        Err(reason) => fail(&mut reporter, 1, reason, remove_control(control)),
    };
    let mode = if args.ignore_damage {
        DamageMode::IgnoreDamage
    } else {
        DamageMode::Recommit
    };
    let start_monitor = &monitors[default_monitor];
    let start = Line::Start {
        output: start_monitor.name.clone(),
        scale: start_monitor.scale,
        usable: start_monitor.usable_area(),
        mode,
        config: config_read,
        layout: layout_path.clone(),
        locked: layout.locked,
        font: font_path,
    };
    if reporter.emit(&start).is_err() {
        finish(&mut reporter, Stop::StderrFailed, remove_control(control));
    }
    let startup = Startup {
        reporter,
        config,
        font,
        layout_path,
        layout,
        requests: sockets.requests,
        events,
        control,
        clients: Clients::new(active, Box::new(clients::read_cmdline)),
        monitors,
        default_monitor,
        mode,
    };
    (startup, snapshot)
}

/// Sends a command to the running daemon and exits. It opens no log, config or layout file.
fn run_command(command: control::Command) -> ! {
    let mut reporter = Reporter::stderr(false);
    let sockets = sockets_from_env(&mut reporter);
    match control::send(&sockets.control, command) {
        Ok(()) => std::process::exit(0),
        Err(e) => fail(&mut reporter, 1, e, None),
    }
}

fn main() -> ! {
    let start = Instant::now();
    let args = match cli::parse_os(std::env::args_os().skip(1)) {
        Ok(cli::Invocation::Daemon(args)) => args,
        Ok(cli::Invocation::Command(command)) => run_command(command),
        Err(e) => {
            let message = e.to_string();
            let mut reporter = Reporter::stderr(false);
            emit_or_finish(
                &mut reporter,
                &Line::Usage {
                    message: message.clone(),
                },
            );
            fail(&mut reporter, 2, message, None)
        }
    };
    let reporter = match args.log.as_deref() {
        None => Reporter::stderr(args.verbose),
        Some(path) => match Reporter::open(path, args.verbose) {
            Ok(reporter) => reporter,
            Err(e) => {
                let mut reporter = Reporter::stderr(args.verbose);
                fail(
                    &mut reporter,
                    1,
                    format!("log {}: {e}", path.display()),
                    None,
                )
            }
        },
    };
    let (mut startup, snapshot) = prepare(&args, reporter);

    let mut event_loop = match EventLoop::<App>::try_new() {
        Ok(l) => l,
        Err(e) => fail(
            &mut startup.reporter,
            1,
            loop_error(e),
            remove_control(startup.control),
        ),
    };
    let mut app = setup(startup, event_loop.handle());
    let signals = match Signals::new(&[Signal::SIGINT, Signal::SIGTERM]) {
        Ok(s) => s,
        Err(e) => app.shutdown(failure(loop_error(e))),
    };
    let inserted = event_loop
        .handle()
        .insert_source(signals, |event, _, app: &mut App| {
            let reason = match event.signal() {
                Signal::SIGINT => "signal SIGINT",
                _ => "signal SIGTERM",
            };
            app.stop_with(0, reason);
        });
    if let Err(e) = inserted {
        app.shutdown(failure(loop_error(&e)));
    }
    app.start_tray();
    for entry in &snapshot {
        if app.stop.is_some() {
            break;
        }
        let changes = app.clients.add(entry);
        app.apply_changes(changes);
    }
    if let Err(e) = app.insert_event_source() {
        app.shutdown(failure(e));
    }
    if let Err(e) = app.insert_control_source() {
        app.shutdown(failure(e));
    }
    app.watch_tray();
    app.drain_tray(false);
    app.announce_control();

    let deadline = args.seconds.map(|s| start + Duration::from_secs(s));
    let stop = loop {
        if let Some(stop) = app.stop.take() {
            break stop;
        }
        let timeout = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        if let Err(e) = event_loop.dispatch(timeout, &mut app) {
            let wayland = app.conn.backend().last_error().map(|w| w.to_string());
            app.stop(failure(dispatch_reason(wayland, e.to_string())));
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            app.stop_with(0, "seconds elapsed");
        }
    };
    app.shutdown(stop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placements_cases() {
        type Case<'a> = (&'a str, &'a [u64], Option<u64>, Vec<(usize, u64)>);
        let cases: [Case; 4] = [
            ("empty", &[], None, vec![]),
            (
                "gesture on a client outside the row",
                &[7, 9],
                Some(8),
                vec![(0, 7), (1, 9)],
            ),
            ("no gesture", &[7, 8, 9], None, vec![(0, 7), (1, 8), (2, 9)]),
            (
                "gesture on the middle",
                &[7, 8, 9],
                Some(8),
                vec![(0, 7), (2, 9)],
            ),
        ];
        for (name, order, gesture, want) in cases {
            assert_eq!(placements(order, gesture), want, "{name}");
        }
    }

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
    fn follows_row_cases() {
        let placed = Origin::User;
        let cases = [
            ("default, idle", Origin::Default, None, 7, true),
            ("default, other gesture", Origin::Default, Some(8), 7, true),
            ("default, own gesture", Origin::Default, Some(7), 7, false),
            ("saved, idle", Origin::Saved, None, 7, false),
            ("placed, idle", placed, None, 7, false),
            ("placed, own gesture", placed, Some(7), 7, false),
        ];
        for (name, origin, active, address, want) in cases {
            assert_eq!(follows_row(origin, active, address), want, "{name}");
        }
    }

    #[test]
    fn dispatch_reason_cases() {
        let cases = [
            (
                "wayland error",
                Some("broken pipe"),
                "poll failed",
                "broken pipe",
            ),
            (
                "no wayland error",
                None,
                "poll failed",
                "event loop: poll failed",
            ),
        ];
        for (name, wayland, dispatch, want) in cases {
            let got = dispatch_reason(wayland.map(String::from), dispatch.to_string());
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn stale_retries_cases() {
        type Case<'a> = (&'a str, &'a [u64], &'a [u64], Vec<u64>);
        let cases: [Case; 4] = [
            ("nothing armed", &[], &[7], vec![]),
            ("armed and pending", &[7, 8], &[7, 8], vec![]),
            ("pending set empty", &[7, 8], &[], vec![7, 8]),
            ("one still pending", &[7, 8], &[8], vec![7]),
        ];
        for (name, armed, pending, want) in cases {
            let got = stale_retries(armed.iter().copied(), |a| pending.contains(&a));
            assert_eq!(got, want, "{name}");
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

    #[derive(Debug)]
    enum Op {
        Begin(u64, usize, u32, &'static str),
        Release(u64),
        Event(u32, Option<Resolved<&'static str>>),
    }

    #[test]
    fn pending_import_cases() {
        use Op::{Begin, Event, Release};
        use Resolved::{Current, Superseded};
        let cur = |owner, slot, import| {
            Some(Current {
                owner,
                slot,
                import,
            })
        };
        let cases = [
            (
                "one pending, its own event",
                vec![Begin(1, 0, 1, "a"), Event(1, cur(1, 0, "a"))],
                vec![],
            ),
            (
                "replaced once, old then new",
                vec![
                    Begin(1, 0, 1, "a"),
                    Begin(1, 0, 2, "b"),
                    Event(1, Some(Superseded("a"))),
                    Event(2, cur(1, 0, "b")),
                ],
                vec![],
            ),
            (
                "replaced once, new then old",
                vec![
                    Begin(1, 0, 1, "a"),
                    Begin(1, 0, 2, "b"),
                    Event(2, cur(1, 0, "b")),
                    Event(1, Some(Superseded("a"))),
                ],
                vec![],
            ),
            (
                "replaced twice, each old event",
                vec![
                    Begin(1, 0, 1, "a"),
                    Begin(1, 0, 2, "b"),
                    Begin(1, 0, 3, "c"),
                    Event(1, Some(Superseded("a"))),
                    Event(2, Some(Superseded("b"))),
                ],
                vec![(3, "c")],
            ),
            (
                "replaced twice, middle event only",
                vec![
                    Begin(1, 0, 1, "a"),
                    Begin(1, 0, 2, "b"),
                    Begin(1, 0, 3, "c"),
                    Event(2, Some(Superseded("b"))),
                ],
                vec![(1, "a"), (3, "c")],
            ),
            (
                "replaced twice, oldest event resolves to its own payload",
                vec![
                    Begin(1, 0, 1, "a"),
                    Begin(1, 0, 2, "b"),
                    Begin(1, 0, 3, "c"),
                    Event(1, Some(Superseded("a"))),
                ],
                vec![(2, "b"), (3, "c")],
            ),
            (
                "slots are independent",
                vec![
                    Begin(1, 0, 1, "a"),
                    Begin(1, 1, 2, "b"),
                    Event(2, cur(1, 1, "b")),
                ],
                vec![(1, "a")],
            ),
            (
                "owners are independent in the same slot",
                vec![
                    Begin(1, 0, 1, "a"),
                    Begin(2, 0, 2, "b"),
                    Event(2, cur(2, 0, "b")),
                ],
                vec![(1, "a")],
            ),
            (
                "released owner's current import resolves as superseded",
                vec![
                    Begin(1, 0, 1, "a"),
                    Release(1),
                    Event(1, Some(Superseded("a"))),
                ],
                vec![],
            ),
            (
                "hidden owner's imports in both slots resolve as superseded",
                vec![
                    Begin(1, 0, 1, "a"),
                    Begin(1, 1, 2, "b"),
                    Release(1),
                    Event(1, Some(Superseded("a"))),
                    Event(2, Some(Superseded("b"))),
                ],
                vec![],
            ),
            (
                "released owner keeps every pending import",
                vec![Begin(1, 0, 1, "a"), Begin(1, 1, 2, "b"), Release(1)],
                vec![(1, "a"), (2, "b")],
            ),
            (
                "re-added owner's import is current",
                vec![
                    Begin(1, 0, 1, "a"),
                    Release(1),
                    Begin(1, 0, 2, "b"),
                    Event(2, cur(1, 0, "b")),
                    Event(1, Some(Superseded("a"))),
                ],
                vec![],
            ),
            (
                "unknown key",
                vec![Begin(1, 0, 1, "a"), Event(9, None)],
                vec![(1, "a")],
            ),
        ];
        for (name, ops, want) in cases {
            let mut imports = Imports::new();
            for op in ops {
                match op {
                    Begin(owner, slot, key, import) => imports.begin(owner, slot, key, import),
                    Release(owner) => imports.release_owner(owner),
                    Event(key, want) => assert_eq!(imports.resolve(&key), want, "{name}"),
                }
            }
            let mut left = imports.drain();
            left.sort_by_key(|(key, _)| *key);
            assert_eq!(left, want, "{name}");
        }
    }

    fn monitor(name: &str, x: i32, y: i32, width: u32, scale: f64, focused: bool) -> Monitor {
        Monitor {
            name: name.to_string(),
            x,
            y,
            width,
            height: width * 9 / 16,
            scale,
            transform: 0,
            reserved: [0, 34, 0, 0],
            focused,
        }
    }

    fn layout_monitors() -> Vec<Monitor> {
        vec![
            monitor("DP-1", 0, 0, 1920, 1.0, false),
            monitor("DP-3", 1920, 0, 3840, 1.5, true),
            monitor("HDMI-A-1", 5000, 0, 1920, 1.0, false),
        ]
    }

    #[test]
    fn default_monitor_cases() {
        let monitors = layout_monitors();
        let unfocused: Vec<Monitor> = monitors
            .iter()
            .cloned()
            .map(|m| Monitor {
                focused: false,
                ..m
            })
            .collect();
        type Case<'a> = (
            &'a str,
            &'a [Monitor],
            Option<&'a str>,
            Result<usize, &'a str>,
        );
        let two_focused: Vec<Monitor> = monitors
            .iter()
            .cloned()
            .map(|m| Monitor {
                focused: m.name != "DP-1",
                ..m
            })
            .collect();
        let cases: [Case; 8] = [
            ("named and present", &monitors, Some("HDMI-A-1"), Ok(2)),
            (
                "named and absent",
                &monitors,
                Some("DP-9"),
                Err("no monitor named DP-9 in j/monitors"),
            ),
            ("unset with one focused", &monitors, None, Ok(1)),
            (
                "unset with none focused",
                &unfocused,
                None,
                Err("no focused monitor in j/monitors"),
            ),
            ("named wins over focus", &monitors, Some("DP-1"), Ok(0)),
            ("two focused, the first wins", &two_focused, None, Ok(1)),
            (
                "named, empty list",
                &[],
                Some("DP-1"),
                Err("no monitor named DP-1 in j/monitors"),
            ),
            (
                "unset, empty list",
                &[],
                None,
                Err("no focused monitor in j/monitors"),
            ),
        ];
        for (name, monitors, output, want) in cases {
            let want = want.map_err(String::from);
            assert_eq!(default_monitor(monitors, output), want, "{name}");
        }
    }

    #[test]
    fn monitor_for_cases() {
        let monitors = layout_monitors();
        let cases = [
            ("names a monitor", Some("HDMI-A-1"), 2),
            ("names an absent monitor", Some("DP-9"), 1),
            ("names none", None, 1),
        ];
        for (name, output, want) in cases {
            assert_eq!(monitor_for(&monitors, output, 1), want, "{name}");
        }
    }

    #[test]
    fn area_origin_cases() {
        let at = |x, y, reserved| Monitor {
            x,
            y,
            reserved,
            ..monitor("M", 0, 0, 1920, 1.0, false)
        };
        let cases = [
            ("top bar", at(0, 0, [0, 34, 0, 0]), (0, 34), (1920, 1046)),
            (
                "scaled",
                layout_monitors()[1].clone(),
                (1920, 34),
                (2560, 1406),
            ),
            (
                "negative position",
                at(-1920, -200, [0, 34, 0, 0]),
                (-1920, -166),
                (1920, 1046),
            ),
            (
                "left reserve",
                at(0, 0, [48, 0, 0, 0]),
                (48, 0),
                (1872, 1080),
            ),
            (
                "left reserve at a position",
                at(1920, 0, [48, 0, 0, 0]),
                (1968, 0),
                (1872, 1080),
            ),
        ];
        for (name, m, origin, size) in cases {
            assert_eq!(area_origin(&m), origin, "{name}");
            let want = Size {
                width: size.0,
                height: size.1,
            };
            assert_eq!(m.usable_area().size(), want, "{name}");
        }
    }

    #[test]
    fn pointer_global_cases() {
        let cases = [
            (
                "no movement",
                (0, 34),
                (100, 50),
                (10.0, 20.0),
                (0.0, 0.0),
                (110.0, 104.0),
            ),
            (
                "offset moves it",
                (1920, 34),
                (100, 50),
                (10.0, 20.0),
                (-5.5, 7.25),
                (2024.5, 111.25),
            ),
        ];
        for (name, origin, start, grab, offset, want) in cases {
            let start = Point {
                x: start.0,
                y: start.1,
            };
            assert_eq!(pointer_global(origin, start, grab, offset), want, "{name}");
        }
    }

    #[test]
    fn desired_origin_cases() {
        let cases = [
            ("no movement", (0, 34), (100, 50), (0.0, 0.0), (100, 84)),
            (
                "offset rounds per axis",
                (1920, 34),
                (100, 50),
                (-5.5, 7.25),
                (2014, 91),
            ),
            (
                "half rounds away from zero",
                (0, 0),
                (1000, 500),
                (3.5, -2.5),
                (1004, 497),
            ),
            (
                "left of the layout",
                (0, 0),
                (10, 10),
                (-40.0, -40.0),
                (-30, -30),
            ),
        ];
        for (name, origin, start, offset, want) in cases {
            let start = Point {
                x: start.0,
                y: start.1,
            };
            assert_eq!(desired_origin(origin, start, offset), want, "{name}");
        }
    }

    fn usable_areas(monitors: &[Monitor]) -> Vec<Area> {
        monitors.iter().map(usable_global).collect()
    }

    #[test]
    fn covered_cases() {
        let areas = usable_areas(&layout_monitors());
        let rect = |x, y| Area {
            x,
            y,
            width: 480,
            height: 264,
        };
        let cases = [
            ("inside one", &areas[..], rect(100, 134), true),
            ("straddles two monitors", &areas[..], rect(1700, 134), true),
            ("over a bar region", &areas[..], rect(100, 0), false),
            ("past the last monitor", &areas[..], rect(4300, 100), false),
            (
                "below the shorter monitor",
                &areas[..],
                rect(1800, 1000),
                false,
            ),
            (
                "in the gap between monitors",
                &areas[..],
                rect(4500, 100),
                false,
            ),
            ("no monitors", &[][..], rect(0, 0), false),
            (
                "zero-area rectangle inside a monitor",
                &areas[..],
                Area {
                    width: 0,
                    ..rect(100, 134)
                },
                false,
            ),
        ];
        for (name, areas, rect, want) in cases {
            assert_eq!(covered(rect, areas), want, "{name}");
        }
    }

    #[test]
    fn dragged_origin_cases() {
        let monitors = layout_monitors();
        let size = Size {
            width: 480,
            height: 264,
        };
        let neighbour = Rect {
            x: 1000,
            y: 400,
            width: 320,
            height: 176,
        };
        type Case<'a> = (
            &'a str,
            &'a [Monitor],
            (i64, i64),
            usize,
            &'a [Rect],
            (i64, i64),
        );
        let cases: [Case; 8] = [
            (
                "single monitor, inside",
                &monitors[..1],
                (100, 134),
                0,
                &[],
                (100, 134),
            ),
            (
                "single monitor, clamped",
                &monitors[..1],
                (-50, -50),
                0,
                &[],
                (0, 34),
            ),
            (
                "straddles two monitors, used as is",
                &monitors[..2],
                (1700, 134),
                1,
                &[],
                (1700, 134),
            ),
            (
                "straddling, snapped to the usable edge of the pointer's monitor",
                &monitors[..2],
                (1925, 134),
                1,
                &[],
                (1920, 134),
            ),
            (
                "over a bar region, clamped",
                &monitors[..2],
                (100, 10),
                0,
                &[],
                (100, 34),
            ),
            (
                "past the outer edge, clamped into the pointer's monitor",
                &monitors[..2],
                (4400, 134),
                1,
                &[],
                (4000, 134),
            ),
            (
                "snapped to a thumbnail",
                &monitors[..2],
                (1315, 84),
                0,
                &[neighbour],
                (1320, 84),
            ),
            (
                "odd position rounds down to even",
                &monitors[..1],
                (101, 135),
                0,
                &[],
                (100, 134),
            ),
        ];
        for (name, monitors, desired, under, others, want) in cases {
            let got = dragged_origin(desired, size, under, monitors, others, 10);
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn dragged_origin_usable_width_cases() {
        let size = Size {
            width: 480,
            height: 264,
        };
        let cases = [
            (
                "even width, right-edge snap",
                2560,
                (2075, 134),
                (2080, 134),
            ),
            (
                "odd width rounds the right-edge snap down",
                2561,
                (2075, 134),
                (2080, 134),
            ),
            (
                "snap past the edge is clamped back",
                2560,
                (2600, 134),
                (2080, 134),
            ),
        ];
        for (name, width, desired, want) in cases {
            let monitors = [Monitor {
                height: 1440,
                ..monitor("M", 0, 0, width, 1.0, true)
            }];
            let got = dragged_origin(desired, size, 0, &monitors, &[], 10);
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn touched_cases() {
        let monitors = layout_monitors();
        let flat = |name, x, y| Monitor {
            reserved: [0; 4],
            ..monitor(name, x, y, 1920, 1.0, false)
        };
        let corner = [flat("A", 0, 0), flat("B", 1920, 0), flat("C", 0, 1080)];
        let rect = |x, y| Area {
            x,
            y,
            width: 480,
            height: 264,
        };
        let cases = [
            ("inside home", &monitors[..], 0, rect(100, 134), vec![]),
            (
                "scale 1 into scale 1.5",
                &monitors[..],
                0,
                rect(1700, 134),
                vec![1],
            ),
            (
                "scale 1.5 into scale 1",
                &monitors[..],
                1,
                rect(1700, 134),
                vec![0],
            ),
            (
                "bar region of the neighbour",
                &monitors[..],
                0,
                Area {
                    x: 1700,
                    y: 0,
                    width: 480,
                    height: 30,
                },
                vec![],
            ),
            (
                "edge contact only",
                &monitors[..],
                0,
                rect(1440, 134),
                vec![],
            ),
            ("third monitor", &monitors[..], 0, rect(4900, 134), vec![2]),
            (
                "corner of three monitors",
                &corner[..],
                0,
                rect(1800, 1000),
                vec![1, 2],
            ),
        ];
        for (name, monitors, home, rect, want) in cases {
            assert_eq!(touched(monitors, home, rect), want, "{name}");
        }
    }

    #[test]
    fn fitted_cases() {
        let thumbnail = config::Thumbnail {
            width: 480,
            min_width: 160,
            max_width: 1600,
            opacity: 100,
            snap_distance: 10,
        };
        let buffer = Size {
            width: 2560,
            height: 1406,
        };
        let usable = |width, height| Size { width, height };
        let size = |width, height| Size { width, height };
        let at = |x, y| Point { x, y };
        let cases = [
            (
                "fits",
                480,
                usable(2560, 1000),
                (100, 134),
                (480, size(480, 264), at(100, 134)),
            ),
            (
                "landing without a resize is unchanged",
                480,
                usable(2560, 1000),
                (80, 100),
                (480, size(480, 264), at(80, 100)),
            ),
            (
                "wider than the usable width",
                1280,
                usable(1080, 1000),
                (0, 0),
                (1080, size(1080, 594), at(0, 0)),
            ),
            (
                "width grown past the landing monitor is capped and the position clamped",
                1280,
                usable(1080, 1000),
                (80, 100),
                (1080, size(1080, 594), at(0, 100)),
            ),
            (
                "landing monitor's width on a narrower monitor",
                1128,
                usable(1080, 1000),
                (0, 0),
                (1080, size(1080, 594), at(0, 0)),
            ),
            (
                "grown past the old monitor's usable width",
                1120,
                usable(2560, 1000),
                (0, 0),
                (1120, size(1120, 616), at(0, 0)),
            ),
            (
                "negative request",
                -5,
                usable(2560, 1000),
                (0, 0),
                (160, size(160, 88), at(0, 0)),
            ),
            (
                "request above u32::MAX",
                5_000_000_000,
                usable(2560, 1000),
                (0, 0),
                (1600, size(1600, 878), at(0, 0)),
            ),
            (
                "odd request rounds down",
                481,
                usable(2560, 1000),
                (0, 0),
                (480, size(480, 264), at(0, 0)),
            ),
            (
                "below the minimum",
                100,
                usable(2560, 1000),
                (0, 0),
                (160, size(160, 88), at(0, 0)),
            ),
            (
                "above the configured maximum",
                2000,
                usable(2560, 1000),
                (0, 0),
                (1600, size(1600, 878), at(0, 0)),
            ),
            (
                "left of the area",
                480,
                usable(2560, 1000),
                (-220, 100),
                (480, size(480, 264), at(0, 100)),
            ),
            (
                "above the area",
                480,
                usable(2560, 1000),
                (80, -34),
                (480, size(480, 264), at(80, 0)),
            ),
            (
                "beyond the right edge",
                480,
                usable(2560, 1000),
                (2480, 100),
                (480, size(480, 264), at(2080, 100)),
            ),
            (
                "beyond the bottom edge",
                480,
                usable(1920, 1046),
                (100, 966),
                (480, size(480, 264), at(100, 782)),
            ),
            (
                "odd position rounds down to even",
                480,
                usable(2560, 1000),
                (101, 135),
                (480, size(480, 264), at(100, 134)),
            ),
        ];
        for (name, width, usable, position, want) in cases {
            assert_eq!(
                fitted(width, &thumbnail, buffer, usable, position),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn under_pointer_cases() {
        let monitors = layout_monitors();
        let cases = [
            ("on the first monitor", (100.0, 100.0), 1, 0),
            ("on the second monitor", (2000.0, 100.0), 0, 1),
            (
                "at the shared edge, the right monitor",
                (1920.0, 100.0),
                0,
                1,
            ),
            ("in a gap keeps the last monitor", (4600.0, 100.0), 1, 1),
            ("in a gap keeps the first monitor", (4600.0, 100.0), 0, 0),
            ("below every monitor", (100.0, 5000.0), 2, 2),
        ];
        for (name, point, last, want) in cases {
            assert_eq!(under_pointer(&monitors, point, last), want, "{name}");
        }
    }

    fn placed(x: i32, y: i32, reserved: [u32; 4]) -> Monitor {
        Monitor {
            x,
            y,
            reserved,
            ..monitor("M", 0, 0, 1920, 1.0, false)
        }
    }

    #[test]
    fn local_offset_cases() {
        let scaled = layout_monitors()[1].clone();
        let flat = placed(0, 0, [0; 4]);
        let cases = [
            (
                "scale 1.0",
                placed(0, 0, [0, 34, 0, 0]),
                (100, 134),
                (100, 100),
                (100, 100),
            ),
            (
                "scale 1.5",
                scaled.clone(),
                (2000, 134),
                (80, 100),
                (80, 100),
            ),
            (
                "monitor at a negative position",
                placed(-1920, -200, [0, 34, 0, 0]),
                (-1900, -100),
                (20, 66),
                (20, 66),
            ),
            (
                "left reserve, a negative local coordinate",
                placed(0, 0, [48, 0, 0, 0]),
                (40, 10),
                (-8, 10),
                (-8, 10),
            ),
            (
                "a point past the surface size",
                scaled,
                (6000, 3000),
                (4080, 2966),
                (4080, 2966),
            ),
            (
                "beyond the i32 range",
                flat,
                (i64::MAX, i64::MIN),
                (i64::MAX, i64::MIN),
                (i32::MAX, i32::MIN),
            ),
        ];
        for (name, monitor, at, relative, (x, y)) in cases {
            assert_eq!(usable_local(at, &monitor), relative, "{name} relative");
            assert_eq!(local_offset(at, &monitor), Offset { x, y }, "{name} offset");
        }
    }

    #[test]
    fn layout_point_cases() {
        let cases = [
            (
                "scale 1.0",
                placed(0, 0, [0, 34, 0, 0]),
                (100, 100),
                (100, 134),
            ),
            (
                "scale 1.5",
                layout_monitors()[1].clone(),
                (80, 100),
                (2000, 134),
            ),
            (
                "monitor at a negative position",
                placed(-1920, -200, [0, 34, 0, 0]),
                (20, 66),
                (-1900, -100),
            ),
            (
                "left reserve",
                placed(0, 0, [48, 0, 0, 0]),
                (0, 10),
                (48, 10),
            ),
        ];
        for (name, monitor, (x, y), want) in cases {
            assert_eq!(layout_point(Point { x, y }, &monitor), want, "{name}");
            assert_eq!(
                usable_local(want, &monitor),
                (i64::from(x), i64::from(y)),
                "{name} inverse"
            );
        }
    }

    #[test]
    fn point_of_cases() {
        let cases = [
            ("inside", (80, 100), (80, 100)),
            ("origin", (0, 0), (0, 0)),
            ("negative x", (-8, 10), (0, 10)),
            ("negative y", (10, -8), (10, 0)),
            (
                "largest",
                (i32::MAX, i32::MAX),
                (2_147_483_647, 2_147_483_647),
            ),
        ];
        for (name, (x, y), (want_x, want_y)) in cases {
            let want = Point {
                x: want_x,
                y: want_y,
            };
            assert_eq!(point_of(Offset { x, y }), want, "{name}");
        }
    }

    #[test]
    fn bound_monitor_cases() {
        let cases = [
            ("home", SurfaceState::Home, 0, 0),
            ("straddling", SurfaceState::Straddling(vec![1, 2]), 0, 0),
            (
                "landing",
                SurfaceState::Landing {
                    monitor: 2,
                    others: vec![1],
                },
                0,
                2,
            ),
        ];
        for (name, state, monitor, want) in cases {
            assert_eq!(bound_monitor(&state, monitor), want, "{name}");
        }
    }

    #[test]
    fn relocation_cases() {
        let monitors = layout_monitors();
        let key = |output, busy| Trigger::KeyChange { output, busy };
        let cases = [
            (
                "key change to a present monitor",
                key(Some("HDMI-A-1"), false),
                Origin::Saved,
                0,
                1,
                Some(2),
            ),
            (
                "key change to the record's own monitor",
                key(Some("DP-1"), false),
                Origin::Saved,
                0,
                1,
                None,
            ),
            (
                "entry without output goes to the default monitor",
                key(None, false),
                Origin::Saved,
                2,
                1,
                Some(1),
            ),
            (
                "entry naming an absent monitor goes to the default monitor",
                key(Some("DP-9"), false),
                Origin::Saved,
                2,
                1,
                Some(1),
            ),
            (
                "default placement on the default monitor",
                key(None, false),
                Origin::Default,
                1,
                1,
                None,
            ),
            (
                "default placement away from the default monitor",
                key(None, false),
                Origin::Default,
                0,
                1,
                Some(1),
            ),
            (
                "key change during a drag or a landing, present monitor",
                key(Some("HDMI-A-1"), true),
                Origin::Saved,
                0,
                1,
                None,
            ),
            (
                "key change during a drag or a landing, default",
                key(None, true),
                Origin::Default,
                0,
                1,
                None,
            ),
            (
                "drop of a user placement stays",
                Trigger::Settled { entry: None },
                Origin::User,
                2,
                1,
                None,
            ),
            (
                "drop of a default placement returns to the default monitor",
                Trigger::Settled { entry: None },
                Origin::Default,
                2,
                1,
                Some(1),
            ),
            (
                "drop of a default placement on the default monitor",
                Trigger::Settled { entry: None },
                Origin::Default,
                1,
                1,
                None,
            ),
            (
                "saved placement on its entry's monitor",
                Trigger::Settled {
                    entry: Some(Some("HDMI-A-1")),
                },
                Origin::Saved,
                2,
                1,
                None,
            ),
            (
                "saved placement, entry names another present monitor",
                Trigger::Settled {
                    entry: Some(Some("HDMI-A-1")),
                },
                Origin::Saved,
                0,
                1,
                Some(2),
            ),
            (
                "saved placement, entry without output, on the default monitor",
                Trigger::Settled { entry: Some(None) },
                Origin::Saved,
                1,
                1,
                None,
            ),
            (
                "saved placement, entry without output, off the default monitor",
                Trigger::Settled { entry: Some(None) },
                Origin::Saved,
                2,
                1,
                Some(1),
            ),
            (
                "saved placement, entry names an absent monitor",
                Trigger::Settled {
                    entry: Some(Some("DP-9")),
                },
                Origin::Saved,
                2,
                1,
                Some(1),
            ),
            (
                "saved placement without an entry on the default monitor",
                Trigger::Settled { entry: None },
                Origin::Saved,
                1,
                1,
                None,
            ),
            (
                "saved placement without an entry off the default monitor",
                Trigger::Settled { entry: None },
                Origin::Saved,
                2,
                1,
                None,
            ),
            (
                "user placement ignores the entry's monitor",
                Trigger::Settled {
                    entry: Some(Some("HDMI-A-1")),
                },
                Origin::User,
                0,
                1,
                None,
            ),
        ];
        for (name, trigger, origin, monitor, default, want) in cases {
            let got = relocation(trigger, &monitors, origin, monitor, default);
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn settled_cases() {
        let entry = |width, x, y| layout::Entry {
            x,
            y,
            width,
            output: None,
        };
        let relocate = |target, width, at| Settle::Relocate { target, width, at };
        let cases = [
            (
                "saved, entry on the same monitor, geometry applied",
                None,
                Origin::Saved,
                Some(entry(400, 30, 40)),
                relocate(None, 400, Some((30, 40))),
            ),
            (
                "saved, entry on another monitor, moves with the entry's geometry",
                Some(2),
                Origin::Saved,
                Some(entry(400, 30, 40)),
                relocate(Some(2), 400, Some((30, 40))),
            ),
            (
                "saved without an entry stays",
                None,
                Origin::Saved,
                None,
                Settle::Stay,
            ),
            (
                "default placement moves to its row",
                Some(1),
                Origin::Default,
                None,
                relocate(Some(1), 480, None),
            ),
            (
                "default placement on the default monitor stays",
                None,
                Origin::Default,
                None,
                Settle::Stay,
            ),
            (
                "user placement is saved",
                None,
                Origin::User,
                None,
                Settle::Save,
            ),
        ];
        for (name, target, origin, entry, want) in cases {
            assert_eq!(settled(target, origin, entry.as_ref(), 480), want, "{name}");
        }
    }

    #[test]
    fn surface_step_cases() {
        use SurfaceAction::{Commit, Create, Destroy, Present};
        use SurfaceEvent::{Configured, ReleaseAt, ReleaseHome, TearDown, Touch};
        use SurfaceState::{Home, Landing, Straddling};
        let landing = |monitor, others: &[usize]| Landing {
            monitor,
            others: others.to_vec(),
        };
        type Case<'a> = (
            &'a str,
            SurfaceState,
            SurfaceEvent,
            SurfaceState,
            Vec<SurfaceAction>,
        );
        let cases: Vec<Case> = vec![
            ("home, still home", Home, Touch(vec![]), Home, vec![]),
            (
                "home to straddling",
                Home,
                Touch(vec![1]),
                Straddling(vec![1]),
                vec![Create(1)],
            ),
            (
                "corner of three monitors",
                Home,
                Touch(vec![1, 2]),
                Straddling(vec![1, 2]),
                vec![Create(1), Create(2)],
            ),
            (
                "corner left for one monitor",
                Straddling(vec![1, 2]),
                Touch(vec![2]),
                Straddling(vec![2]),
                vec![Destroy(1)],
            ),
            (
                "third monitor entered",
                Straddling(vec![1]),
                Touch(vec![2]),
                Straddling(vec![2]),
                vec![Destroy(1), Create(2)],
            ),
            (
                "third monitor left",
                Straddling(vec![2]),
                Touch(vec![]),
                Home,
                vec![Destroy(2)],
            ),
            (
                "same monitors",
                Straddling(vec![1]),
                Touch(vec![1]),
                Straddling(vec![1]),
                vec![],
            ),
            (
                "drag events while landing change nothing",
                landing(1, &[]),
                Touch(vec![]),
                landing(1, &[]),
                vec![],
            ),
            ("release on home from home", Home, ReleaseHome, Home, vec![]),
            (
                "release on home destroys every traveller",
                Straddling(vec![1, 2]),
                ReleaseHome,
                Home,
                vec![Destroy(1), Destroy(2)],
            ),
            (
                "release on a configured traveller commits",
                Straddling(vec![1, 2]),
                ReleaseAt {
                    monitor: 1,
                    configured: true,
                },
                Home,
                vec![Destroy(2), Commit(1)],
            ),
            (
                "release before the traveller's configure",
                Straddling(vec![1, 2]),
                ReleaseAt {
                    monitor: 1,
                    configured: false,
                },
                landing(1, &[2]),
                vec![],
            ),
            (
                "release where no traveller exists yet",
                Straddling(vec![2]),
                ReleaseAt {
                    monitor: 1,
                    configured: false,
                },
                landing(1, &[2]),
                vec![Create(1)],
            ),
            (
                "release or key change onto another monitor from home",
                Home,
                ReleaseAt {
                    monitor: 1,
                    configured: false,
                },
                landing(1, &[]),
                vec![Create(1)],
            ),
            (
                "configure after the release commits",
                landing(1, &[2]),
                Configured(1),
                Home,
                vec![Present(1), Destroy(2), Commit(1)],
            ),
            (
                "another traveller's configure while landing",
                landing(1, &[2]),
                Configured(2),
                landing(1, &[2]),
                vec![Present(2)],
            ),
            (
                "configure of a traveller during the drag",
                Straddling(vec![1]),
                Configured(1),
                Straddling(vec![1]),
                vec![Present(1)],
            ),
            (
                "configure of an unknown monitor during the drag",
                Straddling(vec![1]),
                Configured(2),
                Straddling(vec![1]),
                vec![],
            ),
            (
                "configure of an unknown monitor while landing",
                landing(1, &[2]),
                Configured(3),
                landing(1, &[2]),
                vec![],
            ),
            (
                "release on home while landing",
                landing(1, &[2]),
                ReleaseHome,
                landing(1, &[2]),
                vec![],
            ),
            (
                "release elsewhere while landing",
                landing(1, &[]),
                ReleaseAt {
                    monitor: 2,
                    configured: false,
                },
                landing(1, &[]),
                vec![],
            ),
            ("hide from home", Home, TearDown, Home, vec![]),
            (
                "hide during a drag",
                Straddling(vec![1, 2]),
                TearDown,
                Home,
                vec![Destroy(1), Destroy(2)],
            ),
            (
                "hide while landing",
                landing(1, &[2]),
                TearDown,
                Home,
                vec![Destroy(1), Destroy(2)],
            ),
        ];
        for (name, state, event, next, actions) in cases {
            assert_eq!(surface_step(&state, event), (next, actions), "{name}");
        }
    }

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

    #[test]
    fn setup_error_display_cases() {
        let cases = [
            (
                "global",
                SetupError::Global {
                    name: "wl_compositor",
                    reason: "missing".to_string(),
                },
                "wl_compositor: missing",
            ),
            (
                "relative pointer global",
                SetupError::Global {
                    name: "zwp_relative_pointer_manager_v1",
                    reason: "not advertised by the compositor".to_string(),
                },
                "zwp_relative_pointer_manager_v1: not advertised by the compositor",
            ),
            (
                "alpha modifier global",
                SetupError::Global {
                    name: "wp_alpha_modifier_v1",
                    reason: "missing".to_string(),
                },
                "wp_alpha_modifier_v1: missing",
            ),
            (
                "no output",
                SetupError::NoOutput("HDMI-A-1".to_string()),
                "no wl_output named HDMI-A-1",
            ),
            (
                "wayland",
                SetupError::Wayland("broken pipe".to_string()),
                "broken pipe",
            ),
            (
                "loop",
                SetupError::Loop("poll failed".to_string()),
                "event loop: poll failed",
            ),
            (
                "dmabuf global missing",
                SetupError::DmabufGlobal(GlobalError::MissingGlobal("zwp_linux_dmabuf_v1")),
                "the 'zwp_linux_dmabuf_v1' global was not available",
            ),
            (
                "dmabuf global version",
                SetupError::DmabufGlobal(GlobalError::InvalidVersion {
                    name: "zwp_linux_dmabuf_v1",
                    required: 4,
                    available: 3,
                }),
                "the 'zwp_linux_dmabuf_v1' global does not support interface version 4 (using version 3)",
            ),
            (
                "allocator",
                SetupError::Allocator(DmabufError::NoRenderNode(0x2a)),
                "no /dev/dri/renderD* node with device number 0x2a",
            ),
        ];
        for (name, err, want) in cases {
            assert_eq!(err.to_string(), want, "{name}");
        }
    }

    #[test]
    fn out_of_step_reason_cases() {
        let cases = [
            ("copy", "copy: no frame, overlay or buffer for the slot"),
            (
                "recommit",
                "recommit: no frame, overlay or buffer for the slot",
            ),
            (
                "present",
                "present: no frame, overlay or buffer for the slot",
            ),
        ];
        for (action, want) in cases {
            assert_eq!(out_of_step_reason(action), want, "{action}");
        }
    }
}
