//! Screen 1: choice selection.
//!
//! This is a data-only renderer: it reads the [`ViewModel`] (the choice tree and
//! the selected id set) and forwards clicks as [`Event`]s. The cascading
//! semantics live in the core; the screen derives everything it draws from
//! `view.selected` and emits only `ChoiceToggled` / `ConfirmChoices`.

use std::collections::HashSet;

use eframe::egui;
use zona_installer::app::{Event, ViewModel};
use zona_installer::config::{ChoiceId, ChoiceNode};

/// Test-only id for the Confirm button (can't collide with a choice id).
#[cfg(test)]
const CONFIRM_KEY: &str = "__confirm__";

/// Renders the choice tree and pushes choice events into `events`.
pub fn draw(ui: &mut egui::Ui, view: &ViewModel, events: &mut Vec<Event>) {
    #[cfg(test)]
    clear_recorded();

    ui.heading("Choose what to install");
    ui.label("Pick one option per level. The three top-level branches are mutually exclusive.");
    ui.add_space(8.0);

    // Leave room for the Confirm button; the tree itself scrolls.
    let list_height = (ui.available_height() - 40.0).max(80.0);
    egui::ScrollArea::vertical()
        .max_height(list_height)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            draw_level(ui, &view.choices, &view.selected, events, "");
        });

    ui.add_space(4.0);
    ui.separator();
    // Confirm is disabled until the selection is a complete root->leaf path.
    let confirm = ui.add_enabled(view.can_confirm, egui::Button::new("Confirm choices"));
    #[cfg(test)]
    record_rect(CONFIRM_KEY, CONFIRM_KEY, confirm.rect, confirm.enabled());
    if confirm.clicked() {
        events.push(Event::ConfirmChoices);
    }
}

/// Renders one level of the tree: a radio per node, descending into the picked
/// node's children. `prefix` is the root..parent id path, used only to record
/// stable widget paths for the tests.
fn draw_level(
    ui: &mut egui::Ui,
    nodes: &[ChoiceNode],
    selected: &HashSet<ChoiceId>,
    events: &mut Vec<Event>,
    prefix: &str,
) {
    for node in nodes {
        let path = child_path(prefix, &node.id);
        let picked = selected.contains(&node.id);

        let response = ui.radio(picked, node.label.as_str());
        #[cfg(test)]
        record_rect(&node.id, &path, response.rect, response.enabled());
        if response.clicked() {
            // Clicking always picks; there is no deselect.
            events.push(Event::ChoiceToggled {
                id: node.id.clone(),
                on: true,
            });
        }

        if !node.description.is_empty() {
            ui.label(egui::RichText::new(&node.description).small().weak());
        }

        // Expand only the picked node.
        if picked && !node.choices.is_empty() {
            ui.indent(("choices", &node.id), |ui| {
                draw_level(ui, &node.choices, selected, events, &path);
            });
        }
    }
}

fn child_path(prefix: &str, id: &str) -> String {
    if prefix.is_empty() {
        id.to_string()
    } else {
        format!("{prefix}/{id}")
    }
}

