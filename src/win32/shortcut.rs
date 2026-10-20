//! Windows desktop shortcut creation via the `IShellLink` COM API.
//!
//! Shortcuts are built with the proper Win32 `IShellLink` COM API, with the
//! desktop directory resolved through `SHGetFolderPathW(CSIDL_DESKTOPDIRECTORY)`
//! (so a OneDrive-redirected desktop resolves correctly). A shortcut may carry
//! command-line arguments and an explicit working directory; see
//! [`ShortcutTarget`].
//!
//! The `.lnk` naming rule, the working-directory default, the desktop-dir path
//! join and the buffer decoding are pure and unit-tested; every COM/FFI call is
//! sealed behind [`create_shortcut`] and [`create_shortcut_in`].

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows::core::{IUnknown, Interface, HSTRING};
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{IShellLinkW, SHGetFolderPathW, ShellLink, CSIDL_DESKTOPDIRECTORY};

use crate::error::{AppError, Result};

/// The shortcut file extension, appended when the caller's name lacks it.
const LNK_EXTENSION: &str = ".lnk";

/// `MAX_PATH`: the fixed buffer size `SHGetFolderPathW` writes into.
const MAX_PATH: usize = 260;

/// What a desktop shortcut points at.
///
/// `working_directory` defaults to the target's parent directory when omitted.
#[derive(Debug, Clone, Copy)]
pub struct ShortcutTarget<'a> {
    /// The executable (or file) the `.lnk` opens.
    pub path: &'a Path,
    /// Optional command-line arguments, stored verbatim (quotes included).
    pub arguments: Option<&'a str>,
    /// Optional working directory; defaults to [`Self::path`]'s parent.
    pub working_directory: Option<&'a Path>,
}

impl<'a> ShortcutTarget<'a> {
    /// A shortcut to `path` with no arguments and the default working
    /// directory (the target's parent).
    pub fn new(path: &'a Path) -> Self {
        Self {
            path,
            arguments: None,
            working_directory: None,
        }
    }

    /// Adds command-line arguments (stored verbatim).
    pub fn with_arguments(mut self, arguments: &'a str) -> Self {
        self.arguments = Some(arguments);
        self
    }

    /// Overrides the working directory (defaults to the target's parent).
    pub fn with_working_directory(mut self, working_directory: &'a Path) -> Self {
        self.working_directory = Some(working_directory);
        self
    }

    /// The working directory the `.lnk` carries (pure): the explicit one when
    /// given, else the target's parent.
    fn resolved_working_directory(&self) -> Option<&Path> {
        self.working_directory.or_else(|| self.path.parent())
    }
}

/// Creates a desktop shortcut named `name` targeting `target`.
///
/// The desktop directory is resolved with `SHGetFolderPathW(CSIDL_DESKTOPDIRECTORY)`.
/// `name` is the shortcut's base name (`"Zona"` yields `Zona.lnk`); `icon` is the
/// optional icon path (`None` leaves the default icon). Returns the full path of
/// the created `.lnk`.
pub fn create_shortcut(
    target: &ShortcutTarget<'_>,
    name: &str,
    icon: Option<&Path>,
) -> Result<PathBuf> {
    let desktop = desktop_directory()?;
    create_shortcut_in(&desktop, target, name, icon)
}

