use eframe::egui::{self, Color32};

#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub accent: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub error: Color32,
    pub muted: Color32,
    pub panel: Color32,
    pub card: Color32,
    pub overlay: Color32,
    pub overlay_text: Color32,
}
impl Palette {
    pub fn for_dark(dark: bool) -> Self {
        if dark {
            Self {
                accent: Color32::from_rgb(125, 177, 255),
                success: Color32::from_rgb(101, 214, 153),
                warning: Color32::from_rgb(255, 196, 93),
                error: Color32::from_rgb(255, 144, 144),
                muted: Color32::from_rgb(165, 178, 196),
                panel: Color32::from_rgb(21, 27, 37),
                card: Color32::from_rgb(32, 40, 53),
                overlay: Color32::from_black_alpha(220),
                overlay_text: Color32::WHITE,
            }
        } else {
            Self {
                accent: Color32::from_rgb(32, 91, 170),
                success: Color32::from_rgb(20, 111, 65),
                warning: Color32::from_rgb(143, 76, 0),
                error: Color32::from_rgb(170, 43, 48),
                muted: Color32::from_rgb(88, 100, 117),
                panel: Color32::from_rgb(246, 248, 252),
                card: Color32::WHITE,
                overlay: Color32::from_black_alpha(220),
                overlay_text: Color32::WHITE,
            }
        }
    }
    pub fn current(ctx: &egui::Context) -> Self {
        Self::for_dark(ctx.style().visuals.dark_mode)
    }
}
pub(crate) fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::System);
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        let p = Palette::for_dark(theme == egui::Theme::Dark);
        ctx.style_mut_of(theme, |style| {
            style.spacing.item_spacing = egui::vec2(8.0, 8.0);
            style.spacing.button_padding = egui::vec2(12.0, 7.0);
            style.visuals.panel_fill = p.panel;
            style.visuals.window_fill = p.card;
            style.visuals.extreme_bg_color = p.card;
            style.visuals.selection.bg_fill = p.accent;
            style.visuals.selection.stroke.color = if theme == egui::Theme::Dark {
                p.panel
            } else {
                p.overlay_text
            };
            style.visuals.window_corner_radius = egui::CornerRadius::same(10);
            for widget in [
                &mut style.visuals.widgets.inactive,
                &mut style.visuals.widgets.hovered,
                &mut style.visuals.widgets.active,
            ] {
                widget.corner_radius = egui::CornerRadius::same(7);
            }
            style
                .text_styles
                .insert(egui::TextStyle::Heading, egui::FontId::proportional(20.0));
            style
                .text_styles
                .insert(egui::TextStyle::Body, egui::FontId::proportional(13.5));
            style
                .text_styles
                .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
            style.text_styles.insert(
                egui::TextStyle::Name("section".into()),
                egui::FontId::proportional(15.0),
            );
        });
    }
}
pub(crate) fn badge(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.15))
        .corner_radius(7)
        .inner_margin(egui::Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.colored_label(color, text);
        });
}
#[cfg(test)]
mod tests {
    use super::*;
    fn lum(c: Color32) -> f64 {
        let linear = |v: u8| {
            let x = f64::from(v) / 255.0;
            if x <= 0.04045 {
                x / 12.92
            } else {
                ((x + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(c.r()) + 0.7152 * linear(c.g()) + 0.0722 * linear(c.b())
    }
    #[test]
    fn status_colours_are_readable() {
        for dark in [false, true] {
            let p = Palette::for_dark(dark);
            for c in [p.accent, p.success, p.warning, p.error] {
                let a = lum(c);
                let b = lum(p.panel);
                assert!((a.max(b) + 0.05) / (a.min(b) + 0.05) >= 4.5);
            }
        }
    }
}
