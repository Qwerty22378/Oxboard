//! A routing-matrix cell: a connection's gain, or off.
//!
//! - **Click** toggles (on at 0 dB).
//! - **Drag up/down** sets the gain, 1 dB per 4 px (Shift: 1 dB per 20 px); from off it starts
//!   at the bottom of the scale. The **wheel** does ±1 dB.
//! - **Double-click** opens a box to type dB ("−6", "+3 dB", "off"); Enter sets it, Esc
//!   cancels. Its first click already toggled, so a double-click puts that back first.
//! - A cell that would close a feedback loop is striped and can only be switched off.
//!
//! Brighter means louder. Hovering is reported so the matrix can light the row and column.

use super::{ACCENT, DIM, ECHO_GRACE, OFF};
use crate::taper::{BOOST, db_to_gain, format_db, gain_to_db, parse_db};
use egui::{Align2, Color32, FontId, Rect, Sense, Stroke, StrokeKind, pos2, vec2};

#[derive(Clone, Default)]
struct State {
    /// The dB box's text, while it's open.
    editing: Option<String>,
    /// The gain before the last click, and when (to undo it on a double-click).
    before_click: Option<(Option<f32>, f64)>,
    /// Pixels dragged not yet turned into whole dB.
    drag_px: f32,
    /// The pointer's height last frame, while dragging (from where it was pressed, so egui's
    /// drag threshold loses nothing).
    last_y: Option<f32>,
    /// The cell's own gain while it's dragged (and briefly after): the controller's echo
    /// trails, and steps must add up from what was last sent, not from the stale value.
    own: Option<(Option<f32>, f64)>,
}

pub struct Cell {
    id: egui::Id,
    gain: Option<f32>,
    would_loop: bool,
    size: egui::Vec2,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct CellOut {
    /// The connection's new gain (`Some(None)`: switched off).
    pub set: Option<Option<f32>>,
    pub hovered: bool,
}

impl Cell {
    pub fn new(id: impl std::hash::Hash + std::fmt::Debug, gain: Option<f32>, would_loop: bool) -> Self {
        Self {
            id: egui::Id::new(("cell", id)),
            gain,
            would_loop,
            size: vec2(64.0, 28.0),
        }
    }

    pub fn size(mut self, w: f32, h: f32) -> Self {
        self.size = vec2(w, h);
        self
    }

    pub fn show(self, ui: &mut egui::Ui) -> CellOut {
        let (rect, resp) = ui.allocate_exact_size(self.size, Sense::click_and_drag());
        let now = ui.input(|i| i.time);
        let mut st: State = ui.data(|d| d.get_temp(self.id)).unwrap_or_default();
        let mut out = CellOut {
            hovered: resp.hovered(),
            ..Default::default()
        };
        // Off and looping: nothing to do but explain.
        let locked = self.would_loop && self.gain.is_none();
        if st.own.is_some_and(|(_, t)| !resp.dragged() && now - t > ECHO_GRACE) {
            st.own = None;
        }
        let current = st.own.map_or(self.gain, |(g, _)| g);

        if let Some(text) = st.editing.as_mut() {
            let edit = ui.put(
                rect.shrink(2.0),
                egui::TextEdit::singleline(text)
                    .horizontal_align(egui::Align::Center)
                    .font(FontId::proportional(12.0)),
            );
            if !edit.has_focus() && !edit.lost_focus() {
                edit.request_focus();
            }
            let (enter, esc) = ui.input(|i| (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape)));
            if enter || esc || edit.lost_focus() {
                if enter && let Some(db) = parse_db(text, BOOST.max_db) {
                    out.set = Some((db > f32::NEG_INFINITY).then(|| db_to_gain(db)));
                }
                st.editing = None;
            }
            ui.data_mut(|d| d.insert_temp(self.id, st));
            return out;
        }

