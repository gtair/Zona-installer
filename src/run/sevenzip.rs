//! 7z extract command construction + progress streaming.
//!
//! Every archive is extracted through the bundled `7za.exe`. [`extract_args`]
//! builds the argument list; [`extract`] runs the binary through the
//! [`ChildRunner`] seam, streams the progress stream to the pure parser
//! ([`crate::toolout::sevenzip`]), and decides success.
//!
//! Success is `exit code 0` **and** the `Everything is Ok` marker in stdout;
//! progress is purely cosmetic.

use std::path::Path;
use std::process::Command;

use super::{run_tool, ChildRunner};
use crate::error::{AppError, Result};
use crate::toolout::sevenzip::{is_success_stdout, parse_line, SevenZipEvent, SevenZipProgress};

/// The bundled tool name used in [`AppError::Tool`] reports.
const SEVENZIP_TOOL: &str = "7za.exe";

/// Build the `7za.exe` argument list for extracting `archive` into `target`.
///
/// Exactly `x <archive> -o<target> -y -aoa -bb1 -bsp1`: `x` extracts with full
/// paths, `-o<target>` names the output directory (no space), `-y -aoa` answers
/// yes and overwrites everything, and `-bb1 -bsp1` turn on the progress records
/// the parser consumes. A multipart set is passed by its `.001` volume; 7z
/// follows the remaining volumes itself.
pub fn extract_args(archive: &Path, target: &Path) -> Vec<String> {
    vec![
        "x".to_string(),
        archive.display().to_string(),
        format!("-o{}", target.display()),
        "-y".to_string(),
        "-aoa".to_string(),
        "-bb1".to_string(),
        "-bsp1".to_string(),
    ]
}

/// Run the bundled 7z to extract `archive` into `target`, streaming progress.
///
/// The command is `extract_args(archive, target)` run through the [`ChildRunner`]
/// seam. Each `\r`-separated record is parsed; each percent or filename record
/// updates the running [`SevenZipProgress`] and is handed to `on_progress`.
///
/// The result is `Ok(())` only when the process exits `0` **and** stdout contains
/// `Everything is Ok`; a zero exit without the marker is also a [`AppError::Tool`]
/// (a truncated extraction must never look like a success). No log path is
/// carried: 7z writes no `--log` file.
pub fn extract(
    runner: &dyn ChildRunner,
    sevenzip: &Path,
    archive: &Path,
    target: &Path,
    on_progress: &mut dyn FnMut(SevenZipProgress),
) -> Result<()> {
    let mut cmd = Command::new(sevenzip);
    let args = extract_args(archive, target);
    log::debug!(target: "sevenzip:extract", "7z args: {args:?}");
    cmd.args(&args);

    // The whole stdout is kept to run the substring success check at the end;
    // the running progress state is streamed as records arrive.
    let mut stdout = Vec::new();
    let mut lines = CrBuffer::new();
    let mut progress = SevenZipProgress {
        percent: None,
        file: None,
    };

    let output = run_tool(
        runner,
        SEVENZIP_TOOL,
        "extract archive",
        &mut cmd,
        &mut |chunk| {
            stdout.extend_from_slice(chunk);
            lines.push(chunk, &mut |line| {
                apply_line(line, &mut progress, on_progress);
            });
        },
        None,
    );

    // Emit any trailing record that was not terminated by a `\r`.
    lines.flush(&mut |line| apply_line(line, &mut progress, on_progress));

    let exit_code = match output {
        Ok(output) => output.exit_code,
        // 7z already failed: log everything it printed so the run log carries the
        // real reason (e.g. "Can not open output file") instead of a bare code.
        Err(error) => {
            log_7z_output(&stdout);
            return Err(error);
        }
    };

    if is_success_stdout(&stdout) {
        log::debug!(
            target: "sevenzip:extract",
            "7z success: exit 0 with the 'Everything is Ok' marker",
        );
        Ok(())
    } else {
        // Which arm of the two-part decision was taken is exactly what a
        // post-mortem needs: exit 0 but no marker means the extraction did not
        // report completion.
        log::debug!(
            target: "sevenzip:extract",
            "7z failure: exit code {} without the 'Everything is Ok' marker",
            exit_code,
        );
        log_7z_output(&stdout);
        Err(AppError::Tool {
            tool: SEVENZIP_TOOL,
            context: "extract archive (stdout did not report 'Everything is Ok')".to_string(),
            exit_code: Some(exit_code),
            log_path: None,
        })
    }
}

