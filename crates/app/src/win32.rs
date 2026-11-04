//! The Win32 calls GPUI 0.2.2 does not offer on Windows.
//!
//! The shell draws its own title bar to get Pebrel's chrome, so dragging,
//! maximizing and restoring have to be asked for directly. Everything here is a
//! no-op elsewhere, so the rest of the app stays platform-agnostic.

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicIsize, Ordering};

    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
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
    pub fn drag() {}
    pub fn is_maximized() -> bool {
        false
    }
    pub fn toggle_maximize() {}
    pub fn minimize() {}
    pub fn round_corners() {}
}

pub use imp::{drag, is_maximized, minimize, round_corners, toggle_maximize};
