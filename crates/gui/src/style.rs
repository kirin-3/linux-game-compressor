use eframe::egui::{self, Color32, FontFamily};

const REGULAR: &[u8] = include_bytes!("../assets/fonts/NotoSans-Regular.ttf");
const MEDIUM: &[u8] = include_bytes!("../assets/fonts/NotoSans-Medium.ttf");

/// The heavier font family, used for headings, section titles and strong text.
pub(crate) fn medium() -> FontFamily {
    FontFamily::Name("medium".into())
}

#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub accent: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub error: Color32,
    pub muted: Color32,
    /// Window and panel base.
    pub bg: Color32,
    /// Toolbar, cards and table.
    pub surface: Color32,
    /// Secondary buttons, combo boxes and fields.
    pub raised: Color32,
    pub hover: Color32,
    /// Pressed or open controls.
    pub pressed: Color32,
    pub border: Color32,
    /// Body text.
    pub text: Color32,
    /// Text on an accent fill.
    pub on_accent: Color32,
    /// Fill of a disabled primary button.
    pub accent_disabled: Color32,
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
                bg: Color32::from_rgb(0x0F, 0x14, 0x1C),
                surface: Color32::from_rgb(0x17, 0x1E, 0x29),
                raised: Color32::from_rgb(0x21, 0x2A, 0x38),
                hover: Color32::from_rgb(0x2A, 0x35, 0x46),
                pressed: Color32::from_rgb(0x33, 0x40, 0x55),
                border: Color32::from_rgb(0x28, 0x32, 0x41),
                text: Color32::from_rgb(0xE4, 0xE9, 0xF1),
                on_accent: Color32::from_rgb(0x0B, 0x12, 0x20),
                accent_disabled: Color32::from_rgb(0x4F, 0x6F, 0x9F),
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
                bg: Color32::from_rgb(0xF4, 0xF6, 0xFA),
                surface: Color32::WHITE,
                raised: Color32::from_rgb(0xEE, 0xF1, 0xF6),
                hover: Color32::from_rgb(0xE3, 0xE8, 0xEF),
                pressed: Color32::from_rgb(0xD7, 0xDE, 0xE8),
                border: Color32::from_rgb(0xDC, 0xE2, 0xEA),
                text: Color32::from_rgb(0x1A, 0x22, 0x30),
                on_accent: Color32::WHITE,
                accent_disabled: Color32::from_rgb(0x63, 0x8C, 0xC4),
                overlay: Color32::from_black_alpha(220),
                overlay_text: Color32::WHITE,
            }
        }
    }
    pub fn current(ctx: &egui::Context) -> Self {
        Self::for_dark(ctx.style().visuals.dark_mode)
    }
}

/// Mix two opaque colours in sRGB space; `t` = 0 gives `a`, 1 gives `b`.
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let m = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// Noto Sans first in the proportional and medium families, egui's fonts as fallbacks.
fn fonts() -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "noto-regular".into(),
        egui::FontData::from_static(REGULAR).into(),
    );
    fonts.font_data.insert(
        "noto-medium".into(),
        egui::FontData::from_static(MEDIUM).into(),
    );
    let fallbacks = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let family = |first: &str| {
        let mut list = vec![first.to_owned()];
        list.extend(fallbacks.iter().cloned());
        list
    };
    fonts
        .families
        .insert(FontFamily::Proportional, family("noto-regular"));
    fonts.families.insert(medium(), family("noto-medium"));
    fonts
}

fn visuals(v: &mut egui::Visuals, p: &Palette) {
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.window_stroke = egui::Stroke::new(1.0_f32, p.border);
    v.extreme_bg_color = p.bg;
    v.faint_bg_color = mix(p.surface, p.text, 0.04);
    v.override_text_color = None;
    v.selection.bg_fill = p.accent;
    v.selection.stroke.color = p.on_accent;
    v.window_corner_radius = egui::CornerRadius::same(10);
    v.menu_corner_radius = egui::CornerRadius::same(8);
    let shadow = egui::Shadow {
        offset: [0, 6],
        blur: 20,
        spread: 0,
        color: Color32::from_black_alpha(if v.dark_mode { 110 } else { 40 }),
    };
    v.window_shadow = shadow;
    v.popup_shadow = egui::Shadow {
        offset: [0, 4],
        blur: 12,
        ..shadow
    };
    let w = &mut v.widgets;
    let text = egui::Stroke::new(1.0_f32, p.text);
    let border = egui::Stroke::new(1.0_f32, p.border);
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = border;
    w.noninteractive.fg_stroke = text;
    w.inactive.bg_fill = p.raised;
    w.inactive.weak_bg_fill = p.raised;
    w.inactive.bg_stroke = border;
    w.inactive.fg_stroke = text;
    w.hovered.bg_fill = p.hover;
    w.hovered.weak_bg_fill = p.hover;
    w.hovered.bg_stroke = egui::Stroke::new(1.0_f32, p.accent.gamma_multiply(0.4));
    w.hovered.fg_stroke = text;
    for state in [&mut w.active, &mut w.open] {
        state.bg_fill = p.pressed;
        state.weak_bg_fill = p.pressed;
        state.bg_stroke = border;
        state.fg_stroke = text;
    }
    w.active.bg_stroke = egui::Stroke::new(1.0_f32, p.accent.gamma_multiply(0.6));
    for state in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        state.corner_radius = egui::CornerRadius::same(7);
        state.expansion = 0.0;
    }
}

