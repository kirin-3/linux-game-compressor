use super::{egui, missing_tools, queue, size, style, Command, Row};
use egui_extras::{Column, TableBuilder};
use std::path::PathBuf;

#[derive(Default, Debug, PartialEq)]
struct Totals {
    input: u64,
    output: u64,
    estimated: bool,
    pending: bool,
}
impl Totals {
    fn add(&mut self, input: u64, output: Option<(u64, bool)>) {
        self.input = self.input.saturating_add(input);
        if let Some((bytes, estimate)) = output {
            self.output = self.output.saturating_add(bytes);
            self.estimated |= estimate;
        } else {
            self.pending = true;
        }
    }
}
fn result(row: &Row) -> Option<(u64, bool)> {
    if row.finished {
        if let Some(report) = &row.report {
            return Some((report.output_bytes, false));
        }
    }
    let key = if row.overrides.is_empty() {
        row.profile.as_str()
    } else {
        "custom"
    };
    row.estimates
        .get(key)
        .map(|estimate| (estimate.likely, true))
}

pub(super) fn show(
    ctx: &egui::Context,
    rows: &[Row],
    selected: &mut Option<PathBuf>,
    worker: &tokio::sync::mpsc::UnboundedSender<Command>,
    queue_worker: &tokio::sync::mpsc::UnboundedSender<queue::Command>,
) -> Option<(usize, i32)> {
    let mut action = None;
    egui::TopBottomPanel::top("queue_table")
        .resizable(true)
        .default_height(240.0)
        .min_height(140.0)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Your games");
                ui.weak(format!("{} in queue", rows.len()));
            });
            ui.add_space(4.0);
            if rows.is_empty() {
                egui::Frame::new()
                    .fill(style::Palette::current(ctx).card)
                    .corner_radius(10)
                    .inner_margin(24)
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.strong("Drop your games here");
                            ui.label("Choose one or more game folders to get started.");
                            if ui.button("Add games").clicked() {
                                let _ = worker.send(Command::Pick);
                            }
                        });
                    });
                return;
            }
            let table_height = (ui.available_height() - 32.0).max(40.0);
            // A soft selection keeps status badges readable on the selected row.
            let palette = style::Palette::current(ctx);
            ui.visuals_mut().selection.bg_fill = palette.accent.gamma_multiply(0.22);
            ui.visuals_mut().selection.stroke.color = ui.visuals().text_color();
            TableBuilder::new(ui)
                .max_scroll_height(table_height)
                .striped(true)
                .resizable(true)
                .sense(egui::Sense::click())
                .cell_layout(egui::Layout::top_down(egui::Align::Min))
                .column(Column::remainder().at_least(150.0))
                .column(Column::initial(130.0).at_least(80.0))
                .column(Column::initial(90.0))
                .column(Column::initial(190.0))
                .column(Column::initial(115.0))
                .column(Column::exact(45.0))
                .header(26.0, |mut h| {
                    for label in ["GAME", "ENGINE", "SIZE", "RESULT", "STATUS", ""] {
                        h.col(|ui| {
                            ui.weak(label);
                        });
                    }
                })
                .body(|body| {
                    body.rows(48.0, rows.len(), |mut r| {
                        let index = r.index();
                        let row = &rows[index];
                        r.set_selected(selected.as_ref() == Some(&row.path));
                        let menu = |ui: &mut egui::Ui, action: &mut Option<(usize, i32)>| {
                            if ui
                                .add_enabled(
                                    !row.running
                                        && index > 0
                                        && !rows[index.saturating_sub(1)].running,
                                    egui::Button::new("Move up"),
                                )
                                .clicked()
                            {
                                *action = Some((index, -1));
                                ui.close();
                            }
                            if ui
                                .add_enabled(
                                    !row.running
                                        && index + 1 < rows.len()
                                        && !rows.get(index + 1).is_some_and(|r| r.running),
                                    egui::Button::new("Move down"),
                                )
                                .clicked()
                            {
                                *action = Some((index, 1));
                                ui.close();
                            }
                            if ui
                                .add_enabled(!row.running, egui::Button::new("Remove"))
                                .clicked()
                            {
                                *action = Some((index, 0));
                                ui.close();
                            }
                            if let Some(output) = &row.output {
                                if ui.button("Open output").clicked() {
                                    let _ = worker.send(Command::OpenOutput(output.clone()));
                                    ui.close();
                                }
                            }
                            if row.running && ui.button("Cancel").clicked() {
                                let _ = queue_worker.send(queue::Command::Cancel(row.path.clone()));
                                ui.close();
                            }
                        };
                        r.col(|ui| {
                            let name = row
                                .path
                                .file_name()
                                .unwrap_or(row.path.as_os_str())
                                .to_string_lossy();
                            ui.add_space(4.0);
                            ui.add(
                                egui::Label::new(egui::RichText::new(name).strong())
                                    .truncate()
                                    .selectable(false),
                            )
                            .on_hover_text(row.path.display().to_string());
                        });
                        r.col(|ui| {
                            ui.add_space(4.0);
                            if let Some(info) = &row.inspection {
                                ui.label(format!(
                                    "{} {}",
                                    info.result.engine.name,
                                    info.result.engine.version.as_deref().unwrap_or("")
                                ));
                            } else {
                                ui.weak("Checking…");
                            }
                        });
                        r.col(|ui| {
                            ui.add_space(4.0);
                            if let Some(info) = &row.inspection {
                                ui.label(size(i128::from(info.bytes)));
                            } else {
                                ui.weak("—");
                            }
                        });
                        r.col(|ui| {
                            ui.add_space(4.0);
                            if row.running {
                                let p = &row.progress;
                                let f = if p.total == 0 {
                                    0.0
                                } else {
                                    p.done as f32 / p.total as f32
                                };
                                ui.vertical(|ui| {
                                    ui.add(egui::ProgressBar::new(f).show_percentage());
                                    ui.small(format!(
                                        "{} / {} · {} saved",
                                        p.done,
                                        p.total,
                                        size(p.bytes_saved)
                                    ));
                                });
                            } else if let Some(report) =
                                row.report.as_ref().filter(|_| row.finished)
                            {
                                let percent = if report.input_bytes == 0 {
                                    0.0
                                } else {
                                    (report.input_bytes as f64 - report.output_bytes as f64) * 100.0
                                        / report.input_bytes as f64
                                };
                                ui.label(format!(
                                    "{} · {percent:.0}% saved",
                                    size(i128::from(report.output_bytes))
                                ));
                            } else if let Some((bytes, _)) = result(row) {
                                let key = if row.overrides.is_empty() {
                                    row.profile.as_str()
                                } else {
                                    "custom"
                                };
                                let estimate = &row.estimates[key];
                                let input = row.inspection.as_ref().map_or(0, |i| i.bytes);
                                let percent = if input == 0 {
                                    0.0
                                } else {
                                    (1.0 - bytes as f64 / input as f64) * 100.0
                                };
                                ui.vertical(|ui| {
                                    ui.label(format!(
                                        "~{} · {percent:.0}% smaller",
                                        size(bytes.into())
                                    ));
                                    ui.small(format!(
                                        "{}–{}",
                                        size(estimate.low.into()),
                                        size(estimate.high.into())
                                    ));
                                });
                            } else {
                                ui.weak(if row.estimating {
                                    "Estimating…"
                                } else {
                                    "—"
                                });
                            }
                        });
                        r.col(|ui| {
                            let p = style::Palette::current(ctx);
                            let missing = row
                                .inspection
                                .as_ref()
                                .map(|i| missing_tools(&i.result))
                                .unwrap_or_default();
                            let show_missing = !missing.is_empty()
                                && matches!(row.status.as_str(), "Ready" | "Checking…");
                            let has_failures =
                                row.report.as_ref().is_some_and(|r| !r.failures.is_empty());
                            let c = if row.status.to_lowercase().contains("fail") {
                                p.error
                            } else if row.status.starts_with("Skipped:")
                                || !missing.is_empty()
                                || matches!(
                                    row.destination,
                                    Some(compressor_app::DestinationState::Blocked(_))
                                )
                                || row.status.to_lowercase().contains("warning")
                                || has_failures
                            {
                                p.warning
                            } else if row.finished {
                                p.success
                            } else {
                                p.accent
                            };
                            ui.add_space(2.0);
                            ui.vertical(|ui| {
                                let label = if !row.running && show_missing {
                                    "Needs tools"
                                } else if !row.running
                                    && !row.finished
                                    && matches!(
                                        row.destination,
                                        Some(compressor_app::DestinationState::Blocked(_))
                                    )
                                {
                                    "Blocked"
                                } else if row.status.starts_with("Skipped:") {
                                    "Skipped"
                                } else if row.status.starts_with("Failed:") {
                                    "Failed"
                                } else {
                                    &row.status
                                };
                                style::badge(ui, label, c);
                                if !missing.is_empty() {
                                    ui.small(missing.join(", "));
                                } else if !row.running {
                                    match &row.destination {
                                        Some(compressor_app::DestinationState::Resumes) => {
                                            ui.small("Resumes");
                                        }
                                        Some(
                                            compressor_app::DestinationState::AlreadyCompressed,
                                        ) => {
                                            ui.small("Already compressed");
                                        }
                                        Some(compressor_app::DestinationState::Blocked(_)) => {
                                            ui.small("Pick another output");
                                        }
                                        _ => {}
                                    }
                                }
                            });
                            if row.running && ui.small_button("×").clicked() {
                                let _ = queue_worker.send(queue::Command::Cancel(row.path.clone()));
                            }
                        });
                        r.col(|ui| {
                            ui.menu_button("\u{2026}", |ui| menu(ui, &mut action));
                        });
                        let response = r.response();
                        if response.clicked() {
                            *selected = Some(row.path.clone());
                        }
                        response.context_menu(|ui| menu(ui, &mut action));
                    })
                });
            let mut totals = Totals::default();
            for row in rows {
                totals.add(row.inspection.as_ref().map_or(0, |i| i.bytes), result(row));
            }
            ui.separator();
            if totals.pending {
                ui.weak(format!(
                    "Total input {} · estimating output…",
                    size(totals.input.into())
                ));
            } else {
                let saved = i128::from(totals.input) - i128::from(totals.output);
                let percent = if totals.input == 0 {
                    0.0
                } else {
                    saved as f64 * 100.0 / totals.input as f64
                };
                ui.strong(format!(
                    "Total  {} in  ·  {}{} out  ·  {} saved ({percent:.0}%)",
                    size(totals.input.into()),
                    if totals.estimated { "~" } else { "" },
                    size(totals.output.into()),
                    size(saved)
                ));
            }
        });
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn totals_mix_actual_and_estimated_results() {
        let mut total = Totals::default();
        total.add(100, Some((50, false)));
        total.add(200, Some((90, true)));
        total.add(300, Some((120, true)));
        assert_eq!(
            total,
            Totals {
                input: 600,
                output: 260,
                estimated: true,
                pending: false
            }
        );
    }
}
