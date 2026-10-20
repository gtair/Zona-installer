//! Config model + `config.yaml` parsing and validation.
//!
//! The runtime config lives at `dependencies\config.yaml`, deserialized with
//! `serde_yaml` and validated: every `action` must be one the pipeline knows, and
//! an `extract` step must carry a `file`. Gating selectors accept a single id or
//! a list of ids. No gating logic lives here: this module only models and
//! validates the file.

use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;

pub use crate::error::ConfigError;
use crate::error::{AppError, Result};

/// A choice node id, e.g. `"sss23"`. `"base"` is a normal id meaning
/// "always required".
pub type ChoiceId = String;

/// A single download asset described by the config.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Asset {
    /// Stable identifier used for gating and error messages.
    pub id: String,
    /// Local filename under `downloads\`.
    pub file: String,
    /// Direct download URL.
    pub url: String,
    /// Expected SHA-256, hex; compared case-insensitively.
    pub sha256: String,
    /// Gating selector: the id(s) that make this asset required.
    pub choice: ChoiceSpec,
}

/// One node of the recursive choice tree.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ChoiceNode {
    /// Unique id within the config.
    pub id: ChoiceId,
    /// UI label.
    pub label: String,
    /// UI help text.
    pub description: String,
    /// Nested options; empty means this node is a leaf.
    #[serde(default)]
    pub choices: Vec<ChoiceNode>,
}

/// A gating selector: one id or several.
///
/// Deserialized from either a YAML scalar (`choice: sss23`) or a sequence
/// (`choice: ["sss23", "bhs_hud"]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "StringOrList")]
pub enum ChoiceSpec {
    /// A single gating id.
    One(ChoiceId),
    /// Several gating ids.
    Many(Vec<ChoiceId>),
}

impl From<StringOrList> for ChoiceSpec {
    fn from(value: StringOrList) -> Self {
        match value {
            StringOrList::One(id) => ChoiceSpec::One(id),
            StringOrList::Many(ids) => ChoiceSpec::Many(ids),
        }
    }
}

/// A directory selector: one path or several.
///
/// Deserialized from either a YAML scalar (`dir: ../mo2`) or a sequence
/// (`dir: ["downloads", "../anomaly"]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "StringOrList")]
pub enum DirSpec {
    /// A single path.
    One(String),
    /// Several paths.
    Many(Vec<String>),
}

impl From<StringOrList> for DirSpec {
    fn from(value: StringOrList) -> Self {
        match value {
            StringOrList::One(path) => DirSpec::One(path),
            StringOrList::Many(paths) => DirSpec::Many(paths),
        }
    }
}

/// One ordered install step.
///
/// Only `name` and `action` are mandatory; the remaining fields carry the
/// parameters for the action and are validated in [`parse_config`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Step {
    /// Step name (shown as `Step [i]: <name/info>` in the UI).
    pub name: String,
    /// Action to run; must be one of the known actions.
    pub action: String,
    /// Optional gating selector; absent means "always run".
    #[serde(default)]
    pub choice: Option<ChoiceSpec>,
    /// `create_dir` / `delete` target directories (first occurrence).
    #[serde(default)]
    pub dir: Option<DirSpec>,
    /// `extract` archive path (required for `extract`).
    #[serde(default)]
    pub file: Option<String>,
    /// `extract` / `template_fill` target path.
    #[serde(default)]
    pub target: Option<String>,
    /// `extract` sub-path(s) to strip when relocating archive contents.
    #[serde(default)]
    pub from: Option<Vec<String>>,
    /// `template_fill` source template path.
    #[serde(default)]
    pub source: Option<String>,
    /// `template_fill` `{{KEY}}` replacements.
    #[serde(default)]
    pub replacements: Option<BTreeMap<String, String>>,
}

/// The parsed and validated `config.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Config {
    /// Schema version; currently `1`.
    pub version: u32,
    /// Recursive user-selectable option tree.
    #[serde(default)]
    pub choices: Vec<ChoiceNode>,
    /// Downloadable assets.
    #[serde(default)]
    pub assets: Vec<Asset>,
    /// Ordered install pipeline.
    #[serde(default)]
    pub steps: Vec<Step>,
}

/// The `action` values the pipeline implements.
const KNOWN_ACTIONS: &[&str] = &[
    "create_dir",
    "extract",
    "delete",
    "verify_game",
    "template_fill",
];

/// The only `config.yaml` schema version this build understands.
const SUPPORTED_VERSION: u32 = 1;

