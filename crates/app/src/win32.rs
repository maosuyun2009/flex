//! The Win32 calls GPUI 0.2.2 does not offer on Windows.
//!
//! The shell draws its own title bar to get Pebrel's chrome, so dragging,
//! maximizing and restoring have to be asked for directly. Everything here is a
//! no-op elsewhere, so the rest of the app stays platform-agnostic.

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicIsize, Ordering};

    use super::Pasted;

    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::HGLOBAL;
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
        OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::System::Ole::{CF_DIB, CF_HDROP, CF_UNICODETEXT};
    use windows_sys::Win32::UI::Shell::{DragQueryFileW, HDROP};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, FlashWindowEx, GetForegroundWindow, GetWindowThreadProcessId,
        IsWindowVisible, IsZoomed, SendMessageW, ShowWindowAsync, FLASHWINFO, FLASHW_TIMERNOFG,
        FLASHW_TRAY, HTCAPTION, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, WM_NCLBUTTONDOWN,
    };

    static FOUND: AtomicIsize = AtomicIsize::new(0);

    unsafe extern "system" fn visit(hwnd: HWND, _: LPARAM) -> BOOL {
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid == unsafe { GetCurrentProcessId() } && unsafe { IsWindowVisible(hwnd) } != 0 {
            FOUND.store(hwnd as isize, Ordering::Release);
            return 0; // stop enumerating
        }
        1
    }

    /// This process's first visible top-level window: ours.
    fn hwnd() -> Option<HWND> {
        if let found @ 1.. = FOUND.load(Ordering::Acquire) {
            return Some(found as HWND);
        }
        unsafe { EnumWindows(Some(visit), 0) };
        match FOUND.load(Ordering::Acquire) {
            0 => None,
            found => Some(found as HWND),
        }
    }

    /// Hand the window to the shell so it drags with the pointer.
    pub fn drag() {
        if let Some(hwnd) = hwnd() {
            unsafe {
                ReleaseCapture();
                SendMessageW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION as usize, 0);
            }
        }
    }

    pub fn is_maximized() -> bool {
        hwnd().is_some_and(|hwnd| unsafe { IsZoomed(hwnd) } != 0)
    }

    pub fn toggle_maximize() {
        if let Some(hwnd) = hwnd() {
            let command = if is_maximized() {
                SW_RESTORE
            } else {
                SW_MAXIMIZE
            };
            unsafe { ShowWindowAsync(hwnd, command) };
        }
    }

    pub fn minimize() {
        if let Some(hwnd) = hwnd() {
            unsafe { ShowWindowAsync(hwnd, SW_MINIMIZE) };
        }
    }

    /// Flash the taskbar button until the window comes to the front. Does
    /// nothing while it already is: the notification is on screen there.
    pub fn flash_if_background() {
        let Some(hwnd) = hwnd() else { return };
        if unsafe { GetForegroundWindow() } == hwnd {
            return;
        }
        let info = FLASHWINFO {
            cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
            hwnd,
            dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
            uCount: 0,
            dwTimeout: 0,
        };
        unsafe { FlashWindowEx(&info) };
    }

    /// Windows 11 rounds a decorated window's corners; ours has no decorations,
    /// so ask DWM for the same rounding (DWMWA_WINDOW_CORNER_PREFERENCE = 33).
    /// Text on the Win32 clipboard. GPUI's own reader came back empty for text
    /// another process put there, so read the format directly.
    pub fn clipboard_text() -> Option<String> {
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return None;
            }
            let mut text = None;
            if IsClipboardFormatAvailable(CF_UNICODETEXT as u32) != 0 {
                let handle = GetClipboardData(CF_UNICODETEXT as u32);
                if !handle.is_null() {
                    let global = handle as HGLOBAL;
                    let bytes = GlobalSize(global);
                    let wide = GlobalLock(global) as *const u16;
                    if !wide.is_null() {
                        let limit = bytes / 2;
                        let mut len = 0;
                        while len < limit && *wide.add(len) != 0 {
                            len += 1;
                        }
                        text = Some(String::from_utf16_lossy(std::slice::from_raw_parts(
                            wide, len,
                        )));
                        GlobalUnlock(global);
                    }
                }
            }
            CloseClipboard();
            text
        }
    }

    /// The text a terminal should type for Ctrl-V.
    ///
    /// Clipboards are not only text: a screenshot tool copies the picture as a
    /// file (`CF_HDROP`) or as a bare bitmap, and the system terminal pastes the
    /// path of the file it makes in both cases. GPUI's reader knows neither, so
    /// the path is built here.
    pub fn clipboard_paste() -> Option<Pasted> {
        if let Some(text) = clipboard_text().filter(|text| !text.is_empty()) {
            return Some(Pasted {
                text,
                windows_paths: false,
            });
        }
        let paths = clipboard_files().or_else(clipboard_image)?;
        Some(Pasted {
            text: paths,
            windows_paths: true,
        })
    }

    /// Files on the clipboard, quoted, so a path with a space still reaches the
    /// program as one argument.
    fn clipboard_files() -> Option<String> {
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return None;
            }
            let mut paths = Vec::new();
            if IsClipboardFormatAvailable(CF_HDROP as u32) != 0 {
                let handle = GetClipboardData(CF_HDROP as u32);
                if !handle.is_null() {
                    let drop = handle as HDROP;
                    let count = DragQueryFileW(drop, u32::MAX, std::ptr::null_mut(), 0);
                    for index in 0..count {
                        let len = DragQueryFileW(drop, index, std::ptr::null_mut(), 0) as usize;
                        if len == 0 {
                            continue;
                        }
                        let mut buf = vec![0u16; len + 1];
                        let written = DragQueryFileW(drop, index, buf.as_mut_ptr(), len as u32 + 1)
                            as usize;
                        if written == 0 {
                            continue;
                        }
                        let path = String::from_utf16_lossy(&buf[..written]);
                        paths.push(if path.contains(' ') {
                            format!("\"{path}\"")
                        } else {
                            path
                        });
                    }
                }
            }
            CloseClipboard();
            (!paths.is_empty()).then(|| paths.join(" "))
        }
    }

    /// A bitmap on the clipboard, written to a PNG in the temp directory: what
    /// Win-Shift-S leaves behind, and what the system terminal turns into a path.
    fn clipboard_image() -> Option<String> {
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return None;
            }
            let mut dib = None;
            if IsClipboardFormatAvailable(CF_DIB as u32) != 0 {
                let handle = GetClipboardData(CF_DIB as u32);
                if !handle.is_null() {
                    let global = handle as HGLOBAL;
                    let size = GlobalSize(global);
                    let data = GlobalLock(global) as *const u8;
                    if !data.is_null() && size > 40 {
                        dib = Some(std::slice::from_raw_parts(data, size).to_vec());
                    }
                    GlobalUnlock(global);
                }
            }
            CloseClipboard();
            dib
        }
        .and_then(|dib| save_dib(&dib))
    }

    /// The clipboard's DIB carries no file header: add one and let `image` write
    /// the file. A bitmap that will not decode is kept as a BMP rather than lost.
    pub(super) fn save_dib(dib: &[u8]) -> Option<String> {
        let header = u32::from_le_bytes(dib.get(0..4)?.try_into().ok()?) as usize;
        if header < 12 || header > dib.len() {
            return None;
        }
        let mut bmp = Vec::with_capacity(14 + dib.len());
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&((14 + dib.len()) as u32).to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&((14 + header) as u32).to_le_bytes());
        bmp.extend_from_slice(dib);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir();
        if let Ok(image) = image::load_from_memory_with_format(&bmp, image::ImageFormat::Bmp) {
            let path = dir.join(format!("flex-paste-{stamp}.png"));
            if image.save(&path).is_ok() {
                return Some(path.display().to_string());
            }
        }
        let path = dir.join(format!("flex-paste-{stamp}.bmp"));
        std::fs::write(&path, &bmp).ok()?;
        Some(path.display().to_string())
    }

    /// Put text on the clipboard. A program copying with OSC 52 (vim, tmux, an
    /// agent's copy button) never reaches GPUI's clipboard API, because the text
    /// arrives on a stream callback, not in a UI action.
    pub fn set_clipboard_text(text: &str) {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return;
            }
            EmptyClipboard();
            let bytes = wide.len() * std::mem::size_of::<u16>();
            let global = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if !global.is_null() {
                let dest = GlobalLock(global) as *mut u16;
                if !dest.is_null() {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), dest, wide.len());
                    GlobalUnlock(global);
                    // The clipboard owns the block now; freeing it would be a bug.
                    SetClipboardData(CF_UNICODETEXT as u32, global as *mut _);
                }
            }
            CloseClipboard();
        }
    }

    /// Open a link in whatever the user browses with.
    pub fn open_url(url: &str) {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
        let verb: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                wide.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            );
        }
    }

    pub fn round_corners() {
        const ROUND: u32 = 2; // DWMWCP_ROUND
        if let Some(hwnd) = hwnd() {
            unsafe {
                DwmSetWindowAttribute(hwnd, 33, &ROUND as *const u32 as *const _, 4);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn clipboard_text() -> Option<String> {
        None
    }

    pub fn clipboard_paste() -> Option<super::Pasted> {
        None
    }

    pub fn set_clipboard_text(_: &str) {}

    pub fn open_url(_: &str) {}

    pub fn drag() {}
    pub fn is_maximized() -> bool {
        false
    }
    pub fn toggle_maximize() {}
    pub fn minimize() {}
    pub fn flash_if_background() {}
    pub fn round_corners() {}
}

/// What Ctrl-V should type: the text itself, and whether that text is a list of
/// Windows paths this file made up (a WSL server names those differently).
pub struct Pasted {
    pub text: String,
    pub windows_paths: bool,
}

pub use imp::{
    clipboard_paste, clipboard_text, drag, flash_if_background, is_maximized, minimize, open_url,
    round_corners, set_clipboard_text, toggle_maximize,
};

#[cfg(all(test, windows))]
mod tests {
    use super::imp::save_dib;

    /// A 1x1 24-bit bottom-up DIB, the shape the clipboard hands over.
    fn dib() -> Vec<u8> {
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes()); // header size
        dib.extend_from_slice(&1i32.to_le_bytes()); // width
        dib.extend_from_slice(&1i32.to_le_bytes()); // height
        dib.extend_from_slice(&1u16.to_le_bytes()); // planes
        dib.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
        dib.extend_from_slice(&0u32.to_le_bytes()); // compression
        dib.extend_from_slice(&4u32.to_le_bytes()); // size of the pixels
        for _ in 0..4 {
            dib.extend_from_slice(&0u32.to_le_bytes());
        }
        dib.extend_from_slice(&[0x30, 0x60, 0x90, 0x00]); // blue, green, red, padding
        dib
    }

    #[test]
    fn a_copied_bitmap_becomes_a_file_path() {
        let path = save_dib(&dib()).expect("a 1x1 dib saves");
        assert!(path.ends_with(".png"), "{path}");
        let image = image::open(&path).expect("the png reads back");
        assert_eq!((image.width(), image.height()), (1, 1));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_dib_without_a_header_is_refused() {
        assert!(save_dib(b"nope").is_none());
        assert!(save_dib(&[]).is_none());
    }
}
