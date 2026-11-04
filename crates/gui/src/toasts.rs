use super::{egui, style};
use std::time::{Duration, Instant};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Info,
    Warning,
    Error,
}
struct Toast {
    kind: Kind,
    message: String,
    created: Instant,
}
#[derive(Default)]
pub(super) struct Toasts {
    items: Vec<Toast>,
}
impl Toasts {
    pub fn push(&mut self, kind: Kind, message: String) {
        self.items.push(Toast {
            kind,
            message,
            created: Instant::now(),
        });
    }
    fn expire(&mut self, now: Instant) {
        self.items.retain(|t| {
            t.kind == Kind::Error
                || now.saturating_duration_since(t.created) < Duration::from_secs(5)
        });
    }
    fn hidden(&self) -> usize {
        self.items.len().saturating_sub(4)
    }
    pub fn show(&mut self, ctx: &egui::Context) {
        self.expire(Instant::now());
        if self.items.is_empty() {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(250));
        egui::Area::new("toast_area".into())
            .anchor(egui::Align2::RIGHT_BOTTOM, [-16.0, -16.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                let hidden = self.hidden();
                if hidden > 0 {
                    ui.weak(format!("+{hidden} more"));
                }
                let mut remove = None;
                for (i, toast) in self.items.iter().enumerate().skip(hidden) {
                    let palette = style::Palette::current(ctx);
                    let color = match toast.kind {
                        Kind::Info => palette.accent,
                        Kind::Warning => palette.warning,
                        Kind::Error => palette.error,
                    };
                    egui::Frame::new()
                        .fill(palette.card)
                        .stroke(egui::Stroke::new(1.0_f32, color))
                        .corner_radius(10)
                        .inner_margin(12)
                        .show(ui, |ui| {
                            ui.set_max_width(380.0);
                            ui.horizontal(|ui| {
                                ui.colored_label(
                                    color,
                                    match toast.kind {
                                        Kind::Info => "Info",
                                        Kind::Warning => "Notice",
                                        Kind::Error => "Error",
                                    },
                                );
                                ui.label(&toast.message);
                                if ui.small_button("×").clicked() {
                                    remove = Some(i);
                                }
                            });
                        });
                }
                if let Some(i) = remove {
                    self.items.remove(i);
                }
            });
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expires_notices_and_collapses_old_items() {
        let mut t = Toasts::default();
        for _ in 0..5 {
            t.push(Kind::Info, "message".into());
        }
        t.push(Kind::Warning, "warning".into());
        t.push(Kind::Error, "error".into());
        assert_eq!(t.hidden(), 3);
        t.expire(Instant::now() + Duration::from_secs(6));
        assert_eq!(t.items.len(), 1);
        assert_eq!(t.items[0].kind, Kind::Error);
    }
}
