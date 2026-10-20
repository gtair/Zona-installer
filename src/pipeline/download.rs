//! Download orchestration for the needed assets.
//!
//! [`download_all`] is the whole download stage: it turns the gated asset list
//! into files in `installer\downloads\` with one `aria2c.exe` invocation. The
//! work is ordered so every failure is explicit:
//!
//! 1. **Create `downloads\`** idempotently.
//! 2. **Skip already-correct files** whose SHA-256 matches (case-insensitive).
//! 3. **One aria2c run** for the remainder, streaming progress.
//! 4. **Our own SHA-256 pass** over every file fetched this run (the ground
//!    truth).

use std::path::{Path, PathBuf};

use super::DownloadStatus;
use crate::config::Asset;
use crate::error::{AppError, Result};
use crate::hashing::{hex_eq, sha256_file};
use crate::paths::resolved_paths;
use crate::run::aria2::{build_args, build_input_file, download};
use crate::run::{ChildRunner, RealRunner};
use crate::toolout::aria2::Aria2FileProgress;
use crate::win32::job::JobHandle;

/// The bundled aria2c binary name, resolved under `<install_root>\dependencies\`.
const ARIA2_EXE: &str = "aria2c.exe";

/// The generated aria2c `--input-file` name, inside the downloads directory.
const INPUT_FILE_NAME: &str = "aria2c_input.txt";

/// The per-run aria2c log file name, inside the `logs\` directory.
const ARIA2_LOG_NAME: &str = "aria2c.log";

/// A streamed update from the download stage.
///
/// Everything that takes time reports through this one channel: hashing existing
/// files, resolving mirrors, the aria2c transfer, and the final verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadUpdate {
    /// One aria2c per-file progress record.
    Aria2(Aria2FileProgress),
    /// One file's updated progress.
    FileProgress {
        file: String,
        pct: Option<u32>,
        done: String,
        total: String,
    },
    /// One file's updated status (queued, checking, completed, failed, etc.).
    FileStatus {
        file: String,
        status: DownloadStatus,
    },
    /// A human-readable description of the current phase.
    Status(String),
}

/// Extracts the `dn=` display name parameter from a magnet URL, percent-decoded.
pub fn magnet_display_name(url: &str) -> Option<String> {
    if !url.starts_with("magnet:?") {
        return None;
    }
    for param in url["magnet:?".len()..].split('&') {
        if let Some(dn) = param.strip_prefix("dn=") {
            return Some(percent_decode(dn));
        }
    }
    None
}

/// Helper to percent-decode a query string value.
fn percent_decode(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let h1 = chars.next();
            let h2 = chars.next();
            if let (Some(h1), Some(h2)) = (h1, h2) {
                let v1 = hex_val(h1);
                let v2 = hex_val(h2);
                if let (Some(v1), Some(v2)) = (v1, v2) {
                    bytes.push((v1 << 4) | v2);
                    continue;
                }
            }
        } else if b == b'+' {
            bytes.push(b' ');
            continue;
        }
        bytes.push(b);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Formats a byte count in human-readable aria2 style (e.g. `2.3MiB`).
pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.1}GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1}MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1}KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes}B")
    }
}

/// The sidecar file aria2c keeps next to a partially downloaded file.
fn aria2_control_path(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_os_string();
    name.push(".aria2");
    PathBuf::from(name)
}

/// Runs the whole download stage for the needed `assets` into
/// `<install_root>\downloads\`.
///
/// This is the entry point the app's download worker calls. It wires a
/// kill-on-close job-bound [`RealRunner`] and the bundled `dependencies\aria2c.exe`,
/// then defers to the injected [`download_all_with`].
pub fn download_all(
    install_root: &Path,
    assets: &[&Asset],
    on_update: &mut dyn FnMut(DownloadUpdate),
) -> Result<()> {
    let paths = resolved_paths(install_root);
    let aria2_exe = paths.deps.join(ARIA2_EXE);
    let job = JobHandle::new_kill_on_close()?;
    let runner = RealRunner { job: Some(job) };
    download_all_with(install_root, assets, &runner, &aria2_exe, on_update)
}

