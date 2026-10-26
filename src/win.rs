//! Tiny Windows helpers for the popup window.
//!
//! Moving the window through the OS (instead of through egui's viewport
//! commands) keeps the drag pixel-perfect: the cursor position is read in
//! *physical* pixels and the window is moved by the same amount, so nothing
//! gets lost between frames and mixed-DPI monitors cannot distort it.

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetCursorPos, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
        SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    };

    /// Our own top-level window, found by process id.
    pub struct OwnWindow(HWND);

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let found = &mut *(lparam as *mut Option<HWND>);
        if found.is_none() && IsWindowVisible(hwnd) != 0 {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == std::process::id() {
                *found = Some(hwnd);
            }
        }
        1 // keep going
    }

    impl OwnWindow {
        /// The first visible window of this process.
        pub fn find() -> Option<Self> {
            let mut found: Option<HWND> = None;
            unsafe {
                EnumWindows(Some(enum_proc), &mut found as *mut Option<HWND> as LPARAM);
            }
            found.map(Self)
        }

        /// Top-left corner of the window in physical pixels.
        pub fn origin(&self) -> Option<(i32, i32)> {
            let mut r = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            let ok = unsafe { GetWindowRect(self.0, &mut r) };
            if ok != 0 {
                Some((r.left, r.top))
            } else {
                None
            }
        }

        /// Move the window so its top-left corner sits at (x, y) — physical
        /// pixels, no size or z-order change, no focus stealing.
        pub fn move_to(&self, x: i32, y: i32) -> bool {
            unsafe {
                SetWindowPos(
                    self.0,
                    std::ptr::null_mut(),
                    x,
                    y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                ) != 0
            }
        }
    }

    /// The mouse cursor in physical pixels.
    pub fn cursor_pos() -> Option<(i32, i32)> {
        let mut p = POINT { x: 0, y: 0 };
        let ok = unsafe { GetCursorPos(&mut p) };
        if ok != 0 {
            Some((p.x, p.y))
        } else {
            None
        }
    }
}

/// The window we live in, plus the offset of the grab point inside it.
#[cfg(windows)]
pub struct DragState {
    win: imp::OwnWindow,
    /// Cursor position minus the window origin when the drag started.
    offset: (i32, i32),
}

#[cfg(windows)]
impl DragState {
    /// Start a drag: remember where the cursor grabbed the window.
    pub fn begin() -> Option<Self> {
        let win = imp::OwnWindow::find()?;
        let origin = win.origin()?;
        let cursor = imp::cursor_pos()?;
        Some(Self {
            win,
            offset: (cursor.0 - origin.0, cursor.1 - origin.1),
        })
    }

    /// Follow the cursor. Returns the new window origin in physical pixels.
    pub fn follow(&self) -> Option<(i32, i32)> {
        let cursor = imp::cursor_pos()?;
        let x = cursor.0 - self.offset.0;
        let y = cursor.1 - self.offset.1;
        if self.win.move_to(x, y) {
            Some((x, y))
        } else {
            None
        }
    }
}

/// Non-Windows: no native window to move, the egui path is used instead.
#[cfg(not(windows))]
pub struct DragState;

#[cfg(not(windows))]
impl DragState {
    pub fn begin() -> Option<Self> {
        None
    }
    pub fn follow(&self) -> Option<(i32, i32)> {
        None
    }
}
