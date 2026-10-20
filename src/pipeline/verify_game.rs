//! `verify_game` step handler.
//!
//! The game must be run once before its settings are installed, because the game
//! resets settings on first launch. The flow is user-driven:
//!
//! - resolves the game executable at `..\anomaly\bin\AnomalyDX11AVX.exe`;
//! - emits [`ProgressEvent::VerifyGameReady`] and blocks on the [`VerifyControl`]
//!   channel until the user clicks Start game;
//! - launches the game under a kill-on-close Win32 job object and blocks until
//!   every process in that job has exited;
//! - emits [`ProgressEvent::VerifyGameFinished`] and blocks until the user
//!   acknowledges with Continue.
//!
//! A non-zero game exit code is logged and treated as finished; only a spawn
//! failure is fatal. Job-based waiting (not `child.wait()`) is required because
//! the game is a launcher whose main process exits quickly while the real game
//! continues as a grandchild.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Step;
use crate::error::{AppError, ErrorId, Result};
use crate::paths::resolve;
use crate::win32::job::JobHandle;

use super::{ProgressEvent, VerifyCommand, VerifyControl};

/// The game executable, relative to the install root.
pub const GAME_EXE_REL: &str = r"..\anomaly\bin\AnomalyDX11AVX.exe";

/// The tool name reported in log lines for the game process.
const GAME_TOOL: &str = "AnomalyDX11AVX.exe";

/// Resolves the game executable path for `install_root`.
pub fn game_exe_path(install_root: &Path) -> PathBuf {
    resolve(install_root, GAME_EXE_REL)
}

/// The injectable seam for launching the game and waiting for its process tree.
///
/// Production: [`ProductionGameRunner`]. Tests: [`FakeGameRunner`].
pub(crate) trait GameRunner {
    /// Launch the game at `exe` and block until its entire process tree exits.
    fn launch_and_wait(&self, exe: &Path) -> Result<i32>;
}

/// The production [`GameRunner`]: spawns the game under a kill-on-close job
/// object and waits via an I/O completion port for the whole process tree.
pub(crate) struct ProductionGameRunner;

impl GameRunner for ProductionGameRunner {
    fn launch_and_wait(&self, exe: &Path) -> Result<i32> {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::io::BorrowedHandle;
        use std::os::windows::process::CommandExt;

        let job = JobHandle::new_kill_on_close()?;

        // Spawn the game without capturing pipes — it is a Win32 GUI subsystem
        // application and does not write to stdout/stderr. Capturing them would
        // create inheritable handles that the launcher propagates to its children,
        // which can interfere with GUI startup on some titles.
        //
        // CREATE_NO_WINDOW (0x0800_0000) suppresses a spurious console window for
        // the rare case where the game binary has the console subsystem flag set;
        // it has no effect on normal Win32 GUI applications.
        let mut cmd = Command::new(exe);
        cmd.creation_flags(0x0800_0000);

        let program = exe.to_string_lossy().into_owned();
        let mut child = cmd.spawn().map_err(|source| AppError::Io {
            context: format!("spawn game process '{program}'"),
            path: None,
            source,
        })?;

        // Assign the immediate child to the job BEFORE it can spawn its own
        // children, so the whole process tree is captured.
        // SAFETY: `child` owns a live process handle; the borrowed handle only
        // reads it (non-closing) and `child` outlives it.
        let process = unsafe { BorrowedHandle::borrow_raw(child.as_raw_handle()) };
        job.assign(process)?;

        log::debug!(
            target: "pipeline:verify_game",
            "verify_game: waiting for job to drain (all processes to exit)",
        );

        // Block until the entire process tree (launcher + game children) exits.
        // Unlike `child.wait()`, this waits for the real game even though the
        // launcher's main process may exit quickly.
        job.wait_until_all_exit()?;

        // Reap the immediate child to get its exit code.  It has already exited
        // (the job drained), so this is non-blocking in practice.
        let status = child.wait().map_err(|source| AppError::Io {
            context: "wait for game process exit code".to_string(),
            path: None,
            source,
        })?;
        let exit_code = status.code().unwrap_or(0);

        Ok(exit_code)
    }
}