/// Parses and validates a `config.yaml` document.
///
/// Returns [`AppError::Config`] on a YAML/schema error, an unsupported
/// `version`, an unknown `action`, or an `extract` step that has no `file`.
pub fn parse_config(input: &str) -> Result<Config> {
    let config: Config = serde_yaml::from_str(input).map_err(|err| AppError::Config {
        source: ConfigError::new(format!("failed to parse config.yaml: {err}")),
    })?;
    validate(&config)?;
    Ok(config)
}

/// Length of a SHA-256 digest in hex characters.
const SHA256_HEX_LEN: usize = 64;

/// The literal choice id that is always active (see `gating`).
const BASE_CHOICE: &str = "base";

/// Builds a config error from a message.
fn invalid(message: String) -> AppError {
    AppError::Config {
        source: ConfigError::new(message),
    }
}

/// Structural validation that cannot be expressed by the type shape alone.
///
/// The config ships with the installer, so a mistake in it is a build bug that
/// must surface at startup, not minutes into an install: every step carries the
/// fields its action needs, every path stays inside the Zona folder, every
/// `choice` selector names a node that exists, and assets are well formed and
/// unique.
fn validate(config: &Config) -> Result<()> {
    if config.version != SUPPORTED_VERSION {
        let direction = if config.version > SUPPORTED_VERSION {
            "newer"
        } else {
            "older"
        };
        return Err(invalid(format!(
            "config.yaml version {} is {direction} than this installer supports \
             (expected version {SUPPORTED_VERSION})",
            config.version
        )));
    }

    let mut choice_ids = HashSet::new();
    collect_choice_ids(&config.choices, &mut choice_ids)?;

    validate_assets(config, &choice_ids)?;
    for step in &config.steps {
        validate_step(step, &choice_ids)?;
    }
    Ok(())
}

/// Collects every choice-node id (ids may repeat across branches by design).
fn collect_choice_ids<'a>(nodes: &'a [ChoiceNode], out: &mut HashSet<&'a str>) -> Result<()> {
    for node in nodes {
        if node.id.trim().is_empty() {
            return Err(invalid("a choice node has an empty 'id'".to_string()));
        }
        out.insert(node.id.as_str());
        collect_choice_ids(&node.choices, out)?;
    }
    Ok(())
}

/// Checks that every id in a gating selector exists (or is `base`) and that a
/// list selector is not empty (an empty list could never be active).
fn validate_choice_spec(owner: &str, spec: &ChoiceSpec, ids: &HashSet<&str>) -> Result<()> {
    let list: &[ChoiceId] = match spec {
        ChoiceSpec::One(id) => std::slice::from_ref(id),
        ChoiceSpec::Many(list) => list.as_slice(),
    };
    if list.is_empty() {
        return Err(invalid(format!("{owner}: 'choice' is an empty list")));
    }
    for id in list {
        if id != BASE_CHOICE && !ids.contains(id.as_str()) {
            return Err(invalid(format!(
                "{owner}: 'choice' names '{id}', which is not a node in the choice tree"
            )));
        }
    }
    Ok(())
}

fn validate_assets(config: &Config, ids: &HashSet<&str>) -> Result<()> {
    let mut seen_ids = HashSet::new();
    let mut seen_files = HashSet::new();
    for asset in &config.assets {
        let owner = format!("asset '{}'", asset.id);
        if asset.id.trim().is_empty() {
            return Err(invalid("an asset has an empty 'id'".to_string()));
        }
        if !seen_ids.insert(asset.id.as_str()) {
            return Err(invalid(format!("{owner}: duplicate asset id")));
        }
        // `file` is a bare name under `downloads\` (also aria2c's `out=`).
        let bare = !asset.file.is_empty()
            && asset.file != "."
            && asset.file != ".."
            && !asset.file.contains(['/', '\\', ':']);
        if !bare {
            return Err(invalid(format!(
                "{owner}: 'file' must be a plain file name, got '{}'",
                asset.file
            )));
        }
        // Windows file names are case-insensitive.
        if !seen_files.insert(asset.file.to_ascii_lowercase()) {
            return Err(invalid(format!(
                "{owner}: another asset already downloads to '{}'",
                asset.file
            )));
        }
        if asset.url.trim().is_empty() {
            return Err(invalid(format!("{owner}: 'url' is empty")));
        }
        if asset.sha256.len() != SHA256_HEX_LEN
            || !asset.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid(format!(
                "{owner}: 'sha256' must be {SHA256_HEX_LEN} hex characters"
            )));
        }
        validate_choice_spec(&owner, &asset.choice, ids)?;
    }
    Ok(())
}

