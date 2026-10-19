//! A stereo meter on the fader's scale (its 0 dB lines up with a fader beside it): RMS as the
//! bar, peak as the lighter top of it, a held peak line that waits 1 s then falls 20 dB/s, and
//! a clip light for 2 s after anything reaches 0 dBFS. The engine sends raw levels ~30 Hz;
//! holding and falling are done here, per meter.

use super::{OFF, TRACK};
use crate::taper::{Taper, gain_to_db};
use egui::{Color32, Rect, Sense, pos2, vec2};
use sb_shared::events::MeterLevels;

const HOLD: f64 = 1.0;
const FALL_DB_PER_S: f32 = 20.0;
const CLIP_FOR: f64 = 2.0;

#[derive(Clone, Copy)]
struct Held {
    /// Per channel: the held peak (dB) and when it was set.
    peak: [(f32, f64); 2],
    /// Last time a channel reached 0 dBFS.
    clipped: Option<f64>,
}

impl Default for Held {
    fn default() -> Self {
        Self {
            peak: [(-120.0, 0.0); 2],
            clipped: None,
        }
    }
}

pub struct Meter {
    id: egui::Id,
    levels: Option<MeterLevels>,
    taper: Taper,
    size: egui::Vec2,
}

impl Meter {
    /// `levels`: the latest from the engine (`None`: nothing measured; it reads as silence).
    pub fn new(id: impl std::hash::Hash + std::fmt::Debug, levels: Option<MeterLevels>, taper: Taper) -> Self {
        Self {
            id: egui::Id::new(("meter", id)),
            levels,
            taper,
            size: vec2(12.0, 140.0),
        }
    }

    pub fn size(mut self, w: f32, h: f32) -> Self {
        self.size = vec2(w, h);
        self
    }

    pub fn show(self, ui: &mut egui::Ui) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(self.size, Sense::hover());
        let now = ui.input(|i| i.time);
        let mut held: Held = ui.data(|d| d.get_temp(self.id)).unwrap_or_default();
        let lv = self.levels.unwrap_or(MeterLevels {
            peak: [0.0; 2],
            rms: [0.0; 2],
        });
        let mut moving = false;
        for ch in 0..2 {
            let peak = gain_to_db(lv.peak[ch]).max(-120.0);
            let (h, at) = &mut held.peak[ch];
            let since = now - *at;
            let fallen = if since > HOLD {
                *h - FALL_DB_PER_S * (since - HOLD) as f32
            } else {
                *h
            };
            if peak >= fallen {
                (*h, *at) = (peak, now);
            }
            moving |= fallen.max(peak) > self.taper.min_db;
            if lv.peak[ch] >= 1.0 {
                held.clipped = Some(now);
            }
        }
        ui.data_mut(|d| d.insert_temp(self.id, held));

        let p = ui.painter();
        let clip_h = 4.0;
        let bars = Rect::from_min_max(rect.min + vec2(0.0, clip_h + 1.0), rect.max);
        let w = (bars.width() - 1.0) / 2.0;
        let y = |db: f32| bars.bottom() - bars.height() * self.taper.db_to_position(db);
        for ch in 0..2 {
            let x0 = bars.left() + ch as f32 * (w + 1.0);
            let lane = Rect::from_min_size(pos2(x0, bars.top()), vec2(w, bars.height()));
            p.rect_filled(lane, 1.0, TRACK);
            let color = |db: f32| {
                if db > 0.0 {
                    Color32::from_rgb(235, 90, 70)
                } else if db > -9.0 {
                    Color32::from_rgb(230, 200, 80)
                } else {
                    Color32::from_rgb(80, 200, 120)
                }
            };
            let peak = gain_to_db(lv.peak[ch]);
            let rms = gain_to_db(lv.rms[ch]);
            if peak > self.taper.min_db {
                let r = Rect::from_min_max(pos2(x0, y(peak)), pos2(x0 + w, bars.bottom()));
                p.rect_filled(r, 1.0, color(peak).gamma_multiply(0.45));
            }
            if rms > self.taper.min_db {
                let r = Rect::from_min_max(pos2(x0, y(rms)), pos2(x0 + w, bars.bottom()));
                p.rect_filled(r, 1.0, color(rms));
            }
            let (h, at) = held.peak[ch];
            let since = now - at;
            let shown = if since > HOLD {
                h - FALL_DB_PER_S * (since - HOLD) as f32
            } else {
                h
            };
            if shown > self.taper.min_db {
                let yy = y(shown);
                p.line_segment(
                    [pos2(x0, yy), pos2(x0 + w, yy)],
                    egui::Stroke::new(1.5, Color32::from_gray(230)),
                );
            }
        }
        let lit = held.clipped.is_some_and(|t| now - t < CLIP_FOR);
        let light = Rect::from_min_size(rect.min, vec2(rect.width(), clip_h));
        p.rect_filled(light, 1.0, if lit { Color32::from_rgb(235, 60, 50) } else { OFF });
        if moving || lit {
            // The hold line falls and the clip light goes out by themselves.
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
        }
        resp
    }
}