/// The dispatch entry point for a `verify_game` step.
///
/// Emits [`ProgressEvent::VerifyGameReady`], blocks until the user clicks Start
/// game, launches and waits for the game, emits
/// [`ProgressEvent::VerifyGameFinished`], then blocks until the user clicks
/// Continue. A missing executable is fatal; a non-zero exit code is not.
pub fn verify_game_step(
    install_root: &Path,
    step: &Step,
    index: usize,
    on_progress: &mut dyn FnMut(ProgressEvent),
    verify: &VerifyControl,
) -> Result<()> {
    verify_game_step_with(
        &ProductionGameRunner,
        install_root,
        step,
        index,
        on_progress,
        verify,
    )
}

/// The testable core of [`verify_game_step`]: the game runner is injected.
///
/// The worker waits for [`VerifyCommand::Start`] before launching, and for
/// [`VerifyCommand::Continue`] after the game exits. A non-zero exit code is
/// logged and treated as finished; only a spawn failure is a hard error.
pub(crate) fn verify_game_step_with(
    runner: &dyn GameRunner,
    install_root: &Path,
    step: &Step,
    _index: usize,
    on_progress: &mut dyn FnMut(ProgressEvent),
    verify: &VerifyControl,
) -> Result<()> {
    let exe = game_exe_path(install_root);

    log::info!(
        target: "pipeline:verify_game",
        "verify_game: ready for step '{}', waiting for the user to start {}",
        step.name,
        exe.display(),
    );
    on_progress(ProgressEvent::VerifyGameReady {
        exe: exe.display().to_string(),
    });

    // Block until the user clicks Start game (or the gate is cancelled).
    wait_for(verify, VerifyCommand::Start)?;

    // A missing/unlaunchable executable is the ONLY hard failure.
    let exit_code = runner.launch_and_wait(&exe)?;
    if exit_code != 0 {
        // Not fatal: the user may close the game with a crash dialog or kill it.
        log::warn!(
            target: "pipeline:verify_game",
            "verify_game: {GAME_TOOL} exited with code {exit_code}; treating as finished",
        );
    } else {
        log::info!(target: "pipeline:verify_game", "verify_game: {GAME_TOOL} exited cleanly");
    }

    on_progress(ProgressEvent::VerifyGameFinished);

    // Block until the user acknowledges with Continue (or the gate is cancelled).
    wait_for(verify, VerifyCommand::Continue)?;
    Ok(())
}

/// Blocks until `wanted` arrives, returning an error on cancellation.
///
/// Any other command arriving early is ignored (e.g. a stray Continue while
/// still waiting for Start). A dropped sender (`None`) is a cancellation, which
/// the app ignores on restart.
fn wait_for(verify: &VerifyControl, wanted: VerifyCommand) -> Result<()> {
    loop {
        match verify.wait() {
            Some(command) if command == wanted => return Ok(()),
            Some(VerifyCommand::Cancel) | None => return Err(cancelled_error()),
            Some(_) => continue,
        }
    }
}

