//! aria2c command construction + download orchestration.
//!
//! Downloads run as one `aria2c.exe` invocation fed through an `--input-file`
//! carrying per-URL `dir=` / `out=` / `gid=` / `checksum=` blocks.
//! [`build_input_file`] renders that body, [`build_args`] returns the flag set,
//! and [`download`] runs the process and feeds stdout to the pure parser.
//!
//! Success is decided by the exit code plus the caller's own SHA-256 pass, never
//! from GIDs; results are mapped back to assets by `path/URI`.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::{run_tool, ChildRunner};
use crate::config::Asset;
use crate::error::Result;
use crate::toolout::aria2::{
    gid_for, parse_file_line, parse_summary_line, strip_ansi, Aria2FileProgress,
};

/// The bundled tool name used in [`crate::error::AppError::Tool`] reports.
const ARIA2_TOOL: &str = "aria2c.exe";

/// The browser User-Agent sent with every aria2c request.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36";

/// The indentation used for the per-URL option lines inside the input file.
const OPTION_INDENT: &str = "  ";

/// Render an aria2c `--input-file` body for `assets`.
///
/// Each asset becomes one block: its URL line followed by `dir=` (the downloads
/// directory), `out=` (the asset's local `file`), `gid=` ([`gid_for`] of the
/// asset id). HTTP(S) URLs also get `checksum=sha-256=<hex>`; magnet URIs rely
/// on BitTorrent piece verification, followed by the installer's own SHA-256
/// pass after the transfer completes.
pub fn build_input_file(assets: &[&Asset], downloads_dir: &Path) -> String {
    let dir = downloads_dir.display();
    let mut out = String::new();
    for asset in assets {
        out.push_str(&asset.url);
        out.push('\n');
        out.push_str(OPTION_INDENT);
        out.push_str("dir=");
        out.push_str(&dir.to_string());
        out.push('\n');
        out.push_str(OPTION_INDENT);
        out.push_str("out=");
        out.push_str(&asset.file);
        out.push('\n');
        out.push_str(OPTION_INDENT);
        out.push_str("gid=");
        out.push_str(&gid_for(&asset.id));
        out.push('\n');
        // aria2's per-URI checksum option is intended for HTTP/FTP downloads.
        // BitTorrent verifies pieces itself; pipeline::download performs the
        // authoritative SHA-256 check after either transfer type completes.
        if !asset.url.starts_with("magnet:?") {
            out.push_str(OPTION_INDENT);
            out.push_str("checksum=sha-256=");
            out.push_str(&asset.sha256.to_ascii_lowercase());
            out.push('\n');
        }
    }
    out
}

/// Build the aria2c flag set.
///
/// `input_file` is the generated `--input-file` body's path and `log_file` is
/// the per-run aria2c log path.
pub fn build_args(input_file: &Path, log_file: &Path) -> Vec<String> {
    vec![
        format!("--input-file={}", input_file.display()),
        "--allow-overwrite=true".to_string(),
        "--auto-file-renaming=false".to_string(),
        "--continue=true".to_string(),
        "--max-concurrent-downloads=4".to_string(),
        "--split=4".to_string(),
        "--max-connection-per-server=4".to_string(),
        "--min-split-size=10M".to_string(),
        "--max-tries=5".to_string(),
        "--retry-wait=10".to_string(),
        "--connect-timeout=30".to_string(),
        "--timeout=60".to_string(),
        "--check-integrity=true".to_string(),
        "--auto-save-interval=10".to_string(),
        "--disable-ipv6=true".to_string(),
        "--summary-interval=1".to_string(),
        "--console-log-level=warn".to_string(),
        format!("--log={}", log_file.display()),
        "--log-level=info".to_string(),
        "--seed-time=0".to_string(),
        format!("--user-agent={USER_AGENT}"),
    ]
}