        if resp.double_clicked() && !locked {
            // Undo the toggle the first click made, then type.
            let before = match st.before_click.take() {
                Some((g, at)) if now - at < 0.6 => {
                    out.set = Some(g);
                    g
                }
                _ => self.gain,
            };
            st.editing = Some(before.map_or("off".into(), |g| format!("{:.1}", gain_to_db(g))));
        } else if resp.clicked() && !locked {
            st.before_click = Some((self.gain, now));
            out.set = Some(match self.gain {
                Some(_) => None,
                None => Some(1.0),
            });
        }
        if resp.dragged()
            && !locked
            && let Some(p) = ui.input(|i| i.pointer.interact_pos())
        {
            let fine = ui.input(|i| i.modifiers.shift);
            let from_y = st
                .last_y
                .or_else(|| ui.input(|i| i.pointer.press_origin()).map(|o| o.y))
                .unwrap_or(p.y);
            st.drag_px += from_y - p.y;
            st.last_y = Some(p.y);
            let per_db = if fine { 20.0 } else { 4.0 };
            let steps = (st.drag_px / per_db).trunc();
            if steps != 0.0 {
                st.drag_px -= steps * per_db;
                let g = BOOST.nudge(current.unwrap_or(0.0), steps);
                let g = (g > 0.0).then_some(g);
                out.set = Some(g);
                st.own = Some((g, now));
            }
        }
        if resp.drag_stopped() {
            (st.drag_px, st.last_y) = (0.0, None);
            if let Some((g, _)) = st.own {
                st.own = Some((g, now));
            }
        }
        if resp.hovered() && !locked {
            let wheel: f32 = ui.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        egui::Event::MouseWheel { delta, .. } => Some(delta.y),
                        _ => None,
                    })
                    .sum()
            });
            if wheel != 0.0 {
                let g = BOOST.nudge(current.unwrap_or(0.0), wheel.signum());
                let g = (g > 0.0).then_some(g);
                out.set = Some(g);
                st.own = Some((g, now));
            }
        }
        ui.data_mut(|d| d.insert_temp(self.id, st));

        // What it will be, so the cell answers the mouse before the controller does.
        let shown = out.set.unwrap_or(current);
        paint(ui, rect, shown, self.would_loop, resp.hovered());
        let tip = if self.would_loop {
            "This would feed the sound back into itself".to_string()
        } else {
            match shown {
                Some(g) => format!(
                    "{} (click: off, drag: level, double-click: type)",
                    format_db(gain_to_db(g))
                ),
                None => "Off (click: on at 0 dB, double-click: type a level)".into(),
            }
        };
        resp.on_hover_text(tip);
        out
    }
}

fn paint(ui: &egui::Ui, rect: Rect, gain: Option<f32>, would_loop: bool, hot: bool) {
    let p = ui.painter();
    let r = rect.shrink(1.0);
    match gain {
        Some(g) => {
            let strength = 0.25 + 0.75 * BOOST.gain_to_position(g);
            p.rect_filled(r, 4.0, ACCENT.gamma_multiply(strength));
            let db = gain_to_db(g);
            let text = if db.abs() < 0.05 {
                "0 dB".to_string()
            } else {
                format_db(db).trim_end_matches(" dB").to_string()
            };
            p.text(
                r.center(),
                Align2::CENTER_CENTER,
                text,
                FontId::proportional(12.0),
                Color32::WHITE,
            );
        }
        None => {
            p.rect_filled(r, 4.0, OFF);
            p.text(r.center(), Align2::CENTER_CENTER, "−∞", FontId::proportional(12.0), DIM);
        }
    }
    if would_loop {
        // Diagonal stripes.
        let clip = p.with_clip_rect(r);
        let mut x = r.left() - r.height();
        while x < r.right() {
            clip.line_segment(
                [pos2(x, r.bottom()), pos2(x + r.height(), r.top())],
                Stroke::new(2.0, Color32::from_rgba_unmultiplied(200, 70, 60, 120)),
            );
            x += 7.0;
        }
    }
    if hot {
        p.rect_stroke(r, 4.0, Stroke::new(1.0, Color32::from_gray(200)), StrokeKind::Inside);
    }
}
