//! Win32 relocation and the self-delete trick.
//!
//! When the binary is not yet at `<drive>:\Zona\installer`, the pre-flight flow
//! asks the user for a drive and relocates. Only the installer binary and the
//! `dependencies\` folder move; `downloads\` and everything else stay behind. If
//! `<drive>:\Zona\installer` already exists the move does not happen: [`relocate`]
//! reports [`RelocateOutcome::TargetExists`] and the app core decides
//! delete-or-cancel. Only the installer folder is ever deleted, so sibling game
//! folders (`mo2\`, `anomaly\`) under `<drive>:\Zona` are left untouched.
//!
//! Windows cannot delete the folder a running executable was started from, so
//! the old process spawns the new (relocated) copy with a `/delete <old-dir>`
//! argument and exits; the new copy deletes the old folder. [`delete_old_folder`]
//! validates its `/delete` argument before deleting anything.
//!
//! Relocation never runs under the debug root override ([`relocate`] returns
//! [`RelocateOutcome::Disabled`]).

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf, Prefix};

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_COPY_ALLOWED, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

use crate::error::{AppError, Result};
use crate::paths::{detect_root_mode, RootMode};

/// The fixed installer binary file name.
pub const INSTALLER_BINARY: &str = "zona-installer.exe";

/// The command-line flag that tells the relocated copy to delete the old folder.
pub const SELF_DELETE_FLAG: &str = "/delete";

/// The `dependencies\` folder moved together with the binary.
const DEPENDENCIES_DIR: &str = "dependencies";

/// Outcome of a [`relocate`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelocateOutcome {
    /// The debug root override is active; nothing was moved or spawned.
    Disabled,
    /// `<chosen>:\Zona\installer` already exists; the app core decides
    /// delete-or-cancel.
    TargetExists {
        /// `<chosen>:\Zona\installer`.
        install_root: PathBuf,
    },
    /// The binary and `dependencies\` were moved.
    Relocated {
        /// `<drive>:\Zona\installer`.
        install_root: PathBuf,
        /// `<drive>:\Zona\installer\zona-installer.exe`.
        new_exe: PathBuf,
        /// The old installer folder (the directory the running copy came from).
        old_dir: PathBuf,
    },
}

/// Command line for the relocated copy's self-delete launch (pure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfDeleteCommand {
    /// The relocated executable to spawn.
    pub program: PathBuf,
    /// The arguments passed to it: `/delete <old_dir>`.
    pub args: Vec<OsString>,
}

/// Relocates the installer from `exe_dir` to `<chosen_drive>:\Zona\installer`.
///
/// `chosen_drive` is a drive root such as `C:\`. The move covers exactly the
/// binary ([`INSTALLER_BINARY`]) and `dependencies\`; `downloads\` is left in
/// `exe_dir`. This performs the target-existence check but does NOT delete
/// `<chosen>:\Zona\installer` when it exists; the relocated copy deletes the old
/// folder via the self-delete arguments.
pub fn relocate(exe_dir: &Path, chosen_drive: &Path) -> Result<RelocateOutcome> {
    if detect_root_mode(exe_dir).relocation_disabled() {
        return Ok(RelocateOutcome::Disabled);
    }
    relocate_to(exe_dir, &zona_dir_for_drive(chosen_drive))
}

/// The Zona directory for a chosen drive root: `<chosen_drive>Zona` (pure).
pub fn zona_dir_for_drive(chosen_drive: &Path) -> PathBuf {
    drive_root(chosen_drive).join("Zona")
}

/// The install root for a chosen drive root:
/// `<chosen_drive>Zona\installer` (pure).
pub fn install_root_for_drive(chosen_drive: &Path) -> PathBuf {
    zona_dir_for_drive(chosen_drive).join("installer")
}

/// Builds the self-delete command for the relocated copy (pure).
///
/// The new copy is launched as `<new_exe> /delete <old_dir>`.
pub fn self_delete_command(new_exe: &Path, old_dir: &Path) -> SelfDeleteCommand {
    SelfDeleteCommand {
        program: new_exe.to_path_buf(),
        args: vec![
            OsString::from(SELF_DELETE_FLAG),
            old_dir.as_os_str().to_os_string(),
        ],
    }
}

