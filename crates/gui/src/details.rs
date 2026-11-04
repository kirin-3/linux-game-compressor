use super::{egui, queue, settings_view, size, style, App, Command, Row};
use std::path::PathBuf;

pub(super) fn show(app: &mut App, ctx: &egui::Context) -> Option<PathBuf> {
    let mut changed = None;
    let others: Vec<_> = app
        .queue
        .iter()
        .filter(|r| app.selected.as_ref() != Some(&r.path))
        .filter_map(|r| r.output.clone())
        .collect();
    egui::CentralPanel::default().show(ctx, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            let Some(row) = app
                .queue
                .iter_mut()
                .find(|r| app.selected.as_ref() == Some(&r.path))
            else {
                ui.add_space(20.0);
                ui.vertical_centered(|ui| {
                    ui.heading("Select a game");
                    ui.colored_label(
                        style::Palette::current(ctx).muted,
                        "Choose a game above to review its settings and progress.",
                    );
                });
                return;
            };
            egui::Frame::new().inner_margin(16).show(ui, |ui| {
                header(ui, row, &app.worker.commands, &app.queue_worker.commands);
                inspection(ui, row);
                output(ui, row, &others, &app.worker.commands);
                result(ui, row);
                if row.running {
                    ui.strong("Processing");
                    for path in &row.progress.active {
                        ui.weak(path);
                    }
                }
                ui.add_enabled_ui(!row.running, |ui| {
                    ui.checkbox(&mut row.verify, "Verify this game");
                    if settings_view::show(ui, row) {
                        changed = Some(row.path.clone());
                    }
                });
            });
        });
    });
    changed
}

fn header(
    ui: &mut egui::Ui,
    row: &mut Row,
    worker: &tokio::sync::mpsc::UnboundedSender<Command>,
    queue_worker: &tokio::sync::mpsc::UnboundedSender<queue::Command>,
) {
    ui.horizontal(|ui| {
        ui.heading(
            row.path
                .file_name()
                .unwrap_or(row.path.as_os_str())
                .to_string_lossy(),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let available = row.finished
                || matches!(
                    row.destination,
                    Some(compressor_app::DestinationState::AlreadyCompressed)
                );
            if ui
                .add_enabled(
                    available && !row.verifying && row.output.is_some(),
                    egui::Button::new("Verify"),
                )
                .clicked()
            {
                if let Some(output) = &row.output {
                    row.verifying = true;
                    row.status = "Verifying".into();
                    let _ =
                        queue_worker.send(queue::Command::Verify(row.path.clone(), output.clone()));
                }
            }
            if ui
                .add_enabled(
                    row.output.is_some() && available,
                    egui::Button::new("Open output"),
                )
                .clicked()
            {
                if let Some(output) = &row.output {
                    let _ = worker.send(Command::OpenOutput(output.clone()));
                }
            }
        });
    });
    ui.weak(row.path.display().to_string());
}

fn inspection(ui: &mut egui::Ui, row: &Row) {
    let Some(info) = &row.inspection else {
        return;
    };
    ui.label(format!(
        "{} {}  ·  {}",
        info.result.engine.name,
        info.result
            .engine
            .version
            .as_deref()
            .unwrap_or("unknown version"),
        size(info.bytes.into())
    ));
    let counts = info
        .result
        .media
        .iter()
        .filter(|(_, value)| value.count > 0)
        .map(|(kind, count)| format!("{} {kind} ({})", count.count, size(count.bytes.into())))
        .collect::<Vec<_>>()
        .join("  ·  ");
    if !counts.is_empty() {
        ui.weak(counts);
    }
    for (kind, count) in &info.result.encrypted_media {
        ui.weak(format!(
            "{} {kind} · {}",
            count.count,
            size(count.bytes.into())
        ));
    }
    egui::CollapsingHeader::new("Engine details").show(ui, |ui| {
        for note in &info.result.engine.notes {
            ui.label(note);
        }
        for (archive, bytes) in &info.result.archives {
            ui.label(format!("{archive} · {}", size((*bytes).into())));
        }
        for pack in &info.result.packs {
            ui.label(format!(
                "{} · {} · {} · format {} · {}",
                pack.file.0.display(),
                pack.location,
                pack.version,
                pack.format,
                if pack.encrypted {
                    "encrypted"
                } else {
                    "not encrypted"
                }
            ));
            for (kind, count) in &pack.members {
                ui.label(format!(
                    "{} {kind} · {}",
                    count.count,
                    size(count.bytes.into())
                ));
            }
            if let Some(note) = &pack.note {
                ui.label(note);
            }
        }
        for unity in &info.result.unity {
            ui.label(format!(
                "{} · {} · {} · {}",
                unity.path.0.display(),
                unity.version.as_deref().unwrap_or("unknown version"),
                unity.platform,
                if unity.compressed {
                    "already archived"
                } else {
                    "unarchived"
                }
            ));
            for (name, count) in [
                ("player data", &unity.archive_files),
                ("streamed resources", &unity.streamed_resources),
                ("bundles", &unity.bundles),
            ] {
                ui.label(format!(
                    "{} {name} · {}",
                    count.count,
                    size(count.bytes.into())
                ));
            }
        }
        if let Some(rpg) = &info.result.rpgmaker {
            ui.label(format!(
                "Content: {} · encrypted images: {} · encrypted audio: {} · key: {}",
                rpg.content_root.0.display(),
                rpg.encrypted_images,
                rpg.encrypted_audio,
                rpg.key_source
            ));
            ui.label(format!(
                "{} encrypted files · {} · {} videos · {}",
                rpg.encrypted_count,
                size(rpg.encrypted_bytes.into()),
                rpg.video_count,
                size(rpg.video_bytes.into())
            ));
        }
    });
}

