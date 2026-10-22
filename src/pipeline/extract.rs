//! `extract` step handler.
//!
//! Every archive — plain `.7z`, `.zip`, or a multipart `.7z.001` volume set — is
//! extracted through the single bundled `7za.exe`, reusing the
//! [`crate::run::sevenzip::extract`] seam. This module owns the orchestration:
//!
//! - resolve `file` / `target` relative to the install root;
//! - point 7z at the `.001` volume for a multipart set (a step naming the base
//!   `foo.7z` is redirected to `foo.7z.001` when that volume exists);
//! - create the `target` directory before extracting;
//! - with `from:` set, extract into a same-drive temp dir under `<install_root>\.tmp\`
//!   (never the system temp, so the follow-up move is a same-volume rename), then
//!   move each `from` sub-path's contents into `target`, stripping the prefix.
//!
//! `-y -aoa` (overwrite everything, merging into the target) are part of the
//! reused [`extract_args`](crate::run::sevenzip::extract_args) flag set.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Step;
use crate::error::{AppError, Result};
use crate::paths::{resolve, resolved_paths};
use crate::run::sevenzip;
use crate::run::{ChildRunner, RealRunner};
use crate::toolout::sevenzip::SevenZipProgress;
use crate::win32::job::JobHandle;

use super::ProgressEvent;

/// The bundled 7z binary name, resolved under `<install_root>\dependencies\`.
const SEVENZIP_EXE: &str = "7za.exe";

/// Serializes the same-drive temp directory suffixes within one process.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The dispatch entry point for an `extract` step.
///
/// Resolves the bundled `dependencies\7za.exe` under `install_root` and runs the
/// handler with the real child runner. The 7z child is bound to a kill-on-close
/// job object so it dies with the installer.
pub fn extract_step(
    install_root: &Path,
    step: &Step,
    index: usize,
    on_progress: &mut dyn FnMut(ProgressEvent),
) -> Result<()> {
    let sevenzip = resolved_paths(install_root).deps.join(SEVENZIP_EXE);
    let runner = extract_runner()?;
    extract_step_with(&runner, &sevenzip, install_root, step, index, on_progress)
}

/// Builds the real child runner for an extract step, with its 7z child bound to a
/// kill-on-close job object.
fn extract_runner() -> Result<RealRunner> {
    let job = JobHandle::new_kill_on_close()?;
    Ok(RealRunner { job: Some(job) })
}

/// Extracts `step`'s archive into its target using the `sevenzip` binary, run
/// through `runner`.
///
/// This is the testable core of [`extract_step`]: the 7z path and the child
/// runner are injected.
pub fn extract_step_with(
    runner: &dyn ChildRunner,
    sevenzip: &Path,
    install_root: &Path,
    step: &Step,
    index: usize,
    on_progress: &mut dyn FnMut(ProgressEvent),
) -> Result<()> {
    let file = step
        .file
        .as_deref()
        .ok_or_else(|| super::step_error(step, "action 'extract' requires a 'file'"))?;
    let target_rel = step
        .target
        .as_deref()
        .ok_or_else(|| super::step_error(step, "action 'extract' requires a 'target'"))?;

    let archive = resolve_archive(install_root, file);
    let target = resolve(install_root, target_rel);
    log::debug!(
        target: "pipeline:extract_step",
        "extract step '{}': archive {} -> target {}",
        step.name,
        archive.display(),
        target.display(),
    );

    // Create the target (and any parents) before extracting.
    std::fs::create_dir_all(&target)
        .map_err(|source| io_error(step, &target, "create extract target", source))?;

    match &step.from {
        // No `from:` -> extract straight into the target.
        None => {
            let mut throttle = ProgressThrottle::new();
            sevenzip::extract(runner, sevenzip, &archive, &target, &mut |progress| {
                throttle.report(index, progress, on_progress);
            })?;
        }
        // `from:` -> extract to a same-drive temp dir, then move the stripped
        // sub-path contents into the target.
        Some(from) => {
            let tmp_root = resolved_paths(install_root).tmp;
            std::fs::create_dir_all(&tmp_root).map_err(|source| {
                io_error(step, &tmp_root, "create same-drive temp dir", source)
            })?;
            let tmp_extract = unique_extract_dir(&tmp_root);
            std::fs::create_dir_all(&tmp_extract).map_err(|source| {
                io_error(
                    step,
                    &tmp_extract,
                    "create same-drive temp extract dir",
                    source,
                )
            })?;

            let mut throttle = ProgressThrottle::new();
            let extracted =
                sevenzip::extract(runner, sevenzip, &archive, &tmp_extract, &mut |progress| {
                    throttle.report(index, progress, on_progress)
                });
            if let Err(error) = extracted {
                let _ = std::fs::remove_dir_all(&tmp_extract);
                return Err(error);
            }

            // The extraction finished; renaming the stripped sub-paths into the
            // target is indeterminate (no per-file percentage from 7z).
            on_progress(ProgressEvent::StepProgress {
                index,
                percent: None,
                detail: Some("Moving extracted files…".to_string()),
            });

            let moved = move_from_paths(step, &tmp_extract, &target, from);
            // The temp tree is throwaway; never fail the step because cleanup did.
            let _ = std::fs::remove_dir_all(&tmp_extract);
            moved?;
        }
    }

    Ok(())
}