/// Run the single aria2c download invocation and stream progress to
/// `on_progress`.
///
/// The command is `aria2 <args>` run through the [`ChildRunner`] seam. Streamed
/// stdout is split into lines, ANSI-stripped and parsed: a summary line becomes
/// the pending [`Aria2FileProgress`], and the `FILE:` line that follows supplies
/// the file path and emits the record.
///
/// The result is `Ok(())` only on a zero exit code; a non-zero exit becomes an
/// [`crate::error::AppError::Tool`] carrying the aria2c log path. Success is
/// judged from the exit code plus the caller's own SHA-256 pass — never from
/// GIDs.
pub fn download(
    runner: &dyn ChildRunner,
    aria2: &Path,
    args: &[String],
    on_progress: &mut dyn FnMut(Aria2FileProgress),
) -> Result<()> {
    let mut cmd = Command::new(aria2);
    cmd.args(args);
    // No URLs in the argv: they live in the `--input-file`, so the full flag
    // vector is safe to log.
    log::debug!(target: "aria2:download", "aria2c args: {}", args.join(" "));

    let log_path = log_path_from_args(args);
    if let Some(path) = &log_path {
        log::debug!(target: "aria2:download", "aria2c log file: {}", path.display());
    }
    let mut buffer = LineBuffer::new();
    let mut pending: Option<Aria2FileProgress> = None;

    let result = run_tool(
        runner,
        ARIA2_TOOL,
        "download assets",
        &mut cmd,
        &mut |chunk| {
            buffer.push(chunk, &mut |line| {
                process_line(line, &mut pending, on_progress);
            });
        },
        log_path,
    );

    // Emit a final record if the stream ended without a trailing separator.
    buffer.flush(&mut |line| process_line(line, &mut pending, on_progress));

    result.map(|_| ())
}

/// Recover the aria2c log path from a `--log=<path>` argument, if present.
fn log_path_from_args(args: &[String]) -> Option<PathBuf> {
    args.iter()
        .find_map(|arg| arg.strip_prefix("--log="))
        .map(PathBuf::from)
}

/// Parse one output line, pairing a summary record with its following `FILE:`
/// line before emitting it.
fn process_line(
    line: &[u8],
    pending: &mut Option<Aria2FileProgress>,
    on_progress: &mut dyn FnMut(Aria2FileProgress),
) {
    let clean = strip_ansi(line);
    if let Some(progress) = parse_summary_line(&clean) {
        // The summary record carries no path; hold it until its `FILE:` line.
        *pending = Some(progress);
    } else if let Some(file) = parse_file_line(&clean) {
        if let Some(mut progress) = pending.take() {
            progress.file = Some(file);
            on_progress(progress);
        }
    }
}

/// Accumulates streamed bytes and hands out complete lines.
///
/// aria2c separates in-place updates with a bare `\r` and ends banner lines with
/// `\n`, so both bytes are treated as separators. A chunk boundary may fall
/// mid-line; the trailing partial is retained until more bytes arrive or
/// [`LineBuffer::flush`] is called.
struct LineBuffer {
    buf: Vec<u8>,
}

impl LineBuffer {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Appends `chunk` and invokes `on_line` for every complete line it yields.
    fn push(&mut self, chunk: &[u8], on_line: &mut dyn FnMut(&[u8])) {
        self.buf.extend_from_slice(chunk);
        let mut start = 0;
        let mut i = 0;
        while i < self.buf.len() {
            if matches!(self.buf[i], b'\n' | b'\r') {
                on_line(&self.buf[start..i]);
                start = i + 1;
            }
            i += 1;
        }
        if start > 0 {
            self.buf.drain(..start);
        }
    }

    /// Emits any buffered trailing partial line (without a separator).
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
    use crate::config::ChoiceSpec;
    use crate::error::AppError;
    use crate::run::ChildOutput;

    /// A Rust-level fake that streams canned stdout bytes and returns a canned
    /// exit code. Chunks are deliberately tiny so line boundaries fall mid-chunk.
    struct FakeRunner {
        exit_code: i32,
        stdout: Vec<u8>,
    }

    impl ChildRunner for FakeRunner {
        fn run(&self, _cmd: &mut Command, on_stdout: &mut dyn FnMut(&[u8])) -> Result<ChildOutput> {
            for chunk in self.stdout.chunks(5) {
                on_stdout(chunk);
            }
            Ok(ChildOutput {
                exit_code: self.exit_code,
            })
        }
    }

