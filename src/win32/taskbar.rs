//! Win32 taskbar flash for when user input is needed.
//!
//! [`flash`] requests one via `FlashWindowEx`; [`flash_own_window`] locates this
//! process's own top-level window (by owning process, never by title) and
//! flashes it, so the core needs no window handle.

use windows_sys::Win32::Foundation::{
    GetLastError, SetLastError, ERROR_INVALID_WINDOW_HANDLE, ERROR_SUCCESS, HWND, LPARAM,
};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FlashWindowEx, GetWindowThreadProcessId, IsWindowVisible, FLASHWINFO, FLASHW_ALL,
    FLASHW_TIMERNOFG,
};

use crate::error::{AppError, Result};

/// Requests a taskbar flash for `hwnd`.
///
/// `FlashWindowEx` returns the window's *previous active state*, not a success
/// flag, so a `FALSE` means nothing. This function zeroes the last-error value,
/// calls `FlashWindowEx`, and reports [`AppError::Win32`] only if a non-zero
/// last error is left behind.
pub fn flash(hwnd: HWND) -> Result<()> {
    let info = FLASHWINFO {
        cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
        hwnd,
        dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
        uCount: 0,
        dwTimeout: 0,
    };
    // SAFETY: SetLastError takes the code to store in this thread's last-error
    // slot and dereferences nothing.
    unsafe { SetLastError(ERROR_SUCCESS) };
    // SAFETY: `info` is a fully initialized FLASHWINFO whose `cbSize` is exactly
    // its own size, and it outlives the call. FlashWindowEx only reads it. Its
    // BOOL return is the previous active state, so it is deliberately ignored.
    let _previous_active_state = unsafe { FlashWindowEx(&info) };
    // SAFETY: GetLastError reads thread-local state only; it is read immediately
    // after FlashWindowEx, before any other Win32 call.
    let code = unsafe { GetLastError() };
    if code != ERROR_SUCCESS {
        return Err(AppError::Win32 {
            context: "FlashWindowEx".to_string(),
            code,
        });
    }
    Ok(())
}

/// Plain-data view of one enumerated top-level window.
///
/// Free of any OS call so [`select_own_window`] can be unit-tested without
/// creating a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowCandidate {
    /// The handle [`flash`] would receive if this window is selected.
    window: HWND,
    /// The process that owns the window (`GetWindowThreadProcessId`).
    owner_pid: u32,
    /// Whether the window is visible (`IsWindowVisible`).
    visible: bool,
}

/// Picks this process's own visible top-level window from `candidates`.
///
/// The window is recognised by its OWNING PROCESS, never by its title (a title
/// match could flash a foreign window that happens to share it). A hidden window
/// is skipped. The first match wins.
fn select_own_window(candidates: &[WindowCandidate], own_pid: u32) -> Option<HWND> {
    candidates
        .iter()
        .find(|candidate| candidate.owner_pid == own_pid && candidate.visible)
        .map(|candidate| candidate.window)
}

/// Flashes this process's own top-level window.
///
/// Unlike [`flash`], which receives the handle from the GUI layer, this locates
/// the installer's window itself (by owning process, never by title) and flashes
/// it. Returns [`AppError::Win32`] when there is no visible top-level window;
/// callers treat the flash as advisory, so that error is logged rather than
/// shown.
pub fn flash_own_window() -> Result<()> {
    // SAFETY: GetCurrentProcessId takes no arguments, cannot fail and touches no
    // memory.
    let own_pid = unsafe { GetCurrentProcessId() };

    let mut candidates: Vec<WindowCandidate> = Vec::new();
    let param = &mut candidates as *mut Vec<WindowCandidate> as LPARAM;
    // SAFETY: `collect_candidate` matches `WNDENUMPROC` and only reads the HWND
    // it is handed; the LPARAM is a pointer to `candidates`, a live stack local
    // for the whole call. EnumWindows stops early only if the callback returns
    // FALSE, which `collect_candidate` never does.
    unsafe { EnumWindows(Some(collect_candidate), param) };

    match select_own_window(&candidates, own_pid) {
        Some(window) => flash(window),
        None => Err(AppError::Win32 {
            context: "locate this process's own visible top-level window".to_string(),
            code: ERROR_INVALID_WINDOW_HANDLE,
        }),
    }
}