/// Logs the raw 7z output (stdout and stderr are merged by the runner) at ERROR
/// level so a failed extraction reports the actual reason rather than just an
/// exit code.
///
/// 7z separates its records with `\r` (and `\r\n`); each record becomes one log
/// line so the run log stays readable.
fn log_7z_output(raw: &[u8]) {
    let text = String::from_utf8_lossy(raw);
    for line in text
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        log::error!(target: "sevenzip:extract", "7z output: {line}");
    }
}

/// Fold one stdout record into the running progress state and emit it.
///
/// Only the meaningful records update state; the rest are ignored.
fn apply_line(
    line: &[u8],
    progress: &mut SevenZipProgress,
    on_progress: &mut dyn FnMut(SevenZipProgress),
) {
    match parse_line(line) {
        SevenZipEvent::Percent(percent) => {
            progress.percent = Some(percent);
            on_progress(progress.clone());
        }
        SevenZipEvent::File(file) => {
            progress.file = Some(file);
            on_progress(progress.clone());
        }
        SevenZipEvent::Done | SevenZipEvent::Other => {}
    }
}

/// Accumulates streamed bytes and hands out complete `\r`-separated records.
///
/// 7z separates both its `\r\n`-terminated lines and in-place progress updates
/// with a bare `\r`. A chunk boundary may fall mid-record; the trailing partial
/// is retained until more bytes arrive or [`CrBuffer::flush`] is called.
struct CrBuffer {
    buf: Vec<u8>,
}

impl CrBuffer {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Appends `chunk` and invokes `on_line` for every `\r`-terminated record.
    fn push(&mut self, chunk: &[u8], on_line: &mut dyn FnMut(&[u8])) {
        let mut start = 0;
        for (i, &byte) in chunk.iter().enumerate() {
            if byte == b'\r' {
                self.buf.extend_from_slice(&chunk[start..i]);
                on_line(&self.buf);
                self.buf.clear();
                start = i + 1;
            }
        }
        self.buf.extend_from_slice(&chunk[start..]);
    }

