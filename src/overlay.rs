use std::os::fd::AsFd;
use std::time::Instant;

use anyhow::{Context, Result};
use nix::fcntl::OFlag;
use nix::sys::mman::{shm_open, shm_unlink};
use nix::sys::stat::Mode;
use nix::unistd::ftruncate;
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_callback::WlCallback;
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_pointer::{self, WlPointer};
use wayland_client::protocol::wl_shm;
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{self, ZwlrLayerShellV1};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    self, ZwlrLayerSurfaceV1,
};

use crate::wayland::AppState;

// ── Bitmap font for dimension HUD ─────────────────────────────

/// 5×7 bitmap glyph data for digits '0'–'9' and '×'.
/// Each glyph is 7 rows; each row encodes columns left→right in bits 4..0.
const GLYPH_W: usize = 5;
const GLYPH_H: usize = 7;
const GLYPH_PAD: usize = 1; // 1px gap between glyphs

static GLYPHS: [(char, [u8; 7]); 11] = [
    (
        '0',
        [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
    ),
    (
        '1',
        [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
    ),
    (
        '2',
        [
            0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111,
        ],
    ),
    (
        '3',
        [
            0b01110, 0b10001, 0b00001, 0b00110, 0b00001, 0b10001, 0b01110,
        ],
    ),
    (
        '4',
        [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
    ),
    (
        '5',
        [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
    ),
    (
        '6',
        [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
    ),
    (
        '7',
        [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
    ),
    (
        '8',
        [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
    ),
    (
        '9',
        [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
    ),
    (
        'x',
        [
            0b00000, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b00000,
        ],
    ),
];

fn glyph_for(c: char) -> Option<&'static [u8; 7]> {
    GLYPHS.iter().find(|(ch, _)| *ch == c).map(|(_, g)| g)
}

// ── Selection geometry ────────────────────────────────────────

/// Tracks the interactive region selection via pointer drag.
#[derive(Debug, Clone, Copy, Default)]
pub struct Selection {
    /// Pointer position when the button was first pressed.
    pub start_x: f64,
    pub start_y: f64,
    /// Current pointer position (updated on motion).
    pub end_x: f64,
    pub end_y: f64,
    /// Whether we are currently dragging.
    pub active: bool,
    /// Whether the selection has been finalized (button released).
    pub done: bool,
}

impl Selection {
    /// Returns the axis-aligned bounding box as `(x, y, width, height)` in
    /// surface-local coordinates, clamped to non-negative dimensions.
    pub fn rect(&self) -> (i32, i32, i32, i32) {
        let x0 = self.start_x.min(self.end_x) as i32;
        let y0 = self.start_y.min(self.end_y) as i32;
        let x1 = self.start_x.max(self.end_x) as i32;
        let y1 = self.start_y.max(self.end_y) as i32;
        (x0, y0, (x1 - x0).max(1), (y1 - y0).max(1))
    }
}

// ── Overlay state ─────────────────────────────────────────────

/// State associated with the layer-shell overlay surface.
pub struct OverlayState {
    pub surface: Option<WlSurface>,
    pub layer_surface: Option<ZwlrLayerSurfaceV1>,
    pub configured: bool,
    pub width: u32,
    pub height: u32,
    pub selection: Selection,
    pub pointer: Option<WlPointer>,
    pub keyboard: Option<WlKeyboard>,
    /// SHM pool fd name (for cleanup).
    pub shm_name: Option<String>,
    /// Backing pixel buffer for the overlay (ARGB8888).
    pub buffer_data: Option<memmap2::MmapMut>,
    pub wl_buffer: Option<WlBuffer>,
    pub wl_shm_pool: Option<WlShmPool>,
    /// Frame-callback pacing: true when a callback is in flight.
    pub frame_pending: bool,
    /// Set by input handlers; cleared by the frame callback after redraw.
    pub needs_redraw: bool,
    /// Creation time for fade-in / pulse animations.
    pub start_time: Instant,
    /// Tracks shift key for 10px arrow-key nudge.
    pub shift_held: bool,
    /// Previous selection rect for dirty-rect damage.
    pub prev_rect: (i32, i32, i32, i32),
}

impl OverlayState {
    pub fn new() -> Self {
        Self {
            surface: None,
            layer_surface: None,
            configured: false,
            width: 0,
            height: 0,
            selection: Selection::default(),
            pointer: None,
            keyboard: None,
            shm_name: None,
            buffer_data: None,
            wl_buffer: None,
            wl_shm_pool: None,
            frame_pending: false,
            needs_redraw: false,
            start_time: Instant::now(),
            shift_held: false,
            prev_rect: (0, 0, 0, 0),
        }
    }
}

// ── Layer-shell surface creation ──────────────────────────────

/// Create the full-screen transparent overlay on the given output.
///
/// Must be called after `wayland::connect` returns successfully. It:
/// 1. Binds `wl_compositor` and `zwlr_layer_shell_v1`.
/// 2. Creates a `wl_surface` + `zwlr_layer_surface_v1` at the Overlay layer.
/// 3. Acquires the seat pointer for input events.
pub fn create_overlay(
    event_queue: &mut wayland_client::EventQueue<AppState>,
    state: &mut AppState,
    qh: &QueueHandle<AppState>,
    globals: &wayland_client::globals::GlobalList,
) -> Result<()> {
    // Bind compositor.
    let compositor: WlCompositor = globals
        .bind::<WlCompositor, _, _>(qh, 1..=6, ())
        .context("Compositor does not support wl_compositor")?;

    // Bind layer shell.
    let layer_shell: ZwlrLayerShellV1 = globals
        .bind::<ZwlrLayerShellV1, _, _>(qh, 1..=4, ())
        .context("Compositor does not support zwlr_layer_shell_v1")?;

    // Create surface.
    let surface = compositor.create_surface(qh, ());

    // Use the first output (primary).
    let output = state
        .outputs
        .first()
        .map(|o| &o.wl_output)
        .context("No outputs available")?
        .clone();

    // Create the layer surface at the Overlay layer, covering the full output.
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        Some(&output),
        zwlr_layer_shell_v1::Layer::Overlay,
        "voidsnap-selection".to_string(),
        qh,
        (),
    );

    // Anchor to all edges so the surface spans the full output.
    use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor;
    layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
    layer_surface.set_exclusive_zone(-1); // Don't push other surfaces.
    layer_surface.set_keyboard_interactivity(
        wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand,
    );

    // Commit to trigger the configure event.
    surface.commit();

    state.overlay.surface = Some(surface);
    state.overlay.layer_surface = Some(layer_surface);
    state.overlay.start_time = Instant::now();

    // Get pointer + keyboard from the seat for input tracking.
    if let Some(seat) = &state.seat {
        let pointer = seat.get_pointer(qh, ());
        state.overlay.pointer = Some(pointer);
        let keyboard = seat.get_keyboard(qh, ());
        state.overlay.keyboard = Some(keyboard);
    }

    // Roundtrip to get the configure event with the actual surface size.
    event_queue
        .roundtrip(state)
        .context("Failed to roundtrip after creating overlay")?;

    Ok(())
}

// ── SHM buffer allocation ─────────────────────────────────────

/// Allocate (or reallocate) the SHM-backed pixel buffer for the overlay surface.
///
/// Format: ARGB8888 (4 bytes per pixel).
pub fn allocate_shm_buffer(state: &mut AppState, qh: &QueueHandle<AppState>) -> Result<()> {
    let width = state.overlay.width;
    let height = state.overlay.height;
    if width == 0 || height == 0 {
        return Ok(());
    }

    let stride = width as i32 * 4;
    let size = (stride * height as i32) as usize;

    // Create a POSIX shared memory object.
    let shm_name = format!("/voidsnap-overlay-{}", std::process::id());

    // Use O_TRUNC instead of O_EXCL to gracefully handle stale segments
    // left behind by a previous crash with the same PID.
    let fd = shm_open(
        shm_name.as_str(),
        OFlag::O_CREAT | OFlag::O_RDWR | OFlag::O_TRUNC,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )
    .context("shm_open failed")?;

    // Immediately unlink so the segment is cleaned up when fd closes.
    let _ = shm_unlink(shm_name.as_str());

    ftruncate(&fd, size as i64).context("ftruncate on SHM fd failed")?;

    // Memory-map the buffer.
    let mmap = unsafe {
        memmap2::MmapOptions::new()
            .len(size)
            .map_mut(&fd)
            .context("mmap of SHM buffer failed")?
    };

    // Create wl_shm_pool and wl_buffer.
    let shm = state.shm.as_ref().context("wl_shm not bound")?;
    let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );

    state.overlay.buffer_data = Some(mmap);
    state.overlay.wl_buffer = Some(buffer);
    state.overlay.wl_shm_pool = Some(pool);
    state.overlay.shm_name = Some(shm_name);

    Ok(())
}

// ── Animation helpers ─────────────────────────────────────────

/// Ease-out cubic: fast start → gentle deceleration.
#[inline]
fn ease_out_cubic(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

// ── ARGB8888 pixel helpers ────────────────────────────────────

/// Pack components into a single ARGB8888 pixel value.
#[inline(always)]
fn argb(a: u8, r: u8, g: u8, b: u8) -> u32 {
    (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// Alpha-blend `fg` over `bg` (both ARGB8888).
#[inline(always)]
fn blend(bg: u32, fg: u32) -> u32 {
    let fg_a = (fg >> 24) & 0xFF;
    if fg_a == 0 {
        return bg;
    }
    if fg_a == 255 {
        return fg;
    }
    let inv = 255 - fg_a;
    let r = (((bg >> 16) & 0xFF) * inv + ((fg >> 16) & 0xFF) * fg_a) / 255;
    let g = (((bg >> 8) & 0xFF) * inv + ((fg >> 8) & 0xFF) * fg_a) / 255;
    let b = ((bg & 0xFF) * inv + (fg & 0xFF) * fg_a) / 255;
    let a = ((bg >> 24) & 0xFF).max(fg_a);
    (a << 24) | (r << 16) | (g << 8) | b
}

// ── Request frame callback ────────────────────────────────────

/// Arm the compositor frame callback for vsync pacing.  If a callback is
/// already pending this is a no-op.
pub fn request_frame(state: &mut AppState, qh: &QueueHandle<AppState>) {
    if state.overlay.frame_pending {
        return;
    }
    if let Some(surface) = &state.overlay.surface {
        surface.frame(qh, ());
        surface.commit();
        state.overlay.frame_pending = true;
    }
}

// ── Overlay rendering ─────────────────────────────────────────

/// Render the overlay into the SHM buffer and commit.
///
/// Implements the **Cosmic Dawn** aesthetic with:
/// - Configurable overlay tint + opacity
/// - Freeze-frame compositing (frozen desktop under dim wash)
/// - Row-oriented bulk u32 pixel writes
/// - Fade-in ease-out-cubic animation
/// - Selection border pulse animation
/// - Corner handles in dawn-red (#FF4500)
/// - Live W×H dimension HUD
pub fn draw_overlay(state: &mut AppState, qh: &QueueHandle<AppState>) {
    let width = state.overlay.width as usize;
    let height = state.overlay.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let buf = match state.overlay.buffer_data.as_mut() {
        Some(b) => b,
        None => return,
    };

    // Reinterpret the mmap as a u32 slice — safe because ARGB8888 is 4-byte
    // aligned and the buffer was allocated at page granularity.
    let pixels: &mut [u32] =
        unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u32, width * height) };

    // ── Config values ────────────────────────────────────────
    let [br, bg_c, bb, _] = state.border_color;
    let [or, og, ob] = state.overlay_color;
    let bw = state.border_width as i32;

    // ── Animation ────────────────────────────────────────────
    let elapsed = state.overlay.start_time.elapsed().as_secs_f64();
    let fade = ease_out_cubic((elapsed / 0.25).min(1.0));
    let pulse = if state.overlay.selection.active {
        0.80 + 0.20 * (elapsed * std::f64::consts::TAU / 1.5).sin()
    } else {
        1.0
    };

    // ── Selection state ──────────────────────────────────────
    let has_sel = state.overlay.selection.active || state.overlay.selection.done;
    let (sx, sy, sw, sh) = if has_sel {
        state.overlay.selection.rect()
    } else {
        (0, 0, 0, 0)
    };
    let sel_x0 = sx.max(0) as usize;
    let sel_y0 = sy.max(0) as usize;
    let sel_x1 = ((sx + sw) as usize).min(width);
    let sel_y1 = ((sy + sh) as usize).min(height);

    // ── Pre-compute colour values ────────────────────────────
    let dim_alpha = ((if has_sel {
        state.overlay_opacity
    } else {
        state.overlay_idle_opacity
    }) * fade
        * 255.0) as u8;
    let dim_px = argb(dim_alpha, or, og, ob);

    let border_alpha = (pulse * 255.0) as u8;
    let border_px = argb(border_alpha, br, bg_c, bb);

    let handle_px: u32 = argb(0xFF, 0xFF, 0x45, 0x00); // dawn-red
    let handle_sz = 6usize;
    let show_handles = has_sel && sw as usize >= handle_sz * 3 && sh as usize >= handle_sz * 3;

    // ── Frozen-frame reference ───────────────────────────────
    let frozen: Option<&[u32]> = state.frozen_buffer.as_deref();
    let fw = state.frozen_width as usize;

    // ── Row-based rendering ──────────────────────────────────
    for y in 0..height {
        let row = &mut pixels[y * width..(y + 1) * width];

        // --- Rows entirely outside the selection ---
        if !has_sel || y < sel_y0 || y >= sel_y1 {
            if let Some(frz) = frozen {
                for x in 0..width {
                    let fi = y * fw + x;
                    let base = if fi < frz.len() {
                        frz[fi]
                    } else {
                        0xFF_00_00_00
                    };
                    row[x] = blend(base, dim_px);
                }
            } else {
                row.fill(dim_px);
            }
            continue;
        }

        // --- Rows intersecting the selection ---
        let yi = y as i32;
        for x in 0..width {
            let xi = x as i32;
            let in_sel = x >= sel_x0 && x < sel_x1;

            if !in_sel {
                // Outside selection on this row.
                row[x] = if let Some(frz) = frozen {
                    let fi = y * fw + x;
                    let base = if fi < frz.len() {
                        frz[fi]
                    } else {
                        0xFF_00_00_00
                    };
                    blend(base, dim_px)
                } else {
                    dim_px
                };
                continue;
            }

            // Inside selection rect.
            let on_border =
                xi < sx + bw || xi >= sx + sw - bw || yi < sy + bw || yi >= sy + sh - bw;

            let on_handle = show_handles
                && ((xi - sx < handle_sz as i32 && yi - sy < handle_sz as i32)
                    || (sx + sw - xi <= handle_sz as i32 && yi - sy < handle_sz as i32)
                    || (xi - sx < handle_sz as i32 && sy + sh - yi <= handle_sz as i32)
                    || (sx + sw - xi <= handle_sz as i32 && sy + sh - yi <= handle_sz as i32));

            let overlay_px = if on_handle {
                handle_px
            } else if on_border {
                border_px
            } else {
                0x00_00_00_00 // transparent — show base
            };

            row[x] = if let Some(frz) = frozen {
                let fi = y * fw + x;
                let base = if fi < frz.len() {
                    frz[fi]
                } else {
                    0xFF_00_00_00
                };
                if overlay_px == 0 {
                    base
                } else {
                    blend(base, overlay_px)
                }
            } else {
                overlay_px
            };
        }
    }

    // ── Dimension HUD ────────────────────────────────────────
    if state.show_dimensions && has_sel && sw > 0 && sh > 0 {
        render_dimensions_hud(pixels, width, height, sx, sy, sw, sh, dim_px, border_px);
    }

    // ── Damage + commit ──────────────────────────────────────
    if let (Some(surface), Some(buffer)) = (
        state.overlay.surface.as_ref(),
        state.overlay.wl_buffer.as_ref(),
    ) {
        surface.attach(Some(buffer), 0, 0);

        // Dirty-rect: union of previous + current selection rects.
        let (px, py, pw, ph) = state.overlay.prev_rect;
        let handle_margin = 6i32;
        let margin = bw + handle_margin + 40; // extra for HUD clearance
        if has_sel || pw > 0 {
            let dx0 = sx.min(px).saturating_sub(margin).max(0);
            let dy0 = sy.min(py).saturating_sub(margin).max(0);
            let dx1 = ((sx + sw).max(px + pw) + margin).min(width as i32);
            let dy1 = ((sy + sh).max(py + ph) + margin).min(height as i32);
            surface.damage_buffer(dx0, dy0, (dx1 - dx0).max(1), (dy1 - dy0).max(1));
        } else {
            surface.damage_buffer(0, 0, width as i32, height as i32);
        }

        // Request the next frame callback for vsync pacing.
        surface.frame(qh, ());
        state.overlay.frame_pending = true;

        surface.commit();
    }

    // Save rect for next frame's dirty-rect calculation.
    state.overlay.prev_rect = if has_sel {
        (sx, sy, sw, sh)
    } else {
        (0, 0, 0, 0)
    };
}

// ── Dimension HUD rendering ──────────────────────────────────

/// Render a "W×H" label just below the selection rectangle's bottom-right
/// corner.  Uses the embedded 5×7 bitmap font rendered at 2× scale for
/// crispness on HiDPI displays.
fn render_dimensions_hud(
    pixels: &mut [u32],
    buf_w: usize,
    buf_h: usize,
    sx: i32,
    sy: i32,
    sw: i32,
    sh: i32,
    bg_pill: u32,
    fg_color: u32,
) {
    let label = format!("{}x{}", sw, sh);
    let scale = 2usize;
    let glyph_scaled_w = GLYPH_W * scale;
    let glyph_scaled_h = GLYPH_H * scale;
    let label_w = label.len() * (glyph_scaled_w + GLYPH_PAD * scale) - GLYPH_PAD * scale;
    let label_h = glyph_scaled_h;
    let pad = 4i32 * scale as i32;

    // Position: just below the bottom-right corner, shifted left if needed.
    let mut lx = (sx + sw - label_w as i32 - pad).max(0);
    let mut ly = sy + sh + 4;

    // Shift up if it would go off-screen.
    if ly + label_h as i32 + pad > buf_h as i32 {
        ly = (sy - label_h as i32 - pad - 4).max(0);
    }
    // Shift left if it would go off-screen.
    if lx + label_w as i32 + pad * 2 > buf_w as i32 {
        lx = (buf_w as i32 - label_w as i32 - pad * 2).max(0);
    }

    // Draw background pill.
    let pill_x0 = lx;
    let pill_y0 = ly;
    let pill_x1 = (lx + label_w as i32 + pad * 2).min(buf_w as i32);
    let pill_y1 = (ly + label_h as i32 + pad).min(buf_h as i32);
    let pill_bg = argb(
        0xDD,
        (bg_pill >> 16) as u8,
        (bg_pill >> 8) as u8,
        bg_pill as u8,
    );

    for py in pill_y0..pill_y1 {
        if py < 0 {
            continue;
        }
        let py = py as usize;
        if py >= buf_h {
            break;
        }
        for px in pill_x0..pill_x1 {
            if px < 0 {
                continue;
            }
            let px = px as usize;
            if px >= buf_w {
                break;
            }
            pixels[py * buf_w + px] = pill_bg;
        }
    }

    // Render glyphs.
    let text_x0 = lx + pad;
    let text_y0 = ly + pad / 2;
    let mut cursor_x = text_x0 as usize;

    for ch in label.chars() {
        let glyph = match glyph_for(ch) {
            Some(g) => g,
            None => {
                cursor_x += glyph_scaled_w + GLYPH_PAD * scale;
                continue;
            }
        };

        for gy in 0..GLYPH_H {
            let row_bits = glyph[gy];
            for gx in 0..GLYPH_W {
                if row_bits & (1 << (GLYPH_W - 1 - gx)) != 0 {
                    // Scale-up: fill a scale×scale block.
                    for sy2 in 0..scale {
                        for sx2 in 0..scale {
                            let px = cursor_x + gx * scale + sx2;
                            let py = text_y0 as usize + gy * scale + sy2;
                            if px < buf_w && py < buf_h {
                                pixels[py * buf_w + px] = fg_color;
                            }
                        }
                    }
                }
            }
        }

        cursor_x += glyph_scaled_w + GLYPH_PAD * scale;
    }
}

// ═══════════════════════════════════════════════════════════════
// Dispatch implementations
// ═══════════════════════════════════════════════════════════════

// ── Layer shell (global) ──────────────────────────────────────

impl Dispatch<ZwlrLayerShellV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrLayerShellV1,
        _event: <ZwlrLayerShellV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// ── Layer surface (configure / closed) ────────────────────────

impl Dispatch<ZwlrLayerSurfaceV1, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrLayerSurfaceV1,
        event: <ZwlrLayerSurfaceV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                proxy.ack_configure(serial);
                state.overlay.width = width;
                state.overlay.height = height;
                state.overlay.configured = true;

                // (Re-)allocate the pixel buffer.
                if width > 0 && height > 0 {
                    state.overlay.buffer_data = None;
                    if let Some(buf) = state.overlay.wl_buffer.take() {
                        buf.destroy();
                    }
                    if let Some(pool) = state.overlay.wl_shm_pool.take() {
                        pool.destroy();
                    }
                    if let Err(e) = allocate_shm_buffer(state, qh) {
                        eprintln!("voidsnap: failed to allocate overlay buffer: {e}");
                    }
                }

                draw_overlay(state, qh);
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.running = false;
            }
            _ => {}
        }
    }
}

// ── wl_compositor ─────────────────────────────────────────────

impl Dispatch<WlCompositor, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WlCompositor,
        _event: <WlCompositor as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// ── wl_surface ────────────────────────────────────────────────

impl Dispatch<WlSurface, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSurface,
        _event: <WlSurface as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// ── wl_callback (frame pacing) ────────────────────────────────

impl Dispatch<WlCallback, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlCallback,
        _event: <WlCallback as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        state.overlay.frame_pending = false;
        if state.overlay.needs_redraw {
            state.overlay.needs_redraw = false;
            draw_overlay(state, qh);
        }
    }
}