/// The testable core of [`download_all`]: the child runner and the aria2c path
/// are injected.
///
/// Hashing policy (each byte is hashed by us at most once per run):
/// * a file already in `downloads\` under the right name is hashed once up front;
///   a match is skipped, a mismatch is handed to aria2c to overwrite;
/// * a file with an aria2c `.aria2` control sidecar is a partial download, so it
///   is not hashed and aria2c resumes it;
/// * every file aria2c fetched this run is hashed once afterwards, and that pass
///   is the ground truth.
pub fn download_all_with(
    install_root: &Path,
    assets: &[&Asset],
    runner: &dyn ChildRunner,
    aria2_exe: &Path,
    on_update: &mut dyn FnMut(DownloadUpdate),
) -> Result<()> {
    let paths = resolved_paths(install_root);

    // 1. Create the download target (and the log dir for aria2c) idempotently.
    create_dir(&paths.downloads, "create downloads directory")?;
    create_dir(&paths.logs, "create logs directory")?;

    log::info!(
        target: "pipeline:download_all",
        "download stage starting: {} assets",
        assets.len(),
    );

    // 2. Skip every needed asset whose file is already present and correct.
    let total = assets.len();
    let mut remaining: Vec<&Asset> = Vec::new();
    for (index, &asset) in assets.iter().enumerate() {
        let path = paths.downloads.join(&asset.file);
        let magnet_name = magnet_display_name(&asset.url);
        let magnet_path = magnet_name.as_ref().map(|dn| paths.downloads.join(dn));

        // If the expected target file doesn't exist, but an existing magnet file exists without .aria2:
        if !path.is_file() {
            if let Some(mpath) = &magnet_path {
                if mpath.is_file() && !aria2_control_path(mpath).exists() {
                    on_update(DownloadUpdate::FileStatus {
                        file: asset.file.clone(),
                        status: DownloadStatus::Checking,
                    });
                    on_update(DownloadUpdate::Status(format!(
                        "Checking existing file {}/{total}: {}",
                        index + 1,
                        asset.file
                    )));
                    let actual = sha256_file(mpath)?;
                    if hex_eq(&actual, &asset.sha256) {
                        let _ = std::fs::rename(mpath, &path);
                    }
                }
            }
        }

        if path.is_file() && !aria2_control_path(&path).exists() {
            on_update(DownloadUpdate::FileStatus {
                file: asset.file.clone(),
                status: DownloadStatus::Checking,
            });
            on_update(DownloadUpdate::Status(format!(
                "Checking existing file {}/{total}: {}",
                index + 1,
                asset.file
            )));
            let actual = sha256_file(&path)?;
            if hex_eq(&actual, &asset.sha256) {
                log::debug!(
                    target: "pipeline:download_all_with",
                    "skip {}: existing file matches its hash",
                    asset.file,
                );
                let size_str = std::fs::metadata(&path)
                    .map(|m| format_bytes(m.len()))
                    .unwrap_or_else(|_| "Done".to_string());
                on_update(DownloadUpdate::FileProgress {
                    file: asset.file.clone(),
                    pct: Some(100),
                    done: size_str.clone(),
                    total: size_str,
                });
                on_update(DownloadUpdate::FileStatus {
                    file: asset.file.clone(),
                    status: DownloadStatus::Completed,
                });
                continue;
            }
            log::debug!(
                target: "pipeline:download_all_with",
                "{} exists but its hash differs; aria2c will overwrite it",
                asset.file,
            );
        }
        log::debug!(
            target: "pipeline:download_all_with",
            "queue {} for download",
            asset.file,
        );
        on_update(DownloadUpdate::FileStatus {
            file: asset.file.clone(),
            status: DownloadStatus::Queued,
        });
        remaining.push(asset);
    }

    // 3. One aria2c run for the remainder; nothing to do if every file was
    //    already correct.
    if !remaining.is_empty() {
        let input_file = paths.downloads.join(INPUT_FILE_NAME);
        let log_file = paths.logs.join(ARIA2_LOG_NAME);
        let body = build_input_file(&remaining, &paths.downloads);
        std::fs::write(&input_file, body)
            .map_err(|source| io_error("write aria2c input file", &input_file, source))?;
        let args = build_args(&input_file, &log_file);
        on_update(DownloadUpdate::Status("Downloading...".to_string()));
        let res = download(runner, aria2_exe, &args, &mut |progress| {
            on_update(DownloadUpdate::Aria2(progress));
        });
        if let Err(err) = res {
            for asset in &remaining {
                on_update(DownloadUpdate::FileStatus {
                    file: asset.file.clone(),
                    status: DownloadStatus::Failed,
                });
            }
            return Err(err);
        }
    }

    // 4. Our own SHA-256 pass over every file aria2c fetched this run (the ground
    //    truth). Files skipped in step 2 were hashed moments ago, so they are not
    //    hashed a second time.
    let fetched = remaining.len();
    for (index, asset) in remaining.iter().enumerate() {
        let path = paths.downloads.join(&asset.file);
        // If a magnet download created the file under its dn name, rename it to asset.file.
        if !path.exists() {
            if let Some(dn) = magnet_display_name(&asset.url) {
                let mpath = paths.downloads.join(&dn);
                if mpath.exists() {
                    let _ = std::fs::rename(&mpath, &path);
                }
            }
        }
        on_update(DownloadUpdate::FileStatus {
            file: asset.file.clone(),
            status: DownloadStatus::Verifying,
        });
        on_update(DownloadUpdate::Status(format!(
            "Verifying download {}/{fetched}: {}",
            index + 1,
            asset.file
        )));
        let actual = match sha256_file(&path) {
            Ok(hash) => hash,
            Err(e) => {
                on_update(DownloadUpdate::FileStatus {
                    file: asset.file.clone(),
                    status: DownloadStatus::Failed,
                });
                return Err(e);
            }
        };
        if !hex_eq(&actual, &asset.sha256) {
            log::debug!(
                target: "pipeline:download_all_with",
                "hash mismatch for {}",
                asset.file,
            );
            on_update(DownloadUpdate::FileStatus {
                file: asset.file.clone(),
                status: DownloadStatus::Failed,
            });
            return Err(AppError::HashMismatch {
                asset_id: asset.id.clone(),
                path,
                expected: asset.sha256.clone(),
                actual,
            });
        }
        let size_str = std::fs::metadata(&path)
            .map(|m| format_bytes(m.len()))
            .unwrap_or_else(|_| "Done".to_string());
        on_update(DownloadUpdate::FileProgress {
            file: asset.file.clone(),
            pct: Some(100),
            done: size_str.clone(),
            total: size_str,
        });
        on_update(DownloadUpdate::FileStatus {
            file: asset.file.clone(),
            status: DownloadStatus::Completed,
        });
        log::debug!(target: "pipeline:download_all_with", "hash ok: {}", asset.file);
    }

    Ok(())
}

