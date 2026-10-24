//! Core application state machine: `Phase`, `ViewModel`, `Event` and `App`.
//!
//! This module owns the *state machine* only. It renders no pixels and does no
//! GUI work; `main.rs` forwards [`Event`]s to [`App::handle`] and renders the
//! plain-data [`ViewModel`] that [`App::view_model`] returns. No `egui`/`eframe`
//! type appears here or anywhere in the library crate.
//!
//! # The flow
//!
//! The startup flow is modelled as a [`Phase`] machine:
//!
//! `Preflight -> Relocation { drives } / Choices -> Downloading -> Installing ->
//! VerifyGame -> Finished / Fatal`
//!
//! Every error returned by [`App::handle`] is also recorded in [`Phase::Fatal`],
//! so the GUI can always render the support popup from the view model alone.
//!
//! # Side effects behind an injectable seam
//!
//! The transition logic is kept pure/separated from the filesystem, Win32 and
//! child-process calls through [`Host`]: a bundle of function pointers for the
//! side effects the state machine needs (drive enumeration, config load,
//! relocation, and the worker/action dispatches). [`App::new`] wires
//! [`Host::production`]; tests build an [`App`] with [`App::bootstrap`] and a
//! fake host.

pub mod restart;
pub mod run;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use crate::config::{self, ChoiceId, ChoiceNode, Config};
use crate::error::{AppError, ErrorId, Result};
use crate::finish;
use crate::gating;
use crate::logging;
use crate::paths::{self, RootMode};
pub use crate::pipeline::DownloadStatus;
use crate::pipeline::{ProgressEvent, VerifyCommand};
use crate::preflight::{self, PreflightOutcome, SpawnOutcome};
use crate::win32::drives;
use crate::win32::relocate::{self, RelocateOutcome};
use crate::win32::taskbar;

/// One of the optional post-install actions on the finish screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FinishActionId {
    /// Delete `installer\downloads\` recursively.
    DeleteDownloads,
    /// Start the game.
    StartGame,
}

/// A fatal error surfaced to the support popup.
///
/// `restartable` is `true` only for install-stage failures, which offer the
/// "Restart install from scratch" button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FatalState {
    /// The user-facing message.
    pub message: String,
    /// The stable error id (`AppError::error_id`).
    pub error_id: String,
    /// The run/aria2c log the popup points the user at, when one exists.
    pub log_path: Option<PathBuf>,
    /// Whether the "Restart install from scratch" button applies.
    pub restartable: bool,
}

impl FatalState {
    /// Builds the fatal state from a bubbled-up [`AppError`].
    ///
    /// `run_log` is this run's log file, used only as a FALLBACK: the error's own
    /// log wins, so a download [`AppError::Tool`] still points the user at
    /// aria2c's own log.
    fn from_error(error: &AppError, restartable: bool, run_log: Option<&Path>) -> FatalState {
        FatalState {
            message: error.to_string(),
            error_id: error.error_id().as_str().to_string(),
            log_path: error
                .log_path()
                .map(Path::to_path_buf)
                .or_else(|| run_log.map(Path::to_path_buf)),
            restartable,
        }
    }
}

/// The current screen/flow state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Pre-flight has not been applied yet (transient).
    Preflight,
    /// The user must pick a drive to relocate to from the fixed local drives.
    Relocation {
        /// The `C:\`-style roots offered.
        drives: Vec<String>,
    },
    /// `<drive>:\Zona` already exists: delete it or cancel.
    RelocationTargetExists {
        /// The chosen drive whose `Zona` directory already exists.
        drive: String,
    },
    /// The launcher window (three options: installer / MO2 / game), shown when
    /// the installer starts from its canonical home with `--launcher`.
    Launcher {
        /// Whether `..\mo2\ModOrganizer.exe` exists, i.e. the game is installed.
        /// The MO2/Zona options are disabled when false.
        game_present: bool,
    },
    /// The choice tree is shown (Screen 1).
    Choices,
    /// Downloads are running (Screen 2).
    Downloading,
    /// The install pipeline is running (Screen 3).
    Installing,
    /// The install pipeline is blocked on the game process (Screen 3, gated).
    VerifyGame,
    /// Installation complete (Screen 4).
    Finished,
    /// A fatal error popup is shown.
    Fatal(FatalState),
}

/// Install progress for Screen 3 (the single overall bar + the info line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallProgress {
    /// 0-based index of the running step (the UI adds one for `Step [i]:`).
    pub step: usize,
    /// Number of applicable steps (the bar denominator).
    pub total: usize,
    /// The label of the running step, when known.
    pub label: Option<String>,
    /// The running step's completion percentage (0-100), when the step reports
    /// one (extraction streams real percentages; some steps are indeterminate).
    pub percent: Option<u32>,
    /// A human-readable detail line for the running step (e.g. the file 7z is
    /// extracting right now), when the step reports one.
    pub detail: Option<String>,
}

impl InstallProgress {
    /// The empty progress shown before the pipeline starts.
    fn empty() -> Self {
        InstallProgress {
            step: 0,
            total: 0,
            label: None,
            percent: None,
            detail: None,
        }
    }
}

/// Where the verify-game gate is in its Start -> run -> finish flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyGameStage {
    /// Waiting for the user to click Start game.
    WaitingToStart,
    /// The game process tree is running; Continue is disabled.
    Running,
    /// The game has exited; Continue is enabled.
    Finished,
}

/// The verify-game gate state shown on Screen 3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyGameView {
    /// The game executable that Start game launches.
    pub exe: String,
    /// Where the gate is in the Start -> run -> finish flow.
    pub stage: VerifyGameStage,
}

/// Cosmetic download progress for one Screen-2 row.
///
/// The file it belongs to is the row's own [`DownloadRow::file`]; this record
/// carries only what the bar renders. `pct` is aria2c's reported percentage when
/// the record carried one, otherwise the row's bar is drawn indeterminate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadProgress {
    /// aria2c's reported percentage, if the record carried one.
    pub pct: Option<u32>,
    /// Bytes downloaded so far (aria2c's cosmetic `2.3MiB`-style string).
    pub done: String,
    /// Total size (aria2c's cosmetic string).
    pub total: String,
}

/// One downloadable file shown on Screen 2.
///
/// The file's identity plus the last per-file progress record the download worker
/// streamed for it. `progress` is `None` until a matching
/// [`ProgressEvent::DownloadProgress`] arrives; an already-correct (skipped) file
/// simply never receives one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRow {
    /// The asset id (gating/error identity).
    pub asset_id: String,
    /// The local filename under `downloads\`.
    pub file: String,
    /// Current download status.
    pub status: DownloadStatus,
    /// The last progress record for this file, or `None` until one arrives.
    pub progress: Option<DownloadProgress>,
}

/// The checked/unchecked state of one finish-screen action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FinishAction {
    /// Which optional action.
    pub action: FinishActionId,
    /// Whether its checkbox is checked.
    pub enabled: bool,
}

/// Plain-data snapshot the GUI renders.
///
/// Contains no `egui` type and no logic: `main.rs` only reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewModel {
    /// The current flow state.
    pub phase: Phase,
    /// The choice tree to render on Screen 1.
    pub choices: Vec<ChoiceNode>,
    /// The currently selected set (the picked path).
    pub selected: HashSet<ChoiceId>,
    /// Whether the Confirm button may be enabled on Screen 1: `true` only when
    /// `selected` is a complete root->leaf path.
    pub can_confirm: bool,
    /// Whether the install is ready to start, i.e. the download stage finished
    /// and the Screen-3 Continue button should start the pipeline.
    /// `false` while the pipeline runs, so no Continue button is drawn.
    pub install_can_start: bool,
    /// Install progress for Screen 3.
    pub install: InstallProgress,
    /// The verify-game gate state, when the pipeline is waiting on the user.
    /// `None` outside the gate.
    pub verify_game: Option<VerifyGameView>,
    /// The download rows for Screen 2.
    pub downloads: Vec<DownloadRow>,
    /// What the download stage is doing right now (Screen 2), when known.
    pub download_status: Option<String>,
    /// The finish-screen actions and their checkbox state (Screen 4).
    pub finish_actions: Vec<FinishAction>,
}

impl ViewModel {
    /// The empty view shown before the pre-flight outcome is applied.
    fn empty() -> Self {
        ViewModel {
            phase: Phase::Preflight,
            choices: Vec::new(),
            selected: HashSet::new(),
            can_confirm: false,
            install_can_start: false,
            install: InstallProgress::empty(),
            verify_game: None,
            downloads: Vec::new(),
            download_status: None,
            finish_actions: Vec::new(),
        }
    }
}

/// A user/GUI action forwarded to [`App::handle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A choice node was toggled on/off (Screen 1).
    ChoiceToggled {
        /// The node id.
        id: ChoiceId,
        /// Whether it is now selected.
        on: bool,
    },
    /// The user confirmed the choices (Screen 1 -> download).
    ConfirmChoices,
    /// The user started the install pipeline (Screen 3).
    BeginInstall,
    /// The user launched the verify-game executable (Screen 3).
    StartGame,
    /// The user acknowledged the cleared verify-game gate (Screen 3, after the
    /// game closed). The Screen-3 Continue click before the pipeline starts is
    /// [`Event::BeginInstall`], not this event.
    Continue,
    /// The user picked a relocation drive (Screen pre-flight).
    DriveChosen {
        /// A `C:\`-style root.
        drive: String,
    },
    /// The user answered the `<drive>:\Zona` exists prompt.
    RelocateTarget {
        /// `true` deletes the existing `Zona` directory, `false` cancels.
        delete: bool,
    },
    /// The user chose to proceed into the installer from the launcher.
    LaunchInstaller,
    /// The user chose to launch Mod Organizer (no profile) from the launcher.
    LaunchMo2,
    /// The user chose to launch the game (Anomaly profile) from the launcher.
    LaunchZona,
    /// A finish-screen checkbox was toggled (Screen 4).
    FinishAction {
        /// Which action.
        action: FinishActionId,
        /// The new checkbox state.
        on: bool,
    },
    /// The user asked to restart the install from scratch.
    RestartInstall,
    /// The user pressed Finish (Screen 4).
    Finish,
    /// The user cancelled the current screen.
    Cancel,
}

/// The injectable side-effect seam (see the module docs).
///
/// Each field is a plain function pointer so the whole bundle is `Copy` and a
/// test can override exactly the effects it needs.
#[derive(Clone, Copy)]
pub(crate) struct Host {
    /// Enumerates the fixed local drives offered by the picker.
    pub(crate) drives: fn() -> Result<Vec<String>>,
    /// Loads and parses `dependencies\config.yaml` under an install root.
    pub(crate) load_config: fn(&Path) -> Result<Config>,
    /// Performs the relocation move (does not delete an existing target).
    pub(crate) relocate: fn(&Path, &Path) -> Result<RelocateOutcome>,
    /// Spawns the relocated copy with `/delete <old_dir>` so it can remove the
    /// old folder and carry the run on. The bool forwards `--launcher`. Returns
    /// [`SpawnOutcome::Disabled`] under the dev override.
    pub(crate) spawn_relocated_copy: fn(&RootMode, &Path, &Path, bool) -> Result<SpawnOutcome>,
    /// Starts the download worker.
    pub(crate) start_download: fn(&mut App) -> Result<()>,
    /// Starts the install worker.
    pub(crate) start_install: fn(&mut App) -> Result<()>,
    /// Dispatches the restart-from-scratch flow.
    pub(crate) restart_install: fn(&mut App) -> Result<()>,
    /// Runs the enabled finish-screen actions.
    pub(crate) run_finish_actions: fn(&App) -> Result<()>,
    /// Launches MO2 detached; the bool adds the Anomaly profile argument (the
    /// launcher's "Launch MO2" / "Launch Zona").
    pub(crate) launch_mo2: fn(&Path, &Path, bool) -> Result<()>,
    /// Requests a taskbar flash because the installer now needs user input.
    pub(crate) flash_taskbar: fn() -> Result<()>,
}

impl Host {
    /// The production host: the real Win32/fs calls plus the worker/action
    /// dispatches (see the module docs).
    pub(crate) fn production() -> Host {
        Host {
            drives: fixed_drives,
            load_config: load_config_from_root,
            relocate: relocate_to_drive,
            spawn_relocated_copy: preflight::spawn_relocated_copy,
            start_download: run::start_download,
            start_install: run::start_install,
            restart_install: restart::restart_install,
            run_finish_actions: production_finish_actions,
            launch_mo2: finish::launch_mo2,
            flash_taskbar: taskbar::flash_own_window,
        }
    }
}

/// The application core: the state machine plus the pending progress receiver.
pub struct App {
    view: ViewModel,
    host: Host,
    config: Option<Config>,
    selected: HashSet<ChoiceId>,
    install_root: PathBuf,
    /// The pre-flight root mode, kept so the relocation hand-off can pass it to
    /// [`Host::spawn_relocated_copy`]. It decides whether the spawn is disabled
    /// (dev override).
    mode: RootMode,
    /// Whether the run was launched with `--launcher`, so a relocation hand-off
    /// can forward it to the relocated copy.
    launcher: bool,
    /// The last drive list, kept so cancel/re-pick needs no Win32 call.
    drives: Vec<String>,
    progress_rx: Option<Receiver<ProgressEvent>>,
    /// The UI repaint callback (the worker "wake signal"), set by the GUI.
    wake: Option<run::Wake>,
    /// The most recently spawned background worker (download or install).
    worker: Option<run::Worker>,
    /// Whether the download stage has finished and the install is ready to start.
    ///
    /// Set by [`ProgressEvent::DownloadsComplete`] (which also moves the phase to
    /// `Installing`) and consumed by [`App::begin_install`], which clears it as
    /// it dispatches the install worker. A second Continue click while the
    /// pipeline runs is then a no-op.
    install_ready: bool,
    /// The command channel to the blocked `verify_game` worker.
    ///
    /// Set when the install worker is spawned and cleared on restart. Sending
    /// [`VerifyCommand::Start`] / [`VerifyCommand::Continue`] resumes the worker;
    /// dropping the sender unblocks a stale worker.
    verify_tx: Option<Sender<VerifyCommand>>,
    /// Whether `Event::Finish` has run the finish actions successfully.
    ///
    /// A private flag rather than a [`Phase`] or a [`ViewModel`] field;
    /// [`App::should_close`] exposes it to `main.rs`. Set ONLY on `Ok`.
    finish_done: bool,
    /// Whether the relocation hand-off successfully spawned the relocated copy.
    ///
    /// Set ONLY on [`SpawnOutcome::Spawned`]: the old process then asks to close
    /// ([`App::should_close`]) so the relocated copy owns the run.
    hand_off_done: bool,
    /// Whether the launcher launched MO2/the game; the installer then closes.
    launcher_done: bool,
}

