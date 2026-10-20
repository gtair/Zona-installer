//! Process runner core.
//!
//! [`ChildRunner`] is the seam every orchestration decision is written against:
//! unit tests substitute a fake returning a canned [`ChildOutput`], while
//! [`RealRunner`] spawns a child process, optionally assigns it to a job object,
//! streams both pipes into one callback, and waits for the exit code.

pub mod aria2;
pub mod sevenzip;

use std::io::Read;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;

use crate::error::{AppError, Result};
use crate::win32::job::JobHandle;

/// The exit status of a finished child process.
///
/// The runner returns the raw code; deciding what a non-zero code means is
/// [`run_tool`]'s job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildOutput {
    pub exit_code: i32,
}

/// Runs a child command and streams its output.
///
/// Implementations invoke `on_stdout` with every chunk of stdout/stderr as it
/// arrives and return the exit code.
pub trait ChildRunner {
    fn run(&self, cmd: &mut Command, on_stdout: &mut dyn FnMut(&[u8])) -> Result<ChildOutput>;
}

/// The real [`ChildRunner`]: spawn + job object + stream both pipes + wait.
///
/// When [`RealRunner::job`] is `Some`, the spawned process is assigned to that
/// job before its output is drained.
pub struct RealRunner {
    pub job: Option<JobHandle>,
}

impl ChildRunner for RealRunner {
    fn run(&self, cmd: &mut Command, on_stdout: &mut dyn FnMut(&[u8])) -> Result<ChildOutput> {
        // The runner owns the pipe configuration: both streams are always
        // captured so `on_stdout` receives every byte.
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        // The release GUI has no console, so console children (aria2c, 7z) would each
        // pop a window. CREATE_NO_WINDOW; no effect on GUI-subsystem children.
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }

        let program = cmd.get_program().to_string_lossy().into_owned();
        let mut child = cmd.spawn().map_err(|source| AppError::Io {
            context: format!("spawn child process '{program}'"),
            path: None,
            source,
        })?;

        if let Some(job) = &self.job {
            // SAFETY: `child` owns a live process handle; the borrowed handle
            // only reads it (it is non-closing) and `child` outlives it.
            let process = unsafe { BorrowedHandle::borrow_raw(child.as_raw_handle()) };
            job.assign(process)?;
        }

        let stdout = child.stdout.take().ok_or_else(|| pipe_missing("stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| pipe_missing("stderr"))?;

        // Two reader threads drain the pipes concurrently so neither can fill up
        // and deadlock the child. Chunks are forwarded to the single output
        // callback (which is not `Send`) through a channel read on this thread.
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let stdout_tx = tx.clone();
        let stdout_handle = std::thread::spawn(move || drain(stdout, stdout_tx));
        let stderr_handle = std::thread::spawn(move || drain(stderr, tx));

        for chunk in rx {
            on_stdout(&chunk);
        }

        join_reader(stdout_handle, "stdout")?;
        join_reader(stderr_handle, "stderr")?;

        let status = child.wait().map_err(|source| AppError::Io {
            context: "wait for child process".to_string(),
            path: None,
            source,
        })?;

        // On Windows a normally-terminated child always reports a code, but
        // `ExitStatus::code()` is still an `Option`, so keep this fallible
        // rather than reaching for a default.
        let exit_code = status.code().ok_or_else(|| AppError::Io {
            context: "read child exit code".to_string(),
            path: None,
            source: std::io::Error::other("the child did not report an exit code"),
        })?;

        Ok(ChildOutput { exit_code })
    }
}

/// The orchestration decision layer: run a child tool and require exit code 0.
///
/// [`ChildRunner`] returns the raw [`ChildOutput`] even for a non-zero exit;
/// this helper turns a non-zero code into an [`AppError::Tool`], optionally
/// carrying the tool's log path.
pub fn run_tool(
    runner: &dyn ChildRunner,
    tool: &'static str,
    context: &str,
    cmd: &mut Command,
    on_stdout: &mut dyn FnMut(&[u8]),
    log_path: Option<PathBuf>,
) -> Result<ChildOutput> {
    // The one place that both knows the exact command line and carries the
    // human-readable tool label, so the DEBUG record belongs here.
    log::debug!(target: "run:run_tool", "run '{}': {}", tool, command_line(cmd));
    let output = runner.run(cmd, on_stdout)?;
    log::debug!(
        target: "run:run_tool",
        "'{}' exited with code {}",
        tool,
        output.exit_code,
    );
    if output.exit_code == 0 {
        Ok(output)
    } else {
        Err(AppError::Tool {
            tool,
            context: context.to_string(),
            exit_code: Some(output.exit_code),
            log_path,
        })
    }
}

/// Renders `cmd`'s program and arguments as one loggable string.
///
/// Only used for the DEBUG tool record; aria2c's arguments carry no URLs (they
/// live in `--input-file`), so the full command line is safe to log.
fn command_line(cmd: &Command) -> String {
    let mut line = cmd.get_program().to_string_lossy().into_owned();
    for arg in cmd.get_args() {
        line.push(' ');
        line.push_str(&arg.to_string_lossy());
    }
    line
}

/// Reads a captured pipe to EOF, forwarding each chunk to the channel.
fn drain<R: Read>(mut reader: R, tx: mpsc::Sender<Vec<u8>>) -> std::io::Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                // A dropped receiver means the caller stopped listening (for
                // example its callback unwound); stop reading rather than loop.
                if tx.send(buf[..n].to_vec()).is_err() {
                    return Ok(());
                }
            }
            Err(err) => return Err(err),
        }
    }
}

