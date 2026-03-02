use anyhow::{Context, Result};
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_shm::WlShm;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

use crate::config::ResolvedConfig;

use crate::capture::CaptureFrame;
use crate::overlay::OverlayState;

// ── Output info ───────────────────────────────────────────────

/// Metadata for a wl_output we discover during globals enumeration.
#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub wl_output: WlOutput,
    pub name: Option<String>,
    pub width: i32,
    pub height: i32,
    pub scale: i32,
}

impl OutputInfo {
    pub fn new(wl_output: WlOutput) -> Self {
        Self {
            wl_output,
            name: None,
            width: 0,
            height: 0,
            scale: 1,
        }
    }
}

// ── Application state ─────────────────────────────────────────

/// The top-level Wayland application state.
///
/// Holds all bound globals and sub-states needed by the event loop:
/// - Compositor globals (shm, seat, output)
/// - Screencopy manager
/// - The overlay/selection sub-state (Phase 3)
pub struct AppState {
    // ── Wayland globals ──
    pub shm: Option<WlShm>,
    pub seat: Option<WlSeat>,
    pub outputs: Vec<OutputInfo>,
    pub screencopy_manager: Option<ZwlrScreencopyManagerV1>,

    // ── Overlay / selection ──
    pub overlay: OverlayState,

    // ── Screencopy frame (Phase 6) ──
    pub capture_frame: CaptureFrame,

    // ── Config ──
    pub border_color: [u8; 4],
    pub border_width: u32,
    pub overlay_color: [u8; 3],
    pub overlay_opacity: f64,
    pub overlay_idle_opacity: f64,
    pub show_dimensions: bool,
    pub running: bool,

    // ── Freeze-frame buffer (pre-captured full output as ARGB8888 u32s) ──
    pub frozen_buffer: Option<Vec<u32>>,
    pub frozen_width: u32,
    pub frozen_height: u32,
}

impl AppState {
    pub fn new(cfg: &ResolvedConfig) -> Self {
        Self {
            shm: None,
            seat: None,
            outputs: Vec::new(),
            screencopy_manager: None,
            overlay: OverlayState::new(),
            capture_frame: CaptureFrame::default(),
            border_color: cfg.border_color,
            border_width: cfg.border_width,
            overlay_color: cfg.overlay_color,
            overlay_opacity: cfg.overlay_opacity,
            overlay_idle_opacity: cfg.overlay_idle_opacity,
            show_dimensions: cfg.show_dimensions,
            running: true,
            frozen_buffer: None,
            frozen_width: 0,
            frozen_height: 0,
        }
    }
}

// ── wl_output dispatch ────────────────────────────────────────

impl Dispatch<WlOutput, usize> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlOutput,
        event: <WlOutput as wayland_client::Proxy>::Event,
        data: &usize,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_output::Event;
        let idx = *data;
        if idx >= state.outputs.len() {
            return;
        }
        match event {
            Event::Geometry { .. } => {}
            Event::Mode { width, height, .. } => {
                state.outputs[idx].width = width;
                state.outputs[idx].height = height;
            }
            Event::Scale { factor } => {
                state.outputs[idx].scale = factor;
            }
            Event::Name { name } => {
                state.outputs[idx].name = Some(name);
            }
            Event::Done => {}
            _ => {}
        }
    }
}

// ── wl_registry dispatch ──────────────────────────────────────

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: <wl_registry::WlRegistry as wayland_client::Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Handled by GlobalList roundtrip; nothing extra needed.
    }
}

// ── Environment bootstrap ─────────────────────────────────────

/// Connect to the Wayland display, perform a global roundtrip, and bind
/// all required protocol objects.
///
/// Returns `(Connection, EventQueue, AppState, GlobalList)` ready for the overlay phase.
pub fn connect(
    cfg: &ResolvedConfig,
) -> Result<(Connection, EventQueue<AppState>, AppState, GlobalList)> {
    let conn = Connection::connect_to_env().context("Failed to connect to Wayland display")?;

    // Initial roundtrip to collect globals.
    let (global_list, mut event_queue) =
        registry_queue_init::<AppState>(&conn).context("Failed to enumerate Wayland globals")?;
    let qh = event_queue.handle();

    let mut state = AppState::new(cfg);

    // Bind wl_shm (required for SHM buffer allocation).
    state.shm = Some(
        global_list
            .bind::<WlShm, _, _>(&qh, 1..=1, ())
            .context("Compositor does not support wl_shm")?,
    );

    // Bind wl_seat (required for pointer input).
    state.seat = Some(
        global_list
            .bind::<WlSeat, _, _>(&qh, 1..=9, ())
            .context("Compositor does not advertise a wl_seat")?,
    );

    // Bind all wl_outputs.
    // Use a separate counter so user-data matches the index in state.outputs.
    let mut output_idx = 0usize;
    for global in global_list.contents().clone_list().iter() {
        if global.interface == "wl_output" {
            let output: WlOutput =
                global_list
                    .registry()
                    .bind(global.name, global.version.min(4), &qh, output_idx);
            state.outputs.push(OutputInfo::new(output));
            output_idx += 1;
        }
    }

    // Bind zwlr_screencopy_manager_v1 (required for screen capture).
    state.screencopy_manager = Some(
        global_list
            .bind::<ZwlrScreencopyManagerV1, _, _>(&qh, 1..=3, ())
            .context(
                "Compositor does not support zwlr_screencopy_manager_v1 — \
                 is this a wlroots-based compositor (Hyprland, Sway, etc.)?",
            )?,
    );

    // Second roundtrip to receive output mode/geometry events.
    event_queue
        .roundtrip(&mut state)
        .context("Wayland roundtrip failed")?;

    Ok((conn, event_queue, state, global_list))
}

// ── Blanket dispatch stubs for globals we bind but don't need events from ──

impl Dispatch<WlShm, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WlShm,
        _event: <WlShm as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: <WlSeat as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrScreencopyManagerV1,
        _event: <ZwlrScreencopyManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}