// ── wl_pointer (region selection) ─────────────────────────────

impl Dispatch<WlPointer, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlPointer,
        event: <WlPointer as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Button {
                button,
                state: btn_state,
                ..
            } => {
                use wayland_client::protocol::wl_pointer::ButtonState;
                // BTN_LEFT = 0x110 (272)
                if button == 0x110 {
                    match btn_state {
                        wayland_client::WEnum::Value(ButtonState::Pressed) => {
                            state.overlay.selection.active = true;
                            state.overlay.selection.done = false;
                            state.overlay.selection.start_x = state.overlay.selection.end_x;
                            state.overlay.selection.start_y = state.overlay.selection.end_y;
                        }
                        wayland_client::WEnum::Value(ButtonState::Released) => {
                            if state.overlay.selection.active {
                                state.overlay.selection.active = false;
                                state.overlay.selection.done = true;
                                state.running = false;
                            }
                        }
                        _ => {}
                    }
                }
                // Right-click or middle-click = cancel.
                if button == 0x111 || button == 0x112 {
                    state.running = false;
                    state.overlay.selection.done = false;
                }
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.overlay.selection.end_x = surface_x;
                state.overlay.selection.end_y = surface_y;

                if state.overlay.selection.active {
                    state.overlay.needs_redraw = true;
                    request_frame(state, qh);
                }
            }
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            } => {
                state.overlay.selection.end_x = surface_x;
                state.overlay.selection.end_y = surface_y;
            }
            _ => {}
        }
    }
}