/// Creates a shortcut in an explicit directory (the testable seam).
///
/// This is the directory-parameterised inner function that [`create_shortcut`]
/// calls after resolving the real Desktop. Keeping the directory as a parameter
/// lets the unit test write into a temp dir and never touch the real Desktop.
fn create_shortcut_in(
    directory: &Path,
    target: &ShortcutTarget<'_>,
    name: &str,
    icon: Option<&Path>,
) -> Result<PathBuf> {
    let link_path = resolve_link_path(directory, name);

    // The calling thread must be in a COM apartment; the guard balances
    // CoUninitialize on drop (and is a no-op when another model is already set).
    let _com = ComGuard::initialize()?;

    // SAFETY: every call below is made under a valid COM apartment established
    // by `ComGuard`. `CoCreateInstance` returns an owned `IShellLinkW`; the
    // `Set*` methods only read the borrowed, NUL-terminated `HSTRING`s (which
    // outlive each call) and write them into the shell-link object; `cast`
    // performs a QueryInterface for `IPersistFile`; `Save(true)` writes the .lnk
    // to disk. Every HRESULT is checked via `map_err`, so a failure is never
    // ignored.
    unsafe {
        let link: IShellLinkW =
            CoCreateInstance(&ShellLink, None::<&IUnknown>, CLSCTX_INPROC_SERVER)
                .map_err(|e| win32_error("CoCreateInstance(ShellLink)", e))?;

        link.SetPath(&HSTRING::from(target.path))
            .map_err(|e| win32_error("IShellLink::SetPath", e))?;

        if let Some(arguments) = target.arguments {
            link.SetArguments(&HSTRING::from(arguments))
                .map_err(|e| win32_error("IShellLink::SetArguments", e))?;
        }

        if let Some(working_directory) = target.resolved_working_directory() {
            link.SetWorkingDirectory(&HSTRING::from(working_directory))
                .map_err(|e| win32_error("IShellLink::SetWorkingDirectory", e))?;
        }

        if let Some(icon) = icon {
            link.SetIconLocation(&HSTRING::from(icon), 0)
                .map_err(|e| win32_error("IShellLink::SetIconLocation", e))?;
        }

        let persist: IPersistFile = link
            .cast()
            .map_err(|e| win32_error("IShellLink as IPersistFile", e))?;
        persist
            .Save(&HSTRING::from(link_path.as_path()), true)
            .map_err(|e| win32_error("IPersistFile::Save", e))?;
    }

    // A successful Save must have produced the file; if it did not, surface a
    // real error rather than returning a path that does not exist.
    if !link_path.exists() {
        return Err(AppError::Io {
            context: "shortcut file was not created".to_string(),
            path: Some(link_path),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "IShellLink reported success but no .lnk appeared",
            ),
        });
    }

    Ok(link_path)
}

/// Resolves the current user's Desktop directory via
/// `SHGetFolderPathW(CSIDL_DESKTOPDIRECTORY)`.
fn desktop_directory() -> Result<PathBuf> {
    let mut buffer = [0u16; MAX_PATH];
    // SAFETY: `buffer` is a live, writable `[u16; MAX_PATH]` (the exact array
    // shape SHGetFolderPathW expects) that outlives the call; `None` hwnd/token
    // and a `dwFlags` of 0 request the current user's desktop directory. The
    // call only writes into the provided buffer.
    unsafe { SHGetFolderPathW(None, CSIDL_DESKTOPDIRECTORY as i32, None, 0, &mut buffer) }
        .map_err(|e| win32_error("SHGetFolderPathW(CSIDL_DESKTOPDIRECTORY)", e))?;
    Ok(nul_terminated_path(&buffer))
}

/// Builds a [`PathBuf`] from a NUL-terminated wide buffer, stopping at the first
/// NUL (pure).
fn nul_terminated_path(buffer: &[u16]) -> PathBuf {
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    PathBuf::from(OsString::from_wide(&buffer[..end]))
}

/// Appends the `.lnk` extension to `name` unless it already carries it
/// (case-insensitive) (pure).
fn link_file_name(name: &str) -> String {
    if name.to_ascii_lowercase().ends_with(LNK_EXTENSION) {
        name.to_string()
    } else {
        format!("{name}{LNK_EXTENSION}")
    }
}

/// The full path of the shortcut: `directory` joined with the `.lnk` file name
/// derived from `name` (pure).
fn resolve_link_path(directory: &Path, name: &str) -> PathBuf {
    directory.join(link_file_name(name))
}

/// Maps a `windows`-crate [`windows::core::Error`] to the central
/// [`AppError::Win32`], carrying the HRESULT as the raw code.
fn win32_error(context: &str, error: windows::core::Error) -> AppError {
    AppError::Win32 {
        context: context.to_string(),
        code: error.code().0 as u32,
    }
}

/// RAII guard that initializes COM on the calling thread and balances
/// `CoUninitialize` on drop.
struct ComGuard {
    /// Whether this guard actually initialized the apartment (and must hence
    /// uninitialize it). `false` when the thread was already in a *different*
    /// apartment model.
    uninitialize: bool,
}