/// Spawns the relocated copy with the self-delete arguments (impure).
///
/// The child is intentionally not joined/waited: the old process exits right
/// after this and the new copy continues on its own.
pub fn spawn_new_copy(command: &SelfDeleteCommand) -> Result<()> {
    let mut cmd = std::process::Command::new(&command.program);
    cmd.args(&command.args);
    // The child must NOT inherit this process's cwd: that is the old folder it is
    // about to delete.
    if let Some(dir) = command.program.parent() {
        cmd.current_dir(dir);
    }
    cmd.spawn().map_err(|source| AppError::Io {
        context: "spawn relocated installer".to_string(),
        path: Some(command.program.clone()),
        source,
    })?;
    Ok(())
}

/// Deletes the old installer folder (the receiving half of the self-delete
/// trick, run by the new copy when started with [`SELF_DELETE_FLAG`]).
///
/// `old_dir` arrives from the `/delete <path>` command line, so nothing is
/// deleted unless [`validate_old_folder`] accepts the path first. After the
/// pure shape rules, this asks the filesystem whether `old_dir` resolves to the
/// folder the running executable was started from ([`resolves_to_same_folder`]),
/// so an alias (`SUBST`, a mapped drive letter, a directory junction) cannot
/// make the copy delete its own directory.
pub fn delete_old_folder(old_dir: &Path) -> Result<()> {
    let exe_dir = std::env::current_exe()
        .map_err(|source| io_error("locate running executable", old_dir, source))?
        .parent()
        .map(Path::to_path_buf);
    let mode_arg = exe_dir.as_deref().unwrap_or(old_dir);
    let dev_root = match detect_root_mode(mode_arg) {
        RootMode::DevOverride(root) => Some(root),
        RootMode::Shipped => None,
    };
    validate_old_folder(old_dir, exe_dir.as_deref(), dev_root.as_deref())?;
    if let Some(exe_dir) = exe_dir.as_deref() {
        if resolves_to_same_folder(old_dir, exe_dir) {
            return Err(refusal_error(
                old_dir,
                "refusing to delete a folder that resolves to the running executable's own \
                 directory",
            ));
        }
    }
    std::fs::remove_dir_all(old_dir)
        .map_err(|source| io_error("remove old installer folder", old_dir, source))
}

/// Validates a `/delete <path>` argument before anything is deleted (pure).
///
/// This is the testable core of [`delete_old_folder`]: `exe_dir` and `dev_root`
/// are parameters so the rules can be exercised without touching
/// process-global state (`None` skips the corresponding check; the public
/// wrapper always supplies both). The path is accepted only when it is a local
/// drive-letter path (a `Component::Prefix` of `Prefix::Disk`, so UNC,
/// verbatim and device forms are refused), contains no `..`
/// (`Component::ParentDir`) component, is absolute, is shaped
/// `<anything>\installer` (final component `installer`, case-insensitive), is
/// not the running executable's own directory, and is not under the debug root
/// override. No filesystem call is made here.
fn validate_old_folder(
    old_dir: &Path,
    exe_dir: Option<&Path>,
    dev_root: Option<&Path>,
) -> Result<()> {
    if !old_dir.is_absolute() {
        return Err(refusal_error(old_dir, "refusing to delete a relative path"));
    }
    if !is_local_drive_letter_path(old_dir) {
        return Err(refusal_error(
            old_dir,
            "refusing to delete a path that is not a local drive-letter path",
        ));
    }
    if old_dir
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(refusal_error(
            old_dir,
            "refusing to delete a path containing a `..` component",
        ));
    }
    let is_installer = old_dir
        .file_name()
        .map(|name| os_str_eq_ignore_ascii_case(name, OsStr::new("installer")))
        .unwrap_or(false);
    if !is_installer {
        return Err(refusal_error(
            old_dir,
            "refusing to delete a path not shaped <anything>\\installer",
        ));
    }
    if let Some(exe) = exe_dir {
        if paths_eq_ignore_case(old_dir, exe) {
            return Err(refusal_error(
                old_dir,
                "refusing to delete the running executable's own directory",
            ));
        }
    }
    if let Some(root) = dev_root {
        if path_is_under(old_dir, root) {
            return Err(refusal_error(
                old_dir,
                "refusing to delete a path under the debug root override",
            ));
        }
    }
    Ok(())
}

