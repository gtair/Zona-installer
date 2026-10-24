//! Background-thread plumbing for the two long operations.
//!
//! The download stage and the install pipeline run OFF the UI thread and feed
//! progress back to [`App`] over an `mpsc` channel. [`start_download`] /
//! [`start_install`] are the production [`Host`] dispatch seams; each spawns one
//! worker thread. A *wake signal* ([`Wake`]) is invoked after every event so the
//! GUI can repaint promptly.
//!
//! A worker that finishes with an `Err` must not fail silently; [`spawn_worker`]
//! stores the terminal [`Result`], and [`App::drain_progress`] picks it up and
//! records [`Phase::Fatal`](crate::app::Phase::Fatal).

use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::config::{Asset, Step};
use crate::error::{AppError, Result};
use crate::gating;
use crate::pipeline::download::DownloadUpdate;
use crate::pipeline::{self, ProgressEvent, VerifyControl};
use crate::toolout::aria2::Aria2FileProgress;

use super::App;

/// The UI wake signal (plain data, no GUI type).
///
/// The GUI registers one via [`App::set_repaint`]; a worker calls it after every
/// progress event (and once when it finishes).
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// The download-stage signature injected into [`start_download_with`].
///
/// Production binds [`pipeline::download::download_all`]; tests inject a fake.
pub(crate) type DownloadStage = fn(&Path, &[&Asset], &mut dyn FnMut(DownloadUpdate)) -> Result<()>;

/// The terminal outcome of a worker, shared with the [`App`].
enum Terminal {
    /// Still running.
    Running,
    /// Finished, with the pipeline's result.
    Finished(Result<()>),
}

/// A handle to one spawned worker thread.
///
/// The thread detaches when the handle is dropped; keeping it lets the app reap
/// a finished worker and lets tests join deterministically.
pub(crate) struct Worker {
    handle: Option<JoinHandle<()>>,
    terminal: Arc<Mutex<Terminal>>,
}

