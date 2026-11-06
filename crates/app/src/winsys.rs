//! Windows integration: one running instance per program folder, and a dark
//! window frame that matches the app instead of the system's light title bar.

#[cfg(windows)]
pub use imp::{acquire_instance, dark_frame, focus, place_bottom_right, Start};

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;

    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, RECT, WAIT_OBJECT_0};
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AllowSetForegroundWindow, GetWindowRect, SetForegroundWindow, SetWindowPos,
        SystemParametersInfoW, ASFW_ANY, SPI_GETWORKAREA, SWP_FRAMECHANGED, SWP_NOACTIVATE,
        SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// Per-folder key (FNV-1a of the lower-cased exe directory): a second copy of
    /// the app elsewhere — e.g. an old build next to the stick — may still run, but
    /// the same copy (sharing one config) only once.
    fn instance_key() -> String {
        let dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_string_lossy().to_lowercase()))
            .unwrap_or_default();
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in dir.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        format!("{h:016x}")
    }

    /// Keeps the instance mutex alive and receives "show yourself" requests.
    pub struct InstanceGuard {
        _mutex: HANDLE,
        event: HANDLE,
    }

    impl InstanceGuard {
        /// True once for every later launch that asked us to come to the front.
        pub fn show_requested(&self) -> bool {
            !self.event.is_null() && unsafe { WaitForSingleObject(self.event, 0) } == WAIT_OBJECT_0
        }
    }

    pub enum Start {
        First(InstanceGuard),
        /// Another instance of this copy runs and was asked to show its window.
        AlreadyRunning,
    }

    pub fn acquire_instance() -> Start {
        let key = instance_key();
        let mutex_name = wide(&format!("Local\\MaxtrixPortableManager-{key}"));
        let event_name = wide(&format!("Local\\MaxtrixPortableManager-{key}-show"));
        unsafe {
            let mutex = CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr());
            if !mutex.is_null() && GetLastError() == ERROR_ALREADY_EXISTS {
                let ev = OpenEventW(EVENT_MODIFY_STATE, 0, event_name.as_ptr());
                if !ev.is_null() {
                    // We are the foreground process (the user just started us) —
                    // allow the running instance to take the foreground.
                    AllowSetForegroundWindow(ASFW_ANY);
                    SetEvent(ev);
                    CloseHandle(ev);
                }
                CloseHandle(mutex);
                return Start::AlreadyRunning;
            }
            // Auto-reset event: each SetEvent wakes exactly one poll.
            let event = CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr());
            Start::First(InstanceGuard { _mutex: mutex, event })
        }
    }

    fn hwnd(w: &slint::Window) -> Option<HWND> {
        let handle = w.window_handle();
        let wh = handle.window_handle().ok()?;
        match wh.as_raw() {
            RawWindowHandle::Win32(h) => Some(h.hwnd.get() as HWND),
            _ => None,
        }
    }

    /// COLORREF is 0x00BBGGRR.
    const fn colorref(rgb: u32) -> u32 {
        ((rgb & 0xff) << 16) | (rgb & 0xff00) | ((rgb >> 16) & 0xff)
    }

    /// Dark title bar (Win10 20H1+) and, on Windows 11, caption/border colours
    /// matching the sidebar. Returns false while the native window doesn't exist yet.
    pub fn dark_frame(w: &slint::Window) -> bool {
        let Some(h) = hwnd(w) else { return false };
        let set = |attr: i32, val: u32| unsafe {
            DwmSetWindowAttribute(h, attr as u32, &val as *const u32 as *const c_void, 4);
        };
        set(DWMWA_USE_IMMERSIVE_DARK_MODE, 1);
        set(DWMWA_CAPTION_COLOR, colorref(0x0b0d12)); // sidebar
        set(DWMWA_TEXT_COLOR, colorref(0xc4cbd4));
        set(DWMWA_BORDER_COLOR, colorref(0x1f232a));
        unsafe {
            // Repaint the non-client area so the change shows immediately.
            SetWindowPos(
                h,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
        true
    }

    /// Put a window into the bottom-right corner of the primary screen's work area
    /// (above the taskbar). Uses the real outer size, which only exists once the native
    /// window does and includes title bar, frame and display scaling.
    pub fn place_bottom_right(w: &slint::Window) -> bool {
        const GAP: i32 = 12;
        let Some(h) = hwnd(w) else { return false };
        let mut win = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        let mut area = win;
        unsafe {
            if GetWindowRect(h, &mut win) == 0
                || SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut area as *mut RECT as *mut c_void, 0) == 0
            {
                return false;
            }
            let x = (area.right - (win.right - win.left) - GAP).max(area.left);
            let y = (area.bottom - (win.bottom - win.top) - GAP).max(area.top);
            SetWindowPos(h, std::ptr::null_mut(), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
        true
    }

    /// Bring a shown window to the foreground (e.g. after a second launch).
    pub fn focus(w: &slint::Window) {
        if let Some(h) = hwnd(w) {
            unsafe {
                SetForegroundWindow(h);
            }
        }
    }
}
