//! Startup pre-flight: single-instance gate, root/dependency checks, and the
//! relocation decision.
//!
//! [`run`] acquires the single-instance mutex, detects the root mode, verifies
//! the bundled `aria2c.exe` / `7za.exe` exist, and computes whether the binary
//! still has to be relocated to `<drive>:\Zona\installer`.
//!
//! Under the debug root override relocation and the self-delete trick are
//! disabled. The self-delete trick hands the OLD installer folder to the
//! relocated copy as a `/delete <path>` command-line argument, which is
//! lexically normalized ([`normalize_delete_argument`]) before use.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf, Prefix};
use std::time::Duration;

use crate::error::{AppError, ErrorId, Result};
use crate::paths::{self, RootMode};
use crate::win32::relocate::{self, SelfDeleteCommand};
use crate::win32::single_instance::{self, AcquireOutcome, SingleInstance};

/// The result of [`run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightOutcome {
    /// The detected root mode (shipped layout or debug override).
    pub mode: RootMode,
    /// The effective install root: `<drive>:\Zona\installer`, or the override
    /// directory under [`RootMode::DevOverride`].
    pub install_root: PathBuf,
    /// Whether the binary must be relocated before the normal flow may start.
    pub needs_relocation: bool,
    /// The debug flags parsed from the launch arguments, available before the
    /// config is read so logging can be initialised first thing.
    pub debug: DebugOptions,
}

/// The debug toggles parsed from the launch arguments.
///
/// These replace the old `debug_console` / `log_debug` `config.yaml` keys so
/// they are available immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DebugOptions {
    /// Show the console window.
    pub debug_console: bool,
    /// Write DEBUG-level output to the log file.
    pub log_debug: bool,
}

/// Parses the debug launch flags from the argument list.
///
/// `--debug-console` shows the console, `--log-debug` enables DEBUG logging,
/// and `--debug` is both. `/delete <path>` is left to [`parse_self_delete_arg`].
pub fn parse_debug_options<I: IntoIterator<Item = OsString>>(args: I) -> DebugOptions {
    let mut options = DebugOptions::default();
    for arg in args {
        match arg.to_string_lossy().as_ref() {
            "--debug-console" => options.debug_console = true,
            "--log-debug" => options.log_debug = true,
            "--debug" => {
                options.debug_console = true;
                options.log_debug = true;
            }
            _ => {}
        }
    }
    options
}

/// The bundled tools that must exist before anything else runs.
pub const REQUIRED_TOOLS: [&str; 2] = ["aria2c.exe", "7za.exe"];

/// Runs the startup pre-flight for the directory the binary was launched from.
///
/// Acquires the single-instance mutex, detects the root mode, verifies the
/// dependencies and computes [`PreflightOutcome::needs_relocation`]. The mutex
/// gate is kept for the whole process lifetime (the acquired handle is
/// intentionally leaked).
///
/// Returns an error when another instance already holds the gate
/// (`E_ALREADY_RUNNING`), when the root mode cannot be determined, or when a
/// required dependency is missing.
pub fn run(exe_dir: &Path) -> Result<PreflightOutcome> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let debug = parse_debug_options(args.iter().cloned());

    // Open the console and the log file before any other work so pre-flight is
    // logged too.
    let log = start_run_logging(exe_dir, debug);
    log::info!(
        target: "preflight:run",
        "launch flags: --log-debug={} --debug-console={}",
        debug.log_debug,
        debug.debug_console,
    );

    let mut sleep = std::thread::sleep;
    let mut outcome = run_with(
        exe_dir,
        single_instance::SINGLE_INSTANCE_NAME,
        args.iter().cloned(),
        RetryPolicy::GATE_WAIT,
        RetryPolicy::DELETE_WAIT,
        &mut sleep,
    )?;
    outcome.debug = debug;
    // Hold the log handle for the process lifetime (exactly like the leaked
    // single-instance guard): dropping it would close the file and clear the
    // global sink while the run is still in progress.
    if let Some(handle) = log {
        std::mem::forget(handle);
    }
    Ok(outcome)
}

/// Opens the console and the run log for the directory the binary launched from.
///
/// Best-effort: a logging/console failure must never stop the installer from
/// starting (the same reasoning as the best-effort self-delete). Returns the
/// [`crate::logging::LogHandle`] to hold for the process lifetime, or `None`
/// when initialisation failed. The install root is derived here (mirroring
/// [`preflight_checks`]) so the log lands in `<install_root>\logs\`.
fn start_run_logging(exe_dir: &Path, debug: DebugOptions) -> Option<crate::logging::LogHandle> {
    let mode = paths::detect_root_mode(exe_dir);
    let install_root = paths::install_root(exe_dir, &mode);
    if debug.debug_console {
        // Best effort: bind stderr to a console before the sink mirrors records.
        let _ = crate::win32::console::show_console();
    }
    let logs = paths::resolved_paths(&install_root).logs;
    crate::logging::init_logger(&logs, debug.log_debug, debug.debug_console).ok()
}

/// [`run`] with the single-instance mutex name supplied explicitly and no
/// arguments.
///
/// Tests use a unique name so parallel tests never contend on the production
/// gate. With an empty argument list there is no `/delete`.
#[cfg(test)]
fn run_with_mutex(exe_dir: &Path, mutex_name: &str) -> Result<PreflightOutcome> {
    let mut sleep = |_delay: Duration| {};
    run_with(
        exe_dir,
        mutex_name,
        std::iter::empty::<OsString>(),
        RetryPolicy::GATE_WAIT,
        RetryPolicy::DELETE_WAIT,
        &mut sleep,
    )
}