// ── wl_keyboard (precision controls) ─────────────────────────

impl Dispatch<WlKeyboard, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlKeyboard,
        event: <WlKeyboard as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_keyboard::KeyState;

        match event {
            wl_keyboard::Event::Key {
                key,
                state: key_state,
                ..
            } => {
                let pressed = matches!(key_state, wayland_client::WEnum::Value(KeyState::Pressed));
                if !pressed {
                    return;
                }

                // Linux input-event-codes.h constants.
                const KEY_ESC: u32 = 1;
                const KEY_ENTER: u32 = 28;
                const KEY_SPACE: u32 = 57;
                const KEY_A: u32 = 30;
                const KEY_UP: u32 = 103;
                const KEY_LEFT: u32 = 105;
                const KEY_RIGHT: u32 = 106;
                const KEY_DOWN: u32 = 108;

                let step = if state.overlay.shift_held { 10.0 } else { 1.0 };
                let w = state.overlay.width as f64;
                let h = state.overlay.height as f64;

                match key {
                    KEY_ESC => {
                        // Cancel selection and exit.
                        state.overlay.selection.done = false;
                        state.running = false;
                    }
                    KEY_ENTER | KEY_SPACE => {
                        // Confirm current selection.
                        if state.overlay.selection.active || state.overlay.selection.done {
                            state.overlay.selection.active = false;
                            state.overlay.selection.done = true;
                            state.running = false;
                        }
                    }
                    KEY_A => {
                        // Select entire output.
                        state.overlay.selection.start_x = 0.0;
                        state.overlay.selection.start_y = 0.0;
                        state.overlay.selection.end_x = w;
                        state.overlay.selection.end_y = h;
                        state.overlay.selection.active = false;
                        state.overlay.selection.done = true;
                        state.overlay.needs_redraw = true;
                        request_frame(state, qh);
                    }
                    KEY_UP | KEY_DOWN | KEY_LEFT | KEY_RIGHT => {
                        // Nudge the active end of the selection.
                        let (dx, dy) = match key {
                            KEY_UP => (0.0, -step),
                            KEY_DOWN => (0.0, step),
                            KEY_LEFT => (-step, 0.0),
                            KEY_RIGHT => (step, 0.0),
                            _ => (0.0, 0.0),
                        };
                        state.overlay.selection.end_x =
                            (state.overlay.selection.end_x + dx).clamp(0.0, w);
                        state.overlay.selection.end_y =
                            (state.overlay.selection.end_y + dy).clamp(0.0, h);

                        // Start a selection if there isn't one.
                        if !state.overlay.selection.active && !state.overlay.selection.done {
                            state.overlay.selection.start_x = w / 2.0;
                            state.overlay.selection.start_y = h / 2.0;
                            state.overlay.selection.end_x = w / 2.0;
                            state.overlay.selection.end_y = h / 2.0;
                            state.overlay.selection.active = true;
                        }

                        state.overlay.needs_redraw = true;
                        request_frame(state, qh);
                    }
                    _ => {}
                }
            }
            wl_keyboard::Event::Modifiers { mods_depressed, .. } => {
                // Bit 0 = Shift in the standard xkb modifier mask.
                state.overlay.shift_held = mods_depressed & 0x01 != 0;
            }
            _ => {}
        }
    }
}

// ── wl_shm_pool ───────────────────────────────────────────────

impl Dispatch<WlShmPool, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WlShmPool,
        _event: <WlShmPool as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// ── wl_buffer ─────────────────────────────────────────────────

impl Dispatch<WlBuffer, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WlBuffer,
        _event: <WlBuffer as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}