impl App {
    /// Production constructor: runs the real pre-flight.
    ///
    /// Tests must NOT call this: it acquires the production single-instance
    /// mutex. Use [`App::bootstrap`] with a fake [`Host`] instead.
    pub fn new() -> Result<App> {
        let exe_dir = current_exe_dir()?;
        let outcome = preflight::run(&exe_dir)?;
        App::bootstrap(outcome, Host::production())
    }

    /// The injectable core: applies an already-computed pre-flight outcome and
    /// drives the state machine through `host`.
    ///
    /// `Crate`-visible so sibling tests can build an `App` without the real
    /// pre-flight.
    pub(crate) fn bootstrap(outcome: PreflightOutcome, host: Host) -> Result<App> {
        let install_root = outcome.install_root.clone();
        let mode = outcome.mode.clone();
        let launcher = outcome.launcher;
        let mut app = App {
            view: ViewModel::empty(),
            host,
            config: None,
            selected: HashSet::new(),
            install_root,
            mode,
            launcher,
            drives: Vec::new(),
            progress_rx: None,
            wake: None,
            worker: None,
            install_ready: false,
            verify_tx: None,
            finish_done: false,
            hand_off_done: false,
            launcher_done: false,
        };
        if outcome.needs_relocation {
            let drives = (app.host.drives)()?;
            if drives.is_empty() {
                return Err(AppError::Fatal {
                    message: "no fixed local drives are available to install to".to_string(),
                    error_id: ErrorId::new("E_NO_DRIVES"),
                    log_path: None,
                });
            }
            app.drives = drives.clone();
            app.view.phase = Phase::Relocation { drives };
        } else if launcher {
            app.enter_launcher();
        } else {
            app.enter_choices()?;
        }
        Ok(app)
    }

    /// The plain-data snapshot the GUI renders.
    pub fn view_model(&self) -> &ViewModel {
        &self.view
    }

    /// Whether the installer should close its viewport.
    ///
    /// Becomes `true` once [`Event::Finish`] has run the finish actions
    /// successfully, once a successful relocation hand-off has spawned the
    /// relocated copy, or once the launcher launched MO2/the game; `main.rs`
    /// then sends `egui::ViewportCommand::Close`.
    pub fn should_close(&self) -> bool {
        self.finish_done || self.hand_off_done || self.launcher_done
    }

    /// Handles one GUI event.
    ///
    /// Any error is recorded in [`Phase::Fatal`] *and* returned, so the caller
    /// can both render the popup and observe the failure.
    pub fn handle(&mut self, event: Event) -> Result<()> {
        // Once this process has asked the viewport to close, input arriving in
        // the frames before the close takes effect must not be acted on. This
        // covers BOTH terminal transitions that set `should_close()`: a
        // successful `Finish` (where a second `Finish` would re-run the finish
        // actions and launch the game twice) and the relocation hand-off (where a
        // second `DriveChosen` re-enters the relocation against the already-moved
        // root). The guard is `should_close()`, not a phase check: `finish()`'s
        // phase check is exactly what fails to stop the double-run, and it is not
        // a bare `finish_done` check, because a FAILED `Finish` must not freeze
        // the UI (the guard stays open and the Fatal popup's events are still
        // handled).
        if self.should_close() {
            return Ok(());
        }
        match self.apply(event) {
            Ok(()) => Ok(()),
            Err(error) => {
                // Classify from the phase reached before the failure: only
                // install-stage failures offer "restart from scratch". `apply`
                // sets the stage phase before it calls the fallible seam, so an
                // install-worker error is seen while in `Installing`/`VerifyGame`.
                let restartable = matches!(&self.view.phase, Phase::Installing | Phase::VerifyGame);
                log::error!(target: "app:handle", "fatal: {}: {error}", error.error_id());
                self.enter_fatal(&error, restartable);
                Err(error)
            }
        }
    }

    /// Polls the background workers' channel and returns the pending progress
    /// events, applying each to the view model first.
    ///
    /// Also surfaces a background worker's failure: if the most recent worker has
    /// finished with an `Err`, that error is recorded in [`Phase::Fatal`]. The
    /// receiver is attached by the worker plumbing ([`run`]); before any worker
    /// is started it is absent and this returns an empty vec.
    pub fn drain_progress(&mut self) -> Vec<ProgressEvent> {
        let mut events = Vec::new();
        if let Some(rx) = &self.progress_rx {
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
        }
        for event in &events {
            self.apply_progress(event);
        }

        // A finished-and-failed worker becomes a fatal popup: never silent.
        let worker_error = match &self.worker {
            Some(worker) => worker.take_error(),
            None => None,
        };
        if let Some(error) = worker_error {
            if !matches!(&self.view.phase, Phase::Fatal(_)) {
                // Same classification as `handle`: only install-stage failures
                // offer "restart from scratch".
                let restartable = matches!(&self.view.phase, Phase::Installing | Phase::VerifyGame);
                log::error!(target: "app:drain_progress", "fatal: {}: {error}", error.error_id());
                self.enter_fatal(&error, restartable);
            }
        }
        events
    }

    /// Records a fatal error in the view model and notifies the user.
    ///
    /// The single home of the `Phase::Fatal` transition, so every entry path --
    /// a synchronous [`App::handle`] failure and an asynchronous worker failure
    /// -- flips the phase and flashes exactly once.
    fn enter_fatal(&mut self, error: &AppError, restartable: bool) {
        let run_log = logging::current_log_path();
        self.view.phase = Phase::Fatal(FatalState::from_error(
            error,
            restartable,
            run_log.as_deref(),
        ));
        self.notify_input_needed();
    }

    /// Requests a taskbar flash because user input is now needed.
    ///
    /// Best-effort by design: a failed Win32 call is logged and swallowed rather
    /// than turned into a fatal error. Called once on entering each state that
    /// needs the user.
    fn notify_input_needed(&self) {
        let flash = self.host.flash_taskbar;
        if let Err(error) = flash() {
            log::warn!(target: "app:notify_input_needed", "taskbar flash failed: {error}");
        }
    }

    /// Registers the UI's repaint callback (the worker "wake signal").
    ///
    /// The worker threads call it after every progress event (and once when they
    /// finish), so the GUI can call `egui::Context::request_repaint()` promptly.
    /// Plain data: no GUI type is named here.
    pub fn set_repaint(&mut self, repaint: run::Wake) {
        self.wake = Some(repaint);
    }

    /// Attaches the background workers' progress channel.
    ///
    /// The worker side owns the [`std::sync::mpsc::Sender`]; attaching the
    /// receiving half here is what makes [`App::drain_progress`] return the
    /// workers' events.
    pub fn attach_progress_receiver(&mut self, rx: Receiver<ProgressEvent>) {
        self.progress_rx = Some(rx);
    }

    /// The transition core: mutates state without classifying errors as fatal.
    fn apply(&mut self, event: Event) -> Result<()> {
        match event {
            Event::ChoiceToggled { id, on } => {
                self.toggle_choice(&id, on);
                Ok(())
            }
            Event::ConfirmChoices => self.confirm_choices(),
            Event::BeginInstall => self.begin_install(),
            Event::StartGame => self.start_game(),
            Event::Continue => self.acknowledge_verify(),
            Event::DriveChosen { drive } => self.choose_drive(&drive),
            Event::RelocateTarget { delete } => self.resolve_target(delete),
            Event::LaunchInstaller => self.enter_choices(),
            Event::LaunchMo2 => self.launch_mo2(false),
            Event::LaunchZona => self.launch_mo2(true),
            Event::FinishAction { action, on } => {
                self.set_finish_action(action, on);
                Ok(())
            }
            Event::RestartInstall => self.dispatch_restart(),
            Event::Finish => self.finish(),
            Event::Cancel => {
                self.cancel();
                Ok(())
            }
        }
    }

    /// Loads the config from the current install root and enters `Choices`.
    ///
    /// Logging is opened first thing at startup ([`preflight::run`]), before any
    /// of this runs, so this method only records the loaded config.
    fn enter_choices(&mut self) -> Result<()> {
        let load = self.host.load_config;
        let config = match load(&self.install_root) {
            Ok(config) => config,
            Err(error) => {
                log::error!(target: "app:enter_choices", "config load failed: {error}");
                return Err(error);
            }
        };

        let root = self.install_root.display().to_string();
        log::info!(target: "app:enter_choices", "install root: {root}");
        if let Some(path) = logging::current_log_path() {
            log::info!(target: "app:enter_choices", "log file: {}", path.display());
        }
        log::info!(
            target: "app:enter_choices",
            "config: {} choices / {} assets / {} steps",
            config.choices.len(),
            config.assets.len(),
            config.steps.len(),
        );

        self.view.choices = config.choices.clone();
        self.selected.clear();
        self.view.selected.clear();
        // An empty selection is never a complete root->leaf path.
        self.view.can_confirm = is_complete_choice_path(&config.choices, &self.selected);
        self.config = Some(config);
        self.view.phase = Phase::Choices;
        Ok(())
    }

    /// Enters the launcher window, resolving whether the game is installed.
    ///
    /// Reached only from a canonical install (`needs_relocation == false`) with
    /// `--launcher`. `game_present` is the filesystem answer to "does
    /// `..\mo2\ModOrganizer.exe` exist?"; the two launch options are disabled
    /// when it is false.
    fn enter_launcher(&mut self) {
        let paths = paths::resolved_paths(&self.install_root);
        let game_exe = paths.mo2.join(finish::GAME_EXE_FILE);
        let game_present = game_exe.is_file();
        log::info!(
            target: "app:enter_launcher",
            "launcher shown; game present: {game_present} ({})",
            game_exe.display(),
        );
        self.view.phase = Phase::Launcher { game_present };
    }

    /// `ChoiceToggled`: cascading pick-to-set update.
    ///
    /// Selecting a node replaces the set with the full root..node path, which
    /// enforces "one pick per level" and the mutual exclusivity of the top-level
    /// branches. Deselecting a node removes it and its descendants.
    fn toggle_choice(&mut self, id: &str, on: bool) {
        if !matches!(&self.view.phase, Phase::Choices) {
            return;
        }
        let config = match &self.config {
            Some(config) => config,
            None => return,
        };
        let mut selected = self.selected.clone();
        if let Some(path) = resolve_choice_path(&config.choices, id, &selected) {
            if on {
                selected = path.into_iter().collect();
            } else if let Some(node) = node_at_path(&config.choices, &path) {
                remove_ids(node, &mut selected);
            }
        }
        self.selected = selected;
        self.view.selected = self.selected.clone();
        // Recompute completeness after every change.
        self.view.can_confirm = is_complete_choice_path(&config.choices, &self.selected);
    }

    /// `ConfirmChoices`: resolves the work set and enters `Downloading`.
    ///
    /// A no-op (the phase stays `Choices`) while [`ViewModel::can_confirm`] is
    /// false, i.e. the selection is not a complete root->leaf path.
    fn confirm_choices(&mut self) -> Result<()> {
        if !matches!(&self.view.phase, Phase::Choices) {
            return Ok(());
        }
        if !self.view.can_confirm {
            return Ok(());
        }
        let (downloads, total) = match &self.config {
            Some(config) => {
                let downloads = gating::needed_assets(config, &self.selected)
                    .into_iter()
                    .map(|asset| DownloadRow {
                        asset_id: asset.id.clone(),
                        file: asset.file.clone(),
                        status: DownloadStatus::Queued,
                        progress: None,
                    })
                    .collect::<Vec<_>>();
                let total = gating::needed_steps(config, &self.selected).len();
                (downloads, total)
            }
            None => return Ok(()),
        };
        // The ids, sorted, so a run log records WHICH options were picked and the
        // line is deterministic across runs (the selected set is unordered).
        let mut selected: Vec<&str> = self.selected.iter().map(String::as_str).collect();
        selected.sort_unstable();
        log::info!(
            target: "app:confirm_choices",
            "selection confirmed: [{}]; {} assets, {total} steps",
            selected.join(", "),
            downloads.len(),
        );
        self.view.downloads = downloads;
        self.view.download_status = None;
        self.view.install = InstallProgress {
            step: 0,
            total,
            label: None,
            percent: None,
            detail: None,
        };
        // A fresh download stage: the install is not ready until its worker
        // reports `DownloadsComplete`.
        self.install_ready = false;
        self.view.install_can_start = false;
        self.view.verify_game = None;
        self.view.phase = Phase::Downloading;
        let start = self.host.start_download;
        start(self)
    }

    /// `BeginInstall`: starts the install pipeline and enters `Installing`.
    ///
    /// Accepted from two states: (a) `Downloading`, the pre-existing direct path
    /// (not reachable from the GUI, which shows no button there, but driven by
    /// the app/restart tests), and (b) `Installing` while `install_ready` is set,
    /// i.e. the download stage just finished and the real Screen-3 Continue click
    /// starts the pipeline. The flag is cleared as the install is dispatched, so
    /// a later Continue click while the pipeline runs is a no-op (no double
    /// dispatch).
    fn begin_install(&mut self) -> Result<()> {
        let from_downloading = matches!(&self.view.phase, Phase::Downloading);
        let from_downloads_complete =
            matches!(&self.view.phase, Phase::Installing) && self.install_ready;
        if !from_downloading && !from_downloads_complete {
            return Ok(());
        }
        self.install_ready = false;
        // The Continue button must disappear for the rest of the pipeline.
        self.view.install_can_start = false;
        self.view.verify_game = None;
        self.view.phase = Phase::Installing;
        let start = self.host.start_install;
        start(self)
    }

    /// `StartGame`: launches the game from the verify-game gate.
    ///
    /// Only meaningful while the gate is waiting for the user to start the game.
    fn start_game(&mut self) -> Result<()> {
        if let Some(verify) = &mut self.view.verify_game {
            if verify.stage == VerifyGameStage::WaitingToStart {
                if let Some(tx) = &self.verify_tx {
                    let _ = tx.send(VerifyCommand::Start);
                }
                verify.stage = VerifyGameStage::Running;
            }
        }
        Ok(())
    }