/// The argument-aware core of [`run`].
///
/// `args` is the process argument list (without argv[0]). A `/delete <path>`
/// argument, when relocation is enabled, switches the gate acquisition to
/// [`acquire_gate_with_retry`] (wait for the previous instance to exit) and adds
/// the best-effort [`self_delete_with_retry`]. Otherwise the gate is claimed in a
/// single attempt. `sleep` is injected so tests never wait.
fn run_with<I: IntoIterator<Item = OsString>>(
    exe_dir: &Path,
    mutex_name: &str,
    args: I,
    gate_policy: RetryPolicy,
    delete_policy: RetryPolicy,
    sleep: &mut dyn FnMut(Duration),
) -> Result<PreflightOutcome> {
    let mode = paths::detect_root_mode(exe_dir);
    let delete_arg = parse_self_delete_arg(args);
    if delete_arg.is_some() && !mode.relocation_disabled() {
        acquire_gate_with_retry(mutex_name, gate_policy, sleep)?;
    } else {
        acquire_gate_once(mutex_name)?;
    }
    let mut delete = run_self_delete;
    let outcome = self_delete_with_retry(
        &mode,
        delete_arg.as_deref(),
        delete_policy,
        sleep,
        &mut delete,
    );
    log_hand_off_outcome(&outcome);
    preflight_checks(exe_dir, mode)
}

/// Claims the gate in a single attempt.
fn acquire_gate_once(mutex_name: &str) -> Result<()> {
    match SingleInstance::acquire(mutex_name)? {
        AcquireOutcome::Acquired(guard) => {
            // Keep the gate for the life of the process (see `run`): the handle
            // must not be closed when this function returns.
            std::mem::forget(guard);
            Ok(())
        }
        AcquireOutcome::AlreadyRunning => Err(already_running_error()),
    }
}

/// Claims the gate, retrying while the previous instance still holds it.
///
/// The handle is closed by the kernel when the handing-off process terminates,
/// so a free gate is a definitive "the old instance is gone" signal. At most
/// `policy.attempts` acquisitions are made, `policy.delay` apart; a still-held
/// gate after those is a fatal `E_ALREADY_RUNNING`.
fn acquire_gate_with_retry(
    mutex_name: &str,
    policy: RetryPolicy,
    sleep: &mut dyn FnMut(Duration),
) -> Result<()> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match SingleInstance::acquire(mutex_name)? {
            AcquireOutcome::Acquired(guard) => {
                std::mem::forget(guard);
                return Ok(());
            }
            AcquireOutcome::AlreadyRunning => {
                if attempt >= policy.attempts {
                    return Err(already_running_error());
                }
                sleep(policy.delay);
            }
        }
    }
}

/// The fatal error for a gate that could not be claimed (`E_ALREADY_RUNNING`).
fn already_running_error() -> AppError {
    AppError::Fatal {
        message: "another Zona installer instance is already running".to_string(),
        error_id: ErrorId::new("E_ALREADY_RUNNING"),
        log_path: None,
    }
}

/// The root-mode-aware core of [`run`], split out so the mode-dependent
/// behaviour can be unit-tested without touching the process environment.
fn preflight_checks(exe_dir: &Path, mode: RootMode) -> Result<PreflightOutcome> {
    let install_root = paths::install_root(exe_dir, &mode);
    verify_dependencies(&install_root)?;
    let needs_relocation = needs_relocation(exe_dir, &mode);
    Ok(PreflightOutcome {
        mode,
        install_root,
        needs_relocation,
        // The real flags are attached by [`run`] after parsing the arguments;
        // this default is only ever observed through the test-only entry points.
        debug: DebugOptions::default(),
    })
}

/// Whether the binary still has to be relocated to `<drive>:\Zona\installer`.
///
/// `false` under the debug root override; otherwise `true` unless `exe_dir`
/// already sits at `<drive>:\Zona\installer` exactly.
pub fn needs_relocation(exe_dir: &Path, mode: &RootMode) -> bool {
    if mode.relocation_disabled() {
        return false;
    }
    !is_installer_dir(exe_dir)
}

/// Whether `dir` is shaped `<drive>:\Zona\installer` exactly.
///
/// The last component must be `installer`, its parent component `Zona`, and that
/// parent's parent a drive root. This is a pure NAME/SHAPE check: no filesystem
/// call and no drive-TYPE query.
fn is_installer_dir(dir: &Path) -> bool {
    let is_installer = dir
        .file_name()
        .is_some_and(|name| os_eq_ignore_case(name, "installer"));
    let zona_dir = dir.parent();
    let parent_is_zona = zona_dir
        .and_then(Path::file_name)
        .is_some_and(|name| os_eq_ignore_case(name, "Zona"));
    is_installer && parent_is_zona && zona_dir.and_then(Path::parent).is_some_and(is_drive_root)
}

/// Whether `path` is a drive root `<letter>:\`.
///
/// True only for exactly one [`Component::Prefix`] of kind [`Prefix::Disk`]
/// followed by exactly one [`Component::RootDir`] and nothing else.
fn is_drive_root(path: &Path) -> bool {
    let mut components = path.components();
    let drive_prefix = matches!(
        components.next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))
    ) && matches!(components.next(), Some(Component::RootDir));
    drive_prefix && components.next().is_none()
}

/// Ensures both bundled tools exist under `<install_root>\dependencies\`.
fn verify_dependencies(install_root: &Path) -> Result<()> {
    let deps = paths::resolved_paths(install_root).deps;
    for tool in REQUIRED_TOOLS {
        let path = deps.join(tool);
        if !path.is_file() {
            return Err(AppError::Io {
                context: format!("required dependency '{tool}' is missing"),
                path: Some(path),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{tool} not found"),
                ),
            });
        }
    }
    Ok(())
}

/// The outcome of the optional `/delete <path>` self-delete step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelfDeleteOutcome {
    /// Relocation/self-delete is disabled (dev override): nothing was done.
    Disabled,
    /// The normalized old installer folder was deleted.
    Deleted {
        /// The normalized path that was deleted.
        old_dir: PathBuf,
    },
}

