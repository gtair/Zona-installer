//! Screen 4: finish.
//!
//! A data-only renderer: it reads the [`ViewModel`]'s completion message and the
//! `view.finish_actions` checkboxes and forwards interactions as [`Event`]s. A
//! checkbox toggle forwards [`Event::FinishAction`] and the Finish button
//! forwards [`Event::Finish`]; the core records and runs the actions.

use eframe::egui;
use zona_installer::app::{Event, FinishActionId, ViewModel};

/// Test-only id for the Finish button (can't collide with a choice id).
#[cfg(test)]
const FINISH_KEY: &str = "__finish__";

/// Renders the completion message, the Finish button and the optional
/// post-install checkboxes.
pub fn draw(ui: &mut egui::Ui, view: &ViewModel, events: &mut Vec<Event>) {
    #[cfg(test)]
    clear_recorded();

    ui.heading("Installation complete");
    ui.label("Your selected files have been installed.");
    ui.add_space(8.0);

    // Draw exactly the entries the core put in `view.finish_actions`, in order.
    ui.label("Optional post-install actions:");
    for entry in &view.finish_actions {
        // The view is immutable, so a toggle is forwarded as an event.
        let mut checked = entry.enabled;
        let response = ui.checkbox(&mut checked, action_label(entry.action));
        #[cfg(test)]
        record_rect(action_key(entry.action), response.rect, response.enabled());
        if response.changed() {
            events.push(Event::FinishAction {
                action: entry.action,
                on: checked,
            });
        }
    }

    ui.add_space(8.0);
    ui.separator();

    // The single Finish button; the core runs the enabled actions.
    let finish = ui.button("Finish");
    #[cfg(test)]
    record_rect(FINISH_KEY, finish.rect, finish.enabled());
    if finish.clicked() {
        events.push(Event::Finish);
    }
}

/// The human-readable label for a post-install action checkbox.
fn action_label(action: FinishActionId) -> &'static str {
    match action {
        FinishActionId::DeleteDownloads => "Delete downloads",
        FinishActionId::CreateShortcut => "Create shortcuts",
        FinishActionId::StartGame => "Start the game",
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
    rect: egui::Rect,
    enabled: bool,
}

#[cfg(test)]
fn clear_recorded() {
    RECORDED.with(|recorded| recorded.borrow_mut().clear());
}

#[cfg(test)]
fn record_rect(id: &str, rect: egui::Rect, enabled: bool) {
    RECORDED.with(|recorded| {
        recorded.borrow_mut().push(Recorded {
            id: id.to_string(),
            rect,
            enabled,
        });
    });
}