/// Throttles 7z progress records to at most ~10 UI updates per second and only
/// when the reported percentage changes.
///
/// 7z emits a record per file and per percent tick; forwarding every one would
/// flood the UI thread's channel during a multi-GB extract.
struct ProgressThrottle {
    last_percent: Option<u32>,
    last_emit: Option<std::time::Instant>,
}

impl ProgressThrottle {
    fn new() -> Self {
        Self {
            last_percent: None,
            last_emit: None,
        }
    }

    /// Reports one 7z progress record as a [`ProgressEvent::StepProgress`],
    /// unless it is unchanged or arrived less than ~100 ms after the last one.
    fn report(
        &mut self,
        index: usize,
        progress: SevenZipProgress,
        on_progress: &mut dyn FnMut(ProgressEvent),
    ) {
        let now = std::time::Instant::now();
        let changed = progress.percent != self.last_percent;
        let too_soon = self
            .last_emit
            .is_some_and(|last| now.duration_since(last).as_millis() < 100);
        if !changed || too_soon {
            return;
        }
        self.last_percent = progress.percent;
        self.last_emit = Some(now);
        let detail = match (progress.percent, progress.file.as_deref()) {
            (Some(pct), Some(file)) => Some(format!("Extracting {pct}% | {file}")),
            (Some(pct), None) => Some(format!("Extracting {pct}%")),
            (None, Some(file)) => Some(format!("Extracting {file}")),
            (None, None) => None,
        };
        on_progress(ProgressEvent::StepProgress {
            index,
            percent: progress.percent,
            detail,
        });
    }
}

/// Resolves the configured archive path, redirecting a multipart base name to
/// its `.001` volume.
///
/// A step naming the base archive (`foo.7z`) whose volume set is `foo.7z.001`,
/// `.002`, ... is redirected to `foo.7z.001`, because 7z follows the remaining
/// volumes from the first one. If neither the named file nor a `.001` volume
/// exists the named path is returned unchanged, so 7z produces its own (fatal)
/// missing-archive error.
fn resolve_archive(install_root: &Path, file: &str) -> PathBuf {
    let archive = resolve(install_root, file);
    if archive.exists() {
        return archive;
    }
    let mut first_volume = archive.clone().into_os_string();
    first_volume.push(".001");
    let first_volume = PathBuf::from(first_volume);
    if first_volume.exists() {
        first_volume
    } else {
        archive
    }
}

/// A unique, empty temp directory name under `tmp_root` for one `from:` extract.
///
/// The `.tmp\` root is shared by every `from:` step in a run (and overwritten by
/// `-aoa`), so each extract gets its own sub-directory; the name combines the
/// process id, wall-clock nanos and a process-local counter so concurrent runs
/// and parallel tests never collide.
fn unique_extract_dir(tmp_root: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    tmp_root.join(format!("extract-{}-{nanos}-{seq}", std::process::id()))
}