/// Runs the self-delete trick for a raw `/delete <path>` argument, guarded by
/// `mode`.
///
/// Under [`RootMode::DevOverride`] this returns [`SelfDeleteOutcome::Disabled`]
/// as its first action. Otherwise the raw argument is normalized by
/// [`normalize_delete_argument`] before it is passed to `delete_old_folder`.
pub fn run_self_delete(mode: &RootMode, raw_arg: &Path) -> Result<SelfDeleteOutcome> {
    if mode.relocation_disabled() {
        return Ok(SelfDeleteOutcome::Disabled);
    }
    let old_dir = normalize_delete_argument(raw_arg);
    relocate::delete_old_folder(&old_dir)?;
    Ok(SelfDeleteOutcome::Deleted { old_dir })
}

/// A bounded retry policy (attempts and the delay between them).
///
/// `attempts` is the TOTAL number of tries (not the number of retries): the loop
/// makes at most `attempts` calls and sleeps `delay` between consecutive ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// The maximum number of tries.
    pub attempts: u32,
    /// The delay between two tries.
    pub delay: Duration,
}

impl RetryPolicy {
    /// The receiving copy's wait for the previous instance to release the gate:
    /// 100 tries, 100 ms apart (~10 s).
    pub const GATE_WAIT: RetryPolicy = RetryPolicy {
        attempts: 100,
        delay: Duration::from_millis(100),
    };

    /// The receiving copy's bounded self-delete wait: 20 tries, 100 ms apart
    /// (~2 s).
    pub const DELETE_WAIT: RetryPolicy = RetryPolicy {
        attempts: 20,
        delay: Duration::from_millis(100),
    };
}

/// The outcome of the receiving half of the self-delete hand-off.
///
/// The app does not consume this value — [`run`] only needs the log line — but it
/// exists so the "did it delete?" decision is explicit and honestly logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandOffOutcome {
    /// No `/delete <path>` argument: a normal start, nothing to hand off.
    None,
    /// Relocation/self-delete is disabled (dev override): nothing was deleted.
    Disabled,
    /// The old installer folder was deleted.
    Deleted {
        /// The normalized folder that was deleted.
        old_dir: PathBuf,
    },
    /// The self-delete was attempted but the folder remains (best effort).
    NotDeleted {
        /// The normalized folder that could not be deleted.
        old_dir: PathBuf,
        /// Why the last attempt failed (for the log line).
        reason: String,
    },
}

/// Parses an optional `/delete <path>` self-delete argument.
///
/// The flag is matched case-insensitively; the first occurrence wins and the
/// argument right after it is the operand. A flag without an operand yields
/// `None` (a normal start).
pub fn parse_self_delete_arg<I: IntoIterator<Item = OsString>>(args: I) -> Option<PathBuf> {
    let mut iter = args.into_iter();
    for arg in iter.by_ref() {
        if arg
            .to_string_lossy()
            .eq_ignore_ascii_case(relocate::SELF_DELETE_FLAG)
        {
            // The first `/delete` wins; the next argument (if any) is its operand.
            return iter.next().map(PathBuf::from);
        }
    }
    None
}

/// Runs the best-effort self-delete with the bounded [`RetryPolicy`].
///
/// `raw_arg` is the `/delete` operand, or `None` for a normal start. Under
/// [`RootMode::DevOverride`] the guard returns [`HandOffOutcome::Disabled`] as
/// its first action. Otherwise the normalized folder is deleted; a transient
/// (locked) failure is retried up to `policy.attempts` times and a permanent
/// refusal (`InvalidInput`) stops at once. This NEVER returns an error.
fn self_delete_with_retry(
    mode: &RootMode,
    raw_arg: Option<&Path>,
    policy: RetryPolicy,
    sleep: &mut dyn FnMut(Duration),
    delete: &mut dyn FnMut(&RootMode, &Path) -> Result<SelfDeleteOutcome>,
) -> HandOffOutcome {
    let raw_arg = match raw_arg {
        Some(arg) => arg,
        None => return HandOffOutcome::None,
    };
    if mode.relocation_disabled() {
        return HandOffOutcome::Disabled;
    }
    let old_dir = normalize_delete_argument(raw_arg);
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match delete(mode, raw_arg) {
            Ok(SelfDeleteOutcome::Deleted { .. }) => return HandOffOutcome::Deleted { old_dir },
            Ok(SelfDeleteOutcome::Disabled) => return HandOffOutcome::Disabled,
            Err(error) => {
                if is_permanent_delete_refusal(&error) || attempt >= policy.attempts {
                    return HandOffOutcome::NotDeleted {
                        old_dir,
                        reason: error.to_string(),
                    };
                }
                sleep(policy.delay);
            }
        }
    }
}

/// Whether a delete failure is a permanent refusal that must not be retried (pure).
///
/// [`relocate::delete_old_folder`] refuses a wrongly shaped path with an
/// [`AppError::Io`] whose source is [`std::io::ErrorKind::InvalidInput`]; that
/// refusal cannot change on a retry, so the loop stops immediately. Every other
/// failure (a locked file, a sharing violation) is treated as transient.
fn is_permanent_delete_refusal(error: &AppError) -> bool {
    matches!(
        error,
        AppError::Io { source, .. } if source.kind() == std::io::ErrorKind::InvalidInput
    )
}

/// Logs a [`HandOffOutcome`]: a leftover folder is a `warn!`.
///
/// On a real run the hand-off happens before the config is parsed, so no logger
/// is installed yet and these lines are no-ops; the code still states the intent.
fn log_hand_off_outcome(outcome: &HandOffOutcome) {
    match outcome {
        HandOffOutcome::Deleted { old_dir } => {
            log::info!(
                target: "preflight:hand_off",
                "deleted the old installer folder {}",
                old_dir.display()
            );
        }
        HandOffOutcome::NotDeleted { old_dir, reason } => {
            log::warn!(
                target: "preflight:hand_off",
                "could not delete the old installer folder {}: {reason}",
                old_dir.display()
            );
        }
        HandOffOutcome::None | HandOffOutcome::Disabled => {}
    }
}

