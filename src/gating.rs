//! Pure gating logic: resolve which assets and steps run for a selected
//! choice-id set.
//!
//! An asset or step runs iff its `choice` selector is satisfied by the selected
//! set. The literal id `"base"` is always active; a single-string `choice` acts
//! like a one-element list; a step with no `choice` always runs.

use std::collections::HashSet;

use crate::config::{Asset, ChoiceId, ChoiceSpec, Config, Step};

/// The literal id that is always active no matter what is selected.
const BASE_ID: &str = "base";

/// Returns `true` if a gating selector is satisfied by `selected`.
///
/// `"base"` is always active; a [`ChoiceSpec::Many`] is active when any of its
/// ids is active.
pub fn is_active(spec: &ChoiceSpec, selected: &HashSet<ChoiceId>) -> bool {
    match spec {
        ChoiceSpec::One(id) => id == BASE_ID || selected.contains(id),
        ChoiceSpec::Many(ids) => ids.iter().any(|id| id == BASE_ID || selected.contains(id)),
    }
}

/// All assets that run for `selected`, in config order.
pub fn needed_assets<'a>(cfg: &'a Config, selected: &HashSet<ChoiceId>) -> Vec<&'a Asset> {
    cfg.assets
        .iter()
        .filter(|asset| is_active(&asset.choice, selected))
        .collect()
}

/// All steps that run for `selected`, in config order.
pub fn needed_steps<'a>(cfg: &'a Config, selected: &HashSet<ChoiceId>) -> Vec<&'a Step> {
    cfg.steps
        .iter()
        .filter(|step| match &step.choice {
            None => true,
            Some(spec) => is_active(spec, selected),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn selected(ids: &[&str]) -> HashSet<ChoiceId> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    fn spec_one(id: &str) -> ChoiceSpec {
        ChoiceSpec::One(id.to_string())
    }

    fn spec_many(ids: &[&str]) -> ChoiceSpec {
        ChoiceSpec::Many(ids.iter().map(|id| (*id).to_string()).collect())
    }

    fn asset(id: &str, choice: ChoiceSpec) -> Asset {
        Asset {
            id: id.to_string(),
            file: format!("{id}.7z"),
            url: "https://example.com/download".to_string(),
            sha256: "00112233445566778899aabbccddeeff".to_string(),
            choice,
        }
    }

    fn step(name: &str, action: &str, choice: Option<ChoiceSpec>) -> Step {
        Step {
            name: name.to_string(),
            action: action.to_string(),
            choice,
            dir: None,
            file: None,
            target: None,
            from: None,
            source: None,
            replacements: None::<BTreeMap<String, String>>,
        }
    }

    fn config_with(assets: Vec<Asset>, steps: Vec<Step>) -> Config {
        Config {
            version: 1,
            choices: Vec::new(),
            assets,
            steps,
        }
    }

    #[test]
    fn base_is_always_active_for_one_and_many() {
        let none = selected(&[]);
        let something = selected(&["sss23"]);

        assert!(is_active(&spec_one("base"), &none));
        assert!(is_active(&spec_one("base"), &something));
        assert!(is_active(&spec_many(&["base", "never_selected"]), &none));
        assert!(is_active(
            &spec_many(&["base", "never_selected"]),
            &something
        ));
    }

    #[test]
    fn one_and_many_are_pure_set_membership() {
        let picked = selected(&["install_zona", "sss23"]);

        assert!(is_active(&spec_one("sss23"), &picked));
        assert!(!is_active(&spec_one("bhs_hud"), &picked));
        assert!(!is_active(&spec_one("sss21"), &picked));

        // Many is active when ANY member is selected.
        assert!(is_active(&spec_many(&["sss21", "sss23"]), &picked));
        assert!(!is_active(&spec_many(&["sss21", "clean_hud"]), &picked));
    }

    #[test]
    fn sss23_selection_activates_sss23_assets_and_base() {
        let cfg = config_with(
            vec![
                asset("template", spec_one("base")),
                asset("zona", spec_one("install_zona")),
                asset("sss23_specific", spec_one("sss23")),
                asset("bhs_hud_specific", spec_one("bhs_hud")),
                asset("either", spec_many(&["sss21", "sss23"])),
            ],
            Vec::new(),
        );
        let picked = selected(&["install_zona", "sss23"]);

        let ids: Vec<&str> = needed_assets(&cfg, &picked)
            .iter()
            .map(|asset| asset.id.as_str())
            .collect();
        assert_eq!(ids, vec!["template", "zona", "sss23_specific", "either"]);
    }

    #[test]
    fn no_choice_step_always_runs_and_order_is_preserved() {
        let cfg = config_with(
            Vec::new(),
            vec![
                step("prepare_dirs", "create_dir", Some(spec_one("install_zona"))),
                step("delete_shader_cache", "delete", None),
                step("install_sss23", "extract", Some(spec_one("sss23"))),
                step("install_bhs_hud", "extract", Some(spec_one("bhs_hud"))),
                step("verify", "verify_game", Some(spec_one("install_zona"))),
                step("setup_mo2_ini", "template_fill", Some(spec_one("base"))),
            ],
        );

        let picked = selected(&["install_zona", "sss23"]);
        let names: Vec<&str> = needed_steps(&cfg, &picked)
            .iter()
            .map(|step| step.name.as_str())
            .collect();

        // "install_bhs_hud" is not selected; everything else runs, config order.
        assert_eq!(
            names,
            vec![
                "prepare_dirs",
                "delete_shader_cache",
                "install_sss23",
                "verify",
                "setup_mo2_ini",
            ]
        );
    }

    #[test]
    fn empty_selection_still_yields_base_assets_and_no_choice_steps() {
        let cfg = config_with(
            vec![
                asset("template", spec_one("base")),
                asset("zona", spec_one("install_zona")),
                asset("multi_with_base", spec_many(&["base", "update_graphics"])),
            ],
            vec![
                step("delete_shader_cache", "delete", None),
                step(
                    "install_zona_step",
                    "extract",
                    Some(spec_one("install_zona")),
                ),
            ],
        );

        let none = selected(&[]);
        let asset_ids: Vec<&str> = needed_assets(&cfg, &none)
            .iter()
            .map(|asset| asset.id.as_str())
            .collect();
        assert_eq!(asset_ids, vec!["template", "multi_with_base"]);

        let step_names: Vec<&str> = needed_steps(&cfg, &none)
            .iter()
            .map(|step| step.name.as_str())
            .collect();
        assert_eq!(step_names, vec!["delete_shader_cache"]);
    }
}
