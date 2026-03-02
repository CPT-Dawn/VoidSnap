mod capture;
mod clipboard;
mod config;
mod overlay;
mod wayland;

use std::io::Cursor;

use anyhow::{Context, Result};
use chrono::Local;
use wayland_client::protocol::wl_shm;

fn main() -> Result<()> {
    // ── Phase 1: Load configuration ──────────────────────────
    let cfg = config::load().context("Failed to load VoidSnap configuration")?;

    eprintln!(
        "voidsnap: saving to {}, clipboard={}, border=#{}, freeze={}",
        cfg.save_directory.display(),
        cfg.copy_to_clipboard,
        hex_color_str(&cfg.border_color),
        cfg.freeze_frame,
    );

    // ── Phase 2: Connect to Wayland & bind globals ───────────
    let (_conn, mut event_queue, mut state, globals) =
        wayland::connect(&cfg).context("Failed to set up Wayland environment")?;

    let qh = event_queue.handle();

    eprintln!(
        "voidsnap: connected — {} output(s) detected",
        state.outputs.len()
    );

    // ── Phase 2.5: Freeze-frame capture (before overlay) ─────
    if cfg.freeze_frame {
        match capture::capture_full_output(&mut event_queue, &mut state, &qh) {
            Ok((pixels, w, h, stride, fmt)) => {
                let argb_buf = convert_to_argb8888(&pixels, w, h, stride, fmt)?;
                state.frozen_buffer = Some(argb_buf);
                state.frozen_width = w;
                state.frozen_height = h;
                eprintln!("voidsnap: freeze-frame captured ({w}×{h})");
            }
            Err(e) => {
                eprintln!("voidsnap: freeze-frame failed, falling back to transparent: {e}");
                // Continue without freeze-frame — non-fatal.
            }
        }
    }

    // ── Phase 3: Create the overlay surface ──────────────────
    overlay::create_overlay(&mut event_queue, &mut state, &qh, &globals)
        .context("Failed to create selection overlay")?;

    eprintln!(
        "voidsnap: overlay ready ({}×{}) — click and drag to select a region",
        state.overlay.width, state.overlay.height
    );

    // ── Phase 4: Event loop — wait for selection ─────────────
    while state.running {
        event_queue
            .blocking_dispatch(&mut state)
            .context("Wayland dispatch error")?;
    }

    // ── Phase 5: Destroy overlay before capture ──────────────
    if let Some(ls) = state.overlay.layer_surface.take() {
        ls.destroy();
    }
    if let Some(s) = state.overlay.surface.take() {
        s.destroy();
    }
    event_queue.roundtrip(&mut state)?;

    if !state.overlay.selection.done {
        eprintln!("voidsnap: selection cancelled");
        return Ok(());
    }

    let (sx, sy, sw, sh) = state.overlay.selection.rect();
    eprintln!("voidsnap: capturing region ({sx}, {sy}) {sw}×{sh}");

    // ── Phase 6: Obtain final pixels ─────────────────────────
    // If we have a frozen buffer, crop directly — no second screencopy needed.
    let (rgba_pixels, final_w, final_h) = if let Some(ref frozen) = state.frozen_buffer {
        let fw = state.frozen_width as usize;
        let fh = state.frozen_height as usize;
        let cx0 = (sx as usize).min(fw);
        let cy0 = (sy as usize).min(fh);
        let cx1 = ((sx + sw) as usize).min(fw);
        let cy1 = ((sy + sh) as usize).min(fh);
        let crop_w = cx1 - cx0;
        let crop_h = cy1 - cy0;

        // The frozen buffer is in ARGB8888 (u32). Convert the crop to RGBA8.
        let mut rgba = Vec::with_capacity(crop_w * crop_h * 4);
        for row in cy0..cy1 {
            for col in cx0..cx1 {
                let px = frozen[row * fw + col];
                let r = ((px >> 16) & 0xFF) as u8;
                let g = ((px >> 8) & 0xFF) as u8;
                let b = (px & 0xFF) as u8;
                let a = ((px >> 24) & 0xFF) as u8;
                rgba.push(r);
                rgba.push(g);
                rgba.push(b);
                rgba.push(a);
            }
        }
        (rgba, crop_w as u32, crop_h as u32)
    } else {
        // No freeze-frame: capture from the live output.
        let (pixels, buf_w, buf_h, buf_stride, fmt) =
            capture::capture_region(&mut event_queue, &mut state, &qh, sx, sy, sw, sh)
                .context("Screen capture failed")?;
        let rgba = convert_to_rgba(&pixels, buf_w, buf_h, buf_stride, fmt)?;
        (rgba, buf_w, buf_h)
    };

    // Drop the frozen buffer — no longer needed.
    state.frozen_buffer = None;

    // ── Phase 7: Encode to PNG ───────────────────────────────
    let img: image::ImageBuffer<image::Rgba<u8>, Vec<u8>> =
        image::ImageBuffer::from_raw(final_w, final_h, rgba_pixels)
            .context("Failed to create image buffer from raw pixels")?;

    // Pre-allocate ~50% of raw size as a heuristic for PNG compressed size.
    let raw_size = (final_w * final_h * 4) as usize;
    let mut png_bytes: Vec<u8> = Vec::with_capacity(raw_size / 2);
    img.write_to(&mut Cursor::new(&mut png_bytes), image::ImageFormat::Png)
        .context("PNG encoding failed")?;

    // ── Phase 8: Save to disk ────────────────────────────────
    let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S");
    let filename = format!("{}_voidsnap.png", timestamp);
    let filepath = cfg.save_directory.join(&filename);

    std::fs::write(&filepath, &png_bytes)
        .with_context(|| format!("Failed to write screenshot to {}", filepath.display()))?;

    eprintln!("voidsnap: saved → {}", filepath.display());

    // ── Phase 9: Copy to clipboard (if enabled) ──────────────
    if cfg.copy_to_clipboard {
        clipboard::copy_png_to_clipboard(png_bytes).context("Clipboard copy failed")?;
        eprintln!("voidsnap: copied to clipboard");
    }

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────

/// Format `[r, g, b, a]` back into a hex string like `"A78BFA"`.
fn hex_color_str(c: &[u8; 4]) -> String {
    format!("{:02X}{:02X}{:02X}", c[0], c[1], c[2])
}

/// Convert screencopy pixels to ARGB8888 u32 values (for freeze-frame buffer).
///
/// Strips stride padding and normalises all supported wl_shm formats into a
/// contiguous `Vec<u32>` of `width * height` elements in 0xAARRGGBB order.
fn convert_to_argb8888(
    raw: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
) -> Result<Vec<u32>> {
    let mut out = Vec::with_capacity((width * height) as usize);

    for y in 0..height {
        for x in 0..width {
            let offset = (y * stride + x * 4) as usize;
            if offset + 4 > raw.len() {
                anyhow::bail!("Pixel data underflow at ({x}, {y})");
            }

            // Memory layout on little-endian:
            // ARGB8888: bytes [B, G, R, A] → u32 = 0xAARRGGBB
            // XRGB8888: bytes [B, G, R, X] → u32 = 0xXXRRGGBB (force alpha=255)
            let pixel = match format {
                wl_shm::Format::Argb8888 => u32::from_le_bytes([
                    raw[offset],
                    raw[offset + 1],
                    raw[offset + 2],
                    raw[offset + 3],
                ]),
                wl_shm::Format::Xrgb8888 => {
                    u32::from_le_bytes([raw[offset], raw[offset + 1], raw[offset + 2], 0xFF])
                }
                wl_shm::Format::Abgr8888 => {
                    // LE bytes [R, G, B, A] — need to swap R↔B
                    let r = raw[offset];
                    let g = raw[offset + 1];
                    let b = raw[offset + 2];
                    let a = raw[offset + 3];
                    argb_u32(a, r, g, b)
                }
                wl_shm::Format::Xbgr8888 => {
                    let r = raw[offset];
                    let g = raw[offset + 1];
                    let b = raw[offset + 2];
                    argb_u32(0xFF, r, g, b)
                }
                _ => anyhow::bail!("Unsupported SHM pixel format: {:?}", format),
            };
            out.push(pixel);
        }
    }

    Ok(out)
}

/// Pack into ARGB u32 manually.
#[inline(always)]
fn argb_u32(a: u8, r: u8, g: u8, b: u8) -> u32 {
    (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// Convert the screencopy pixel buffer to RGBA8 byte order (for PNG encoding).
///
/// Wayland SHM formats are defined as the byte order on the wire:
/// - `Xrgb8888` / `Argb8888`: on little-endian, memory layout is [B, G, R, A/X].
fn convert_to_rgba(
    raw: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity((width * height * 4) as usize);

    for y in 0..height {
        for x in 0..width {
            let offset = (y * stride + x * 4) as usize;
            if offset + 4 > raw.len() {
                anyhow::bail!("Pixel data underflow at ({x}, {y})");
            }

            let (r, g, b, a) = match format {
                wl_shm::Format::Argb8888 => (
                    raw[offset + 2],
                    raw[offset + 1],
                    raw[offset],
                    raw[offset + 3],
                ),
                wl_shm::Format::Xrgb8888 => (raw[offset + 2], raw[offset + 1], raw[offset], 255),
                wl_shm::Format::Abgr8888 => (
                    raw[offset],
                    raw[offset + 1],
                    raw[offset + 2],
                    raw[offset + 3],
                ),
                wl_shm::Format::Xbgr8888 => (raw[offset], raw[offset + 1], raw[offset + 2], 255),
                _ => anyhow::bail!("Unsupported SHM pixel format: {:?}", format),
            };

            out.push(r);
            out.push(g);
            out.push(b);
            out.push(a);
        }
    }

    Ok(out)
}
