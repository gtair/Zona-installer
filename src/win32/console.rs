//! Win32 console allocation for the `debug_console` option.
//!
//! `show_console` is idempotent: if the process already owns a console,
//! `AllocConsole` fails with `ERROR_ACCESS_DENIED`, which is treated as success.

use windows_sys::Win32::Foundation::{GetLastError, ERROR_ACCESS_DENIED};
use windows_sys::Win32::System::Console::AllocConsole;

use crate::error::{AppError, Result};

/// Allocates a real Windows console window for this process.
///
/// Idempotent: returns `Ok(())` if a console was created, or if the process
/// already had one (`AllocConsole` reports `ERROR_ACCESS_DENIED` in that case,
/// which is a success for this function). Any other `AllocConsole` failure is
/// returned as [`AppError::Win32`].
pub fn show_console() -> Result<()> {
    // SAFETY: AllocConsole takes no arguments; it attaches a new console to the
    // calling process, or fails if one is already attached.
    let ok = unsafe { AllocConsole() };
    if ok != 0 {
        return Ok(());
    }
    // SAFETY: GetLastError reads thread-local state only; it is read immediately
    // after the failing AllocConsole call, before any other Win32 call.
    let code = unsafe { GetLastError() };
    if console_already_present(code) {
        return Ok(());
    }
    Err(AppError::Win32 {
        context: "AllocConsole".to_string(),
        code,
    })
}

/// Whether `code` means a console already exists (nothing to create).
fn console_already_present(code: u32) -> bool {
    code == ERROR_ACCESS_DENIED
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_denied_means_a_console_already_exists() {
        assert!(console_already_present(ERROR_ACCESS_DENIED));
        // 0 is ERROR_SUCCESS, not an "already present" signal.
        assert!(!console_already_present(0));
        // An unrelated failure must NOT be swallowed as success.
        assert!(!console_already_present(6));
    }

    #[test]
    fn show_console_is_ok_and_idempotent() {
        // Whether the test harness already has a console or not, both paths are
        // a success for `show_console`; the second call must match the first.
        assert!(show_console().is_ok());
        assert!(show_console().is_ok());
    }
}
