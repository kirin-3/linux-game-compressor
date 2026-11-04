use super::{egui, labels, size, style, Row};
use compressor_core::{OptionKind, Resolved};
use std::collections::BTreeMap;

/// One settings grid row: label, control, and a reset for changed values.
fn widget(
    ui: &mut egui::Ui,
    key: &str,
    kind: &OptionKind,
    value: &Resolved,
    overrides: &mut BTreeMap<String, serde_json::Value>,
) -> bool {
    let label = labels::setting(key);
    let overridden = overrides.contains_key(key);
    let mut edited = false;
    let mut result = value.value.clone();
    let palette = style::Palette::current(ui.ctx());
    let name = if overridden {
        egui::RichText::new(format!("{} \u{2022}", label.name)).color(palette.accent)
    } else {
        egui::RichText::new(&label.name)
    };
    ui.label(name).on_hover_text(label.help);
    ui.horizontal(|ui| {
        ui.spacing_mut().slider_width = 220.0;
        match kind {
            OptionKind::Bool => {
                let mut v = result.as_bool().unwrap_or(false);
                if ui.checkbox(&mut v, "").on_hover_text(label.help).changed() {
                    result = v.into();
                    edited = true;
                }
            }
            OptionKind::Int { min, max } => {
                let mut v = result.as_i64().unwrap_or(*min);
                if key == "video.crf" {
                    // Right means better quality, so the slider runs on inverted CRF.
                    let mut quality = 63 - v;
                    let response = ui
                        .add(
                            egui::Slider::new(&mut quality, (63 - max)..=(63 - min))
                                .show_value(false),
                        )
                        .on_hover_text(label.help);
                    ui.weak(format!("CRF {}", 63 - quality));
                    if response.changed() {
                        result = (63 - quality).into();
                        edited = true;
                    }
                } else {
                    let response = ui
                        .add(egui::Slider::new(&mut v, *min..=*max).suffix(
                            if label.unit.is_empty() {
                                String::new()
                            } else {
                                format!(" {}", label.unit)
                            },
                        ))
                        .on_hover_text(label.help);
                    if response.changed() {
                        result = v.into();
                        edited = true;
                    }
                }
            }
            OptionKind::Enum(choices) => {
                let mut v = result.as_str().unwrap_or("").to_owned();
                egui::ComboBox::from_id_salt(key)
                    .selected_text(&v)
                    .show_ui(ui, |ui| {
                        for choice in choices {
                            edited |= ui
                                .selectable_value(&mut v, choice.clone(), choice)
                                .changed();
                        }
                    });
                if edited {
                    result = v.into();
                }
            }
        }
    });
    let reset = overridden
        && ui
            .small_button("Reset")
            .on_hover_text("Restore this setting")
            .clicked();
    if !overridden {
        ui.label("");
    }
    ui.end_row();
    if reset {
        overrides.remove(key);
        return true;
    }
    if edited {
        overrides.insert(key.into(), result);
    }
    edited
}
fn media_present(row: &Row, kind: &str) -> bool {
    let Some(info) = &row.inspection else {
        return false;
    };
    let matches = |name: &str| match kind {
        "image" => ["png", "jpeg", "webp", "ctex", "texture", "image"]
            .iter()
            .any(|x| name.contains(x)),
        "audio" => ["ogg", "mp3", "opus", "audio", "vorbis", "flac"]
            .iter()
            .any(|x| name.contains(x)),
        "video" => name.contains("video"),
        _ => false,
    };
    info.result
        .media
        .iter()
        .any(|(k, v)| v.count > 0 && matches(k))
        || info
            .result
            .encrypted_media
            .iter()
            .any(|(k, v)| v.count > 0 && matches(k))
        || info
            .result
            .packs
            .iter()
            .any(|p| p.members.iter().any(|(k, v)| v.count > 0 && matches(k)))
}
fn estimate_text(row: &Row, key: &str) -> String {
    if let Some(e) = row.estimates.get(key) {
        let input = row.inspection.as_ref().map_or(0, |i| i.bytes);
        let saved = if input == 0 {
            0.0
        } else {
            (1.0 - e.likely as f64 / input as f64) * 100.0
        };
        format!(
            "About {} · {:.0}% smaller\n{}–{}",
            size(e.likely.into()),
            saved,
            size(e.low.into()),
            size(e.high.into())
        )
    } else if row.estimating {
        "Estimating…".into()
    } else {
        "Estimate unavailable".into()
    }
}
pub(super) fn show(ui: &mut egui::Ui, row: &mut Row) -> bool {
    let mut changed = false;
    ui.add_space(8.0);
    ui.heading("How much to compress?");
    if !row.overrides.is_empty() {
        ui.horizontal(|ui| {
            ui.strong(format!(
                "Custom (based on {})",
                labels::profile(&row.profile).0
            ));
            if ui
                .button(format!("Back to {}", labels::profile(&row.profile).0))
                .clicked()
            {
                row.overrides.clear();
                changed = true;
            }
        });
    }
    ui.columns(3, |cols| {
        for (index, profile) in ["quality", "balanced", "small"].iter().enumerate() {
            let (name, description) = labels::profile(profile);
            let palette = style::Palette::current(cols[index].ctx());
            egui::Frame::new()
                .fill(palette.card)
                .stroke(egui::Stroke::new(
                    if row.profile == *profile && row.overrides.is_empty() {
                        2.0_f32
                    } else {
                        0.0_f32
                    },
                    palette.accent,
                ))
                .corner_radius(10)
                .inner_margin(12)
                .show(&mut cols[index], |ui| {
                    // Columns are justified; left-align so wrapped text is not stretched.
                    ui.set_width(ui.available_width());
                    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.set_min_height(112.0);
                        let selected = row.profile == *profile && row.overrides.is_empty();
                        let name = if selected {
                            egui::RichText::new(name)
                                .strong()
                                .color(if ui.visuals().dark_mode {
                                    palette.panel
                                } else {
                                    palette.overlay_text
                                })
                        } else {
                            egui::RichText::new(name).strong()
                        };
                        if ui.selectable_label(selected, name).clicked()
                            && (row.profile != *profile || !row.overrides.is_empty())
                        {
                            row.profile = (*profile).into();
                            row.overrides.clear();
                            changed = true;
                        }
                        ui.label(description);
                        ui.add_space(6.0);
                        ui.colored_label(palette.accent, estimate_text(row, profile));
                    });
                });
        }
    });
    if !row.overrides.is_empty() {
        ui.weak(format!("Custom estimate: {}", estimate_text(row, "custom")));
    }
    ui.add_space(12.0);
    let Some(info) = &row.inspection else {
        return changed;
    };
    let engine = info.engine_id.clone();
    let engine_name = info.result.engine.name.clone();
    let specs = compressor_app::engine_specs(&compressor_app::engine_registry(false));
    let Ok((_, settings)) =
        compressor_app::resolve_settings(&row.profile, &row.overrides, &engine, &specs)
    else {
        ui.label("Unable to resolve settings");
        return changed;
    };
    let mut advanced = Vec::new();
    for (group, heading) in [("image", "Images"), ("audio", "Audio"), ("video", "Video")] {
        if !media_present(row, group) {
            continue;
        }
        ui.strong(heading);
        settings_grid(ui, group, |ui| {
            for (key, value) in settings
                .values
                .iter()
                .filter(|(key, _)| key.starts_with(&format!("{group}.")))
            {
                if matches!(
                    key.as_str(),
                    "image.effort" | "image.sharp_yuv" | "image.reencode_webp" | "video.effort"
                ) {
                    advanced.push((key.clone(), value.clone()));
                    continue;
                }
                if let Some(kind) = setting_kind(key) {
                    changed |= widget(ui, key, &kind, value, &mut row.overrides);
                }
            }
        });
        ui.add_space(6.0);
    }
    if let Some(options) = specs.get(&engine) {
        if !options.is_empty() {
            ui.strong(format!("{engine_name} options"));
            settings_grid(ui, "engine", |ui| {
                for option in options {
                    if let Some(value) = settings.engine_options.get(&option.name) {
                        changed |= widget(
                            ui,
                            &format!("engines.{engine}.{}", option.name),
                            &option.kind,
                            value,
                            &mut row.overrides,
                        );
                    }
                }
            });
        }
    }
    if !advanced.is_empty() {
        egui::CollapsingHeader::new("More options").show(ui, |ui| {
            settings_grid(ui, "advanced", |ui| {
                for (key, value) in &advanced {
                    if let Some(kind) = setting_kind(key) {
                        changed |= widget(ui, key, &kind, value, &mut row.overrides);
                    }
                }
            });
        });
    }
    changed
}
fn settings_grid(ui: &mut egui::Ui, id: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Grid::new(("settings", id))
        .num_columns(3)
        .min_col_width(180.0)
        .spacing([16.0, 8.0])
        .show(ui, add);
}
fn setting_kind(key: &str) -> Option<OptionKind> {
    Some(match key {
        "image.quality" => OptionKind::Int { min: 0, max: 100 },
        "image.effort" => OptionKind::Int { min: 0, max: 6 },
        "image.sharp_yuv" | "image.reencode_webp" => OptionKind::Bool,
        "audio.vorbis_quality" => OptionKind::Int { min: 0, max: 10 },
        "audio.opus_kbps_per_channel" | "video.opus_kbps_per_channel" => {
            OptionKind::Int { min: 6, max: 256 }
        }
        "audio.mp3_kbps_per_channel" => OptionKind::Int { min: 8, max: 160 },
        "video.crf" => OptionKind::Int { min: 0, max: 63 },
        "video.effort" => OptionKind::Int { min: 0, max: 5 },
        _ => return None,
    })
}
#[cfg(test)]
mod tests {
    #[test]
    fn inverted_video_quality_resolves_like_cli() {
        let specs = compressor_app::engine_specs(&compressor_app::engine_registry(false));
        let crf = 63 - 29;
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert("video.crf".into(), serde_json::json!(crf));
        let (_, gui) =
            compressor_app::resolve_settings("balanced", &overrides, "rpgmaker", &specs).unwrap();
        let cli = compressor_core::settings::resolve(
            Some("balanced"),
            &[format!("video.crf={crf}")],
            "rpgmaker",
            &specs,
        )
        .unwrap();
        assert_eq!(gui, cli);
    }
}
