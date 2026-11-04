//! Which font the terminals use.
//!
//! Maple Mono NF is asked for by name; GPUI resolves families through the
//! platform, so a machine without it silently substitutes *something*, possibly
//! a proportional face, which would wreck the grid. Pick from the installed list
//! instead, and fall back to the system monospace.

use gpui::{App, SharedString};

/// Best first. The installed family name of a Nerd Font build includes the
/// variant, which is why "Normal" appears here.
const PREFERRED: &[&str] = &[
    "Maple Mono Normal NF CN",
    "Maple Mono NF CN",
    "Maple Mono Normal NF",
    "Maple Mono NF",
    "Maple Mono",
    "Cascadia Mono",
    "Consolas",
    "monospace",
];

pub fn resolve(cx: &App) -> SharedString {
    let installed = cx.text_system().all_font_names();
    PREFERRED
        .iter()
        .find(|wanted| {
            installed
                .iter()
                .any(|have| have.eq_ignore_ascii_case(wanted))
        })
        .copied()
        .unwrap_or("monospace")
        .into()
}
