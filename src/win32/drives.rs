//! Win32 fixed-drive enumeration for the drive picker.
//!
//! [`fixed_local_drives`] enumerates logical drives and keeps only `DRIVE_FIXED`
//! ones; network, removable, optical and RAM-disk drives are excluded. The
//! bitmask filtering is pure and unit-tested; only the two raw calls are FFI.

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};

use crate::error::{AppError, Result};

/// `DRIVE_FIXED`: the drive is a fixed disk.
///
/// windows-sys declares this constant in a large feature-gated module, so it is
/// declared here rather than pulling that whole feature in for one constant.
const DRIVE_FIXED: u32 = 3;

/// Enumerates the fixed local drives as `C:\`-style roots, ascending.
pub fn fixed_local_drives() -> Result<Vec<String>> {
    // SAFETY: GetLogicalDrives takes no arguments and returns a bitmask of the
    // currently available logical drives (bit 0 = `A:`), or 0 on failure.
    let mask = unsafe { GetLogicalDrives() };
    if mask == 0 {
        return Err(last_error("GetLogicalDrives"));
    }
    Ok(fixed_roots(mask, get_drive_type))
}

/// Queries `GetDriveTypeW` for a drive root such as `C:\`; a vanished root
/// yields `DRIVE_NO_ROOT_DIR` so the filter drops it.
fn get_drive_type(root: &str) -> u32 {
    let wide = to_wide_null(root);
    // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives this call;
    // GetDriveTypeW only reads it.
    unsafe { GetDriveTypeW(wide.as_ptr()) }
}

/// The drive letters present in a `GetLogicalDrives` bitmask, ascending.
///
/// Bit 0 is `A`, bit 1 is `B`, ... bit 25 is `Z`.
fn drive_letters(mask: u32) -> Vec<char> {
    (0..26u8)
        .filter(|&index| mask & (1 << index) != 0)
        .map(|index| char::from(b'A' + index))
        .collect()
}

fn root_path(letter: char) -> String {
    format!("{letter}:\\")
}

/// Keeps the roots whose `drive_type` is `DRIVE_FIXED`, in ascending order.
///
/// `drive_type` is a parameter so the filtering is pure and testable.
fn fixed_roots(mask: u32, mut drive_type: impl FnMut(&str) -> u32) -> Vec<String> {
    drive_letters(mask)
        .into_iter()
        .map(root_path)
        .filter(|root| drive_type(root) == DRIVE_FIXED)
        .collect()
}

/// Encodes `text` as a NUL-terminated UTF-16 buffer for the `*W` Win32 APIs.
fn to_wide_null(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Builds an [`AppError::Win32`] carrying the calling thread's last-error code.
fn last_error(context: &str) -> AppError {
    // SAFETY: GetLastError takes no arguments and reads thread-local state only;
    // it is read immediately after the failing Win32 call.
    let code = unsafe { GetLastError() };
    AppError::Win32 {
        context: context.to_string(),
        code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Documented `GetDriveTypeW` results other than DRIVE_FIXED (see `DRIVE_FIXED`).
    const DRIVE_NO_ROOT_DIR: u32 = 1;
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_REMOTE: u32 = 4;
    const DRIVE_CDROM: u32 = 5;
    const DRIVE_RAMDISK: u32 = 6;

    #[test]
    fn drive_letters_map_bits_to_uppercase_letters() {
        assert_eq!(drive_letters(0), Vec::<char>::new());
        assert_eq!(drive_letters(0b1), vec!['A']);
        assert_eq!(drive_letters(0b100), vec!['C']);
        assert_eq!(drive_letters(0b1011), vec!['A', 'B', 'D']);
        // A full 32-bit mask still yields exactly the 26 real drive letters.
        assert_eq!(drive_letters(u32::MAX).len(), 26);
    }

    #[test]
    fn root_path_formats_a_letter_style_root() {
        assert_eq!(root_path('C'), "C:\\");
        assert_eq!(root_path('Z'), "Z:\\");
    }

    #[test]
    fn fixed_filter_excludes_non_fixed_drives_in_letter_order() {
        let mask = 0b11111; // A..E
        let roots = fixed_roots(mask, |root| match root {
            "A:\\" => DRIVE_REMOVABLE,
            "B:\\" => DRIVE_FIXED,
            "C:\\" => DRIVE_FIXED,
            "D:\\" => DRIVE_REMOTE,
            "E:\\" => DRIVE_CDROM,
            _ => DRIVE_NO_ROOT_DIR,
        });
        assert_eq!(roots, vec!["B:\\".to_string(), "C:\\".to_string()]);
    }

    #[test]
    fn a_vanished_drive_is_skipped_not_fatal() {
        // C: is fixed; D: disappears between enumeration and the type query.
        let mask = 0b1100; // C, D
        let roots = fixed_roots(mask, |root| match root {
            "C:\\" => DRIVE_FIXED,
            "D:\\" => DRIVE_NO_ROOT_DIR,
            _ => DRIVE_RAMDISK,
        });
        assert_eq!(roots, vec!["C:\\".to_string()]);
    }

    #[test]
    fn fixed_local_drives_returns_c_style_roots_and_is_non_empty() {
        let drives = fixed_local_drives().expect("enumerate fixed local drives");
        assert!(
            !drives.is_empty(),
            "expected at least one fixed local drive"
        );
        for drive in &drives {
            assert_eq!(drive.len(), 3, "root should be `C:\\`-style: {drive:?}");
            let bytes = drive.as_bytes();
            assert!(bytes[0].is_ascii_uppercase(), "drive letter: {drive:?}");
            assert_eq!(bytes[1], b':');
            assert_eq!(bytes[2], b'\\');
        }
    }
}
