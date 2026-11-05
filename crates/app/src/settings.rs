//! What the settings page changes, and where it is kept.
//!
//! Values live in the engine's store, which persists them next to the saved
//! server list, so the shell opens the way it was left.

use rexshell_engine::Engine;
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
        }
    }
}

pub const FONT_MIN: f32 = 9.0;
pub const FONT_MAX: f32 = 28.0;

pub const SCROLL_MIN: u64 = 500;
pub const SCROLL_MAX: u64 = 100_000;

impl Settings {
    pub fn load(engine: &Engine) -> Self {
        let store = engine.shrew();
        let number = |key: &str| {
            store
                .setting(key)
                .and_then(|v| v.as_f64())
                .map(|v| v as f32)
        };
        let flag = |key: &str| store.setting(key).and_then(|v| v.as_bool());
        let default = Self::default();
        Self {
            font_size: number("rexshell.font_size")
                .unwrap_or(default.font_size)
                .clamp(FONT_MIN, FONT_MAX),
            sidebar: flag("rexshell.sidebar").unwrap_or(default.sidebar),
            acrylic: flag("rexshell.acrylic").unwrap_or(default.acrylic),
            background: store
                .setting("rexshell.background")
                .and_then(|v| v.as_str().map(str::to_owned)),
            background_opacity: number("rexshell.background_opacity")
                .unwrap_or(default.background_opacity)
                .clamp(0.05, 0.95),
            scrollback_lines: store
                .setting("rexshell.scrollback_lines")
                .and_then(|v| v.as_u64())
                .unwrap_or(default.scrollback_lines)
                .clamp(SCROLL_MIN, SCROLL_MAX),
        }
    }

    pub fn save(&self, engine: &Engine) {
        let store = engine.shrew();
        store.set_setting("rexshell.font_size", json!(self.font_size));
        store.set_setting("rexshell.sidebar", json!(self.sidebar));
        store.set_setting("rexshell.acrylic", json!(self.acrylic));
        store.set_setting(
            "rexshell.background",
            match &self.background {
                Some(path) => json!(path),
                None => json!(null),
            },
        );
        store.set_setting(
            "rexshell.background_opacity",
            json!(self.background_opacity),
        );
        store.set_setting("rexshell.scrollback_lines", json!(self.scrollback_lines));
    }
}
