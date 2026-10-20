//! Screen 2: download progress.
//!
//! A data-only renderer: it reads the [`ViewModel`] and draws the scrollable
//! per-file list. Consecutive parts of a split archive (`.001`, `.002`, ...) are
//! drawn as one row with an indented sub-line per part.
//!
//! A row's bar is a real fraction from `progress.pct` with a `done / total`
//! caption; while `progress` is `None` the bar is indeterminate. A split archive's
//! overall bar is the mean of its parts' percentages.

use eframe::egui;
use zona_installer::app::{DownloadRow, DownloadStatus, Event, ViewModel};

/// Draws the download screen and pushes any needed events into `events`.
///
/// Renders one row per downloaded file, except that the parts of a split
/// archive are collapsed into a single row with an indented sub-line per part.
///
/// No [`Event`] is emitted: this screen offers no button or toggle. `_events`
/// stays in the signature because it is the shared screen contract the shell
/// calls.
pub fn draw(ui: &mut egui::Ui, view: &ViewModel, _events: &mut Vec<Event>) {
    ui.heading("Downloading");
    ui.label("Your selected files are being downloaded. A split archive is shown as one entry.");

    // What the stage is doing right now (hashing, resolving, downloading, verifying).
    if let Some(status) = &view.download_status {
        ui.label(egui::RichText::new(status).italics());
    }

    ui.add_space(8.0);

    // Cap the list and let it scroll so trailing captions stay on screen.
    let list_height = (ui.available_height() - 48.0).max(120.0);
    egui::ScrollArea::vertical()
        .max_height(list_height)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for group in group_rows(&view.downloads) {
                match group {
                    Group::Single(row) => draw_file_row(ui, row),
                    Group::Multipart { stem, parts } => draw_multipart_row(ui, stem, &parts),
                }
                ui.add_space(6.0);
            }
        });

    ui.add_space(4.0);
    ui.separator();

    // Resume-on-failure is fixed copy: aria2c's `--continue` guarantees it, and
    // resume is carried by the byte counts, not a separate flag.
    ui.label("If a download fails or is interrupted, it resumes automatically.");
}

/// The bar fraction for one row: `pct / 100`, `1.0` when Completed, or `None`
/// when there is no progress record or no percentage.
fn fraction(row: &DownloadRow) -> Option<f32> {
    if row.status == DownloadStatus::Completed {
        return Some(1.0);
    }
    row.progress
        .as_ref()
        .and_then(|progress| progress.pct)
        .map(|pct| pct as f32 / 100.0)
}

/// The overall bar fraction for a multipart group: the mean of the parts'
/// percentages.
fn group_fraction(parts: &[&DownloadRow]) -> Option<f32> {
    if parts.is_empty() {
        return None;
    }
    let mut sum = 0.0;
    for part in parts {
        sum += fraction(part).unwrap_or(0.0);
    }
    Some(sum / parts.len() as f32)
}

fn group_status(parts: &[&DownloadRow]) -> DownloadStatus {
    if parts.is_empty() {
        return DownloadStatus::Queued;
    }
    if parts.iter().all(|p| p.status == DownloadStatus::Completed) {
        return DownloadStatus::Completed;
    }
    if parts.iter().any(|p| p.status == DownloadStatus::Failed) {
        return DownloadStatus::Failed;
    }
    if parts
        .iter()
        .any(|p| p.status == DownloadStatus::Downloading)
    {
        return DownloadStatus::Downloading;
    }
    if parts.iter().any(|p| p.status == DownloadStatus::Checking) {
        return DownloadStatus::Checking;
    }
    if parts.iter().any(|p| p.status == DownloadStatus::Verifying) {
        return DownloadStatus::Verifying;
    }
    DownloadStatus::Queued
}

fn status_badge(status: DownloadStatus) -> (&'static str, egui::Color32) {
    match status {
        DownloadStatus::Queued => ("Queued", egui::Color32::from_gray(140)),
        DownloadStatus::Checking => ("Checking...", egui::Color32::from_rgb(100, 180, 255)),
        DownloadStatus::Downloading => ("Downloading", egui::Color32::from_rgb(80, 160, 240)),
        DownloadStatus::Verifying => ("Verifying...", egui::Color32::from_rgb(220, 180, 80)),
        DownloadStatus::Completed => ("Completed", egui::Color32::from_rgb(80, 200, 120)),
        DownloadStatus::Failed => ("Failed", egui::Color32::from_rgb(240, 80, 80)),
    }
}

