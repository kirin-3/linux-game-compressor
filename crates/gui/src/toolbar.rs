use super::{egui, queue, style, App, Command};

pub(super) fn show(app: &mut App, ctx: &egui::Context) {
    egui::TopBottomPanel::top("toolbar")
        .frame(style::panel_frame(ctx, style::PanelKind::Toolbar))
        .show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::Image::new((app.logo.id(), egui::vec2(28.0, 28.0))));
                ui.heading(super::APP_NAME);
                ui.add_space(12.0);
                if ui.button("+  Add games").clicked() {
                    let _ = app.worker.commands.send(Command::Pick);
                }
                ui.separator();
                let output = app.prefs.output_dir.as_ref().map_or_else(
                    || "Beside each game".to_owned(),
                    |p| p.display().to_string(),
                );
                ui.label("Output");
                if ui
                    .add_enabled(!app.queue_running, egui::Button::new(output))
                    .clicked()
                {
                    let _ = app.worker.commands.send(Command::PickOutputBase);
                }
                if app.prefs.output_dir.is_some()
                    && ui
                        .add_enabled(!app.queue_running, egui::Button::new("Reset"))
                        .clicked()
                {
                    app.prefs.output_dir = None;
                    app.assign_outputs(true);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if app.starting {
                        ui.add_enabled(false, egui::Button::new("Starting…"));
                    } else if app.queue_running {
                        ui.label(format!(
                            "{} / {} games",
                            app.queue_completed,
                            app.queue.len().max(app.queue_completed)
                        ));
                        if ui.button("Stop queue").clicked() {
                            app.estimator.restore();
                            let _ = app.queue_worker.commands.send(queue::Command::Stop);
                        }
                    } else {
                        if style::primary_button(ui, "▶  Start compression", !app.queue.is_empty())
                            .clicked()
                        {
                            app.starting = true;
                            app.estimator.prepare_start();
                        }
                    }
                });
            });
            ui.horizontal(|ui| {
                // The row starts at the label's height, so without a minimum the
                // label is centred higher than the taller combo box next to it.
                ui.set_min_height(
                    ui.spacing().interact_size.y + 2.0 * ui.spacing().button_padding.y,
                );
                ui.weak("Default profile");
                egui::ComboBox::from_id_salt("default_profile")
                    .selected_text(match app.prefs.default_profile.as_str() {
                        "quality" => "Best looking",
                        "small" => "Smallest",
                        _ => "Recommended",
                    })
                    .show_ui(ui, |ui| {
                        for (value, label) in [
                            ("quality", "Best looking"),
                            ("balanced", "Recommended"),
                            ("small", "Smallest"),
                        ] {
                            ui.selectable_value(
                                &mut app.prefs.default_profile,
                                value.into(),
                                label,
                            );
                        }
                    });
                ui.checkbox(&mut app.prefs.verify, "Verify completed games");
            });
            if !app.tool_warnings.is_empty() {
                ui.colored_label(
                    style::Palette::current(ctx).warning,
                    format!("Missing tools: {}", app.tool_warnings.join("; ")),
                );
            }
            if app.wayland {
                ui.weak("Wayland: use Add games to choose folders.");
            }
        });
}