impl Worker {
    /// Joins the thread if it has already finished, otherwise leaves it detached.
    fn reap(&mut self) {
        let finished = match &self.handle {
            Some(handle) => handle.is_finished(),
            None => true,
        };
        if finished {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    /// Blocks until the worker finishes.
    ///
    /// Test-only: production never blocks the UI thread on a worker.
    #[cfg(test)]
    pub(crate) fn wait(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Takes the worker's error, if it has finished and failed.
    ///
    /// Returns `None` while it is still running or if it succeeded. The stored
    /// error is removed so it is surfaced exactly once.
    pub(crate) fn take_error(&self) -> Option<AppError> {
        let mut slot = self.terminal.lock().ok()?;
        if !matches!(&*slot, Terminal::Finished(Err(_))) {
            return None;
        }
        match std::mem::replace(&mut *slot, Terminal::Finished(Ok(()))) {
            Terminal::Finished(Err(error)) => Some(error),
            _ => None,
        }
    }
}

/// Spawns `work` on a background thread, forwarding every [`ProgressEvent`] it
/// emits on `tx` and waking the UI after each one.
///
/// The thread's terminal [`Result`] is stored in the returned [`Worker`] so a
/// failure can be surfaced by [`App::drain_progress`] rather than dropped. A
/// failure to create the OS thread is an explicit [`AppError`], never a panic.
pub(crate) fn spawn_worker<W>(
    name: &str,
    work: W,
    tx: Sender<ProgressEvent>,
    wake: Option<Wake>,
) -> Result<Worker>
where
    W: FnOnce(&mut dyn FnMut(ProgressEvent)) -> Result<()> + Send + 'static,
{
    let terminal = Arc::new(Mutex::new(Terminal::Running));
    let slot = Arc::clone(&terminal);
    let worker_name = name.to_string();

    let handle = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let result = {
                let mut report = |event: ProgressEvent| {
                    // A dropped receiver means the app is gone; keep the work
                    // running to completion regardless.
                    let _ = tx.send(event);
                    if let Some(wake) = &wake {
                        wake();
                    }
                };
                // A panic must not leave the terminal slot at `Running` forever
                // (the UI would hang with no popup): convert it into a fatal
                // error.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&mut report)))
                    .unwrap_or_else(|payload| {
                        let detail = payload
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_string())
                            .or_else(|| payload.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic".to_string());
                        Err(AppError::Fatal {
                            message: format!(
                                "background worker '{worker_name}' panicked: {detail}"
                            ),
                            error_id: crate::error::ErrorId::new("E_WORKER_PANIC"),
                            log_path: None,
                        })
                    })
            };
            match &result {
                Ok(()) => {
                    log::info!(target: "run:spawn_worker", "worker '{worker_name}' finished: Ok")
                }
                Err(error) => log::info!(
                    target: "run:spawn_worker",
                    "worker '{worker_name}' finished: Err({})",
                    error.error_id(),
                ),
            }
            if let Ok(mut guard) = slot.lock() {
                *guard = Terminal::Finished(result);
            }
            // Wake once more so the UI notices completion (and any error).
            if let Some(wake) = &wake {
                wake();
            }
        })
        .map_err(|source| AppError::Io {
            context: format!("spawn the '{name}' background worker thread"),
            path: None,
            source,
        })?;

    Ok(Worker {
        handle: Some(handle),
        terminal,
    })
}

/// The shared worker start both stages use: create the channel, attach its
/// receiving half to `app`, and spawn one worker running `work`.
fn start_worker<W>(app: &mut App, name: &str, work: W) -> Result<()>
where
    W: FnOnce(&mut dyn FnMut(ProgressEvent)) -> Result<()> + Send + 'static,
{
    // Reap a previous, already-finished worker so no thread handle is retained.
    if let Some(previous) = app.worker.as_mut() {
        previous.reap();
    }
    let (tx, rx) = mpsc::channel();
    app.attach_progress_receiver(rx);
    let worker = spawn_worker(name, work, tx, app.wake.clone())?;
    log::info!(target: "run:start_worker", "worker '{name}' started");
    app.worker = Some(worker);
    Ok(())
}

/// Production [`Host`] dispatch for the download stage.
///
/// A thin wrapper over [`start_download_with`] binding the real
/// [`pipeline::download::download_all`] as the stage.
pub(crate) fn start_download(app: &mut App) -> Result<()> {
    start_download_with(app, pipeline::download::download_all)
}

/// Maps one streamed aria2c progress record to Screen 2's per-file
/// [`ProgressEvent::DownloadProgress`].
///
/// Keyed by the FILE path's file-name component, which is how it is matched
/// against the `DownloadRow`s. A record with no file (or a path with no
/// file-name component) yields no event.
pub(crate) fn download_progress(record: &Aria2FileProgress) -> Option<ProgressEvent> {
    let file = record.file.as_deref()?.file_name()?.to_str()?;
    Some(ProgressEvent::DownloadProgress {
        file: file.to_string(),
        pct: record.pct,
        done: record.done.clone(),
        total: record.total.clone(),
    })
}

/// Maps an aria2c progress record against known assets, redirecting magnet `dn` names
/// back to the asset's configured `file` name.
pub(crate) fn map_aria2_progress(
    record: &Aria2FileProgress,
    assets: &[Asset],
) -> Option<ProgressEvent> {
    let file_name = record.file.as_deref()?.file_name()?.to_str()?;
    for asset in assets {
        if asset.file == file_name {
            return Some(ProgressEvent::DownloadProgress {
                file: asset.file.clone(),
                pct: record.pct,
                done: record.done.clone(),
                total: record.total.clone(),
            });
        }
        if let Some(dn) = crate::pipeline::download::magnet_display_name(&asset.url) {
            if dn == file_name {
                return Some(ProgressEvent::DownloadProgress {
                    file: asset.file.clone(),
                    pct: record.pct,
                    done: record.done.clone(),
                    total: record.total.clone(),
                });
            }
        }
    }
    download_progress(record)
}

/// The injectable download-stage worker used by [`start_download`].
///
/// Runs `stage` (production: [`pipeline::download::download_all`]) on a
/// background thread so the UI stays responsive. Each per-file record `stage`
/// streams is mapped to a [`ProgressEvent::DownloadProgress`]; the worker then
/// emits [`ProgressEvent::DownloadsComplete`] exactly once after `stage` returns
/// `Ok` (a failed stage emits no completion event and surfaces `Phase::Fatal`).
pub(crate) fn start_download_with(app: &mut App, stage: DownloadStage) -> Result<()> {
    let install_root = app.install_root.clone();
    let assets: Vec<Asset> = match &app.config {
        Some(config) => gating::needed_assets(config, &app.selected)
            .into_iter()
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    start_worker(
        app,
        "download-worker",
        move |report: &mut dyn FnMut(ProgressEvent)| -> Result<()> {
            let refs: Vec<&Asset> = assets.iter().collect();
            let stage_result = stage(&install_root, &refs, &mut |update| match update {
                DownloadUpdate::Aria2(progress) => {
                    if let Some(event) = map_aria2_progress(&progress, &assets) {
                        report(event);
                    }
                }
                DownloadUpdate::FileProgress {
                    file,
                    pct,
                    done,
                    total,
                } => {
                    report(ProgressEvent::DownloadProgress {
                        file,
                        pct,
                        done,
                        total,
                    });
                }
                DownloadUpdate::FileStatus { file, status } => {
                    report(ProgressEvent::DownloadFileStatus { file, status });
                }
                DownloadUpdate::Status(message) => {
                    report(ProgressEvent::DownloadStatus { message });
                }
            });
            if let Err(error) = stage_result {
                // Log before propagating: a bare `?` would lose the reason.
                log::error!(target: "run:start_download_with", "download failed: {error}");
                return Err(error);
            }
            // Emit the transition signal only on success (a failed stage returned
            // above and emits nothing).
            report(ProgressEvent::DownloadsComplete);
            Ok(())
        },
    )
}

/// Production [`Host`] dispatch for the install pipeline.
///
/// Runs the real [`pipeline::run_steps`] on a background thread and forwards
/// every [`ProgressEvent`] it emits. `verify_game` blocks inside this worker,
/// leaving the egui event loop free.
pub(crate) fn start_install(app: &mut App) -> Result<()> {
    let install_root = app.install_root.clone();
    let steps: Vec<Step> = match &app.config {
        Some(config) => gating::needed_steps(config, &app.selected)
            .into_iter()
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    // The verify-game command channel: the app holds the sender (to send Start /
    // Continue from the UI) and the worker owns the receiver.
    let (verify_tx, verify) = VerifyControl::channel();
    app.verify_tx = Some(verify_tx);
    start_worker(
        app,
        "install-worker",
        move |report: &mut dyn FnMut(ProgressEvent)| -> Result<()> {
            let refs: Vec<&Step> = steps.iter().collect();
            if let Err(error) = pipeline::run_steps(&install_root, &refs, report, &verify) {
                // Log before propagating: a bare `?` would lose the reason.
                log::error!(target: "run:start_install", "install failed: {error}");
                return Err(error);
            }
            Ok(())
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Event, Host, Phase};
    use crate::config::{ChoiceNode, Config};
    use crate::error::ErrorId;
    use crate::paths::RootMode;
    use crate::preflight::{PreflightOutcome, SpawnOutcome};
    use crate::win32::relocate::RelocateOutcome;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Number of canned events the fake download producer emits.
    const CANNED: usize = 4;

    // ---- injectable host fns (no real fs / Win32 / child process) ------------

    fn one_drive() -> Result<Vec<String>> {
        Ok(vec![r"C:\".to_string()])
    }

    fn leaf(id: &str) -> ChoiceNode {
        ChoiceNode {
            id: id.to_string(),
            label: id.to_string(),
            description: String::new(),
            choices: Vec::new(),
        }
    }

    /// A minimal config whose choice tree is a single top-level LEAF, so a test
    /// can pick a complete root->leaf path and enable Confirm.
    fn leaf_config(_root: &Path) -> Result<Config> {
        Ok(Config {
            version: 1,
            current_version: None,
            github_repo: None,
            choices: vec![leaf("only")],
            assets: Vec::new(),
            steps: Vec::new(),
        })
    }

    fn never_relocate(_exe: &Path, _drive: &Path) -> Result<RelocateOutcome> {
        Ok(RelocateOutcome::Disabled)
    }

    fn noop_download(_app: &mut App) -> Result<()> {
        Ok(())
    }

    fn noop_install(_app: &mut App) -> Result<()> {
        Ok(())
    }

    fn noop_restart(_app: &mut App) -> Result<()> {
        Ok(())
    }

    fn noop_finish(_app: &App) -> Result<()> {
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

    /// A fake download stage that succeeds without touching the network or the
    /// filesystem (matches `pipeline::download::download_all`'s signature).
    fn ok_stage(
        _root: &Path,
        _assets: &[&Asset],
        _on_progress: &mut dyn FnMut(DownloadUpdate),
    ) -> Result<()> {
        Ok(())
    }

    /// A fake download stage that fails.
    fn fail_stage(
        _root: &Path,
        _assets: &[&Asset],
        _on_progress: &mut dyn FnMut(DownloadUpdate),
    ) -> Result<()> {
        Err(AppError::Fatal {
            message: "stage boom".to_string(),
            error_id: ErrorId::new("E_TEST_STAGE"),
            log_path: None,
        })
    }

    /// The launcher MO2 seam: never reached (this host never opens the launcher).
    fn noop_launch(_exe: &Path, _cwd: &Path, _with_profile: bool) -> Result<()> {
        Ok(())
    }

    fn test_host() -> Host {
        Host {
            drives: one_drive,
            load_config: leaf_config,
            relocate: never_relocate,
            spawn_relocated_copy: noop_spawn,
            start_download: noop_download,
            start_install: noop_install,
            restart_install: noop_restart,
            run_finish_actions: noop_finish,
            launch_mo2: noop_launch,
            flash_taskbar: noop_flash,
        }
    }

    /// Builds an `App` without the real pre-flight (needs no relocation, so it
    /// enters `Choices` with the tiny single-leaf config).
    fn app_with(host: Host) -> App {
        let outcome = PreflightOutcome {
            mode: RootMode::Shipped,
            install_root: PathBuf::from(r"C:\Zona\installer"),
            needs_relocation: false,
            launcher: false,
            debug: crate::preflight::DebugOptions::default(),
        };
        App::bootstrap(outcome, host).expect("bootstrap app")
    }

    /// A fake download worker driven through the same `start_worker` seam the
    /// production `start_download` uses: it emits [`CANNED`] ordered events.
    fn canned_download(app: &mut App) -> Result<()> {
        start_worker(
            app,
            "canned-download",
            |report: &mut dyn FnMut(ProgressEvent)| -> Result<()> {
                for index in 0..CANNED {
                    report(ProgressEvent::StepStarted {
                        index,
                        name: format!("canned-{index}"),
                    });
                }
                Ok(())
            },
        )
    }

    /// A worker emits N events and `drain_progress` returns them in order.
    #[test]
    fn worker_emits_events_and_drain_progress_returns_them_in_order() {
        let wakes = Arc::new(AtomicUsize::new(0));

        let mut host = test_host();
        host.start_download = canned_download;
        let mut app = app_with(host);
        app.set_repaint({
            let wakes = Arc::clone(&wakes);
            Arc::new(move || {
                wakes.fetch_add(1, Ordering::SeqCst);
            })
        });

        // Pick the complete root->leaf path (`only` is a leaf) so Confirm is
        // enabled.
        app.handle(Event::ChoiceToggled {
            id: "only".to_string(),
            on: true,
        })
        .expect("pick the only choice");
        app.handle(Event::ConfirmChoices).expect("confirm choices");
        assert_eq!(app.view_model().phase, Phase::Downloading);

        // Deterministic join: every event is in the channel before we drain.
        app.worker
            .as_mut()
            .expect("the download dispatch must have spawned a worker")
            .wait();

        let events = app.drain_progress();
        let expected: Vec<ProgressEvent> = (0..CANNED)
            .map(|index| ProgressEvent::StepStarted {
                index,
                name: format!("canned-{index}"),
            })
            .collect();
        assert_eq!(events, expected, "events must arrive in emission order");

        // One wake per event plus one on worker completion.
        assert_eq!(wakes.load(Ordering::SeqCst), CANNED + 1);
    }

    /// A worker failure is surfaced as `Phase::Fatal` by `drain_progress` (never
    /// dropped silently).
    #[test]
    fn worker_failure_is_surfaced_as_fatal_by_drain_progress() {
        let mut app = app_with(test_host());
        start_worker(
            &mut app,
            "failing-worker",
            |_report: &mut dyn FnMut(ProgressEvent)| -> Result<()> {
                Err(AppError::Fatal {
                    message: "worker boom".to_string(),
                    error_id: ErrorId::new("E_TEST_WORKER"),
                    log_path: None,
                })
            },
        )
        .expect("spawn the failing worker");

        app.worker.as_mut().expect("a worker was spawned").wait();

        let events = app.drain_progress();
        assert!(
            events.is_empty(),
            "a failed worker emits no progress events"
        );
        match &app.view_model().phase {
            Phase::Fatal(state) => assert_eq!(state.error_id, "E_TEST_WORKER"),
            other => panic!("expected Phase::Fatal, got {other:?}"),
        }
    }

    /// With no worker ever spawned, draining returns nothing (no channel attached).
    #[test]
    fn drain_progress_without_a_worker_is_empty() {
        let mut app = app_with(test_host());
        assert!(app.drain_progress().is_empty());
    }

    /// `start_download_with` + a stage returning `Ok` emits exactly
    /// `[ProgressEvent::DownloadsComplete]`.
    #[test]
    fn download_worker_emits_downloads_complete_once_on_success() {
        let mut app = app_with(test_host());
        start_download_with(&mut app, ok_stage).expect("spawn the download worker");

        // Join first so the event is in the channel before draining.
        app.worker.as_mut().expect("a worker was spawned").wait();

        let events = app.drain_progress();
        assert_eq!(
            events,
            vec![ProgressEvent::DownloadsComplete],
            "a successful stage emits DownloadsComplete exactly once"
        );
    }

    /// A stage returning `Err` surfaces `Phase::Fatal` and emits no
    /// `DownloadsComplete`.
    #[test]
    fn download_worker_emits_nothing_and_is_fatal_on_stage_error() {
        let mut app = app_with(test_host());
        start_download_with(&mut app, fail_stage).expect("spawn the download worker");

        app.worker.as_mut().expect("a worker was spawned").wait();

        let events = app.drain_progress();
        assert!(
            events.is_empty(),
            "a failed stage emits no DownloadsComplete"
        );
        match &app.view_model().phase {
            Phase::Fatal(state) => assert_eq!(state.error_id, "E_TEST_STAGE"),
            other => panic!("expected Phase::Fatal, got {other:?}"),
        }
    }

    // ---- per-file download progress mapping -------------------------------

    /// An aria2c summary record with an optional associated FILE path.
    fn progress_record(file: Option<&str>) -> Aria2FileProgress {
        Aria2FileProgress {
            gid: "deadbe".to_string(),
            done: "16KiB".to_string(),
            total: "2.8MiB".to_string(),
            pct: Some(7),
            cn: 1,
            dl: "15KiB".to_string(),
            eta: "3m3s".to_string(),
            file: file.map(PathBuf::from),
        }
    }

    /// A fake stage that streams ONE named record and ONE nameless record.
    fn streaming_stage(
        _root: &Path,
        _assets: &[&Asset],
        on_progress: &mut dyn FnMut(DownloadUpdate),
    ) -> Result<()> {
        on_progress(DownloadUpdate::Aria2(progress_record(Some(
            r"C:\Zona\installer\downloads\anomaly.7z",
        ))));
        on_progress(DownloadUpdate::Aria2(progress_record(None)));
        Ok(())
    }

    /// A record with a FILE path maps to a `DownloadProgress` event keyed by the
    /// path's file-name component.
    #[test]
    fn download_progress_maps_a_file_path_to_a_basename_event() {
        let record = progress_record(Some(r"C:\Zona\installer\downloads\anomaly.7z"));
        assert_eq!(
            download_progress(&record),
            Some(ProgressEvent::DownloadProgress {
                file: "anomaly.7z".to_string(),
                pct: Some(7),
                done: "16KiB".to_string(),
                total: "2.8MiB".to_string(),
            })
        );
    }

    /// A record with no file (`None`) or a path with no file-name component
    /// maps to no event.
    #[test]
    fn download_progress_without_a_file_name_maps_to_no_event() {
        assert_eq!(download_progress(&progress_record(None)), None, "no file");
        assert_eq!(
            download_progress(&progress_record(Some(r"C:\"))),
            None,
            "a drive root has no file-name component"
        );
        assert_eq!(
            download_progress(&progress_record(Some(r"C:\Zona\.."))),
            None,
            "a parent path has no file-name component"
        );
    }

    /// The worker forwards each named record and drops the nameless one, then
    /// still emits the single `DownloadsComplete` on success.
    #[test]
    fn download_worker_forwards_named_records_and_drops_nameless_ones() {
        let mut app = app_with(test_host());
        start_download_with(&mut app, streaming_stage).expect("spawn the download worker");

        app.worker.as_mut().expect("a worker was spawned").wait();

        assert_eq!(
            app.drain_progress(),
            vec![
                ProgressEvent::DownloadProgress {
                    file: "anomaly.7z".to_string(),
                    pct: Some(7),
                    done: "16KiB".to_string(),
                    total: "2.8MiB".to_string(),
                },
                ProgressEvent::DownloadsComplete,
            ],
            "the named record is forwarded and the nameless one dropped"
        );
    }
}