/// Moves the contents of every `from` sub-path in the freshly extracted temp
/// tree into `target`, stripping the sub-path prefix.
///
/// A `from` entry is a path inside the archive (`Bhs Hud`, `anomaly/all_game_files`).
/// If it names a directory, that directory's entries are moved into `target`
/// (recursively merging sub-directories and overwriting files); if it names a
/// file, the file is moved into `target`. An entry missing from the archive is an
/// explicit fatal error rather than a silent no-op.
fn move_from_paths(step: &Step, tmp_extract: &Path, target: &Path, from: &[String]) -> Result<()> {
    for sub in from {
        let src = resolve(tmp_extract, sub.as_str());
        let metadata = std::fs::symlink_metadata(&src).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                super::step_error(step, format!("archive has no 'from' sub-path '{sub}'"))
            } else {
                io_error(step, &src, "inspect 'from' sub-path", source)
            }
        })?;

        if metadata.is_dir() {
            move_dir_contents(step, &src, target)?;
            // The source directory is now empty; drop it so no prefix is left.
            std::fs::remove_dir(&src).map_err(|source| {
                io_error(step, &src, "remove emptied 'from' directory", source)
            })?;
        } else {
            let name = src.file_name().ok_or_else(|| {
                super::step_error(
                    step,
                    format!("'from' sub-path '{}' has no file name", src.display()),
                )
            })?;
            rename_replace(step, &src, &target.join(name))?;
        }
    }
    Ok(())
}

/// Recursively moves the entries of `src_dir` into `dst_dir`, creating/merging
/// `dst_dir` and overwriting existing files (same-volume renames).
fn move_dir_contents(step: &Step, src_dir: &Path, dst_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dst_dir)
        .map_err(|source| io_error(step, dst_dir, "create moved directory", source))?;

    let entries = std::fs::read_dir(src_dir)
        .map_err(|source| io_error(step, src_dir, "read extracted directory", source))?;
    for entry in entries {
        let entry =
            entry.map_err(|source| io_error(step, src_dir, "read extracted entry", source))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|source| io_error(step, &path, "inspect extracted entry", source))?;
        let dest = dst_dir.join(entry.file_name());

        if file_type.is_dir() {
            move_dir_contents(step, &path, &dest)?;
            std::fs::remove_dir(&path)
                .map_err(|source| io_error(step, &path, "remove emptied directory", source))?;
        } else {
            rename_replace(step, &path, &dest)?;
        }
    }
    Ok(())
}

/// Renames `src` onto `dst`, replacing an existing destination file.
///
/// `std::fs::rename` is a same-volume move and, on Windows, replaces an existing
/// file.
fn rename_replace(step: &Step, src: &Path, dst: &Path) -> Result<()> {
    std::fs::rename(src, dst).map_err(|source| AppError::Io {
        context: format!("step '{}': move extracted content into target", step.name),
        path: Some(dst.to_path_buf()),
        source,
    })
}

