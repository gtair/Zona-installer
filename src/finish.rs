//! Finish-screen actions.
//!
//! [`run_actions`] runs the enabled post-install actions in a fixed order that
//! does not depend on the checkbox order:
//!
//! 1. **delete downloads** — remove `installer\downloads\` recursively;
//! 2. **create shortcuts** — two desktop shortcuts targeting
//!    `<drive>:\Zona\mo2\ModOrganizer.exe`;
//! 3. **start game** — launch the same executable in `<drive>:\Zona\mo2` with the
//!    Anomaly profile argument.
//!
//! The pure parts (ordering, target path, launch command) are unit-tested; the
//! impure parts are sealed behind the [`FinishEffects`] seam, with [`RealEffects`]
//! doing the real Desktop and game-spawn work.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::app::FinishActionId;
use crate::error::{AppError, Result};
use crate::paths::ResolvedPaths;
use crate::win32::shortcut::{self, ShortcutTarget};

/// The plain MO2 shortcut's base name.
const MO2_SHORTCUT_NAME: &str = "Mod Organizer Zona";

/// The game-launching shortcut's base name.
const LAUNCH_SHORTCUT_NAME: &str = "Launch Zona";

/// The MO2 profile argument the "Launch Zona" shortcut stores, including the
/// quotes MO2 needs because the profile name contains spaces.
const MO2_PROFILE_ARGUMENTS: &str = "\"moshortcut://:Anomaly (DX11-AVX)\"";

/// The same profile argument as a single process argument (no surrounding
/// quotes: `std::process::Command` applies the Windows quoting itself).
const MO2_PROFILE_ARGUMENT: &str = "moshortcut://:Anomaly (DX11-AVX)";

/// The game executable's file name inside the `mo2` directory.
const GAME_EXE_FILE: &str = "ModOrganizer.exe";

/// The shortcut icon shipped beside the tools in `dependencies\`.
const ICON_FILE: &str = "icon.ico";

/// Runs the enabled finish-screen actions in the fixed order delete downloads ->
/// create shortcut -> start game.
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
    let icon = shortcut_icon(&paths.deps);
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
            FinishActionId::CreateShortcut => {
                for shortcut in shortcuts(paths) {
                    log::debug!(
                        target: "finish:run_actions",
                        "finish action: create shortcut {} ({})",
                        shortcut.name,
                        shortcut.executable.display(),
                    );
                    effects.create_shortcut(&shortcut, icon.as_deref())?;
                }
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
        FinishActionId::CreateShortcut,
        FinishActionId::StartGame,
    ]
    .into_iter()
    .filter(|action| enabled.contains(action))
    .collect()
}

/// One desktop shortcut the Create shortcuts action creates.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ShortcutSpec {
    /// The `.lnk` base name (no extension).
    name: &'static str,
    /// The executable the shortcut opens.
    executable: PathBuf,
    /// Arguments passed to the executable, stored verbatim (quotes included).
    arguments: Option<&'static str>,
    /// The working directory the shortcut starts in.
    working_directory: PathBuf,
}

impl ShortcutSpec {
    /// The [`ShortcutTarget`] this spec describes.
    fn target(&self) -> ShortcutTarget<'_> {
        let target =
            ShortcutTarget::new(&self.executable).with_working_directory(&self.working_directory);
        match self.arguments {
            Some(arguments) => target.with_arguments(arguments),
            None => target,
        }
    }
}

/// The two desktop shortcuts the Create shortcuts action creates:
///
/// - `Mod Organizer Zona.lnk` — the plain MO2 launcher;
/// - `Launch Zona.lnk` — starts in `<Zona>\mo2` and passes the Anomaly profile
///   argument.
fn shortcuts(paths: &ResolvedPaths) -> [ShortcutSpec; 2] {
    let executable = shortcut_target(paths);
    [
        ShortcutSpec {
            name: MO2_SHORTCUT_NAME,
            executable: executable.clone(),
            arguments: None,
            working_directory: paths.mo2.clone(),
        },
        ShortcutSpec {
            name: LAUNCH_SHORTCUT_NAME,
            executable,
            arguments: Some(MO2_PROFILE_ARGUMENTS),
            working_directory: paths.mo2.clone(),
        },
    ]
}