    fn asset(id: &str, file: &str, url: &str, sha256: &str) -> Asset {
        Asset {
            id: id.to_string(),
            file: file.to_string(),
            url: url.to_string(),
            sha256: sha256.to_string(),
            choice: ChoiceSpec::One("install_zona".to_string()),
        }
    }

    #[test]
    fn input_file_has_per_url_blocks_with_native_paths_and_checksums() {
        let downloads = PathBuf::from(r"C:\Zona\installer\downloads");
        let assets = [
            asset(
                "download_zona_1",
                "zona_multipart.7z.001",
                "https://example.com/zona_multipart.7z.001",
                "BD16CF6F22475D99D47B5DC14A25923445AA3A8FD68909DA9E9414E75FCB9661",
            ),
            asset(
                "download_anomaly",
                "anomaly.7z",
                "https://example.com/anomaly.7z",
                "e564d9110e673eacfa5604b0837f6e6208eae4cd46cd5682326ed59f931527ae",
            ),
            asset(
                "download_anomaly_torrent",
                "anomaly-torrent.7z",
                "magnet:?xt=urn:btih:205676f90498f2be3900a3eb0d29b9f50a591fa1&dn=Anomaly-1.5.3-Full.7z",
                "e564d9110e673eacfa5604b0837f6e6208eae4cd46cd5682326ed59f931527ae",
            ),
        ];
        let refs: Vec<&Asset> = assets.iter().collect();

        let input = build_input_file(&refs, &downloads);

        let expected_first = format!(
            "https://example.com/zona_multipart.7z.001\n\
             {indent}dir=C:\\Zona\\installer\\downloads\n\
             {indent}out=zona_multipart.7z.001\n\
             {indent}gid={}\n\
             {indent}checksum=sha-256=bd16cf6f22475d99d47b5dc14a25923445aa3a8fd68909da9e9414e75fcb9661\n",
            gid_for("download_zona_1"),
            indent = OPTION_INDENT,
        );
        let expected_second = format!(
            "https://example.com/anomaly.7z\n\
             {indent}dir=C:\\Zona\\installer\\downloads\n\
             {indent}out=anomaly.7z\n\
             {indent}gid={}\n\
             {indent}checksum=sha-256=e564d9110e673eacfa5604b0837f6e6208eae4cd46cd5682326ed59f931527ae\n",
            gid_for("download_anomaly"),
            indent = OPTION_INDENT,
        );

        assert!(input.starts_with("https://example.com/zona_multipart.7z.001\n"));
        assert!(
            input.contains(&expected_first),
            "first block missing/misformatted:\n{input}"
        );
        assert!(
            input.contains(&expected_second),
            "second block missing/misformatted:\n{input}"
        );
        // HTTP(S) entries keep aria2's checksum option.
        assert!(input.contains("checksum=sha-256=bd16cf6f"));
        assert!(input.contains("checksum=sha-256=e564d911"));
        // Magnet entries are passed through as a URI, with their output name
        // and GID, but no per-URI checksum option; final SHA-256 is checked by
        // pipeline::download after aria2c finishes.
        assert!(input.contains(
            "magnet:?xt=urn:btih:205676f90498f2be3900a3eb0d29b9f50a591fa1&dn=Anomaly-1.5.3-Full.7z\n"
        ));
        let magnet_block = input.split("magnet:?xt=").nth(1).expect("magnet block");
        let magnet_block = magnet_block
            .split("https://")
            .next()
            .unwrap_or(magnet_block);
        assert!(magnet_block.contains("out=anomaly-torrent.7z"));
        assert!(!magnet_block.contains("checksum=sha-256="));
        // The derived GIDs are the 16-hex stable ids.
        assert!(input.contains(&format!("gid={}", gid_for("download_zona_1"))));
    }

