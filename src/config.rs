use anyhow::{Context, Result};
use directories::BaseDirs;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

/// The default configuration file, baked into the binary at compile time.
const DEFAULT_CONFIG: &str = include_str!("default_config.toml");

// ── Config Struct ─────────────────────────────────────────────

/// Strongly-typed representation of `config.toml`.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// Directory where screenshots are saved (~ is expanded at runtime).
    pub save_directory: String,

    /// Whether to inject PNG bytes into the Wayland clipboard after saving.
    pub copy_to_clipboard: bool,

    /// CSS hex color string for the selection rectangle border.
    pub selection_border_color: String,

    /// Border thickness in pixels.
    #[serde(default = "default_border_width")]
    pub selection_border_width: u32,

    /// Overlay tint color (CSS hex).
    #[serde(default = "default_overlay_color")]
    pub overlay_color: String,

    /// Overlay opacity when a selection is active (0.0–1.0).
    #[serde(default = "default_overlay_opacity")]
    pub overlay_opacity: f64,

    /// Overlay opacity before any selection starts (0.0–1.0).
    #[serde(default = "default_overlay_idle_opacity")]
    pub overlay_idle_opacity: f64,

    /// Whether to capture a freeze-frame before showing the overlay.
    #[serde(default = "default_true")]
    pub freeze_frame: bool,

    /// Whether to show a live WxH dimension label.
    #[serde(default = "default_true")]
    pub show_dimensions: bool,
}

fn default_border_width() -> u32 {
    3
}
fn default_overlay_color() -> String {
    "#0D0B14".to_string()
}
fn default_overlay_opacity() -> f64 {
    0.70
}
fn default_overlay_idle_opacity() -> f64 {
    0.40
}
fn default_true() -> bool {
    true
}

/// The parsed, runtime-ready configuration with fully resolved paths and
/// the border color decoded into RGBA bytes.
#[derive(Debug)]
pub struct ResolvedConfig {
    /// Fully expanded, absolute path to the save directory (guaranteed to exist).
    pub save_directory: PathBuf,

    /// Whether to copy the screenshot to the Wayland clipboard.
    pub copy_to_clipboard: bool,

    /// Selection border color as `[r, g, b, a]` (0–255 each, alpha always 255).
    pub border_color: [u8; 4],

    /// Selection border thickness in pixels.
    pub border_width: u32,

    /// Overlay tint color as `[r, g, b]`.
    pub overlay_color: [u8; 3],

    /// Overlay opacity (0.0–1.0) when selecting.
    pub overlay_opacity: f64,

    /// Overlay idle opacity (0.0–1.0) before selection.
    pub overlay_idle_opacity: f64,

    /// Whether to capture a freeze-frame.
    pub freeze_frame: bool,

    /// Whether to show live dimensions label.
    pub show_dimensions: bool,
}

// ── Bootloader ────────────────────────────────────────────────

/// Load (or bootstrap) the configuration from disk.
///
/// 1. Resolve `~/.config/voidsnap/config.toml` via XDG.
/// 2. If the directory or file is missing, create it with the embedded default.
/// 3. Parse TOML into [`Config`], then resolve paths and colors into
///    [`ResolvedConfig`].
pub fn load() -> Result<ResolvedConfig> {
    let base_dirs = BaseDirs::new().context("Failed to determine XDG base directories")?;

    // ~/.config/voidsnap/
    let config_dir = base_dirs.config_dir().join("voidsnap");
    let config_path = config_dir.join("config.toml");

    // Bootstrap: create directory tree + default config if missing.
    if !config_path.exists() {
        fs::create_dir_all(&config_dir).with_context(|| {
            format!(
                "Failed to create config directory: {}",
                config_dir.display()
            )
        })?;

        fs::write(&config_path, DEFAULT_CONFIG).with_context(|| {
            format!(
                "Failed to write default config to: {}",
                config_path.display()
            )
        })?;

        eprintln!(
            "voidsnap: created default config at {}",
            config_path.display()
        );
    }

    // Read and parse.
    let raw = fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config: {}", config_path.display()))?;

    let config: Config =
        toml::from_str(&raw).with_context(|| "Failed to parse config.toml — check syntax")?;

    resolve(config, base_dirs)
}

