#![deny(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod capture;
mod cli;
mod dmabuf;
mod geometry;
mod hypr;
mod overlay;
mod protocol;
mod report;

use std::time::{Duration, Instant};

use calloop::signals::{Signal, Signals};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, LoopHandle, RegistrationToken};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::dmabuf::{DmabufFeedback, DmabufHandler, DmabufState};
use smithay_client_toolkit::error::GlobalError;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::shell::wlr_layer::{
    LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use wayland_client::globals::{GlobalList, registry_queue_init};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use capture::{Action, Capture, DamageMode, DmabufInfo, Input, SLOTS};
use dmabuf::{Allocator, DmaBuffer, DmabufError, FormatModifier};
use geometry::Size;
use overlay::Overlay;
use protocol::hyprland_toplevel_export_frame_v1::{self, Flags, HyprlandToplevelExportFrameV1};
use protocol::hyprland_toplevel_export_manager_v1::HyprlandToplevelExportManagerV1;
use report::Line;

const OUTPUT_NAME: &str = "DP-3";

enum Stop {
    Exit { code: u8, reason: String },
    StderrFailed,
}

#[derive(Debug)]
enum SetupError {
    Global { name: &'static str, reason: String },
    NoOutput,
    Wayland(String),
    Loop(String),
    DmabufGlobal(GlobalError),
    Allocator(DmabufError),
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupError::Global { name, reason } => write!(f, "{name}: {reason}"),
            SetupError::NoOutput => write!(f, "no wl_output named {OUTPUT_NAME}"),
            SetupError::Wayland(e) => write!(f, "wayland: {e}"),
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
    Current { slot: usize, import: T },
    Superseded(T),
}

struct SlotImports<K, T> {
    current: Option<(K, T)>,
    superseded: Vec<(K, T)>,
}

/// Pending imports per slot. A replaced import stays listed until its own event arrives.
struct Imports<K, T> {
    slots: [SlotImports<K, T>; SLOTS],
}

impl<K: PartialEq, T> Imports<K, T> {
    fn new() -> Self {
        Imports {
            slots: std::array::from_fn(|_| SlotImports {
                current: None,
                superseded: Vec::new(),
            }),
        }
    }

    fn begin(&mut self, slot: usize, key: K, import: T) {
        let entry = &mut self.slots[slot];
        if let Some(old) = entry.current.replace((key, import)) {
            entry.superseded.push(old);
        }
    }

    /// Removes `key` from the list and returns its payload. `None` means the key is not listed.
    fn resolve(&mut self, key: &K) -> Option<Resolved<T>> {
        for (slot, entry) in self.slots.iter_mut().enumerate() {
            if entry.current.as_ref().is_some_and(|(k, _)| k == key)
                && let Some((_, import)) = entry.current.take()
            {
                return Some(Resolved::Current { slot, import });
            }
            if let Some(at) = entry.superseded.iter().position(|(k, _)| k == key) {
                return Some(Resolved::Superseded(entry.superseded.remove(at).1));
            }
        }
        None
    }

    /// Empties the list and returns every entry, current and superseded.
    fn drain(&mut self) -> Vec<(K, T)> {
        let mut out = Vec::new();
        for entry in &mut self.slots {
            out.extend(entry.current.take());
            out.append(&mut entry.superseded);
        }
        out
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

struct App {
    conn: Connection,
    qh: QueueHandle<App>,
    loop_handle: LoopHandle<'static, App>,
    registry_state: RegistryState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    dmabuf_state: DmabufState,
    viewporter: WpViewporter,
    export_manager: HyprlandToplevelExportManagerV1,
    feedback: Feedback,
    allocator: Allocator,
    capture: Capture,
    handle: u32,
    overlay: Option<Overlay>,
    buffers: [Option<DmaBuffer>; SLOTS],
    imports: Imports<ZwpLinuxBufferParamsV1, PendingImport>,
    frame: Option<Frame>,
    last_sync_id: u64,
    timer: Option<RegistrationToken>,
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

    fn feed(&mut self, input: Input) {
        if self.stop.is_some() {
            return;
        }
        let actions = self.capture.handle(input, Instant::now());
        self.execute(actions);
    }

    fn execute(&mut self, actions: Vec<Action>) {
        for action in actions {
            if self.stop.is_some() {
                return;
            }
            match action {
                Action::RequestFrame => {
                    let proxy = self
                        .export_manager
                        .capture_toplevel(0, self.handle, &self.qh, ());
                    self.last_sync_id += 1;
                    let sync_id = self.last_sync_id;
                    self.conn.display().sync(&self.qh, sync_id);
                    self.frame = Some(Frame {
                        proxy,
                        sync_id,
                        got_event: false,
                        dmabuf: None,
                    });
                }
                Action::CreateOverlay { size } => self.create_overlay(size),
                Action::Allocate { slot, fourcc, size } => {
                    if let Err(stop) = self.allocate(slot, fourcc, size) {
                        self.stop(stop);
                    }
                }
                Action::Copy {
                    slot,
                    ignore_damage,
                } => match (&self.frame, &self.buffers[slot]) {
                    (Some(frame), Some(buffer)) => {
                        frame
                            .proxy
                            .copy(&buffer.wl_buffer, i32::from(ignore_damage));
                    }
                    _ => self.out_of_step("copy"),
                },
                Action::Recommit { slot, buffer_size } => {
                    match (self.overlay.as_mut(), &self.buffers[slot]) {
                        (Some(overlay), Some(buffer)) => {
                            overlay.recommit(&buffer.wl_buffer, buffer_size);
                        }
                        _ => self.out_of_step("recommit"),
                    }
                }
                Action::Present {
                    slot,
                    buffer_size,
                    size,
                    y_invert,
                } => match (self.overlay.as_mut(), &self.buffers[slot]) {
                    (Some(overlay), Some(buffer)) => {
                        overlay.present(&buffer.wl_buffer, buffer_size, size, y_invert);
                    }
                    _ => self.out_of_step("present"),
                },
                Action::DestroyFrame => {
                    if let Some(frame) = self.frame.take() {
                        frame.proxy.destroy();
                    }
                }
                Action::WakeAt(at) => self.arm_timer(at),
                Action::Report(line) => {
                    if let Err(stop) = emit(&line) {
                        self.stop(stop);
                    }
                }
                Action::Exit { code, reason } => self.stop_with(code, reason),
            }
        }
    }

    fn out_of_step(&mut self, action: &str) {
        self.stop(failure(out_of_step_reason(action)));
    }

    fn dp3(&self) -> Option<WlOutput> {
        self.output_state.outputs().find(|o| {
            self.output_state
                .info(o)
                .is_some_and(|i| i.name.as_deref() == Some(OUTPUT_NAME))
        })
    }

    fn create_overlay(&mut self, size: Size) {
        let Some(output) = self.dp3() else {
            self.stop(failure(SetupError::NoOutput));
            return;
        };
        match Overlay::new(
            &self.qh,
            &self.compositor,
            &self.layer_shell,
            &self.viewporter,
            &output,
            size,
        ) {
            Ok(overlay) => self.overlay = Some(overlay),
            Err(e) => self.stop(failure(e)),
        }
    }

    fn allocate(&mut self, slot: usize, fourcc: u32, size: Size) -> Result<(), Stop> {
        if let Some(old) = self.buffers[slot].take() {
            old.destroy();
        }
        let modifiers =
            dmabuf::modifiers_for(&self.feedback.table, &self.feedback.tranches, fourcc);
        let bo = self
            .allocator
            .allocate(size, fourcc, &modifiers)
            .map_err(failure)?;
        emit(&Line::Format {
            fourcc,
            modifier: u64::from(bo.modifier()),
            size,
            planes: bo.plane_count(),
        })?;
        let params = self.dmabuf_state.create_params(&self.qh).map_err(failure)?;
        let params = dmabuf::import(&bo, params).map_err(failure)?;
        self.imports.begin(
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

    fn arm_timer(&mut self, at: Instant) {
        if let Some(token) = self.timer.take() {
            self.loop_handle.remove(token);
        }
        let timer = Timer::from_deadline(at);
        match self
            .loop_handle
            .insert_source(timer, |_, _, app: &mut App| {
                app.timer = None;
                app.feed(Input::Wake);
                TimeoutAction::Drop
            }) {
            Ok(token) => self.timer = Some(token),
            Err(e) => self.stop(failure(loop_error(e))),
        }
    }

    fn teardown(mut self) -> Option<String> {
        if let Some(token) = self.timer.take() {
            self.loop_handle.remove(token);
        }
        if let Some(frame) = self.frame.take()
            && frame.got_event
        {
            frame.proxy.destroy();
        }
        if let Some(overlay) = self.overlay.take() {
            overlay.destroy();
        }
        let mut objects = Vec::new();
        for slot in &mut self.buffers {
            if let Some(buffer) = slot.take() {
                buffer.wl_buffer.destroy();
                objects.push(buffer.bo);
            }
        }
        let mut pending = Vec::new();
        for (params, import) in self.imports.drain() {
            params.destroy();
            pending.push(import);
        }
        self.export_manager.destroy();
        let error = self.conn.flush().err().map(|e| format!("flush: {e}"));
        drop(objects);
        drop(pending);
        drop(self.allocator);
        error
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
        _: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        if overlay.is_configured() {
            return;
        }
        overlay.set_configured();
        self.feed(Input::OverlayConfigured);
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
            Some(Resolved::Current { slot, import }) => {
                params.destroy();
                self.buffers[slot] = Some(DmaBuffer {
                    bo: import.bo,
                    wl_buffer: buffer,
                });
                self.feed(Input::Imported { slot });
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
        let slot = self
            .buffers
            .iter()
            .position(|b| b.as_ref().is_some_and(|b| b.wl_buffer == *buffer));
        if let Some(slot) = slot {
            self.feed(Input::Released { slot });
        }
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers!(OutputState);
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
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

impl Dispatch<HyprlandToplevelExportFrameV1, ()> for App {
    fn event(
        state: &mut App,
        proxy: &HyprlandToplevelExportFrameV1,
        event: hyprland_toplevel_export_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        if state.stop.is_some() {
            return;
        }
        let Some(frame) = state.frame.as_mut() else {
            return;
        };
        if frame.proxy != *proxy {
            return;
        }
        frame.got_event = true;
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
            state.feed(input);
        }
    }
}

wayland_client::delegate_noop!(App: ignore WpViewporter);

wayland_client::delegate_noop!(App: ignore WpViewport);

impl Dispatch<WlCallback, u64> for App {
    fn event(
        state: &mut App,
        _: &WlCallback,
        event: wl_callback::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        let wl_callback::Event::Done { .. } = event else {
            return;
        };
        let missing = state.frame.as_ref().is_some_and(|f| f.sync_id == *id);
        if missing {
            state.feed(Input::FrameMissing);
        }
    }
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

smithay_client_toolkit::delegate_registry!(App);
smithay_client_toolkit::delegate_dispatch2!(App);
smithay_client_toolkit::delegate_dispatch2!(Probe);

fn emit(line: &Line) -> Result<(), Stop> {
    report::emit(line).map_err(|_| Stop::StderrFailed)
}

fn failure(e: impl std::fmt::Display) -> Stop {
    Stop::Exit {
        code: 1,
        reason: e.to_string(),
    }
}

fn wayland(e: impl std::fmt::Display) -> SetupError {
    SetupError::Wayland(e.to_string())
}

/// Prints the `exit` line and exits. Callers keep the `Signals` source installed until the
/// process exits, so a late SIGINT or SIGTERM stays pending instead of changing the status.
fn finish(stop: Stop, teardown: Option<String>) -> ! {
    let code = match stop {
        Stop::StderrFailed => 1,
        Stop::Exit { code, reason } => {
            let (code, line) = report::exit_line(code, reason, teardown);
            match emit(&line) {
                Ok(()) => code,
                Err(_) => 1,
            }
        }
    };
    std::process::exit(i32::from(code))
}

fn loop_error(e: impl std::fmt::Display) -> SetupError {
    SetupError::Loop(e.to_string())
}

fn out_of_step_reason(action: &str) -> String {
    format!("{action}: no frame, overlay or buffer for the slot")
}

fn exit_failure(reason: impl std::fmt::Display) -> ! {
    finish(failure(reason), None)
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

fn setup(
    capture: Capture,
    handle: u32,
    loop_handle: LoopHandle<'static, App>,
) -> Result<App, SetupError> {
    let conn = Connection::connect_to_env().map_err(wayland)?;
    let (globals, mut queue) = registry_queue_init::<App>(&conn).map_err(wayland)?;
    let qh = queue.handle();
    let (allocator, feedback) = probe_feedback(&conn, &globals)?;
    let global = |name: &'static str| {
        move |e: wayland_client::globals::BindError| SetupError::Global {
            name,
            reason: e.to_string(),
        }
    };
    let compositor = CompositorState::bind(&globals, &qh).map_err(global("wl_compositor"))?;
    let layer_shell = LayerShell::bind(&globals, &qh).map_err(global("zwlr_layer_shell_v1"))?;
    let viewporter = globals
        .bind::<WpViewporter, _, _>(&qh, 1..=1, ())
        .map_err(global("wp_viewporter"))?;
    let export_manager = globals
        .bind::<HyprlandToplevelExportManagerV1, _, _>(&qh, 1..=2, ())
        .map_err(global("hyprland_toplevel_export_manager_v1"))?;
    let mut app = App {
        conn: conn.clone(),
        qh: qh.clone(),
        loop_handle: loop_handle.clone(),
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        dmabuf_state: DmabufState::new(&globals, &qh),
        viewporter,
        export_manager,
        feedback,
        allocator,
        capture,
        handle,
        overlay: None,
        buffers: Default::default(),
        imports: Imports::new(),
        frame: None,
        last_sync_id: 0,
        timer: None,
        stop: None,
    };
    // The wl_output binds from OutputState::new are answered up to `done` before the sync returns.
    queue.roundtrip(&mut app).map_err(wayland)?;
    if app.dp3().is_none() {
        return Err(SetupError::NoOutput);
    }
    WaylandSource::new(conn, queue)
        .insert(loop_handle)
        .map_err(loop_error)?;
    Ok(app)
}

fn main() -> ! {
    let start = Instant::now();
    let args = match cli::parse_os(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            let message = e.to_string();
            let stop = match emit(&Line::Usage {
                message: message.clone(),
            }) {
                Ok(()) => Stop::Exit {
                    code: 2,
                    reason: message,
                },
                Err(stop) => stop,
            };
            finish(stop, None);
        }
    };
    let clients = match hypr::query_clients() {
        Ok(clients) => clients,
        Err(e) => exit_failure(e),
    };
    let client = match hypr::select(&clients, args.address) {
        Ok(client) => client,
        Err(e) => exit_failure(e),
    };
    let handle = hypr::capture_handle(client.address);
    let mode = if args.ignore_damage {
        DamageMode::IgnoreDamage
    } else {
        DamageMode::Recommit
    };
    let lines = [
        Line::Client {
            address: client.address,
            handle,
            title: client.title.clone(),
            workspace: client.workspace.name.clone(),
        },
        Line::Mode { mode },
    ];
    for line in &lines {
        if let Err(stop) = emit(line) {
            finish(stop, None);
        }
    }

    let mut event_loop = match EventLoop::<App>::try_new() {
        Ok(l) => l,
        Err(e) => exit_failure(loop_error(e)),
    };
    let capture = Capture::new(args.width, mode, client.address, handle);
    let mut app = match setup(capture, handle, event_loop.handle()) {
        Ok(app) => app,
        Err(e) => exit_failure(e),
    };
    let signals = match Signals::new(&[Signal::SIGINT, Signal::SIGTERM]) {
        Ok(s) => s,
        Err(e) => finish(failure(loop_error(e)), app.teardown()),
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
        finish(failure(loop_error(&e)), app.teardown());
    }
    app.feed(Input::Start);

    let deadline = args.seconds.map(|s| start + Duration::from_secs(s));
    let stop = loop {
        if let Some(stop) = app.stop.take() {
            break stop;
        }
        let timeout = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        if let Err(e) = event_loop.dispatch(timeout, &mut app) {
            let reason = match app.conn.protocol_error() {
                Some(p) => loop_error(p),
                None => loop_error(e),
            };
            app.stop(failure(reason));
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            app.stop_with(0, "seconds elapsed");
        }
    };
    let teardown = app.teardown();
    finish(stop, teardown)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        Begin(usize, u32, &'static str),
        Event(u32, Option<Resolved<&'static str>>),
    }

    #[test]
    fn pending_import_cases() {
        use Op::{Begin, Event};
        use Resolved::{Current, Superseded};
        let cases = [
            (
                "one pending, its own event",
                vec![
                    Begin(0, 1, "a"),
                    Event(
                        1,
                        Some(Current {
                            slot: 0,
                            import: "a",
                        }),
                    ),
                ],
                vec![],
            ),
            (
                "replaced once, old then new",
                vec![
                    Begin(0, 1, "a"),
                    Begin(0, 2, "b"),
                    Event(1, Some(Superseded("a"))),
                    Event(
                        2,
                        Some(Current {
                            slot: 0,
                            import: "b",
                        }),
                    ),
                ],
                vec![],
            ),
            (
                "replaced once, new then old",
                vec![
                    Begin(0, 1, "a"),
                    Begin(0, 2, "b"),
                    Event(
                        2,
                        Some(Current {
                            slot: 0,
                            import: "b",
                        }),
                    ),
                    Event(1, Some(Superseded("a"))),
                ],
                vec![],
            ),
            (
                "replaced twice, each old event",
                vec![
                    Begin(0, 1, "a"),
                    Begin(0, 2, "b"),
                    Begin(0, 3, "c"),
                    Event(1, Some(Superseded("a"))),
                    Event(2, Some(Superseded("b"))),
                ],
                vec![(3, "c")],
            ),
            (
                "replaced twice, middle event only",
                vec![
                    Begin(0, 1, "a"),
                    Begin(0, 2, "b"),
                    Begin(0, 3, "c"),
                    Event(2, Some(Superseded("b"))),
                ],
                vec![(1, "a"), (3, "c")],
            ),
            (
                "replaced twice, oldest event resolves to its own payload",
                vec![
                    Begin(0, 1, "a"),
                    Begin(0, 2, "b"),
                    Begin(0, 3, "c"),
                    Event(1, Some(Superseded("a"))),
                ],
                vec![(2, "b"), (3, "c")],
            ),
            (
                "slots are independent",
                vec![
                    Begin(0, 1, "a"),
                    Begin(1, 2, "b"),
                    Event(
                        2,
                        Some(Current {
                            slot: 1,
                            import: "b",
                        }),
                    ),
                ],
                vec![(1, "a")],
            ),
            (
                "unknown key",
                vec![Begin(0, 1, "a"), Event(9, None)],
                vec![(1, "a")],
            ),
        ];
        for (name, ops, want) in cases {
            let mut imports = Imports::new();
            for op in ops {
                match op {
                    Begin(slot, key, import) => imports.begin(slot, key, import),
                    Event(key, want) => assert_eq!(imports.resolve(&key), want, "{name}"),
                }
            }
            let mut left = imports.drain();
            left.sort_by_key(|(key, _)| *key);
            assert_eq!(left, want, "{name}");
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
            ("no output", SetupError::NoOutput, "no wl_output named DP-3"),
            (
                "wayland",
                SetupError::Wayland("broken pipe".to_string()),
                "wayland: broken pipe",
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
