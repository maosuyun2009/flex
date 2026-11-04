//! A blurred copy of whatever is behind the window.
//!
//! DWM cannot frost this window — GPUI paints opaque pixels through
//! DirectComposition, so `WindowBackgroundAppearance::Blurred` either blacks out
//! screen capture or composites over black, and `DWMWA_SYSTEMBACKDROP_TYPE`
//! draws nothing. So the shell takes the picture itself: keep the window out of
//! capture (`WDA_EXCLUDEFROMCAPTURE`), grab the client area every few hundred
//! milliseconds, blur it hard, and hand the UI an image to paint behind
//! everything.
//!
//! The trap is exclusion: while it is on, this window does not appear in the
//! user's own screenshots or screen shares. Turning the Acrylic setting off
//! turns it off too.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_channel::Sender;
use gpui::RenderImage;
use image::{imageops::FilterType, Frame, RgbaImage};

/// How often the background is re-read. Fast enough to follow a scroll, slow
/// enough that the capture and blur cost nothing noticeable.
const INTERVAL: Duration = Duration::from_millis(350);
/// Samples per side used to notice that the desktop behind us changed.
const GRID: usize = 16;
/// Mean channel difference below which a new frame is not worth showing.
const THRESHOLD: f32 = 1.5;

/// Blur the desktop behind the window until `enabled` says stop, handing each
/// new frame to the UI. Runs on its own thread: GDI capture and the blur are
/// both blocking.
pub fn live_blur(frames: Sender<Arc<RenderImage>>, enabled: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        // Without this, the capture would include this window and the backdrop
        // would slowly smear into itself.
        let testing = std::env::var_os("REXSHELL_NO_EXCLUSION").is_some();
        let mut excluded = false;
        let mut previous: Option<Vec<f32>> = None;

        let debug = std::env::var_os("REXSHELL_DEBUG").is_some();
        loop {
            std::thread::sleep(INTERVAL);

            if !enabled.load(Ordering::Relaxed) {
                if excluded {
                    crate::win32::exclude_from_capture(false);
                    excluded = false;
                }
                previous = None;
                continue;
            }
            if !excluded {
                if !testing {
                    crate::win32::exclude_from_capture(true);
                }
                excluded = true;
                previous = None;
            }

            let Some(rect) = crate::win32::client_rect_on_screen() else {
                if debug {
                    eprintln!("[backdrop] no client rect");
                }
                continue;
            };
            let Some(bgra) = crate::win32::capture(rect) else {
                if debug {
                    eprintln!("[backdrop] capture failed for {rect:?}");
                }
                continue;
            };
            let (width, height) = (rect.2 as u32, rect.3 as u32);
            let Some(image) = RgbaImage::from_raw(width, height, bgra) else {
                continue;
            };

            let signature = signature(&image);
            if previous
                .as_ref()
                .is_some_and(|before| distance(before, &signature) < THRESHOLD)
            {
                continue;
            }
            previous = Some(signature);

            if debug {
                eprintln!("[backdrop] frame {}x{}", width, height);
            }
            let small = frosted(&image);
            if debug {
                let probe = std::env::temp_dir().join("rexshell-probe.png");
                let _ = small.save(&probe);
            }
            let frame = Frame::new(small);
            if frames
                .try_send(Arc::new(RenderImage::new([frame])))
                .is_err()
            {
                // The UI is gone; stop taking pictures of the desktop.
                crate::win32::exclude_from_capture(false);
                return;
            }
        }
    });
}

/// Downscale hard, then blur: the downscale is most of the frost and the blur
/// removes the boxes it leaves behind.
fn frosted(image: &RgbaImage) -> RgbaImage {
    let (width, height) = (image.width(), image.height());
    let small_width = (width / 12).clamp(24, 360);
    let small_height = ((height as f64 / width as f64) * small_width as f64)
        .round()
        .max(16.0) as u32;
    let mut small = image::imageops::resize(image, small_width, small_height, FilterType::Triangle);
    box_blur(&mut small, 2);
    box_blur(&mut small, 2);
    small
}

/// Mean colour of a coarse grid, for cheap change detection.
fn signature(image: &RgbaImage) -> Vec<f32> {
    let (width, height) = (image.width().max(1), image.height().max(1));
    let mut out = Vec::with_capacity(GRID * GRID * 3);
    for row in 0..GRID {
        for column in 0..GRID {
            let x = (width * column as u32) / GRID as u32;
            let y = (height * row as u32) / GRID as u32;
            let pixel = image.get_pixel(x.min(width - 1), y.min(height - 1));
            out.extend([pixel.0[0] as f32, pixel.0[1] as f32, pixel.0[2] as f32]);
        }
    }
    out
}

fn distance(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return f32::MAX;
    }
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f32>() / a.len() as f32
}

/// Separable box blur, one pass each way. Slow in principle, irrelevant at this
/// size: the image is a few hundred pixels wide.
fn box_blur(image: &mut RgbaImage, radius: i32) {
    let (width, height) = (image.width() as i32, image.height() as i32);
    let source = image.clone();
    for y in 0..height {
        for x in 0..width {
            let mut sums = [0u32; 4];
            let mut count = 0u32;
            for offset in -radius..=radius {
                let sample = source.get_pixel((x + offset).clamp(0, width - 1) as u32, y as u32);
                for channel in 0..4 {
                    sums[channel] += sample.0[channel] as u32;
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
    let source = image.clone();
    for y in 0..height {
        for x in 0..width {
            let mut sums = [0u32; 4];
            let mut count = 0u32;
            for offset in -radius..=radius {
                let sample = source.get_pixel(x as u32, (y + offset).clamp(0, height - 1) as u32);
                for channel in 0..4 {
                    sums[channel] += sample.0[channel] as u32;
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
