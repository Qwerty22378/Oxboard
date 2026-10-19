//! The panel both shells draw: one fader and one meter (a made-up signal at ~30 Hz), plus
//! what the spike measured last.

use egui::{Color32, CornerRadius, Sense, Stroke, vec2};
use std::time::{Duration, Instant};

/// The panel's size in logical pixels.
pub const SIZE: (f32, f32) = (420.0, 300.0);

/// The fader taper from the UI notes: position 0..1 → −60..+6 dB, finer near the top.
fn db(position: f32) -> f32 {
    6.0 - 66.0 * (1.0 - position).powi(2)
}

fn unity() -> f32 {
    1.0 - (6.0f32 / 66.0).sqrt()
}

pub struct Panel {
    position: f32,
    start: Instant,
    level: f32,
    peak: f32,
    peak_at: Instant,
    /// Lines the shell wants shown (what it is, what it measured).
    pub status: Vec<String>,
    /// Set when the user asked to hide it (Esc).
    pub close: bool,
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            position: unity(),
            start: Instant::now(),
            level: 0.0,
            peak: 0.0,
            peak_at: Instant::now(),
            status: Vec::new(),
            close: false,
        }
    }
}

impl Panel {
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.close = true;
        }
        // A made-up signal through the fader; peak hold 1 s, then falls 20 dB/s.
        let t = self.start.elapsed().as_secs_f32();
        let raw = ((t * 1.7).sin() * 0.5 + 0.5) * (0.55 + 0.45 * (t * 13.0).sin().abs());
        let gain = if self.position <= 0.0 {
            0.0
        } else {
            10f32.powf(db(self.position) / 20.0)
        };
        self.level = (raw * gain).min(2.0);
        if self.level >= self.peak {
            (self.peak, self.peak_at) = (self.level, Instant::now());
        } else if self.peak_at.elapsed() > Duration::from_secs(1) {
            self.peak = (self.peak * 10f32.powf(-1.0 / 30.0)).max(self.level);
        }

        let frame = egui::Frame::new()
            .fill(Color32::from_rgba_unmultiplied(18, 18, 26, 235))
            .stroke(Stroke::new(1.0, Color32::from_gray(80)))
            .corner_radius(CornerRadius::same(14))
            .inner_margin(16.0);
        egui::CentralPanel::no_frame().show(ui, |ui| {
            frame.show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                ui.label(egui::RichText::new("Soundboard overlay spike").size(18.0).strong());
                for line in &self.status {
                    ui.label(egui::RichText::new(line).size(11.0).color(Color32::from_gray(170)));
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let r = ui.add(
                        egui::Slider::new(&mut self.position, 0.0..=1.0)
                            .vertical()
                            .show_value(false),
                    );
                    if r.double_clicked() {
                        self.position = unity();
                    }
                    meter(ui, self.level, self.peak);
                    ui.vertical(|ui| {
                        let d = db(self.position);
                        ui.label(if self.position <= 0.0 {
                            "−∞ dB".to_string()
                        } else {
                            format!("{d:+.1} dB")
                        });
                        ui.label(
                            egui::RichText::new("double-click: 0 dB\nEsc, the hotkey or a click\noutside hides it")
                                .size(11.0)
                                .color(Color32::from_gray(150)),
                        );
                    });
                });
            });
        });
        ui.ctx().request_repaint_after(Duration::from_millis(33));
    }
}

/// A vertical meter, −60..+6 dB, with a peak line.
fn meter(ui: &mut egui::Ui, level: f32, peak: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(16.0, 140.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 3.0, Color32::from_gray(40));
    let frac = |v: f32| {
        let d = 20.0 * v.max(1e-6).log10();
        ((d + 60.0) / 66.0).clamp(0.0, 1.0)
    };
    let top = rect.bottom() - rect.height() * frac(level);
    let fill = egui::Rect::from_min_max(egui::pos2(rect.left(), top), rect.right_bottom());
    let color = if level > 1.0 {
        Color32::from_rgb(230, 80, 60)
    } else {
        Color32::from_rgb(80, 200, 120)
    };
    p.rect_filled(fill, 3.0, color);
    let y = rect.bottom() - rect.height() * frac(peak);
    p.hline(rect.x_range(), y, Stroke::new(2.0, Color32::WHITE));
}
