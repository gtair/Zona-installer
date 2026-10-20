//! Install-step dispatch loop and the `create_dir` / `delete` / `template_fill`
//! handlers.
//!
//! [`run_steps`] walks the gated step list in order and dispatches each step to
//! its handler by `action`, emitting a [`ProgressEvent::StepStarted`] before and
//! a [`ProgressEvent::StepDone`] after every step that succeeds; the long-running
//! handlers emit live [`ProgressEvent::StepProgress`] events in between.
//!
//! Paths are resolved relative to the install root; `template_fill` also
//! auto-injects `{{BASE_PATH}}`. `extract` is handled by [`extract::extract_step`],
//! `verify_game` by [`verify_game::verify_game_step`], which blocks on the
//! [`VerifyControl`] channel.

pub mod download;
pub mod extract;
pub mod verify_game;

use std::collections::BTreeMap;
use std::path::Path;

use crate::config::{DirSpec, Step};
use crate::error::{AppError, ErrorId, Result};
use crate::paths::{resolve, zona_dir};
use crate::template::fill_template;

/// A progress event emitted while running the install pipeline.
///
/// This is the plain-data seam the GUI renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressEvent {
    /// Step `index` (0-based position in the gated step list) has started.
    StepStarted { index: usize, name: String },
    /// Live progress within step `index`.
    ///
    /// `extract` streams 7z's real percentage plus the file being written,
    /// `delete` / `create_dir` report a count-based percentage, and
    /// `template_fill` reports the target as an indeterminate detail. `percent`
    /// is `None` when the step cannot report a real fraction; `detail` is `None`
    /// when there is nothing to show.
    StepProgress {
        /// 0-based position of the step this progress belongs to.
        index: usize,
        /// The step's completion percentage, when it has a real one.
        percent: Option<u32>,
        /// A human-readable detail line for the step.
        detail: Option<String>,
    },
    /// Step `index` finished successfully.
    StepDone { index: usize, name: String },
    /// The `verify_game` step is ready: ask the user to launch the game.
    ///
    /// A non-zero game exit is NOT fatal: the worker logs it and still emits
    /// [`ProgressEvent::VerifyGameFinished`].
    VerifyGameReady {
        /// The game executable that will be launched.
        exe: String,
    },
    /// The game process tree has exited; the UI may enable the Continue button.
    VerifyGameFinished,
    /// Cosmetic progress for one file the download stage is fetching.
    ///
    /// Keyed by `file` — the local filename. `done` / `total` are aria2c's
    /// cosmetic `2.3MiB`-style strings and `pct` is its reported percentage when
    /// present. A record with no associated file name yields no event.
    DownloadProgress {
        /// The local file name the record belongs to.
        file: String,
        /// aria2c's reported percentage, when the record carried one.
        pct: Option<u32>,
        /// Bytes downloaded so far (aria2c's cosmetic string).
        done: String,
        /// Total size (aria2c's cosmetic string).
        total: String,
    },
    /// The download stage finished successfully.
    ///
    /// Emitted exactly once after [`download::download_all`] returns `Ok`; a
    /// failed stage returns a fatal error instead. It lets the app leave
    /// `Phase::Downloading` and show the install screen.
    DownloadsComplete,
    /// A human-readable description of what the download stage is doing right now.
    DownloadStatus {
        /// The status line shown on the download screen.
        message: String,
    },
    /// Status transition for a specific download file (queued, checking,
    /// completed, failed, etc.).
    DownloadFileStatus {
        /// The local file name.
        file: String,
        /// The new status of the download file.
        status: DownloadStatus,
    },
}

/// Cosmetic status for one file in the download stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadStatus {
    /// Waiting to be processed or downloaded.
    Queued,
    /// Existing file is being checked against its expected hash.
    Checking,
    /// Currently being downloaded by aria2c.
    Downloading,
    /// Download finished and the file is being verified via SHA-256.
    Verifying,
    /// Verified successfully or already present and correct.
    Completed,
    /// Download or verification failed.
    Failed,
}

/// A command sent from the app to the blocked `verify_game` worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyCommand {
    /// The user clicked "Start game": launch the game and wait for its tree.
    Start,
    /// The user clicked "Continue" after the game closed: resume the pipeline.
    Continue,
    /// Abort the gate (restart / teardown). The worker returns an error which
    /// the app deliberately ignores, because a new worker owns the run.
    Cancel,
}