/// Joins a reader thread and maps its outcome (including a panic) to
/// [`AppError::Io`], since any read failure on a captured pipe is fatal.
fn join_reader(handle: std::thread::JoinHandle<std::io::Result<()>>, stream: &str) -> Result<()> {
    match handle.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(source)) => Err(AppError::Io {
            context: format!("read child {stream}"),
            path: None,
            source,
        }),
        Err(_) => Err(AppError::Io {
            context: format!("read child {stream}"),
            path: None,
            source: std::io::Error::other("reader thread panicked"),
        }),
    }
}

/// Builds the error for a pipe that was unexpectedly not captured.
fn pipe_missing(stream: &str) -> AppError {
    AppError::Io {
        context: format!("capture child {stream}"),
        path: None,
        source: std::io::Error::other(format!("{stream} was not piped")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Rust-level fake that streams canned stdout bytes and returns a canned
    /// exit code.
    struct FakeRunner {
        exit_code: i32,
        stdout: Vec<u8>,
    }

    impl ChildRunner for FakeRunner {
        fn run(&self, _cmd: &mut Command, on_stdout: &mut dyn FnMut(&[u8])) -> Result<ChildOutput> {
            // Stream in small chunks so the test exercises the same "many
            // callback invocations" shape as a real pipe read.
            for chunk in self.stdout.chunks(3) {
                on_stdout(chunk);
            }
            Ok(ChildOutput {
                exit_code: self.exit_code,
            })
        }
    }

    #[test]
    fn orchestration_returns_canned_output_and_exit_code() {
        let canned: Vec<u8> = b"one\ntwo\nthree\n".to_vec();
        let fake = FakeRunner {
            exit_code: 0,
            stdout: canned.clone(),
        };

        let mut received = Vec::new();
        let mut cmd = Command::new("fake-tool");
        let output = run_tool(
            &fake,
            "fake.exe",
            "probe",
            &mut cmd,
            &mut |chunk| received.extend_from_slice(chunk),
            None,
        )
        .expect("exit code 0 must succeed");

        assert_eq!(output.exit_code, 0);
        assert_eq!(received, canned);
    }

    #[test]
    fn orchestration_turns_nonzero_exit_into_tool_error() {
        let fake = FakeRunner {
            exit_code: 7,
            stdout: b"boom\n".to_vec(),
        };
        let log = Some(PathBuf::from(r"C:\Zona\installer\logs\run.log"));

        let mut received = Vec::new();
        let mut cmd = Command::new("fake-tool");
        let result = run_tool(
            &fake,
            "7z.exe",
            "extract archive",
            &mut cmd,
            &mut |chunk| received.extend_from_slice(chunk),
            log.clone(),
        );

        match result {
            Err(AppError::Tool {
                tool,
                context,
                exit_code,
                log_path,
            }) => {
                assert_eq!(tool, "7z.exe");
                assert_eq!(context, "extract archive");
                assert_eq!(exit_code, Some(7));
                assert_eq!(log_path, log);
            }
            other => panic!("expected a Tool error, got {other:?}"),
        }
        assert_eq!(received, b"boom\n");
    }

    #[test]
    fn real_runner_runs_bundled_7z_listing_offline() {
        let exe = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("dependencies")
            .join("7z.exe");
        let runner = RealRunner { job: None };

        let mut received = Vec::new();
        let mut cmd = Command::new(&exe);
        cmd.arg("i");

        let output = runner
            .run(&mut cmd, &mut |chunk| received.extend_from_slice(chunk))
            .expect("bundled 7z 'i' must run");

        assert_eq!(output.exit_code, 0);
        assert!(
            !received.is_empty(),
            "bundled 7z 'i' must stream non-empty output"
        );
    }

    #[test]
    fn command_line_joins_the_program_and_its_arguments() {
        let mut cmd = Command::new("7z.exe");
        cmd.arg("x").arg(r"C:\Zona\a b\payload.7z");
        assert_eq!(command_line(&cmd), r"7z.exe x C:\Zona\a b\payload.7z");

        let bare = Command::new("aria2c.exe");
        assert_eq!(command_line(&bare), "aria2c.exe");
    }
}
