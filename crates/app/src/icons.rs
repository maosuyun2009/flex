//! Icons, drawn from divs rather than shipped as assets.
//!
//! GPUI can render SVGs, but that needs an `AssetSource` and a bundled file per
//! icon. Every icon the shell needs is a line or a box, so painting them from
//! primitives is cheaper, always renders, and keeps the single-exe story true.
//! (GPUI 0.2.2 has no rotation, so the cross is the `×` character, which does
//! exist in the Windows fallback chain.)

use gpui::prelude::*;
use gpui::{div, px, AnyElement, Hsla, Pixels};

use crate::theme;

#[derive(Clone, Copy)]
pub enum Icon {
    /// Sidebar: a frame with a divider.
    Sidebar,
    /// Settings: sliders.
    Sliders,
    /// Split the pane, dividing it left/right.
    SplitRight,
    /// Split the pane, dividing it top/bottom.
    SplitDown,
    /// Floating pane: two offset frames.
    Float,
    /// Close: a cross.
    Close,
    /// New tab: a plus.
    Plus,
}

pub fn icon(kind: Icon, size: Pixels, color: Hsla) -> AnyElement {
    let inner = size - px(2.0);
    let line = |w: Pixels, h: Pixels| div().w(w).h(h).bg(color);
    let body = match kind {
        Icon::Sidebar => div()
            .relative()
            .size(size)
            .border_1()
            .border_color(color)
            .rounded_sm()
            .child(
                div()
                    .absolute()
                    .left(size * 0.4)
                    .top(px(1.0))
                    .w(px(1.0))
                    .h(inner)
                    .bg(color),
            ),
        Icon::Sliders => div()
            .size(size)
            .flex()
            .flex_col()
            .justify_center()
            .gap(px(3.0))
            .children([0.55_f32, 0.85, 0.7].map(|fraction| line(size * fraction, px(1.0)))),
        Icon::SplitRight => div()
            .relative()
            .size(size)
            .border_1()
            .border_color(color)
            .rounded_sm()
            .child(
                div()
                    .absolute()
                    .left(size * 0.5)
                    .top(px(1.0))
                    .w(px(1.0))
                    .h(inner)
                    .bg(color),
            ),
        Icon::SplitDown => div()
            .relative()
            .size(size)
            .border_1()
            .border_color(color)
            .rounded_sm()
            .child(
                div()
                    .absolute()
                    .top(size * 0.5)
                    .left(px(1.0))
                    .h(px(1.0))
                    .w(inner)
                    .bg(color),
            ),
        Icon::Float => div()
            .relative()
            .size(size)
            .child(
                div()
                    .absolute()
                    .left(px(0.0))
                    .bottom(px(0.0))
                    .size(size * 0.66)
                    .border_1()
                    .border_color(color),
            )
            .child(
                div()
                    .absolute()
                    .right(px(0.0))
                    .top(px(0.0))
                    .size(size * 0.66)
                    .border_1()
                    .border_color(color),
            ),
        Icon::Close => div()
            .size(size)
            .flex()
            .items_center()
            .justify_center()
            .text_size(size * 0.95)
            .text_color(color)
            .child("×"),
        Icon::Plus => div()
            .relative()
            .size(size)
            .child(
                div()
                    .absolute()
                    .top(size * 0.5)
                    .w(size)
                    .h(px(1.0))
                    .bg(color),
            )
            .child(
                div()
                    .absolute()
                    .left(size * 0.5)
                    .h(size)
                    .w(px(1.0))
                    .bg(color),
            ),
    };
    body.into_any_element()
}

/// A square icon button for the chrome. The caller attaches the click, so this
/// can take a `cx.listener` from its own view.
pub fn button(id: &'static str, kind: Icon, active: bool) -> gpui::Stateful<gpui::Div> {
    let tint = if active {
        theme::accent()
    } else {
        theme::muted()
    };
    div()
        .id(id)
        .size(px(26.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_color(tint)
        .hover(|el| el.bg(theme::hover()).text_color(theme::text()))
        .child(icon(kind, px(14.0), tint))
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
}
