//! Thin `eframe`/`egui` shell for the Zona Installer.
//!
//! This binary only renders the library's plain-data [`ViewModel`] and forwards
//! [`Event`]s to [`App::handle`]; it contains no business logic.

// Release builds must not flash a console; debug builds keep the terminal.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ui;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui;
use zona_installer::app::{App, Event, Phase, ViewModel};
use zona_installer::error::AppError;

/// The GUI shell state: a running core, or the error that prevented startup.
enum Shell {
    /// The application core started; its view model is rendered each frame.
    Running(Box<App>),
    /// Startup failed before an [`App`] existed; render the popup from the error.
    StartupFailed(AppError),
}

/// The contents of the fatal support popup, owned so it can be rendered without
/// borrowing the core.
struct FatalView {
    message: String,
    error_id: String,
    log_path: Option<PathBuf>,
    restartable: bool,
}

/// The `eframe` application: holds the core and forwards events to it.
struct ZonaInstallerApp {
    shell: Shell,
}

impl eframe::App for ZonaInstallerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut events: Vec<Event> = Vec::new();
        let mut fatal: Option<FatalView> = None;

        match &mut self.shell {
            Shell::Running(app) => {
                // Repaint promptly when worker progress arrives.
                if !app.drain_progress().is_empty() {
                    ctx.request_repaint();
                }
                let view = app.view_model();
                egui::CentralPanel::default().show(ctx, |ui| {
                    draw_screen(ui, view, &mut events);
                });
                if let Phase::Fatal(state) = &view.phase {
                    fatal = Some(FatalView {
                        message: state.message.clone(),
                        error_id: state.error_id.clone(),
                        log_path: state.log_path.clone(),
                        restartable: state.restartable,
                    });
                }
            }
            Shell::StartupFailed(error) => {
                fatal = Some(FatalView {
                    message: error.to_string(),
                    error_id: error.error_id().as_str().to_string(),
                    log_path: error.log_path().map(Path::to_path_buf),
                    // Startup failures can't be restarted from scratch.
                    restartable: false,
                });
            }
        }

        if let Some(fatal) = &fatal {
            egui::Window::new("Something went wrong")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| draw_fatal(ui, fatal, &mut events));
        }

        // Forward the user's events to the core; errors are already recorded as
        // `Phase::Fatal` by `App::handle`.
        if let Shell::Running(app) = &mut self.shell {
            for event in events {
                let _ = app.handle(event);
            }
            // Close the window when the core asks (finish or relocation hand-off).
            if app.should_close() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

/// Renders the screen for the current phase (render-only; no logic).
fn draw_screen(ui: &mut egui::Ui, view: &ViewModel, events: &mut Vec<Event>) {
    match &view.phase {
        Phase::Choices => ui::choice::draw(ui, view, events),
        Phase::Downloading => ui::download::draw(ui, view, events),
        Phase::Installing | Phase::VerifyGame => ui::install::draw(ui, view, events),
        Phase::Finished => ui::finish::draw(ui, view, events),
        // Launcher: the three-option window shown for a `--launcher` start.
        Phase::Launcher { .. } => ui::launcher::draw(ui, view, events),
        // Relocation: the drive picker and the "Zona already exists" prompt.
        Phase::Relocation { .. } | Phase::RelocationTargetExists { .. } => {
            ui::relocation::draw(ui, view, events);
        }
        // Transient: the pre-flight outcome has not been applied yet.
        Phase::Preflight => {
            ui.label("Preparing the installer...");
        }
        // The fatal state is rendered as the support popup (see `update`).
        Phase::Fatal(_) => {}
    }
}

/// Renders the fatal support popup body: message, stable error id, log path
/// (when one exists) and the restart button for restartable failures.
fn draw_fatal(ui: &mut egui::Ui, fatal: &FatalView, events: &mut Vec<Event>) {
    ui.heading("Installation failed");
    ui.label("An unexpected error occurred. Please contact support.");
    ui.add_space(8.0);
    ui.label(&fatal.message);
    ui.label(format!("Error id: {}", fatal.error_id));
    if let Some(path) = &fatal.log_path {
        ui.label(format!("Log: {}", path.display()));
    }
    if fatal.restartable {
        ui.add_space(8.0);
        if ui.button("Restart install from scratch").clicked() {
            events.push(Event::RestartInstall);
        }
    }
}

fn main() -> eframe::Result {
    let result = run();
    if let Err(error) = &result {
        report_startup_failure(&error.to_string());
    }
    result
}

/// A release build has no console, so a failure to create the window would
/// vanish silently; leave a note at `%TEMP%\zona-installer-startup-error.txt`.
fn report_startup_failure(message: &str) {
    let path = std::env::temp_dir().join("zona-installer-startup-error.txt");
    let _ = std::fs::write(
        path,
        format!("The installer could not open its window:\n{message}\n"),
    );
}

/// The application icon
static APP_ICON_ICO: &[u8] = include_bytes!("../dependencies/icon.ico");

/// Decodes the window icon from the embedded `.ico` bytes.
///
/// The shipped icon is a single PNG-encoded image; [`icon_data_from_ico`] picks
/// the largest PNG entry and decodes it. Returns `None` when the bytes are not
/// a decodable PNG-based icon, in which case the window keeps the OS default.
fn window_icon() -> Option<egui::IconData> {
    icon_data_from_ico(APP_ICON_ICO)
}

/// Extracts the largest PNG-encoded image from an `.ico` byte buffer.
///
/// ICO files pack an `ICONDIR` header (reserved `u16`, type `u16` == 1, count
/// `u16`) followed by 16-byte directory entries that locate each image; modern
/// icons store those images as PNG. Only PNG entries are decoded here.
fn icon_data_from_ico(bytes: &[u8]) -> Option<egui::IconData> {
    if bytes.len() < 6 {
        return None;
    }
    if u16::from_le_bytes([bytes[0], bytes[1]]) != 0 || u16::from_le_bytes([bytes[2], bytes[3]]) != 1
    {
        return None;
    }
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;

    let mut best: Option<(&[u8], u64)> = None;
    for index in 0..count {
        let entry_bytes = bytes.get(6 + index * 16..6 + (index + 1) * 16)?;
        // A stored dimension of 0 means 256.
        let width = if entry_bytes[0] == 0 { 256 } else { entry_bytes[0] as u64 };
        let height = if entry_bytes[1] == 0 { 256 } else { entry_bytes[1] as u64 };
        let size = u32::from_le_bytes(entry_bytes[8..12].try_into().ok()?) as usize;
        let offset = u32::from_le_bytes(entry_bytes[12..16].try_into().ok()?) as usize;
        let data = bytes.get(offset..offset.checked_add(size)?)?;
        if data.starts_with(b"\x89PNG\r\n\x1a\n") {
            let area = width * height;
            if best.as_ref().map_or(true, |(_, best_area)| area > *best_area) {
                best = Some((data, area));
            }
        }
    }

    let (png, _) = best?;
    eframe::icon_data::from_png_bytes(png).ok()
}

fn run() -> eframe::Result {
    let viewport = match window_icon() {
        Some(icon) => egui::ViewportBuilder::default().with_icon(icon),
        None => egui::ViewportBuilder::default(),
    };
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Zona Installer",
        options,
        Box::new(|cc| {
            let shell = match App::new() {
                Ok(mut app) => {
                    // Wake the UI when worker progress arrives.
                    let ctx = cc.egui_ctx.clone();
                    app.set_repaint(Arc::new(move || ctx.request_repaint()));
                    Shell::Running(Box::new(app))
                }
                Err(error) => Shell::StartupFailed(error),
            };
            Ok(Box::new(ZonaInstallerApp { shell }))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use zona_installer::app::InstallProgress;

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

    // ---- headless render -------------------------------------------------

    fn input() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1024.0, 768.0),
            )),
            ..Default::default()
        }
    }

    fn collect_shape(shape: &egui::epaint::Shape, out: &mut Vec<String>) {
        match shape {
            egui::epaint::Shape::Text(text) => out.push(text.galley.text().to_owned()),
            egui::epaint::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_shape(shape, out);
                }
            }
            _ => {}
        }
    }

    /// Drives the real `draw_screen` through a headless `egui::Context` and
    /// returns the text runs it painted.
    fn painted_texts(view: &ViewModel) -> Vec<String> {
        let ctx = egui::Context::default();
        let run = |ctx: &egui::Context| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let mut events = Vec::new();
                draw_screen(ui, view, &mut events);
            });
        };
        // Warm up the font atlas so the capture frame lays out every glyph.
        let _ = ctx.run(input(), |ctx| run(ctx));
        let output = ctx.run(input(), |ctx| run(ctx));
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_shape(&clipped.shape, &mut texts);
        }
        texts
    }

    /// The relocation phases must route to a real screen, not the bare
    /// "Preparing the installer..." placeholder (which had no drive list and no
    /// way to emit `Event::DriveChosen`).
    #[test]
    fn relocation_phases_route_to_a_real_screen_not_the_placeholder() {
        let texts = painted_texts(&view(Phase::Relocation {
            drives: vec!["C:\\".to_string(), "D:\\".to_string()],
        }));
        assert!(
            texts.iter().any(|text| text == "C:\\"),
            "the drive picker must paint its drives: {texts:?}"
        );
        assert!(
            !texts
                .iter()
                .any(|text| text == "Preparing the installer..."),
            "a relocation phase must not fall through to the placeholder: {texts:?}"
        );

        let prompt = painted_texts(&view(Phase::RelocationTargetExists {
            drive: "C:\\".to_string(),
        }));
        assert!(
            prompt.iter().any(|text| text == "Delete and continue"),
            "the exists-prompt must paint its choices: {prompt:?}"
        );
    }
}