/// The worker-side handle for the verify-game command channel.
///
/// The app owns the [`std::sync::mpsc::Sender`] (so it can send commands from
/// the UI thread) and hands this receiver to [`run_steps`]. A channel is used
/// rather than a condvar because it blocks the worker naturally, is easy to
/// test, and dropping the sender unblocks a stale worker.
pub struct VerifyControl {
    rx: std::sync::mpsc::Receiver<VerifyCommand>,
}

impl VerifyControl {
    /// Creates a command channel, returning the app-side sender and the
    /// worker-side control.
    pub fn channel() -> (std::sync::mpsc::Sender<VerifyCommand>, VerifyControl) {
        let (tx, rx) = std::sync::mpsc::channel();
        (tx, VerifyControl { rx })
    }

    /// Blocks until the next command arrives.
    ///
    /// Returns `None` when every sender has been dropped, which the caller must
    /// treat as a cancellation (the run is being torn down).
    pub fn wait(&self) -> Option<VerifyCommand> {
        self.rx.recv().ok()
    }
}

/// Runs `steps` in order, dispatching each by `action`.
///
/// Emits [`ProgressEvent::StepStarted`] immediately before a step runs and
/// [`ProgressEvent::StepDone`] immediately after it succeeds. The `index` is the
/// 0-based position within `steps`; the UI adds one for the `Step [i]:` label.
/// The first failing step stops the pipeline and returns its (fatal) error; no
/// `StepDone` is emitted for it.
pub fn run_steps(
    install_root: &Path,
    steps: &[&Step],
    on_progress: &mut dyn FnMut(ProgressEvent),
    verify: &VerifyControl,
) -> Result<()> {
    let total = steps.len();
    log::info!(target: "pipeline:run_steps", "install pipeline starting: {total} steps");
    for (index, step) in steps.iter().enumerate() {
        // 1-based for the human-readable log, matching the UI's `Step [i]:`.
        let i = index + 1;
        log::info!(
            target: "pipeline:run_steps",
            "step {i}/{total} start: {} [{}]",
            step.name,
            step.action,
        );
        on_progress(ProgressEvent::StepStarted {
            index,
            name: step.name.clone(),
        });
        if let Err(error) = dispatch(install_root, step, index, &mut *on_progress, verify) {
            // The loop returns early, so a failed step never gets a "done" line:
            // log the failure BEFORE propagating.
            log::error!(
                target: "pipeline:run_steps",
                "step {i}/{total} '{}' failed: {error}",
                step.name,
            );
            return Err(error);
        }
        on_progress(ProgressEvent::StepDone {
            index,
            name: step.name.clone(),
        });
        log::info!(target: "pipeline:run_steps", "step {i}/{total} done: {}", step.name);
    }
    Ok(())
}

/// Dispatches one step to its action handler.
///
/// `index` and `on_progress` are forwarded so the long-running handlers can
/// emit live [`ProgressEvent::StepProgress`]; `verify` is forwarded to
/// `verify_game`, which blocks on the user's Start/Continue commands.
fn dispatch(
    install_root: &Path,
    step: &Step,
    index: usize,
    on_progress: &mut dyn FnMut(ProgressEvent),
    verify: &VerifyControl,
) -> Result<()> {
    match step.action.as_str() {
        "create_dir" => create_dir_step(install_root, step, index, on_progress),
        "delete" => delete_step(install_root, step, index, on_progress),
        "template_fill" => template_fill_step(install_root, step, index, on_progress),
        "extract" => extract::extract_step(install_root, step, index, on_progress),
        "verify_game" => {
            verify_game::verify_game_step(install_root, step, index, on_progress, verify)
        }
        other => Err(AppError::Fatal {
            message: format!("step '{}': unknown action '{}'", step.name, other),
            error_id: ErrorId::new("E_PIPELINE_ACTION"),
            log_path: None,
        }),
    }
}

/// `create_dir`: creates every directory in `dir` (single or list).
///
/// Each path is resolved relative to the install root; `create_dir_all` makes
/// intermediate directories and is idempotent, so an existing directory is fine.
/// A count-based [`ProgressEvent::StepProgress`] is emitted before each one.
pub fn create_dir_step(
    install_root: &Path,
    step: &Step,
    index: usize,
    on_progress: &mut dyn FnMut(ProgressEvent),
) -> Result<()> {
    let dirs = dir_list(step)?;
    let total = dirs.len();
    for (i, rel) in dirs.iter().enumerate() {
        let path = resolve(install_root, rel);
        log::debug!(
            target: "pipeline:create_dir_step",
            "create_dir step '{}': {}",
            step.name,
            path.display(),
        );
        on_progress(ProgressEvent::StepProgress {
            index,
            percent: Some(count_percent(i, total)),
            detail: Some(format!("Creating {}", path.display())),
        });
        std::fs::create_dir_all(&path).map_err(|source| AppError::Io {
            context: format!("create directory for step '{}'", step.name),
            path: Some(path),
            source,
        })?;
    }
    Ok(())
}