impl ComGuard {
    /// Initializes an apartment-threaded COM apartment for the calling thread.
    ///
    /// An already-initialized thread is tolerated: `RPC_E_CHANGED_MODE` (the
    /// thread is already in another apartment model) is not fatal — COM is still
    /// usable and this guard must not uninitialize an apartment it did not
    /// create — while any other failure is a real [`AppError::Win32`].
    fn initialize() -> Result<Self> {
        // SAFETY: CoInitializeEx initializes COM for the calling thread and
        // dereferences nothing (`None` reserved pointer). Its HRESULT is checked
        // below.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr.is_ok() {
            Ok(Self { uninitialize: true })
        } else if hr == RPC_E_CHANGED_MODE {
            Ok(Self {
                uninitialize: false,
            })
        } else {
            Err(AppError::Win32 {
                context: "CoInitializeEx".to_string(),
                code: hr.0 as u32,
            })
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.uninitialize {
            // SAFETY: balanced with the CoInitializeEx above that returned a
            // success HRESULT on this same thread.
            unsafe { CoUninitialize() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A unique, empty temp directory for one test, removed by the caller.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("zona_installer_shortcut_{tag}_{pid}_{nanos}_{n}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn creates_a_lnk_in_a_temp_dir_pointing_at_a_temp_target() {
        let dir = unique_temp_dir("basic");
        let target = dir.join("modorganizer.exe");
        std::fs::write(&target, b"stub executable").expect("write temp target");

        let result = create_shortcut_in(&dir, &ShortcutTarget::new(&target), "Zona", None);

        let link = result.expect("create shortcut");
        assert_eq!(link, dir.join("Zona.lnk"));
        assert!(link.exists(), "the .lnk file must exist on disk");
        // A real shell link carries a header and item-id list, so it is never
        // empty; a zero-byte file would mean Save did not actually write one.
        let len = std::fs::metadata(&link).expect("stat .lnk").len();
        assert!(len > 0, "the .lnk file should not be empty (len={len})");

        std::fs::remove_dir_all(&dir).expect("cleanup temp dir");
    }

    #[test]
    fn creates_a_shortcut_with_an_explicit_icon() {
        let dir = unique_temp_dir("icon");
        let target = dir.join("modorganizer.exe");
        std::fs::write(&target, b"stub executable").expect("write temp target");
        let icon = dir.join("icon.ico");
        std::fs::write(&icon, b"not a real icon").expect("write temp icon");

        let link = create_shortcut_in(&dir, &ShortcutTarget::new(&target), "Zona", Some(&icon))
            .expect("create shortcut");
        assert!(link.exists());

        std::fs::remove_dir_all(&dir).expect("cleanup temp dir");
    }

    #[test]
    fn shortcut_target_keeps_the_arguments_and_explicit_working_directory() {
        let exe = Path::new(r"C:\Zona\mo2\ModOrganizer.exe");
        let target = ShortcutTarget::new(exe)
            .with_arguments("\"moshortcut://:Anomaly (DX11-AVX)\"")
            .with_working_directory(Path::new(r"C:\Zona\mo2"));

        assert_eq!(target.path, exe);
        assert_eq!(
            target.arguments,
            Some("\"moshortcut://:Anomaly (DX11-AVX)\"")
        );
        assert_eq!(
            target.resolved_working_directory(),
            Some(Path::new(r"C:\Zona\mo2"))
        );
    }

    #[test]
    fn resolved_working_directory_defaults_to_the_target_parent() {
        let target = ShortcutTarget::new(Path::new(r"C:\Zona\mo2\ModOrganizer.exe"));
        assert_eq!(
            target.resolved_working_directory(),
            Some(Path::new(r"C:\Zona\mo2"))
        );
        assert_eq!(target.arguments, None);
    }

    #[test]
    fn link_file_name_appends_the_lnk_extension_once() {
        assert_eq!(link_file_name("Zona"), "Zona.lnk");
        assert_eq!(link_file_name("Zona.lnk"), "Zona.lnk");
        assert_eq!(link_file_name("Zona.LNK"), "Zona.LNK");
        assert_eq!(link_file_name("my.game"), "my.game.lnk");
    }

    #[test]
    fn resolve_link_path_joins_the_directory_and_the_lnk_name() {
        let path = resolve_link_path(Path::new(r"C:\Users\me\Desktop"), "Zona");
        assert_eq!(path, PathBuf::from(r"C:\Users\me\Desktop\Zona.lnk"));
    }

    #[test]
    fn nul_terminated_path_stops_at_the_first_nul() {
        let wide: Vec<u16> = "C:\\Zona"
            .encode_utf16()
            .chain([0u16])
            .chain([65u16])
            .collect();
        assert_eq!(nul_terminated_path(&wide), PathBuf::from("C:\\Zona"));
        // No NUL at all: the whole buffer is used.
        assert_eq!(nul_terminated_path(&[65u16, 66u16]), PathBuf::from("AB"));
    }
}
