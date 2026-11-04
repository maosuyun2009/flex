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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_size: 14.0,
            sidebar: true,
            acrylic: true,
        }
    }
}

pub const FONT_MIN: f32 = 9.0;
pub const FONT_MAX: f32 = 28.0;

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
        }
    }

    pub fn save(&self, engine: &Engine) {
        let store = engine.shrew();
        store.set_setting("rexshell.font_size", json!(self.font_size));
        store.set_setting("rexshell.sidebar", json!(self.sidebar));
        store.set_setting("rexshell.acrylic", json!(self.acrylic));
    }
}