/// `delete`: recursively removes every directory in `dir` (single or list).
///
/// A path that does not exist is skipped silently (matches the old behavior). A
/// path that exists but cannot be removed is a fatal I/O error. A count-based
/// [`ProgressEvent::StepProgress`] is emitted before each one.
pub fn delete_step(
    install_root: &Path,
    step: &Step,
    index: usize,
    on_progress: &mut dyn FnMut(ProgressEvent),
) -> Result<()> {
    let dirs = dir_list(step)?;
    let total = dirs.len();
    for (i, rel) in dirs.iter().enumerate() {
        let path = resolve(install_root, rel);
        on_progress(ProgressEvent::StepProgress {
            index,
            percent: Some(count_percent(i, total)),
            detail: Some(format!("Deleting {}", path.display())),
        });
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {}
            // Missing path: skip silently, but still log at DEBUG for post-mortems.
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                log::debug!(
                    target: "pipeline:delete_step",
                    "delete step '{}': {} does not exist, skipping",
                    step.name,
                    path.display(),
                );
            }
            Err(source) => {
                return Err(AppError::Io {
                    context: format!("delete directory for step '{}'", step.name),
                    path: Some(path),
                    source,
                });
            }
        }
    }
    Ok(())
}

/// `template_fill`: reads `source`, substitutes `{{BASE_PATH}}` (auto, doubled
/// backslashes) plus every step `replacements` key, creates the `target` parent
/// directories, and writes the result to `target`.
pub fn template_fill_step(
    install_root: &Path,
    step: &Step,
    index: usize,
    on_progress: &mut dyn FnMut(ProgressEvent),
) -> Result<()> {
    let source_rel = step.source.as_deref().ok_or_else(|| {
        step_error(
            step,
            "action 'template_fill' requires a 'source' template path",
        )
    })?;
    let target_rel = step
        .target
        .as_deref()
        .ok_or_else(|| step_error(step, "action 'template_fill' requires a 'target' path"))?;

    let source = resolve(install_root, source_rel);
    let target = resolve(install_root, target_rel);
    log::debug!(
        target: "pipeline:template_fill_step",
        "template_fill step '{}': {} -> {}",
        step.name,
        source.display(),
        target.display(),
    );
    // A single-file write: report the target as an indeterminate detail line.
    on_progress(ProgressEvent::StepProgress {
        index,
        percent: None,
        detail: Some(format!("Writing {}", target.display())),
    });

    let template = std::fs::read_to_string(&source).map_err(|source_err| AppError::Io {
        context: format!("read template for step '{}'", step.name),
        path: Some(source),
        source: source_err,
    })?;

    // BASE_PATH = the Zona dir = the install root's parent, with backslashes
    // doubled by `fill_template`.
    let base_path = zona_dir(install_root);
    let base_path = base_path.to_str().ok_or_else(|| {
        step_error(
            step,
            format!("install root is not valid UTF-8: {}", base_path.display()),
        )
    })?;

    let empty = BTreeMap::new();
    let replacements = step.replacements.as_ref().unwrap_or(&empty);
    let filled = fill_template(&template, replacements, base_path)?;

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|source| AppError::Io {
            context: format!("create target parent for step '{}'", step.name),
            path: Some(parent.to_path_buf()),
            source,
        })?;
    }

    std::fs::write(&target, filled).map_err(|source| AppError::Io {
        context: format!("write template target for step '{}'", step.name),
        path: Some(target),
        source,
    })?;
    Ok(())
}

/// Extracts the `dir` selector of a `create_dir` / `delete` step as a slice.
///
/// `DirSpec::One` borrows the single string without allocating; a step with no
/// `dir` is an explicit error rather than a silent no-op.
fn dir_list(step: &Step) -> Result<&[String]> {
    match &step.dir {
        Some(DirSpec::One(path)) => Ok(std::slice::from_ref(path)),
        Some(DirSpec::Many(paths)) => Ok(paths.as_slice()),
        None => Err(step_error(
            step,
            format!("action '{}' requires a 'dir'", step.action),
        )),
    }
}

/// A count-based completion percentage for the `index`th of `total` items (pure).
///
/// The first item reports 0%, the last reports `(total - 1) * 100 / total`, and
/// the loop's `StepDone` reports 100% for the whole step. Cosmetic only.
fn count_percent(index: usize, total: usize) -> u32 {
    if total == 0 {
        return 0;
    }
    ((index as u64 * 100) / total as u64).min(100) as u32
}