    /// Emits any buffered trailing partial record (without a `\r` terminator).
    fn flush(&mut self, on_line: &mut dyn FnMut(&[u8])) {
        if !self.buf.is_empty() {
            on_line(&self.buf);
            self.buf.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{ChildOutput, RealRunner};
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A Rust-level fake that streams canned stdout bytes and returns a canned
    /// exit code. Chunks are deliberately tiny so boundaries fall mid-chunk.
    struct FakeRunner {
        exit_code: i32,
        stdout: Vec<u8>,
    }

    impl ChildRunner for FakeRunner {
        fn run(&self, _cmd: &mut Command, on_stdout: &mut dyn FnMut(&[u8])) -> Result<ChildOutput> {
            for chunk in self.stdout.chunks(3) {
                on_stdout(chunk);
            }
            Ok(ChildOutput {
                exit_code: self.exit_code,
            })
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

    /// The bundled `dependencies\7za.exe`.
    fn bundled_sevenzip() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("dependencies")
            .join("7za.exe")
    }

    #[test]
    fn extract_args_use_the_carried_over_flag_set() {
        let args = extract_args(
            Path::new(r"C:\Zona\installer\downloads\zona_multipart.7z.001"),
            Path::new(r"C:\Zona\anomaly"),
        );

        let expected: Vec<String> = vec![
            "x".to_string(),
            r"C:\Zona\installer\downloads\zona_multipart.7z.001".to_string(),
            r"-oC:\Zona\anomaly".to_string(),
            "-y".to_string(),
            "-aoa".to_string(),
            "-bb1".to_string(),
            "-bsp1".to_string(),
        ];
        assert_eq!(args, expected);
        // The multipart set is pointed at by its `.001` volume only.
        assert!(args[1].ends_with(".7z.001"));
        // The output directory carries no separating space.
        assert_eq!(args[2], r"-oC:\Zona\anomaly");
    }

    #[test]
    fn extract_requires_exit_zero_and_the_success_marker() {
        let archive = Path::new("payload.7z");
        let target = Path::new("out");
        let exe = Path::new("7za.exe");

        // Exit 0 + marker -> success.
        let ok = FakeRunner {
            exit_code: 0,
            stdout: b"Extracting...\r\nEverything is Ok\r\n".to_vec(),
        };
        assert!(extract(&ok, exe, archive, target, &mut |_| {}).is_ok());

        // Exit 0 but no marker -> still a Tool error (never a silent success).
        let no_marker = FakeRunner {
            exit_code: 0,
            stdout: b"Extracting...\r\nERROR: cannot open file\r\n".to_vec(),
        };
        let error = extract(&no_marker, exe, archive, target, &mut |_| {})
            .expect_err("a missing success marker must be a Tool error");
        match error {
            AppError::Tool {
                tool, exit_code, ..
            } => {
                assert_eq!(tool, "7za.exe");
                assert_eq!(exit_code, Some(0));
            }
            other => panic!("expected a Tool error, got {other:?}"),
        }

        // Non-zero exit -> Tool error even if the marker somehow appeared.
        let bad = FakeRunner {
            exit_code: 2,
            stdout: b"Everything is Ok\r\n".to_vec(),
        };
        let error = extract(&bad, exe, archive, target, &mut |_| {})
            .expect_err("a non-zero exit must be a Tool error");
        match error {
            AppError::Tool {
                tool, exit_code, ..
            } => {
                assert_eq!(tool, "7za.exe");
                assert_eq!(exit_code, Some(2));
            }
            other => panic!("expected a Tool error, got {other:?}"),
        }
    }

    #[test]
    fn integration_extracts_a_generated_archive_into_a_temp_dir() {
        let sandbox = temp_dir("zona_installer_7z_extract");
        let src = sandbox.join("src");
        std::fs::create_dir_all(src.join("nested")).expect("create source tree");
        std::fs::write(src.join("hello.txt"), b"zona tiny archive\n").expect("write hello.txt");
        std::fs::write(src.join("nested").join("inner.txt"), b"inner\n").expect("write inner.txt");

        let exe = bundled_sevenzip();
        let archive = sandbox.join("payload.7z");
        let status = Command::new(&exe)
            .current_dir(&src)
            .args(["a", "-t7z"])
            .arg(&archive)
            .args(["hello.txt", "nested"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn the bundled 7z to build the test archive");
        assert!(
            status.success(),
            "bundled 7z failed to build the archive: {status}"
        );

        let target = sandbox.join("out");
        std::fs::create_dir_all(&target).expect("create target dir");

        let runner = RealRunner { job: None };
        let mut updates = 0usize;
        extract(&runner, &exe, &archive, &target, &mut |_p| updates += 1)
            .expect("extracting a generated archive must succeed");
        assert!(updates > 0, "extraction must stream progress records");

        assert_eq!(
            std::fs::read_to_string(target.join("hello.txt")).expect("read hello.txt"),
            "zona tiny archive\n"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("nested").join("inner.txt"))
                .expect("read nested/inner.txt"),
            "inner\n"
        );

        let _ = std::fs::remove_dir_all(&sandbox);
    }

    #[test]
    fn missing_archive_is_a_tool_error() {
        let sandbox = temp_dir("zona_installer_7z_missing");
        let exe = bundled_sevenzip();
        let archive = sandbox.join("does-not-exist.7z");
        let target = sandbox.join("out");
        std::fs::create_dir_all(&target).expect("create target dir");

        let runner = RealRunner { job: None };
        let error = extract(&runner, &exe, &archive, &target, &mut |_| {})
            .expect_err("a missing archive must fail");

        match error {
            AppError::Tool {
                tool, exit_code, ..
            } => {
                assert_eq!(tool, "7za.exe");
                assert!(
                    matches!(exit_code, Some(code) if code != 0),
                    "unexpected exit code {exit_code:?}"
                );
            }
            other => panic!("expected a Tool error, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&sandbox);
    }
}
