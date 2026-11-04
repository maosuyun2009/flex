//! Chrome palette and metrics, following Pebrel's dark UI: near-black surfaces,
//! one accent, hairline borders, generous rounding on the chips.

use gpui::prelude::*;
use gpui::{px, Hsla, Pixels};

pub const BG: u32 = 0x14161a; // window body, behind the panes
pub const SURFACE: u32 = 0x1a1c21; // title bar and sidebar
pub const HOVER: u32 = 0x24272e;
pub const ACTIVE: u32 = 0x2b2f37; // selected tab chip
pub const BORDER: u32 = 0x24262c;
pub const TEXT: u32 = 0xc9ced8;
pub const MUTED: u32 = 0x78808f;
pub const ACCENT: u32 = 0xf0b429; // the star in Pebrel's title bar
pub const DANGER: u32 = 0xc4564f; // close button hover

pub const TITLE_H: Pixels = px(36.0);
pub const TAB_H: Pixels = px(34.0);
pub const SIDEBAR_W: Pixels = px(208.0);

pub fn color(v: u32) -> Hsla {
    gpui::rgb(v).into()
}

/// Uppercase section label, as in Pebrel's sidebar.
pub fn section(text: impl Into<gpui::SharedString>) -> gpui::Div {
    gpui::div()
        .px_3()
        .pt_3()
        .pb_1()
        .text_xs()
        .text_color(color(MUTED))
        .child(text.into())
}