/// Draws one row's bar: a real fraction with a `done / total` caption, or
/// status text when queued/failed/checking.
fn draw_bar(ui: &mut egui::Ui, row: &DownloadRow, bar_fraction: Option<f32>) {
    let fraction = bar_fraction.unwrap_or(0.0);
    match row.status {
        DownloadStatus::Queued => {
            ui.add(egui::ProgressBar::new(0.0).text("Queued"));
        }
        DownloadStatus::Checking => {
            ui.add(
                egui::ProgressBar::new(0.0)
                    .animate(true)
                    .text("Checking existing file..."),
            );
        }
        DownloadStatus::Downloading => {
            if let Some(progress) = &row.progress {
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .text(format!("{} / {}", progress.done, progress.total)),
                );
            } else {
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .animate(true)
                        .text("Downloading..."),
                );
            }
        }
        DownloadStatus::Verifying => {
            ui.add(
                egui::ProgressBar::new(1.0)
                    .animate(true)
                    .text("Verifying..."),
            );
        }
        DownloadStatus::Completed => {
            let caption = if let Some(progress) = &row.progress {
                if progress.total != "Done" && !progress.total.is_empty() {
                    format!("{} / {}", progress.done, progress.total)
                } else {
                    "Completed".to_string()
                }
            } else {
                "Completed".to_string()
            };
            ui.add(egui::ProgressBar::new(1.0).text(caption));
        }
        DownloadStatus::Failed => {
            ui.add(egui::ProgressBar::new(fraction).text("Failed"));
        }
    }
}

fn draw_file_row(ui: &mut egui::Ui, row: &DownloadRow) {
    ui.horizontal(|ui| {
        ui.label(&row.file);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (label, color) = status_badge(row.status);
            ui.label(egui::RichText::new(label).color(color).small());
        });
    });
    draw_bar(ui, row, fraction(row));
}

/// Draws one split-archive row: a header (toggle + archive name + status badge),
/// the overall bar at full width, then the collapsible per-part sub-lines
/// indented once.
///
/// The overall bar must sit directly under the archive name, and the parts must
/// be indented once, not twice; `egui::CollapsingHeader` auto-indents its body,
/// so the parts are drawn under a manual toggle plus a single `ui.indent`.
fn draw_multipart_row(ui: &mut egui::Ui, stem: &str, parts: &[&DownloadRow]) {
    let name = stem.strip_suffix('.').unwrap_or(stem);
    let avg = group_fraction(parts).unwrap_or(0.0);
    let status = group_status(parts);

    let bar_text = match status {
        DownloadStatus::Completed => "Completed".to_string(),
        DownloadStatus::Failed => "Failed".to_string(),
        DownloadStatus::Queued => "Queued".to_string(),
        DownloadStatus::Checking => "Checking...".to_string(),
        DownloadStatus::Verifying => "Verifying...".to_string(),
        DownloadStatus::Downloading => format!("{:.0}%", avg * 100.0),
    };

    // The toggle state lives in egui's temp data, keyed by the archive stem.
    let toggle_id = egui::Id::new(("multipart", stem));
    let is_open = ui.data_mut(|data| *data.get_temp_mut_or_insert_with(toggle_id, || true));

    // Header row: toggle + name + right-aligned status badge.
    ui.horizontal(|ui| {
        let arrow = if is_open { "▾" } else { "▸" };
        if ui.small_button(arrow).clicked() {
            ui.data_mut(|data| {
                let open = data.get_temp_mut_or_insert_with(toggle_id, || true);
                *open = !*open;
            });
        }
        ui.label(format!("{name} ({} parts)", parts.len()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (label, color) = status_badge(status);
            ui.label(egui::RichText::new(label).color(color).small());
        });
    });

    // The overall bar, full width, directly under the archive name.
    ui.add(egui::ProgressBar::new(avg).text(bar_text));

    // The parts, indented once.
    if is_open {
        ui.indent(("parts", stem), |ui| {
            for part in parts {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&part.file).small().weak());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (label, color) = status_badge(part.status);
                        ui.label(egui::RichText::new(label).color(color).small());
                    });
                });
                draw_bar(ui, part, fraction(part));
            }
        });
    }
}