pub(crate) fn apply(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    ctx.set_theme(egui::ThemePreference::System);
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        let p = Palette::for_dark(theme == egui::Theme::Dark);
        ctx.style_mut_of(theme, |style| {
            style.spacing.item_spacing = egui::vec2(8.0, 8.0);
            style.spacing.button_padding = egui::vec2(12.0, 7.0);
            visuals(&mut style.visuals, &p);
            style
                .text_styles
                .insert(egui::TextStyle::Heading, egui::FontId::new(20.0, medium()));
            style
                .text_styles
                .insert(egui::TextStyle::Body, egui::FontId::proportional(13.5));
            style
                .text_styles
                .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
            style.text_styles.insert(
                egui::TextStyle::Name("section".into()),
                egui::FontId::new(15.0, medium()),
            );
        });
    }
}

/// Text in the medium weight, for places that need weight, not only colour.
pub(crate) fn strong(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).family(medium())
}

#[derive(Clone, Copy)]
pub(crate) enum PanelKind {
    Toolbar,
    Queue,
    Detail,
}

/// Frame for a top-level panel. All panels share the 16 px page gutter.
pub(crate) fn panel_frame(ctx: &egui::Context, kind: PanelKind) -> egui::Frame {
    let p = Palette::current(ctx);
    match kind {
        PanelKind::Toolbar => egui::Frame::new()
            .fill(p.surface)
            .inner_margin(egui::Margin::symmetric(16, 12)),
        PanelKind::Queue | PanelKind::Detail => egui::Frame::new().fill(p.bg).inner_margin(16),
    }
}

/// A `surface` card with a 1 px border. Callers may change its stroke or margin.
pub(crate) fn card_frame(ctx: &egui::Context) -> egui::Frame {
    let p = Palette::current(ctx);
    egui::Frame::new()
        .fill(p.surface)
        .stroke(egui::Stroke::new(1.0_f32, p.border))
        .corner_radius(10)
        .inner_margin(16)
}

/// A full-width card around `add`.
pub(crate) fn card<R>(
    ui: &mut egui::Ui,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    card_frame(ui.ctx()).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        add(ui)
    })
}

/// The accent button for the main action. When disabled it does not use
/// egui's faded disabled state, so its label stays readable.
pub(crate) fn primary_button(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    let p = Palette::current(ui.ctx());
    let button = egui::Button::new(strong(text).color(p.on_accent)).stroke(egui::Stroke::NONE);
    if enabled {
        ui.add(button.fill(p.accent))
    } else {
        ui.add(button.fill(p.accent_disabled).sense(egui::Sense::hover()))
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
    fn contrast(a: Color32, b: Color32) -> f64 {
        let (a, b) = (lum(a), lum(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }
    #[test]
    fn status_colours_are_readable() {
        for dark in [false, true] {
            let p = Palette::for_dark(dark);
            for c in [p.accent, p.success, p.warning, p.error] {
                assert!(contrast(c, p.bg) >= 4.5);
            }
        }
    }
    #[test]
    fn text_colours_are_readable() {
        for dark in [false, true] {
            let p = Palette::for_dark(dark);
            for back in [p.bg, p.surface] {
                assert!(contrast(p.text, back) >= 4.5);
                assert!(contrast(p.muted, back) >= 4.5);
            }
            assert!(contrast(p.on_accent, p.accent) >= 4.5);
            assert!(contrast(p.on_accent, p.accent_disabled) >= 3.0);
        }
    }
    #[test]
    fn noto_comes_first_with_egui_fallbacks() {
        let fonts = fonts();
        let defaults = egui::FontDefinitions::default();
        let fallbacks = &defaults.families[&FontFamily::Proportional];
        assert!(!fallbacks.is_empty());
        for (family, first) in [
            (FontFamily::Proportional, "noto-regular"),
            (medium(), "noto-medium"),
        ] {
            let list = &fonts.families[&family];
            assert_eq!(list[0], first);
            assert_eq!(&list[1..], fallbacks.as_slice());
        }
        assert_eq!(
            fonts.families[&FontFamily::Monospace],
            defaults.families[&FontFamily::Monospace]
        );
    }
    #[test]
    fn no_widget_keeps_an_egui_default_colour() {
        for dark in [false, true] {
            let default = if dark {
                egui::Visuals::dark()
            } else {
                egui::Visuals::light()
            };
            let mut themed = default.clone();
            visuals(&mut themed, &Palette::for_dark(dark));
            let states = |v: &egui::Visuals| {
                let w = &v.widgets;
                [w.noninteractive, w.inactive, w.hovered, w.active, w.open]
            };
            for (t, d) in states(&themed).iter().zip(states(&default).iter()) {
                assert_ne!(t.bg_fill, d.bg_fill);
                assert_ne!(t.weak_bg_fill, d.weak_bg_fill);
                assert_ne!(t.bg_stroke.color, d.bg_stroke.color);
                assert_ne!(t.fg_stroke.color, d.fg_stroke.color);
            }
        }
    }
}
