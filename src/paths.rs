//! Install-root resolution and derived paths.
//!
//! Paths are relative to the running binary's directory: the shipped install
//! root is `<drive>:\Zona\installer`, so `downloads/...` lands inside it while
//! `../anomaly` / `../mo2` resolve to `<drive>:\Zona\anomaly` / `<drive>:\Zona\mo2`.
//!
//! In debug builds the `ZONA_INSTALLER_DEV_ROOT` env var overrides the root;
//! under that override relocation is disabled ([`RootMode::relocation_disabled`]).

use std::path::{Component, Path, PathBuf};

/// Install-root mode: the shipped `<drive>:\Zona\installer`, or a debug override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootMode {
    /// Normal shipped layout: the install root is the binary's own directory.
    Shipped,
    /// Debug-only override: the contained directory is the install root.
    DevOverride(PathBuf),
}

impl RootMode {
    /// Whether relocation and the self-delete-the-old-folder trick are disabled.
    pub fn relocation_disabled(&self) -> bool {
        matches!(self, RootMode::DevOverride(_))
    }
}

/// The environment variable that overrides the install root in debug builds.
#[cfg(debug_assertions)]
const DEV_ROOT_ENV: &str = "ZONA_INSTALLER_DEV_ROOT";

/// Detects the install-root mode for the given executable directory.
///
/// `exe_dir` is used unchanged by [`install_root`] in the shipped branch, so it
/// is unused here.
pub fn detect_root_mode(_exe_dir: &Path) -> RootMode {
    #[cfg(debug_assertions)]
    {
        root_mode_from_override(std::env::var_os(DEV_ROOT_ENV))
    }

    #[cfg(not(debug_assertions))]
    {
        RootMode::Shipped
    }
}

/// Maps a raw `ZONA_INSTALLER_DEV_ROOT` value onto a [`RootMode`].
///
/// Split out from [`detect_root_mode`] so the "empty value means unset" rule is
/// unit-testable without mutating process-global env.
#[cfg(debug_assertions)]
fn root_mode_from_override(value: Option<std::ffi::OsString>) -> RootMode {
    match value.filter(|value| !value.is_empty()) {
        Some(root) => RootMode::DevOverride(PathBuf::from(root)),
        None => RootMode::Shipped,
    }
}

/// The effective install root for `exe_dir` under `mode`.
pub fn install_root(exe_dir: &Path, mode: &RootMode) -> PathBuf {
    match mode {
        RootMode::Shipped => exe_dir.to_path_buf(),
        RootMode::DevOverride(root) => root.clone(),
    }
}

/// The Zona directory: the parent of the install root (`<drive>:\Zona`).
///
/// Falls back to the install root itself when it has no parent.
pub fn zona_dir(install_root: &Path) -> PathBuf {
    match install_root.parent() {
        Some(parent) => parent.to_path_buf(),
        None => install_root.to_path_buf(),
    }
}

/// Joins `rel` onto `install_root`, normalizing `.`/`..` components lexically.
pub fn resolve(install_root: &Path, rel: &str) -> PathBuf {
    normalize(&install_root.join(rel))
}

/// Derived directories under an install root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPaths {
    /// `installer\downloads\`.
    pub downloads: PathBuf,
    /// `installer\logs\`.
    pub logs: PathBuf,
    /// `installer\.tmp\` (same-drive temp used by `from:` extracts).
    pub tmp: PathBuf,
    /// `installer\dependencies\`.
    pub deps: PathBuf,
    /// `installer\dependencies\config.yaml`.
    pub config: PathBuf,
    /// `<Zona>\anomaly` (`../anomaly`).
    pub anomaly: PathBuf,
    /// `<Zona>\mo2` (`../mo2`).
    pub mo2: PathBuf,
}

/// Computes the derived paths for `install_root`.
pub fn resolved_paths(install_root: &Path) -> ResolvedPaths {
    ResolvedPaths {
        downloads: resolve(install_root, "downloads"),
        logs: resolve(install_root, "logs"),
        tmp: resolve(install_root, ".tmp"),
        deps: resolve(install_root, "dependencies"),
        config: resolve(install_root, "dependencies/config.yaml"),
        anomaly: resolve(install_root, "../anomaly"),
        mo2: resolve(install_root, "../mo2"),
    }
}