/// The error returned when the verify gate is cancelled while the worker waits.
fn cancelled_error() -> AppError {
    AppError::Fatal {
        message: "the verify-game step was cancelled".to_string(),
        error_id: ErrorId::new("E_VERIFY_CANCELLED"),
        log_path: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A fake runner returning a canned exit code; it never spawns anything.
    struct FakeGameRunner {
        exit_code: i32,
    }

    impl GameRunner for FakeGameRunner {
        fn launch_and_wait(&self, _exe: &Path) -> Result<i32> {
            Ok(self.exit_code)
        }
    }

    /// A unique directory under the system temp dir.
    fn temp_dir(prefix: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("{prefix}-{}-{nanos}-{seq}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    /// A `verify_game` step with every optional field empty.
    fn step_for(name: &str) -> Step {
        Step {
            name: name.to_string(),
            action: "verify_game".to_string(),
            choice: None,
            dir: None,
            file: None,
            target: None,
            from: None,
            source: None,
            replacements: None,
        }
    }

    #[test]
    fn start_then_continue_emits_ready_and_finished_and_succeeds() {
        let root = temp_dir("zona_installer_verify_game_ok");
        let install_root = root.join("installer");
        std::fs::create_dir_all(&install_root).expect("create install root");
        let step = step_for("verify_game");
        let exe = game_exe_path(&install_root).display().to_string();

        // Pre-send the two commands the worker blocks on, so this synchronous
        // test never deadlocks.
        let (tx, verify) = VerifyControl::channel();
        tx.send(VerifyCommand::Start).expect("send start");
        tx.send(VerifyCommand::Continue).expect("send continue");

        let mut events = Vec::new();
        verify_game_step_with(
            &FakeGameRunner { exit_code: 0 },
            &install_root,
            &step,
            0,
            &mut |event| events.push(event),
            &verify,
        )
        .expect("an exit code of 0 must succeed");

        // Ready first (Start game), finished after the process tree exits.
        assert_eq!(
            events,
            vec![
                ProgressEvent::VerifyGameReady { exe },
                ProgressEvent::VerifyGameFinished,
            ]
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nonzero_exit_is_logged_but_not_fatal_and_still_finishes() {
        let root = temp_dir("zona_installer_verify_game_fail");
        let install_root = root.join("installer");
        std::fs::create_dir_all(&install_root).expect("create install root");
        let step = step_for("verify_game");
        let exe = game_exe_path(&install_root).display().to_string();

        let (tx, verify) = VerifyControl::channel();
        tx.send(VerifyCommand::Start).expect("send start");
        tx.send(VerifyCommand::Continue).expect("send continue");

        let mut events = Vec::new();
        // A non-zero game exit is NOT fatal.
        verify_game_step_with(
            &FakeGameRunner { exit_code: 3 },
            &install_root,
            &step,
            0,
            &mut |event| events.push(event),
            &verify,
        )
        .expect("a non-zero exit must still finish");

        assert_eq!(
            events,
            vec![
                ProgressEvent::VerifyGameReady { exe },
                ProgressEvent::VerifyGameFinished,
            ]
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cancel_while_waiting_to_start_returns_a_cancellation_error() {
        let root = temp_dir("zona_installer_verify_game_cancel");
        let install_root = root.join("installer");
        std::fs::create_dir_all(&install_root).expect("create install root");
        let step = step_for("verify_game");

        let (tx, verify) = VerifyControl::channel();
        tx.send(VerifyCommand::Cancel).expect("send cancel");

        let mut events = Vec::new();
        let error = verify_game_step_with(
            &FakeGameRunner { exit_code: 0 },
            &install_root,
            &step,
            0,
            &mut |event| events.push(event),
            &verify,
        )
        .expect_err("a cancelled gate must return an error");
        assert_eq!(error.error_id().as_str(), "E_VERIFY_CANCELLED");
        // Only the ready event was emitted; no finish on a cancellation.
        assert_eq!(
            events,
            vec![ProgressEvent::VerifyGameReady {
                exe: game_exe_path(&install_root).display().to_string(),
            }]
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn game_exe_path_resolves_to_the_anomaly_bin_under_a_temp_root() {
        let root = temp_dir("zona_installer_verify_game_path");
        let install_root = root.join("installer");
        std::fs::create_dir_all(&install_root).expect("create install root");

        // install_root = <root>\installer, so `..\anomaly\...` lands in <root>.
        let exe = game_exe_path(&install_root);
        assert_eq!(
            exe,
            root.join("anomaly").join("bin").join("AnomalyDX11AVX.exe")
        );
        // The documented relative form is exactly the hardcoded resolution.
        assert_eq!(GAME_EXE_REL, r"..\anomaly\bin\AnomalyDX11AVX.exe");
        assert_eq!(
            exe.file_name().and_then(|n| n.to_str()),
            Some("AnomalyDX11AVX.exe")
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
