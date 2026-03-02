<div align="center">

# VOIDSNAP

**A zero-dependency, pure-Rust screenshot utility for the Wayland ecosystem.**

[![Rust](https://img.shields.io/badge/Rust-1.75%2B-f74c00?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-00FFFF?style=flat-square)](LICENSE)
[![Wayland](https://img.shields.io/badge/Protocol-Wayland-yellow?style=flat-square&logo=wayland&logoColor=white)](https://wayland.freedesktop.org/)
[![Maintenance](https://img.shields.io/badge/Maintained-Actively-brightgreen?style=flat-square)]()

![Demo](assets/demo.gif)

</div>

---

## Philosophy

The Linux screenshot landscape is fragmented. Most solutions are shell script wrappers stitching together `grim`, `slurp`, and `wl-copy` — three separate processes, three separate failure modes, and a pipe-based architecture that breaks silently. Others drag in toolkit dependencies measured in tens of megabytes for a task that should complete in milliseconds.

**VoidSnap rejects this entirely.**

It is a single, statically-linkable Rust binary that handles the complete screenshot lifecycle — overlay rendering, region selection, pixel capture, PNG encoding, and clipboard injection — without spawning a single child process. No `grim`. No `slurp`. No `wl-copy`. No `xclip`. Every byte of work happens inside one address space, using memory-safe Rust and direct Wayland protocol negotiation.

It exists because screenshots are a solved problem that keeps getting solved poorly.

## Features

- **Pure Rust, zero external binaries** — the entire pipeline from Wayland surface creation to clipboard injection is self-contained. `std::process::Command` is never called.
- **Wayland-native** — communicates directly with the compositor via `wlr-layer-shell-unstable-v1` (overlay), `wlr-screencopy-unstable-v1` (pixel capture), and `wlr-data-control` (clipboard).
- **Freeze-frame selection** — captures a full-screen screenshot *before* displaying the overlay. You draw your selection on a static image, so animations, video, and cursor movement don't contaminate the final capture. Crop is instant — no second screencopy needed.
- **Interactive region selection** — full-screen transparent overlay at the `Overlay` z-layer. Click and drag to define a bounding box; the selection is drawn in real time with configurable border color, width, and dimmed surround.
- **Keyboard precision controls** — arrow keys nudge the selection 1px at a time (10px with Shift). `A` selects the full output. `Enter`/`Space` confirms. `Escape` cancels.
- **Live dimension HUD** — a `W×H` label is rendered at 2× scale beside the selection rectangle, updating in real time as you drag.
- **60 FPS vsync-paced rendering** — `wl_surface::frame` callbacks ensure the overlay redraws at compositor refresh rate. Mouse input at 1000 Hz sets a dirty flag; actual redraws are batched to the next vsync.
- **Cosmic Dawn aesthetic** — deep void wash (`#0D0B14`), electric cyan border (`#00FFFF`), dawn-red corner handles (`#FF4500`), 250ms ease-out-cubic fade-in, and a subtle border alpha pulse during active selection.
- **Zero-copy SHM capture** — negotiates shared memory buffers directly with the compositor. Pixel data is `mmap`'d and read in-place.
- **Automatic format handling** — detects and converts `Argb8888`, `Xrgb8888`, `Abgr8888`, and `Xbgr8888` SHM pixel formats.
- **Self-bootstrapping configuration** — a fully commented TOML config is baked into the binary via `include_str!` and written to disk on first run. Zero manual setup.
- **Timestamped output** — screenshots saved as `2026-02-28_14-30-00_voidsnap.png` with automatic directory creation.
- **Native clipboard injection** — PNG bytes are pushed to the Wayland clipboard via `wlr-data-control` using `wl-clipboard-rs`. Paste directly into any application.
- **Dirty-rect damage** — only the union of the old and new selection rectangles is marked for compositor recomposition. No full-surface damage spam.

> [!IMPORTANT]
> VoidSnap requires a Wayland compositor that implements **`wlr-layer-shell-unstable-v1`** and **`wlr-screencopy-unstable-v1`**. This includes **Hyprland**, **Sway**, **river**, **labwc**, and most wlroots-based compositors. It will **not** work on GNOME (Mutter) or KDE (KWin) without additional protocol support.

## Installation

### Building from Source

Requires Rust 1.75+ and a working `cargo` toolchain.

```bash
git clone https://github.com/yourusername/VoidSnap.git
cd VoidSnap
cargo build --release
```

The binary is emitted to `target/release/voidsnap`. Copy it to your `$PATH`:

```bash
sudo cp target/release/voidsnap /usr/local/bin/
```

### Arch Linux (AUR)

```bash
yay -S voidsnap
```

> [!NOTE]
> The AUR package is a placeholder. The exact package name will be updated once published.

## Configuration

VoidSnap uses an **embedded asset bootloader** pattern. A fully commented default configuration is compiled directly into the binary. On first launch, the application:

1. Resolves `$XDG_CONFIG_HOME` (defaulting to `~/.config/`).
2. Checks for `~/.config/voidsnap/config.toml`.
3. If the file or directory is missing, creates the full path and writes the embedded default.
4. Parses the TOML into a strict typed struct before any Wayland connection is established.

**Config location:**

```
~/.config/voidsnap/config.toml
```

**Default contents:**

```toml
# ╔══════════════════════════════════════════════════════════════╗
# ║                    VoidSnap Configuration                    ║
# ╚══════════════════════════════════════════════════════════════╝

save_directory = "~/Pictures/Screenshots/"
copy_to_clipboard = true
selection_border_color = "#00FFFF"
selection_border_width = 3
overlay_color = "#0D0B14"
overlay_opacity = 0.70
overlay_idle_opacity = 0.40
freeze_frame = true
show_dimensions = true
```

| Key | Type | Default | Description |
|---|---|---|---|
| `save_directory` | `String` | `~/Pictures/Screenshots/` | Absolute path for saved PNGs. `~` is expanded at runtime. |
| `copy_to_clipboard` | `bool` | `true` | Inject PNG bytes into the Wayland clipboard after capture. |
| `selection_border_color` | `String` | `#00FFFF` | Hex color for the selection rectangle border. |
| `selection_border_width` | `u32` | `3` | Border thickness in pixels. |
| `overlay_color` | `String` | `#0D0B14` | Tint color for the dimmed overlay region. |
| `overlay_opacity` | `f64` | `0.70` | Opacity of the dim wash when selecting (0.0–1.0). |
| `overlay_idle_opacity` | `f64` | `0.40` | Opacity of the overlay before selection starts. |
| `freeze_frame` | `bool` | `true` | Capture a static frame before the overlay appears. |
| `show_dimensions` | `bool` | `true` | Show a live W×H label beside the selection. |

## Usage

### Take a screenshot

```bash
voidsnap
```

A transparent overlay appears over the active output. Click and drag to select a region. Release to capture.

### Controls

| Input | Action |
|---|---|
| **Left-click + drag** | Define selection region |
| **Release** | Capture the selected area, encode to PNG, save to disk |
| **Right-click / Middle-click** | Cancel selection and exit |
| **Escape** | Cancel selection and exit |
| **Enter / Space** | Confirm current selection |
| **Arrow keys** | Nudge selection edge by 1 pixel |
| **Shift + Arrow keys** | Nudge selection edge by 10 pixels |
| **A** | Select entire output |

### Keybinding (Hyprland example)

Add to `~/.config/hypr/hyprland.conf`:

```ini
bind = , Print, exec, voidsnap
bind = SUPER, S, exec, voidsnap
```

### Keybinding (Sway example)

Add to `~/.config/sway/config`:

```ini
bindsym Print exec voidsnap
bindsym $mod+Shift+s exec voidsnap
```

## Architecture

```
main.rs           ─── Entry point: config → connect → freeze-frame → overlay → loop → crop/capture → encode → clipboard
  ├── config.rs   ─── Embedded TOML bootloader, serde struct, ~ expansion, hex parsing (9 config keys)
  ├── wayland.rs  ─── Registry init, global binding (wl_shm, wl_seat, wl_output, screencopy_manager)
  ├── overlay.rs  ─── Layer-shell surface, SHM pixel buffer, pointer + keyboard dispatch,
  │                    Cosmic Dawn renderer, frame-callback pacing, bitmap font HUD, animations
  ├── capture.rs  ─── Full-output + region screencopy, SHM buffer alloc, pixel extraction
  └── clipboard.rs─── wl-clipboard-rs wrapper, image/png MIME injection
```

## Contributing

Contributions are welcome. Please open an issue before submitting large changes.

1. Fork the repository.
2. Create a feature branch: `git checkout -b feat/your-feature`.
3. Commit with clear, imperative messages.
4. Open a pull request against `main`.

All code must pass `cargo clippy` with zero warnings and `cargo test` with all tests passing.

## License

This project is licensed under the [MIT License](LICENSE).

---

<div align="center">
<sub>Built with Rust. For Wayland. Nothing else.</sub>
</div>