/// The directory-parameterised relocation core (the testable seam).
///
/// `zona_dir` is `<chosen>:\Zona`. Splitting this out of [`relocate`] lets tests
/// exercise the target-existence check and the move with a temporary directory
/// instead of a real drive path. Only the `installer` sub-directory is checked
/// and created, so a pre-existing game install (`mo2\`, `anomaly\`) under
/// `zona_dir` is left alone.
fn relocate_to(exe_dir: &Path, zona_dir: &Path) -> Result<RelocateOutcome> {
    let install_root = zona_dir.join("installer");
    if install_root.exists() {
        return Ok(RelocateOutcome::TargetExists { install_root });
    }

    let new_exe = install_root.join(INSTALLER_BINARY);
    let old_dir = exe_dir.to_path_buf();

    create_dir_all(&install_root)?;

    // COPY, never move. The running exe cannot be deleted (and a cross-volume
    // `MoveFileExW` is copy + delete, so it fails on the delete half), and a move
    // that fails midway would strand the install with its `dependencies\` gone.
    // The source is left intact; the relocated copy deletes the old folder via
    // the `/delete` hand-off once this process has exited. Dependencies first,
    // then the binary, so the binary arriving marks a complete install. On any
    // failure the half-built target is removed (it did not exist before, checked
    // above) so the source install stays usable and a retry starts clean.
    let copied = copy_tree(
        &exe_dir.join(DEPENDENCIES_DIR),
        &install_root.join(DEPENDENCIES_DIR),
    )
    .and_then(|()| copy_file(&exe_dir.join(INSTALLER_BINARY), &new_exe));
    if let Err(error) = copied {
        let _ = std::fs::remove_dir_all(&install_root);
        return Err(error);
    }

    Ok(RelocateOutcome::Relocated {
        install_root,
        new_exe,
        old_dir,
    })
}

/// Ensures `chosen_drive` is a rooted path ending in a separator (pure).
///
/// The drive picker yields `C:\`, but a bare `C:` (or a temporary directory used
/// as a stand-in in tests) has no trailing separator, so joining would otherwise
/// produce a drive-relative path.
fn drive_root(chosen_drive: &Path) -> PathBuf {
    let text = chosen_drive.as_os_str().to_string_lossy();
    if text.ends_with('\\') || text.ends_with('/') {
        chosen_drive.to_path_buf()
    } else {
        let mut rooted = chosen_drive.as_os_str().to_os_string();
        rooted.push("\\");
        PathBuf::from(rooted)
    }
}

/// Case-insensitive comparison of two [`OsStr`]s, lossy on both sides (pure).
fn os_str_eq_ignore_ascii_case(a: &OsStr, b: &OsStr) -> bool {
    a.to_string_lossy()
        .eq_ignore_ascii_case(&b.to_string_lossy())
}

/// Case-insensitive component-wise comparison of two paths (pure).
///
/// Comparing `Component`s (rather than the raw strings) makes the comparison
/// independent of trailing separators and of drive-letter case.
fn paths_eq_ignore_case(a: &Path, b: &Path) -> bool {
    let a: Vec<_> = a.components().collect();
    let b: Vec<_> = b.components().collect();
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| os_str_eq_ignore_ascii_case(x.as_os_str(), y.as_os_str()))
}

/// Whether `path` equals or sits under `ancestor` (pure, case-insensitive,
/// component-wise).
///
/// Component-wise matching means `C:\Zona` is NOT "under" `C:\ZonaX` (a raw
/// string-prefix comparison would wrongly match). No filesystem access is
/// made.
pub(crate) fn path_is_under(path: &Path, ancestor: &Path) -> bool {
    let path: Vec<_> = path.components().collect();
    let ancestor: Vec<_> = ancestor.components().collect();
    path.len() >= ancestor.len()
        && ancestor
            .iter()
            .zip(path.iter())
            .all(|(x, y)| os_str_eq_ignore_ascii_case(x.as_os_str(), y.as_os_str()))
}

/// Whether two paths resolve to the same folder on disk (impure).
///
/// The pure shape rules in [`validate_old_folder`] compare spellings, so they
/// cannot see two different texts naming one folder (`SUBST Z: C:\` making
/// `Z:\Zona\installer` and `C:\Zona\installer` the same directory, a mapped drive
/// letter, or a directory junction). Both sides are canonicalized *before* the
/// comparison: `fs::canonicalize` returns a `\\?\`-prefixed path, so a canonical
/// form compared against a raw one could never be equal and the whole check would
/// be silently inert. The comparison itself is the module's case-insensitive
/// [`paths_eq_ignore_case`].
///
/// When either side cannot be canonicalized (typically because it does not exist)
/// this falls back to the pure [`paths_eq_ignore_case`] verdict, so the check adds
/// no new failure mode; `remove_dir_all` still decides.
fn resolves_to_same_folder(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(canonical_a), Ok(canonical_b)) => paths_eq_ignore_case(&canonical_a, &canonical_b),
        _ => paths_eq_ignore_case(a, b),
    }
}

