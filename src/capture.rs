use std::os::fd::AsFd;

use anyhow::{Context, Result};
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;
use nix::unistd::ftruncate;
use wayland_client::protocol::wl_shm;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{
    self, ZwlrScreencopyFrameV1,
};

use crate::wayland::AppState;

// ── Screencopy frame state ────────────────────────────────────

/// Per-frame state for the screencopy buffer negotiation.
/// Stored directly in [`AppState`] so the dispatch handler can mutate it.
#[derive(Debug, Default)]
pub struct CaptureFrame {
    /// Format + dimensions advertised by the compositor.
    pub format: Option<wl_shm::Format>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,

    /// Whether the frame is ready (pixels written).
    pub ready: bool,
    /// Whether the frame capture failed.
    pub failed: bool,
}

// ── Capture logic ─────────────────────────────────────────────

/// Capture the full output (no region) for freeze-frame mode.
///
/// Returns the same tuple as [`capture_region`].
pub fn capture_full_output(
    event_queue: &mut wayland_client::EventQueue<AppState>,
    state: &mut AppState,
    qh: &QueueHandle<AppState>,
) -> Result<(Vec<u8>, u32, u32, u32, wl_shm::Format)> {
    let manager = state
        .screencopy_manager
        .as_ref()
        .context("Screencopy manager not bound")?
        .clone();

    let output = state
        .outputs
        .first()
        .context("No output available")?
        .wl_output
        .clone();

    state.capture_frame = CaptureFrame::default();

    let frame = manager.capture_output(0, &output, qh, ());

    event_queue
        .roundtrip(state)
        .context("Roundtrip failed while negotiating screencopy frame")?;

    let fmt = state
        .capture_frame
        .format
        .context("Compositor did not send buffer format")?;
    let buf_width = state.capture_frame.width;
    let buf_height = state.capture_frame.height;
    let buf_stride = state.capture_frame.stride;
    let buf_size = (buf_stride * buf_height) as usize;

    let shm_name = format!("/voidsnap-freeze-{}", std::process::id());
    let fd = nix::sys::mman::shm_open(
        shm_name.as_str(),
        OFlag::O_CREAT | OFlag::O_RDWR | OFlag::O_TRUNC,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )
    .context("shm_open for freeze buffer failed")?;
    let _ = nix::sys::mman::shm_unlink(shm_name.as_str());

    ftruncate(&fd, buf_size as i64).context("ftruncate on freeze SHM fd failed")?;

    let mmap = unsafe {
        memmap2::MmapOptions::new()
            .len(buf_size)
            .map_mut(&fd)
            .context("mmap of freeze SHM buffer failed")?
    };

    let shm = state.shm.as_ref().context("wl_shm not bound")?;
    let pool = shm.create_pool(fd.as_fd(), buf_size as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        buf_width as i32,
        buf_height as i32,
        buf_stride as i32,
        fmt,
        qh,
        (),
    );

    frame.copy(&buffer);

    const MAX_ROUNDTRIPS: usize = 100;
    for attempt in 0..MAX_ROUNDTRIPS {
        event_queue
            .roundtrip(state)
            .context("Roundtrip failed during freeze capture")?;

        if state.capture_frame.failed {
            anyhow::bail!("Freeze-frame capture failed");
        }
        if state.capture_frame.ready {
            break;
        }
        if attempt == MAX_ROUNDTRIPS - 1 {
            anyhow::bail!("Freeze-frame timed out");
        }
    }

    let pixels = mmap[..buf_size].to_vec();

    buffer.destroy();
    pool.destroy();
    frame.destroy();

    Ok((pixels, buf_width, buf_height, buf_stride, fmt))
}

