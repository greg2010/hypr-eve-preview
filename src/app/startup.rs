use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use calloop::LoopHandle;
use smithay_client_toolkit::compositor::CompositorState;
use smithay_client_toolkit::dmabuf::{DmabufFeedback, DmabufHandler, DmabufState};
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::RegistryState;
use smithay_client_toolkit::seat::SeatState;
use smithay_client_toolkit::seat::relative_pointer::RelativePointerState;
use smithay_client_toolkit::shell::wlr_layer::LayerShell;
use smithay_client_toolkit::shm::Shm;
use smithay_client_toolkit::subcompositor::SubcompositorState;
use wayland_client::globals::{GlobalList, registry_queue_init};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::{Connection, EventQueue, QueueHandle};
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_v1::WpAlphaModifierV1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use super::buffers::{Feedback, Imports};
use super::{App, ControlLoop};
use crate::chrome::Font;
use crate::clients::Clients;
use crate::config::Config;
use crate::dmabuf::{Allocator, FormatModifier};
use crate::exit::{
    SetupError, Stop, emit_or_finish, fail, failure, finish, loop_error, remove_control, wayland,
};
use crate::hypr::Monitor;
use crate::input::Gestures;
use crate::ipc::EventSocket;
use crate::layout::{LayoutFile, SaveSchedule};
use crate::placement::default_monitor;
use crate::protocol::hyprland_toplevel_export_manager_v1::HyprlandToplevelExportManagerV1;
use crate::report::{DamageMode, Line, Reporter};
use crate::{chrome, cli, clients, config, hypr, ipc, layout};

/// State of the short-lived feedback queue.
struct Probe {
    dmabuf_state: DmabufState,
    feedback: Option<(u64, Feedback)>,
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

smithay_client_toolkit::delegate_dispatch2!(Probe);

/// Sends a Hyprland request and parses the reply. The error is the `exit` reason.
pub(super) fn query<T>(
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

pub(crate) struct Startup {
    pub(crate) reporter: Reporter,
    config: Config,
    font: Font,
    layout_path: PathBuf,
    layout: LayoutFile,
    requests: PathBuf,
    events: EventSocket,
    pub(crate) control: crate::control::Server,
    clients: Clients,
    monitors: Vec<Monitor>,
    default_monitor: usize,
    mode: DamageMode,
}

/// Binds the Wayland globals and output and opens the GBM device. A failure prints the `exit` line
/// and exits.
pub(crate) fn setup(startup: Startup, loop_handle: LoopHandle<'static, App>) -> App {
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
pub(crate) fn prepare(args: &cli::Args, mut reporter: Reporter) -> (Startup, Vec<hypr::Client>) {
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
    let control = match crate::control::Server::bind(&sockets.control) {
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
pub(crate) fn run_command(command: crate::control::Command) -> ! {
    let mut reporter = Reporter::stderr(false);
    let sockets = sockets_from_env(&mut reporter);
    match crate::control::send(&sockets.control, command) {
        Ok(()) => std::process::exit(0),
        Err(e) => fail(&mut reporter, 1, e, None),
    }
}
