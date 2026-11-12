//! What the settings page changes, and where it is kept.
//!
//! Values live in the engine's store, which persists them next to the saved
//! server list, so the shell opens the way it was left.

use flex_engine::Engine;
use serde_json::json;

#[derive(Clone, Debug)]
pub struct Settings {
    pub font_size: f32,
    pub sidebar: bool,
    pub acrylic: bool,
    /// A picture drawn behind the terminal text, if any.
    pub background: Option<String>,
    /// How much of it shows through, 0..1. The terminal's own background is
    /// painted over the picture at this opacity, so higher means more picture.
    pub background_opacity: f32,
    /// How many lines above the screen scrolling may reach. The engine keeps a
    /// fixed 16 MiB of scrollback; this caps how much of it the view exposes.
    pub scrollback_lines: u64,
    /// Explicit consent before other clients may drive this window.
    pub allow_remote_actions: bool,
    /// Programs on the server (OSC 52) and other clients may set the local
    /// clipboard. Each write is announced; turning this off refuses them.
    pub remote_clipboard: bool,
    pub files_open: bool,
    pub files_width: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_size: 14.0,
            sidebar: true,
            acrylic: true,
            background: None,
            background_opacity: 0.35,
            scrollback_lines: 10_000,
            allow_remote_actions: false,
            remote_clipboard: true,
            files_open: false,
            files_width: 320.0,
        }
    }
}

/// Keep at least 260 logical pixels for the terminal on a narrow window.
pub fn file_panel_width(requested: f32, window_width: f32, sidebar_width: f32) -> f32 {
    let requested = if requested.is_finite() {
        requested
    } else {
        320.0
    };
    requested
        .clamp(180.0, 520.0)
        .min((window_width - sidebar_width - 260.0).max(0.0))
}

#[cfg(test)]
mod file_panel_tests {
    use super::*;
    #[test]
    fn panel_never_covers_the_terminal_in_a_narrow_window() {
        assert_eq!(file_panel_width(320.0, 720.0, 208.0), 252.0);
        assert_eq!(file_panel_width(400.0, 1200.0, 208.0), 400.0);
        assert_eq!(file_panel_width(800.0, 1600.0, 0.0), 520.0);
        assert_eq!(file_panel_width(320.0, 200.0, 208.0), 0.0);
        assert_eq!(file_panel_width(f32::NAN, 1200.0, 208.0), 320.0);
    }
}

pub const FONT_MIN: f32 = 9.0;
pub const FONT_MAX: f32 = 28.0;

pub const SCROLL_MIN: u64 = 500;
pub const SCROLL_MAX: u64 = 100_000;

impl Settings {
    pub fn load(engine: &Engine) -> Self {
        let store = engine.shrew();
        // Settings saved before the rename still count, so nobody loses their
        // font size to a name change.
        let setting = |key: &str| {
            store
                .setting(key)
                .or_else(|| store.setting(&key.replace("flex.", "rexshell.")))
        };
        let number = |key: &str| setting(key).and_then(|v| v.as_f64()).map(|v| v as f32);
        let flag = |key: &str| setting(key).and_then(|v| v.as_bool());
        let default = Self::default();
        Self {
            font_size: number("flex.font_size")
                .unwrap_or(default.font_size)
                .clamp(FONT_MIN, FONT_MAX),
            sidebar: flag("flex.sidebar").unwrap_or(default.sidebar),
            acrylic: flag("flex.acrylic").unwrap_or(default.acrylic),
            allow_remote_actions: flag("flex.allow_remote_actions").unwrap_or(false),
            remote_clipboard: flag("flex.remote_clipboard").unwrap_or(default.remote_clipboard),
            files_open: flag("flex.files_open").unwrap_or(false),
            files_width: number("flex.files_width")
                .filter(|w| w.is_finite())
                .unwrap_or(320.0)
                .clamp(180.0, 520.0),
            background: setting("flex.background").and_then(|v| v.as_str().map(str::to_owned)),
            background_opacity: number("flex.background_opacity")
                .unwrap_or(default.background_opacity)
                .clamp(0.05, 0.95),
            scrollback_lines: setting("flex.scrollback_lines")
                .and_then(|v| v.as_u64())
                .unwrap_or(default.scrollback_lines)
                .clamp(SCROLL_MIN, SCROLL_MAX),
        }
    }

    pub fn save(&self, engine: &Engine) {
        let store = engine.shrew();
        store.set_setting("flex.font_size", json!(self.font_size));
        store.set_setting("flex.sidebar", json!(self.sidebar));
        store.set_setting("flex.acrylic", json!(self.acrylic));
        store.set_setting(
            "flex.allow_remote_actions",
            json!(self.allow_remote_actions),
        );
        store.set_setting("flex.remote_clipboard", json!(self.remote_clipboard));
        store.set_setting("flex.files_open", json!(self.files_open));
        store.set_setting("flex.files_width", json!(self.files_width));
        store.set_setting(
            "flex.background",
            match &self.background {
                Some(path) => json!(path),
                None => json!(null),
            },
        );
        store.set_setting("flex.background_opacity", json!(self.background_opacity));
        store.set_setting("flex.scrollback_lines", json!(self.scrollback_lines));
    }
}
