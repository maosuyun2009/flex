//! The Win32 calls GPUI 0.2.2 does not offer on Windows.
//!
//! The shell draws its own title bar to get Pebrel's chrome, so dragging,
//! maximizing and restoring have to be asked for directly. Everything here is a
//! no-op elsewhere, so the rest of the app stays platform-agnostic.

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicIsize, Ordering};

    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
        GetDC, GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        CAPTUREBLT, DIB_RGB_COLORS, HDC, SRCCOPY,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClientRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible, IsZoomed,
        SendMessageW, SetWindowDisplayAffinity, ShowWindowAsync, HTCAPTION, SW_MAXIMIZE,
        SW_MINIMIZE, SW_RESTORE, WM_NCLBUTTONDOWN,
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

    /// Keep this window out of every screen capture, so that capturing the
    /// desktop behind it returns the desktop and not a photograph of ourselves.
    /// The cost is that the window also disappears from the user's screenshots
    /// and screen shares.
    pub fn exclude_from_capture(enable: bool) {
        const WDA_NONE: u32 = 0;
        const WDA_EXCLUDEFROMCAPTURE: u32 = 0x0011;
        if let Some(hwnd) = hwnd() {
            unsafe {
                SetWindowDisplayAffinity(
                    hwnd,
                    if enable {
                        WDA_EXCLUDEFROMCAPTURE
                    } else {
                        WDA_NONE
                    },
                );
            }
        }
    }

    /// This window's client area in screen pixels, or `None` when it is hidden,
    /// minimized or not created yet.
    pub fn client_rect_on_screen() -> Option<(i32, i32, i32, i32)> {
        let hwnd = hwnd()?;
        if unsafe { IsIconic(hwnd) } != 0 || unsafe { IsWindowVisible(hwnd) } == 0 {
            return None;
        }
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        unsafe { GetClientRect(hwnd, &mut rect) };
        let mut origin = POINT { x: 0, y: 0 };
        unsafe { ClientToScreen(hwnd, &mut origin) };
        let size = (rect.right - rect.left, rect.bottom - rect.top);
        (size.0 > 0 && size.1 > 0).then_some((origin.x, origin.y, size.0, size.1))
    }

    /// The screen under `rect`, as top-down BGRA rows. `CAPTUREBLT` includes
    /// layered windows, which a browser full of them needs.
    pub fn capture(rect: (i32, i32, i32, i32)) -> Option<Vec<u8>> {
        let (x, y, width, height) = rect;
        if width <= 0 || height <= 0 {
            return None;
        }
        unsafe {
            let screen: HDC = GetDC(std::ptr::null_mut());
            if screen.is_null() {
                return None;
            }
            let memory = CreateCompatibleDC(screen);
            let bitmap = CreateCompatibleBitmap(screen, width, height);
            let previous = SelectObject(memory, bitmap);
            let copied = BitBlt(
                memory,
                0,
                0,
                width,
                height,
                screen,
                x,
                y,
                SRCCOPY | CAPTUREBLT,
            );

            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB as u32,
                ..std::mem::zeroed()
            };
            let mut pixels = vec![0u8; (width * height * 4) as usize];
            let lines = GetDIBits(
                memory,
                bitmap,
                0,
                height as u32,
                pixels.as_mut_ptr().cast(),
                &mut info,
                DIB_RGB_COLORS,
            );

            SelectObject(memory, previous);
            DeleteObject(bitmap);
            DeleteDC(memory);
            ReleaseDC(std::ptr::null_mut(), screen);

            (copied != 0 && lines == height).then_some(pixels)
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
    pub fn exclude_from_capture(_enable: bool) {}
    pub fn client_rect_on_screen() -> Option<(i32, i32, i32, i32)> {
        None
    }
    pub fn capture(_rect: (i32, i32, i32, i32)) -> Option<Vec<u8>> {
        None
    }
}

pub use imp::{
    capture, client_rect_on_screen, drag, exclude_from_capture, is_maximized, minimize,
    round_corners, toggle_maximize,
};