/// Whether `path` begins with a local drive-letter prefix (`C:`) (pure).
///
/// Only `Prefix::Disk` is accepted, so UNC (`\\server\share\...`), verbatim
/// (`\\?\C:\...`) and device (`\\.\C:\...`) forms — including the local
/// admin-share aliases that spell the same folder as `\\127.0.0.1\C$\...` —
/// are refused. No filesystem access is made.
fn is_local_drive_letter_path(path: &Path) -> bool {
    matches!(
        path.components().next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))
    )
}

/// Copies `src` to `dst`, recursing into directories (impure).
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    let metadata = std::fs::metadata(src).map_err(|source| io_error("stat source", src, source))?;
    if metadata.is_dir() {
        create_dir_all(dst)?;
        let entries = std::fs::read_dir(src)
            .map_err(|source| io_error("read source directory", src, source))?;
        for entry in entries {
            let entry =
                entry.map_err(|source| io_error("read source directory entry", src, source))?;
            copy_tree(&entry.path(), &dst.join(entry.file_name().as_os_str()))?;
        }
        Ok(())
    } else {
        copy_file(src, dst)
    }
}

/// Copies one file, creating the destination directory (impure).
fn copy_file(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        create_dir_all(parent)?;
    }
    std::fs::copy(src, dst)
        .map(|_| ())
        .map_err(|source| io_error("copy file", src, source))
}

/// Moves `src` to `dst`, recursing into directories (impure).
#[allow(dead_code)]
///
/// Files are moved with `MoveFileExW(MOVEFILE_COPY_ALLOWED)` so the move also
/// works across volumes (a plain rename fails with `ERROR_NOT_SAME_DEVICE` when
/// the chosen drive differs from the source drive — the normal relocation case).
fn move_path(src: &Path, dst: &Path) -> Result<()> {
    let metadata = std::fs::metadata(src).map_err(|source| io_error("stat source", src, source))?;
    if metadata.is_dir() {
        create_dir_all(dst)?;
        let entries = std::fs::read_dir(src)
            .map_err(|source| io_error("read source directory", src, source))?;
        for entry in entries {
            let entry =
                entry.map_err(|source| io_error("read source directory entry", src, source))?;
            move_path(&entry.path(), &dst.join(entry.file_name().as_os_str()))?;
        }
        std::fs::remove_dir(src)
            .map_err(|source| io_error("remove source directory", src, source))?;
        Ok(())
    } else {
        move_file(src, dst)
    }
}

/// Moves a single file to `dst`, creating the destination directory (impure).
#[allow(dead_code)]
fn move_file(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        create_dir_all(parent)?;
    }
    let src_wide = to_wide_null(src);
    let dst_wide = to_wide_null(dst);
    // SAFETY: `src_wide` and `dst_wide` are NUL-terminated UTF-16 buffers that
    // outlive the call; MoveFileExW only reads them. `MOVEFILE_COPY_ALLOWED`
    // makes the move work across volumes, `MOVEFILE_REPLACE_EXISTING` overwrites
    // any leftover destination file, and `MOVEFILE_WRITE_THROUGH` flushes a
    // cross-volume copy before returning so the relocated exe is fully on disk
    // before it is spawned.
    let moved = unsafe {
        MoveFileExW(
            src_wide.as_ptr(),
            dst_wide.as_ptr(),
            MOVEFILE_COPY_ALLOWED | MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(last_error("MoveFileExW"));
    }
    Ok(())
}

/// Creates `path` (and parents), mapping failures to [`AppError::Io`].
fn create_dir_all(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|source| io_error("create directory", path, source))
}

