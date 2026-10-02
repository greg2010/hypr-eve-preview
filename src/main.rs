#![deny(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod capture;
mod chrome;
mod cli;
mod clients;
mod config;
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

use std::collections::BTreeMap;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use calloop::generic::Generic;
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
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1;
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use capture::{Action, Capture, DmabufInfo, Input, SLOTS};
use chrome::{Chrome, Font};
use clients::{Change, Clients};
use config::Config;
use dmabuf::{Allocator, DmaBuffer, DmabufError, FormatModifier};
use geometry::{Point, Rect, Size, thumbnail_size};
use input::{Cursor, Effect, GRIP_SIZE, Gestures, PointerInput};
use ipc::EventSocket;
use layout::{Entry, KeyChange, LayoutFile, SaveAction, SaveSchedule};
use overlay::Overlay;
use protocol::hyprland_toplevel_export_frame_v1::{self, Flags, HyprlandToplevelExportFrameV1};
use protocol::hyprland_toplevel_export_manager_v1::HyprlandToplevelExportManagerV1;
use report::{DamageMode, Line, Reporter};

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

struct ClientState {
    capture: Capture,
    overlay: Option<Overlay>,
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
    fn entry(&self) -> Entry {
        Entry {
            x: self.position.x,
            y: self.position.y,
            width: self.width,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PressStart {
    address: u64,
    position: Point,
    width: u32,
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
    feedback: Feedback,
    allocator: Allocator,
    pointers: Vec<SeatPointer>,
    gestures: Gestures,
    press: Option<PressStart>,
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
    scale: f64,
    usable: Size,
    mode: DamageMode,
    reporter: Reporter,
    requests: PathBuf,
    events: EventSocket,
    event_token: Option<RegistrationToken>,
    stop: Option<Stop>,
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

    /// The user id of a game client. A failure is reported and means no user id, and the account
    /// key then falls back to the character name from the title.
    fn game_user_id(&mut self, entry: &hypr::Client) -> Option<u64> {
        if !hypr::is_game_client(&entry.class, &entry.title) {
            return None;
        }
        match clients::read_user_id(entry.pid) {
            Ok(id) => Some(id),
            Err(err) => {
                self.emit(&Line::Account {
                    address: entry.address,
                    error: err.to_string(),
                });
                None
            }
        }
    }

    fn feed(&mut self, address: u64, input: Input) {
        if self.stop.is_some() {
            return;
        }
        let Some(record) = self.records.get_mut(&address) else {
            return;
        };
        let actions = record.capture.handle(input, Instant::now());
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
                Action::Recommit { slot, buffer_size } => {
                    let record = self.records.get_mut(&address);
                    match record {
                        Some(ClientState {
                            overlay: Some(overlay),
                            buffers,
                            ..
                        }) if buffers[slot].is_some() => {
                            if let Some(buffer) = buffers[slot].as_ref() {
                                overlay.recommit(&buffer.wl_buffer, buffer_size);
                            }
                        }
                        _ => self.out_of_step("recommit"),
                    }
                }
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

    fn output(&self) -> Option<WlOutput> {
        self.output_state.outputs().find(|o| {
            self.output_state
                .info(o)
                .is_some_and(|i| i.name.as_deref() == Some(self.config.output.as_str()))
        })
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
        let Some(output) = self.output() else {
            self.stop(failure(SetupError::NoOutput(self.config.output.clone())));
            return;
        };
        let Some(width) = self.records.get(&address).map(|r| r.width) else {
            return;
        };
        let size = thumbnail_size(width, buffer_size);
        let position = self.position_for(address, size);
        let globals = overlay::Globals {
            compositor: &self.compositor,
            subcompositor: &self.subcompositor,
            layer_shell: &self.layer_shell,
            viewporter: &self.viewporter,
            shm: &self.shm,
            output: &output,
        };
        match Overlay::new(&self.qh, &globals, position, size) {
            Ok(overlay) => {
                if let Some(record) = self.records.get_mut(&address) {
                    record.overlay = Some(overlay);
                    record.buffer_size = Some(buffer_size);
                    record.position = position;
                }
                let changes = self.clients.thumbnail_created(address);
                self.apply_changes(changes);
            }
            Err(e) => self.stop(failure(e)),
        }
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
        let redraw = !record.chromed || record.overlay.as_ref().is_some_and(|o| o.size() != size);
        let position = self.position_for(address, size);
        if redraw && !self.draw_chrome(address, size) {
            return;
        }
        let Some(record) = self.records.get_mut(&address) else {
            return;
        };
        record.buffer_size = Some(buffer_size);
        if let (Some(overlay), Some(buffer)) =
            (record.overlay.as_mut(), record.buffers[slot].as_ref())
        {
            overlay.present(&buffer.wl_buffer, buffer_size, size, position, y_invert);
            record.position = overlay.position();
        }
    }

    fn draw_chrome(&mut self, address: u64, logical: Size) -> bool {
        let Some(label) = self.clients.get(address).map(|t| t.label()) else {
            return true;
        };
        let border = &self.config.border;
        let ring = (self.clients.ring_owner() == Some(address) && border.width > 0)
            .then_some((border.width, border.color));
        let buffer = chrome::buffer_size(logical, self.scale);
        let Some(record) = self.records.get_mut(&address) else {
            return true;
        };
        let Some(overlay) = record.overlay.as_mut() else {
            return true;
        };
        let chrome = Chrome {
            ring,
            label: &label,
            style: &self.config.label,
            scale: self.scale,
        };
        let font = &self.font;
        let result = overlay.draw_chrome(logical, buffer, |canvas, size| {
            chrome::render(canvas, size, &chrome, font);
        });
        match result {
            Ok(()) => {
                record.chromed = true;
                true
            }
            Err(e) => {
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
        if !self.draw_chrome(address, size) {
            return;
        }
        if let Some(overlay) = self
            .records
            .get_mut(&address)
            .and_then(|r| r.overlay.as_mut())
        {
            overlay.commit();
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
        let entry = record.entry();
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
            Some(entry) => {
                let user_id = self.game_user_id(entry);
                self.clients.add(entry, user_id)
            }
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
        let saved = account.and_then(|k| self.layout.entries.get(&k.to_string()).copied());
        let configured = &self.config.thumbnail;
        let (origin, width, position) = match saved {
            Some(entry) => (
                Origin::Saved,
                layout::effective_width(entry.width, configured, self.usable),
                Point {
                    x: entry.x,
                    y: entry.y,
                },
            ),
            None => (
                Origin::Default,
                layout::effective_width(configured.width, configured, self.usable),
                Point { x: 0, y: 0 },
            ),
        };
        let handle = hypr::capture_handle(address);
        self.records.insert(
            address,
            ClientState {
                capture: Capture::new(self.mode, address, handle),
                overlay: None,
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
            if let Some(token) = record.timer.take() {
                self.loop_handle.remove(token);
            }
            if let Some(frame) = record.frame.take() {
                if frame.got_event {
                    frame.proxy.destroy();
                } else {
                    self.orphans.push(frame);
                }
            }
            if let Some(overlay) = record.overlay.take() {
                overlay.destroy();
            }
            for buffer in record.buffers.iter_mut().filter_map(Option::take) {
                buffer.destroy();
            }
            self.imports.release_owner(address);
        }
        self.emit(&Line::ClientRemoved { address, reason });
        self.recompute_placement();
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
        let current = record.entry();
        let user_placed = record.origin == Origin::User;
        match layout::key_change(&mut self.layout, &key.to_string(), current, user_placed) {
            KeyChange::Saved => self.request_save(),
            KeyChange::Apply(entry) => {
                if let Some(record) = self.records.get_mut(&address) {
                    record.origin = Origin::Saved;
                }
                self.set_geometry(
                    address,
                    i64::from(entry.width),
                    Some((i64::from(entry.x), i64::from(entry.y))),
                );
                self.recompute_placement();
            }
            KeyChange::Default => {
                if let Some(record) = self.records.get_mut(&address) {
                    record.origin = Origin::Default;
                }
                self.set_geometry(address, i64::from(self.config.thumbnail.width), None);
                self.recompute_placement();
            }
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

    fn position_for(&self, address: u64, size: Size) -> Point {
        let Some(record) = self.records.get(&address) else {
            return Point { x: 0, y: 0 };
        };
        let (x, y) = if follows_row(record.origin, self.gestures.active(), address) {
            let index = self
                .default_order()
                .iter()
                .position(|a| *a == address)
                .unwrap_or(0);
            self.default_slot(index)
        } else {
            (i64::from(record.position.x), i64::from(record.position.y))
        };
        layout::clamp_position(x, y, size, self.usable)
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

    fn move_clamped(&mut self, address: u64, x: i64, y: i64) {
        let usable = self.usable;
        let Some(record) = self.records.get_mut(&address) else {
            return;
        };
        let Some(overlay) = record.overlay.as_mut() else {
            return;
        };
        let position = layout::clamp_position(x, y, overlay.size(), usable);
        if position != record.position {
            overlay.move_to(position);
            record.position = position;
        }
    }

    fn set_geometry(&mut self, address: u64, requested: i64, desired: Option<(i64, i64)>) {
        let clamped = u32::try_from(requested.max(0)).unwrap_or(u32::MAX);
        let width = layout::effective_width(clamped, &self.config.thumbnail, self.usable);
        let usable = self.usable;
        let Some(record) = self.records.get_mut(&address) else {
            return;
        };
        let (x, y) =
            desired.unwrap_or((i64::from(record.position.x), i64::from(record.position.y)));
        record.width = width;
        let (Some(buffer_size), Some(overlay)) = (record.buffer_size, record.overlay.as_ref())
        else {
            record.position = Point {
                x: u32::try_from(x.max(0)).unwrap_or(u32::MAX),
                y: u32::try_from(y.max(0)).unwrap_or(u32::MAX),
            };
            return;
        };
        let size = thumbnail_size(width, buffer_size);
        let position = layout::clamp_position(x, y, size, usable);
        let resized = overlay.size() != size;
        let moved = overlay.position() != position;
        if resized && !self.draw_chrome(address, size) {
            return;
        }
        let Some(record) = self.records.get_mut(&address) else {
            return;
        };
        record.position = position;
        if let Some(overlay) = record.overlay.as_mut() {
            if resized {
                overlay.resize(size, position);
            } else if moved {
                overlay.move_to(position);
            }
        }
    }

    fn mark_user_placed(&mut self, address: u64) {
        if let Some(record) = self.records.get_mut(&address) {
            record.origin = Origin::User;
        }
    }

    fn thumbnail_at(&self, surface: &WlSurface) -> Option<u64> {
        self.records
            .iter()
            .find(|(_, r)| r.overlay.as_ref().is_some_and(|o| o.surface() == surface))
            .map(|(address, _)| *address)
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
                Effect::DragEnd { address } | Effect::ResizeEnd { address } => {
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

    fn drag(&mut self, address: u64, offset: (f64, f64)) {
        let Some(start) = self.press.filter(|p| p.address == address) else {
            return;
        };
        let x = i64::from(start.position.x) + offset.0.round() as i64;
        let y = i64::from(start.position.y) + offset.1.round() as i64;
        self.move_clamped(address, x, y);
    }

    fn begin_press(&mut self, address: u64) {
        if let Some(record) = self.records.get(&address) {
            self.press = Some(PressStart {
                address,
                position: record.position,
                width: record.width,
            });
        }
    }

    fn in_grip_at(&self, address: u64, position: (f64, f64)) -> bool {
        self.records
            .get(&address)
            .and_then(|r| r.overlay.as_ref())
            .is_some_and(|o| in_grip(position, o.size()))
    }

    fn drop_pointers(&mut self, seat: &WlSeat) {
        if let Some(address) = self.gestures.active()
            && self.stop.is_none()
        {
            self.route_input(None, PointerInput::Leave { address });
        }
        self.press = None;
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

    /// Tears down timers, the pending layout save and Wayland objects, prints `exit` and exits.
    /// Callers keep the `Signals` source installed until exit, so a late signal stays pending.
    fn shutdown(mut self, stop: Stop) -> ! {
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
        let error = self.conn.flush().err().map(|e| format!("flush: {e}"));
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
        let Some(address) = self.thumbnail_at(layer.wl_surface()) else {
            return;
        };
        let Some(overlay) = self
            .records
            .get_mut(&address)
            .and_then(|r| r.overlay.as_mut())
        else {
            return;
        };
        if overlay.is_configured() {
            return;
        }
        overlay.set_configured();
        self.feed(address, Input::OverlayConfigured);
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
            let Some(address) = self.thumbnail_at(&event.surface) else {
                continue;
            };
            let input = match &event.kind {
                PointerEventKind::Enter { .. } => PointerInput::Enter { address },
                PointerEventKind::Leave { .. } => PointerInput::Leave { address },
                PointerEventKind::Motion { .. } => PointerInput::Motion {
                    address,
                    in_grip: self.in_grip_at(address, event.position),
                },
                PointerEventKind::Press { button, .. } => {
                    if *button == BTN_LEFT {
                        self.begin_press(address);
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

fn fail(reporter: &mut Reporter, code: u8, reason: impl std::fmt::Display) -> ! {
    finish(
        reporter,
        Stop::Exit {
            code,
            reason: reason.to_string(),
        },
        None,
    )
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
    clients: Clients,
    scale: f64,
    usable: Rect,
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
        clients,
        scale,
        usable,
        mode,
    } = startup;
    let parts = match bind_wayland() {
        Ok(parts) => parts,
        Err(e) => fail(&mut reporter, 1, e),
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
        seat_state,
        relative_pointer_state,
    } = parts;
    let qh = queue.handle();
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
        feedback,
        allocator,
        pointers: Vec::new(),
        gestures: Gestures::default(),
        press: None,
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
        scale,
        usable: Size {
            width: usable.width,
            height: usable.height,
        },
        mode,
        reporter,
        requests,
        events,
        event_token: None,
        stop: None,
    };
    // The wl_output binds from OutputState::new are answered up to `done` before the sync returns.
    let settled = queue
        .roundtrip(&mut app)
        .map_err(wayland)
        .and_then(|_| {
            if app.output().is_none() {
                return Err(SetupError::NoOutput(app.config.output.clone()));
            }
            Ok(())
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

/// Runs startup from the log file to the `start` line. A failure prints the `exit` line and exits.
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
        ),
    };
    let (font, font_path) = match &config.label.font_file {
        Some(path) => match Font::load(path) {
            Ok(font) => (font, path.clone()),
            Err(e) => fail(
                &mut reporter,
                2,
                format!("label.font_file {}: {e}", path.display()),
            ),
        },
        None => {
            let family = &config.label.font;
            let loaded = chrome::resolve_font(family)
                .and_then(|path| Font::load(&path).map(|font| (font, path)));
            match loaded {
                Ok(loaded) => loaded,
                Err(e) => fail(&mut reporter, 1, format!("fc-match {family}: {e}")),
            }
        }
    };
    let state_home = std::env::var_os("XDG_STATE_HOME");
    let Some(layout_path) = layout::default_path(state_home.as_deref(), home.as_deref()) else {
        fail(
            &mut reporter,
            1,
            "no layout path: XDG_STATE_HOME and HOME are unset or unusable",
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
            ),
        },
    };
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
    let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE");
    let sockets = match ipc::sockets(runtime_dir.as_deref(), signature.as_deref()) {
        Ok(sockets) => sockets,
        Err(e) => fail(&mut reporter, 1, e),
    };
    let events = match EventSocket::connect(&sockets.events) {
        Ok(events) => events,
        Err(e) => fail(&mut reporter, 1, format!("event socket: {e}")),
    };
    let monitors = match query(&sockets.requests, "j/monitors", hypr::parse_monitors) {
        Ok(monitors) => monitors,
        Err(reason) => fail(&mut reporter, 1, reason),
    };
    let Some(monitor) = monitors.iter().find(|m| m.name == config.output) else {
        fail(
            &mut reporter,
            1,
            format!("no monitor named {} in j/monitors", config.output),
        )
    };
    let usable = monitor.usable_area();
    let scale = monitor.scale;
    let snapshot = match query(&sockets.requests, "j/clients", hypr::parse_clients) {
        Ok(snapshot) => snapshot,
        Err(reason) => fail(&mut reporter, 1, reason),
    };
    let active = match query(
        &sockets.requests,
        "j/activewindow",
        hypr::parse_active_window,
    ) {
        Ok(active) => active,
        Err(reason) => fail(&mut reporter, 1, reason),
    };
    let mode = if args.ignore_damage {
        DamageMode::IgnoreDamage
    } else {
        DamageMode::Recommit
    };
    let start = Line::Start {
        output: config.output.clone(),
        scale,
        usable,
        mode,
        config: config_read,
        layout: layout_path.clone(),
        font: font_path,
    };
    emit_or_finish(&mut reporter, &start);
    let startup = Startup {
        reporter,
        config,
        font,
        layout_path,
        layout,
        requests: sockets.requests,
        events,
        clients: Clients::new(active),
        scale,
        usable,
        mode,
    };
    (startup, snapshot)
}

fn main() -> ! {
    let start = Instant::now();
    let args = match cli::parse_os(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            let message = e.to_string();
            let mut reporter = Reporter::stderr(false);
            emit_or_finish(
                &mut reporter,
                &Line::Usage {
                    message: message.clone(),
                },
            );
            fail(&mut reporter, 2, message)
        }
    };
    let reporter = match args.log.as_deref() {
        None => Reporter::stderr(args.verbose),
        Some(path) => match Reporter::open(path, args.verbose) {
            Ok(reporter) => reporter,
            Err(e) => {
                let mut reporter = Reporter::stderr(args.verbose);
                fail(&mut reporter, 1, format!("log {}: {e}", path.display()))
            }
        },
    };
    let (mut startup, snapshot) = prepare(&args, reporter);

    let mut event_loop = match EventLoop::<App>::try_new() {
        Ok(l) => l,
        Err(e) => fail(&mut startup.reporter, 1, loop_error(e)),
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
    for entry in &snapshot {
        if app.stop.is_some() {
            break;
        }
        let user_id = app.game_user_id(entry);
        let changes = app.clients.add(entry, user_id);
        app.apply_changes(changes);
    }
    if let Err(e) = app.insert_event_source() {
        app.shutdown(failure(e));
    }

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
