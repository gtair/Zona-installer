//! Screen 3: install progress.
//!
//! A data-only renderer: it reads the [`ViewModel`] and draws the overall
//! install bar, the `Step [i]:` info line and a context-dependent button. Which
//! button is drawn is decided by the view model:
//!
//! * `install_can_start`: a **Continue** button that starts the pipeline
//!   ([`Event::BeginInstall`]).
//! * `verify_game` is `Some`: the verify-game message plus a **Start game**
//!   button, a disabled **Continue**, or an enabled **Continue**, depending on
//!   [`VerifyGameStage`].
//! * otherwise: no button.

use eframe::egui;
use zona_installer::app::{Event, InstallProgress, VerifyGameStage, ViewModel};

/// Test-only ids for the buttons.
#[cfg(test)]
const CONTINUE_KEY: &str = "__continue__";
#[cfg(test)]
const START_GAME_KEY: &str = "__start_game__";

/// The message shown while the verify-game gate is active.
const VERIFY_MESSAGE: &str =
    "Launch the game and, once you load into the main menu, make sure you see \"S.T.A.L.K.E.R. Anomaly 1.5.3 Modded Exes\". Do not start a save.";

/// Renders the overall install bar, the live detail line, the step info line and
/// the context-dependent button.
pub fn draw(ui: &mut egui::Ui, view: &ViewModel, events: &mut Vec<Event>) {
    #[cfg(test)]
    clear_recorded();

    ui.heading("Installing");
    ui.label("Your selected configuration is being installed.");
    ui.add_space(8.0);

    // One overall bar; the running step's percentage smooths it.
    ui.add(egui::ProgressBar::new(overall_fraction(&view.install)).text(bar_text(&view.install)));

    ui.add_space(6.0);

    // The live detail line, directly under the bar.
    if let Some(detail) = &view.install.detail {
        ui.label(detail);
    }

    // `Step [i]: [step information]`.
    ui.label(step_line(&view.install));

    ui.add_space(8.0);
    ui.separator();

    // The verify-game gate.
    if let Some(verify) = &view.verify_game {
        ui.label(VERIFY_MESSAGE);
        ui.add_space(6.0);
        match verify.stage {
            VerifyGameStage::WaitingToStart => {
                let start = ui.add(egui::Button::new("Start game"));
                #[cfg(test)]
                record_rect(START_GAME_KEY, start.rect, start.enabled());
                if start.clicked() {
                    events.push(Event::StartGame);
                }
            }
            VerifyGameStage::Running => {
                let response = ui.add_enabled(false, egui::Button::new("Continue"));
                #[cfg(test)]
                record_rect(CONTINUE_KEY, response.rect, response.enabled());
                let _ = response;
            }
            VerifyGameStage::Finished => {
                let response = ui.add(egui::Button::new("Continue"));
                #[cfg(test)]
                record_rect(CONTINUE_KEY, response.rect, response.enabled());
                if response.clicked() {
                    events.push(Event::Continue);
                }
            }
        }
        return;
    }

    // The post-download Continue button; during normal execution no button is
    // drawn.
    if view.install_can_start {
        let response = ui.add(egui::Button::new("Continue"));
        #[cfg(test)]
        record_rect(CONTINUE_KEY, response.rect, response.enabled());
        if response.clicked() {
            events.push(Event::BeginInstall);
        }
    }
}

/// The overall bar fraction: `(step + percent/100) / total`.
///
/// `step` steps have finished; the running step's `percent` advances the bar
/// *inside* the step. With no applicable steps the bar is empty.
fn overall_fraction(install: &InstallProgress) -> f32 {
    if install.total == 0 {
        0.0
    } else {
        let within = install.percent.unwrap_or(0) as f32 / 100.0;
        ((install.step as f32 + within) / install.total as f32).clamp(0.0, 1.0)
    }
}

/// The overall bar's text: `completed/total`.
fn bar_text(install: &InstallProgress) -> String {
    format!("{}/{}", install.step, install.total)
}

/// The `Step [i]: [step information]` info line.
///
/// `i` is one-based; with no running step yet (`label == None`) a static
/// "waiting to start" placeholder is used.
fn step_line(install: &InstallProgress) -> String {
    let number = install.step + 1;
    match &install.label {
        Some(label) => format!("Step {number}: {label}"),
        None => format!("Step {number}: waiting to start"),
    }
}