/// Encodes `path` as a NUL-terminated UTF-16 buffer for the `*W` Win32 APIs.
fn to_wide_null(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Builds an [`AppError::Io`] carrying the operation's context and path.
fn io_error(context: &str, path: &Path, source: std::io::Error) -> AppError {
    AppError::Io {
        context: context.to_string(),
        path: Some(path.to_path_buf()),
        source,
    }
}

/// Builds an [`AppError::Win32`] carrying the calling thread's last-error code.
fn last_error(context: &str) -> AppError {
    // SAFETY: GetLastError reads thread-local state only and is read immediately
    // after the failing Win32 call.
    let code = unsafe { GetLastError() };
    AppError::Win32 {
        context: context.to_string(),
        code,
    }
}

/// Builds the [`AppError::Io`] for a refused `/delete` target.
///
/// [`AppError`] has no dedicated invalid-argument variant, so refusals surface as
/// [`AppError::Io`] carrying an `InvalidInput` source.
fn refusal_error(old_dir: &Path, message: &str) -> AppError {
    AppError::Io {
        context: "validate old installer folder".to_string(),
        path: Some(old_dir.to_path_buf()),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, message),
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
            std::env::temp_dir().join(format!("zona_installer_relocate_{tag}_{pid}_{nanos}_{n}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Writes `contents` to `path`, creating any parent directories first.
    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture parent dir");
        }
        std::fs::write(path, contents).expect("write fixture file");
    }

    /// RAII guard that clears `ZONA_INSTALLER_DEV_ROOT` for one test and
    /// restores the previous value on drop.
    ///
    /// The variable name is a private const in `paths.rs`, so the literal
    /// string is used here. The shared static mutex serializes every
    /// env-touching test: environment variables are process-global while tests
    /// run on parallel threads. Edition 2021, so `set_var`/`remove_var` are
    /// safe functions (no `unsafe`).
    struct DevRootGuard {
        previous: Option<OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    impl DevRootGuard {
        fn new() -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = std::env::var_os("ZONA_INSTALLER_DEV_ROOT");
            std::env::remove_var("ZONA_INSTALLER_DEV_ROOT");
            DevRootGuard {
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for DevRootGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var("ZONA_INSTALLER_DEV_ROOT", value),
                None => std::env::remove_var("ZONA_INSTALLER_DEV_ROOT"),
            }
        }
    }

    #[test]
    fn drive_root_ensures_a_trailing_separator() {
        assert_eq!(drive_root(Path::new("C:\\")), PathBuf::from("C:\\"));
        assert_eq!(drive_root(Path::new("C:")), PathBuf::from("C:\\"));
        assert_eq!(
            drive_root(Path::new(r"D:\sandbox\drive")),
            PathBuf::from(r"D:\sandbox\drive\")
        );
    }

    #[test]
    fn zona_dir_and_install_root_derive_from_the_drive() {
        assert_eq!(
            zona_dir_for_drive(Path::new("C:\\")),
            PathBuf::from("C:\\Zona")
        );
        assert_eq!(
            install_root_for_drive(Path::new("C:\\")),
            PathBuf::from("C:\\Zona\\installer")
        );
    }

    #[test]
    fn self_delete_command_produces_new_exe_and_old_dir_arguments() {
        let command = self_delete_command(
            Path::new(r"D:\Zona\installer\zona-installer.exe"),
            Path::new(r"C:\zona-installer-old"),
        );
        assert_eq!(
            command.program,
            PathBuf::from(r"D:\Zona\installer\zona-installer.exe")
        );
        assert_eq!(
            command.args,
            vec![
                OsString::from("/delete"),
                OsString::from(r"C:\zona-installer-old")
            ]
        );
    }

    #[test]
    fn relocate_moves_only_binary_and_dependencies() {
        let _guard = DevRootGuard::new();
        let exe_dir = unique_temp_dir("src");
        let drive_root = unique_temp_dir("drive");

        // Source tree: the binary and dependencies must move; downloads/ and
        // logs/ must stay behind.
        write(&exe_dir.join("zona-installer.exe"), "fake exe");
        write(&exe_dir.join("dependencies/aria2c.exe"), "aria2c");
        write(&exe_dir.join("dependencies/7za.exe"), "7z");
        write(&exe_dir.join("dependencies/config.yaml"), "config");
        write(
            &exe_dir.join("downloads/zona_multipart.7z.001"),
            "large payload",
        );
        write(&exe_dir.join("logs/run.log"), "log line");

        let outcome = relocate(&exe_dir, &drive_root).expect("relocate");

        let (install_root, new_exe, old_dir) = match outcome {
            RelocateOutcome::Relocated {
                install_root,
                new_exe,
                old_dir,
            } => (install_root, new_exe, old_dir),
            other => panic!("expected Relocated, got {other:?}"),
        };

        assert_eq!(install_root, drive_root.join("Zona").join("installer"));
        assert_eq!(new_exe, install_root.join("zona-installer.exe"));
        assert_eq!(old_dir, exe_dir);

        // Destination has the binary and the whole dependencies tree...
        assert!(install_root.join("zona-installer.exe").exists());
        assert!(install_root.join("dependencies/aria2c.exe").exists());
        assert!(install_root.join("dependencies/7za.exe").exists());
        assert!(install_root.join("dependencies/config.yaml").exists());
        // ...but not downloads/ or logs/.
        assert!(!install_root.join("downloads").exists());
        assert!(!install_root.join("logs").exists());

        // Source: binary and dependencies are gone; downloads/ and logs/ remain.
        // Copy semantics: the source stays until the relocated copy deletes it.
        assert!(exe_dir.join("zona-installer.exe").exists());
        assert!(exe_dir.join("dependencies").exists());
        assert!(exe_dir.join("downloads/zona_multipart.7z.001").exists());
        assert!(exe_dir.join("logs/run.log").exists());

        std::fs::remove_dir_all(&exe_dir).expect("cleanup src");
        std::fs::remove_dir_all(&drive_root).expect("cleanup drive");
    }

    #[test]
    fn relocate_reports_target_exists_and_does_not_delete() {
        let _guard = DevRootGuard::new();
        let exe_dir = unique_temp_dir("src");
        let drive_root = unique_temp_dir("drive");
        let zona_dir = drive_root.join("Zona");
        let install_root = zona_dir.join("installer");

        write(&exe_dir.join("zona-installer.exe"), "fake exe");
        write(&exe_dir.join("dependencies/aria2c.exe"), "aria2c");
        write(&install_root.join("marker.txt"), "pre-existing");

        let outcome = relocate(&exe_dir, &drive_root).expect("relocate");

        match outcome {
            RelocateOutcome::TargetExists { install_root: reported } => {
                assert_eq!(reported, install_root)
            }
            other => panic!("expected TargetExists, got {other:?}"),
        }

        // The pre-existing installer directory is untouched...
        assert!(install_root.join("marker.txt").exists());
        // ...and nothing was moved out of the source.
        assert!(exe_dir.join("zona-installer.exe").exists());
        assert!(exe_dir.join("dependencies/aria2c.exe").exists());

        std::fs::remove_dir_all(&exe_dir).expect("cleanup src");
        std::fs::remove_dir_all(&drive_root).expect("cleanup drive");
    }

    #[test]
    fn spawn_new_copy_reports_an_error_for_a_missing_program() {
        let command = SelfDeleteCommand {
            program: PathBuf::from(r"Z:\does\not\exist\zona-installer.exe"),
            args: vec![
                OsString::from("/delete"),
                OsString::from(r"C:\zona-installer-old"),
            ],
        };
        assert!(spawn_new_copy(&command).is_err());
    }

    #[test]
    fn delete_old_folder_removes_the_directory() {
        let _guard = DevRootGuard::new();
        let base = unique_temp_dir("del");
        let zona = base.join("Zona");
        let old_dir = zona.join("installer");
        write(&old_dir.join("keep.txt"), "x");
        delete_old_folder(&old_dir).expect("delete old folder");
        assert!(!old_dir.exists());
        // Only the installer dir itself is deleted; the Zona parent remains.
        assert!(zona.exists());
        std::fs::remove_dir_all(&base).expect("cleanup del");
    }

    #[test]
    fn delete_old_folder_refuses_a_drive_root() {
        let _guard = DevRootGuard::new();
        // A drive root has no file_name, so it can never match the shape.
        assert!(delete_old_folder(Path::new("C:\\")).is_err());
    }

    #[test]
    fn delete_old_folder_refuses_an_absolute_path_of_the_wrong_shape() {
        let _guard = DevRootGuard::new();
        assert!(delete_old_folder(Path::new("C:\\Windows")).is_err());
    }

    #[test]
    fn delete_old_folder_refuses_a_relative_path() {
        let _guard = DevRootGuard::new();
        assert!(delete_old_folder(Path::new("Zona\\installer")).is_err());
    }

    #[test]
    fn delete_old_folder_accepts_any_installer_named_dir_and_refuses_other_names() {
        let _guard = DevRootGuard::new();
        let base = unique_temp_dir("shape");

        // Any directory whose last component is `installer` is deleted, whatever
        // its parent: a `Downloads\installer` copy is cleaned up after relocation.
        let downloads_copy = base.join("Downloads").join("installer");
        let marker = downloads_copy.join("keep.txt");
        write(&marker, "x");
        delete_old_folder(&downloads_copy).expect("delete a Downloads copy");
        assert!(!downloads_copy.exists());

        // A directory whose last component is not `installer` is still refused.
        let wrong_child = base.join("Zona").join("something");
        let marker = wrong_child.join("keep.txt");
        write(&marker, "x");
        assert!(delete_old_folder(&wrong_child).is_err());
        assert!(wrong_child.exists());
        assert!(marker.exists());

        std::fs::remove_dir_all(&base).expect("cleanup shape");
    }

    #[test]
    fn delete_old_folder_refuses_the_running_executable_directory() {
        let _guard = DevRootGuard::new();
        let exe_dir = std::env::current_exe()
            .expect("current exe")
            .parent()
            .map(Path::to_path_buf)
            .expect("exe parent");
        assert!(delete_old_folder(&exe_dir).is_err());
    }

    /// A `\\?\`-verbatim spelling of `path` — the form `fs::canonicalize` returns
    /// on Windows — built from the raw `OsStr` so no lossy conversion is involved.
    fn verbatim(path: &Path) -> PathBuf {
        let mut spelling = OsString::from(r"\\?\");
        spelling.push(path.as_os_str());
        PathBuf::from(spelling)
    }

    /// Creates a directory junction `link` -> `target` via `cmd /c mklink /J`,
    /// which needs no elevation (unlike a symbolic link). Returns whether it
    /// succeeded.
    fn create_junction(link: &Path, target: &Path) -> bool {
        std::process::Command::new("cmd")
            .arg("/c")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn resolves_to_same_folder_matches_case_and_verbatim_spellings() {
        let dir = unique_temp_dir("samefolder");
        let verbatim_dir = verbatim(&dir);
        let upper = PathBuf::from(dir.as_os_str().to_string_lossy().to_uppercase());

        // A `\\?\`-verbatim spelling and an uppercase spelling both name `dir`.
        assert!(resolves_to_same_folder(&dir, &verbatim_dir));
        assert!(resolves_to_same_folder(&dir, &upper));

        // The raw comparison the fallback uses CANNOT match the verbatim spelling,
        // so the assertion above only holds because BOTH sides are canonicalized;
        // canonicalizing one side alone would make this test fail.
        assert!(!paths_eq_ignore_case(&dir, &verbatim_dir));

        std::fs::remove_dir_all(&dir).expect("cleanup samefolder");
    }

    #[test]
    fn resolves_to_same_folder_distinguishes_two_folders() {
        let a = unique_temp_dir("distinct_a");
        let b = unique_temp_dir("distinct_b");
        assert!(!resolves_to_same_folder(&a, &b));
        std::fs::remove_dir_all(&a).expect("cleanup a");
        std::fs::remove_dir_all(&b).expect("cleanup b");
    }

    #[test]
    fn resolves_to_same_folder_falls_back_to_the_text_verdict_when_a_side_is_missing() {
        let base = unique_temp_dir("missing_probe");
        // `missing` does not exist, so canonicalize fails on that side and the
        // pure text verdict decides: an identical spelling counts as the same
        // folder, a different one does not.
        let missing = base.join("does_not_exist");
        assert!(resolves_to_same_folder(&missing, &missing));
        assert!(!resolves_to_same_folder(&missing, &base));

        std::fs::remove_dir_all(&base).expect("cleanup missing_probe");
    }

    #[test]
    fn delete_old_folder_deletes_a_folder_that_does_not_resolve_to_the_exe_dir() {
        let _guard = DevRootGuard::new();
        let base = unique_temp_dir("distinctdel");
        let old_dir = base.join("Zona").join("installer");
        write(&old_dir.join("keep.txt"), "x");
        // A genuinely different folder: the same-folder check does not fire, so
        // the delete proceeds and the folder is gone.
        delete_old_folder(&old_dir).expect("delete old folder");
        assert!(!old_dir.exists());
        std::fs::remove_dir_all(&base).expect("cleanup distinctdel");
    }

    #[test]
    fn delete_old_folder_refuses_a_junction_to_the_running_executable_directory() {
        let _guard = DevRootGuard::new();
        let exe_dir = std::env::current_exe()
            .expect("current exe")
            .parent()
            .map(Path::to_path_buf)
            .expect("exe parent");
        let base = unique_temp_dir("junction");
        let link = base.join("Zona").join("installer");
        std::fs::create_dir_all(link.parent().expect("link parent")).expect("create link parent");

        if !create_junction(&link, &exe_dir) {
            panic!("could not create a directory junction (mklink /J) in this environment");
        }

        // Lexically the link is NOT the exe dir, so only the filesystem check can
        // refuse it; without that check the delete would recurse into the exe dir.
        assert!(!paths_eq_ignore_case(&link, &exe_dir));
        assert!(delete_old_folder(&link).is_err());
        // Refused before any mutation: both the link and the exe dir survive.
        assert!(link.exists());
        assert!(exe_dir.exists());

        std::fs::remove_dir(&link).expect("remove junction link");
        std::fs::remove_dir_all(&base).expect("cleanup junction");
    }

    #[test]
    fn validate_old_folder_enforces_exe_dir_and_dev_root_rules() {
        let base = unique_temp_dir("exedir");
        let old = base.join("Zona").join("installer");
        let elsewhere = unique_temp_dir("elsewhere");

        // Refuses the running executable's own directory...
        assert!(validate_old_folder(&old, Some(&old), None).is_err());
        // ...accepts a different directory...
        assert!(validate_old_folder(&old, Some(&elsewhere), None).is_ok());
        // ...and refuses anything under the dev override root (an ancestor of
        // `old` counts as "under"; an unrelated root does not).
        assert!(validate_old_folder(&old, None, Some(&base)).is_err());
        assert!(validate_old_folder(&old, None, Some(&elsewhere)).is_ok());

        std::fs::remove_dir_all(&base).expect("cleanup exedir");
        std::fs::remove_dir_all(&elsewhere).expect("cleanup elsewhere");
    }

    #[test]
    fn validate_old_folder_refuses_a_parent_dir_component() {
        // A root-escaping `..` that `paths::normalize` keeps as-is: absolute and
        // shaped `Zona\installer`, but its component list differs from the OS's
        // resolution of the running copy's own directory.
        assert!(validate_old_folder(Path::new(r"C:\..\Zona\installer"), None, None).is_err());
        // A `..` in the middle, same evasion.
        assert!(validate_old_folder(Path::new(r"C:\Zona\..\Zona\installer"), None, None).is_err());
    }

    #[test]
    fn validate_old_folder_refuses_non_local_drive_paths() {
        // Local admin-share aliases and other UNC spellings of the same folder.
        assert!(
            validate_old_folder(Path::new(r"\\127.0.0.1\C$\Zona\installer"), None, None).is_err()
        );
        assert!(
            validate_old_folder(Path::new(r"\\server\share\Zona\installer"), None, None).is_err()
        );
        // Verbatim and device forms.
        assert!(validate_old_folder(Path::new(r"\\?\C:\Zona\installer"), None, None).is_err());
        assert!(validate_old_folder(Path::new(r"\\.\C:\Zona\installer"), None, None).is_err());
    }

    #[test]
    fn validate_old_folder_accepts_a_plain_local_drive_path_and_still_guards_it() {
        let path = Path::new(r"D:\Games\Zona\installer");
        // A plain local drive-letter path of the right shape is still accepted.
        assert!(validate_old_folder(path, None, None).is_ok());
        // ...but the existing guards still apply to it: its own exe dir...
        assert!(validate_old_folder(path, Some(path), None).is_err());
        // ...and the debug root override.
        assert!(validate_old_folder(path, None, Some(Path::new(r"D:\Games"))).is_err());
    }

    #[test]
    fn path_comparison_helpers_are_case_insensitive_and_component_wise() {
        assert!(paths_eq_ignore_case(
            Path::new(r"C:\Zona\installer"),
            Path::new(r"c:\zona\INSTALLER")
        ));
        assert!(path_is_under(
            Path::new(r"C:\Zona\installer"),
            Path::new(r"c:\ZONA")
        ));
        // Equality with the ancestor itself counts as under.
        assert!(path_is_under(Path::new(r"C:\Zona"), Path::new(r"C:\Zona")));
        // Component-wise: a raw string prefix must not match.
        assert!(!path_is_under(
            Path::new(r"C:\Zona"),
            Path::new(r"C:\ZonaX")
        ));
        assert!(!path_is_under(
            Path::new(r"C:\Zona2"),
            Path::new(r"C:\Zona")
        ));
    }
}
