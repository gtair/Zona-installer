//! Self-update: check for a newer GitHub release and hand off to the bundled
//! PowerShell update script.
//!
//! The check runs only from a canonical shipped install (post-relocation). When
//! a strictly newer release tag exists, the installer spawns
//! `dependencies\update.ps1`, which waits for this process to exit (releasing
//! the running exe's lock), downloads the release zip into `installer\downloads\`,
//! extracts it over `<drive>:\Zona`, and relaunches the new installer with the
//! original flags. The console window the script opens is the "updating" UI —
//! there is no in-app spinner.
//!
//! The version comparison lives here (the configured version against the
//! release tag, both normalized by stripping a leading `v`); the download and
//! extract are delegated to PowerShell so a ~5MB update needs no bundled HTTP
//! or archive tooling.

use std::cmp::Ordering;
use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use crate::error::{AppError, Result};
use crate::preflight::DebugOptions;

/// The User-Agent GitHub requires on API requests.
const USER_AGENT: &str = "zona-installer";

/// The bundled update script, relative to `dependencies\`.
const UPDATE_SCRIPT: &str = "update.ps1";

/// Strips a single leading `v`/`V` from a version tag (pure).
pub fn strip_v(version: &str) -> &str {
    version
        .strip_prefix('v')
        .or_else(|| version.strip_prefix('V'))
        .unwrap_or(version)
}

/// Whether `tag` names a strictly newer version than `current` (pure).
///
/// Both may be `v`-prefixed or bare; the prefix is stripped before comparing.
pub fn is_newer(tag: &str, current: &str) -> bool {
    compare_versions(strip_v(tag), strip_v(current)) == Ordering::Greater
}

/// Compares two dot-separated version strings (pure).
///
/// Numeric segments compare numerically (`0.2` > `0.10`); a missing trailing
/// segment counts as `0` (`1.0` == `1.0.0`); a non-numeric segment falls back to
/// lexical comparison.
fn compare_versions(a: &str, b: &str) -> Ordering {
    let mut a = a.split('.');
    let mut b = b.split('.');
    loop {
        match (a.next(), b.next()) {
            (Some(x), Some(y)) => {
                let ordering = compare_segment(x, y);
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            (Some(x), None) => {
                if !segment_is_zero(x) {
                    return Ordering::Greater;
                }
            }
            (None, Some(y)) => {
                if !segment_is_zero(y) {
                    return Ordering::Less;
                }
            }
            (None, None) => return Ordering::Equal,
        }
    }
}

fn segment_is_zero(segment: &str) -> bool {
    segment.parse::<u64>().map_or(false, |value| value == 0)
}

fn compare_segment(a: &str, b: &str) -> Ordering {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        _ => a.cmp(b),
    }
}

/// One usable release: its tag and the `.zip` asset URL to download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Release {
    tag: String,
    zip_url: String,
}

/// Fetches the latest release and its `.zip` asset (impure, best effort).
///
/// `repo` is the `owner/repo` to query. A network or parse failure is treated
/// as "no update", never a fatal error: the caller simply proceeds with the
/// normal flow. Returns `None` when the check cannot complete or no `.zip`
/// asset exists.
pub(crate) fn check_for_update(repo: &str) -> Option<Release> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let mut response = match ureq::get(&url).header("User-Agent", USER_AGENT).call() {
        Ok(response) => response,
        Err(error) => {
            log::warn!(target: "update", "update check failed: {error}");
            return None;
        }
    };
    let body = match response.body_mut().read_to_string() {
        Ok(body) => body,
        Err(error) => {
            log::warn!(target: "update", "read update response failed: {error}");
            return None;
        }
    };
    match parse_release(&body) {
        Some(release) => Some(release),
        None => {
            log::warn!(target: "update", "no .zip asset in the latest release");
            None
        }
    }
}

/// Parses a GitHub `releases/latest` JSON body into its tag and `.zip` asset
/// URL (pure).
fn parse_release(json: &str) -> Option<Release> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let tag = value.get("tag_name")?.as_str()?.to_string();
    let zip_url = value
        .get("assets")?
        .as_array()?
        .iter()
        .filter_map(|asset| asset.get("browser_download_url")?.as_str())
        .find(|url| url.to_ascii_lowercase().ends_with(".zip"))?
        .to_string();
    Some(Release { tag, zip_url })
}

/// The relaunch flags to preserve through a self-update (pure).
///
/// Keeps `--launcher` and the debug flags. `/delete` is consumed earlier by
/// preflight and deliberately not forwarded.
pub fn relaunch_args(launcher: bool, debug: DebugOptions) -> Vec<OsString> {
    let mut args = Vec::new();
    if launcher {
        args.push(OsString::from(crate::preflight::LAUNCHER_FLAG));
    }
    if debug.debug_console && debug.log_debug {
        args.push(OsString::from("--debug"));
    } else {
        if debug.debug_console {
            args.push(OsString::from("--debug-console"));
        }
        if debug.log_debug {
            args.push(OsString::from("--log-debug"));
        }
    }
    args
}

