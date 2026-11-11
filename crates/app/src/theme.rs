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
// Secondary text. Light enough to read at `text_xs` on every surface it
// lands on (4.5:1 or better on BG, SURFACE and HOVER), still clearly
// quieter than TEXT.
pub const MUTED: u32 = 0x8a92a0;
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
/// Tab selection owns the surface and underline; notifications have a separate marker.
pub struct TabAppearance {
    pub background: Option<Hsla>,
    pub foreground: Hsla,
    pub underline: bool,
    pub bell: bool,
}

pub fn tab_appearance(chrome: Chrome, active: bool, bell: bool) -> TabAppearance {
    TabAppearance {
        background: active.then(|| chrome.active()),
        foreground: if active { text() } else { muted() },
        underline: active,
        bell: !active && bell,
    }
}

#[cfg(test)]
mod tab_tests {
    use super::*;

    /// WCAG contrast ratio between two opaque colours.
    fn contrast(a: u32, b: u32) -> f32 {
        let luminance = |v: u32| {
            let channel = |shift: u32| {
                let c = ((v >> shift) & 0xff) as f32 / 255.0;
                if c <= 0.03928 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
        };
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn secondary_text_is_readable_on_the_surfaces_it_sits_on() {
        for (name, surface) in [("bg", BG), ("surface", SURFACE), ("hover", HOVER)] {
            let ratio = contrast(MUTED, surface);
            assert!(ratio >= 4.5, "muted on {name}: {ratio:.2}");
        }
        // And still plainly secondary next to the primary text.
        assert!(contrast(TEXT, SURFACE) > contrast(MUTED, SURFACE) * 1.5);
    }

    #[test]
    fn background_bell_cannot_look_like_the_selected_tab() {
        for glass in [false, true] {
            let chrome = Chrome { glass };
            let quiet = tab_appearance(chrome, false, false);
            let alert = tab_appearance(chrome, false, true);
            assert_eq!(alert.background, quiet.background);
            assert_eq!(alert.foreground, quiet.foreground);
            assert!(!alert.underline);
            assert!(alert.bell);
            assert!(!quiet.bell);
            let selected = tab_appearance(chrome, true, true);
            assert_eq!(selected.background, Some(chrome.active()));
            assert_eq!(selected.foreground, text());
            assert!(selected.underline);
            assert!(!selected.bell);
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

/// The dot beside a session or tab, from the program-status protocol: white
/// idle, green working, red blocked, amber failed, blue finished.
pub fn agent(state: shrew_core::AgentState) -> Hsla {
    use shrew_core::AgentState;
    color(match state {
        AgentState::Idle => 0xf2f5f9,
        AgentState::Working => 0x4e_c9_7b,
        AgentState::Blocked => 0xe0_5b_4f,
        AgentState::Error => 0xe0_a0_4f,
        AgentState::Done => 0x6e_a8_d8,
    })
}

pub struct Hint(pub &'static str);

/// What a control does, shown when the pointer rests on it: an icon alone
/// does not say, and the key that does the same is worth learning.
pub fn hint<E: gpui::StatefulInteractiveElement>(element: E, text: &'static str) -> E {
    element.tooltip(move |_, cx| cx.new(|_| Hint(text)).into())
}

impl gpui::Render for Hint {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        gpui::div()
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(color(SURFACE))
            .border_1()
            .border_color(border())
            .text_xs()
            .text_color(text())
            .child(self.0)
    }
}

/// A 6px status dot.
pub fn dot(colour: Hsla) -> gpui::Div {
    gpui::div().size(px(6.0)).rounded_full().bg(colour)
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
