//! The Win32 calls GPUI 0.2.2 does not offer on Windows.
//!
//! The shell draws its own title bar to get Pebrel's chrome, so dragging,
//! maximizing and restoring have to be asked for directly. Everything here is a
//! no-op elsewhere, so the rest of the app stays platform-agnostic.

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicIsize, Ordering};

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
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsWindowVisible, IsZoomed, SendMessageW,
        ShowWindowAsync, HTCAPTION, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, WM_NCLBUTTONDOWN,
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

    pub fn set_clipboard_text(_: &str) {}

    pub fn open_url(_: &str) {}

    pub fn drag() {}
    pub fn is_maximized() -> bool {
        false
    }
    pub fn toggle_maximize() {}
    pub fn minimize() {}
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

    pub fn round_corners() {}
}

pub use imp::{
    clipboard_text, drag, is_maximized, minimize, open_url, round_corners, set_clipboard_text,
    toggle_maximize,
};