/// `EnumWindows` callback that records one top-level window as a
/// [`WindowCandidate`] (the only impure part of the selection).
///
/// # Safety
///
/// `lparam` must be a pointer to a `Vec<WindowCandidate>` that stays alive and
/// exclusively borrowed for the whole `EnumWindows` call; [`flash_own_window`]
/// upholds this by passing a pointer to its own live stack local.
unsafe extern "system" fn collect_candidate(window: HWND, lparam: LPARAM) -> i32 {
    // SAFETY: per this function's contract, `lparam` points at the caller's live
    // `Vec<WindowCandidate>`.
    let candidates = &mut *(lparam as *mut Vec<WindowCandidate>);
    let mut owner_pid: u32 = 0;
    // SAFETY: `window` is a handle EnumWindows just vetted; `&mut owner_pid`
    // points at a live local that this call writes and nothing else reads.
    GetWindowThreadProcessId(window, &mut owner_pid);
    // SAFETY: `window` is a valid top-level window handle.
    let visible = IsWindowVisible(window) != 0;
    candidates.push(WindowCandidate {
        window,
        owner_pid,
        visible,
    });
    // Non-zero keeps enumerating every top-level window.
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow};

    /// Creates a hidden top-level window owned by this thread and returns its
    /// handle. The `STATIC` system class needs no registration; `GetConsoleWindow`
    /// is unusable here because `cargo test` has no console under this shell.
    fn create_test_window() -> HWND {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let title: Vec<u16> = "ZonaInstallerFlashTest\0".encode_utf16().collect();
        // SAFETY: both strings are NUL-terminated UTF-16 buffers that outlive the
        // call; a null parent/menu/instance and an empty style create a plain
        // top-level window of the system `STATIC` class. CreateWindowExW only
        // reads the pointers.
        unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                0,
                0,
                0,
                100,
                100,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        }
    }

    #[test]
    fn flashing_a_live_window_returns_ok() {
        let hwnd = create_test_window();
        assert!(
            !hwnd.is_null(),
            "CreateWindowExW failed with code {}",
            // SAFETY: reads this thread's last-error value only.
            unsafe { GetLastError() }
        );

        let result = flash(hwnd);

        // SAFETY: `hwnd` is a live window created above and is not used after
        // this call.
        unsafe { DestroyWindow(hwnd) };
        assert!(result.is_ok());
    }

    #[test]
    fn flashing_an_invalid_handle_reports_an_error() {
        // A NULL handle is not a real window; the failure criterion must surface
        // the resulting Win32 error rather than reporting Ok unconditionally.
        assert!(flash(std::ptr::null_mut()).is_err());
    }

    // ---- pure window selection (no real window is created) ----------------

    /// A distinguishable dummy window handle for the selection tests. It is only
    /// ever compared, never passed to Win32.
    fn handle(value: usize) -> HWND {
        value as HWND
    }

    fn candidate(window: usize, owner_pid: u32, visible: bool) -> WindowCandidate {
        WindowCandidate {
            window: handle(window),
            owner_pid,
            visible,
        }
    }

    /// The own-and-visible window wins even when a foreign window is enumerated
    /// first and an own-but-hidden one precedes it: the choice is by owner
    /// process plus visibility, never by enumeration order or title.
    #[test]
    fn select_own_window_picks_the_own_visible_window() {
        let candidates = vec![
            candidate(1, 999, true), // a foreign, visible window
            candidate(2, 42, false), // this process, but hidden
            candidate(3, 42, true),  // this process and visible
        ];
        assert_eq!(select_own_window(&candidates, 42), Some(handle(3)));
    }

    /// With no window owned by this process there is nothing to flash.
    #[test]
    fn select_own_window_is_none_without_an_own_window() {
        let candidates = vec![candidate(1, 999, true)];
        assert_eq!(select_own_window(&candidates, 42), None);
        assert_eq!(select_own_window(&[], 42), None);
    }

    /// An own window that is not visible is skipped: a hidden window has no
    /// taskbar button to flash.
    #[test]
    fn select_own_window_is_none_when_the_own_window_is_hidden() {
        let candidates = vec![candidate(2, 42, false)];
        assert_eq!(select_own_window(&candidates, 42), None);
    }
}