    #[test]
    fn args_are_the_exact_carried_over_flag_set() {
        let input_file = PathBuf::from(r"C:\Zona\installer\downloads\input.txt");
        let log_file = PathBuf::from(r"C:\Zona\installer\logs\aria2c.log");

        let args = build_args(&input_file, &log_file);

        let expected: Vec<String> = vec![
            r"--input-file=C:\Zona\installer\downloads\input.txt".to_string(),
            "--allow-overwrite=true".to_string(),
            "--auto-file-renaming=false".to_string(),
            "--continue=true".to_string(),
            "--max-concurrent-downloads=4".to_string(),
            "--split=4".to_string(),
            "--max-connection-per-server=4".to_string(),
            "--min-split-size=10M".to_string(),
            "--max-tries=5".to_string(),
            "--retry-wait=10".to_string(),
            "--connect-timeout=30".to_string(),
            "--timeout=60".to_string(),
            "--check-integrity=true".to_string(),
            "--auto-save-interval=10".to_string(),
            "--disable-ipv6=true".to_string(),
            "--summary-interval=1".to_string(),
            "--console-log-level=warn".to_string(),
            r"--log=C:\Zona\installer\logs\aria2c.log".to_string(),
            "--log-level=info".to_string(),
            "--seed-time=0".to_string(),
            format!("--user-agent={USER_AGENT}"),
        ];
        assert_eq!(args, expected);
        // The UA is a Chrome UA.
        assert!(args
            .iter()
            .any(|arg| arg == &format!("--user-agent={USER_AGENT}")));
    }

    #[test]
    fn download_succeeds_on_exit_zero_and_reports_tool_error_otherwise() {
        // Non-existent paths: the fake runner never spawns anything.
        let input_file = PathBuf::from(r"C:\Zona\installer\downloads\input.txt");
        let log_file = PathBuf::from(r"C:\Zona\installer\logs\aria2c.log");
        let args = build_args(&input_file, &log_file);

        let ok = FakeRunner {
            exit_code: 0,
            stdout: Vec::new(),
        };
        let mut progress = Vec::new();
        assert!(
            download(&ok, Path::new("aria2c.exe"), &args, &mut |p| progress
                .push(p))
            .is_ok()
        );
        assert!(progress.is_empty());

        let bad = FakeRunner {
            exit_code: 7,
            stdout: b"some output\n".to_vec(),
        };
        let mut progress = Vec::new();
        let error = download(&bad, Path::new("aria2c.exe"), &args, &mut |p| {
            progress.push(p)
        })
        .expect_err("a non-zero exit must be a Tool error");

        match error {
            AppError::Tool {
                tool,
                exit_code,
                log_path,
                ..
            } => {
                assert_eq!(tool, "aria2c.exe");
                assert_eq!(exit_code, Some(7));
                assert_eq!(log_path, Some(log_file));
            }
            other => panic!("expected a Tool error, got {other:?}"),
        }
    }

    #[test]
    fn download_streams_summary_progress_paired_with_file_lines() {
        let raw = include_bytes!("../../tests/fixtures/aria2_summary.stdout");
        let input_file = PathBuf::from(r"C:\Zona\installer\downloads\input.txt");
        let log_file = PathBuf::from(r"C:\Zona\installer\logs\aria2c.log");
        let args = build_args(&input_file, &log_file);

        let fake = FakeRunner {
            exit_code: 0,
            stdout: raw.to_vec(),
        };
        let mut progress = Vec::new();
        download(&fake, Path::new("aria2c.exe"), &args, &mut |p| {
            progress.push(p)
        })
        .expect("exit 0 must succeed");

        // Four `FILE:` lines each close a pending summary -> four records, in
        // order, each carrying the downloaded file (the summary alone has none).
        let pcts: Vec<Option<u32>> = progress.iter().map(|p| p.pct).collect();
        assert_eq!(pcts, vec![Some(0), Some(27), Some(55), Some(83)]);
        assert!(progress
            .iter()
            .all(|p| p.file.as_deref() == Some(Path::new("dl_sum/blob.bin"))));
        assert_eq!(progress[3].done, "2.3MiB");
        assert_eq!(progress[3].total, "2.8MiB");
    }

    #[test]
    fn log_path_is_recovered_only_from_the_log_flag() {
        assert_eq!(
            log_path_from_args(&[
                "--input-file=x".to_string(),
                r"--log=C:\logs\a.log".to_string(),
                "--log-level=info".to_string(),
            ]),
            Some(PathBuf::from(r"C:\logs\a.log"))
        );
        assert_eq!(log_path_from_args(&["--log-level=info".to_string()]), None);
        assert_eq!(log_path_from_args(&[]), None);
    }
}