/// A fatal error naming the offending step.
fn step_error(step: &Step, message: impl Into<String>) -> AppError {
    AppError::Fatal {
        message: format!("step '{}': {}", step.name, message.into()),
        error_id: ErrorId::new("E_PIPELINE_STEP"),
        log_path: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChoiceSpec;
    use crate::template::doubled_backslashes;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique temp directory, cleaned up by [`cleanup`] at the end of a test.
    fn temp_root(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "zona_installer_pipeline_{tag}_{}_{}_{}",
            std::process::id(),
            nanos,
            unique
        ));
        std::fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A step with every optional field empty; tests override what they need.
    fn step(name: &str, action: &str) -> Step {
        Step {
            name: name.to_string(),
            action: action.to_string(),
            choice: None,
            dir: None,
            file: None,
            target: None,
            from: None,
            source: None,
            replacements: None,
        }
    }

    /// A `VerifyControl` whose sender is dropped, so `wait()` returns `None`
    /// immediately. These tests have no `verify_game` step, so it is never used.
    fn disconnected_verify() -> VerifyControl {
        let (tx, verify) = VerifyControl::channel();
        drop(tx);
        verify
    }

    #[test]
    fn dispatch_runs_steps_in_order_and_emits_indexed_events() {
        let root = temp_root("dispatch");
        let install_root = root.join("installer");

        // 0: create `a`; 1: create `b` + `c`; 2: delete `a`. The delete only
        // succeeds if step 0 already ran, so the final state proves the order.
        let step_a = Step {
            dir: Some(DirSpec::One("a".to_string())),
            ..step("create_a", "create_dir")
        };
        let step_b = Step {
            dir: Some(DirSpec::Many(vec!["b".to_string(), "c".to_string()])),
            ..step("create_b_c", "create_dir")
        };
        let step_c = Step {
            dir: Some(DirSpec::One("a".to_string())),
            ..step("delete_a", "delete")
        };

        let steps: Vec<&Step> = vec![&step_a, &step_b, &step_c];
        let verify = disconnected_verify();
        let mut events = Vec::new();
        run_steps(
            &install_root,
            &steps,
            &mut |event| events.push(event),
            &verify,
        )
        .expect("all three steps must succeed");

        // The StepStarted/StepDone order is unchanged; each step also emits
        // live StepProgress events between them.
        let started_done: Vec<&ProgressEvent> = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    ProgressEvent::StepStarted { .. } | ProgressEvent::StepDone { .. }
                )
            })
            .collect();
        assert_eq!(
            started_done,
            vec![
                &ProgressEvent::StepStarted {
                    index: 0,
                    name: "create_a".to_string(),
                },
                &ProgressEvent::StepDone {
                    index: 0,
                    name: "create_a".to_string(),
                },
                &ProgressEvent::StepStarted {
                    index: 1,
                    name: "create_b_c".to_string(),
                },
                &ProgressEvent::StepDone {
                    index: 1,
                    name: "create_b_c".to_string(),
                },
                &ProgressEvent::StepStarted {
                    index: 2,
                    name: "delete_a".to_string(),
                },
                &ProgressEvent::StepDone {
                    index: 2,
                    name: "delete_a".to_string(),
                },
            ]
        );

        // create_a reports one, create_b_c two, delete_a one: four progress
        // events, each carrying its step index and a count-based percentage.
        let progress: Vec<&ProgressEvent> = events
            .iter()
            .filter(|event| matches!(event, ProgressEvent::StepProgress { .. }))
            .collect();
        assert_eq!(progress.len(), 4, "one StepProgress per item: {events:?}");
        for event in progress {
            match event {
                ProgressEvent::StepProgress {
                    index,
                    percent,
                    detail,
                } => {
                    assert!(*index <= 2);
                    assert!(percent.is_some(), "count-based percent");
                    assert!(
                        detail
                            .as_ref()
                            .is_some_and(|line| line.starts_with("Creating ")
                                || line.starts_with("Deleting ")),
                        "detail names the path: {detail:?}"
                    );
                }
                _ => unreachable!(),
            }
        }

        assert!(!install_root.join("a").exists(), "step 2 deleted `a`");
        assert!(install_root.join("b").is_dir(), "step 1 created `b`");
        assert!(install_root.join("c").is_dir(), "step 1 created `c`");

        cleanup(&root);
    }

    #[test]
    fn dispatch_stops_on_first_failure_without_step_done() {
        let root = temp_root("stop");
        let install_root = root.join("installer");

        // Step 0 template_fill has no `source`: a fatal error before any StepDone.
        let bad = step("bad_template", "template_fill");
        let good = Step {
            dir: Some(DirSpec::One("never".to_string())),
            ..step("never_runs", "create_dir")
        };

        let steps: Vec<&Step> = vec![&bad, &good];
        let verify = disconnected_verify();
        let mut events = Vec::new();
        let error = run_steps(
            &install_root,
            &steps,
            &mut |event| events.push(event),
            &verify,
        )
        .expect_err("a step without `source` must fail");

        assert!(matches!(error, AppError::Fatal { .. }));
        assert_eq!(
            events,
            vec![ProgressEvent::StepStarted {
                index: 0,
                name: "bad_template".to_string(),
            }]
        );
        assert!(!install_root.join("never").exists(), "step 1 must not run");

        cleanup(&root);
    }

    #[test]
    fn create_dir_and_delete_accept_single_and_list_forms() {
        let root = temp_root("forms");
        let install_root = root.join("installer");

        let create = Step {
            choice: Some(ChoiceSpec::One("base".to_string())),
            dir: Some(DirSpec::Many(vec!["one".to_string(), "two".to_string()])),
            ..step("create_dirs", "create_dir")
        };
        create_dir_step(&install_root, &create, 0, &mut |_| {})
            .expect("list form creates both dirs");
        assert!(install_root.join("one").is_dir());
        assert!(install_root.join("two").is_dir());

        let remove = Step {
            dir: Some(DirSpec::Many(vec![
                "one".to_string(),
                "already_missing".to_string(),
            ])),
            ..step("delete_dirs", "delete")
        };
        delete_step(&install_root, &remove, 0, &mut |_| {})
            .expect("the missing entry is skipped and the present one removed");
        assert!(!install_root.join("one").exists());
        assert!(install_root.join("two").is_dir(), "unlisted dir untouched");

        cleanup(&root);
    }

    #[test]
    fn delete_removes_existing_tree_and_skips_missing_dir_silently() {
        let root = temp_root("delete");
        let install_root = root.join("installer");
        let cache = install_root.join("cache/nested");
        std::fs::create_dir_all(&cache).expect("create cache tree");
        std::fs::write(cache.join("file.bin"), b"data").expect("write cache file");

        let remove = Step {
            dir: Some(DirSpec::One("cache".to_string())),
            ..step("delete_cache", "delete")
        };
        delete_step(&install_root, &remove, 0, &mut |_| {}).expect("recursive delete must succeed");
        assert!(!install_root.join("cache").exists());

        let missing = Step {
            dir: Some(DirSpec::One("does_not_exist".to_string())),
            ..step("delete_missing", "delete")
        };
        delete_step(&install_root, &missing, 0, &mut |_| {})
            .expect("a missing dir is skipped silently");

        cleanup(&root);
    }

    #[test]
    fn template_fill_step_writes_substituted_file_into_temp_tree() {
        let root = temp_root("template");
        let install_root = root.join("installer");
        let downloads = install_root.join("downloads");
        std::fs::create_dir_all(&downloads).expect("create downloads dir");
        // Mirrors the real ModOrganizer.ini.template line (escaped backslashes).
        std::fs::write(
            downloads.join("ModOrganizer.ini.template"),
            "gamePath=@ByteArray({{BASE_PATH}}\\\\anomaly)\nselected_profile={{PROFILE_NAME}}\n",
        )
        .expect("write template");

        let mut replacements = BTreeMap::new();
        replacements.insert("PROFILE_NAME".to_string(), "ZONA SSS23".to_string());

        let step = Step {
            source: Some("downloads/ModOrganizer.ini.template".to_string()),
            target: Some("../mo2/ModOrganizer.ini".to_string()),
            replacements: Some(replacements),
            ..step("setup_ini", "template_fill")
        };

        template_fill_step(&install_root, &step, 0, &mut |_| {})
            .expect("template fill must succeed");

        // `../mo2` resolves to <root>\mo2; the parent dir did not exist.
        let target = root.join("mo2/ModOrganizer.ini");
        assert!(target.is_file(), "target written with parent dirs created");

        let base_path = doubled_backslashes(root.to_str().expect("temp path is UTF-8"));
        let written = std::fs::read_to_string(&target).expect("read written target");
        assert_eq!(
            written,
            format!("gamePath=@ByteArray({base_path}\\\\anomaly)\nselected_profile=ZONA SSS23\n")
        );

        cleanup(&root);
    }
}