/// The outcome of the guarded self-delete spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnOutcome {
    /// Relocation/self-delete is disabled (dev override): nothing was spawned.
    Disabled,
    /// The relocated copy was spawned with the self-delete arguments.
    Spawned {
        /// The command line that was spawned.
        command: SelfDeleteCommand,
    },
}

/// Spawns the relocated copy so it can delete the old folder, guarded by `mode`.
///
/// Under [`RootMode::DevOverride`] this returns [`SpawnOutcome::Disabled`] as its
/// FIRST action, so [`relocate::spawn_new_copy`] is never reached when relocation
/// is disabled. Otherwise the program (the relocated
/// `<...>\Zona\installer\zona-installer.exe`) is shape-validated and the old
/// folder argument normalized before the spawn command is built.
pub fn spawn_relocated_copy(
    mode: &RootMode,
    new_exe: &Path,
    old_dir: &Path,
) -> Result<SpawnOutcome> {
    if mode.relocation_disabled() {
        return Ok(SpawnOutcome::Disabled);
    }
    validate_relocated_exe(new_exe)?;
    let old_dir = normalize_delete_argument(old_dir);
    let command = relocate::self_delete_command(new_exe, &old_dir);
    relocate::spawn_new_copy(&command)?;
    Ok(SpawnOutcome::Spawned { command })
}

/// Ensures `exe` is the relocated installer binary
/// `<...>\Zona\installer\zona-installer.exe`.
///
/// The self-delete spawn program must be the copy relocation produced, not an
/// arbitrary path from a caller. Refusals surface as [`AppError::Io`] carrying an
/// `InvalidInput` source.
fn validate_relocated_exe(exe: &Path) -> Result<()> {
    let is_binary = exe
        .file_name()
        .is_some_and(|name| os_eq_ignore_case(name, relocate::INSTALLER_BINARY));
    let parent_is_installer = exe
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| os_eq_ignore_case(name, "installer"));
    let grandparent_is_zona = exe
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .is_some_and(|name| os_eq_ignore_case(name, "Zona"));
    if !exe.is_absolute() || !is_binary || !parent_is_installer || !grandparent_is_zona {
        return Err(invalid_input(
            "validate relocated installer program",
            exe,
            "refusing to spawn a program not shaped <anything>\\Zona\\installer\\zona-installer.exe",
        ));
    }
    Ok(())
}

/// Lexically normalizes a raw `/delete <path>` argument (no filesystem).
///
/// Handles three shapes: a plain absolute path with `..`/`.` components, a
/// `\\?\` verbatim path (including `\\?\UNC\...`), and a UNC local admin-share
/// alias (`\\localhost\C$\Zona\installer` -> `C:\Zona\installer`). The result is
/// left as-is when none of those apply; the downstream `delete_old_folder`
/// validation still rejects anything not shaped `<...>\Zona\installer`.
pub fn normalize_delete_argument(raw: &Path) -> PathBuf {
    let text = raw.as_os_str().to_string_lossy();
    let stripped = strip_verbatim_prefix(&text);
    let mapped = map_local_admin_share(&stripped);
    // Pure lexical normalization: `paths::resolve` joins onto an empty root (an
    // absolute argument replaces it) and collapses `.`/`..` components.
    paths::resolve(Path::new(""), &mapped)
}

/// Strips a `\\?\` verbatim prefix, converting `\\?\UNC\...` to `\\...` (pure).
fn strip_verbatim_prefix(text: &str) -> String {
    match text.strip_prefix(r"\\?\") {
        Some(rest) => match rest
            .strip_prefix("UNC\\")
            .or_else(|| rest.strip_prefix("unc\\"))
        {
            Some(unc) => format!("\\\\{unc}"),
            None => rest.to_string(),
        },
        None => text.to_string(),
    }
}

/// Maps `\\localhost\<L>$\rest` (a local admin-drive share) to `<L>:\rest`
/// (pure). Only the localhost alias is mapped; a remote `\\server\...` is left
/// untouched so it can never be mistaken for a local drive.
fn map_local_admin_share(text: &str) -> String {
    const PREFIX: &str = r"\\localhost\";
    match text.get(..PREFIX.len()) {
        Some(head) if head.eq_ignore_ascii_case(PREFIX) => {}
        _ => return text.to_string(),
    }
    let rest = text.get(PREFIX.len()..).unwrap_or("");
    let mut parts = rest.splitn(2, '\\');
    let share = parts.next().unwrap_or("");
    let remainder = parts.next().unwrap_or("");
    let bytes = share.as_bytes();
    // An admin share is a single drive letter followed by '$': `C$`, `D$`, ...
    if bytes.len() == 2 && bytes[1] == b'$' && bytes[0].is_ascii_alphabetic() {
        let drive = format!("{}:\\", char::from(bytes[0]).to_ascii_uppercase());
        return if remainder.is_empty() {
            drive
        } else {
            format!("{drive}{remainder}")
        };
    }
    text.to_string()
}

/// Case-insensitive comparison of an [`OsStr`] against an ASCII literal (pure).
fn os_eq_ignore_case(value: &OsStr, expected: &str) -> bool {
    value.to_string_lossy().eq_ignore_ascii_case(expected)
}