fn validate_step(step: &Step, ids: &HashSet<&str>) -> Result<()> {
    let owner = format!("step '{}'", step.name);
    if !KNOWN_ACTIONS.contains(&step.action.as_str()) {
        return Err(invalid(format!(
            "step '{}' has unknown action '{}'",
            step.name, step.action
        )));
    }
    if let Some(spec) = &step.choice {
        validate_choice_spec(&owner, spec, ids)?;
    }

    match step.action.as_str() {
        "extract" => {
            match step.file.as_deref() {
                None => {
                    return Err(invalid(format!(
                        "step '{}' has action 'extract' but no 'file'",
                        step.name
                    )));
                }
                Some("") => {
                    return Err(invalid(format!(
                        "step '{}' has action 'extract' but an empty 'file'",
                        step.name
                    )));
                }
                Some(_) => {}
            }
            check_path(&owner, "file", step.file.as_deref(), false)?;
            check_path(
                &owner,
                "target",
                require(&owner, "target", &step.target)?,
                false,
            )?;
            if let Some(from) = &step.from {
                for entry in from {
                    check_archive_subpath(&owner, entry)?;
                }
            }
        }
        "create_dir" | "delete" => {
            let dirs: &[String] = match &step.dir {
                Some(DirSpec::One(path)) => std::slice::from_ref(path),
                Some(DirSpec::Many(paths)) => paths.as_slice(),
                None => {
                    return Err(invalid(format!(
                        "{owner}: action '{}' requires 'dir'",
                        step.action
                    )));
                }
            };
            if dirs.is_empty() {
                return Err(invalid(format!("{owner}: 'dir' is an empty list")));
            }
            // A delete must never be able to remove the install root or the Zona
            // folder itself, only something below them.
            let strictly_below = step.action == "delete";
            for dir in dirs {
                check_path(&owner, "dir", Some(dir.as_str()), strictly_below)?;
            }
        }
        "template_fill" => {
            check_path(
                &owner,
                "source",
                require(&owner, "source", &step.source)?,
                false,
            )?;
            check_path(
                &owner,
                "target",
                require(&owner, "target", &step.target)?,
                false,
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// A required optional field.
fn require<'a>(owner: &str, field: &str, value: &'a Option<String>) -> Result<Option<&'a str>> {
    match value.as_deref() {
        Some(text) if !text.trim().is_empty() => Ok(Some(text)),
        _ => Err(invalid(format!("{owner}: '{field}' is required"))),
    }
}

/// Lexically confines a config path to the Zona folder (the install root's
/// parent) and everything below it.
///
/// Paths are relative to the install root (`<Zona>\installer`), so at most ONE
/// leading `..` is allowed (`../anomaly`); absolute and drive-qualified paths are
/// refused. With `strictly_below`, the path must also name something below the
/// install root or the Zona folder, never either of those themselves.
fn check_path(owner: &str, field: &str, value: Option<&str>, strictly_below: bool) -> Result<()> {
    let path = value.unwrap_or("");
    let bad = |why: &str| invalid(format!("{owner}: '{field}' value '{path}' {why}"));
    if path.trim().is_empty() {
        return Err(bad("is empty"));
    }
    let bytes = path.as_bytes();
    if path.starts_with(['/', '\\']) || (bytes.len() >= 2 && bytes[1] == b':') {
        return Err(bad(
            "must be relative (no absolute or drive-qualified paths)",
        ));
    }
    let mut ups = 0usize;
    let mut names: Vec<&str> = Vec::new();
    for component in path.split(['/', '\\']) {
        match component {
            "" | "." => {}
            ".." => {
                if names.pop().is_none() {
                    ups += 1;
                    if ups > 1 {
                        return Err(bad("escapes the Zona folder"));
                    }
                }
            }
            name => names.push(name),
        }
    }
    if strictly_below && names.is_empty() {
        return Err(bad(
            "must name a folder below the install root or the Zona folder",
        ));
    }
    Ok(())
}

/// A `from` entry is a path INSIDE an archive: relative, no `..`.
fn check_archive_subpath(owner: &str, entry: &str) -> Result<()> {
    let bad = |why: &str| invalid(format!("{owner}: 'from' entry '{entry}' {why}"));
    if entry.trim().is_empty() {
        return Err(bad("is empty"));
    }
    let bytes = entry.as_bytes();
    if entry.starts_with(['/', '\\']) || (bytes.len() >= 2 && bytes[1] == b':') {
        return Err(bad("must be relative"));
    }
    if entry.split(['/', '\\']).any(|component| component == "..") {
        return Err(bad("must not contain '..'"));
    }
    Ok(())
}

/// Helper for the scalar-or-list `choice` / `dir` decoding above.
///
/// `From` impls for [`ChoiceSpec`] / [`DirSpec`] map the variants across; the
/// untagged deserialization tries the scalar first, then a sequence.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum StringOrList {
    One(String),
    Many(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inline mirror of the real schema: nested choices, a mixed-case sha256, and
    /// both single-string and list `choice` / `dir` selectors.
    const INLINE_CONFIG: &str = r#"
version: 1

choices:
  - id: install_zona
    label: "Install Zona"
    description: "Installs the pack."
    choices:
      - id: sss23
        label: "SSS23"
        description: "High graphics."
        choices:
          - id: clean_hud
            label: "Clean HUD"
            description: "Medik device."
          - id: bhs_hud
            label: "BHS HUD"
            description: "Tarkov-style."

assets:
  - id: download_template
    file: ModOrganizer.ini.template
    url: https://example.com/ModOrganizer.ini.template
    sha256: "C934D463BA60F1E13143FC73B8DB72D52ACD347C80E525E48A10F66C3B711660"
    choice: base
  - id: download_zona_1
    file: zona_multipart.7z.001
    url: https://example.com/zona_multipart.7z.001
    sha256: "bd16cf6f22475d99d47b5dc14a25923445aa3a8fd68909da9e9414e75fcb9661"
    choice: ["install_zona", "bhs_hud"]

steps:
  - name: prepare_dirs
    action: create_dir
    choice: install_zona
    dir: ["downloads", "../anomaly"]
  - name: install_base_anomaly
    action: extract
    choice: install_zona
    file: "downloads/anomaly.7z"
    target: "../anomaly"
  - name: delete_shader_cache
    action: delete
    dir: "../anomaly/appdata/shaders_cache/r4"
  - name: prompt_user_verify_game
    action: verify_game
    choice: install_zona
  - name: setup_mo2_ini_sss23
    action: template_fill
    choice: sss23
    source: "downloads/ModOrganizer.ini.template"
    target: "../mo2/ModOrganizer.ini"
    replacements:
      PROFILE_NAME: "ZONA V1.38 SSS23.5 (High Graphics)"
  - name: install_selected_settings_bhs_hud
    action: extract
    choice: bhs_hud
    file: "downloads/bhs_hud.7z"
    target: "../anomaly"
    from: ["Bhs Hud"]
"#;

    #[test]
    fn parses_full_inline_config() {
        let config = parse_config(INLINE_CONFIG).expect("inline config should parse");

        assert_eq!(config.version, 1);

        // Nested choice tree.
        assert_eq!(config.choices.len(), 1);
        let top = &config.choices[0];
        assert_eq!(top.id, "install_zona");
        assert_eq!(top.label, "Install Zona");
        let sss = &top.choices[0];
        assert_eq!(sss.id, "sss23");
        let leaf_ids: Vec<&str> = sss.choices.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(leaf_ids, vec!["clean_hud", "bhs_hud"]);
        assert!(sss.choices[0].choices.is_empty(), "leaf stays leaf");

        // Assets: single-string and list choices, mixed-case sha256 preserved.
        assert_eq!(config.assets.len(), 2);
        let template = &config.assets[0];
        assert_eq!(template.id, "download_template");
        assert_eq!(template.choice, ChoiceSpec::One("base".to_string()));
        assert_eq!(
            template.sha256,
            "C934D463BA60F1E13143FC73B8DB72D52ACD347C80E525E48A10F66C3B711660"
        );
        let zona = &config.assets[1];
        assert_eq!(
            zona.choice,
            ChoiceSpec::Many(vec!["install_zona".to_string(), "bhs_hud".to_string()])
        );
        assert_eq!(
            zona.sha256,
            "bd16cf6f22475d99d47b5dc14a25923445aa3a8fd68909da9e9414e75fcb9661"
        );

        // Steps: actions, dir spec shapes, from list, replacements map.
        assert_eq!(config.steps.len(), 6);

        let prepare = &config.steps[0];
        assert_eq!(prepare.action, "create_dir");
        assert_eq!(
            prepare.dir,
            Some(DirSpec::Many(vec![
                "downloads".to_string(),
                "../anomaly".to_string()
            ]))
        );
        assert_eq!(
            prepare.choice,
            Some(ChoiceSpec::One("install_zona".to_string()))
        );

        let extract = &config.steps[1];
        assert_eq!(extract.action, "extract");
        assert_eq!(extract.file.as_deref(), Some("downloads/anomaly.7z"));
        assert_eq!(extract.target.as_deref(), Some("../anomaly"));

        // No choice -> always runs.
        let delete = &config.steps[2];
        assert_eq!(delete.action, "delete");
        assert_eq!(delete.choice, None);
        assert_eq!(
            delete.dir,
            Some(DirSpec::One(
                "../anomaly/appdata/shaders_cache/r4".to_string()
            ))
        );

        let verify = &config.steps[3];
        assert_eq!(verify.action, "verify_game");
        assert_eq!(verify.file, None);

        let template_step = &config.steps[4];
        assert_eq!(template_step.action, "template_fill");
        assert_eq!(
            template_step.source.as_deref(),
            Some("downloads/ModOrganizer.ini.template")
        );
        let replacements = template_step.replacements.as_ref().expect("replacements");
        assert_eq!(
            replacements.get("PROFILE_NAME"),
            Some(&"ZONA V1.38 SSS23.5 (High Graphics)".to_string())
        );

        let hud = &config.steps[5];
        assert_eq!(hud.from, Some(vec!["Bhs Hud".to_string()]));
    }

    #[test]
    fn rejects_unknown_action() {
        let yaml = r#"
version: 1
steps:
  - name: bad_step
    action: frobnicate
"#;
        let err = parse_config(yaml).expect_err("unknown action must be rejected");
        match err {
            AppError::Config { source } => {
                let message = source.message();
                assert!(message.contains("frobnicate"), "message: {message}");
                assert!(message.contains("bad_step"), "message: {message}");
            }
            other => panic!("expected AppError::Config, got {other:?}"),
        }
    }

    #[test]
    fn rejects_extract_without_file() {
        let yaml = r#"
version: 1
steps:
  - name: extract_all
    action: extract
    target: "../anomaly"
"#;
        let err = parse_config(yaml).expect_err("extract without file must be rejected");
        match err {
            AppError::Config { source } => {
                let message = source.message();
                assert!(message.contains("extract_all"), "message: {message}");
                assert!(message.contains("file"), "message: {message}");
            }
            other => panic!("expected AppError::Config, got {other:?}"),
        }
    }

    #[test]
    fn rejects_extract_with_empty_file() {
        // An empty string is present but is not a file, so it must be refused
        // here rather than resolved to the install root and failing later.
        let yaml = r#"
version: 1
steps:
  - name: extract_all
    action: extract
    file: ""
    target: "../anomaly"
"#;
        let err = parse_config(yaml).expect_err("extract with an empty file must be rejected");
        match err {
            AppError::Config { source } => {
                let message = source.message();
                assert!(message.contains("extract_all"), "message: {message}");
                assert!(message.contains("file"), "message: {message}");
            }
            other => panic!("expected AppError::Config, got {other:?}"),
        }
    }

    #[test]
    fn rejects_malformed_yaml() {
        // A tabs-indented sequence is invalid YAML; a schema violation (e.g. a
        // string where a u32 `version` is expected) is equally a Config error.
        let err = parse_config("version: not-a-number\n").expect_err("bad version must fail");
        assert!(matches!(err, AppError::Config { .. }));
    }

    #[test]
    fn rejects_a_version_newer_than_this_installer_supports() {
        // `version > 1` is fatal ("config too new") rather than guessed at.
        let err = parse_config("version: 2\n").expect_err("version 2 must be rejected");
        match err {
            AppError::Config { source } => {
                let message = source.message();
                assert!(message.contains("newer"), "message: {message}");
                assert!(message.contains('2'), "message: {message}");
            }
            other => panic!("expected AppError::Config, got {other:?}"),
        }
    }

    #[test]
    fn rejects_a_version_older_than_this_installer_supports() {
        // `version < 1` (only 0 is expressible in a u32) is equally fatal.
        let err = parse_config("version: 0\n").expect_err("version 0 must be rejected");
        match err {
            AppError::Config { source } => {
                let message = source.message();
                assert!(message.contains("older"), "message: {message}");
            }
            other => panic!("expected AppError::Config, got {other:?}"),
        }
    }

    #[test]
    fn missing_optional_sections_default_to_empty() {
        let config = parse_config("version: 1\n").expect("minimal config parses");
        assert_eq!(config.version, 1);
        assert!(config.choices.is_empty());
        assert!(config.assets.is_empty());
        assert!(config.steps.is_empty());
    }

    // ---- validation (reject at load) --------------------------------------

    const GOOD_SHA: &str = "bd16cf6f22475d99d47b5dc14a25923445aa3a8fd68909da9e9414e75fcb9661";

    fn expect_config_error(yaml: &str, needle: &str) {
        match parse_config(yaml).expect_err("config must be rejected") {
            AppError::Config { source } => {
                let message = source.message().to_string();
                assert!(
                    message.contains(needle),
                    "expected '{needle}' in: {message}"
                );
            }
            other => panic!("expected AppError::Config, got {other:?}"),
        }
    }

    #[test]
    fn rejects_steps_missing_their_required_fields() {
        expect_config_error(
            "version: 1\nsteps:\n  - name: s\n    action: create_dir\n",
            "requires 'dir'",
        );
        expect_config_error(
            "version: 1\nsteps:\n  - name: s\n    action: delete\n",
            "requires 'dir'",
        );
        expect_config_error(
            "version: 1\nsteps:\n  - name: s\n    action: template_fill\n    target: ../mo2/a.ini\n",
            "'source' is required",
        );
        expect_config_error(
            "version: 1\nsteps:\n  - name: s\n    action: template_fill\n    source: downloads/a\n",
            "'target' is required",
        );
        expect_config_error(
            "version: 1\nsteps:\n  - name: s\n    action: extract\n    file: downloads/a.7z\n",
            "'target' is required",
        );
    }

    #[test]
    fn rejects_paths_that_escape_the_zona_folder() {
        for bad in ["../..", "../../x", "C:\\\\Windows", "/etc", "a/../../../x"] {
            let yaml = format!(
                "version: 1\nsteps:\n  - name: s\n    action: create_dir\n    dir: \"{bad}\"\n"
            );
            expect_config_error(&yaml, "'dir'");
        }
        // One `..` (the Zona folder's children) and nested paths are fine.
        let ok = "version: 1\nsteps:\n  - name: s\n    action: create_dir\n    dir: [\"downloads\", \"../anomaly/a/..\"]\n";
        parse_config(ok).expect("in-bounds paths are accepted");
    }

    #[test]
    fn delete_must_stay_strictly_below_the_roots() {
        for bad in [".", "..", "../", "downloads/.."] {
            let yaml = format!(
                "version: 1\nsteps:\n  - name: s\n    action: delete\n    dir: \"{bad}\"\n"
            );
            expect_config_error(&yaml, "below");
        }
    }

    #[test]
    fn rejects_unknown_choice_ids_and_empty_choice_lists() {
        let yaml = format!(
            "version: 1\nchoices:\n  - id: a\n    label: A\n    description: d\nassets:\n  - id: x\n    file: x.bin\n    url: https://e/x\n    sha256: {GOOD_SHA}\n    choice: typo\n"
        );
        expect_config_error(&yaml, "'typo'");
        let yaml = "version: 1\nsteps:\n  - name: s\n    action: verify_game\n    choice: []\n";
        expect_config_error(yaml, "empty list");
    }

    #[test]
    fn rejects_malformed_or_duplicate_assets() {
        let asset = |id: &str, file: &str, sha: &str| {
            format!("  - id: {id}\n    file: {file}\n    url: https://e/{file}\n    sha256: {sha}\n    choice: base\n")
        };
        let doc = |body: String| format!("version: 1\nassets:\n{body}");
        expect_config_error(&doc(asset("a", "a.bin", "abc")), "64 hex");
        expect_config_error(&doc(asset("a", "dir/a.bin", GOOD_SHA)), "plain file name");
        expect_config_error(
            &doc(format!(
                "{}{}",
                asset("a", "a.bin", GOOD_SHA),
                asset("a", "b.bin", GOOD_SHA)
            )),
            "duplicate asset id",
        );
        expect_config_error(
            &doc(format!(
                "{}{}",
                asset("a", "A.bin", GOOD_SHA),
                asset("b", "a.BIN", GOOD_SHA)
            )),
            "already downloads",
        );
    }

    #[test]
    fn rejects_dotdot_in_archive_from_paths() {
        let yaml = "version: 1\nsteps:\n  - name: s\n    action: extract\n    file: downloads/a.7z\n    target: ../anomaly\n    from: [\"../x\"]\n";
        expect_config_error(yaml, "'from'");
    }
}