/// The game executable the shortcuts target and the start action launches:
/// `<Zona>\mo2\ModOrganizer.exe`.
fn shortcut_target(paths: &ResolvedPaths) -> PathBuf {
    paths.mo2.join(GAME_EXE_FILE)
}

/// The desktop shortcuts' icon: `<deps>\icon.ico` when that file was shipped,
/// else `None` (so the `.lnk` keeps the shell's default icon).
fn shortcut_icon(deps: &Path) -> Option<PathBuf> {
    let icon = deps.join(ICON_FILE);
    icon.is_file().then_some(icon)
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

/// Builds the command that starts the game (pure; nothing is spawned).
///
/// Mirrors the `Launch Zona` shortcut: MO2 runs in `working_directory`
/// (`<Zona>\mo2`) with the Anomaly profile argument. The argument is passed
/// unquoted: `Command` adds the Windows quoting itself.
fn game_command(exe: &Path, working_directory: &Path) -> Command {
    let mut command = Command::new(exe);
    command.current_dir(working_directory);
    command.arg(MO2_PROFILE_ARGUMENT);
    command
}

/// The impure side effects the finish flow needs, injected for testing.
trait FinishEffects {
    /// Creates one desktop shortcut, returning its path. `icon` is the shipped
    /// `icon.ico` when present; `None` keeps the default.
    fn create_shortcut(&self, shortcut: &ShortcutSpec, icon: Option<&Path>) -> Result<PathBuf>;
    /// Launches `exe` with the Anomaly profile argument, starting in
    /// `working_directory`, detached and fire-and-forget.
    fn start_game(&self, exe: &Path, working_directory: &Path) -> Result<()>;
}

/// Production effects: the desktop-shortcut API plus a detached game spawn.
struct RealEffects;

impl FinishEffects for RealEffects {
    fn create_shortcut(&self, shortcut: &ShortcutSpec, icon: Option<&Path>) -> Result<PathBuf> {
        shortcut::create_shortcut(&shortcut.target(), shortcut.name, icon)
    }

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
    let child = game_command(exe, working_directory)
        .spawn()
        .map_err(|source| AppError::Io {
            context: "start the game".to_string(),
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

    /// One shortcut a [`FakeEffects::create_shortcut`] call observed.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct CreatedShortcut {
        name: String,
        executable: PathBuf,
        arguments: Option<String>,
        working_directory: PathBuf,
    }

    /// Records the effects a test asked for, in call order, and creates the
    /// shortcuts in an injected desktop directory (never the real Desktop).
    struct FakeEffects {
        desktop: PathBuf,
        calls: RefCell<Vec<FinishActionId>>,
        shortcuts: RefCell<Vec<CreatedShortcut>>,
        shortcut_icon: RefCell<Option<PathBuf>>,
        game_exe: RefCell<Option<PathBuf>>,
        game_cwd: RefCell<Option<PathBuf>>,
    }

    impl FakeEffects {
        fn new(desktop: PathBuf) -> Self {
            FakeEffects {
                desktop,
                calls: RefCell::new(Vec::new()),
                shortcuts: RefCell::new(Vec::new()),
                shortcut_icon: RefCell::new(None),
                game_exe: RefCell::new(None),
                game_cwd: RefCell::new(None),
            }
        }
    }

    impl FinishEffects for FakeEffects {
        fn create_shortcut(&self, shortcut: &ShortcutSpec, icon: Option<&Path>) -> Result<PathBuf> {
            self.calls.borrow_mut().push(FinishActionId::CreateShortcut);
            self.shortcuts.borrow_mut().push(CreatedShortcut {
                name: shortcut.name.to_string(),
                executable: shortcut.executable.clone(),
                arguments: shortcut.arguments.map(str::to_string),
                working_directory: shortcut.working_directory.clone(),
            });
            *self.shortcut_icon.borrow_mut() = icon.map(Path::to_path_buf);
            let link = self.desktop.join(format!("{}.lnk", shortcut.name));
            std::fs::write(&link, b"stub shortcut").map_err(|source| AppError::Io {
                context: "write stub shortcut".to_string(),
                path: Some(link.clone()),
                source,
            })?;
            Ok(link)
        }

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
    fn shortcuts_are_the_plain_mo2_launcher_and_the_profile_launcher() {
        let root = unique_temp_dir("specs");
        let paths = paths_under(&root);

        let specs = shortcuts(&paths);

        assert_eq!(specs.len(), 2, "Create shortcuts creates two shortcuts");

        // 1. The plain MO2 launcher.
        assert_eq!(specs[0].name, "Mod Organizer Zona");
        assert_eq!(specs[0].executable, paths.mo2.join("ModOrganizer.exe"));
        assert_eq!(specs[0].arguments, None);
        assert_eq!(specs[0].working_directory, paths.mo2);

        // 2. The Anomaly profile launcher: same exe, cwd `<Zona>\mo2`, and the
        // quoted moshortcut argument.
        assert_eq!(specs[1].name, "Launch Zona");
        assert_eq!(specs[1].executable, paths.mo2.join("ModOrganizer.exe"));
        assert_eq!(
            specs[1].arguments,
            Some("\"moshortcut://:Anomaly (DX11-AVX)\"")
        );
        assert_eq!(specs[1].working_directory, paths.mo2);

        std::fs::remove_dir_all(&root).expect("cleanup temp dir");
    }

    #[test]
    fn shortcut_icon_is_the_shipped_icon_only_when_it_exists() {
        let root = unique_temp_dir("icon");
        let deps = root.join("dependencies");
        std::fs::create_dir_all(&deps).expect("create dependencies");

        // Absent (a checkout without the binary asset): no icon, so `IShellLink`
        // keeps the shell default.
        assert_eq!(shortcut_icon(&deps), None);

        let icon = deps.join(ICON_FILE);
        std::fs::write(&icon, b"icon bytes").expect("write icon");
        assert_eq!(shortcut_icon(&deps), Some(icon));

        std::fs::remove_dir_all(&root).expect("cleanup temp dir");
    }

    #[test]
    fn run_actions_hands_the_shipped_icon_to_the_shortcut() {
        let root = unique_temp_dir("icon_run");
        let paths = paths_under(&root);
        std::fs::create_dir_all(&paths.deps).expect("create dependencies");
        let icon = paths.deps.join(ICON_FILE);
        std::fs::write(&icon, b"icon bytes").expect("write icon");

        let desktop = unique_temp_dir("icon_desktop");
        let effects = FakeEffects::new(desktop.clone());

        run_actions_with(&paths, &[FinishActionId::CreateShortcut], &effects).expect("run actions");

        assert_eq!(
            effects.shortcut_icon.borrow().as_deref(),
            Some(icon.as_path()),
            "the shipped dependencies\\icon.ico must reach the shortcut"
        );

        std::fs::remove_dir_all(&desktop).expect("cleanup desktop");
        std::fs::remove_dir_all(&root).expect("cleanup temp dir");
    }

    #[test]
    fn ordered_actions_fix_the_order_regardless_of_checkbox_order() {
        // Deliberately scrambled, with a duplicate: the canonical order wins.
        let enabled = [
            FinishActionId::StartGame,
            FinishActionId::DeleteDownloads,
            FinishActionId::CreateShortcut,
            FinishActionId::StartGame,
        ];
        assert_eq!(
            ordered_actions(&enabled),
            vec![
                FinishActionId::DeleteDownloads,
                FinishActionId::CreateShortcut,
                FinishActionId::StartGame,
            ]
        );

        // Only the checked actions run.
        assert_eq!(
            ordered_actions(&[FinishActionId::StartGame, FinishActionId::DeleteDownloads]),
            vec![FinishActionId::DeleteDownloads, FinishActionId::StartGame]
        );
        assert!(ordered_actions(&[]).is_empty());
    }

    #[test]
    fn run_actions_creates_both_shortcuts_in_the_injected_desktop_and_follows_order() {
        let root = unique_temp_dir("run");
        let mut paths = paths_under(&root);
        let downloads = paths.downloads.clone();
        std::fs::create_dir_all(&downloads).expect("create downloads");
        std::fs::write(downloads.join("a.7z"), b"bytes").expect("write file");

        let desktop = unique_temp_dir("desktop");
        let expected_target = PathBuf::from(r"C:\Zona\mo2\ModOrganizer.exe");
        // Use the canonical `<Zona>\mo2` shape for the target assertion.
        paths.mo2 = PathBuf::from(r"C:\Zona\mo2");
        let effects = FakeEffects::new(desktop.clone());

        run_actions_with(
            &paths,
            &[
                FinishActionId::StartGame,
                FinishActionId::CreateShortcut,
                FinishActionId::DeleteDownloads,
            ],
            &effects,
        )
        .expect("run actions");

        // Order is fixed: delete first (dir gone), then the shortcuts, then game.
        assert!(!downloads.exists(), "downloads must be deleted first");

        // Both shortcuts were created in the injected desktop dir, targeting the
        // MO2 executable.
        for name in ["Mod Organizer Zona", "Launch Zona"] {
            let link = desktop.join(format!("{name}.lnk"));
            assert!(
                link.exists(),
                "{name}.lnk must exist in the injected desktop"
            );
        }

        let created = effects.shortcuts.borrow().clone();
        assert_eq!(
            created,
            vec![
                CreatedShortcut {
                    name: "Mod Organizer Zona".to_string(),
                    executable: expected_target.clone(),
                    arguments: None,
                    working_directory: PathBuf::from(r"C:\Zona\mo2"),
                },
                CreatedShortcut {
                    name: "Launch Zona".to_string(),
                    executable: expected_target.clone(),
                    arguments: Some("\"moshortcut://:Anomaly (DX11-AVX)\"".to_string()),
                    working_directory: PathBuf::from(r"C:\Zona\mo2"),
                },
            ]
        );
        assert_eq!(
            effects.game_exe.borrow().as_deref(),
            Some(expected_target.as_path()),
            "start game must launch the same path as the shortcuts"
        );
        assert_eq!(
            effects.game_cwd.borrow().as_deref(),
            Some(Path::new(r"C:\Zona\mo2")),
            "start game must run in <Zona>\\mo2 like the Launch Zona shortcut"
        );
        assert_eq!(
            *effects.calls.borrow(),
            vec![
                FinishActionId::CreateShortcut,
                FinishActionId::CreateShortcut,
                FinishActionId::StartGame,
            ]
        );
        assert_eq!(
            effects.shortcut_icon.borrow().as_deref(),
            None,
            "nothing shipped under dependencies\\: the shortcuts keep the default icon"
        );

        std::fs::remove_dir_all(&desktop).expect("cleanup desktop");
        std::fs::remove_dir_all(&root).expect("cleanup temp dir");
    }

    #[test]
    fn empty_enabled_list_runs_nothing() {
        let root = unique_temp_dir("empty");
        let paths = paths_under(&root);
        let downloads = paths.downloads.clone();
        std::fs::create_dir_all(&downloads).expect("create downloads");

        let desktop = unique_temp_dir("empty_desktop");
        let effects = FakeEffects::new(desktop.clone());

        run_actions_with(&paths, &[], &effects).expect("no actions");

        assert!(downloads.exists(), "nothing enabled: downloads kept");
        assert!(effects.calls.borrow().is_empty());

        std::fs::remove_dir_all(&desktop).expect("cleanup desktop");
        std::fs::remove_dir_all(&root).expect("cleanup temp dir");
    }

    #[test]
    fn game_command_starts_mo2_in_the_mo2_dir_with_the_anomaly_profile() {
        let exe = PathBuf::from(r"C:\Zona\mo2\ModOrganizer.exe");
        let cwd = PathBuf::from(r"C:\Zona\mo2");
        let cmd = game_command(&exe, &cwd);

        assert_eq!(cmd.get_program(), exe.as_os_str());
        assert_eq!(
            cmd.get_current_dir(),
            Some(cwd.as_path()),
            "the game must start in <Zona>\\mo2 (same as the Launch Zona shortcut)"
        );
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            vec![std::ffi::OsStr::new("moshortcut://:Anomaly (DX11-AVX)")],
            "one argument, unquoted: Command adds the Windows quoting itself"
        );
    }

    #[test]
    fn shortcut_arguments_are_the_quoted_form_of_the_game_argument() {
        // The .lnk stores a raw command line (quotes included), the spawn passes
        // one argument; both must describe the same profile or the two launch
        // paths would diverge.
        assert_eq!(MO2_PROFILE_ARGUMENTS, format!("\"{MO2_PROFILE_ARGUMENT}\""));
    }

    #[test]
    fn start_game_detached_spawns_and_returns_without_gating_on_exit_code() {
        // A real, harmless read-only Windows program that exits NON-ZERO when
        // handed the moshortcut argument. Fire-and-forget must still return Ok:
        // the exit code is not acted on. Never the real game. The mo2 command
        // shape is covered by
        // `game_command_starts_mo2_in_the_mo2_dir_with_the_anomaly_profile`.
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