    /// `Continue`: acknowledges the cleared verify-game gate.
    ///
    /// Sending [`VerifyCommand::Continue`] resumes the worker's remaining steps.
    /// The phase returns to `Installing` only on this explicit acknowledgement,
    /// never automatically when the game exits.
    fn acknowledge_verify(&mut self) -> Result<()> {
        let finished = matches!(
            &self.view.verify_game,
            Some(verify) if verify.stage == VerifyGameStage::Finished
        );
        if !finished {
            return Ok(());
        }
        if let Some(tx) = &self.verify_tx {
            let _ = tx.send(VerifyCommand::Continue);
        }
        self.view.verify_game = None;
        self.view.phase = Phase::Installing;
        Ok(())
    }

    /// `DriveChosen`: relocates to the chosen drive.
    fn choose_drive(&mut self, drive: &str) -> Result<()> {
        if !matches!(&self.view.phase, Phase::Relocation { .. }) {
            return Ok(());
        }
        let relocate = self.host.relocate;
        let outcome = relocate(&self.install_root, Path::new(drive))?;
        self.apply_relocate_outcome(drive, outcome)
    }

    /// Applies a relocation outcome to the state machine.
    fn apply_relocate_outcome(&mut self, drive: &str, outcome: RelocateOutcome) -> Result<()> {
        match outcome {
            // Relocation disabled (dev override): continue with the normal flow.
            RelocateOutcome::Disabled => self.enter_choices(),
            // `<drive>:\Zona` exists: ask the user to delete or cancel.
            RelocateOutcome::TargetExists { .. } => {
                self.view.phase = Phase::RelocationTargetExists {
                    drive: drive.to_string(),
                };
                Ok(())
            }
            // Moved: hand the run off to the relocated copy. The old process
            // spawns `<new_exe> /delete <old_dir>` and then asks to close
            // ([`App::should_close`]); the receiving copy deletes the old folder
            // and reaches its own pre-flight at the new root.
            RelocateOutcome::Relocated {
                install_root,
                new_exe,
                old_dir,
            } => {
                self.install_root = install_root;
                let mode = self.mode.clone();
                let spawn = self.host.spawn_relocated_copy;
                match spawn(&mode, &new_exe, &old_dir, self.launcher)? {
                    SpawnOutcome::Spawned { .. } => {
                        // The old process stays on the drive picker while it
                        // exits (it never reaches Choices; the relocated copy owns
                        // the run).
                        self.view.phase = Phase::Relocation {
                            drives: self.drives.clone(),
                        };
                        self.hand_off_done = true;
                        Ok(())
                    }
                    // Defensive (dev override): no hand-off is possible, so keep
                    // the run in this process on the normal flow.
                    SpawnOutcome::Disabled => self.enter_choices(),
                }
            }
        }
    }

    /// `RelocateTarget`: delete the existing `Zona` directory or cancel.
    fn resolve_target(&mut self, delete: bool) -> Result<()> {
        let drive = match &self.view.phase {
            Phase::RelocationTargetExists { drive } => drive.clone(),
            _ => return Ok(()),
        };
        if !delete {
            // Cancel: back to the drive picker; the target is left untouched.
            self.view.phase = Phase::Relocation {
                drives: self.drives.clone(),
            };
            return Ok(());
        }
        let zona_dir = relocate::zona_dir_for_drive(Path::new(&drive));
        if relocate::path_is_under(&self.install_root, &zona_dir) {
            return Err(AppError::Fatal {
                message: format!(
                    "{} contains this installer ({}); deleting it would destroy the running \
                     installer. Move the installer folder out of it and run it again.",
                    zona_dir.display(),
                    self.install_root.display()
                ),
                error_id: ErrorId::new("E_TARGET_CONTAINS_INSTALLER"),
                log_path: None,
            });
        }
        delete_zona_target(&zona_dir)?;
        let relocate = self.host.relocate;
        let outcome = relocate(&self.install_root, Path::new(&drive))?;
        self.apply_relocate_outcome(&drive, outcome)
    }

    /// `LaunchMo2` / `LaunchZona`: spawns MO2 detached (with the Anomaly profile
    /// argument when `with_profile`) and asks to close. Only meaningful from the
    /// launcher while the game is installed.
    fn launch_mo2(&mut self, with_profile: bool) -> Result<()> {
        if !matches!(&self.view.phase, Phase::Launcher { game_present: true }) {
            return Ok(());
        }
        let paths = paths::resolved_paths(&self.install_root);
        let exe = paths.mo2.join(finish::GAME_EXE_FILE);
        let launch = self.host.launch_mo2;
        launch(&exe, &paths.mo2, with_profile)?;
        log::info!(
            target: "app:launch_mo2",
            "launcher launched MO2 (profile: {with_profile}) {}",
            exe.display(),
        );
        self.launcher_done = true;
        Ok(())
    }

    /// `FinishAction`: records a finish-screen checkbox (Screen 4).
    fn set_finish_action(&mut self, action: FinishActionId, on: bool) {
        if !matches!(&self.view.phase, Phase::Finished) {
            return;
        }
        for entry in &mut self.view.finish_actions {
            if entry.action == action {
                entry.enabled = on;
            }
        }
    }

    /// `RestartInstall`: dispatches to the restart seam when the fatal state is
    /// restartable; otherwise the event is recognised and ignored.
    fn dispatch_restart(&mut self) -> Result<()> {
        let restartable = matches!(&self.view.phase, Phase::Fatal(state) if state.restartable);
        if !restartable {
            return Ok(());
        }
        let restart = self.host.restart_install;
        restart(self)
    }

    /// `Finish`: runs the enabled finish-screen actions and, on success, records
    /// that the installer should close.
    ///
    /// The [`App::should_close`] flag is set ONLY on `Ok`: a failed `Finish` is
    /// classified as [`Phase::Fatal`] by [`App::handle`] and leaves the flag
    /// `false`.
    fn finish(&mut self) -> Result<()> {
        if !matches!(&self.view.phase, Phase::Finished) {
            return Ok(());
        }
        let run = self.host.run_finish_actions;
        log::info!(target: "app:finish", "finish actions beginning");
        run(self)?;
        log::info!(target: "app:finish", "finish actions completed");
        self.finish_done = true;
        Ok(())
    }

    /// `Cancel`: aborts the current screen back to the choice screen (or the
    /// drive picker while relocating). Recognised and otherwise a no-op.
    fn cancel(&mut self) {
        match &self.view.phase {
            Phase::Relocation { .. } | Phase::RelocationTargetExists { .. } => {
                if !self.drives.is_empty() {
                    self.view.phase = Phase::Relocation {
                        drives: self.drives.clone(),
                    };
                }
            }
            _ if self.config.is_some() => {
                self.view.phase = Phase::Choices;
            }
            _ => {}
        }
    }

    /// Applies one progress event from the workers to the view model.
    fn apply_progress(&mut self, event: &ProgressEvent) {
        // Events still queued when a failure was recorded must not overwrite the
        // fatal popup (and the error has already been taken, so it would be lost).
        if matches!(&self.view.phase, Phase::Fatal(_)) {
            return;
        }
        match event {
            ProgressEvent::StepStarted { index, name } => {
                self.view.install.step = *index;
                self.view.install.label = Some(name.clone());
                // A fresh step: clear the previous step's live detail/percent
                // until the step reports its own.
                self.view.install.percent = None;
                self.view.install.detail = None;
            }
            // Live progress inside the running step: move the bar and update the
            // detail line under it. Events for a step that is no longer current
            // are ignored.
            ProgressEvent::StepProgress {
                index,
                percent,
                detail,
            } => {
                if self.view.install.step == *index {
                    self.view.install.percent = *percent;
                    self.view.install.detail = detail.clone();
                }
            }
            ProgressEvent::StepDone { index, .. } => {
                if self.view.install.step == *index {
                    self.view.install.percent = Some(100);
                    self.view.install.detail = None;
                }
                if self.view.install.total > 0 && index + 1 >= self.view.install.total {
                    self.enter_finished();
                }
            }
            // The verify-game gate became ready: show the message + Start-game
            // button and keep the phase gated until the user clicks Continue.
            ProgressEvent::VerifyGameReady { exe } => {
                if self.view.verify_game.is_none() {
                    self.view.verify_game = Some(VerifyGameView {
                        exe: exe.clone(),
                        stage: VerifyGameStage::WaitingToStart,
                    });
                }
                // Flash only on a real phase change: a repeated Ready while
                // already gated must not flash again.
                if !matches!(&self.view.phase, Phase::VerifyGame) {
                    self.view.phase = Phase::VerifyGame;
                    self.notify_input_needed();
                }
            }
            // The game closed: enable Continue (the user must acknowledge it),
            // so this fresh "input needed" moment flashes.
            ProgressEvent::VerifyGameFinished => {
                if let Some(verify) = &mut self.view.verify_game {
                    verify.stage = VerifyGameStage::Finished;
                }
                self.notify_input_needed();
            }
            // A per-file download record: update the matching row by file name.
            // An unknown file (no row with that name) is ignored, and every
            // other row is left untouched.
            ProgressEvent::DownloadProgress {
                file,
                pct,
                done,
                total,
            } => {
                if let Some(row) = self
                    .view
                    .downloads
                    .iter_mut()
                    .find(|row| row.file.as_str() == file.as_str())
                {
                    row.progress = Some(DownloadProgress {
                        pct: *pct,
                        done: done.clone(),
                        total: total.clone(),
                    });
                    if row.status != DownloadStatus::Completed
                        && row.status != DownloadStatus::Failed
                    {
                        row.status = DownloadStatus::Downloading;
                    }
                }
            }
            ProgressEvent::DownloadFileStatus { file, status } => {
                if let Some(row) = self
                    .view
                    .downloads
                    .iter_mut()
                    .find(|row| row.file.as_str() == file.as_str())
                {
                    row.status = *status;
                    if *status == DownloadStatus::Completed {
                        if let Some(p) = &mut row.progress {
                            p.pct = Some(100);
                            p.done = p.total.clone();
                        } else {
                            row.progress = Some(DownloadProgress {
                                pct: Some(100),
                                done: "Done".to_string(),
                                total: "Done".to_string(),
                            });
                        }
                    }
                }
            }
            // The download stage succeeded: show the install screen with the
            // install ready to start (Continue enabled). Only meaningful while
            // still downloading; anywhere else it is ignored.
            ProgressEvent::DownloadStatus { message } => {
                if matches!(&self.view.phase, Phase::Downloading) {
                    self.view.download_status = Some(message.clone());
                }
            }
            ProgressEvent::DownloadsComplete => {
                if matches!(&self.view.phase, Phase::Downloading) {
                    self.view.download_status = None;
                    self.install_ready = true;
                    // Show the Continue button that starts the pipeline.
                    self.view.install_can_start = true;
                    self.view.phase = Phase::Installing;
                    // The download -> install transition needs the user.
                    self.notify_input_needed();
                }
            }
        }
    }

    /// Enters `Finished` and seeds the three unchecked finish actions.
    ///
    /// The finish screen needs the user, so entering it flashes. The flash is
    /// gated on the phase actually changing: a repeated final `StepDone` re-seeds
    /// the checkboxes exactly as before but must not flash again.
    fn enter_finished(&mut self) {
        let entering = !matches!(&self.view.phase, Phase::Finished);
        self.view.phase = Phase::Finished;
        self.view.finish_actions = vec![
            FinishAction {
                action: FinishActionId::DeleteDownloads,
                enabled: false,
            },
            FinishAction {
                action: FinishActionId::StartGame,
                enabled: false,
            },
        ];
        if entering {
            self.notify_input_needed();
        }
    }
}

/// The root..node id path a toggle of `id` should act on, resolved against the
/// current selection context (pure).
///
/// The tree is not a set of unique ids: the real `config.yaml` repeats `sss21`,
/// `sss23`, `bhs_hud` and `clean_hud` under all three top-level branches
/// (`install_zona`, `update_graphics`, `update_zona`). A naive depth-first first
/// match would therefore let a toggle of a repeated id silently jump to a
/// different branch. Instead every path ending at `id` is collected and scored:
/// the path with the longest prefix already contained in `selected` wins (i.e.
/// the occurrence inside the currently selected context), which also covers the
/// "top-level branch already selected" case because the branch is the first
/// element of the path. Ties -- in particular when nothing is selected yet, so
/// a bare leaf/branch id is being established -- fall back to config order
/// (`min_by_key` returns the first minimum).
fn resolve_choice_path(
    nodes: &[ChoiceNode],
    id: &str,
    selected: &HashSet<ChoiceId>,
) -> Option<Vec<ChoiceId>> {
    let mut candidates = Vec::new();
    collect_paths(nodes, id, &mut Vec::new(), &mut candidates);
    candidates
        .into_iter()
        .min_by_key(|path| std::cmp::Reverse(selected_prefix_len(path, selected)))
}

/// Length of the longest prefix of `path` whose ids are all in `selected` (pure).
///
/// `selected` is always a single root..node path, so this is the depth at which
/// `path` diverges from the current selection (0 when the root branch already
/// differs).
fn selected_prefix_len(path: &[ChoiceId], selected: &HashSet<ChoiceId>) -> usize {
    path.iter().take_while(|id| selected.contains(*id)).count()
}

/// Collects every root..`id` path in the tree, in config order (pure).
fn collect_paths(
    nodes: &[ChoiceNode],
    id: &str,
    prefix: &mut Vec<ChoiceId>,
    out: &mut Vec<Vec<ChoiceId>>,
) {
    for node in nodes {
        prefix.push(node.id.clone());
        if node.id == id {
            out.push(prefix.clone());
        }
        collect_paths(&node.choices, id, prefix, out);
        prefix.pop();
    }
}

/// The node reached by following `path` from `nodes` (pure).
fn node_at_path<'a>(nodes: &'a [ChoiceNode], path: &[ChoiceId]) -> Option<&'a ChoiceNode> {
    let (first, rest) = path.split_first()?;
    let node = nodes.iter().find(|node| &node.id == first)?;
    if rest.is_empty() {
        Some(node)
    } else {
        node_at_path(&node.choices, rest)
    }
}

