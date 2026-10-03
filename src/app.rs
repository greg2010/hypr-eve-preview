use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use calloop::ping::Ping;
use calloop::{LoopHandle, RegistrationToken};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::dmabuf::DmabufState;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::seat::SeatState;
use smithay_client_toolkit::seat::pointer::ThemedPointer;
use smithay_client_toolkit::seat::relative_pointer::RelativePointerState;
use smithay_client_toolkit::shell::wlr_layer::LayerShell;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::subcompositor::SubcompositorState;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, QueueHandle};
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_v1::WpAlphaModifierV1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1;
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use crate::capture::{Capture, DmabufInfo, SLOTS};
use crate::chrome::Font;
use crate::clients::Clients;
use crate::config::Config;
use crate::dmabuf::{Allocator, DmaBuffer};
use crate::exit::{SetupError, Stop, failure, out_of_step_reason};
use crate::geometry::{Offset, Point, Size};
use crate::hypr::Monitor;
use crate::input::Gestures;
use crate::ipc::EventSocket;
use crate::layout::{Entry, LayoutFile, SaveSchedule};
use crate::overlay;
use crate::overlay::Overlay;
use crate::placement::Origin;
use crate::protocol::hyprland_toplevel_export_frame_v1::HyprlandToplevelExportFrameV1;
use crate::protocol::hyprland_toplevel_export_manager_v1::HyprlandToplevelExportManagerV1;
use crate::report::{DamageMode, Line, Reporter};
use crate::surface::SurfaceState;

mod buffers;
mod capture;
mod control;
mod placement;
mod pointer;
mod records;
mod render;
mod shutdown;
mod startup;
mod surfaces;
mod tray;

use buffers::{Feedback, Imports, PendingImport};
pub(crate) use startup::{prepare, run_command, setup};

struct Frame {
    proxy: HyprlandToplevelExportFrameV1,
    sync_id: u64,
    got_event: bool,
    dmabuf: Option<DmabufInfo>,
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

pub(crate) struct App {
    pub(crate) conn: Connection,
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
    pub(crate) clients: Clients,
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
    control: Option<crate::control::Server>,
    control_loop: ControlLoop,
    tray: Option<crate::tray::Tray>,
    tray_token: Option<RegistrationToken>,
    ping: Option<Ping>,
    ping_token: Option<RegistrationToken>,
    pub(crate) stop: Option<Stop>,
}

/// The loop registrations of the control socket. `App::control` holds the server itself, which
/// is `None` only after shutdown has removed it.
#[derive(Default)]
struct ControlLoop {
    listener: Option<RegistrationToken>,
    pause: Option<RegistrationToken>,
    connections: HashMap<crate::control::ConnectionId, ConnectionTokens>,
}

struct ConnectionTokens {
    source: RegistrationToken,
    timer: Option<RegistrationToken>,
}

impl App {
    pub(crate) fn stop(&mut self, stop: Stop) {
        if self.stop.is_none() {
            self.stop = Some(stop);
        }
    }

    pub(crate) fn stop_with(&mut self, code: u8, reason: impl Into<String>) {
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

smithay_client_toolkit::delegate_registry!(App);

smithay_client_toolkit::delegate_dispatch2!(App);
