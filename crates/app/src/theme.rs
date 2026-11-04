//! Chrome palette and metrics, following Pebrel's dark UI: near-black surfaces,
//! one accent, hairline borders, generous rounding on the chips.
//!
//! The surfaces carry alpha so the window can be acrylic; `Settings::acrylic`
//! switches the window between blurred and opaque, and these values work either
//! way because they are composited over whatever is behind.

use gpui::prelude::*;
use gpui::{px, Hsla, Pixels};

pub const BG: u32 = 0x14161a; // window body, behind the panes
pub const SURFACE: u32 = 0x1a1c21; // title bar and sidebar
pub const HOVER: u32 = 0x24272e;
pub const ACTIVE: u32 = 0x2b2f37; // selected tab chip
pub const BORDER: u32 = 0x24262c;
pub const TEXT: u32 = 0xc9ced8;
pub const MUTED: u32 = 0x78808f;
pub const ACCENT: u32 = 0xf0b429; // the mark in the title bar
pub const DANGER: u32 = 0xc4564f; // close button hover

pub const TITLE_H: Pixels = px(36.0);
pub const SIDEBAR_W: Pixels = px(208.0);

fn translucent(v: u32, alpha: f32) -> Hsla {
    let c: Hsla = gpui::rgb(v).into();
    Hsla { a: alpha, ..c }
}

pub fn color(v: u32) -> Hsla {
    gpui::rgb(v).into()
}

pub fn bg() -> Hsla {
    color(BG)
}
pub fn hover() -> Hsla {
    color(HOVER)
}

/// The chrome in one of two finishes.
///
/// This is the shell's own glass, not DWM's: GPUI's Windows window is
/// `WS_EX_NOREDIRECTIONBITMAP` over DirectComposition, so there is no
/// redirection surface for DWM to frost. `Blurred` (the legacy accent policy)
/// blacks out screen capture on this driver, and `DWMWA_SYSTEMBACKDROP_TYPE`
/// composites nothing, so a translucent window just renders over black.
/// Instead the window stays opaque and the surfaces are layered over a
/// gradient, which reads as frosted glass and behaves everywhere.
#[derive(Clone, Copy)]
pub struct Chrome {
    pub glass: bool,
}

impl Chrome {
    pub fn surface(&self) -> Hsla {
        if self.glass {
            translucent(SURFACE, 0.62)
        } else {
            color(SURFACE)
        }
    }
    pub fn hover(&self) -> Hsla {
        if self.glass {
            translucent(HOVER, 0.72)
        } else {
            color(HOVER)
        }
    }
    pub fn active(&self) -> Hsla {
        if self.glass {
            translucent(ACTIVE, 0.8)
        } else {
            color(ACTIVE)
        }
    }
}
pub fn border() -> Hsla {
    translucent(BORDER, 0.9)
}
pub fn text() -> Hsla {
    color(TEXT)
}
pub fn muted() -> Hsla {
    color(MUTED)
}
pub fn accent() -> Hsla {
    color(ACCENT)
}
pub fn danger() -> Hsla {
    color(DANGER)
}

/// Uppercase section label, as in Pebrel's sidebar.
pub fn section(text: impl Into<gpui::SharedString>) -> gpui::Div {
    gpui::div()
        .px_3()
        .pt_3()
        .pb_1()
        .text_xs()
        .text_color(muted())
        .child(text.into())
}