/// Creates `dir` (and its parents) idempotently.
fn create_dir(dir: &Path, context: &str) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|source| io_error(context, dir, source))
}

/// Builds an [`AppError::Io`] naming the operation and the offending path.
fn io_error(context: &str, path: &Path, source: std::io::Error) -> AppError {
    AppError::Io {
        context: context.to_string(),
        path: Some(path.to_path_buf()),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChoiceSpec;
    use crate::run::ChildOutput;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// The well-known SHA-256 of `b"hello world"` (ground truth; see `hashing`).
    const SHA256_HELLO_WORLD: &str =
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique temp directory.
    fn temp_root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "zona_installer_download_{tag}_{}_{nanos}_{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    /// Removes a temp tree at the end of a test.
    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Builds an asset with a minimal, inactive choice.
    fn asset(id: &str, file: &str, url: &str, sha256: &str) -> Asset {
        Asset {
            id: id.to_string(),
            file: file.to_string(),
            url: url.to_string(),
            sha256: sha256.to_string(),
            choice: ChoiceSpec::One("install_zona".to_string()),
        }
    }

    /// A fake [`ChildRunner`] that records how many times it ran, optionally
    /// writes canned files (simulating aria2c producing the downloads) and
    /// streams canned stdout, returning a canned exit code.
    struct FakeRunner {
        exit_code: i32,
        stdout: Vec<u8>,
        writes: Vec<(PathBuf, Vec<u8>)>,
        calls: RefCell<usize>,
    }

    impl FakeRunner {
        fn new(exit_code: i32) -> Self {
            Self {
                exit_code,
                stdout: Vec::new(),
                writes: Vec::new(),
                calls: RefCell::new(0),
            }
        }
    }

    impl ChildRunner for FakeRunner {
        fn run(&self, _cmd: &mut Command, on_stdout: &mut dyn FnMut(&[u8])) -> Result<ChildOutput> {
            *self.calls.borrow_mut() += 1;
            for (path, content) in &self.writes {
                std::fs::write(path, content).expect("fake runner writes canned file");
            }
            for chunk in self.stdout.chunks(4) {
                on_stdout(chunk);
            }
            Ok(ChildOutput {
                exit_code: self.exit_code,
            })
        }
    }

    #[test]
    fn already_correct_files_are_omitted_and_a_mismatch_is_a_hash_mismatch() {
        let root = temp_root("skip");
        let install_root = root.join("installer");
        let downloads = install_root.join("downloads");
        std::fs::create_dir_all(&downloads).expect("create downloads dir");

        // `keep` is present and correct even though the expected hash is UPPER
        // case (the comparison is case-insensitive).
        std::fs::write(downloads.join("keep.bin"), b"hello world").expect("write keep");
        // `bad` is present but its bytes do not match the expected hash.
        std::fs::write(downloads.join("bad.bin"), b"totally different bytes").expect("write bad");

        let keep = asset(
            "keep",
            "keep.bin",
            "https://example.com/keep.bin",
            SHA256_HELLO_WORLD.to_uppercase().as_str(),
        );
        let bad = asset(
            "bad",
            "bad.bin",
            "https://example.com/bad.bin",
            SHA256_HELLO_WORLD,
        );
        let assets = [&keep, &bad];

        let runner = FakeRunner::new(0);

        let error = download_all_with(
            &install_root,
            &assets,
            &runner,
            Path::new("aria2c.exe"),
            &mut |_| {},
        )
        .expect_err("a mismatched hash must be fatal");

        match &error {
            AppError::HashMismatch {
                asset_id, expected, ..
            } => {
                assert_eq!(asset_id, "bad");
                assert_eq!(expected, SHA256_HELLO_WORLD);
            }
            other => panic!("expected HashMismatch, got {other:?}"),
        }

        // The already-correct file is omitted; only the mismatched one is queued.
        let input = std::fs::read_to_string(downloads.join(INPUT_FILE_NAME))
            .expect("read generated input file");
        assert!(
            input.contains("https://example.com/bad.bin"),
            "the mismatched asset must be queued: {input}"
        );
        assert!(
            !input.contains("https://example.com/keep.bin"),
            "the already-correct asset must be omitted: {input}"
        );
        assert_eq!(
            *runner.calls.borrow(),
            1,
            "exactly one aria2c run for the remaining assets"
        );

        cleanup(&root);
    }

    #[test]
    fn all_present_assets_skip_the_aria2c_run() {
        let root = temp_root("all_present");
        let install_root = root.join("installer");
        let downloads = install_root.join("downloads");
        std::fs::create_dir_all(&downloads).expect("create downloads dir");
        std::fs::write(downloads.join("keep.bin"), b"hello world").expect("write keep");

        let keep = asset(
            "keep",
            "keep.bin",
            "https://example.com/keep.bin",
            SHA256_HELLO_WORLD,
        );
        let assets = [&keep];

        let runner = FakeRunner::new(0);

        download_all_with(
            &install_root,
            &assets,
            &runner,
            Path::new("aria2c.exe"),
            &mut |_| {},
        )
        .expect("everything already correct must succeed");

        assert_eq!(*runner.calls.borrow(), 0, "no aria2c run is needed");
        assert!(
            !downloads.join(INPUT_FILE_NAME).exists(),
            "no input file is written when nothing remains to download"
        );

        cleanup(&root);
    }

    #[test]
    fn a_failed_aria2c_run_surfaces_the_log_path() {
        let root = temp_root("tool_error");
        let install_root = root.join("installer");
        let downloads = install_root.join("downloads");
        std::fs::create_dir_all(&downloads).expect("create downloads dir");

        // Not present -> queued -> the fake aria2c exits non-zero.
        let missing = asset(
            "missing",
            "missing.bin",
            "https://example.com/missing.bin",
            SHA256_HELLO_WORLD,
        );
        let assets = [&missing];

        let runner = FakeRunner::new(7);

        let error = download_all_with(
            &install_root,
            &assets,
            &runner,
            Path::new("aria2c.exe"),
            &mut |_| {},
        )
        .expect_err("a non-zero exit must be a fatal tool error");

        match error {
            AppError::Tool {
                tool,
                exit_code,
                log_path,
                ..
            } => {
                assert_eq!(tool, "aria2c.exe");
                assert_eq!(exit_code, Some(7));
                assert_eq!(
                    log_path,
                    Some(install_root.join("logs").join(ARIA2_LOG_NAME)),
                    "the aria2c log path must be surfaced"
                );
            }
            other => panic!("expected a Tool error, got {other:?}"),
        }

        cleanup(&root);
    }

    #[test]
    fn magnet_display_name_extracts_and_decodes_dn() {
        let url = "magnet:?xt=urn:btih:205676f90498f2be3900a3eb0d29b9f50a591fa1&dn=Anomaly-1.5.3-Full.7z&tr=udp://bt2.archive.org:6969/announce";
        assert_eq!(
            magnet_display_name(url),
            Some("Anomaly-1.5.3-Full.7z".to_string())
        );

        let encoded = "magnet:?xt=urn:btih:1234&dn=My+Archive%20File.7z";
        assert_eq!(
            magnet_display_name(encoded),
            Some("My Archive File.7z".to_string())
        );

        assert_eq!(magnet_display_name("https://example.com/file.7z"), None);
        assert_eq!(magnet_display_name("magnet:?xt=urn:btih:1234"), None);
    }

    #[test]
    fn format_bytes_formats_units() {
        assert_eq!(format_bytes(500), "500B");
        assert_eq!(format_bytes(1024), "1.0KiB");
        assert_eq!(format_bytes(1024 * 1024 * 5), "5.0MiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024 * 2), "2.0GiB");
    }

    #[test]
    fn skipped_file_reports_completed_status_and_100_pct() {
        let root = temp_root("skipped_status");
        let install_root = root.join("installer");
        let downloads = install_root.join("downloads");
        std::fs::create_dir_all(&downloads).expect("create downloads dir");
        std::fs::write(downloads.join("keep.bin"), b"hello world").expect("write keep");

        let keep = asset(
            "keep",
            "keep.bin",
            "https://example.com/keep.bin",
            SHA256_HELLO_WORLD,
        );
        let assets = [&keep];
        let runner = FakeRunner::new(0);

        let mut updates = Vec::new();
        download_all_with(
            &install_root,
            &assets,
            &runner,
            Path::new("aria2c.exe"),
            &mut |update| updates.push(update),
        )
        .expect("download_all_with must succeed");

        assert!(
            updates.iter().any(|u| matches!(
                u,
                DownloadUpdate::FileStatus {
                    file,
                    status: DownloadStatus::Completed
                } if file == "keep.bin"
            )),
            "skipped file must report Completed status"
        );
        assert!(
            updates.iter().any(|u| matches!(
                u,
                DownloadUpdate::FileProgress {
                    file,
                    pct: Some(100),
                    ..
                } if file == "keep.bin"
            )),
            "skipped file must report 100% progress"
        );

        cleanup(&root);
    }

    #[test]
    fn magnet_dn_file_is_renamed_to_asset_file_when_present() {
        let root = temp_root("magnet_rename");
        let install_root = root.join("installer");
        let downloads = install_root.join("downloads");
        std::fs::create_dir_all(&downloads).expect("create downloads dir");
        // Write file under magnet dn name instead of asset.file name.
        std::fs::write(downloads.join("Anomaly-1.5.3-Full.7z"), b"hello world")
            .expect("write dn file");

        let magnet_asset = asset(
            "download_anomaly",
            "anomaly.7z",
            "magnet:?xt=urn:btih:205676f90498f2be3900a3eb0d29b9f50a591fa1&dn=Anomaly-1.5.3-Full.7z",
            SHA256_HELLO_WORLD,
        );
        let assets = [&magnet_asset];
        let runner = FakeRunner::new(0);

        download_all_with(
            &install_root,
            &assets,
            &runner,
            Path::new("aria2c.exe"),
            &mut |_| {},
        )
        .expect("download_all_with must succeed and rename magnet dn file");

        assert!(
            downloads.join("anomaly.7z").exists(),
            "file must be renamed to asset.file"
        );
        assert!(
            !downloads.join("Anomaly-1.5.3-Full.7z").exists(),
            "original dn file should no longer exist after rename"
        );

        cleanup(&root);
    }
}