/// Joins the relaunch flags into one space-separated string for the script's
/// `-Relaunch` argument (pure). All flags are space-free, so the join is safe.
pub fn relaunch_args_string(launcher: bool, debug: DebugOptions) -> String {
    relaunch_args(launcher, debug)
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Spawns the update script and returns once it has started (impure).
///
/// The caller must exit immediately after this so the script's wait on the
/// process id succeeds and the exe lock is released.
fn spawn_update(exe_dir: &Path, release: &Release, relaunch: &str) -> Result<()> {
    let script = exe_dir.join("dependencies").join(UPDATE_SCRIPT);
    let zona = exe_dir.parent().unwrap_or(exe_dir);

    let mut command = Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .arg("-InstallerPid")
        .arg(std::process::id().to_string())
        .arg("-Url")
        .arg(&release.zip_url)
        .arg("-Dest")
        .arg(zona)
        .arg("-Relaunch")
        .arg(relaunch);

    command.spawn().map_err(|source| AppError::Io {
        context: "spawn update script".to_string(),
        path: Some(script),
        source,
    })?;
    Ok(())
}

/// Runs the self-update check from a canonical shipped install.
///
/// `current_version` is the installer version to compare against and `repo` is
/// the GitHub `owner/repo` to query, both read from `config.yaml`.
///
/// Returns `true` when an update was handed off to the script and the caller
/// must exit immediately; `false` otherwise (no update, offline, or a failed
/// hand-off — all of which let the installer continue normally).
pub fn maybe_self_update(
    exe_dir: &Path,
    launcher: bool,
    debug: DebugOptions,
    current_version: &str,
    repo: &str,
) -> bool {
    let Some(release) = check_for_update(repo) else {
        return false;
    };
    if !is_newer(&release.tag, current_version) {
        log::info!(
            target: "update",
            "up to date ({current_version}; latest {})",
            release.tag
        );
        return false;
    }
    log::info!(
        target: "update",
        "update available: {current_version} -> {}",
        release.tag
    );
    let relaunch = relaunch_args_string(launcher, debug);
    match spawn_update(exe_dir, &release, &relaunch) {
        Ok(()) => true,
        Err(error) => {
            // A failed hand-off must not block an otherwise-working install.
            log::warn!(target: "update", "update hand-off failed: {error}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_v_removes_a_single_leading_v() {
        assert_eq!(strip_v("v0.2.0"), "0.2.0");
        assert_eq!(strip_v("V1.0.0"), "1.0.0");
        assert_eq!(strip_v("0.2.0"), "0.2.0");
        assert_eq!(strip_v("1.2.3"), "1.2.3");
    }

    #[test]
    fn compare_versions_orders_numerically_not_lexically() {
        assert_eq!(compare_versions("0.2.0", "0.10.0"), Ordering::Less);
        assert_eq!(compare_versions("0.10.0", "0.2.0"), Ordering::Greater);
        assert_eq!(compare_versions("1.0", "1.0.0"), Ordering::Equal);
        assert_eq!(compare_versions("1.0.1", "1.0"), Ordering::Greater);
        assert_eq!(compare_versions("0.1.9", "0.2.0"), Ordering::Less);
    }

    #[test]
    fn is_newer_detects_strictly_newer_tags() {
        assert!(is_newer("v0.2.0", "v0.1.9"));
        assert!(!is_newer("v0.1.9", "v0.2.0"));
        assert!(!is_newer("v0.2.0", "v0.2.0"));
        // A bare tag against a prefixed current still compares.
        assert!(is_newer("0.3.0", "v0.2.0"));
    }

    #[test]
    fn parse_release_extracts_the_tag_and_zip_asset() {
        let json = r#"{
            "tag_name": "v0.3.0",
            "assets": [
                { "browser_download_url": "https://example.com/SHA256SUMS" },
                { "browser_download_url": "https://example.com/zona-installer.zip" }
            ]
        }"#;
        let release = parse_release(json).expect("parse release");
        assert_eq!(release.tag, "v0.3.0");
        assert_eq!(release.zip_url, "https://example.com/zona-installer.zip");
    }

    #[test]
    fn parse_release_rejects_a_body_without_a_zip_asset() {
        let json = r#"{
            "tag_name": "v0.3.0",
            "assets": [{ "browser_download_url": "https://example.com/SHA256SUMS" }]
        }"#;
        assert!(parse_release(json).is_none());
    }

    #[test]
    fn relaunch_args_preserves_the_launcher_and_debug_flags() {
        let none = DebugOptions::default();
        assert_eq!(relaunch_args(false, none), Vec::<OsString>::new());

        let launcher = DebugOptions::default();
        assert_eq!(
            relaunch_args(true, launcher),
            vec![OsString::from("--launcher")]
        );

        let both = DebugOptions {
            debug_console: true,
            log_debug: true,
        };
        assert_eq!(
            relaunch_args(false, both),
            vec![OsString::from("--debug")]
        );

        let split = DebugOptions {
            debug_console: true,
            log_debug: false,
        };
        assert_eq!(
            relaunch_args(false, split),
            vec![OsString::from("--debug-console")]
        );
    }

    #[test]
    fn relaunch_args_string_joins_the_flags() {
        let debug = DebugOptions {
            debug_console: true,
            log_debug: false,
        };
        assert_eq!(
            relaunch_args_string(true, debug),
            "--launcher --debug-console"
        );
        assert_eq!(relaunch_args_string(false, DebugOptions::default()), "");
    }
}
