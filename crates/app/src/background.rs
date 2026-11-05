//! A picture to draw behind the terminal text.
//!
//! Decoded once and kept as a `RenderImage`; the terminal paints it, then its own
//! colour over it, so the text keeps its contrast.

use std::sync::Arc;

use gpui::RenderImage;
use image::Frame;

/// Decode `path` into an image the renderer can draw. `None` when the file is
/// missing, unsupported, or too large to be worth decoding at startup.
pub fn load(path: &str) -> Option<Arc<RenderImage>> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > 64 << 20 {
        eprintln!("flex: background image is larger than 64 MiB, ignoring it");
        return None;
    }
    let mut decoded = image::load_from_memory(&bytes).ok()?.to_rgba8();
    // The renderer takes BGRA, premultiplied. Feeding it RGBA swaps red and blue,
    // which is what made a blue picture render brown.
    for pixel in decoded.pixels_mut() {
        let [r, g, b, a] = pixel.0;
        let alpha = a as f32 / 255.0;
        pixel.0 = [
            (b as f32 * alpha) as u8,
            (g as f32 * alpha) as u8,
            (r as f32 * alpha) as u8,
            a,
        ];
    }
    Some(Arc::new(RenderImage::new([Frame::new(decoded)])))
}
