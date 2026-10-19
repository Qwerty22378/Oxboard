//! A fader: position on the [`Taper`], dragged relatively (a click never jumps it), Shift for
//! fine moves, the wheel for ±1 dB, double-click for 0 dB.
//!
//! **Optimistic:** while it's dragged (and for [`ECHO_GRACE`] after) it shows its own value,
//! not the one it was given: the controller's diffs trail the mouse, and showing them would
//! make the knob stutter backwards. After that the controller's value is the final word.

use super::{ACCENT, ECHO_GRACE, TRACK};
use crate::taper::{self, Taper};
use egui::{Color32, Rect, Sense, Stroke, StrokeKind, Vec2, pos2, vec2};

#[derive(Clone, Copy, Default)]
struct Drag {
    /// The fader's own position, while it's in charge.
    position: Option<f32>,
    /// When the user let go (egui time).
    released: f64,
    /// While dragging: the pointer (along the travel) and the position it started from, and
    /// whether that was a fine (Shift) drag. Moves are measured from here, so egui's drag
    /// threshold loses nothing and there's no drift.
    anchor: Option<(f32, f32, bool)>,
}

pub struct Fader<'a> {
    id: egui::Id,
    gain: f32,
    taper: Taper,
    vertical: bool,
    length: f32,
    muted: bool,
    label: Option<&'a str>,
}

/// What the user did.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct FaderOut {
    /// A new gain to send (each drag step; the coalescer thins them).
    pub gain: Option<f32>,
}

impl<'a> Fader<'a> {
    pub fn new(id: impl std::hash::Hash + std::fmt::Debug, gain: f32, taper: Taper) -> Self {
        Self {
            id: egui::Id::new(("fader", id)),
            gain,
            taper,
            vertical: true,
            length: 140.0,
            muted: false,
            label: None,
        }
    }

    pub fn horizontal(mut self, length: f32) -> Self {
        (self.vertical, self.length) = (false, length);
        self
    }

    pub fn length(mut self, length: f32) -> Self {
        self.length = length;
        self
    }

    /// Drawn greyed (the level is kept; the mute is separate).
    pub fn muted(mut self, muted: bool) -> Self {
        self.muted = muted;
        self
    }

    /// For screen readers and the hover text.
    pub fn label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    pub fn show(self, ui: &mut egui::Ui) -> FaderOut {
        let thick = 26.0;
        let size = if self.vertical {
            vec2(thick, self.length)
        } else {
            vec2(self.length, thick)
        };
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
        let now = ui.input(|i| i.time);
        let mut drag: Drag = ui.data(|d| d.get_temp(self.id)).unwrap_or_default();
        if drag.position.is_some() && !resp.dragged() && now - drag.released > ECHO_GRACE {
            drag.position = None;
        }
        let given = self.taper.gain_to_position(self.gain);
        let mut position = drag.position.unwrap_or(given);
        let mut out = FaderOut::default();
        let travel = self.length - 12.0;

        if resp.dragged()
            && let Some(p) = ui.input(|i| i.pointer.interact_pos())
        {
            let along = |p: egui::Pos2| if self.vertical { -p.y } else { p.x };
            let fine = ui.input(|i| i.modifiers.shift);
            let anchor = match drag.anchor {
                // Shift pressed or let go mid-drag: carry on from here at the new speed.
                Some((_, _, f)) if f != fine => (along(p), position, fine),
                Some(a) => a,
                None => {
                    let origin = ui.input(|i| i.pointer.press_origin()).unwrap_or(p);
                    (along(origin), position, fine)
                }
            };
            drag.anchor = Some(anchor);
            let speed = if fine { 0.2 } else { 1.0 };
            let next = (anchor.1 + (along(p) - anchor.0) / travel * speed).clamp(0.0, 1.0);
            if next != position {
                position = next;
                out.gain = Some(self.taper.position_to_gain(position));
            }
            drag.position = Some(position);
        }
        if resp.drag_stopped() {
            drag.released = now;
            drag.anchor = None;
        }
        if resp.double_clicked() {
            position = self.taper.unity();
            out.gain = Some(1.0);
            (drag.position, drag.released) = (Some(position), now);
        }
        if resp.hovered() && !resp.dragged() {
            let notches: f32 = ui.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        egui::Event::MouseWheel { delta, .. } => Some(delta.y + delta.x),
                        _ => None,
                    })
                    .sum()
            });
            if notches != 0.0 {
                let from = self.taper.position_to_gain(position);
                let g = self.taper.nudge(from, notches.signum() * notches.abs().ceil());
                position = self.taper.gain_to_position(g);
                out.gain = Some(g);
                (drag.position, drag.released) = (Some(position), now);
            }
        }
        if drag.position.is_some() {
            // Keep drawing until the echo grace runs out, so the hand-over is seen.
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        ui.data_mut(|d| d.insert_temp(self.id, drag));

        let db = self.taper.position_to_db(position);
        let tip = format!(
            "{}{}",
            self.label.map_or(String::new(), |l| format!("{l}: ")),
            taper::format_db(db)
        );
        let resp = resp.on_hover_text(tip);
        paint(
            ui,
            rect,
            position,
            self.taper,
            self.vertical,
            self.muted,
            resp.hovered() || resp.dragged(),
        );
        out
    }
}

fn paint(ui: &egui::Ui, rect: Rect, position: f32, taper: Taper, vertical: bool, muted: bool, hot: bool) {
    let p = ui.painter();
    let c = rect.center();
    let travel = if vertical { rect.height() } else { rect.width() } - 12.0;
    // Along the travel: 0 at the bottom (or left), 1 at the top (or right).
    let at = |f: f32| {
        if vertical {
            pos2(c.x, rect.bottom() - 6.0 - f * travel)
        } else {
            pos2(rect.left() + 6.0 + f * travel, c.y)
        }
    };
    let (a, b) = (at(0.0), at(1.0));
    let track = if vertical {
        Rect::from_two_pos(a - vec2(2.0, 0.0), b + vec2(2.0, 0.0))
    } else {
        Rect::from_two_pos(a - vec2(0.0, 2.0), b + vec2(0.0, 2.0))
    };
    p.rect_filled(track, 2.0, TRACK);
    let fill = if muted { Color32::from_gray(90) } else { ACCENT };
    let k = at(position);
    let filled = if vertical {
        Rect::from_two_pos(a - vec2(2.0, 0.0), k + vec2(2.0, 0.0))
    } else {
        Rect::from_two_pos(a - vec2(0.0, 2.0), k + vec2(0.0, 2.0))
    };
    p.rect_filled(filled, 2.0, fill);
    // 0 dB tick.
    let u = at(taper.unity());
    let tick = if vertical {
        [u - vec2(9.0, 0.0), u + vec2(9.0, 0.0)]
    } else {
        [u - vec2(0.0, 9.0), u + vec2(0.0, 9.0)]
    };
    p.line_segment(tick, Stroke::new(1.0, Color32::from_gray(110)));
    // The knob.
    let knob_size = if vertical { vec2(22.0, 10.0) } else { vec2(10.0, 22.0) };
    let knob = Rect::from_center_size(k, knob_size);
    let face = if hot {
        Color32::from_gray(235)
    } else {
        Color32::from_gray(200)
    };
    p.rect_filled(knob, 3.0, face);
    p.rect_stroke(knob, 3.0, Stroke::new(1.0, Color32::from_gray(60)), StrokeKind::Inside);
    let grip = if vertical { Vec2::X * 6.0 } else { Vec2::Y * 6.0 };
    p.line_segment([k - grip, k + grip], Stroke::new(1.0, Color32::from_gray(90)));
}
