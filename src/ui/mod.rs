//! The GUI screens.
//!
//! Each screen is a data-only renderer: it reads [`ViewModel`] and pushes
//! [`Event`]s back to the shell, which forwards them to `zona_installer::app::App`.
//!
//! [`ViewModel`]: zona_installer::app::ViewModel
//! [`Event`]: zona_installer::app::Event

pub mod choice;
pub mod download;
pub mod finish;
pub mod install;
pub mod launcher;
pub mod relocation;

use eframe::egui;

/// The credits notice shown at the bottom of every screen.
///
/// The shell renders it in a `TopBottomPanel::bottom`, which pins it to the
/// bottom edge regardless of the active phase's content height.
pub fn footer(ui: &mut egui::Ui) {
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.label("S.T.A.L.K.E.R. Zona windows installer. Made by");
        ui.hyperlink_to("Gtair", "https://github.com/gtair");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every text run painted this frame.
    fn painted_texts(shapes: &[egui::epaint::ClippedShape]) -> Vec<String> {
        fn walk(shape: &egui::epaint::Shape, out: &mut Vec<String>) {
            match shape {
                egui::epaint::Shape::Text(text) => out.push(text.galley.text().to_owned()),
                egui::epaint::Shape::Vec(shapes) => {
                    for shape in shapes {
                        walk(shape, out);
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        for clipped in shapes {
            walk(&clipped.shape, &mut out);
        }
        out
    }

    #[test]
    fn footer_paints_the_credits_notice() {
        let ctx = egui::Context::default();
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1024.0, 800.0),
            )),
            ..Default::default()
        };
        // Warm up the font atlas so the capture frame lays out every glyph.
        let _ = ctx.run(input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| footer(ui));
        });
        let output = ctx.run(input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| footer(ui));
        });

        let texts = painted_texts(&output.shapes);
        assert!(
            texts
                .iter()
                .any(|text| text == "S.T.A.L.K.E.R. Zona windows installer. Made by"),
            "the credits notice is painted"
        );
        assert!(
            texts.iter().any(|text| text == "Gtair"),
            "the credits link is painted"
        );
    }
}
