use anyhow::{Context, Result};
use wl_clipboard_rs::copy::{MimeType, Options, Source};

/// Copy the given PNG-encoded bytes into the Wayland clipboard.
///
/// Uses the `wl-clipboard-rs` crate which speaks the `ext-data-control` /
/// `wlr-data-control` protocol natively — no external binaries involved.
///
/// The clipboard content remains available until another application replaces it.
/// We set `serve_requests` to `ServeRequests::Only(1)` is not exposed easily,
/// so we use `Options::copy` which by default forks a background thread to serve
/// paste requests.
pub fn copy_png_to_clipboard(png_bytes: Vec<u8>) -> Result<()> {
    let opts = Options::new();
    opts.copy(
        Source::Bytes(png_bytes.into_boxed_slice()),
        MimeType::Specific("image/png".to_string()),
    )
    .context("Failed to copy image to Wayland clipboard")?;

    Ok(())
}
