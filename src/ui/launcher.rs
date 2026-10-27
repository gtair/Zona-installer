//! The launcher screen: three choices shown when the installer starts from its
//! canonical home.
//!
//! Data-only renderer: it reads `Phase::Launcher { game_present }` and forwards
//! `Event::LaunchInstaller` / `Event::LaunchMo2` / `Event::LaunchZona`. The MO2
//! and Zona buttons are disabled when the game executable is not installed.

use eframe::egui;
use zona_installer::app::{Event, Phase, ViewModel};

/// Test-only ids for the three buttons.
#[cfg(test)]
const INSTALLER_KEY: &str = "__launcher_installer__";
#[cfg(test)]
const MO2_KEY: &str = "__launcher_mo2__";
#[cfg(test)]
const ZONA_KEY: &str = "__launcher_zona__";

/// Renders the launcher screen, or nothing for a phase it does not own.
pub fn draw(ui: &mut egui::Ui, view: &ViewModel, events: &mut Vec<Event>) {
    #[cfg(test)]
    clear_recorded();

    let game_present = match &view.phase {
        Phase::Launcher { game_present } => *game_present,
        _ => return,
    };

    // Center a constrained column so the launcher reads as a small, focused
    // dialog rather than a bare list of buttons.
    ui.vertical_centered(|ui| {
        ui.add_space(24.0);

        ui.label(egui::RichText::new("S.T.A.L.K.E.R. Zona").size(32.0).strong());
        ui.label(egui::RichText::new("Choose what to launch").size(15.0).weak());
        ui.add_space(24.0);

        let width = 260.0;
        let button = |text: &str| egui::Button::new(text).min_size(egui::vec2(width, 44.0));

        let installer = ui.add_enabled(true, button("Launch installer"));
        #[cfg(test)]
        record_rect(INSTALLER_KEY, installer.rect, installer.enabled());
        if installer.clicked() {
            events.push(Event::LaunchInstaller);
        }

        ui.add_space(8.0);

        let mo2 = ui.add_enabled(game_present, button("Launch Mod Organizer"));
        #[cfg(test)]
        record_rect(MO2_KEY, mo2.rect, mo2.enabled());
        if mo2.clicked() {
            events.push(Event::LaunchMo2);
        }

        ui.add_space(8.0);

        let zona = ui.add_enabled(game_present, button("Launch Zona"));
        #[cfg(test)]
        record_rect(ZONA_KEY, zona.rect, zona.enabled());
        if zona.clicked() {
            events.push(Event::LaunchZona);
        }

        if !game_present {
            ui.add_space(12.0);
            ui.label(
                egui::RichText::new("The game is not installed yet — run the installer first.")
                    .size(12.0)
                    .weak(),
            );
        }
    });
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

    fn view(game_present: bool) -> ViewModel {
        ViewModel {
            phase: Phase::Launcher { game_present },
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

    #[test]
    fn launcher_renders_three_buttons_and_greys_the_game_options_when_absent() {
        let ctx = egui::Context::default();
        draw_frame(&ctx, &view(false), Vec::new());

        let ids: Vec<(String, bool)> = recorded()
            .into_iter()
            .map(|(id, _, enabled)| (id, enabled))
            .collect();
        assert_eq!(
            ids,
            vec![
                (INSTALLER_KEY.to_string(), true),
                (MO2_KEY.to_string(), false),
                (ZONA_KEY.to_string(), false),
            ],
            "installer enabled, MO2/Zona disabled when the game is absent"
        );
    }

    #[test]
    fn launcher_enables_all_buttons_when_the_game_is_present() {
        let ctx = egui::Context::default();
        draw_frame(&ctx, &view(true), Vec::new());

        let ids: Vec<(String, bool)> = recorded()
            .into_iter()
            .map(|(id, _, enabled)| (id, enabled))
            .collect();
        assert_eq!(
            ids,
            vec![
                (INSTALLER_KEY.to_string(), true),
                (MO2_KEY.to_string(), true),
                (ZONA_KEY.to_string(), true),
            ],
            "all three options enabled when the game is installed"
        );
    }

    #[test]
    fn launcher_buttons_forward_the_right_events() {
        let ctx = egui::Context::default();
        let view = view(true);

        assert_eq!(click(&ctx, &view, INSTALLER_KEY), vec![Event::LaunchInstaller]);
        assert_eq!(click(&ctx, &view, MO2_KEY), vec![Event::LaunchMo2]);
        assert_eq!(click(&ctx, &view, ZONA_KEY), vec![Event::LaunchZona]);
    }

    #[test]
    fn a_foreign_phase_renders_nothing() {
        let ctx = egui::Context::default();
        let view = ViewModel {
            phase: Phase::Choices,
            ..view(false)
        };
        let events = draw_frame(&ctx, &view, Vec::new());
        assert!(recorded().is_empty(), "no widget for a foreign phase");
        assert!(events.is_empty(), "a foreign phase emits nothing");
    }
}