// ── Resolution helpers ────────────────────────────────────────

/// Expand `~` at the start of a path to the user's home directory.
fn expand_tilde(path: &str, home: &Path) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else if path == "~" {
        home.to_path_buf()
    } else {
        PathBuf::from(path)
    }
}

/// Parse a CSS hex color string (`#RRGGBB`) into `[R, G, B, 255]`.
fn parse_hex_color(hex: &str) -> Result<[u8; 4]> {
    let hex = hex.trim_start_matches('#');
    anyhow::ensure!(
        hex.len() == 6,
        "Invalid color '#{hex}': expected 6 hex digits (e.g. #A78BFA)"
    );
    let r = u8::from_str_radix(&hex[0..2], 16)?;
    let g = u8::from_str_radix(&hex[2..4], 16)?;
    let b = u8::from_str_radix(&hex[4..6], 16)?;
    Ok([r, g, b, 255])
}

/// Turn a raw [`Config`] into a fully resolved [`ResolvedConfig`].
fn resolve(config: Config, base_dirs: BaseDirs) -> Result<ResolvedConfig> {
    let home = base_dirs.home_dir();

    // Expand ~ and ensure the save directory exists.
    let save_directory = expand_tilde(&config.save_directory, home);
    fs::create_dir_all(&save_directory).with_context(|| {
        format!(
            "Failed to create save directory: {}",
            save_directory.display()
        )
    })?;

    // Parse the border color.
    let border_color = parse_hex_color(&config.selection_border_color)?;

    // Parse the overlay tint color (RGB only).
    let overlay_rgba = parse_hex_color(&config.overlay_color)?;
    let overlay_color = [overlay_rgba[0], overlay_rgba[1], overlay_rgba[2]];

    // Clamp opacities.
    let overlay_opacity = config.overlay_opacity.clamp(0.0, 1.0);
    let overlay_idle_opacity = config.overlay_idle_opacity.clamp(0.0, 1.0);

    Ok(ResolvedConfig {
        save_directory,
        copy_to_clipboard: config.copy_to_clipboard,
        border_color,
        border_width: config.selection_border_width.max(1),
        overlay_color,
        overlay_opacity,
        overlay_idle_opacity,
        freeze_frame: config.freeze_frame,
        show_dimensions: config.show_dimensions,
    })
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_tilde() {
        let home = Path::new("/home/alice");
        assert_eq!(
            expand_tilde("~/Pictures/Screenshots/", home),
            PathBuf::from("/home/alice/Pictures/Screenshots/")
        );
        assert_eq!(expand_tilde("~", home), PathBuf::from("/home/alice"));
        assert_eq!(
            expand_tilde("/absolute/path", home),
            PathBuf::from("/absolute/path")
        );
    }

    #[test]
    fn test_parse_hex_color() {
        assert_eq!(parse_hex_color("#A78BFA").unwrap(), [0xA7, 0x8B, 0xFA, 255]);
        assert_eq!(parse_hex_color("#000000").unwrap(), [0, 0, 0, 255]);
        assert_eq!(parse_hex_color("#FFFFFF").unwrap(), [255, 255, 255, 255]);
    }

    #[test]
    fn test_parse_hex_color_invalid() {
        assert!(parse_hex_color("#FFF").is_err());
        assert!(parse_hex_color("ZZZZZZ").is_err());
    }

    #[test]
    fn test_default_config_parses() {
        let config: Config = toml::from_str(DEFAULT_CONFIG)
            .expect("Embedded default_config.toml must be valid TOML");
        assert_eq!(config.save_directory, "~/Pictures/Screenshots/");
        assert!(config.copy_to_clipboard);
        assert_eq!(config.selection_border_color, "#00FFFF");
        assert_eq!(config.selection_border_width, 3);
        assert_eq!(config.overlay_color, "#0D0B14");
        assert!((config.overlay_opacity - 0.70).abs() < f64::EPSILON);
        assert!((config.overlay_idle_opacity - 0.40).abs() < f64::EPSILON);
        assert!(config.freeze_frame);
        assert!(config.show_dimensions);
    }
}