// Test-only record of the Continue button laid out this frame, so tests can
// drive a real pointer event at its centre.
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
    use zona_installer::app::{Phase, VerifyGameView};

    fn install(step: usize, total: usize, label: Option<&str>) -> InstallProgress {
        InstallProgress {
            step,
            total,
            label: label.map(str::to_string),
            percent: None,
            detail: None,
        }
    }

    /// An `InstallProgress` with the live percent/detail fields set.
    fn install_with(
        step: usize,
        total: usize,
        label: Option<&str>,
        percent: Option<u32>,
        detail: Option<&str>,
    ) -> InstallProgress {
        InstallProgress {
            step,
            total,
            label: label.map(str::to_string),
            percent,
            detail: detail.map(str::to_string),
        }
    }

    /// A fixture `ViewModel` with a running install step.
    fn view(phase: Phase, step: usize, total: usize, label: Option<&str>) -> ViewModel {
        ViewModel {
            phase,
            choices: Vec::new(),
            selected: HashSet::new(),
            can_confirm: false,
            install_can_start: false,
            install: install(step, total, label),
            verify_game: None,
            downloads: Vec::new(),
            download_status: None,
            finish_actions: Vec::new(),
        }
    }

    /// A fixture with the verify-game gate in the given stage.
    fn verify_view(stage: VerifyGameStage) -> ViewModel {
        let mut view = view(Phase::VerifyGame, 2, 3, Some("verify_game"));
        view.verify_game = Some(VerifyGameView {
            exe: "anomaly.exe".to_string(),
            stage,
        });
        view
    }

    // ---- pure helpers -----------------------------------------------------

    #[test]
    fn step_line_is_one_based_and_names_the_running_step() {
        assert_eq!(
            step_line(&install(1, 3, Some("install_sss23"))),
            "Step 2: install_sss23"
        );
        assert_eq!(step_line(&install(0, 3, Some("base"))), "Step 1: base");
        // No running step yet: a static placeholder, never a fabricated name.
        assert_eq!(step_line(&install(0, 3, None)), "Step 1: waiting to start");
    }

    #[test]
    fn overall_fraction_is_completed_over_total_and_clamps() {
        assert_eq!(overall_fraction(&install(0, 3, None)), 0.0);
        assert_eq!(overall_fraction(&install(1, 4, None)), 0.25);
        assert_eq!(overall_fraction(&install(3, 3, None)), 1.0);
        assert_eq!(overall_fraction(&install(0, 0, None)), 0.0, "no steps");
        assert_eq!(overall_fraction(&install(5, 3, None)), 1.0, "clamped");
    }

    /// The running step's percentage smooths the overall bar.
    #[test]
    fn overall_fraction_smooths_the_running_step_percent() {
        // Step 1 of 4 at 50% -> (1 + 0.5) / 4 = 0.375.
        assert_eq!(
            overall_fraction(&install_with(1, 4, None, Some(50), None)),
            0.375
        );
        // `percent: None` behaves like 0 (the bar does not jump until reported).
        assert_eq!(overall_fraction(&install(1, 4, None)), 0.25);
        // A completed step cannot exceed the overall bar.
        assert_eq!(
            overall_fraction(&install_with(1, 4, None, Some(100), None)),
            0.5
        );
    }

    #[test]
    fn bar_text_shows_completed_over_the_denominator() {
        assert_eq!(bar_text(&install(1, 3, Some("x"))), "1/3");
        assert_eq!(bar_text(&install(0, 7, None)), "0/7");
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

    fn recorded() -> Option<(egui::Rect, bool)> {
        RECORDED.with(|recorded| {
            recorded
                .borrow()
                .iter()
                .find(|entry| entry.id == CONTINUE_KEY)
                .map(|entry| (entry.rect, entry.enabled))
        })
    }

    /// The Start-game button `draw` laid out this frame, as `(rect, enabled)`.
    fn recorded_start() -> Option<(egui::Rect, bool)> {
        RECORDED.with(|recorded| {
            recorded
                .borrow()
                .iter()
                .find(|entry| entry.id == START_GAME_KEY)
                .map(|entry| (entry.rect, entry.enabled))
        })
    }

    /// Whether the Continue button was laid out enabled this frame.
    fn button_enabled(ctx: &egui::Context, view: &ViewModel) -> bool {
        draw_frame(ctx, view, Vec::new());
        recorded().expect("the Continue button was laid out").1
    }

    fn pointer(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// Lays out one frame, finds the Continue button, then drives a real pointer
    /// hover, press and release at its centre and returns the events `draw`
    /// emitted.
    fn click(ctx: &egui::Context, view: &ViewModel) -> Vec<Event> {
        draw_frame(ctx, view, Vec::new());
        let pos = recorded()
            .expect("the Continue button was laid out")
            .0
            .center();
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

    /// A fixture shows the overall bar and the `Step [i]:` info line.
    #[test]
    fn renders_the_overall_bar_and_step_info_line() {
        let ctx = egui::Context::default();
        let view = view(Phase::Installing, 1, 3, Some("install_sss23"));
        let texts = paint(&ctx, &view);

        assert!(
            painted(&texts, "Step 2: install_sss23"),
            "the info line must read `Step [i]: [step information]`: {texts:?}"
        );
        assert!(
            painted(&texts, "1/3"),
            "the bar must show completed/total (denominator = applicable steps): {texts:?}"
        );
    }

    /// The live detail line is rendered directly under the bar.
    #[test]
    fn renders_the_live_detail_line() {
        let ctx = egui::Context::default();
        let mut view = view(Phase::Installing, 1, 3, Some("install_base_anomaly"));
        view.install = install_with(
            1,
            3,
            Some("install_base_anomaly"),
            Some(47),
            Some("Extracting 47% | Big\\sub\\file.bin"),
        );
        let texts = paint(&ctx, &view);
        assert!(
            painted(&texts, "Extracting 47% | Big\\sub\\file.bin"),
            "the detail line must be painted: {texts:?}"
        );
    }

    /// During normal pipeline execution no Continue button is drawn.
    #[test]
    fn no_button_during_normal_pipeline_execution() {
        let ctx = egui::Context::default();
        let running = view(Phase::Installing, 1, 3, Some("install_base_anomaly"));
        draw_frame(&ctx, &running, Vec::new());
        assert!(
            recorded().is_none(),
            "no Continue button during normal pipeline execution"
        );
    }

    /// After the download completes (`install_can_start`) the Continue button
    /// appears and starts the pipeline.
    #[test]
    fn post_download_continue_starts_the_pipeline() {
        let ctx = egui::Context::default();
        let mut ready = view(Phase::Installing, 0, 3, None);
        ready.install_can_start = true;

        assert!(
            button_enabled(&ctx, &ready),
            "Continue is enabled once the download completes"
        );
        let events = click(&ctx, &ready);
        assert_eq!(
            events,
            vec![Event::BeginInstall],
            "clicking the post-download Continue starts the pipeline"
        );
    }

    /// Problem 6: WaitingToStart paints the verify message and a Start-game
    /// button, and no Continue button.
    #[test]
    fn verify_waiting_shows_the_message_and_start_game() {
        let ctx = egui::Context::default();
        let waiting = verify_view(VerifyGameStage::WaitingToStart);

        let texts = paint(&ctx, &waiting);
        assert!(
            painted(&texts, VERIFY_MESSAGE),
            "the verify message must be painted: {texts:?}"
        );

        draw_frame(&ctx, &waiting, Vec::new());
        assert!(
            recorded().is_none(),
            "no Continue button while waiting to start the game"
        );
        let start = recorded_start().expect("the Start game button was laid out");
        assert!(start.1, "Start game is enabled");
    }

    /// Problem 6: Running shows a DISABLED Continue and a click emits nothing.
    #[test]
    fn verify_running_shows_disabled_continue() {
        let ctx = egui::Context::default();
        let running = verify_view(VerifyGameStage::Running);

        assert!(
            !button_enabled(&ctx, &running),
            "Continue must be disabled while the game runs"
        );
        let events = click(&ctx, &running);
        assert!(
            events.is_empty(),
            "a disabled Continue must emit nothing: {events:?}"
        );
    }

    /// Problem 6: Finished shows an enabled Continue that acknowledges the gate.
    #[test]
    fn verify_finished_shows_enabled_continue() {
        let ctx = egui::Context::default();
        let finished = verify_view(VerifyGameStage::Finished);

        assert!(
            button_enabled(&ctx, &finished),
            "Continue must be enabled once the game closes"
        );
        let events = click(&ctx, &finished);
        assert_eq!(
            events,
            vec![Event::Continue],
            "clicking Continue acknowledges the verify gate"
        );
    }
}