/// Whether `selected` is a COMPLETE root->leaf path in `nodes`, i.e. the deepest
/// selected node has no children.
///
/// The core keeps `selected` as a single root..node path (see `toggle_choice`),
/// so the deepest selected node is found by descending from the roots. An empty
/// selection (or one that does not start at a root) is incomplete.
fn is_complete_choice_path(nodes: &[ChoiceNode], selected: &HashSet<ChoiceId>) -> bool {
    let path = selected_path(nodes, selected);
    match node_at_path(nodes, &path) {
        Some(node) => node.choices.is_empty(),
        None => false,
    }
}

/// The root..deepest-selected-node id path implied by `selected` (pure).
///
/// At each level exactly one node id is on the selected path, so it is the one
/// found; the walk stops when no child of the current node is selected (the
/// deepest selected node). An empty selection yields an empty path.
fn selected_path(nodes: &[ChoiceNode], selected: &HashSet<ChoiceId>) -> Vec<ChoiceId> {
    let mut path = Vec::new();
    let mut current = nodes;
    while let Some(node) = current.iter().find(|node| selected.contains(&node.id)) {
        path.push(node.id.clone());
        current = &node.choices;
    }
    path
}

/// Removes a node and all of its descendant ids from `selected` (pure).
fn remove_ids(node: &ChoiceNode, selected: &mut HashSet<ChoiceId>) {
    selected.remove(&node.id);
    for child in &node.choices {
        remove_ids(child, selected);
    }
}

/// Deletes a pre-existing `<drive>:\Zona`.
///
/// A missing directory is treated as already deleted; any other I/O failure is
/// fatal.
fn delete_zona_target(zona_dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(zona_dir) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(AppError::Io {
            context: "delete existing Zona target".to_string(),
            path: Some(zona_dir.to_path_buf()),
            source,
        }),
    }
}

/// The directory the running executable lives in.
fn current_exe_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().map_err(|source| AppError::Io {
        context: "locate running executable".to_string(),
        path: None,
        source,
    })?;
    match exe.parent() {
        Some(parent) => Ok(parent.to_path_buf()),
        None => Err(AppError::Io {
            context: "running executable has no parent directory".to_string(),
            path: Some(exe),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no parent directory"),
        }),
    }
}

/// Production `load_config`: reads and parses `dependencies\config.yaml`.
fn load_config_from_root(install_root: &Path) -> Result<Config> {
    let path = paths::resolved_paths(install_root).config;
    let text = std::fs::read_to_string(&path).map_err(|source| AppError::Io {
        context: "read config.yaml".to_string(),
        path: Some(path.clone()),
        source,
    })?;
    config::parse_config(&text)
}

/// Production `relocate`: the real relocation move.
fn relocate_to_drive(exe_dir: &Path, drive: &Path) -> Result<RelocateOutcome> {
    relocate::relocate(exe_dir, drive)
}

/// Production `drives`: the real fixed-local-drive enumeration.
fn fixed_drives() -> Result<Vec<String>> {
    drives::fixed_local_drives()
}

