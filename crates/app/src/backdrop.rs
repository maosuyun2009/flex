//! A blurred copy of the desktop wallpaper, painted behind the chrome.
//!
//! Real acrylic is out of reach here, and this was measured rather than assumed:
//! GPUI's Windows window renders opaque pixels, so `WindowBackgroundAppearance::
//! Blurred` (the legacy accent policy) either blacks out screen capture or
//! composites over black, and `DWMWA_SYSTEMBACKDROP_TYPE` draws nothing on a
//! DirectComposition window. So the window stays opaque and fakes the frost the
//! way Mica does: blur what is behind the window, paint it as the background,
//! and keep every surface translucent on top.
//!
//! The wallpaper is read once at startup. Wallpaper slideshow changes are not
//! picked up until the shell restarts.

use std::path::PathBuf;

use image::{imageops::FilterType, RgbaImage};

/// A heavily blurred, darkened copy of the wallpaper, or `None` when the desktop
/// has no image (a solid colour, say) or it cannot be read.
pub fn generate() -> Option<PathBuf> {
    let source = crate::win32::wallpaper()?;
    let image = image::open(&source).ok()?.to_rgba8();
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return None;
    }

    // Downscaling does most of the work; two box blurs each way remove the
    // boxes that a plain downscale leaves behind.
    let small_width = (width / 14).clamp(24, 320);
    let small_height = ((height as f64 / width as f64) * small_width as f64)
        .round()
        .max(16.0) as u32;
    let mut small =
        image::imageops::resize(&image, small_width, small_height, FilterType::Triangle);
    box_blur(&mut small, 3);
    box_blur(&mut small, 3);

    // Left at its own brightness: the translucent surfaces on top are what keep
    // text readable, and darkening here would flatten the effect away.

    let path = std::env::temp_dir().join(format!("rexshell-backdrop-{}.png", std::process::id()));
    small.save(&path).ok()?;
    Some(path)
}

/// Separable box blur, one pass each way. Slow in principle, irrelevant at this
/// size: the image is a couple of hundred pixels wide.
fn box_blur(image: &mut RgbaImage, radius: i32) {
    let (width, height) = (image.width() as i32, image.height() as i32);
    let mut horizontal = image.clone();
    for y in 0..height {
        for x in 0..width {
            let mut sums = [0u32; 4];
            let mut count = 0u32;
            for offset in -radius..=radius {
                let sample_x = (x + offset).clamp(0, width - 1) as u32;
                let pixel = image.get_pixel(sample_x, y as u32);
                for channel in 0..4 {
                    sums[channel] += pixel.0[channel] as u32;
                }
                count += 1;
            }
            let mut out = [0u8; 4];
            for channel in 0..4 {
                out[channel] = (sums[channel] / count) as u8;
            }
            horizontal.put_pixel(x as u32, y as u32, image::Rgba(out));
        }
    }
    for y in 0..height {
        for x in 0..width {
            let mut sums = [0u32; 4];
            let mut count = 0u32;
            for offset in -radius..=radius {
                let sample_y = (y + offset).clamp(0, height - 1) as u32;
                let pixel = horizontal.get_pixel(x as u32, sample_y);
                for channel in 0..4 {
                    sums[channel] += pixel.0[channel] as u32;
                }
                count += 1;
            }
            let mut out = [0u8; 4];
            for channel in 0..4 {
                out[channel] = (sums[channel] / count) as u8;
            }
            image.put_pixel(x as u32, y as u32, image::Rgba(out));
        }
    }
}