/// Builds the [`AppError::Io`] for a refused value (invalid-argument shape).
///
/// [`AppError`] has no dedicated invalid-argument variant, so refusals surface as
/// [`AppError::Io`] carrying an `InvalidInput` source, matching
/// `relocate::delete_old_folder`.
fn invalid_input(context: &str, path: &Path, message: &str) -> AppError {
    AppError::Io {
        context: context.to_string(),
        path: Some(path.to_path_buf()),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, message.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::ffi::OsString;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
            std::env::temp_dir().join(format!("zona_installer_preflight_{tag}_{pid}_{nanos}_{n}"));
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

    /// Removes a temp directory and everything under it.
    fn cleanup(path: &Path) {
        let _ = std::fs::remove_dir_all(path);
    }

    /// A process-unique single-instance mutex name so parallel tests never
    /// contend on the production gate.
    fn unique_mutex_name(label: &str) -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!(
            r"Local\ZonaInstallerTest_{}_{label}_{n}",
            std::process::id()
        )
    }

    /// RAII guard that clears `ZONA_INSTALLER_DEV_ROOT` for one test and restores
    /// the previous value on drop.
    ///
    /// The variable name is a private const in `paths.rs`, so the literal string
    /// is used here. A shared static mutex serializes the env-touching tests
    /// (environment variables are process-global while tests run on parallel
    /// threads). Edition 2021, so `remove_var`/`set_var` are safe functions.
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
    fn normalize_delete_argument_resolves_parent_components() {
        assert_eq!(
            normalize_delete_argument(Path::new(r"C:\Zona\..\Zona\installer")),
            PathBuf::from(r"C:\Zona\installer")
        );
        assert_eq!(
            normalize_delete_argument(Path::new(r"C:\Zona\installer\..\..\Zona\installer")),
            PathBuf::from(r"C:\Zona\installer")
        );
    }

    #[test]
    fn normalize_delete_argument_strips_verbatim_prefix() {
        assert_eq!(
            normalize_delete_argument(Path::new(r"\\?\C:\Zona\installer")),
            PathBuf::from(r"C:\Zona\installer")
        );
        // The UNC verbatim form is first reduced to a plain UNC path, then the
        // localhost alias is applied.
        assert_eq!(
            normalize_delete_argument(Path::new(r"\\?\UNC\localhost\C$\Zona\installer")),
            PathBuf::from(r"C:\Zona\installer")
        );
    }

    #[test]
    fn normalize_delete_argument_maps_local_admin_share() {
        assert_eq!(
            normalize_delete_argument(Path::new(r"\\localhost\C$\Zona\installer")),
            PathBuf::from(r"C:\Zona\installer")
        );
        // Case-insensitive host/share and a lower-case drive letter.
        assert_eq!(
            normalize_delete_argument(Path::new(r"\\LOCALHOST\d$\Zona\installer")),
            PathBuf::from(r"D:\Zona\installer")
        );
    }

    #[test]
    fn normalize_delete_argument_leaves_plain_and_remote_paths_alone() {
        assert_eq!(
            normalize_delete_argument(Path::new(r"D:\Zona\installer")),
            PathBuf::from(r"D:\Zona\installer")
        );
        // A remote admin share must NOT be mapped onto a local drive.
        assert_eq!(
            normalize_delete_argument(Path::new(r"\\server\C$\Zona\installer")),
            PathBuf::from(r"\\server\C$\Zona\installer")
        );
        // A relative argument stays relative (delete_old_folder then rejects it).
        assert_eq!(
            normalize_delete_argument(Path::new(r"Zona\installer")),
            PathBuf::from(r"Zona\installer")
        );
    }

    #[test]
    fn needs_relocation_is_false_for_installer_dir_and_true_otherwise() {
        assert!(!needs_relocation(
            Path::new(r"C:\Zona\installer"),
            &RootMode::Shipped
        ));
        // Case-insensitive shape match.
        assert!(!needs_relocation(
            Path::new(r"D:\zona\INSTALLER"),
            &RootMode::Shipped
        ));
        // Not under Zona, or named `Zona` itself -> still needs relocation.
        assert!(needs_relocation(
            Path::new(r"C:\downloads\zona"),
            &RootMode::Shipped
        ));
        assert!(needs_relocation(Path::new(r"C:\Zona"), &RootMode::Shipped));
        // The nested branch that hid finding 1: `<anything>\Zona\installer` is
        // NOT `<drive>:\Zona\installer`, so the drive picker must be offered.
        assert!(needs_relocation(
            Path::new(r"C:\Users\x\Downloads\Zona\installer"),
            &RootMode::Shipped
        ));
    }

    #[test]
    fn is_installer_dir_accepts_only_a_drive_root_zona_installer() {
        // `<drive>:\Zona\installer` exactly (case-insensitive).
        assert!(is_installer_dir(Path::new(r"C:\Zona\installer")));
        assert!(is_installer_dir(Path::new(r"D:\zona\INSTALLER")));
        // Nested, UNC, verbatim, device, root-relative and relative all false.
        assert!(!is_installer_dir(Path::new(r"C:\anything\Zona\installer")));
        assert!(!is_installer_dir(Path::new(
            r"C:\Users\x\Downloads\Zona\installer"
        )));
        assert!(!is_installer_dir(Path::new(
            r"\\server\share\Zona\installer"
        )));
        assert!(!is_installer_dir(Path::new(r"\\?\C:\Zona\installer")));
        assert!(!is_installer_dir(Path::new(r"\\.\C:\Zona\installer")));
        assert!(!is_installer_dir(Path::new(r"\Zona\installer")));
        assert!(!is_installer_dir(Path::new(r"Zona\installer")));
        // The drive-RELATIVE spelling: a prefix with no root (`C:` / `C:Zona`)
        // is not a drive root, so the `RootDir` leg of `is_drive_root` is needed.
        assert!(!is_installer_dir(Path::new(r"C:Zona\installer")));
        assert!(!is_installer_dir(Path::new("C:")));
    }

    #[test]
    fn needs_relocation_is_false_under_dev_override() {
        let mode = RootMode::DevOverride(PathBuf::from(r"D:\dev\zona"));
        assert!(!needs_relocation(Path::new(r"C:\somewhere"), &mode));
    }

    #[test]
    fn run_ok_with_dependencies_in_a_nested_zona_installer_requires_relocation() {
        let _env = DevRootGuard::new();
        let root = unique_temp_dir("installed");
        // A temp dir is never a drive root, so `<temp>\Zona\installer` is the
        // NESTED case: it is no longer mistaken for an in-place install, so
        // preflight still passes (dependencies exist) but offers the drive
        // picker. The pure in-place case is asserted directly in
        // `is_installer_dir_accepts_only_a_drive_root_zona_installer`, which does
        // not need a real `<drive>:\` directory.
        let exe_dir = root.join("Zona").join("installer");
        write(&exe_dir.join("dependencies/aria2c.exe"), "x");
        write(&exe_dir.join("dependencies/7za.exe"), "x");

        let outcome =
            run_with_mutex(&exe_dir, &unique_mutex_name("ok")).expect("preflight should pass");

        assert_eq!(outcome.mode, RootMode::Shipped);
        assert_eq!(outcome.install_root, exe_dir);
        assert!(outcome.needs_relocation);
        cleanup(&root);
    }

    #[test]
    fn run_errors_when_a_dependency_is_missing() {
        let _env = DevRootGuard::new();
        let root = unique_temp_dir("missing");
        let exe_dir = root.join("Zona").join("installer");
        std::fs::create_dir_all(&exe_dir).expect("create installer dir");
        // 7za.exe is present but aria2c.exe is not.
        write(&exe_dir.join("dependencies/7za.exe"), "x");

        let error = run_with_mutex(&exe_dir, &unique_mutex_name("missing"))
            .expect_err("missing dependency must fail");
        match error {
            AppError::Io { path, .. } => {
                let reported = path.expect("missing dependency error carries a path");
                assert!(reported.ends_with("aria2c.exe"), "got {reported:?}");
            }
            other => panic!("expected AppError::Io, got {other:?}"),
        }
        cleanup(&root);
    }

    #[test]
    fn run_outside_zona_is_ok_but_requires_relocation() {
        let _env = DevRootGuard::new();
        let root = unique_temp_dir("notzona");
        let exe_dir = root.join("somewhere");
        write(&exe_dir.join("dependencies/aria2c.exe"), "x");
        write(&exe_dir.join("dependencies/7za.exe"), "x");

        let outcome =
            run_with_mutex(&exe_dir, &unique_mutex_name("notzona")).expect("preflight should pass");

        assert!(outcome.needs_relocation);
        assert_eq!(outcome.install_root, exe_dir);
        cleanup(&root);
    }

    #[test]
    fn dev_override_uses_the_override_root_and_disables_relocation() {
        let override_root = unique_temp_dir("devroot");
        write(&override_root.join("dependencies/aria2c.exe"), "x");
        write(&override_root.join("dependencies/7za.exe"), "x");
        let exe_dir = unique_temp_dir("devexe");

        let outcome = preflight_checks(&exe_dir, RootMode::DevOverride(override_root.clone()))
            .expect("preflight should pass");

        assert!(outcome.mode.relocation_disabled());
        assert_eq!(outcome.install_root, override_root);
        assert!(!outcome.needs_relocation);
        cleanup(&override_root);
        cleanup(&exe_dir);
    }

    #[test]
    fn run_reports_already_running_when_the_gate_is_taken() {
        let _env = DevRootGuard::new();
        let root = unique_temp_dir("mutex");
        let exe_dir = root.join("Zona").join("installer");
        write(&exe_dir.join("dependencies/aria2c.exe"), "x");
        write(&exe_dir.join("dependencies/7za.exe"), "x");
        let name = unique_mutex_name("taken");

        // The first call acquires (and keeps) the gate; the second must see it.
        assert!(run_with_mutex(&exe_dir, &name).is_ok());
        let error = run_with_mutex(&exe_dir, &name).expect_err("second run must fail");
        assert_eq!(error.error_id().as_str(), "E_ALREADY_RUNNING");
        cleanup(&root);
    }

    #[test]
    fn run_self_delete_is_disabled_and_deletes_nothing_under_dev_override() {
        let base = unique_temp_dir("selfdel_disabled");
        let old_dir = base.join("Zona").join("installer");
        write(&old_dir.join("keep.txt"), "x");
        let mode = RootMode::DevOverride(base.clone());

        let outcome = run_self_delete(&mode, &old_dir).expect("guarded self-delete");

        assert_eq!(outcome, SelfDeleteOutcome::Disabled);
        // The guard ran first: delete_old_folder was never reached.
        assert!(old_dir.join("keep.txt").exists());
        cleanup(&base);
    }

    #[test]
    fn run_self_delete_normalizes_and_deletes_when_relocation_is_enabled() {
        let _env = DevRootGuard::new();
        let base = unique_temp_dir("selfdel_enabled");
        let old_dir = base.join("Zona").join("installer");
        write(&old_dir.join("keep.txt"), "x");
        // A raw argument with a `..` detour: normalization must reach the real dir.
        let raw = base.join("Zona").join("..").join("Zona").join("installer");

        let outcome = run_self_delete(&RootMode::Shipped, &raw).expect("self-delete");

        match outcome {
            SelfDeleteOutcome::Deleted { old_dir: deleted } => assert_eq!(deleted, old_dir),
            other => panic!("expected Deleted, got {other:?}"),
        }
        assert!(!old_dir.exists());
        cleanup(&base);
    }

    #[test]
    fn run_self_delete_refuses_a_path_of_the_wrong_shape() {
        let _env = DevRootGuard::new();
        let base = unique_temp_dir("selfdel_wrong");
        let wrong = base.join("not_zona").join("installer");
        write(&wrong.join("keep.txt"), "x");

        assert!(run_self_delete(&RootMode::Shipped, &wrong).is_err());
        assert!(wrong.join("keep.txt").exists());
        cleanup(&base);
    }

    #[test]
    fn spawn_relocated_copy_is_disabled_under_dev_override() {
        let mode = RootMode::DevOverride(PathBuf::from(r"D:\dev\zona"));
        // A well-shaped program under this test's own empty temp dir: guaranteed
        // absent on every machine, so a real spawn would fail. An Ok(Disabled)
        // therefore proves spawn_new_copy was never reached.
        let root = unique_temp_dir("spawn_disabled");
        let new_exe = root
            .join("Zona")
            .join("installer")
            .join("zona-installer.exe");

        let outcome = spawn_relocated_copy(&mode, &new_exe, &root).expect("guarded");

        assert_eq!(outcome, SpawnOutcome::Disabled);
        cleanup(&root);
    }

    #[test]
    fn spawn_relocated_copy_reaches_the_spawn_when_relocation_is_enabled() {
        // A well-shaped program under this test's own empty temp dir: guaranteed
        // absent on every machine, so reaching spawn_new_copy must surface an I/O
        // error (proving the guard let the call through) and no real installer is
        // ever launched. No hardcoded drive letter, in case a real shipped install
        // exists there.
        let root = unique_temp_dir("spawn_enabled");
        let new_exe = root
            .join("Zona")
            .join("installer")
            .join("zona-installer.exe");
        assert!(!new_exe.exists(), "temp fixture must be empty");

        let error = spawn_relocated_copy(&RootMode::Shipped, &new_exe, Path::new(r"C:\old"))
            .expect_err("spawning a missing program must fail");

        assert!(matches!(error, AppError::Io { .. }));
        cleanup(&root);
    }

    #[test]
    fn spawn_relocated_copy_rejects_a_program_of_the_wrong_shape() {
        let error = spawn_relocated_copy(
            &RootMode::Shipped,
            Path::new(r"C:\Windows\notepad.exe"),
            Path::new(r"C:\old"),
        )
        .expect_err("wrong-shape program must be refused");
        assert!(matches!(error, AppError::Io { .. }));
    }

    // ---- receiving half --------------------------------------------------

    /// A transient (locked) delete failure, i.e. one that is safe to retry.
    fn transient_error() -> AppError {
        AppError::Io {
            context: "remove old installer folder".to_string(),
            path: None,
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "locked"),
        }
    }

    /// A permanent (`InvalidInput`) refusal, i.e. the shape check said no.
    fn permanent_error() -> AppError {
        invalid_input(
            "validate old installer folder",
            Path::new(r"C:\x"),
            "refused",
        )
    }

    #[test]
    fn parse_self_delete_arg_reads_the_flag_and_its_operand() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();

        // Flag + operand.
        assert_eq!(
            parse_self_delete_arg(args(&["/delete", r"C:\Zona\installer"])),
            Some(PathBuf::from(r"C:\Zona\installer"))
        );
        // The flag is matched case-insensitively.
        assert_eq!(
            parse_self_delete_arg(args(&["/DELETE", r"C:\old"])),
            Some(PathBuf::from(r"C:\old"))
        );
        // A flag with no operand is a normal start (None).
        assert_eq!(parse_self_delete_arg(args(&["/delete"])), None);
        // No flag at all.
        assert_eq!(parse_self_delete_arg(args(&["--verbose", "x"])), None);
        // The FIRST occurrence wins.
        assert_eq!(
            parse_self_delete_arg(args(&["/delete", r"C:\first", "/delete", r"C:\second"])),
            Some(PathBuf::from(r"C:\first"))
        );
    }

    #[test]
    fn acquire_gate_with_retry_is_bounded_when_the_gate_stays_held() {
        let name = unique_mutex_name("gate_held");
        let held = match SingleInstance::acquire(&name).expect("hold gate") {
            AcquireOutcome::Acquired(guard) => guard,
            AcquireOutcome::AlreadyRunning => panic!("the gate must start free"),
        };

        let sleeps = AtomicUsize::new(0);
        let mut sleep = |_delay: Duration| {
            sleeps.fetch_add(1, Ordering::SeqCst);
        };
        let error = acquire_gate_with_retry(&name, RetryPolicy::GATE_WAIT, &mut sleep)
            .expect_err("a gate held for the whole wait must exhaust the retry");

        assert_eq!(error.error_id().as_str(), "E_ALREADY_RUNNING");
        // Exactly `GATE_WAIT.attempts` acquisitions, one sleep between each pair.
        assert_eq!(
            sleeps.load(Ordering::SeqCst),
            RetryPolicy::GATE_WAIT.attempts as usize - 1
        );
        drop(held);
    }

    #[test]
    fn acquire_gate_with_retry_recovers_when_the_previous_instance_exits() {
        let name = unique_mutex_name("gate_release");
        let held = match SingleInstance::acquire(&name).expect("hold gate") {
            AcquireOutcome::Acquired(guard) => guard,
            AcquireOutcome::AlreadyRunning => panic!("the gate must start free"),
        };
        // Simulate the handing-off instance exiting after three waits: the
        // leaked handle closing frees the named mutex for the next probe.
        let held = RefCell::new(Some(held));
        let sleeps = AtomicUsize::new(0);
        let mut sleep = |_delay: Duration| {
            let n = sleeps.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 3 {
                drop(held.borrow_mut().take());
            }
        };

        acquire_gate_with_retry(&name, RetryPolicy::GATE_WAIT, &mut sleep)
            .expect("the probe after the release must succeed");
        assert_eq!(sleeps.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn self_delete_with_retry_retries_transient_failures_then_succeeds() {
        let calls = AtomicUsize::new(0);
        let mut delete = |_mode: &RootMode, _raw: &Path| -> Result<SelfDeleteOutcome> {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n < 3 {
                Err(transient_error())
            } else {
                Ok(SelfDeleteOutcome::Deleted {
                    old_dir: PathBuf::from(r"C:\Zona\installer"),
                })
            }
        };
        let mut sleep = |_delay: Duration| {};

        let outcome = self_delete_with_retry(
            &RootMode::Shipped,
            // A `..` detour proves the outcome carries the NORMALIZED folder.
            Some(Path::new(r"C:\Zona\..\Zona\installer")),
            RetryPolicy::DELETE_WAIT,
            &mut sleep,
            &mut delete,
        );

        match outcome {
            HandOffOutcome::Deleted { old_dir } => {
                assert_eq!(old_dir, PathBuf::from(r"C:\Zona\installer"));
            }
            other => panic!("expected Deleted, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 4, "3 transient + 1 success");
    }

    #[test]
    fn self_delete_with_retry_stops_at_a_permanent_refusal() {
        let calls = AtomicUsize::new(0);
        let mut delete = |_mode: &RootMode, _raw: &Path| -> Result<SelfDeleteOutcome> {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(permanent_error())
        };
        let sleeps = AtomicUsize::new(0);
        let mut sleep = |_delay: Duration| {
            sleeps.fetch_add(1, Ordering::SeqCst);
        };

        let outcome = self_delete_with_retry(
            &RootMode::Shipped,
            Some(Path::new(r"C:\wrong\shape")),
            RetryPolicy::DELETE_WAIT,
            &mut sleep,
            &mut delete,
        );

        assert!(matches!(outcome, HandOffOutcome::NotDeleted { .. }));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a permanent refusal must not be retried"
        );
        assert_eq!(sleeps.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn self_delete_with_retry_is_bounded_and_never_errors() {
        let calls = AtomicUsize::new(0);
        let mut delete = |_mode: &RootMode, _raw: &Path| -> Result<SelfDeleteOutcome> {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(transient_error())
        };
        let sleeps = AtomicUsize::new(0);
        let mut sleep = |_delay: Duration| {
            sleeps.fetch_add(1, Ordering::SeqCst);
        };

        let outcome = self_delete_with_retry(
            &RootMode::Shipped,
            Some(Path::new(r"C:\Zona\installer")),
            RetryPolicy::DELETE_WAIT,
            &mut sleep,
            &mut delete,
        );

        match outcome {
            HandOffOutcome::NotDeleted { reason, .. } => {
                assert!(reason.contains("locked"), "got {reason:?}");
            }
            other => panic!("expected NotDeleted, got {other:?}"),
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            RetryPolicy::DELETE_WAIT.attempts as usize
        );
        assert_eq!(
            sleeps.load(Ordering::SeqCst),
            RetryPolicy::DELETE_WAIT.attempts as usize - 1
        );
    }

    #[test]
    fn self_delete_with_retry_is_disabled_before_any_delete_under_dev_override() {
        let calls = AtomicUsize::new(0);
        let mut delete = |_mode: &RootMode, _raw: &Path| -> Result<SelfDeleteOutcome> {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(SelfDeleteOutcome::Deleted {
                old_dir: PathBuf::from(r"C:\Zona\installer"),
            })
        };
        let mut sleep = |_delay: Duration| {};

        let outcome = self_delete_with_retry(
            &RootMode::DevOverride(PathBuf::from(r"D:\dev")),
            Some(Path::new(r"C:\Zona\installer")),
            RetryPolicy::DELETE_WAIT,
            &mut sleep,
            &mut delete,
        );

        assert_eq!(outcome, HandOffOutcome::Disabled);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "the guard runs first");
    }

    #[test]
    fn run_with_delete_consumes_the_argument_and_deletes_the_old_folder() {
        let _env = DevRootGuard::new();
        let root = unique_temp_dir("receive_delete");
        let exe_dir = root.join("Zona").join("installer");
        write(&exe_dir.join("dependencies/aria2c.exe"), "x");
        write(&exe_dir.join("dependencies/7za.exe"), "x");
        // A path shaped <anything>\Zona\installer so delete_old_folder accepts it.
        let old_dir = root.join("old").join("Zona").join("installer");
        write(&old_dir.join("keep.txt"), "x");

        let args = vec![
            OsString::from("/delete"),
            old_dir.as_os_str().to_os_string(),
        ];
        let mut sleep = |_delay: Duration| {};
        let outcome = run_with(
            &exe_dir,
            &unique_mutex_name("receive"),
            args,
            RetryPolicy::GATE_WAIT,
            RetryPolicy::DELETE_WAIT,
            &mut sleep,
        )
        .expect("the hand-off start must reach its own pre-flight");

        assert_eq!(outcome.install_root, exe_dir);
        assert!(
            !old_dir.exists(),
            "the receiving copy deletes the old folder"
        );
        cleanup(&root);
    }

    #[test]
    fn run_with_an_unshaped_delete_argument_warns_but_still_starts() {
        let _env = DevRootGuard::new();
        let root = unique_temp_dir("receive_unshaped");
        let exe_dir = root.join("Zona").join("installer");
        write(&exe_dir.join("dependencies/aria2c.exe"), "x");
        write(&exe_dir.join("dependencies/7za.exe"), "x");
        // NOT shaped <anything>\Zona\installer: a permanent refusal, never fatal.
        let wrong = root.join("not_zona").join("installer");
        write(&wrong.join("keep.txt"), "x");

        let args = vec![OsString::from("/delete"), wrong.as_os_str().to_os_string()];
        let mut sleep = |_delay: Duration| {};
        let outcome = run_with(
            &exe_dir,
            &unique_mutex_name("receive_unshaped"),
            args,
            RetryPolicy::GATE_WAIT,
            RetryPolicy::DELETE_WAIT,
            &mut sleep,
        )
        .expect("a refused self-delete must never be fatal");

        assert_eq!(outcome.install_root, exe_dir);
        assert!(wrong.join("keep.txt").exists(), "nothing was deleted");
        cleanup(&root);
    }
}