/// Builds an [`AppError::Io`] naming the step and the offending path.
fn io_error(step: &Step, path: &Path, what: &str, source: std::io::Error) -> AppError {
    AppError::Io {
        context: format!("step '{}': {what}", step.name),
        path: Some(path.to_path_buf()),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// A unique directory under the system temp dir.
    fn temp_dir(prefix: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
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

    /// A step with every optional field empty; tests override what they need.
    fn step_for(name: &str, action: &str) -> Step {
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

    /// Builds `dest` with `7z a -t7z [-v1k] <dest> <entries>` (cwd = `cwd`).
    fn archive(exe: &Path, cwd: &Path, dest: &Path, split: bool, entries: &[&str]) {
        let mut cmd = Command::new(exe);
        cmd.current_dir(cwd).arg("a").arg("-t7z");
        if split {
            cmd.arg("-v1k");
        }
        cmd.arg(dest).args(entries);
        let status = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn the bundled 7z to build the test archive");
        assert!(
            status.success(),
            "bundled 7z failed to build the archive: {status}"
        );
    }

    /// A deterministic, effectively incompressible byte blob (fixed-seed LCG),
    /// so a 1 KiB split volume set spans more than one volume.
    fn lcg_blob(len: usize) -> Vec<u8> {
        let mut state: u32 = 123_456_789;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            out.push((state >> 16) as u8);
        }
        out
    }

    /// Whether the same-drive `.tmp\` root has no leftover extract directory.
    fn temp_is_clean(sandbox: &Path) -> bool {
        match std::fs::read_dir(sandbox.join(".tmp")) {
            Ok(entries) => entries.count() == 0,
            Err(_) => true,
        }
    }

    /// Regression: the production extract entry point must bind its 7z child to a
    /// kill-on-close job object, like download and verify_game do. A jobless runner
    /// here orphans a multi-GB 7z write if the installer dies mid-extract.
    #[test]
    fn production_extract_runner_binds_a_kill_on_close_job() {
        let runner = extract_runner().expect("job creation must succeed");
        assert!(
            runner.job.is_some(),
            "the production extract runner must carry a kill-on-close job"
        );
    }

    #[test]
    fn integration_from_extracts_stripped_contents_into_target() {
        let sandbox = temp_dir("zona_installer_extract_from");
        let src = sandbox.join("src");
        std::fs::create_dir_all(src.join("gamefiles/sub")).expect("create source tree");
        std::fs::write(src.join("gamefiles/a.txt"), b"alpha\n").expect("write a.txt");
        std::fs::write(src.join("gamefiles/sub/b.txt"), b"beta\n").expect("write b.txt");
        std::fs::write(src.join("other.txt"), b"other\n").expect("write other.txt");

        let exe = bundled_sevenzip();
        let downloads = sandbox.join("downloads");
        std::fs::create_dir_all(&downloads).expect("create downloads dir");
        archive(
            &exe,
            &src,
            &downloads.join("nested.7z"),
            false,
            &["gamefiles", "other.txt"],
        );

        // A nested target proves the parent dirs are created as needed.
        let step = Step {
            file: Some("downloads/nested.7z".to_string()),
            target: Some("nested/deep/anomaly".to_string()),
            from: Some(vec!["gamefiles".to_string()]),
            ..step_for("install_gamefiles", "extract")
        };

        extract_step_with(
            &RealRunner { job: None },
            &exe,
            &sandbox,
            &step,
            0,
            &mut |_| {},
        )
        .expect("a from: extract must succeed");

        let target = sandbox.join("nested/deep/anomaly");
        assert_eq!(
            std::fs::read_to_string(target.join("a.txt")).expect("read a.txt"),
            "alpha\n"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("sub/b.txt")).expect("read sub/b.txt"),
            "beta\n"
        );
        assert!(
            !target.join("gamefiles").exists(),
            "the from prefix folder must be stripped"
        );
        assert!(
            !target.join("other.txt").exists(),
            "only the from: sub-path is moved into the target"
        );
        assert!(
            temp_is_clean(&sandbox),
            "the same-drive temp extract dir must be removed"
        );

        let _ = std::fs::remove_dir_all(&sandbox);
    }

    #[test]
    fn integration_multipart_001_extracts_the_whole_volume_set() {
        let sandbox = temp_dir("zona_installer_extract_multipart");
        let src = sandbox.join("src");
        std::fs::create_dir_all(src.join("payload")).expect("create source tree");
        std::fs::write(src.join("readme.txt"), b"zona tiny archive\n").expect("write readme.txt");
        std::fs::write(src.join("payload/file_a.txt"), b"alpha\n").expect("write file_a.txt");
        let blob = lcg_blob(2000);
        std::fs::write(src.join("payload/blob.bin"), &blob).expect("write blob.bin");

        let exe = bundled_sevenzip();
        let downloads = sandbox.join("downloads");
        std::fs::create_dir_all(&downloads).expect("create downloads dir");
        archive(
            &exe,
            &src,
            &downloads.join("multipart.7z"),
            true,
            &["readme.txt", "payload"],
        );
        assert!(
            downloads.join("multipart.7z.001").is_file(),
            "the split archive must produce a .001 volume"
        );
        assert!(
            downloads.join("multipart.7z.002").is_file(),
            "the split archive must actually span more than one volume"
        );

        let step = Step {
            file: Some("downloads/multipart.7z.001".to_string()),
            target: Some("out".to_string()),
            ..step_for("extract_multipart", "extract")
        };

        extract_step_with(
            &RealRunner { job: None },
            &exe,
            &sandbox,
            &step,
            0,
            &mut |_| {},
        )
        .expect("a multipart extract from the .001 volume must succeed");

        let target = sandbox.join("out");
        assert_eq!(
            std::fs::read_to_string(target.join("readme.txt")).expect("read readme.txt"),
            "zona tiny archive\n"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("payload/file_a.txt")).expect("read file_a.txt"),
            "alpha\n"
        );
        assert_eq!(
            std::fs::read(target.join("payload/blob.bin")).expect("read blob.bin"),
            blob
        );

        let _ = std::fs::remove_dir_all(&sandbox);
    }

    #[test]
    fn multipart_base_name_is_redirected_to_the_001_volume() {
        let sandbox = temp_dir("zona_installer_extract_multipart_base");
        let src = sandbox.join("src");
        std::fs::create_dir_all(src.join("payload")).expect("create source tree");
        std::fs::write(src.join("readme.txt"), b"zona tiny archive\n").expect("write readme.txt");
        std::fs::write(src.join("payload/file_a.txt"), b"alpha\n").expect("write file_a.txt");
        std::fs::write(src.join("payload/blob.bin"), lcg_blob(2000)).expect("write blob.bin");

        let exe = bundled_sevenzip();
        let downloads = sandbox.join("downloads");
        std::fs::create_dir_all(&downloads).expect("create downloads dir");
        archive(
            &exe,
            &src,
            &downloads.join("multipart.7z"),
            true,
            &["readme.txt", "payload"],
        );
        // The base archive name itself must not exist; only the volumes do.
        assert!(!downloads.join("multipart.7z").exists());

        let step = Step {
            file: Some("downloads/multipart.7z".to_string()),
            target: Some("out".to_string()),
            ..step_for("extract_multipart_base", "extract")
        };

        extract_step_with(
            &RealRunner { job: None },
            &exe,
            &sandbox,
            &step,
            0,
            &mut |_| {},
        )
        .expect("naming the base archive must redirect to the .001 volume");

        assert!(sandbox.join("out/readme.txt").is_file());
        assert!(sandbox.join("out/payload/file_a.txt").is_file());

        let _ = std::fs::remove_dir_all(&sandbox);
    }

    #[test]
    fn missing_file_is_an_explicit_fatal_error() {
        let sandbox = temp_dir("zona_installer_extract_nofile");
        let step = Step {
            target: Some("out".to_string()),
            ..step_for("no_file", "extract")
        };

        let error = extract_step_with(
            &RealRunner { job: None },
            &bundled_sevenzip(),
            &sandbox,
            &step,
            0,
            &mut |_| {},
        )
        .expect_err("an extract step without 'file' must fail");
        assert_eq!(error.error_id().as_str(), "E_PIPELINE_STEP");

        let _ = std::fs::remove_dir_all(&sandbox);
    }

    #[test]
    fn missing_target_is_an_explicit_fatal_error() {
        let sandbox = temp_dir("zona_installer_extract_notarget");
        let step = Step {
            file: Some("downloads/anomaly.7z".to_string()),
            ..step_for("no_target", "extract")
        };

        let error = extract_step_with(
            &RealRunner { job: None },
            &bundled_sevenzip(),
            &sandbox,
            &step,
            0,
            &mut |_| {},
        )
        .expect_err("an extract step without 'target' must fail");
        assert_eq!(error.error_id().as_str(), "E_PIPELINE_STEP");

        let _ = std::fs::remove_dir_all(&sandbox);
    }

    #[test]
    fn missing_from_sub_path_is_an_explicit_fatal_error() {
        let sandbox = temp_dir("zona_installer_extract_badfrom");
        let src = sandbox.join("src");
        std::fs::create_dir_all(src.join("gamefiles")).expect("create source tree");
        std::fs::write(src.join("gamefiles/a.txt"), b"alpha\n").expect("write a.txt");

        let exe = bundled_sevenzip();
        let downloads = sandbox.join("downloads");
        std::fs::create_dir_all(&downloads).expect("create downloads dir");
        archive(
            &exe,
            &src,
            &downloads.join("nested.7z"),
            false,
            &["gamefiles"],
        );

        let step = Step {
            file: Some("downloads/nested.7z".to_string()),
            target: Some("out".to_string()),
            from: Some(vec!["does_not_exist".to_string()]),
            ..step_for("bad_from", "extract")
        };

        let error = extract_step_with(
            &RealRunner { job: None },
            &exe,
            &sandbox,
            &step,
            0,
            &mut |_| {},
        )
        .expect_err("a missing from: sub-path must be a fatal error");
        assert_eq!(error.error_id().as_str(), "E_PIPELINE_STEP");
        assert!(
            temp_is_clean(&sandbox),
            "the same-drive temp dir must be cleaned up even on failure"
        );

        let _ = std::fs::remove_dir_all(&sandbox);
    }
}