/// One rendered row: a lone file, or a split archive collapsed to one row.
#[derive(Debug, PartialEq, Eq)]
enum Group<'a> {
    /// A file that is not part of a split archive.
    Single(&'a DownloadRow),
    /// All the parts of one split archive, in `view.downloads` order.
    Multipart {
        /// The filename up to and including the dot before the `.00N` suffix.
        stem: &'a str,
        /// The rows for each part.
        parts: Vec<&'a DownloadRow>,
    },
}

/// Groups `rows` into render rows.
///
/// Rows whose filename is a split-archive part (a trailing `.` + 3-or-more
/// digits) are grouped by their stem; every other row stays its own
/// [`Group::Single`]. First-appearance order is preserved.
fn group_rows(rows: &[DownloadRow]) -> Vec<Group<'_>> {
    let mut groups: Vec<Group<'_>> = Vec::new();
    for row in rows {
        match part_stem(&row.file) {
            None => groups.push(Group::Single(row)),
            Some(stem) => {
                let existing = groups.iter_mut().find_map(|group| match group {
                    Group::Multipart { stem: other, parts } if *other == stem => Some(parts),
                    _ => None,
                });
                match existing {
                    Some(parts) => parts.push(row),
                    None => groups.push(Group::Multipart {
                        stem,
                        parts: vec![row],
                    }),
                }
            }
        }
    }
    groups
}

