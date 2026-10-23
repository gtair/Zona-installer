//! Restart the install from scratch.
//!
//! A fatal install-stage failure offers a "Restart install from scratch" button;
//! `app` routes that event to [`restart_install`]. Restarting is NOT a rollback:
//! the whole `needed_steps` pipeline is re-run from step 1 with the progress bar
//! reset, and already-extracted directories are left in place.

use crate::error::Result;
use crate::gating;

use super::{App, InstallProgress, Phase};

/// Resets the install progress and re-runs the full install pipeline from step 1.
///
/// Called by [`App::handle`](super::App::handle) for
/// [`Event::RestartInstall`](super::Event::RestartInstall) only when the current
/// fatal state is restartable; it re-enters [`Phase::Installing`] and dispatches
/// the install worker. No rollback: this touches no filesystem path.
pub(crate) fn restart_install(app: &mut App) -> Result<()> {
    // Only meaningful from a restartable install-stage fatal.
    let restartable = matches!(&app.view.phase, Phase::Fatal(state) if state.restartable);
    if !restartable {
        return Ok(());
    }

    // Reset the step index / progress bar for the full applicable run.
    let total = match &app.config {
        Some(config) => gating::needed_steps(config, &app.selected).len(),
        None => 0,
    };
    app.view.install = InstallProgress {
        step: 0,
        total,
        label: None,
        percent: None,
        detail: None,
    };

    // Drop any command sender for the old, possibly still-blocked verify_game
    // worker: dropping it unblocks the worker, and replacing `app.worker` below
    // discards its cancellation result so it cannot surface as a new fatal popup.
    app.verify_tx = None;
    app.view.verify_game = None;
    app.view.install_can_start = false;

    // Re-enter the install screen and re-dispatch the pipeline from step 1.
    log::info!(target: "app:restart", "restarting install from scratch");
    app.view.phase = Phase::Installing;
    let start = app.host.start_install;
    start(app)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Event, Host};
    use crate::config::{ChoiceNode, ChoiceSpec, Config, Step};
    use crate::error::{AppError, ErrorId};
    use crate::paths::RootMode;
    use crate::preflight::{PreflightOutcome, SpawnOutcome};
    use crate::win32::relocate::RelocateOutcome;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    // ---- fixtures (hermetic: no real pipeline / fs beyond temp dirs) --------

    fn node(id: &str, children: Vec<ChoiceNode>) -> ChoiceNode {
        ChoiceNode {
            id: id.to_string(),
            label: id.to_string(),
            description: String::new(),
            choices: children,
        }
    }

    fn leaf(id: &str) -> ChoiceNode {
        node(id, Vec::new())
    }

    fn step(name: &str, action: &str, choice: Option<ChoiceSpec>) -> Step {
        Step {
            name: name.to_string(),
            action: action.to_string(),
            choice,
            dir: None,
            file: None,
            target: None,
            from: None,
            source: None,
            replacements: None::<BTreeMap<String, String>>,
        }
    }

    /// `install_zona -> sss23 -> bhs_hud`; the complete root->leaf path
    /// `{install_zona, sss23, bhs_hud}` activates four steps.
    fn config() -> Config {
        Config {
            version: 1,
            choices: vec![node(
                "install_zona",
                vec![node("sss23", vec![leaf("bhs_hud")])],
            )],
            assets: Vec::new(),
            steps: vec![
                step(
                    "prepare_dirs",
                    "create_dir",
                    Some(ChoiceSpec::One("install_zona".to_string())),
                ),
                step("delete_shader_cache", "delete", None),
                step(
                    "install_sss23",
                    "extract",
                    Some(ChoiceSpec::One("sss23".to_string())),
                ),
                step(
                    "install_bhs_hud",
                    "extract",
                    Some(ChoiceSpec::One("bhs_hud".to_string())),
                ),
            ],
        }
    }

    /// The full applicable step list for the complete path
    /// `{install_zona, sss23, bhs_hud}`, in config order.
    const FULL_STEPS: [&str; 4] = [
        "prepare_dirs",
        "delete_shader_cache",
        "install_sss23",
        "install_bhs_hud",
    ];

    fn ok_drives() -> Result<Vec<String>> {
        Ok(vec![r"C:\".to_string()])
    }

    fn load_config(_root: &Path) -> Result<Config> {
        Ok(config())
    }

    fn never_relocate(_exe: &Path, _drive: &Path) -> Result<RelocateOutcome> {
        Ok(RelocateOutcome::Disabled)
    }

    fn ok_download(_app: &mut App) -> Result<()> {
        Ok(())
    }

    fn ok_install(_app: &mut App) -> Result<()> {
        Ok(())
    }

    fn ok_finish(_app: &App) -> Result<()> {
        Ok(())
    }

    /// The taskbar-flash seam: a no-op, so these tests never touch a real
    /// taskbar.
    fn noop_flash() -> Result<()> {
        Ok(())
    }

    /// The relocated-copy hand-off seam: never reached (this host never
    /// relocates).
    fn noop_spawn(
        _mode: &RootMode,
        _new_exe: &Path,
        _old_dir: &Path,
        _launcher: bool,
    ) -> Result<SpawnOutcome> {
        Ok(SpawnOutcome::Disabled)
    }

    /// The launcher MO2 seam: never reached (this host never opens the launcher).
    fn noop_launch(_exe: &Path, _cwd: &Path, _with_profile: bool) -> Result<()> {
        Ok(())
    }

    /// A host whose restart seam is the REAL [`restart_install`] under test and
    /// whose install seam is overridden per test (canned worker, no pipeline).
    fn test_host() -> Host {
        Host {
            drives: ok_drives,
            load_config,
            relocate: never_relocate,
            spawn_relocated_copy: noop_spawn,
            start_download: ok_download,
            start_install: ok_install,
            restart_install: super::restart_install,
            run_finish_actions: ok_finish,
            launch_mo2: noop_launch,
            flash_taskbar: noop_flash,
        }
    }

    fn outcome(install_root: PathBuf) -> PreflightOutcome {
        PreflightOutcome {
            mode: RootMode::Shipped,
            install_root,
            needs_relocation: false,
            launcher: false,
            debug: crate::preflight::DebugOptions::default(),
        }
    }

    fn toggle(id: &str, on: bool) -> Event {
        Event::ChoiceToggled {
            id: id.to_string(),
            on,
        }
    }

    /// An install seam that fails once, then succeeds: records, for every call,
    /// the full `needed_steps` list it would run and the step index the bar was
    /// at when it was dispatched.
    static RESET_CALLS: AtomicUsize = AtomicUsize::new(0);
    static RESET_STEPS: Mutex<Vec<Vec<String>>> = Mutex::new(Vec::new());
    static RESET_INDEX: Mutex<Vec<usize>> = Mutex::new(Vec::new());

    fn recording_install(app: &mut App) -> Result<()> {
        let call = RESET_CALLS.fetch_add(1, Ordering::SeqCst);
        let names: Vec<String> = match &app.config {
            Some(config) => gating::needed_steps(config, &app.selected)
                .iter()
                .map(|step| step.name.clone())
                .collect(),
            None => Vec::new(),
        };
        RESET_STEPS.lock().expect("steps lock").push(names);
        RESET_INDEX
            .lock()
            .expect("index lock")
            .push(app.view.install.step);

        if call == 0 {
            Err(AppError::Fatal {
                message: "install boom".to_string(),
                error_id: ErrorId::new("E_TEST_INSTALL"),
                log_path: None,
            })
        } else {
            Ok(())
        }
    }

    /// After an injected install failure, `RestartInstall` resets the step index
    /// to 0 and re-dispatches the full `needed_steps` run.
    #[test]
    fn restart_resets_progress_and_redispatches_full_run() {
        RESET_CALLS.store(0, Ordering::SeqCst);
        RESET_STEPS.lock().expect("steps lock").clear();
        RESET_INDEX.lock().expect("index lock").clear();

        let root = unique_temp_dir("restart_reset");
        let mut host = test_host();
        host.start_install = recording_install;
        let mut app = App::bootstrap(outcome(root.clone()), host).expect("bootstrap");

        // Pick the complete install_zona -> sss23 -> bhs_hud path: four steps
        // apply, and Confirm is enabled.
        app.handle(toggle("install_zona", true)).expect("branch");
        app.handle(toggle("sss23", true)).expect("level");
        app.handle(toggle("bhs_hud", true)).expect("leaf");
        app.handle(Event::ConfirmChoices).expect("confirm");
        assert_eq!(app.view_model().install.total, FULL_STEPS.len());

        // First run: the injected install failure (install-stage -> restartable).
        let error = app
            .handle(Event::BeginInstall)
            .expect_err("the injected install failure must surface");
        assert_eq!(error.error_id().as_str(), "E_TEST_INSTALL");
        assert!(matches!(
            &app.view_model().phase,
            Phase::Fatal(state) if state.restartable
        ));
        assert_eq!(RESET_CALLS.load(Ordering::SeqCst), 1);

        // The bar had advanced before the failure; the restart must reset it.
        app.view.install.step = 2;
        app.view.install.label = Some("install_sss23".to_string());

        app.handle(Event::RestartInstall).expect("restart");

        // The step index / progress bar is reset to 0 / the full run.
        assert_eq!(app.view_model().install.step, 0, "step index reset to 0");
        assert_eq!(app.view_model().install.label, None, "step label reset");
        assert_eq!(
            app.view_model().install.total,
            FULL_STEPS.len(),
            "the bar holds the full applicable-step denominator"
        );

        // The install seam really ran again, with the FULL needed_steps list and
        // from step 1 (index 0). This is the re-dispatch proof.
        assert_eq!(
            RESET_CALLS.load(Ordering::SeqCst),
            2,
            "RestartInstall must re-invoke the install seam"
        );
        let runs = RESET_STEPS.lock().expect("steps lock").clone();
        assert_eq!(runs.len(), 2);
        assert_eq!(
            runs[1], FULL_STEPS,
            "the re-dispatch carries the full needed_steps list"
        );
        let indexes = RESET_INDEX.lock().expect("index lock").clone();
        assert_eq!(indexes[1], 0, "the re-dispatch starts from step 1");

        cleanup(&root);
    }

    /// An install seam that mimics extraction: on every call it writes a real
    /// temp directory + file under the install root, then fails the first time.
    static EXTRACT_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn extracting_install(app: &mut App) -> Result<()> {
        let call = EXTRACT_CALLS.fetch_add(1, Ordering::SeqCst);
        let dir = app
            .install_root
            .join("anomaly")
            .join("mods")
            .join(format!("mod{call}"));
        std::fs::create_dir_all(&dir).map_err(|source| AppError::Io {
            context: "create extracted dir".to_string(),
            path: Some(dir.clone()),
            source,
        })?;
        let file = dir.join("keep.txt");
        std::fs::write(&file, "extracted").map_err(|source| AppError::Io {
            context: "write extracted file".to_string(),
            path: Some(file),
            source,
        })?;

        if call == 0 {
            Err(AppError::Fatal {
                message: "install boom".to_string(),
                error_id: ErrorId::new("E_TEST_INSTALL"),
                log_path: None,
            })
        } else {
            Ok(())
        }
    }

    /// The re-run does NOT delete or roll back already-extracted directories —
    /// real temp dirs created by the "install" survive the restart.
    #[test]
    fn restart_leaves_already_extracted_dirs_in_place() {
        EXTRACT_CALLS.store(0, Ordering::SeqCst);

        let root = unique_temp_dir("restart_norollback");
        let extracted = root.join("anomaly").join("mods").join("mod0");
        let file = extracted.join("keep.txt");

        let mut host = test_host();
        host.start_install = extracting_install;
        let mut app = App::bootstrap(outcome(root.clone()), host).expect("bootstrap");

        app.handle(toggle("install_zona", true)).expect("branch");
        app.handle(toggle("sss23", true)).expect("level");
        app.handle(toggle("bhs_hud", true)).expect("leaf");
        app.handle(Event::ConfirmChoices).expect("confirm");
        app.handle(Event::BeginInstall)
            .expect_err("the injected install failure must surface");

        // Precondition: the partial run really extracted dirs + a file.
        assert!(
            file.exists(),
            "precondition: the partial run extracted a file"
        );
        assert_eq!(
            std::fs::read(&file).expect("read extracted file"),
            b"extracted"
        );

        app.handle(Event::RestartInstall).expect("restart");

        // The re-run happened AND nothing already-extracted was rolled back.
        assert_eq!(
            EXTRACT_CALLS.load(Ordering::SeqCst),
            2,
            "the restart must re-run the install"
        );
        assert!(
            extracted.exists(),
            "an already-extracted dir must survive the restart"
        );
        assert!(
            file.exists(),
            "an already-extracted file must survive the restart"
        );
        assert_eq!(
            std::fs::read(&file).expect("read extracted file after restart"),
            b"extracted",
            "the extracted file's contents must be untouched"
        );

        cleanup(&root);
    }

    // ---- temp dirs --------------------------------------------------------

    fn unique_temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "zona_installer_restart_{tag}_{}_{}_{}",
            std::process::id(),
            nanos,
            n
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }
}
