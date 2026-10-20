//! Win32 named-mutex single-instance gate.
//!
//! A single named mutex enforces one installer instance per logon session. The
//! gate is the *existence* of the named object, not its ownership state:
//! `CreateMutexW` is called unowned, and a repeated create for the same name is
//! reported by `GetLastError` as `ERROR_ALREADY_EXISTS`.

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows_sys::Win32::System::Threading::CreateMutexW;

use crate::error::{AppError, Result};

/// The fixed mutex name used by the real installer.
pub const SINGLE_INSTANCE_NAME: &str = r"Local\ZonaInstaller";

/// The result of [`SingleInstance::acquire`].
pub enum AcquireOutcome {
    /// This process created the mutex; keep the guard alive for the whole run.
    Acquired(SingleInstance),
    /// Another instance already holds the mutex; exit without running.
    AlreadyRunning,
}

/// An owning handle to the single-instance mutex.
///
/// Dropping the guard closes the handle; once the last handle closes, the OS
/// destroys the named mutex and a later process can acquire it again.
pub struct SingleInstance {
    handle: HANDLE,
}

impl SingleInstance {
    /// Acquires (or finds) the named single-instance mutex.
    ///
    /// Returns [`AcquireOutcome::Acquired`] when this call created the mutex, or
    /// [`AcquireOutcome::AlreadyRunning`] when it already existed.
    pub fn acquire(name: &str) -> Result<AcquireOutcome> {
        let wide = to_wide_null(name);
        // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives the
        // call; a null security-attributes pointer asks for default security and
        // `bInitialOwner = 0` leaves the mutex unowned. No caller memory is
        // dereferenced.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
        if handle.is_null() {
            return Err(last_error("CreateMutexW"));
        }
        // SAFETY: GetLastError takes no arguments and reads thread-local state
        // only; it is read immediately after CreateMutexW, before any other Win32
        // call, so it reflects that call's result.
        let code = unsafe { GetLastError() };
        if code == ERROR_ALREADY_EXISTS {
            // A handle to the pre-existing mutex was opened; close it. The
            // process that created the mutex still holds the gate.
            // SAFETY: `handle` came from CreateMutexW and is owned solely by this
            // function; it is not used after this call.
            unsafe {
                CloseHandle(handle);
            }
            return Ok(AcquireOutcome::AlreadyRunning);
        }
        Ok(AcquireOutcome::Acquired(SingleInstance { handle }))
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: `self.handle` came from CreateMutexW and is owned solely by
        // this wrapper; it is not used after this call.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// Encodes `name` as a NUL-terminated UTF-16 buffer for the `*W` Win32 APIs.
fn to_wide_null(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Builds an [`AppError::Win32`] carrying the calling thread's last-error code.
fn last_error(context: &str) -> AppError {
    // SAFETY: GetLastError takes no arguments and reads thread-local state only.
    let code = unsafe { GetLastError() };
    AppError::Win32 {
        context: context.to_string(),
        code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A per-process, per-test mutex name so parallel tests never collide.
    fn test_name(label: &str) -> String {
        format!(r"Local\ZonaInstallerTest_{}_{label}", std::process::id())
    }

    #[test]
    fn production_name_is_in_the_local_namespace() {
        assert_eq!(SINGLE_INSTANCE_NAME, r"Local\ZonaInstaller");
    }

    #[test]
    fn wide_encoding_is_null_terminated_utf16() {
        let wide = to_wide_null(r"Local\ZonaInstaller");
        // "Local" in UTF-16 code units, ended by a NUL terminator.
        assert_eq!(wide[..5], [0x4C, 0x6F, 0x63, 0x61, 0x6C]);
        assert_eq!(wide.last(), Some(&0));
        assert_eq!(
            wide.len(),
            r"Local\ZonaInstaller".encode_utf16().count() + 1
        );
    }

    #[test]
    fn acquiring_the_same_name_twice_reports_already_running() {
        let name = test_name("same");
        let first = match SingleInstance::acquire(&name).expect("first acquire") {
            AcquireOutcome::Acquired(guard) => guard,
            AcquireOutcome::AlreadyRunning => panic!("first acquire should create the mutex"),
        };
        // `first` is still alive, so the second acquire sees the same object.
        assert!(matches!(
            SingleInstance::acquire(&name).expect("second acquire"),
            AcquireOutcome::AlreadyRunning
        ));
        drop(first);
    }

    #[test]
    fn distinct_names_both_acquire() {
        let a = test_name("a");
        let b = test_name("b");
        let guard_a = match SingleInstance::acquire(&a).expect("acquire a") {
            AcquireOutcome::Acquired(guard) => guard,
            AcquireOutcome::AlreadyRunning => panic!("name 'a' should be free"),
        };
        let guard_b = match SingleInstance::acquire(&b).expect("acquire b") {
            AcquireOutcome::Acquired(guard) => guard,
            AcquireOutcome::AlreadyRunning => panic!("name 'b' should be free"),
        };
        drop(guard_a);
        drop(guard_b);
    }
}