/// Production `run_finish_actions`: runs the finish screen's enabled actions.
///
/// Resolves the install root's derived paths, collects the enabled
/// [`FinishActionId`]s from the view model and delegates to
/// [`finish::run_actions`].
fn production_finish_actions(app: &App) -> Result<()> {
    let paths = paths::resolved_paths(&app.install_root);
    let enabled = app
        .view
        .finish_actions
        .iter()
        .filter(|entry| entry.enabled)
        .map(|entry| entry.action)
        .collect::<Vec<FinishActionId>>();
    finish::run_actions(&paths, &enabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Asset, ChoiceSpec, Step};
    use crate::paths::RootMode;
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    // ---- fixtures ---------------------------------------------------------

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

    /// The three mutually exclusive top-level branches with nested picks.
    fn tree() -> Vec<ChoiceNode> {
        vec![
            node(
                "install_zona",
                vec![leaf("sss21"), node("sss23", vec![leaf("bhs_hud")])],
            ),
            node("update_graphics", vec![leaf("only_gfx")]),
            node("update_zona", vec![leaf("only_zona")]),
        ]
    }

    /// One top-level branch shaped like the real `config.yaml`: `sss21`/`sss23`
    /// (with their `bhs_hud`/`clean_hud` children) repeat under every branch.
    fn branch(id: &str) -> ChoiceNode {
        node(
            id,
            vec![
                node("sss21", vec![leaf("bhs_hud")]),
                node("sss23", vec![leaf("clean_hud"), leaf("bhs_hud")]),
            ],
        )
    }

    /// The choice tree with ids repeated under all three branches (regression).
    fn repeated_tree() -> Vec<ChoiceNode> {
        vec![
            branch("install_zona"),
            branch("update_graphics"),
            branch("update_zona"),
        ]
    }

    fn asset(id: &str, choice: ChoiceSpec) -> Asset {
        Asset {
            id: id.to_string(),
            file: format!("{id}.7z"),
            url: "https://example.com/download".to_string(),
            sha256: "00112233445566778899aabbccddeeff".to_string(),
            choice,
        }
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

    fn config() -> Config {
        Config {
            version: 1,
            current_version: None,
            github_repo: None,
            choices: tree(),
            assets: vec![
                asset("template", ChoiceSpec::One("base".to_string())),
                asset("zona", ChoiceSpec::One("install_zona".to_string())),
                asset("sss23", ChoiceSpec::One("sss23".to_string())),
            ],
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
            ],
        }
    }

    fn outcome(needs_relocation: bool, install_root: PathBuf) -> PreflightOutcome {
        PreflightOutcome {
            mode: RootMode::Shipped,
            install_root,
            needs_relocation,
            launcher: false,
            debug: preflight::DebugOptions::default(),
        }
    }

    /// A `PreflightOutcome` launched with `--launcher`.
    fn launcher_outcome(needs_relocation: bool, install_root: PathBuf) -> PreflightOutcome {
        PreflightOutcome {
            mode: RootMode::Shipped,
            install_root,
            needs_relocation,
            launcher: true,
            debug: preflight::DebugOptions::default(),
        }
    }

    // ---- injectable host fns ---------------------------------------------

    fn ok_drives() -> Result<Vec<String>> {
        Ok(vec!["C:\\".to_string()])
    }

    fn ok_config(_root: &Path) -> Result<Config> {
        Ok(config())
    }

    /// Like `ok_config` but with the ids-repeated-under-every-branch tree.
    fn repeated_config(_root: &Path) -> Result<Config> {
        let mut config = config();
        config.choices = repeated_tree();
        Ok(config)
    }

    /// A deterministic, filesystem-based stand-in for `relocate`: it reports
    /// `TargetExists` exactly when `<drive>\Zona` exists on disk.
    fn relocate_by_fs(exe_dir: &Path, drive: &Path) -> Result<RelocateOutcome> {
        let zona_dir = relocate::zona_dir_for_drive(drive);
        if zona_dir.exists() {
            Ok(RelocateOutcome::TargetExists { zona_dir })
        } else {
            let install_root = relocate::install_root_for_drive(drive);
            Ok(RelocateOutcome::Relocated {
                new_exe: install_root.join("zona-installer.exe"),
                install_root,
                old_dir: exe_dir.to_path_buf(),
            })
        }
    }

    fn ok_download(_app: &mut App) -> Result<()> {
        Ok(())
    }

    fn ok_install(_app: &mut App) -> Result<()> {
        Ok(())
    }

    fn fail_download(_app: &mut App) -> Result<()> {
        Err(AppError::Fatal {
            message: "download boom".to_string(),
            error_id: ErrorId::new("E_TEST_DOWNLOAD"),
            log_path: None,
        })
    }

    fn fail_install(_app: &mut App) -> Result<()> {
        Err(AppError::Fatal {
            message: "install boom".to_string(),
            error_id: ErrorId::new("E_TEST_INSTALL"),
            log_path: None,
        })
    }

    /// Counts dispatches of the restart seam across the one test that sends
    /// `Event::RestartInstall`.
    static RESTART_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn record_restart(_app: &mut App) -> Result<()> {
        RESTART_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Counts dispatches of the install seam (`BeginInstall` must dispatch it
    /// exactly once).
    static INSTALL_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn counting_install(_app: &mut App) -> Result<()> {
        INSTALL_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn ok_finish(_app: &App) -> Result<()> {
        Ok(())
    }

    fn fail_finish(_app: &App) -> Result<()> {
        Err(AppError::Fatal {
            message: "finish boom".to_string(),
            error_id: ErrorId::new("E_TEST_FINISH"),
            log_path: None,
        })
    }

    /// Counts `run_finish_actions` dispatches: two `Finish` events in a row must
    /// run the actions exactly once. A `fn` pointer cannot capture, so the count
    /// is a process-global static; `FINISH_LOCK` serializes the tests that set
    /// this seam.
    static FINISH_CALLS: AtomicUsize = AtomicUsize::new(0);
    static FINISH_LOCK: Mutex<()> = Mutex::new(());

    /// Counts a finish-actions dispatch and reports success.
    fn counting_finish(_app: &App) -> Result<()> {
        FINISH_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    // ---- taskbar-flash seam -----------------------------------------------

    /// Counts taskbar-flash requests.
    static FLASH_CALLS: AtomicUsize = AtomicUsize::new(0);
    /// Serializes the flash-recording tests (the counter is a process-global
    /// static and tests run on parallel threads).
    static FLASH_LOCK: Mutex<()> = Mutex::new(());

    /// The default test seam: a flash that does nothing and is NOT counted.
    fn noop_flash() -> Result<()> {
        Ok(())
    }

    /// Counts a flash request and reports success.
    fn record_flash() -> Result<()> {
        FLASH_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// A failing flash seam: counts the attempt and returns an error, proving
    /// `notify_input_needed` is best-effort and never fatal.
    fn fail_flash() -> Result<()> {
        FLASH_CALLS.fetch_add(1, Ordering::SeqCst);
        Err(AppError::Win32 {
            context: "flash test failure".to_string(),
            code: 1400,
        })
    }

    // ---- hand-off seam fakes ---------------------------------------------

    /// Records each `spawn_relocated_copy` call and reports success. A `fn`
    /// pointer cannot capture, so the observations go in a static.
    static SPAWN_CALLS: AtomicUsize = AtomicUsize::new(0);
    static SPAWN_ARGS: Mutex<Vec<(PathBuf, PathBuf)>> = Mutex::new(Vec::new());
    static SPAWN_LAUNCHER: Mutex<Vec<bool>> = Mutex::new(Vec::new());
    /// Serializes the recording-spawn tests (the observations are process-global
    /// statics, and tests run on parallel threads).
    static SPAWN_LOCK: Mutex<()> = Mutex::new(());

    /// Counts `relocate` seam calls: the re-entry-guard regression test asserts a
    /// second `DriveChosen` after a hand-off never reaches the seam again.
    static RELOCATE_CALLS: AtomicUsize = AtomicUsize::new(0);

    /// A recording stand-in for `relocate`: the first call CREATES
    /// `<drive>\Zona\installer` and reports `Relocated`; a second call against
    /// the same drive then reports `TargetExists`. That is the re-entry scenario
    /// the guard test asserts on (via the phase, not merely the call count).
    fn record_relocate_creating_target(exe_dir: &Path, drive: &Path) -> Result<RelocateOutcome> {
        RELOCATE_CALLS.fetch_add(1, Ordering::SeqCst);
        let zona_dir = relocate::zona_dir_for_drive(drive);
        if zona_dir.exists() {
            return Ok(RelocateOutcome::TargetExists { zona_dir });
        }
        let install_root = relocate::install_root_for_drive(drive);
        std::fs::create_dir_all(&install_root).map_err(|source| AppError::Io {
            context: "create relocated install root".to_string(),
            path: Some(install_root.clone()),
            source,
        })?;
        Ok(RelocateOutcome::Relocated {
            new_exe: install_root.join("zona-installer.exe"),
            install_root,
            old_dir: exe_dir.to_path_buf(),
        })
    }

    fn record_spawn_ok(
        _mode: &RootMode,
        new_exe: &Path,
        old_dir: &Path,
        launcher: bool,
    ) -> Result<SpawnOutcome> {
        SPAWN_CALLS.fetch_add(1, Ordering::SeqCst);
        SPAWN_ARGS
            .lock()
            .expect("spawn args lock")
            .push((new_exe.to_path_buf(), old_dir.to_path_buf()));
        SPAWN_LAUNCHER
            .lock()
            .expect("spawn launcher lock")
            .push(launcher);
        Ok(SpawnOutcome::Spawned {
            command: relocate::self_delete_command(new_exe, old_dir),
        })
    }

    /// A hand-off seam that never spawns (the dev-override shape): the old
    /// process keeps the run and enters `Choices`.
    fn spawn_disabled(
        _mode: &RootMode,
        _new_exe: &Path,
        _old_dir: &Path,
        _launcher: bool,
    ) -> Result<SpawnOutcome> {
        Ok(SpawnOutcome::Disabled)
    }

    /// A hand-off seam that fails: a failed spawn must be fatal and must NOT
    /// close the old process.
    fn spawn_fails(
        _mode: &RootMode,
        _new_exe: &Path,
        _old_dir: &Path,
        _launcher: bool,
    ) -> Result<SpawnOutcome> {
        Err(AppError::Io {
            context: "spawn relocated installer".to_string(),
            path: None,
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "boom"),
        })
    }

    /// Counts launcher MO2 spawns and records the (exe, cwd, with_profile) args.
    static LAUNCH_CALLS: AtomicUsize = AtomicUsize::new(0);
    static LAUNCH_ARGS: Mutex<Vec<(PathBuf, PathBuf, bool)>> = Mutex::new(Vec::new());
    /// Serializes the recording-launch tests.
    static LAUNCH_LOCK: Mutex<()> = Mutex::new(());

    fn ok_launch(_exe: &Path, _cwd: &Path, _with_profile: bool) -> Result<()> {
        Ok(())
    }

    fn record_launch(exe: &Path, cwd: &Path, with_profile: bool) -> Result<()> {
        LAUNCH_CALLS.fetch_add(1, Ordering::SeqCst);
        LAUNCH_ARGS
            .lock()
            .expect("launch args lock")
            .push((exe.to_path_buf(), cwd.to_path_buf(), with_profile));
        Ok(())
    }

    fn fail_launch(_exe: &Path, _cwd: &Path, _with_profile: bool) -> Result<()> {
        Err(AppError::Fatal {
            message: "launch boom".to_string(),
            error_id: ErrorId::new("E_TEST_LAUNCH"),
            log_path: None,
        })
    }

    fn test_host() -> Host {
        Host {
            drives: ok_drives,
            load_config: ok_config,
            relocate: relocate_by_fs,
            spawn_relocated_copy: spawn_disabled,
            start_download: ok_download,
            start_install: ok_install,
            restart_install: record_restart,
            run_finish_actions: ok_finish,
            launch_mo2: ok_launch,
            flash_taskbar: noop_flash,
        }
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
            "zona_installer_app_{tag}_{}_{}_{}",
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

    fn set(ids: &[&str]) -> HashSet<ChoiceId> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    fn toggle(id: &str, on: bool) -> Event {
        Event::ChoiceToggled {
            id: id.to_string(),
            on,
        }
    }

    /// Picks the full `install_zona -> sss23 -> bhs_hud` path in the test tree,
    /// i.e. a complete root->leaf path, so `ConfirmChoices` is enabled.
    fn pick_complete_path(app: &mut App) {
        for id in ["install_zona", "sss23", "bhs_hud"] {
            app.handle(toggle(id, true)).expect("pick complete path");
        }
    }

    // ---- tests ------------------------------------------------------------

    /// Driving `handle` produces the expected `phase`.
    #[test]
    fn handle_drives_the_expected_phase() {
        let root = unique_temp_dir("phases");
        let mut app = App::bootstrap(outcome(false, root.clone()), test_host()).expect("bootstrap");
        assert_eq!(app.view_model().phase, Phase::Choices);

        app.handle(Event::ChoiceToggled {
            id: "install_zona".to_string(),
            on: true,
        })
        .expect("toggle branch");
        assert_eq!(app.view_model().selected, set(&["install_zona"]));
        assert_eq!(app.view_model().phase, Phase::Choices);

        app.handle(Event::ChoiceToggled {
            id: "sss23".to_string(),
            on: true,
        })
        .expect("toggle leaf");
        assert_eq!(app.view_model().selected, set(&["install_zona", "sss23"]));

        // Reaching a leaf completes the root->leaf path, so Confirm is enabled.
        app.handle(Event::ChoiceToggled {
            id: "bhs_hud".to_string(),
            on: true,
        })
        .expect("toggle hud leaf");
        assert_eq!(
            app.view_model().selected,
            set(&["install_zona", "sss23", "bhs_hud"])
        );
        assert!(app.view_model().can_confirm);

        app.handle(Event::ConfirmChoices).expect("confirm");
        assert_eq!(app.view_model().phase, Phase::Downloading);
        // base + install_zona + sss23 assets.
        assert_eq!(app.view_model().downloads.len(), 3);
        // three applicable steps -> the install bar denominator.
        assert_eq!(app.view_model().install.total, 3);

        app.handle(Event::BeginInstall).expect("begin");
        assert_eq!(app.view_model().phase, Phase::Installing);

        cleanup(&root);
    }

    /// An injected `Err` transitions to `Fatal`.
    #[test]
    fn injected_error_transitions_to_fatal() {
        let root = unique_temp_dir("fatal");
        let mut host = test_host();
        host.start_download = fail_download;
        let mut app = App::bootstrap(outcome(false, root.clone()), host).expect("bootstrap");
        pick_complete_path(&mut app);

        let error = app
            .handle(Event::ConfirmChoices)
            .expect_err("the injected error must surface");
        assert_eq!(error.error_id().as_str(), "E_TEST_DOWNLOAD");

        match &app.view_model().phase {
            Phase::Fatal(state) => {
                assert_eq!(state.error_id, "E_TEST_DOWNLOAD");
                assert!(
                    state.message.contains("download boom"),
                    "got {:?}",
                    state.message
                );
                // Not an install-stage failure: no restart offered.
                assert!(!state.restartable);
            }
            other => panic!("expected Phase::Fatal, got {other:?}"),
        }

        cleanup(&root);
    }

    /// Progress events drive the verify gate, the live step details, and the
    /// terminal finish state.
    #[test]
    fn progress_events_drive_verify_and_finish() {
        let root = unique_temp_dir("progress");
        let mut app = App::bootstrap(outcome(false, root.clone()), test_host()).expect("bootstrap");
        pick_complete_path(&mut app);
        app.handle(Event::ConfirmChoices).expect("confirm");
        app.handle(Event::BeginInstall).expect("begin");

        let (tx, rx) = mpsc::channel();
        app.attach_progress_receiver(rx);

        // Live step detail updates the view while the step runs.
        tx.send(ProgressEvent::StepStarted {
            index: 0,
            name: "install_base_anomaly".to_string(),
        })
        .expect("send started");
        tx.send(ProgressEvent::StepProgress {
            index: 0,
            percent: Some(47),
            detail: Some("Extracting 47% | file.bin".to_string()),
        })
        .expect("send progress");
        app.drain_progress();
        assert_eq!(app.view_model().install.step, 0);
        assert_eq!(app.view_model().install.percent, Some(47));
        assert_eq!(
            app.view_model().install.detail.as_deref(),
            Some("Extracting 47% | file.bin")
        );

        // The verify-game gate: Ready shows WaitingToStart and gates the screen.
        tx.send(ProgressEvent::VerifyGameReady {
            exe: "anomaly.exe".to_string(),
        })
        .expect("send ready");
        assert_eq!(
            app.drain_progress(),
            vec![ProgressEvent::VerifyGameReady {
                exe: "anomaly.exe".to_string(),
            }]
        );
        assert_eq!(app.view_model().phase, Phase::VerifyGame);
        assert_eq!(
            app.view_model().verify_game.as_ref().map(|view| view.stage),
            Some(VerifyGameStage::WaitingToStart)
        );

        // Start game -> Running; Continue before the game finishes is a no-op.
        app.handle(Event::StartGame).expect("start game");
        assert_eq!(
            app.view_model().verify_game.as_ref().map(|view| view.stage),
            Some(VerifyGameStage::Running)
        );
        app.handle(Event::Continue)
            .expect("continue before finish is ignored");
        assert_eq!(
            app.view_model().phase,
            Phase::VerifyGame,
            "the gate persists until the game has finished AND the user continues"
        );

        // The game closed -> Finished; Continue acknowledges and resumes.
        tx.send(ProgressEvent::VerifyGameFinished)
            .expect("send finished");
        app.drain_progress();
        assert_eq!(
            app.view_model().verify_game.as_ref().map(|view| view.stage),
            Some(VerifyGameStage::Finished)
        );
        app.handle(Event::Continue).expect("acknowledge");
        assert!(app.view_model().verify_game.is_none());
        assert_eq!(app.view_model().phase, Phase::Installing);

        // The last step finishing (index 2 of 3) completes the install.
        tx.send(ProgressEvent::StepDone {
            index: 2,
            name: "install_sss23".to_string(),
        })
        .expect("send done");
        app.drain_progress();
        assert_eq!(app.view_model().phase, Phase::Finished);
        assert_eq!(app.view_model().finish_actions.len(), 2);

        app.handle(Event::FinishAction {
            action: FinishActionId::DeleteDownloads,
            on: true,
        })
        .expect("toggle action");
        assert!(app
            .view_model()
            .finish_actions
            .iter()
            .any(|entry| entry.action == FinishActionId::DeleteDownloads && entry.enabled));

        // `Finish` dispatches to the (stubbed but Ok here) finish seam.
        app.handle(Event::Finish).expect("finish");
        assert!(app.should_close(), "an Ok finish closes the installer");

        cleanup(&root);
    }

    /// The two finish actions, all unchecked (Screen 4 defaults).
    fn unset_finish_actions() -> Vec<FinishAction> {
        vec![
            FinishAction {
                action: FinishActionId::DeleteDownloads,
                enabled: false,
            },
            FinishAction {
                action: FinishActionId::StartGame,
                enabled: false,
            },
        ]
    }

    /// A successful `Finish` flips `should_close()` and stays non-fatal; a failed
    /// one leaves it `false` and goes `Fatal`.
    #[test]
    fn finish_ok_sets_should_close_and_err_is_fatal() {
        let root = unique_temp_dir("finish_close");

        let mut app = App::bootstrap(outcome(false, root.clone()), test_host()).expect("bootstrap");
        app.view.phase = Phase::Finished;
        app.view.finish_actions = unset_finish_actions();
        assert!(!app.should_close(), "not closed before Finish");

        app.handle(Event::Finish).expect("finish succeeds");
        assert!(app.should_close(), "an Ok finish closes the installer");
        assert_eq!(
            app.view_model().phase,
            Phase::Finished,
            "an Ok finish stays Finished"
        );

        // A failing seam: `should_close()` stays false and the phase is Fatal.
        let mut failing = test_host();
        failing.run_finish_actions = fail_finish;
        let mut app2 = App::bootstrap(outcome(false, root.clone()), failing).expect("bootstrap");
        app2.view.phase = Phase::Finished;
        app2.view.finish_actions = unset_finish_actions();
        let error = app2
            .handle(Event::Finish)
            .expect_err("the injected error must surface");
        assert_eq!(error.error_id().as_str(), "E_TEST_FINISH");
        assert!(
            !app2.should_close(),
            "a failed finish must NOT close the installer"
        );
        assert!(
            matches!(app2.view_model().phase, Phase::Fatal(_)),
            "a failed finish is Fatal"
        );

        cleanup(&root);
    }

    /// A second `Event::Finish` delivered before the viewport close takes effect
    /// must NOT re-run the finish actions (`start game` is a detached spawn, so a
    /// double-click would launch the game twice).
    #[test]
    fn a_second_finish_runs_the_finish_actions_exactly_once() {
        let _guard = FINISH_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        FINISH_CALLS.store(0, Ordering::SeqCst);

        let root = unique_temp_dir("finish_reentry");
        let mut host = test_host();
        host.run_finish_actions = counting_finish;
        let mut app = App::bootstrap(outcome(false, root.clone()), host).expect("bootstrap");
        app.view.phase = Phase::Finished;
        app.view.finish_actions = unset_finish_actions();

        app.handle(Event::Finish).expect("first finish succeeds");
        assert!(app.should_close(), "an Ok finish asks to close");
        assert_eq!(
            FINISH_CALLS.load(Ordering::SeqCst),
            1,
            "the first Finish runs the actions once"
        );

        // The close has been requested but not yet taken effect; a second Finish
        // arriving now must be ignored, not re-run the actions.
        app.handle(Event::Finish)
            .expect("a second finish is ignored");
        assert_eq!(
            FINISH_CALLS.load(Ordering::SeqCst),
            1,
            "a second Finish must not re-run the finish actions"
        );
        assert!(
            app.should_close(),
            "the close request survives the ignored event"
        );

        cleanup(&root);
    }

    /// A `Finish` whose seam returns `Err` must NOT set `should_close()` (the
    /// flag is set only on `Ok`), so the guard stays open and the Fatal popup's
    /// events are still handled.
    #[test]
    fn a_failed_finish_leaves_the_guard_open_and_handles_later_events() {
        let root = unique_temp_dir("finish_fail_guard");
        let mut host = test_host();
        host.run_finish_actions = fail_finish;
        let mut app = App::bootstrap(outcome(false, root.clone()), host).expect("bootstrap");
        app.view.phase = Phase::Finished;
        app.view.finish_actions = unset_finish_actions();

        let error = app
            .handle(Event::Finish)
            .expect_err("the injected error must surface");
        assert_eq!(error.error_id().as_str(), "E_TEST_FINISH");
        assert!(
            !app.should_close(),
            "a failed finish must leave should_close() false"
        );
        assert!(matches!(app.view_model().phase, Phase::Fatal(_)));

        // The guard did not swallow the fatal path: a later event is still
        // handled. `Cancel` leaves the Fatal popup and returns to `Choices`
        // because the config is already loaded -- an observable state change
        // proving the event reached the state machine.
        app.handle(Event::Cancel)
            .expect("a later event must still be handled");
        assert_eq!(
            app.view_model().phase,
            Phase::Choices,
            "the event was handled, not swallowed by the guard"
        );

        cleanup(&root);
    }

    /// The production wrapper resolves the install root's paths and runs exactly
    /// the enabled actions (delete downloads here); with nothing enabled it is a
    /// no-op that touches neither the Desktop nor a game.
    #[test]
    fn production_finish_actions_deletes_downloads_only_when_enabled() {
        let root = unique_temp_dir("prod_finish");
        let downloads = root.join("downloads");
        std::fs::create_dir_all(downloads.join("nested")).expect("create downloads");
        std::fs::write(downloads.join("a.7z"), b"bytes").expect("write file");

        let production = Host::production().run_finish_actions;

        // Nothing enabled: a no-op — the downloads dir is kept, and neither the
        // real Desktop (shortcut) nor a game is touched.
        let mut idle =
            App::bootstrap(outcome(false, root.clone()), test_host()).expect("bootstrap");
        idle.view.phase = Phase::Finished;
        idle.view.finish_actions = unset_finish_actions();
        production(&idle).expect("nothing enabled is a no-op");
        assert!(downloads.exists(), "nothing enabled: downloads kept");

        // Only DeleteDownloads: the wrapper deletes `<install_root>\downloads`.
        let mut active =
            App::bootstrap(outcome(false, root.clone()), test_host()).expect("bootstrap");
        active.view.phase = Phase::Finished;
        active.view.finish_actions = vec![
            FinishAction {
                action: FinishActionId::DeleteDownloads,
                enabled: true,
            },
            FinishAction {
                action: FinishActionId::StartGame,
                enabled: false,
            },
        ];
        production(&active).expect("delete downloads");
        assert!(
            !downloads.exists(),
            "the wrapper must delete <install_root>\\downloads"
        );

        cleanup(&root);
    }

    /// The download worker's `DownloadsComplete` signal moves
    /// `Downloading -> Installing` with the install NOT yet started; the Screen-3
    /// Continue click (`BeginInstall`) then dispatches the install seam exactly
    /// once and a second click is a no-op.
    #[test]
    fn downloads_complete_enables_exactly_one_begin_install() {
        INSTALL_CALLS.store(0, Ordering::SeqCst);
        let root = unique_temp_dir("downloads_complete");
        let mut host = test_host();
        host.start_install = counting_install;
        let mut app = App::bootstrap(outcome(false, root.clone()), host).expect("bootstrap");
        pick_complete_path(&mut app);
        app.handle(Event::ConfirmChoices).expect("confirm");
        assert_eq!(app.view_model().phase, Phase::Downloading);

        // Drain the download worker's success signal.
        let (tx, rx) = mpsc::channel();
        app.attach_progress_receiver(rx);
        tx.send(ProgressEvent::DownloadsComplete)
            .expect("send complete");
        assert_eq!(app.drain_progress(), vec![ProgressEvent::DownloadsComplete]);
        assert_eq!(app.view_model().phase, Phase::Installing);
        assert_eq!(
            INSTALL_CALLS.load(Ordering::SeqCst),
            0,
            "the install must not start until the user clicks Continue"
        );

        // Continue starts the pipeline exactly once.
        app.handle(Event::BeginInstall).expect("begin install");
        assert_eq!(INSTALL_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(app.view_model().phase, Phase::Installing);

        // A second Continue click while the pipeline runs is a no-op.
        app.handle(Event::BeginInstall)
            .expect("second click is a no-op");
        assert_eq!(
            INSTALL_CALLS.load(Ordering::SeqCst),
            1,
            "a later BeginInstall must not re-dispatch the install"
        );

        cleanup(&root);
    }

    /// A drained `DownloadProgress` event updates the matching row's `progress`
    /// (by file) and leaves every other row untouched; an unknown file is
    /// ignored.
    #[test]
    fn download_progress_event_updates_matching_row_only() {
        let root = unique_temp_dir("download_progress");
        let mut app = App::bootstrap(outcome(false, root.clone()), test_host()).expect("bootstrap");
        pick_complete_path(&mut app);
        app.handle(Event::ConfirmChoices).expect("confirm");
        assert_eq!(app.view_model().phase, Phase::Downloading);
        // base + install_zona + sss23 assets, seeded with no progress yet.
        assert!(app
            .view_model()
            .downloads
            .iter()
            .all(|row| row.progress.is_none()));

        let (tx, rx) = mpsc::channel();
        app.attach_progress_receiver(rx);
        tx.send(ProgressEvent::DownloadProgress {
            file: "zona.7z".to_string(),
            pct: Some(42),
            done: "1.0MiB".to_string(),
            total: "2.4MiB".to_string(),
        })
        .expect("send progress");
        app.drain_progress();

        let downloads = &app.view_model().downloads;
        let zona = downloads
            .iter()
            .find(|row| row.file == "zona.7z")
            .expect("the zona row exists");
        assert_eq!(
            zona.progress,
            Some(DownloadProgress {
                pct: Some(42),
                done: "1.0MiB".to_string(),
                total: "2.4MiB".to_string(),
            })
        );
        for row in downloads.iter().filter(|row| row.file != "zona.7z") {
            assert_eq!(row.progress, None, "row {} must be untouched", row.file);
        }

        // An unknown file is ignored: no panic, no row changes.
        let before = app.view_model().downloads.clone();
        tx.send(ProgressEvent::DownloadProgress {
            file: "not_downloaded.7z".to_string(),
            pct: Some(99),
            done: "9MiB".to_string(),
            total: "9MiB".to_string(),
        })
        .expect("send unknown progress");
        app.drain_progress();
        assert_eq!(
            app.view_model().downloads,
            before,
            "an unknown file changes nothing"
        );

        cleanup(&root);
    }

    /// `RelocationTargetExists` is reached only when the relocate outcome reports
    /// the target exists.
    #[test]
    fn relocation_target_exists_only_when_outcome_reports_it() {
        let base = unique_temp_dir("reloc_present");
        let drive = base.join("drive");
        let drive_str = drive.to_string_lossy().to_string();

        // (a) No `<drive>\Zona`: a clean drive hands off to the relocated copy —
        // the old process spawns it once and asks to close, staying on the drive
        // picker rather than advancing to Choices.
        let _guard = SPAWN_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        SPAWN_CALLS.store(0, Ordering::SeqCst);
        SPAWN_ARGS.lock().expect("spawn args lock").clear();
        let mut host = test_host();
        host.spawn_relocated_copy = record_spawn_ok;
        let mut app =
            App::bootstrap(outcome(true, base.join("installer")), host).expect("bootstrap");
        assert!(matches!(app.view_model().phase, Phase::Relocation { .. }));
        app.handle(Event::DriveChosen {
            drive: drive_str.clone(),
        })
        .expect("clean drive");
        assert_eq!(SPAWN_CALLS.load(Ordering::SeqCst), 1);
        assert!(
            matches!(app.view_model().phase, Phase::Relocation { .. }),
            "the hand-off must not advance the OLD process to Choices"
        );
        assert!(app.should_close(), "a successful hand-off asks to close");

        // (b) With an existing `<drive>\Zona`, the outcome reports TargetExists.
        let zona = relocate::zona_dir_for_drive(&drive);
        std::fs::create_dir_all(&zona).expect("create zona");
        let mut app =
            App::bootstrap(outcome(true, base.join("installer")), test_host()).expect("bootstrap");
        app.handle(Event::DriveChosen {
            drive: drive_str.clone(),
        })
        .expect("occupied drive");
        match &app.view_model().phase {
            Phase::RelocationTargetExists { drive } => assert_eq!(drive, &drive_str),
            other => panic!("expected RelocationTargetExists, got {other:?}"),
        }

        cleanup(&base);
    }

    /// `delete: false` cancels (target kept), `delete: true` proceeds (target
    /// removed) and then hands off to the relocated copy.
    #[test]
    fn relocate_target_cancel_keeps_and_delete_proceeds() {
        let base = unique_temp_dir("reloc_delete");
        let drive = base.join("drive");
        let drive_str = drive.to_string_lossy().to_string();
        let zona = relocate::zona_dir_for_drive(&drive);
        std::fs::create_dir_all(&zona).expect("create zona");
        let marker = zona.join("marker.txt");
        std::fs::write(&marker, "keep").expect("write marker");

        let _guard = SPAWN_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        SPAWN_CALLS.store(0, Ordering::SeqCst);
        SPAWN_ARGS.lock().expect("spawn args lock").clear();
        let mut host = test_host();
        host.spawn_relocated_copy = record_spawn_ok;
        let mut app =
            App::bootstrap(outcome(true, base.join("installer")), host).expect("bootstrap");

        // Cancel: back to the drive picker, nothing deleted, nothing spawned.
        app.handle(Event::DriveChosen {
            drive: drive_str.clone(),
        })
        .expect("drive");
        assert!(matches!(
            app.view_model().phase,
            Phase::RelocationTargetExists { .. }
        ));
        app.handle(Event::RelocateTarget { delete: false })
            .expect("cancel");
        assert!(matches!(app.view_model().phase, Phase::Relocation { .. }));
        assert!(marker.exists(), "cancel must not delete the target");
        assert_eq!(
            SPAWN_CALLS.load(Ordering::SeqCst),
            0,
            "cancel must not spawn"
        );

        // Delete: the target goes away, relocation succeeds and hands off.
        app.handle(Event::DriveChosen {
            drive: drive_str.clone(),
        })
        .expect("drive again");
        assert!(matches!(
            app.view_model().phase,
            Phase::RelocationTargetExists { .. }
        ));
        app.handle(Event::RelocateTarget { delete: true })
            .expect("delete");
        assert!(!zona.exists(), "delete: true must remove the target");
        assert_eq!(SPAWN_CALLS.load(Ordering::SeqCst), 1);
        assert!(matches!(app.view_model().phase, Phase::Relocation { .. }));
        assert!(
            app.should_close(),
            "the hand-off asks the old process to close"
        );

        cleanup(&base);
    }

    /// A successful relocation spawns the relocated copy exactly once, with the
    /// `new_exe`/`old_dir` the outcome carries, sets the install root to the new
    /// location, keeps the phase at `Relocation` and asks to close.
    #[test]
    fn relocate_hand_off_spawns_once_with_the_new_exe_and_old_dir() {
        let _guard = SPAWN_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        SPAWN_CALLS.store(0, Ordering::SeqCst);
        SPAWN_ARGS.lock().expect("spawn args lock").clear();

        let base = unique_temp_dir("hand_off_spawn");
        let drive = base.join("drive");
        let drive_str = drive.to_string_lossy().to_string();
        let exe_dir = base.join("installer");

        let mut host = test_host();
        host.spawn_relocated_copy = record_spawn_ok;
        let mut app = App::bootstrap(outcome(true, exe_dir.clone()), host).expect("bootstrap");

        app.handle(Event::DriveChosen {
            drive: drive_str.clone(),
        })
        .expect("clean drive");

        let expected_root = relocate::install_root_for_drive(&drive);
        let expected_new_exe = expected_root.join("zona-installer.exe");
        assert_eq!(
            SPAWN_CALLS.load(Ordering::SeqCst),
            1,
            "spawned exactly once"
        );
        assert_eq!(
            SPAWN_ARGS.lock().expect("spawn args lock").clone(),
            vec![(expected_new_exe, exe_dir)],
            "the seam receives the outcome's new_exe/old_dir"
        );
        assert_eq!(
            app.install_root, expected_root,
            "the install root moves to the new location"
        );
        assert!(
            matches!(app.view_model().phase, Phase::Relocation { .. }),
            "the hand-off keeps the old process on the drive picker"
        );
        assert!(app.should_close());

        cleanup(&base);
    }

    /// Once the hand-off has been decided, a second `DriveChosen` arriving in the
    /// frames before the viewport close takes effect must be a no-op. Without the
    /// guard, the old process would re-enter relocation against the already-moved
    /// root.
    #[test]
    fn relocate_hand_off_ignores_a_second_drive_chosen() {
        let _guard = SPAWN_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        RELOCATE_CALLS.store(0, Ordering::SeqCst);
        SPAWN_CALLS.store(0, Ordering::SeqCst);
        SPAWN_ARGS.lock().expect("spawn args lock").clear();

        let base = unique_temp_dir("hand_off_reentry");
        let drive = base.join("drive");
        let drive_str = drive.to_string_lossy().to_string();
        let exe_dir = base.join("installer");

        let mut host = test_host();
        host.relocate = record_relocate_creating_target;
        host.spawn_relocated_copy = record_spawn_ok;
        let mut app = App::bootstrap(outcome(true, exe_dir.clone()), host).expect("bootstrap");

        // First pick: relocates, spawns the child once, asks to close.
        app.handle(Event::DriveChosen {
            drive: drive_str.clone(),
        })
        .expect("clean drive");
        assert_eq!(RELOCATE_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(SPAWN_CALLS.load(Ordering::SeqCst), 1);
        assert!(app.should_close(), "a successful hand-off asks to close");

        // Second pick on the SAME drive, before the close lands. The hand-off is
        // already decided, so this must not reach the relocate seam again.
        app.handle(Event::DriveChosen {
            drive: drive_str.clone(),
        })
        .expect("second drive pick is ignored");

        assert_eq!(
            RELOCATE_CALLS.load(Ordering::SeqCst),
            1,
            "a second DriveChosen after the hand-off must not re-run relocation"
        );
        assert_eq!(
            SPAWN_CALLS.load(Ordering::SeqCst),
            1,
            "a second DriveChosen after the hand-off must not spawn again"
        );
        assert!(
            !matches!(app.view_model().phase, Phase::RelocationTargetExists { .. }),
            "the delete-or-cancel prompt must never appear after a hand-off"
        );
        assert!(
            app.should_close(),
            "the hand-off close request survives the ignored event"
        );

        cleanup(&base);
    }

    /// A `Disabled` spawn (dev override) keeps the OLD process on the normal
    /// flow and does not ask to close.
    #[test]
    fn relocate_hand_off_disabled_falls_back_to_choices() {
        let base = unique_temp_dir("hand_off_disabled");
        let drive = base.join("drive");
        let drive_str = drive.to_string_lossy().to_string();

        // `test_host()`'s hand-off seam is `spawn_disabled`.
        let mut app =
            App::bootstrap(outcome(true, base.join("installer")), test_host()).expect("bootstrap");
        app.handle(Event::DriveChosen {
            drive: drive_str.clone(),
        })
        .expect("clean drive");

        assert_eq!(app.view_model().phase, Phase::Choices);
        assert!(!app.should_close(), "a Disabled hand-off keeps the process");

        cleanup(&base);
    }

    /// A failed spawn is fatal and `should_close()` stays false (no other
    /// process exists to carry the run on).
    #[test]
    fn relocate_hand_off_spawn_failure_is_fatal_and_does_not_close() {
        let base = unique_temp_dir("hand_off_fail");
        let drive = base.join("drive");
        let drive_str = drive.to_string_lossy().to_string();

        let mut host = test_host();
        host.spawn_relocated_copy = spawn_fails;
        let mut app =
            App::bootstrap(outcome(true, base.join("installer")), host).expect("bootstrap");

        let error = app
            .handle(Event::DriveChosen {
                drive: drive_str.clone(),
            })
            .expect_err("a failed hand-off spawn must surface");

        assert_eq!(error.error_id().as_str(), "E_IO");
        assert!(
            matches!(app.view_model().phase, Phase::Fatal(_)),
            "a failed spawn is fatal"
        );
        assert!(!app.should_close(), "a failed spawn must NOT close");

        cleanup(&base);
    }

    // ---- launcher ---------------------------------------------------------

    /// A canonical start with `--launcher` (no relocation needed) opens the
    /// launcher window; without a `mo2` install the game is reported absent.
    #[test]
    fn launcher_opens_without_the_game_present() {
        let root = unique_temp_dir("launcher_absent");
        let app =
            App::bootstrap(launcher_outcome(false, root.clone()), test_host()).expect("bootstrap");
        assert_eq!(
            app.view_model().phase,
            Phase::Launcher { game_present: false }
        );
        cleanup(&root);
    }

    /// When `..\mo2\ModOrganizer.exe` exists the launcher reports the game
    /// present.
    #[test]
    fn launcher_reports_the_game_present() {
        let base = unique_temp_dir("launcher_present");
        let install_root = base.join("Zona").join("installer");
        let mo2 = base.join("Zona").join("mo2");
        std::fs::create_dir_all(&mo2).expect("create mo2");
        std::fs::write(mo2.join("ModOrganizer.exe"), b"stub").expect("write exe");

        let app = App::bootstrap(launcher_outcome(false, install_root), test_host())
            .expect("bootstrap");
        assert_eq!(
            app.view_model().phase,
            Phase::Launcher { game_present: true }
        );
        cleanup(&base);
    }

    /// `LaunchInstaller` proceeds into the normal installer interface.
    #[test]
    fn launcher_launch_installer_enters_choices() {
        let root = unique_temp_dir("launcher_installer");
        let mut app =
            App::bootstrap(launcher_outcome(false, root.clone()), test_host()).expect("bootstrap");
        app.handle(Event::LaunchInstaller).expect("launch installer");
        assert_eq!(app.view_model().phase, Phase::Choices);
        cleanup(&root);
    }

    /// `LaunchMo2` spawns MO2 without the profile argument and asks to close.
    #[test]
    fn launcher_launch_mo2_spawns_and_closes() {
        let _guard = LAUNCH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        LAUNCH_CALLS.store(0, Ordering::SeqCst);
        LAUNCH_ARGS.lock().expect("launch args lock").clear();

        let base = unique_temp_dir("launcher_mo2");
        let install_root = base.join("Zona").join("installer");
        let mo2 = base.join("Zona").join("mo2");
        std::fs::create_dir_all(&mo2).expect("create mo2");
        std::fs::write(mo2.join("ModOrganizer.exe"), b"stub").expect("write exe");

        let mut host = test_host();
        host.launch_mo2 = record_launch;
        let mut app =
            App::bootstrap(launcher_outcome(false, install_root), host).expect("bootstrap");

        app.handle(Event::LaunchMo2).expect("launch mo2");
        assert_eq!(LAUNCH_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(
            LAUNCH_ARGS.lock().expect("launch args lock").clone(),
            vec![(mo2.join("ModOrganizer.exe"), mo2.clone(), false)],
            "LaunchMo2 spawns MO2 without the profile argument"
        );
        assert!(app.should_close(), "launching MO2 closes the installer");

        // The close guard stops further input: a follow-up launch is ignored.
        app.handle(Event::LaunchZona).expect("guarded");
        assert_eq!(LAUNCH_CALLS.load(Ordering::SeqCst), 1);
        cleanup(&base);
    }

    /// `LaunchZona` spawns MO2 with the Anomaly profile argument.
    #[test]
    fn launcher_launch_zona_passes_the_profile_argument() {
        let _guard = LAUNCH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        LAUNCH_CALLS.store(0, Ordering::SeqCst);
        LAUNCH_ARGS.lock().expect("launch args lock").clear();

        let base = unique_temp_dir("launcher_zona");
        let install_root = base.join("Zona").join("installer");
        let mo2 = base.join("Zona").join("mo2");
        std::fs::create_dir_all(&mo2).expect("create mo2");
        std::fs::write(mo2.join("ModOrganizer.exe"), b"stub").expect("write exe");

        let mut host = test_host();
        host.launch_mo2 = record_launch;
        let mut app =
            App::bootstrap(launcher_outcome(false, install_root), host).expect("bootstrap");

        app.handle(Event::LaunchZona).expect("launch zona");
        assert_eq!(
            LAUNCH_ARGS.lock().expect("launch args lock").clone(),
            vec![(mo2.join("ModOrganizer.exe"), mo2.clone(), true)],
            "LaunchZona spawns MO2 WITH the profile argument"
        );
        assert!(app.should_close());
        cleanup(&base);
    }

    /// When the game is absent, the two launch options are no-ops (the UI greys
    /// them out; the core enforces it too).
    #[test]
    fn launcher_launch_is_a_noop_when_the_game_is_absent() {
        let _guard = LAUNCH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        LAUNCH_CALLS.store(0, Ordering::SeqCst);

        let root = unique_temp_dir("launcher_noop");
        let mut host = test_host();
        host.launch_mo2 = record_launch;
        let mut app =
            App::bootstrap(launcher_outcome(false, root.clone()), host).expect("bootstrap");

        app.handle(Event::LaunchMo2).expect("guarded noop");
        app.handle(Event::LaunchZona).expect("guarded noop");
        assert_eq!(LAUNCH_CALLS.load(Ordering::SeqCst), 0);
        assert!(!app.should_close(), "a no-op launch must not close");
        cleanup(&root);
    }

    /// A failed MO2 launch is fatal and does not close.
    #[test]
    fn launcher_launch_failure_is_fatal() {
        let base = unique_temp_dir("launcher_fail");
        let install_root = base.join("Zona").join("installer");
        let mo2 = base.join("Zona").join("mo2");
        std::fs::create_dir_all(&mo2).expect("create mo2");
        std::fs::write(mo2.join("ModOrganizer.exe"), b"stub").expect("write exe");

        let mut host = test_host();
        host.launch_mo2 = fail_launch;
        let mut app =
            App::bootstrap(launcher_outcome(false, install_root), host).expect("bootstrap");

        let error = app.handle(Event::LaunchMo2).expect_err("a failed launch must surface");
        assert_eq!(error.error_id().as_str(), "E_TEST_LAUNCH");
        assert!(matches!(app.view_model().phase, Phase::Fatal(_)));
        assert!(!app.should_close(), "a failed launch must not close");
        cleanup(&base);
    }

    /// A `--launcher` start that still needs relocation forwards the flag to the
    /// relocated copy through the hand-off.
    #[test]
    fn relocate_hand_off_forwards_the_launcher_flag() {
        let _guard = SPAWN_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        SPAWN_CALLS.store(0, Ordering::SeqCst);
        SPAWN_ARGS.lock().expect("spawn args lock").clear();
        SPAWN_LAUNCHER.lock().expect("spawn launcher lock").clear();

        let base = unique_temp_dir("hand_off_launcher");
        let drive = base.join("drive");
        let drive_str = drive.to_string_lossy().to_string();

        let mut host = test_host();
        host.spawn_relocated_copy = record_spawn_ok;
        let mut app =
            App::bootstrap(launcher_outcome(true, base.join("installer")), host)
                .expect("bootstrap");

        app.handle(Event::DriveChosen {
            drive: drive_str.clone(),
        })
        .expect("clean drive");

        assert_eq!(SPAWN_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(
            SPAWN_LAUNCHER.lock().expect("spawn launcher lock").clone(),
            vec![true],
            "the hand-off forwards --launcher to the relocated copy"
        );
        assert!(app.should_close());
        cleanup(&base);
    }

    /// `RestartInstall` is recognised, routed to the restart seam only when
    /// `FatalState.restartable`, and `restartable` is surfaced.
    #[test]
    fn restart_install_is_dispatched_only_when_restartable() {
        RESTART_CALLS.store(0, Ordering::SeqCst);
        let root = unique_temp_dir("restart");

        // A download-stage failure: fatal but NOT restartable.
        let mut host = test_host();
        host.start_download = fail_download;
        let mut app = App::bootstrap(outcome(false, root.clone()), host).expect("bootstrap");
        pick_complete_path(&mut app);
        app.handle(Event::ConfirmChoices)
            .expect_err("download failure");
        assert!(matches!(
            &app.view_model().phase,
            Phase::Fatal(state) if !state.restartable
        ));
        app.handle(Event::RestartInstall).expect("recognised");
        assert_eq!(
            RESTART_CALLS.load(Ordering::SeqCst),
            0,
            "not restartable: the restart seam must not be dispatched"
        );

        // An install-stage failure: restartable, so it is dispatched.
        let mut host = test_host();
        host.start_install = fail_install;
        let mut app = App::bootstrap(outcome(false, root.clone()), host).expect("bootstrap");
        pick_complete_path(&mut app);
        app.handle(Event::ConfirmChoices).expect("confirm");
        app.handle(Event::BeginInstall)
            .expect_err("install failure");
        match &app.view_model().phase {
            Phase::Fatal(state) => {
                assert_eq!(state.error_id, "E_TEST_INSTALL");
                assert!(state.restartable, "install failure must be restartable");
            }
            other => panic!("expected Phase::Fatal, got {other:?}"),
        }
        app.handle(Event::RestartInstall).expect("recognised");
        assert_eq!(
            RESTART_CALLS.load(Ordering::SeqCst),
            1,
            "restartable fatal must dispatch to the restart seam"
        );

        cleanup(&root);
    }

    /// The cascading pick-to-set logic and the mutual exclusivity of the
    /// top-level branches.
    #[test]
    fn choice_toggles_cascade_and_branches_are_exclusive() {
        let root = unique_temp_dir("choices");
        let mut app = App::bootstrap(outcome(false, root.clone()), test_host()).expect("bootstrap");

        app.handle(Event::ChoiceToggled {
            id: "install_zona".to_string(),
            on: true,
        })
        .expect("branch");
        app.handle(Event::ChoiceToggled {
            id: "sss23".to_string(),
            on: true,
        })
        .expect("level");
        app.handle(Event::ChoiceToggled {
            id: "bhs_hud".to_string(),
            on: true,
        })
        .expect("leaf");
        assert_eq!(
            app.view_model().selected,
            set(&["install_zona", "sss23", "bhs_hud"])
        );

        // Picking a different branch drops the first one entirely.
        app.handle(Event::ChoiceToggled {
            id: "update_zona".to_string(),
            on: true,
        })
        .expect("other branch");
        assert_eq!(app.view_model().selected, set(&["update_zona"]));

        // Deselecting a node removes it and its descendants.
        app.handle(Event::ChoiceToggled {
            id: "install_zona".to_string(),
            on: true,
        })
        .expect("branch again");
        app.handle(Event::ChoiceToggled {
            id: "sss23".to_string(),
            on: true,
        })
        .expect("level again");
        assert_eq!(app.view_model().selected, set(&["install_zona", "sss23"]));
        app.handle(Event::ChoiceToggled {
            id: "sss23".to_string(),
            on: false,
        })
        .expect("deselect");
        assert_eq!(app.view_model().selected, set(&["install_zona"]));

        cleanup(&root);
    }

    /// The real `config.yaml` repeats `sss21`/`sss23`/`bhs_hud`/`clean_hud`
    /// under all three top-level branches, so a toggle must resolve the id
    /// inside the currently selected context rather than by a context-blind
    /// depth-first first match (which would silently switch branches).
    #[test]
    fn repeated_ids_resolve_inside_the_selected_branch() {
        let root = unique_temp_dir("repeated");
        let mut host = test_host();
        host.load_config = repeated_config;
        let mut app = App::bootstrap(outcome(false, root.clone()), host).expect("bootstrap");

        // (e) With nothing selected, a bare repeated id establishes the first
        // branch in config order: install_zona.
        app.handle(toggle("sss23", true)).expect("establish branch");
        assert_eq!(app.view_model().selected, set(&["install_zona", "sss23"]));

        // (a) Select update_zona, then toggle the repeated sss23: it stays
        // inside update_zona instead of jumping to install_zona.
        app.handle(toggle("update_zona", true)).expect("branch");
        assert_eq!(app.view_model().selected, set(&["update_zona"]));
        app.handle(toggle("sss23", true)).expect("sss23");
        assert_eq!(app.view_model().selected, set(&["update_zona", "sss23"]));

        // (b) From [install_zona, sss23], bhs_hud resolves under sss23 (the
        // selected child), not the sss21 variant.
        app.handle(toggle("install_zona", true)).expect("branch");
        app.handle(toggle("sss23", true)).expect("sss23");
        assert_eq!(app.view_model().selected, set(&["install_zona", "sss23"]));
        app.handle(toggle("bhs_hud", true)).expect("bhs_hud");
        assert_eq!(
            app.view_model().selected,
            set(&["install_zona", "sss23", "bhs_hud"])
        );

        // (c) From [install_zona, sss21], bhs_hud resolves under sss21.
        app.handle(toggle("sss21", true)).expect("sss21");
        assert_eq!(app.view_model().selected, set(&["install_zona", "sss21"]));
        app.handle(toggle("bhs_hud", true)).expect("bhs_hud");
        assert_eq!(
            app.view_model().selected,
            set(&["install_zona", "sss21", "bhs_hud"])
        );

        // (d) Toggling another top-level branch drops the previous one.
        app.handle(toggle("update_graphics", true)).expect("branch");
        assert_eq!(app.view_model().selected, set(&["update_graphics"]));

        cleanup(&root);
    }

    /// Toggling a parent off removes its whole subtree.
    #[test]
    fn deselecting_a_parent_removes_its_subtree() {
        let root = unique_temp_dir("subtree");
        let mut host = test_host();
        host.load_config = repeated_config;
        let mut app = App::bootstrap(outcome(false, root.clone()), host).expect("bootstrap");

        app.handle(toggle("install_zona", true)).expect("branch");
        app.handle(toggle("sss23", true)).expect("sss23");
        app.handle(toggle("bhs_hud", true)).expect("bhs_hud");
        assert_eq!(
            app.view_model().selected,
            set(&["install_zona", "sss23", "bhs_hud"])
        );

        app.handle(toggle("sss23", false)).expect("deselect parent");
        assert_eq!(app.view_model().selected, set(&["install_zona"]));

        cleanup(&root);
    }

    /// Confirm is gated on a COMPLETE root->leaf path. An incomplete selection
    /// keeps `can_confirm == false` and `ConfirmChoices` a no-op; only a full
    /// root->leaf path unlocks it and proceeds to `Downloading`.
    #[test]
    fn confirm_requires_a_complete_root_to_leaf_path() {
        let root = unique_temp_dir("confirm_gate");
        let mut app = App::bootstrap(outcome(false, root.clone()), test_host()).expect("bootstrap");

        // (a) Empty selection: not a complete path, Confirm is a no-op.
        assert!(
            !app.view_model().can_confirm,
            "empty selection is incomplete"
        );
        app.handle(Event::ConfirmChoices).expect("no-op confirm");
        assert_eq!(app.view_model().phase, Phase::Choices);

        // (b) A lone top-level branch has children: not confirmable.
        app.handle(toggle("install_zona", true)).expect("branch");
        assert_eq!(app.view_model().selected, set(&["install_zona"]));
        assert!(!app.view_model().can_confirm);
        app.handle(Event::ConfirmChoices).expect("no-op confirm");
        assert_eq!(app.view_model().phase, Phase::Choices);

        // (c) A path stopping at `sss23` (which has children) is not confirmable.
        app.handle(toggle("sss23", true)).expect("sss23");
        assert_eq!(app.view_model().selected, set(&["install_zona", "sss23"]));
        assert!(!app.view_model().can_confirm);
        app.handle(Event::ConfirmChoices).expect("no-op confirm");
        assert_eq!(app.view_model().phase, Phase::Choices);

        // (d) Reaching a leaf completes the path.
        app.handle(toggle("bhs_hud", true)).expect("leaf");
        assert_eq!(
            app.view_model().selected,
            set(&["install_zona", "sss23", "bhs_hud"])
        );
        assert!(app.view_model().can_confirm);

        // (e) Dropping back to an incomplete selection re-locks Confirm.
        app.handle(toggle("bhs_hud", false)).expect("unpick leaf");
        assert_eq!(app.view_model().selected, set(&["install_zona", "sss23"]));
        assert!(!app.view_model().can_confirm, "sss23 still has children");

        // (f) A complete path again proceeds to the download stage.
        app.handle(toggle("bhs_hud", true)).expect("leaf again");
        assert!(app.view_model().can_confirm);
        app.handle(Event::ConfirmChoices).expect("confirm");
        assert_eq!(app.view_model().phase, Phase::Downloading);

        cleanup(&root);
    }

    /// The completeness walk is anchored at the selected parent even when ids
    /// repeat under every branch: `update_zona -> sss23 -> bhs_hud` is complete,
    /// but `sss23` has children so shorter paths are not.
    #[test]
    fn complete_path_walk_handles_repeated_ids() {
        let nodes = repeated_tree();
        assert!(is_complete_choice_path(
            &nodes,
            &set(&["update_zona", "sss23", "bhs_hud"])
        ));
        assert!(!is_complete_choice_path(
            &nodes,
            &set(&["update_zona", "sss23"])
        ));
        assert!(!is_complete_choice_path(&nodes, &set(&["update_zona"])));
        assert!(!is_complete_choice_path(&nodes, &HashSet::new()));
    }

    #[test]
    fn no_fixed_drives_is_a_fatal_error() {
        fn no_drives() -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        let mut host = test_host();
        host.drives = no_drives;
        let root = unique_temp_dir("nodrives");
        let error = App::bootstrap(outcome(true, root.clone()), host)
            .err()
            .expect("no drives must fail");
        assert_eq!(error.error_id().as_str(), "E_NO_DRIVES");
        cleanup(&root);
    }

    /// The fatal popup falls back to the active run log when the error carries
    /// none, while a `Tool` error's own log still wins.
    #[test]
    fn fatal_state_falls_back_to_the_run_log_but_the_error_log_wins() {
        let run_log = PathBuf::from(r"C:\Zona\installer\logs\log_2026-10-09_13-05-07.log");

        // An Io error carries no log path, so the run log is used.
        let io = AppError::Io {
            context: "read".to_string(),
            path: None,
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "nope"),
        };
        let state = FatalState::from_error(&io, false, Some(&run_log));
        assert_eq!(
            state.log_path,
            Some(run_log.clone()),
            "an error with no log path falls back to the run log"
        );

        // No run log and no error log -> still none.
        let state = FatalState::from_error(&io, false, None);
        assert_eq!(state.log_path, None);

        // A Tool error's own log takes precedence over the run log.
        let tool_log = PathBuf::from(r"C:\Zona\installer\logs\aria2c.log");
        let tool = AppError::Tool {
            tool: "aria2c.exe",
            context: "download assets".to_string(),
            exit_code: Some(7),
            log_path: Some(tool_log.clone()),
        };
        let state = FatalState::from_error(&tool, false, Some(&run_log));
        assert_eq!(
            state.log_path,
            Some(tool_log),
            "the tool's own log must win over the run log"
        );
    }

    // ---- taskbar flash wiring --------------------------------------------

    /// A download stage that fails, driving the ASYNC worker-error path into
    /// `Phase::Fatal` so the fatal flash can be asserted for that entry path too
    /// (the synchronous path is exercised via `fail_download`).
    fn failing_download_stage(
        _root: &Path,
        _assets: &[&Asset],
        _on_progress: &mut dyn FnMut(crate::pipeline::download::DownloadUpdate),
    ) -> Result<()> {
        Err(AppError::Fatal {
            message: "worker boom".to_string(),
            error_id: ErrorId::new("E_TEST_WORKER"),
            log_path: None,
        })
    }

    /// Brings an app with a recording flash seam to `Phase::Downloading` and
    /// attaches a fresh progress channel. Returns the app and the sender.
    fn app_downloading_with_recording_flash(root: &Path) -> (App, mpsc::Sender<ProgressEvent>) {
        let mut host = test_host();
        host.flash_taskbar = record_flash;
        let mut app = App::bootstrap(outcome(false, root.to_path_buf()), host).expect("bootstrap");
        pick_complete_path(&mut app);
        app.handle(Event::ConfirmChoices).expect("confirm");
        let (tx, rx) = mpsc::channel();
        app.attach_progress_receiver(rx);
        (app, tx)
    }

    /// The download -> install transition (`DownloadsComplete` ->
    /// `Phase::Installing`) flashes exactly once, and a repeated
    /// `DownloadsComplete` (which does not change the phase) does not flash.
    #[test]
    fn downloads_complete_flashes_once_and_a_repeat_does_not() {
        let _guard = FLASH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        FLASH_CALLS.store(0, Ordering::SeqCst);

        let root = unique_temp_dir("flash_downloads_complete");
        let (mut app, tx) = app_downloading_with_recording_flash(&root);
        assert_eq!(app.view_model().phase, Phase::Downloading);
        assert_eq!(
            FLASH_CALLS.load(Ordering::SeqCst),
            0,
            "starting the download does not flash"
        );

        tx.send(ProgressEvent::DownloadsComplete).expect("send");
        app.drain_progress();
        assert_eq!(app.view_model().phase, Phase::Installing);
        assert_eq!(
            FLASH_CALLS.load(Ordering::SeqCst),
            1,
            "the download -> install transition flashes once"
        );

        tx.send(ProgressEvent::DownloadsComplete).expect("repeat");
        app.drain_progress();
        assert_eq!(app.view_model().phase, Phase::Installing);
        assert_eq!(
            FLASH_CALLS.load(Ordering::SeqCst),
            1,
            "a repeated DownloadsComplete must not flash"
        );

        cleanup(&root);
    }

    /// Entering the verify gate flashes exactly once, and the game finishing
    /// flashes again (Continue now needs the user).
    #[test]
    fn verify_gate_flashes_on_ready_and_finish() {
        let _guard = FLASH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        FLASH_CALLS.store(0, Ordering::SeqCst);

        let root = unique_temp_dir("flash_verify");
        let (mut app, tx) = app_downloading_with_recording_flash(&root);
        app.handle(Event::BeginInstall).expect("begin");
        assert_eq!(app.view_model().phase, Phase::Installing);
        assert_eq!(
            FLASH_CALLS.load(Ordering::SeqCst),
            0,
            "starting the install does not flash"
        );

        // Ready: enter the gate -> flash once.
        tx.send(ProgressEvent::VerifyGameReady {
            exe: "anomaly.exe".to_string(),
        })
        .expect("send");
        app.drain_progress();
        assert_eq!(app.view_model().phase, Phase::VerifyGame);
        assert_eq!(
            FLASH_CALLS.load(Ordering::SeqCst),
            1,
            "entering VerifyGame flashes once"
        );

        // A repeated Ready while already gated does not change the phase.
        tx.send(ProgressEvent::VerifyGameReady {
            exe: "anomaly.exe".to_string(),
        })
        .expect("repeat");
        app.drain_progress();
        assert_eq!(app.view_model().phase, Phase::VerifyGame);
        assert_eq!(
            FLASH_CALLS.load(Ordering::SeqCst),
            1,
            "a repeated gate must not flash"
        );

        // Start game, then the game finishes -> flash (Continue needs the user).
        app.handle(Event::StartGame).expect("start");
        tx.send(ProgressEvent::VerifyGameFinished)
            .expect("finished");
        app.drain_progress();
        assert_eq!(
            app.view_model().verify_game.as_ref().map(|view| view.stage),
            Some(VerifyGameStage::Finished)
        );
        assert_eq!(
            FLASH_CALLS.load(Ordering::SeqCst),
            2,
            "the finished gate flashes"
        );

        // Continue resumes without flashing; a later fresh gate flashes again.
        app.handle(Event::Continue).expect("continue");
        assert_eq!(app.view_model().phase, Phase::Installing);
        assert_eq!(
            FLASH_CALLS.load(Ordering::SeqCst),
            2,
            "resuming does not flash"
        );
        tx.send(ProgressEvent::VerifyGameReady {
            exe: "anomaly.exe".to_string(),
        })
        .expect("re-gate");
        app.drain_progress();
        assert_eq!(app.view_model().phase, Phase::VerifyGame);
        assert_eq!(
            FLASH_CALLS.load(Ordering::SeqCst),
            3,
            "a fresh gate entry flashes again"
        );

        cleanup(&root);
    }

    /// Entering `Phase::Finished` flashes exactly once; a repeated final
    /// `StepDone` does not flash again.
    #[test]
    fn finish_flashes_once_on_finished() {
        let _guard = FLASH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        FLASH_CALLS.store(0, Ordering::SeqCst);

        let root = unique_temp_dir("flash_finished");
        let (mut app, tx) = app_downloading_with_recording_flash(&root);
        app.handle(Event::BeginInstall).expect("begin");
        assert_eq!(app.view_model().phase, Phase::Installing);

        // The last of the three applicable steps finishing completes the install.
        tx.send(ProgressEvent::StepDone {
            index: 2,
            name: "install_sss23".to_string(),
        })
        .expect("send done");
        app.drain_progress();
        assert_eq!(app.view_model().phase, Phase::Finished);
        assert_eq!(
            FLASH_CALLS.load(Ordering::SeqCst),
            1,
            "entering Finished flashes once"
        );

        // A repeated final StepDone does not change the phase.
        tx.send(ProgressEvent::StepDone {
            index: 2,
            name: "install_sss23".to_string(),
        })
        .expect("repeat");
        app.drain_progress();
        assert_eq!(app.view_model().phase, Phase::Finished);
        assert_eq!(
            FLASH_CALLS.load(Ordering::SeqCst),
            1,
            "a repeated final step must not flash"
        );

        cleanup(&root);
    }

    /// Ordinary download-progress ticks while `Downloading` (and entering
    /// `Downloading` itself) do not flash — the notification is
    /// transition-triggered, not per-frame.
    #[test]
    fn download_progress_ticks_do_not_flash() {
        let _guard = FLASH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        FLASH_CALLS.store(0, Ordering::SeqCst);

        let root = unique_temp_dir("flash_ticks");
        let (mut app, tx) = app_downloading_with_recording_flash(&root);
        assert_eq!(
            FLASH_CALLS.load(Ordering::SeqCst),
            0,
            "entering Downloading does not flash"
        );

        for pct in [1u32, 42, 99] {
            tx.send(ProgressEvent::DownloadProgress {
                file: "zona.7z".to_string(),
                pct: Some(pct),
                done: "1.0MiB".to_string(),
                total: "2.4MiB".to_string(),
            })
            .expect("send tick");
            app.drain_progress();
        }
        assert_eq!(
            FLASH_CALLS.load(Ordering::SeqCst),
            0,
            "ordinary ticks while Downloading must not flash"
        );
        assert_eq!(app.view_model().phase, Phase::Downloading);

        cleanup(&root);
    }

    /// The synchronous fatal path (`App::handle` returning `Err`) flashes
    /// exactly once.
    #[test]
    fn handle_error_flashes_once_on_fatal() {
        let _guard = FLASH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        FLASH_CALLS.store(0, Ordering::SeqCst);

        let root = unique_temp_dir("flash_handle_fatal");
        let mut host = test_host();
        host.flash_taskbar = record_flash;
        host.start_download = fail_download;
        let mut app = App::bootstrap(outcome(false, root.clone()), host).expect("bootstrap");
        pick_complete_path(&mut app);

        app.handle(Event::ConfirmChoices)
            .expect_err("the injected error must surface");
        assert!(matches!(app.view_model().phase, Phase::Fatal(_)));
        assert_eq!(
            FLASH_CALLS.load(Ordering::SeqCst),
            1,
            "a synchronous fatal flashes once"
        );

        cleanup(&root);
    }

    /// The asynchronous fatal path (a background worker's error surfaced by
    /// `App::drain_progress`) also flashes exactly once.
    #[test]
    fn worker_error_flashes_once_on_fatal() {
        let _guard = FLASH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        FLASH_CALLS.store(0, Ordering::SeqCst);

        let root = unique_temp_dir("flash_worker_fatal");
        let mut host = test_host();
        host.flash_taskbar = record_flash;
        let mut app = App::bootstrap(outcome(false, root.clone()), host).expect("bootstrap");
        pick_complete_path(&mut app);

        run::start_download_with(&mut app, failing_download_stage).expect("spawn the worker");
        app.worker.as_mut().expect("a worker was spawned").wait();
        app.drain_progress();

        match &app.view_model().phase {
            Phase::Fatal(state) => assert_eq!(state.error_id, "E_TEST_WORKER"),
            other => panic!("expected Phase::Fatal, got {other:?}"),
        }
        assert_eq!(
            FLASH_CALLS.load(Ordering::SeqCst),
            1,
            "an asynchronous fatal flashes once"
        );

        cleanup(&root);
    }

    /// A failed flash is best-effort — it does not turn the state into
    /// `Phase::Fatal`, does not stop the transition, and leaves the install
    /// runnable.
    #[test]
    fn a_failed_flash_is_best_effort_and_never_fatal() {
        let _guard = FLASH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        FLASH_CALLS.store(0, Ordering::SeqCst);

        let root = unique_temp_dir("flash_failure");
        let mut host = test_host();
        host.flash_taskbar = fail_flash;
        let mut app = App::bootstrap(outcome(false, root.clone()), host).expect("bootstrap");
        pick_complete_path(&mut app);
        app.handle(Event::ConfirmChoices).expect("confirm");
        let (tx, rx) = mpsc::channel();
        app.attach_progress_receiver(rx);

        tx.send(ProgressEvent::DownloadsComplete).expect("send");
        app.drain_progress();

        assert_eq!(
            FLASH_CALLS.load(Ordering::SeqCst),
            1,
            "the failing seam was still attempted"
        );
        assert_eq!(
            app.view_model().phase,
            Phase::Installing,
            "the transition must still happen when the flash fails"
        );
        assert!(
            !matches!(app.view_model().phase, Phase::Fatal(_)),
            "a failed flash must not be fatal"
        );

        // The install still runs: BeginInstall dispatches without an error.
        app.handle(Event::BeginInstall)
            .expect("a failed flash must not block starting the install");
        assert_eq!(app.view_model().phase, Phase::Installing);

        cleanup(&root);
    }
}