/// Lexically normalizes `.` / `..` components without touching the filesystem.
///
/// `..` pops the preceding normal component; if there is nothing to pop (an
/// absolute root, or a leading `..`), the `..` is kept.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_normalizes_parent_components() {
        let root = Path::new(r"C:\Zona\installer");

        assert_eq!(resolve(root, "../mo2"), PathBuf::from(r"C:\Zona\mo2"));
        assert_eq!(
            resolve(root, "../anomaly/appdata/shaders_cache/r4"),
            PathBuf::from(r"C:\Zona\anomaly\appdata\shaders_cache\r4")
        );
        assert_eq!(
            resolve(root, "downloads/ModOrganizer.ini.template"),
            PathBuf::from(r"C:\Zona\installer\downloads\ModOrganizer.ini.template")
        );
        assert_eq!(
            resolve(root, r"..\anomaly\bin\AnomalyDX11AVX.exe"),
            PathBuf::from(r"C:\Zona\anomaly\bin\AnomalyDX11AVX.exe")
        );
    }

    #[test]
    fn resolve_keeps_plain_relative_paths_under_root() {
        let root = Path::new(r"C:\Zona\installer");

        assert_eq!(
            resolve(root, "downloads/zona_multipart.7z.001"),
            PathBuf::from(r"C:\Zona\installer\downloads\zona_multipart.7z.001")
        );
    }

    #[test]
    fn resolve_normalizes_cur_and_parent_components() {
        let root = Path::new(r"C:\Zona\installer");

        assert_eq!(
            resolve(root, "./sub/../sibling/file.txt"),
            PathBuf::from(r"C:\Zona\installer\sibling\file.txt")
        );
    }

    #[test]
    fn zona_dir_returns_the_parent() {
        assert_eq!(
            zona_dir(Path::new(r"C:\Zona\installer")),
            PathBuf::from(r"C:\Zona")
        );
        // A drive root has no parent: fall back to itself.
        assert_eq!(zona_dir(Path::new(r"C:\")), PathBuf::from(r"C:\"));
    }

    #[test]
    fn resolved_paths_derive_config_under_dependencies() {
        let root = Path::new(r"C:\Zona\installer");
        let paths = resolved_paths(root);

        assert_eq!(paths.deps, PathBuf::from(r"C:\Zona\installer\dependencies"));
        assert_eq!(
            paths.config,
            PathBuf::from(r"C:\Zona\installer\dependencies\config.yaml")
        );
        assert_eq!(
            paths.downloads,
            PathBuf::from(r"C:\Zona\installer\downloads")
        );
        assert_eq!(paths.logs, PathBuf::from(r"C:\Zona\installer\logs"));
        assert_eq!(paths.tmp, PathBuf::from(r"C:\Zona\installer\.tmp"));
        assert_eq!(paths.anomaly, PathBuf::from(r"C:\Zona\anomaly"));
        assert_eq!(paths.mo2, PathBuf::from(r"C:\Zona\mo2"));
    }

    #[test]
    fn dev_override_sets_install_root_and_disables_relocation() {
        let exe_dir = PathBuf::from(r"D:\zona-dev\target\debug");
        let temp = PathBuf::from(r"D:\zona-dev");
        let mode = RootMode::DevOverride(temp.clone());

        assert_eq!(install_root(&exe_dir, &mode), temp);
        assert!(mode.relocation_disabled());
    }

    #[test]
    fn shipped_install_root_is_exe_dir_and_relocation_is_enabled() {
        let exe_dir = PathBuf::from(r"C:\Zona\installer");
        let mode = RootMode::Shipped;

        assert_eq!(install_root(&exe_dir, &mode), exe_dir);
        assert!(!mode.relocation_disabled());
    }

    #[cfg(debug_assertions)]
    #[test]
    fn debug_override_env_maps_to_dev_override() {
        assert_eq!(
            root_mode_from_override(Some(std::ffi::OsString::from(r"D:\dev\zona"))),
            RootMode::DevOverride(PathBuf::from(r"D:\dev\zona"))
        );
        // An empty value counts as unset.
        assert_eq!(
            root_mode_from_override(Some(std::ffi::OsString::new())),
            RootMode::Shipped
        );
        assert_eq!(root_mode_from_override(None), RootMode::Shipped);
    }
}