// Test-only record of widgets laid out this frame, so tests can drive real
// pointer events at a widget's centre.
#[cfg(test)]
thread_local! {
    static RECORDED: std::cell::RefCell<Vec<Recorded>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
struct Recorded {
    id: String,
    path: String,
    rect: egui::Rect,
    enabled: bool,
}

#[cfg(test)]
fn clear_recorded() {
    RECORDED.with(|recorded| recorded.borrow_mut().clear());
}

#[cfg(test)]
fn record_rect(id: &str, path: &str, rect: egui::Rect, enabled: bool) {
    RECORDED.with(|recorded| {
        recorded.borrow_mut().push(Recorded {
            id: id.to_string(),
            path: path.to_string(),
            rect,
            enabled,
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use zona_installer::app::{InstallProgress, Phase, ViewModel};

    fn node(id: &str, label: &str, children: Vec<ChoiceNode>) -> ChoiceNode {
        ChoiceNode {
            id: id.to_string(),
            label: label.to_string(),
            description: String::new(),
            choices: children,
        }
    }

    fn leaf(id: &str, label: &str) -> ChoiceNode {
        node(id, label, Vec::new())
    }

    /// The real `config.yaml` shape.
    fn tree() -> Vec<ChoiceNode> {
        let branch = |id: &str| {
            node(
                id,
                id,
                vec![
                    node("sss21", "SSS21", vec![leaf("bhs_hud", "BHS HUD")]),
                    node(
                        "sss23",
                        "SSS23",
                        vec![leaf("clean_hud", "Clean HUD"), leaf("bhs_hud", "BHS HUD")],
                    ),
                ],
            )
        };
        vec![
            branch("install_zona"),
            branch("update_graphics"),
            branch("update_zona"),
        ]
    }

    fn view(selected: &[&str], can_confirm: bool) -> ViewModel {
        ViewModel {
            phase: Phase::Choices,
            choices: tree(),
            selected: selected.iter().map(|id| (*id).to_string()).collect(),
            can_confirm,
            install_can_start: false,
            install: InstallProgress {
                step: 0,
                total: 0,
                label: None,
                percent: None,
                detail: None,
            },
            verify_game: None,
            downloads: Vec::new(),
            download_status: None,
            finish_actions: Vec::new(),
        }
    }

    // ---- headless driving -------------------------------------------------

    fn input(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1024.0, 768.0),
            )),
            events,
            ..Default::default()
        }
    }

    /// Runs one real `draw` frame through a headless `egui::Context` and returns
    /// the events it emitted.
    fn draw_frame(ctx: &egui::Context, view: &ViewModel, events: Vec<egui::Event>) -> Vec<Event> {
        let mut out = Vec::new();
        let _ = ctx.run(input(events), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                draw(ui, view, &mut out);
            });
        });
        out
    }

    fn recorded() -> Vec<(String, String, egui::Rect, bool)> {
        RECORDED.with(|recorded| {
            recorded
                .borrow()
                .iter()
                .map(|entry| {
                    (
                        entry.id.clone(),
                        entry.path.clone(),
                        entry.rect,
                        entry.enabled,
                    )
                })
                .collect()
        })
    }

    /// The root..node paths of every choice node drawn this frame (excluding the
    /// Confirm button).
    fn rendered_paths() -> Vec<String> {
        recorded()
            .into_iter()
            .map(|(_, path, _, _)| path)
            .filter(|path| path != CONFIRM_KEY)
            .collect()
    }

    fn rect_of(id: &str) -> egui::Rect {
        recorded()
            .into_iter()
            .find(|(record_id, _, _, _)| record_id == id)
            .map(|(_, _, rect, _)| rect)
            .expect("a recorded widget with that id")
    }

    /// Whether the recorded widget with `id` was laid out enabled this frame.
    fn enabled_of(id: &str) -> bool {
        recorded()
            .into_iter()
            .find(|(record_id, _, _, _)| record_id == id)
            .map(|(_, _, _, enabled)| enabled)
            .expect("a recorded widget with that id")
    }

    fn pointer(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// Drives a real pointer click at the widget's centre and returns the events.
    fn click(ctx: &egui::Context, view: &ViewModel, id: &str) -> Vec<Event> {
        draw_frame(ctx, view, Vec::new());
        let pos = rect_of(id).center();
        draw_frame(ctx, view, vec![egui::Event::PointerMoved(pos)]);
        let mut events = draw_frame(
            ctx,
            view,
            vec![egui::Event::PointerMoved(pos), pointer(pos, true)],
        );
        events.extend(draw_frame(
            ctx,
            view,
            vec![egui::Event::PointerMoved(pos), pointer(pos, false)],
        ));
        events
    }

    /// A fixture renders the tree: all three top-level branches, no expansion
    /// while nothing is picked.
    #[test]
    fn renders_all_top_level_branches_and_expands_nothing_when_unpicked() {
        let ctx = egui::Context::default();
        let view = view(&[], false);
        draw_frame(&ctx, &view, Vec::new());

        let paths = rendered_paths();
        assert!(paths.contains(&"install_zona".to_string()), "got {paths:?}");
        assert!(
            paths.contains(&"update_graphics".to_string()),
            "got {paths:?}"
        );
        assert!(paths.contains(&"update_zona".to_string()), "got {paths:?}");
        assert_eq!(
            paths.len(),
            3,
            "nothing is picked, so no branch may expand: {paths:?}"
        );
    }

    /// Picking a top-level branch expands only that branch.
    #[test]
    fn picking_a_top_level_branch_expands_only_that_branch() {
        let ctx = egui::Context::default();
        let view = view(&["update_zona"], false);
        draw_frame(&ctx, &view, Vec::new());

        let paths = rendered_paths();
        assert!(
            paths.contains(&"update_zona/sss21".to_string()),
            "the picked branch must expand: {paths:?}"
        );
        assert!(
            paths.contains(&"update_zona/sss23".to_string()),
            "got {paths:?}"
        );
        assert!(
            !paths.contains(&"install_zona/sss21".to_string()),
            "a deselected branch must not expand: {paths:?}"
        );
        assert!(
            !paths.contains(&"update_graphics/sss21".to_string()),
            "a deselected branch must not expand: {paths:?}"
        );
    }

    /// Clicking a top-level branch forwards the toggle.
    #[test]
    fn clicking_a_top_level_branch_emits_choice_toggled() {
        let ctx = egui::Context::default();
        let view = view(&[], false);
        let events = click(&ctx, &view, "update_zona");

        assert_eq!(
            events,
            vec![Event::ChoiceToggled {
                id: "update_zona".to_string(),
                on: true,
            }]
        );
    }

    /// Toggling a leaf emits its toggle; the picked path is rendered as expanded.
    #[test]
    fn clicking_a_leaf_emits_choice_toggled_for_the_picked_path() {
        let ctx = egui::Context::default();
        let view = view(&["install_zona", "sss23"], false);
        draw_frame(&ctx, &view, Vec::new());

        let paths = rendered_paths();
        assert!(
            paths.contains(&"install_zona/sss23/clean_hud".to_string()),
            "the deepest picked node must expand: {paths:?}"
        );
        assert!(
            paths.contains(&"install_zona/sss23/bhs_hud".to_string()),
            "got {paths:?}"
        );
        assert!(
            !paths.contains(&"update_zona/sss23".to_string()),
            "another branch must stay collapsed: {paths:?}"
        );

        let events = click(&ctx, &view, "bhs_hud");
        assert_eq!(
            events,
            vec![Event::ChoiceToggled {
                id: "bhs_hud".to_string(),
                on: true,
            }]
        );
    }

    /// The Confirm button forwards `ConfirmChoices` when the selection is a
    /// complete root->leaf path.
    #[test]
    fn clicking_confirm_emits_confirm_choices_when_enabled() {
        let ctx = egui::Context::default();
        let view = view(&["install_zona", "sss23", "bhs_hud"], true);
        let events = click(&ctx, &view, CONFIRM_KEY);

        assert_eq!(events, vec![Event::ConfirmChoices]);
    }

    /// The Confirm button renders disabled while `can_confirm` is false, and a
    /// click on it emits nothing.
    #[test]
    fn confirm_is_rendered_disabled_when_can_confirm_is_false() {
        let ctx = egui::Context::default();
        let view = view(&["install_zona", "sss23"], false);

        draw_frame(&ctx, &view, Vec::new());
        assert!(
            !enabled_of(CONFIRM_KEY),
            "Confirm must be laid out disabled when can_confirm is false"
        );

        let events = click(&ctx, &view, CONFIRM_KEY);
        assert!(
            events.is_empty(),
            "a disabled Confirm must emit nothing: {events:?}"
        );
    }

    /// The Confirm button renders enabled once `can_confirm` is true.
    #[test]
    fn confirm_is_rendered_enabled_when_can_confirm_is_true() {
        let ctx = egui::Context::default();
        let view = view(&["install_zona", "sss23", "bhs_hud"], true);

        draw_frame(&ctx, &view, Vec::new());
        assert!(
            enabled_of(CONFIRM_KEY),
            "Confirm must be laid out enabled when can_confirm is true"
        );
    }
}
