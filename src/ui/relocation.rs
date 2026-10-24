//! Pre-flight relocation screens: the drive picker and the "`Zona` already
//! exists" prompt.
//!
//! These are data-only renderers: they read the [`ViewModel`] (`Phase::Relocation
//! { drives }` and `Phase::RelocationTargetExists { drive }`) and forward the
//! user's choice as [`Event::DriveChosen`] / [`Event::RelocateTarget`]. The
//! screen renders the `drives` slice it is handed and never invents or filters
//! drives.

use eframe::egui;
use zona_installer::app::{Event, Phase, ViewModel};

/// Test-only ids for the target-exists buttons (can't collide with a drive).
#[cfg(test)]
const DELETE_KEY: &str = "__relocation_delete__";
#[cfg(test)]
const CANCEL_KEY: &str = "__relocation_cancel__";

/// Renders whichever relocation screen the phase calls for.
pub fn draw(ui: &mut egui::Ui, view: &ViewModel, events: &mut Vec<Event>) {
    #[cfg(test)]
    clear_recorded();

    match &view.phase {
        Phase::Relocation { drives } => draw_drive_picker(ui, drives, events),
        Phase::RelocationTargetExists { drive } => draw_target_exists(ui, drive, events),
        _ => {}
    }
}

/// The drive picker: one button per drive; clicking one forwards
/// [`Event::DriveChosen`].
fn draw_drive_picker(ui: &mut egui::Ui, drives: &[String], events: &mut Vec<Event>) {
    ui.heading("Choose where to install");
    ui.label("Zona is installed into a `Zona` folder on the drive you pick.");
    ui.add_space(8.0);

    for drive in drives {
        let response = ui.button(drive.as_str());
        #[cfg(test)]
        record_rect(drive, response.rect, response.enabled());
        if response.clicked() {
            events.push(Event::DriveChosen {
                drive: drive.clone(),
            });
        }
    }
}

/// The `<drive>:\Zona\installer already exists` prompt.
///
/// Delete forwards [`Event::RelocateTarget`] with `delete: true`; Cancel
/// forwards it with `delete: false`.
fn draw_target_exists(ui: &mut egui::Ui, drive: &str, events: &mut Vec<Event>) {
    ui.heading("The Zona installer already exists on that drive");
    ui.label(format!("{} already exists.", install_target_display(drive)));
    ui.label("Do you wish to replace it?");
    ui.add_space(8.0);

    let delete = ui.button("Replace and continue");
    #[cfg(test)]
    record_rect(DELETE_KEY, delete.rect, delete.enabled());
    if delete.clicked() {
        events.push(Event::RelocateTarget { delete: true });
    }

    let cancel = ui.button("Cancel");
    #[cfg(test)]
    record_rect(CANCEL_KEY, cancel.rect, cancel.enabled());
    if cancel.clicked() {
        events.push(Event::RelocateTarget { delete: false });
    }
}

/// The `<drive>:\Zona\installer` path for display, given a `C:\`-style drive
/// root.
fn install_target_display(drive: &str) -> String {
    if drive.ends_with('\\') || drive.ends_with('/') {
        format!("{drive}Zona\\installer")
    } else {
        format!("{drive}\\Zona\\installer")
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use zona_installer::app::InstallProgress;

    // ---- fixtures ---------------------------------------------------------

    /// A fixture `ViewModel` on one of the relocation phases.
    fn view(phase: Phase) -> ViewModel {
        ViewModel {
            phase,
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
            finish_actions: Vec::new(),
        }
    }

    fn drives() -> Vec<String> {
        vec!["C:\\".to_string(), "D:\\".to_string()]
    }

    // ---- pure helper ------------------------------------------------------

    /// The display path joins the trailing separator once, for both shapes of
    /// drive root the picker can hand over.
    #[test]
    fn install_target_display_joins_the_separator_once() {
        assert_eq!(install_target_display("C:\\"), "C:\\Zona\\installer");
        assert_eq!(install_target_display("D:/"), "D:/Zona\\installer");
        assert_eq!(install_target_display("E:"), "E:\\Zona\\installer");
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

    /// The drive picker renders one button per drive and forwards the clicked one.
    #[test]
    fn drive_picker_renders_every_drive_and_forwards_the_clicked_one() {
        let ctx = egui::Context::default();
        let view = view(Phase::Relocation { drives: drives() });

        draw_frame(&ctx, &view, Vec::new());
        let ids: Vec<String> = recorded().into_iter().map(|(id, _, _)| id).collect();
        assert!(ids.contains(&"C:\\".to_string()), "rendered: {ids:?}");
        assert!(ids.contains(&"D:\\".to_string()), "rendered: {ids:?}");

        let events = click(&ctx, &view, "D:\\");
        assert_eq!(
            events,
            vec![Event::DriveChosen {
                drive: "D:\\".to_string(),
            }]
        );
    }

    /// The target-exists prompt names the conflicting folder and offers both
    /// choices; delete and cancel forward the matching flag.
    #[test]
    fn target_exists_offers_delete_and_cancel() {
        let ctx = egui::Context::default();
        let view = view(Phase::RelocationTargetExists {
            drive: "C:\\".to_string(),
        });

        let texts = paint(&ctx, &view);
        assert!(
            painted(&texts, "C:\\Zona\\installer already exists."),
            "the prompt must name the conflicting folder: {texts:?}"
        );
        assert!(
            painted(&texts, "Replace and continue"),
            "the replace choice must be offered: {texts:?}"
        );
        assert!(
            painted(&texts, "Cancel"),
            "the cancel choice must be offered: {texts:?}"
        );

        assert_eq!(
            click(&ctx, &view, DELETE_KEY),
            vec![Event::RelocateTarget { delete: true }]
        );
        assert_eq!(
            click(&ctx, &view, CANCEL_KEY),
            vec![Event::RelocateTarget { delete: false }]
        );
    }

    /// The screen lays out nothing for a phase it does not own (the shell only
    /// routes the two relocation phases here), so there is no widget to click.
    #[test]
    fn other_phases_render_nothing() {
        let ctx = egui::Context::default();
        let view = view(Phase::Preflight);

        let events = draw_frame(&ctx, &view, Vec::new());
        assert!(recorded().is_empty(), "no widget for a foreign phase");
        assert!(events.is_empty(), "a foreign phase emits nothing");
    }
}