/// Capture the selected region of the given output via wlr-screencopy.
///
/// Returns the raw pixel buffer (format as reported by the compositor,
/// typically XRGB8888 or ARGB8888) along with width, height, stride, and format.
pub fn capture_region(
    event_queue: &mut wayland_client::EventQueue<AppState>,
    state: &mut AppState,
    qh: &QueueHandle<AppState>,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<(Vec<u8>, u32, u32, u32, wl_shm::Format)> {
    let manager = state
        .screencopy_manager
        .as_ref()
        .context("Screencopy manager not bound")?
        .clone();

    let output = state
        .outputs
        .first()
        .context("No output available")?
        .wl_output
        .clone();

    // Reset the capture frame state.
    state.capture_frame = CaptureFrame::default();

    // Request a frame for the selected region (overlay_cursor = false → don't include cursor).
    let frame = manager.capture_output_region(
        0, // overlay_cursor: 0 = no, 1 = yes
        &output,
        x,
        y,
        width,
        height,
        qh,
        (), // user data — we use AppState.capture_frame instead
    );

    // Roundtrip to get the buffer event with format info.
    event_queue
        .roundtrip(state)
        .context("Roundtrip failed while negotiating screencopy frame")?;

    // Read back the negotiated format from AppState.
    let fmt = state
        .capture_frame
        .format
        .context("Compositor did not send buffer format")?;
    let buf_width = state.capture_frame.width;
    let buf_height = state.capture_frame.height;
    let buf_stride = state.capture_frame.stride;
    let buf_size = (buf_stride * buf_height) as usize;

    // Allocate SHM for the capture buffer.
    let shm_name = format!("/voidsnap-capture-{}", std::process::id());
    let fd = nix::sys::mman::shm_open(
        shm_name.as_str(),
        OFlag::O_CREAT | OFlag::O_RDWR | OFlag::O_TRUNC,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )
    .context("shm_open for capture buffer failed")?;
    let _ = nix::sys::mman::shm_unlink(shm_name.as_str());

    ftruncate(&fd, buf_size as i64).context("ftruncate on capture SHM fd failed")?;

    let mmap = unsafe {
        memmap2::MmapOptions::new()
            .len(buf_size)
            .map_mut(&fd)
            .context("mmap of capture SHM buffer failed")?
    };

    // Create wl_shm_pool + wl_buffer.
    let shm = state.shm.as_ref().context("wl_shm not bound")?;
    let pool = shm.create_pool(fd.as_fd(), buf_size as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        buf_width as i32,
        buf_height as i32,
        buf_stride as i32,
        fmt,
        qh,
        (),
    );

    // Tell the compositor to copy pixels into our buffer.
    frame.copy(&buffer);

    // Roundtrip until the frame is ready or failed (with timeout guard).
    const MAX_ROUNDTRIPS: usize = 100;
    for attempt in 0..MAX_ROUNDTRIPS {
        event_queue
            .roundtrip(state)
            .context("Roundtrip failed during screencopy")?;

        if state.capture_frame.failed {
            anyhow::bail!("Screencopy capture failed — compositor reported an error");
        }
        if state.capture_frame.ready {
            break;
        }
        if attempt == MAX_ROUNDTRIPS - 1 {
            anyhow::bail!(
                "Screencopy timed out after {MAX_ROUNDTRIPS} roundtrips — \
                 compositor never sent Ready or Failed"
            );
        }
    }

    // Copy the pixels out.
    let pixels = mmap[..buf_size].to_vec();

    // Clean up the wl objects.
    buffer.destroy();
    pool.destroy();
    frame.destroy();

    Ok((pixels, buf_width, buf_height, buf_stride, fmt))
}

// ── Dispatch for screencopy frame ─────────────────────────────

impl Dispatch<ZwlrScreencopyFrameV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrScreencopyFrameV1,
        event: <ZwlrScreencopyFrameV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                if let wayland_client::WEnum::Value(f) = format {
                    state.capture_frame.format = Some(f);
                }
                state.capture_frame.width = width;
                state.capture_frame.height = height;
                state.capture_frame.stride = stride;
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.capture_frame.ready = true;
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.capture_frame.failed = true;
            }
            _ => {}
        }
    }
}
