//! Local Files palette and asset-free, stroked icons.
use gpui::prelude::*;
use gpui::{canvas, div, point, px, Hsla, IntoElement, PathBuilder};

pub const BACKGROUND: u32 = 0x1e2030;
pub const SEARCH: u32 = 0x262839;
pub const SELECTED: u32 = 0x36394f;
pub const FOREGROUND: u32 = 0xc6cdeb;
pub const MUTED: u32 = 0xa9b0ce;
pub const LAVENDER: u32 = 0xb9b5ee;
pub const WARNING: u32 = 0xf0a5ac;

pub fn color(value: u32) -> Hsla {
    gpui::rgb(value).into()
}

#[derive(Clone, Copy)]
pub enum FileIcon {
    Folder,
    File,
    Chevron,
    Search,
    Upload,
    Download,
    Refresh,
    Terminal,
    Eye,
    Link,
    Info,
}

pub fn icon(kind: FileIcon, size: f32, tint: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let at = |x: f32, y: f32| {
                point(
                    bounds.origin.x + bounds.size.width * (x / 20.),
                    bounds.origin.y + bounds.size.height * (y / 20.),
                )
            };
            let mut path = PathBuilder::stroke(px(1.25));
            let mut line = |points: &[(f32, f32)]| {
                if let Some(&(x, y)) = points.first() {
                    path.move_to(at(x, y));
                    for &(x, y) in &points[1..] {
                        path.line_to(at(x, y));
                    }
                }
            };
            match kind {
                FileIcon::Folder => {
                    line(&[
                        (2., 6.),
                        (2., 3.),
                        (7., 3.),
                        (9., 5.),
                        (18., 5.),
                        (18., 16.),
                        (2., 16.),
                        (2., 6.),
                        (18., 6.),
                    ]);
                }
                FileIcon::File => {
                    line(&[
                        (4., 2.),
                        (12., 2.),
                        (17., 7.),
                        (17., 18.),
                        (4., 18.),
                        (4., 2.),
                    ]);
                    line(&[(12., 2.), (12., 7.), (17., 7.)]);
                }
                FileIcon::Chevron => line(&[(7., 5.), (12., 10.), (7., 15.)]),
                FileIcon::Search => {
                    line(&[(13., 13.), (18., 18.)]);
                    path.move_to(at(15., 8.5));
                    path.arc_to(at_radius(size, 6.5), px(0.), true, true, at(2., 8.5));
                    path.arc_to(at_radius(size, 6.5), px(0.), true, true, at(15., 8.5));
                }
                FileIcon::Upload => {
                    line(&[(10., 13.), (10., 2.)]);
                    line(&[(5., 7.), (10., 2.), (15., 7.)]);
                    line(&[(3., 12.), (3., 17.), (17., 17.), (17., 12.)]);
                }
                FileIcon::Download => {
                    line(&[(10., 2.), (10., 13.)]);
                    line(&[(5., 8.), (10., 13.), (15., 8.)]);
                    line(&[(3., 13.), (3., 17.), (17., 17.), (17., 13.)]);
                }
                FileIcon::Refresh => {
                    line(&[(17., 2.), (17., 7.), (12., 7.)]);
                    path.move_to(at(17., 7.));
                    path.cubic_bezier_to(at(3., 10.), at(11., -1.), at(2., 3.));
                    path.cubic_bezier_to(at(16., 14.), at(2., 18.), at(12., 20.));
                }
                FileIcon::Terminal => {
                    line(&[
                        (3., 3.),
                        (17., 3.),
                        (18., 4.),
                        (18., 16.),
                        (17., 17.),
                        (3., 17.),
                        (2., 16.),
                        (2., 4.),
                        (3., 3.),
                    ]);
                    line(&[(5., 7.), (8., 10.), (5., 13.)]);
                    line(&[(10., 13.), (14., 13.)]);
                }
                FileIcon::Eye => {
                    path.move_to(at(1., 10.));
                    path.cubic_bezier_to(at(19., 10.), at(6., 1.), at(14., 1.));
                    path.cubic_bezier_to(at(1., 10.), at(14., 19.), at(6., 19.));
                    path.move_to(at(13., 10.));
                    path.arc_to(at_radius(size, 3.), px(0.), true, true, at(7., 10.));
                    path.arc_to(at_radius(size, 3.), px(0.), true, true, at(13., 10.));
                }
                FileIcon::Link => {
                    line(&[(5., 14.), (15., 4.), (9., 4.)]);
                    line(&[(15., 4.), (15., 10.)]);
                }
                FileIcon::Info => {
                    path.move_to(at(18., 10.));
                    path.arc_to(at_radius(size, 8.), px(0.), true, true, at(2., 10.));
                    path.arc_to(at_radius(size, 8.), px(0.), true, true, at(18., 10.));
                    path.move_to(at(10., 8.));
                    path.line_to(at(10., 15.));
                    path.move_to(at(10., 5.));
                    path.line_to(at(10., 6.));
                }
            }
            if let Ok(path) = path.build() {
                window.paint_path(path, tint);
            }
        },
    )
    .size(px(size))
    .flex_shrink_0()
}

fn at_radius(size: f32, radius: f32) -> gpui::Point<gpui::Pixels> {
    point(px(size * radius / 20.), px(size * radius / 20.))
}

/// Pixel geometry of the draggable thumb inside the visible file list.
pub fn scroll_thumb(viewport: f32, maximum: f32, offset: f32) -> Option<(f32, f32)> {
    if viewport <= 0.0
        || maximum <= 0.0
        || !viewport.is_finite()
        || !maximum.is_finite()
        || !offset.is_finite()
    {
        return None;
    }
    let height = (viewport * viewport / (viewport + maximum))
        .max(24.0)
        .min(viewport);
    Some((
        (-offset / maximum).clamp(0.0, 1.0) * (viewport - height),
        height,
    ))
}

#[cfg(test)]
mod tests {
    use super::scroll_thumb;
    #[test]
    fn scroll_thumb_tracks_viewport_and_clamps_overscroll() {
        assert_eq!(scroll_thumb(300.0, 600.0, 0.0), Some((0.0, 100.0)));
        assert_eq!(scroll_thumb(300.0, 600.0, -300.0), Some((100.0, 100.0)));
        assert_eq!(scroll_thumb(300.0, 600.0, -900.0), Some((200.0, 100.0)));
        assert_eq!(scroll_thumb(300.0, 600.0, 10.0), Some((0.0, 100.0)));
        assert_eq!(scroll_thumb(300.0, 0.0, 0.0), None);
        assert_eq!(scroll_thumb(0.0, 10.0, 0.0), None);
    }
}

pub struct Hint(pub String);
impl gpui::Render for Hint {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .max_w(px(320.))
            .rounded_md()
            .bg(color(SEARCH))
            .text_xs()
            .text_color(color(FOREGROUND))
            .child(self.0.clone())
    }
}