/// A stable test key for one action's checkbox (test-only).
#[cfg(test)]
fn action_key(action: FinishActionId) -> &'static str {
    match action {
        FinishActionId::DeleteDownloads => "__finish_delete_downloads__",
        FinishActionId::CreateShortcut => "__finish_create_shortcut__",
        FinishActionId::StartGame => "__finish_start_game__",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use zona_installer::app::{FinishAction, InstallProgress, Phase};

    /// The three finish actions the core seeds in `enter_finished`, unchecked.
    fn actions() -> Vec<FinishAction> {
        vec![
            FinishAction {
                action: FinishActionId::DeleteDownloads,
                enabled: false,
            },
            FinishAction {
                action: FinishActionId::CreateShortcut,
                enabled: false,
            },
            FinishAction {
                action: FinishActionId::StartGame,
                enabled: false,
            },
        ]
    }

    /// A fixture `ViewModel` on the finish screen.
    fn view(finish_actions: Vec<FinishAction>) -> ViewModel {
        ViewModel {
            phase: Phase::Finished,
            choices: Vec::new(),
            selected: HashSet::new(),
            can_confirm: false,
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
            finish_actions,
        }
    }

    // ---- headless driving -------------------------------------------------

    #[test]
    fn action_label_names_each_post_install_action() {
        assert_eq!(
            action_label(FinishActionId::DeleteDownloads),
            "Delete downloads"
        );
        assert_eq!(
            action_label(FinishActionId::CreateShortcut),
            "Create shortcuts"
        );
        assert_eq!(action_label(FinishActionId::StartGame), "Start the game");
    }

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

    fn recorded() -> Vec<(String, egui::Rect, bool)> {
        RECORDED.with(|recorded| {
            recorded
                .borrow()
                .iter()
                .map(|entry| (entry.id.clone(), entry.rect, entry.enabled))
                .collect()
        })
    }

    fn rect_of(id: &str) -> egui::Rect {
        recorded()
            .into_iter()
            .find(|(record_id, _, _)| record_id == id)
            .map(|(_, rect, _)| rect)
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

    /// Lays out one frame, finds the widget, then drives a real pointer hover,
    /// press and release at its centre and returns the events `draw` emitted.
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

    // ---- headless render: painted text ------------------------------------

    /// One text run the real `draw` painted this frame.
    type Painted = (egui::Pos2, String);

    fn collect_text(shapes: &[egui::epaint::ClippedShape], out: &mut Vec<Painted>) {
        for clipped in shapes {
            collect_shape(&clipped.shape, out);
        }
    }

    fn collect_shape(shape: &egui::epaint::Shape, out: &mut Vec<Painted>) {
        match shape {
            egui::epaint::Shape::Text(text) => out.push((text.pos, text.galley.text().to_owned())),
            egui::epaint::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_shape(shape, out);
                }
            }
            _ => {}
        }
    }

    /// Drives the real `draw` through a headless `egui::Context` and returns the
    /// text runs it actually painted.
    fn paint(ctx: &egui::Context, view: &ViewModel) -> Vec<Painted> {
        let run = || {
            egui::CentralPanel::default().show(ctx, |ui| {
                let mut events = Vec::new();
                draw(ui, view, &mut events);
            });
        };
        // Warm up the font atlas so the capture frame lays out every glyph.
        let _ = ctx.run(input(Vec::new()), |_| run());
        let output = ctx.run(input(Vec::new()), |_| run());
        let mut texts = Vec::new();
        collect_text(&output.shapes, &mut texts);
        texts
    }

    fn painted(texts: &[Painted], label: &str) -> bool {
        texts.iter().any(|(_, text)| text == label)
    }

    // ---- tests ------------------------------------------------------------

    /// Renders the completion message, Finish button and three checkboxes.
    #[test]
    fn renders_message_finish_button_and_three_checkboxes() {
        let ctx = egui::Context::default();
        let view = view(actions());
        let texts = paint(&ctx, &view);

        assert!(
            painted(&texts, "Installation complete"),
            "the completion message must be painted: {texts:?}"
        );
        assert!(
            painted(&texts, "Finish"),
            "the Finish button must be painted: {texts:?}"
        );
        for label in ["Delete downloads", "Create shortcuts", "Start the game"] {
            assert!(
                painted(&texts, label),
                "checkbox {label:?} painted: {texts:?}"
            );
        }
    }

    /// Toggling a checkbox forwards `FinishAction` with the new state.
    #[test]
    fn toggling_a_checkbox_forwards_finish_action() {
        let ctx = egui::Context::default();
        let view = view(actions());

        let events = click(&ctx, &view, action_key(FinishActionId::CreateShortcut));
        assert_eq!(
            events,
            vec![Event::FinishAction {
                action: FinishActionId::CreateShortcut,
                on: true,
            }]
        );
    }

    /// A checkbox that is already checked toggles back off, forwarding
    /// `FinishAction { on: false }`.
    #[test]
    fn untoggling_a_checked_checkbox_forwards_the_off_state() {
        let ctx = egui::Context::default();
        let mut finish_actions = actions();
        finish_actions[0].enabled = true;
        let view = view(finish_actions);

        let events = click(&ctx, &view, action_key(FinishActionId::DeleteDownloads));
        assert_eq!(
            events,
            vec![Event::FinishAction {
                action: FinishActionId::DeleteDownloads,
                on: false,
            }]
        );
    }

    /// Clicking Finish forwards `Finish`.
    #[test]
    fn clicking_finish_forwards_finish() {
        let ctx = egui::Context::default();
        let view = view(actions());
        let events = click(&ctx, &view, FINISH_KEY);

        assert_eq!(events, vec![Event::Finish]);
    }
}