/// The part-number stem of a split-archive filename, or `None`.
///
/// `zona_multipart.7z.001` -> `Some("zona_multipart.7z.")`. The suffix must be a
/// trailing `.` followed by 3 or more ASCII digits.
fn part_stem(file: &str) -> Option<&str> {
    let dot = file.rfind('.')?;
    let suffix = &file[dot + 1..];
    if suffix.len() >= 3 && suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        Some(&file[..=dot])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use zona_installer::app::{DownloadProgress, InstallProgress, Phase};

    fn row(file: &str) -> DownloadRow {
        DownloadRow {
            asset_id: file.to_string(),
            file: file.to_string(),
            status: DownloadStatus::Queued,
            progress: None,
        }
    }

    /// A row with a per-file progress record (the file name is the row key).
    fn row_with(file: &str, pct: Option<u32>, done: &str, total: &str) -> DownloadRow {
        DownloadRow {
            asset_id: file.to_string(),
            file: file.to_string(),
            status: if pct == Some(100) {
                DownloadStatus::Completed
            } else {
                DownloadStatus::Downloading
            },
            progress: Some(DownloadProgress {
                pct,
                done: done.to_string(),
                total: total.to_string(),
            }),
        }
    }

    /// The real `config.yaml` download shape: two single files, seven parts of
    /// `zona_multipart.7z`, then another single file.
    fn real_config_rows() -> Vec<DownloadRow> {
        let mut rows = vec![row("anomaly.7z"), row("mod_organizer.7z")];
        for part in 1..=7 {
            rows.push(row(&format!("zona_multipart.7z.{part:03}")));
        }
        rows.push(row("zona_profiles.7z"));
        rows
    }

    fn view(downloads: Vec<DownloadRow>) -> ViewModel {
        ViewModel {
            phase: Phase::Downloading,
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
            downloads,
            download_status: None,
            finish_actions: Vec::new(),
        }
    }

    // ---- pure grouping ----------------------------------------------------

    #[test]
    fn part_stem_recognizes_split_archive_parts_only() {
        assert_eq!(
            part_stem("zona_multipart.7z.001"),
            Some("zona_multipart.7z.")
        );
        assert_eq!(
            part_stem("zona_multipart.7z.007"),
            Some("zona_multipart.7z.")
        );
        assert_eq!(part_stem("a.b.c.0100"), Some("a.b.c."));
        assert_eq!(part_stem("anomaly.7z"), None);
        assert_eq!(part_stem("modded_exes.zip"), None);
        assert_eq!(part_stem("archive.7z.00"), None, "two digits is not a part");
        assert_eq!(part_stem("archive.7z.abc"), None, "non-digit suffix");
        assert_eq!(part_stem("nodot"), None);
    }

    #[test]
    fn group_rows_collapses_multipart_into_one_group_and_preserves_order() {
        let rows = real_config_rows();
        let groups = group_rows(&rows);

        assert_eq!(groups.len(), 4, "ten rows collapse into four render rows");
        assert_eq!(groups[0], Group::Single(&rows[0]));
        assert_eq!(groups[3], Group::Single(&rows[9]));

        match &groups[2] {
            Group::Multipart { stem, parts } => {
                assert_eq!(*stem, "zona_multipart.7z.");
                assert_eq!(parts.len(), 7);
                assert_eq!(parts[0].file, "zona_multipart.7z.001");
                assert_eq!(parts[6].file, "zona_multipart.7z.007");
            }
            other => panic!("expected one multipart group, got {other:?}"),
        }
    }

    #[test]
    fn group_rows_joins_parts_that_are_not_adjacent() {
        let rows = vec![row("z.7z.001"), row("plain.7z"), row("z.7z.002")];
        let groups = group_rows(&rows);

        assert_eq!(groups.len(), 2);
        match &groups[0] {
            Group::Multipart { stem, parts } => {
                assert_eq!(*stem, "z.7z.");
                assert_eq!(parts.len(), 2, "non-adjacent parts share one group");
            }
            other => panic!("expected a multipart group, got {other:?}"),
        }
        assert_eq!(groups[1], Group::Single(&rows[1]));
    }

    // ---- headless render --------------------------------------------------

    /// One text run the real `draw` painted this frame, with its top-left
    /// position (used to tell a row header from an indented sub-line).
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
    fn paint(view: &ViewModel) -> Vec<Painted> {
        let ctx = egui::Context::default();
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1024.0, 2000.0),
            )),
            ..Default::default()
        };
        // Warm up the font atlas so the capture frame lays out every glyph.
        let _ = ctx.run(input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let mut events = Vec::new();
                draw(ui, view, &mut events);
            });
        });
        let output = ctx.run(input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let mut events = Vec::new();
                draw(ui, view, &mut events);
            });
        });
        let mut texts = Vec::new();
        collect_text(&output.shapes, &mut texts);
        texts
    }

    fn pos_of(texts: &[Painted], label: &str) -> egui::Pos2 {
        texts
            .iter()
            .find(|(_, text)| text == label)
            .unwrap_or_else(|| panic!("expected a painted label {label:?}"))
            .0
    }

    /// Renders the list with multipart parts grouped into ONE row.
    #[test]
    fn renders_multipart_parts_grouped_into_one_row() {
        let texts = paint(&view(real_config_rows()));

        // Exactly one header for the archive, naming all seven parts.
        let header = "zona_multipart.7z (7 parts)";
        assert_eq!(
            texts.iter().filter(|(_, text)| text == header).count(),
            1,
            "the archive is one row, not seven"
        );

        // The archive header is rendered, and the seven part sub-lines are indented
        // and sit between the archive header and the next row's header.
        let base_x = pos_of(&texts, "anomaly.7z").x;
        let header_pos = pos_of(&texts, header);
        assert!(
            header_pos.x >= base_x,
            "the archive header is rendered as a header"
        );
        let next_row_y = pos_of(&texts, "zona_profiles.7z").y;

        let sub_lines: Vec<&Painted> = texts
            .iter()
            .filter(|(_, text)| text.starts_with("zona_multipart.7z.00"))
            .collect();
        assert_eq!(sub_lines.len(), 7, "one sub-line per part");
        for (index, (pos, text)) in sub_lines.iter().enumerate() {
            assert_eq!(text, &format!("zona_multipart.7z.{:03}", index + 1));
            assert!(
                pos.y > header_pos.y && pos.y < next_row_y,
                "part {text} belongs to the archive row block"
            );
            // One indent from the ROW's left edge (the header label itself is
            // offset by the toggle button, so the row edge is the reference).
            assert!(pos.x > base_x, "part {text} sits on an indented sub-line");
        }

        // None of the part filenames is drawn as a row header of its own.
        for part in 1..=7 {
            let name = format!("zona_multipart.7z.{part:03}");
            let is_own_row = texts
                .iter()
                .any(|(pos, text)| text == &name && (pos.x - base_x).abs() < 1.0);
            assert!(!is_own_row, "{name} must not be its own row");
        }

        // The single files are their own rows.
        for name in ["anomaly.7z", "mod_organizer.7z", "zona_profiles.7z"] {
            assert!(
                (pos_of(&texts, name).x - base_x).abs() < 1.0,
                "{name} is a row header"
            );
        }
    }

    /// The resume-on-failure note is rendered (fixed copy; resume is carried by
    /// the byte counts).
    #[test]
    fn renders_the_resume_on_failure_note() {
        let texts = paint(&view(real_config_rows()));
        assert!(
            texts.iter().any(|(_, text)| text
                == "If a download fails or is interrupted, it resumes automatically."),
            "the resume note is painted"
        );
    }

    // ---- bar math and per-file bars ---------------------------------------

    /// The pure `fraction` / `group_fraction` helpers are the bar math: a
    /// per-file fraction, a multipart mean, and `None` when no honest value
    /// exists.
    #[test]
    fn fraction_and_group_fraction_compute_the_bar_math() {
        let single = row_with("anomaly.7z", Some(25), "1MiB", "4MiB");
        assert_eq!(fraction(&single), Some(0.25), "pct 25 -> 0.25");

        // No record, or a record without a percentage: indeterminate.
        assert_eq!(fraction(&row("skipped.7z")), None, "no record -> None");
        assert_eq!(
            fraction(&row_with("nopct.7z", None, "0B", "0B")),
            None,
            "no percentage -> None"
        );

        let parts = [
            row_with("z.7z.001", Some(10), "a", "b"),
            row_with("z.7z.002", Some(20), "c", "d"),
            row_with("z.7z.003", Some(30), "e", "f"),
        ];
        let refs: Vec<&DownloadRow> = parts.iter().collect();
        assert_eq!(
            group_fraction(&refs),
            Some(0.2),
            "the group bar is the MEAN of 10, 20, 30"
        );

        // A part without a percentage contributes 0% to the group mean.
        let incomplete = [row_with("z.7z.001", Some(10), "a", "b"), row("z.7z.002")];
        let refs: Vec<&DownloadRow> = incomplete.iter().collect();
        assert_eq!(group_fraction(&refs), Some(0.05));
        assert_eq!(group_fraction(&[]), None, "an empty group is indeterminate");
    }

    /// A fixture with progress records renders a real per-file bar, the multipart
    /// group as one row with per-part sub-lines, and no caption for rows with no
    /// record.
    #[test]
    fn renders_real_per_file_bars_and_indeterminate_rows() {
        let downloads = vec![
            row_with("anomaly.7z", Some(25), "1MiB", "4MiB"),
            // No record: its bar stays indeterminate and paints no caption.
            row("mod_organizer.7z"),
            row_with("zona_multipart.7z.001", Some(10), "1MiB", "10MiB"),
            row_with("zona_multipart.7z.002", Some(20), "2MiB", "10MiB"),
            row_with("zona_multipart.7z.003", Some(30), "3MiB", "10MiB"),
        ];
        let texts = paint(&view(downloads));

        // The single file with a record paints its `done / total` caption.
        assert!(
            texts.iter().any(|(_, text)| text == "1MiB / 4MiB"),
            "the anomaly row paints its done / total caption"
        );

        // Each part's own caption is painted on its indented sub-line.
        let base_x = pos_of(&texts, "anomaly.7z").x;
        for caption in ["1MiB / 10MiB", "2MiB / 10MiB", "3MiB / 10MiB"] {
            let (pos, _) = texts
                .iter()
                .find(|(_, text)| text == caption)
                .unwrap_or_else(|| panic!("expected the part caption {caption:?}"));
            assert!(
                pos.x > base_x + 1.0,
                "part caption {caption:?} sits on an indented sub-line"
            );
        }

        // Exactly the rows with a percentage paint a `done / total` caption: the
        // indeterminate (no-record) row contributes none.
        let captions = texts
            .iter()
            .filter(|(_, text)| text.contains(" / "))
            .count();
        assert_eq!(
            captions, 4,
            "only the one single + three part rows paint a caption"
        );

        // The three parts are still one grouped row.
        assert!(
            texts
                .iter()
                .any(|(_, text)| text == "zona_multipart.7z (3 parts)"),
            "the split archive stays one row"
        );
    }
}