fn output(
    ui: &mut egui::Ui,
    row: &Row,
    others: &[PathBuf],
    worker: &tokio::sync::mpsc::UnboundedSender<Command>,
) {
    if row.status.starts_with("Skipped:") || row.status.starts_with("Failed:") {
        let palette = style::Palette::current(ui.ctx());
        let color = if row.status.starts_with("Failed:") {
            palette.error
        } else {
            palette.warning
        };
        ui.colored_label(color, &row.status);
    }
    ui.horizontal(|ui| {
        ui.label(row.output.as_ref().map_or_else(
            || "Output: checking…".into(),
            |p| format!("Output: {}", p.display()),
        ));
        if ui
            .add_enabled(!row.running, egui::Button::new("Change"))
            .clicked()
        {
            let _ = worker.send(Command::PickRowOutput(row.path.clone(), others.to_vec()));
        }
    });
    let status = match &row.destination {
        Some(compressor_app::DestinationState::New) => "New output".to_owned(),
        Some(compressor_app::DestinationState::Resumes) => "Resume available".into(),
        Some(compressor_app::DestinationState::AlreadyCompressed) => "Already compressed".into(),
        Some(compressor_app::DestinationState::Blocked(reason)) => format!("Blocked: {reason}"),
        _ => "Checking destination…".into(),
    };
    style::badge(ui, &status, style::Palette::current(ui.ctx()).muted);
}

fn result(ui: &mut egui::Ui, row: &Row) {
    if let Some(report) = row.report.as_ref().filter(|_| row.finished) {
        let fraction = if report.input_bytes == 0 {
            0.0
        } else {
            (report.output_bytes as f64 / report.input_bytes as f64).clamp(0.0, 1.0) as f32
        };
        let saved = i128::from(report.input_bytes) - i128::from(report.output_bytes);
        let percent = if report.input_bytes == 0 {
            0.0
        } else {
            saved as f64 * 100.0 / report.input_bytes as f64
        };
        ui.add_space(8.0);
        ui.strong(format!(
            "{} -> {}  ·  {} saved ({percent:.0}%)",
            size(report.input_bytes.into()),
            size(report.output_bytes.into()),
            size(saved)
        ));
        ui.add(
            egui::ProgressBar::new(fraction).text(format!("{:.0}% of original", fraction * 100.0)),
        );
    }
    let verify_errors = match &row.verification {
        Some(Ok(result)) => result.errors.len(),
        Some(Err(_)) => 1,
        None => 0,
    };
    let problems = row
        .report
        .as_ref()
        .map_or(0, |report| report.failures.len())
        + verify_errors;
    if problems > 0 {
        egui::CollapsingHeader::new(format!("Problems ({problems})")).show(ui, |ui| {
            if let Some(report) = &row.report {
                // Report paths already include the pack member.
                for item in &report.failures {
                    ui.colored_label(
                        style::Palette::current(ui.ctx()).error,
                        format!("{}: {}", item.path.0.display(), item.outcome),
                    );
                }
            }
            match &row.verification {
                Some(Ok(result)) => {
                    for error in &result.errors {
                        ui.colored_label(style::Palette::current(ui.ctx()).error, error);
                    }
                }
                Some(Err(error)) => {
                    ui.colored_label(style::Palette::current(ui.ctx()).error, error);
                }
                None => {}
            }
        });
    }
    if let Some(report) = row.report.as_ref().filter(|r| !r.skips.is_empty()) {
        egui::CollapsingHeader::new(format!("Kept unchanged ({})", report.skips.len())).show(
            ui,
            |ui| {
                for item in &report.skips {
                    ui.weak(format!("{}: {}", item.path.0.display(), item.outcome));
                }
            },
        );
    }
    if let Some(Ok(verified)) = &row.verification {
        if verified.errors.is_empty() {
            ui.label(format!("Verified {} files", verified.checked));
        }
    }
}
