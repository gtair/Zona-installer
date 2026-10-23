//! Finish-screen actions.
//!
//! [`run_actions`] runs the enabled post-install actions in a fixed order that
//! does not depend on the checkbox order:
//!
//! 1. **delete downloads** — remove `installer\downloads\` recursively;
//! 2. **start game** — launch `<drive>:\Zona\mo2\ModOrganizer.exe` with the
//!    Anomaly profile argument.
//!
//! The pure parts (ordering, target path, launch command) are unit-tested; the
//! impure parts are sealed behind the [`FinishEffects`] seam, with [`RealEffects`]
//! doing the real game-spawn work.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::app::FinishActionId;
use crate::error::{AppError, Result};
use crate::paths::ResolvedPaths;

/// The profile argument as a single process argument (no surrounding quotes:
/// `std::process::Command` applies the Windows quoting itself).
const MO2_PROFILE_ARGUMENT: &str = "moshortcut://:Anomaly (DX11-AVX)";

/// The game executable's file name inside the `mo2` directory.
pub(crate) const GAME_EXE_FILE: &str = "ModOrganizer.exe";

/// Runs the enabled finish-screen actions in the fixed order delete downloads ->
/// start game.
///
/// `enabled` carries the checked checkboxes in any order; unchecked actions are
/// skipped. A missing `downloads` directory is not an error.
pub fn run_actions(paths: &ResolvedPaths, enabled: &[FinishActionId]) -> Result<()> {
    run_actions_with(paths, enabled, &RealEffects)
}

/// The testable core of [`run_actions`]: the impure side effects are injected.
fn run_actions_with(
    paths: &ResolvedPaths,
    enabled: &[FinishActionId],
    effects: &dyn FinishEffects,
) -> Result<()> {
    let target = shortcut_target(paths);
    for action in ordered_actions(enabled) {
        match action {
            FinishActionId::DeleteDownloads => {
                log::debug!(
                    target: "finish:run_actions",
                    "finish action: delete downloads ({})",
                    paths.downloads.display(),
                );
                delete_downloads(&paths.downloads)?;
            }
            FinishActionId::StartGame => {
                log::debug!(
                    target: "finish:run_actions",
                    "finish action: start game ({} in {})",
                    target.display(),
                    paths.mo2.display(),
                );
                effects.start_game(&target, &paths.mo2)?;
            }
        }
    }
    Ok(())
}

/// The enabled actions in the fixed execution order, de-duplicated.
///
/// The checkbox order in `enabled` is irrelevant.
fn ordered_actions(enabled: &[FinishActionId]) -> Vec<FinishActionId> {
    [
        FinishActionId::DeleteDownloads,
        FinishActionId::StartGame,
    ]
    .into_iter()
    .filter(|action| enabled.contains(action))
    .collect()
}

/// The game executable the start action launches: `<Zona>\mo2\ModOrganizer.exe`.
fn shortcut_target(paths: &ResolvedPaths) -> PathBuf {
    paths.mo2.join(GAME_EXE_FILE)
}

/// Removes `downloads` and everything in it; a missing directory is treated as
/// already deleted.
fn delete_downloads(downloads: &Path) -> Result<()> {
    match std::fs::remove_dir_all(downloads) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(AppError::Io {
            context: "delete downloads directory".to_string(),
            path: Some(downloads.to_path_buf()),
            source,
        }),
    }
}

/// Builds the command that starts MO2 (pure; nothing is spawned).
///
/// `with_profile` adds the Anomaly profile argument (the "Launch Zona" case);
/// `false` launches MO2 plain (the "Launch Mod Organizer" case).
fn mo2_command(exe: &Path, working_directory: &Path, with_profile: bool) -> Command {
    let mut command = Command::new(exe);
    command.current_dir(working_directory);
    if with_profile {
        command.arg(MO2_PROFILE_ARGUMENT);
    }
    command
}

/// The impure side effects the finish flow needs, injected for testing.
trait FinishEffects {
    /// Launches `exe` with the Anomaly profile argument, starting in
    /// `working_directory`, detached and fire-and-forget.
    fn start_game(&self, exe: &Path, working_directory: &Path) -> Result<()>;
}

/// Production effects: a detached game spawn.
struct RealEffects;

impl FinishEffects for RealEffects {
    fn start_game(&self, exe: &Path, working_directory: &Path) -> Result<()> {
        start_game_detached(exe, working_directory)
    }
}

/// Production start-game: spawn the game detached and return immediately.
///
/// Fire-and-forget: the returned `Child` is dropped right away, and the child is
/// NOT placed in a kill-on-close job object (that would terminate the game when
/// the installer exits). A failed spawn is still fatal.
fn start_game_detached(exe: &Path, working_directory: &Path) -> Result<()> {
    launch_mo2(exe, working_directory, true)
}

/// Launches MO2 detached (the shared seam the finish action and the launcher
/// screen both use).
///
/// `with_profile` adds the Anomaly profile argument. Fire-and-forget: the
/// returned `Child` is dropped right away and the child is NOT placed in a
/// kill-on-close job object. A failed spawn is still fatal.
pub(crate) fn launch_mo2(
    exe: &Path,
    working_directory: &Path,
    with_profile: bool,
) -> Result<()> {
    let child = mo2_command(exe, working_directory, with_profile)
        .spawn()
        .map_err(|source| AppError::Io {
            context: "launch Mod Organizer".to_string(),
            path: Some(exe.to_path_buf()),
            source,
        })?;
    // Drop the handle immediately: the game must outlive the installer.
    drop(child);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    /// A unique empty temp directory for one test, removed by the caller.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "zona_installer_finish_{tag}_{}_{}_{}",
            std::process::id(),
            nanos,
            seq
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// A [`ResolvedPaths`] whose directories all live under `root` (a temp dir),
    /// so `delete_downloads` never touches a real install.
    fn paths_under(root: &Path) -> ResolvedPaths {
        ResolvedPaths {
            downloads: root.join("downloads"),
            logs: root.join("logs"),
            tmp: root.join("tmp"),
            deps: root.join("dependencies"),
            config: root.join("dependencies").join("config.yaml"),
            anomaly: root.join("Zona").join("anomaly"),
            mo2: root.join("Zona").join("mo2"),
        }
    }

    /// Records the effects a test asked for, in call order.
    struct FakeEffects {
        calls: RefCell<Vec<FinishActionId>>,
        game_exe: RefCell<Option<PathBuf>>,
        game_cwd: RefCell<Option<PathBuf>>,
    }

    impl FakeEffects {
        fn new() -> Self {
            FakeEffects {
                calls: RefCell::new(Vec::new()),
                game_exe: RefCell::new(None),
                game_cwd: RefCell::new(None),
            }
        }
    }

    impl FinishEffects for FakeEffects {
        fn start_game(&self, exe: &Path, working_directory: &Path) -> Result<()> {
            self.calls.borrow_mut().push(FinishActionId::StartGame);
            *self.game_exe.borrow_mut() = Some(exe.to_path_buf());
            *self.game_cwd.borrow_mut() = Some(working_directory.to_path_buf());
            Ok(())
        }
    }

    /// The path to a harmless, always-present Windows program under
    /// `%SystemRoot%\System32` (tests only). Used to exercise the production
    /// spawn path with a real process that is NOT the game.
    fn system32_exe(name: &str) -> PathBuf {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        PathBuf::from(root).join("System32").join(name)
    }

    #[test]
    fn deletes_a_temp_downloads_dir_recursively() {
        let root = unique_temp_dir("delete");
        let downloads = root.join("downloads");
        std::fs::create_dir_all(downloads.join("nested")).expect("create nested");
        std::fs::write(downloads.join("a.7z"), b"bytes").expect("write file");
        std::fs::write(downloads.join("nested").join("b.7z"), b"bytes").expect("write nested file");

        delete_downloads(&downloads).expect("delete downloads");

        assert!(!downloads.exists(), "the downloads dir must be gone");
        // Idempotent: deleting an already-absent dir is not an error.
        delete_downloads(&downloads).expect("second delete is a no-op");

        std::fs::remove_dir_all(&root).expect("cleanup temp dir");
    }

    #[test]
    fn shortcut_target_is_the_mo2_modorganizer_exe() {
        let root = unique_temp_dir("target");
        let paths = paths_under(&root);
        assert_eq!(shortcut_target(&paths), paths.mo2.join("ModOrganizer.exe"));
        // And `<Zona>\mo2\ModOrganizer.exe` for a real install root.
        let real = crate::paths::resolved_paths(Path::new(r"C:\Zona\installer"));
        assert_eq!(
            shortcut_target(&real),
            PathBuf::from(r"C:\Zona\mo2\ModOrganizer.exe")
        );
        std::fs::remove_dir_all(&root).expect("cleanup temp dir");
    }

    #[test]
    fn ordered_actions_fix_the_order_regardless_of_checkbox_order() {
        // Deliberately scrambled, with a duplicate: the canonical order wins.
        let enabled = [
            FinishActionId::StartGame,
            FinishActionId::DeleteDownloads,
            FinishActionId::StartGame,
        ];
        assert_eq!(
            ordered_actions(&enabled),
            vec![FinishActionId::DeleteDownloads, FinishActionId::StartGame]
        );

        // Only the checked actions run.
        assert_eq!(
            ordered_actions(&[FinishActionId::StartGame, FinishActionId::DeleteDownloads]),
            vec![FinishActionId::DeleteDownloads, FinishActionId::StartGame]
        );
        assert!(ordered_actions(&[]).is_empty());
    }

    #[test]
    fn run_actions_deletes_downloads_then_starts_the_game() {
        let root = unique_temp_dir("run");
        let mut paths = paths_under(&root);
        let downloads = paths.downloads.clone();
        std::fs::create_dir_all(&downloads).expect("create downloads");
        std::fs::write(downloads.join("a.7z"), b"bytes").expect("write file");

        let expected_target = PathBuf::from(r"C:\Zona\mo2\ModOrganizer.exe");
        // Use the canonical `<Zona>\mo2` shape for the target assertion.
        paths.mo2 = PathBuf::from(r"C:\Zona\mo2");
        let effects = FakeEffects::new();

        run_actions_with(
            &paths,
            &[
                FinishActionId::StartGame,
                FinishActionId::DeleteDownloads,
            ],
            &effects,
        )
        .expect("run actions");

        // Order is fixed: delete first (dir gone), then the game.
        assert!(!downloads.exists(), "downloads must be deleted first");
        assert_eq!(
            effects.game_exe.borrow().as_deref(),
            Some(expected_target.as_path()),
            "start game must launch the MO2 executable"
        );
        assert_eq!(
            effects.game_cwd.borrow().as_deref(),
            Some(Path::new(r"C:\Zona\mo2")),
            "start game must run in <Zona>\\mo2"
        );
        // Delete downloads runs directly (not via the effects seam); the only
        // recorded effect is the game spawn.
        assert_eq!(*effects.calls.borrow(), vec![FinishActionId::StartGame]);

        std::fs::remove_dir_all(&root).expect("cleanup temp dir");
    }

    #[test]
    fn empty_enabled_list_runs_nothing() {
        let root = unique_temp_dir("empty");
        let paths = paths_under(&root);
        let downloads = paths.downloads.clone();
        std::fs::create_dir_all(&downloads).expect("create downloads");

        let effects = FakeEffects::new();

        run_actions_with(&paths, &[], &effects).expect("no actions");

        assert!(downloads.exists(), "nothing enabled: downloads kept");
        assert!(effects.calls.borrow().is_empty());

        std::fs::remove_dir_all(&root).expect("cleanup temp dir");
    }

    #[test]
    fn mo2_command_with_profile_adds_the_anomaly_argument() {
        let exe = PathBuf::from(r"C:\Zona\mo2\ModOrganizer.exe");
        let cwd = PathBuf::from(r"C:\Zona\mo2");
        let cmd = mo2_command(&exe, &cwd, true);

        assert_eq!(cmd.get_program(), exe.as_os_str());
        assert_eq!(
            cmd.get_current_dir(),
            Some(cwd.as_path()),
            "the game must start in <Zona>\\mo2"
        );
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            vec![std::ffi::OsStr::new("moshortcut://:Anomaly (DX11-AVX)")],
            "one argument, unquoted: Command adds the Windows quoting itself"
        );
    }

    #[test]
    fn mo2_command_without_profile_adds_no_argument() {
        let exe = PathBuf::from(r"C:\Zona\mo2\ModOrganizer.exe");
        let cwd = PathBuf::from(r"C:\Zona\mo2");
        let cmd = mo2_command(&exe, &cwd, false);

        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            Vec::<&std::ffi::OsStr>::new()
        );
        assert_eq!(cmd.get_current_dir(), Some(cwd.as_path()));
    }

    #[test]
    fn start_game_detached_spawns_and_returns_without_gating_on_exit_code() {
        // A real, harmless read-only Windows program that exits NON-ZERO when
        // handed the moshortcut argument. Fire-and-forget must still return Ok:
        // the exit code is not acted on. Never the real game. The mo2 command
        // shape is covered by `mo2_command_with_profile_adds_the_anomaly_argument`.
        let exe = system32_exe("whoami.exe");
        let cwd = exe.parent().expect("System32 dir").to_path_buf();

        let started = Instant::now();
        start_game_detached(&exe, &cwd)
            .expect("fire-and-forget must return Ok even when the child exits non-zero");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "start-game must return immediately, not wait for the child to exit"
        );
    }

    #[test]
    fn start_game_detached_missing_exe_is_fatal_and_returns_immediately() {
        let root = unique_temp_dir("missing");
        let exe = root.join("mo2").join(GAME_EXE_FILE);

        let started = Instant::now();
        let cwd = root.join("mo2");
        let error = start_game_detached(&exe, &cwd).expect_err("a missing exe must be fatal");

        assert!(
            matches!(error, AppError::Io { .. }),
            "a failed spawn is a fatal AppError::Io, got {error:?}"
        );
        assert_eq!(error.error_id().as_str(), "E_IO");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "a failed spawn must fail fast, not block"
        );

        std::fs::remove_dir_all(&root).expect("cleanup temp dir");
    }

    #[test]
    fn source_no_longer_uses_the_blocking_child_runner() {
        // The old start-game path blocked on the child and gated on exit code 0
        // (proxying `verify_game`); it must be gone. The forbidden identifiers
        // are assembled from parts so this assertion does not itself contain
        // them.
        let source = include_str!("finish.rs");
        for forbidden in [
            ["Child", "Runner"].concat(),
            ["run", "_tool"].concat(),
            ["Real", "Runner"].concat(),
            ["Job", "Handle"].concat(),
            ["start_game", "_with"].concat(),
        ] {
            assert!(
                !source.contains(&forbidden),
                "finish.rs must not reference the old blocking start-game path: {forbidden}"
            );
        }
    }
}
