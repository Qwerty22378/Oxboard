//! A soundboard pad: the clip's name and color, its hotkey, and a ring that spins while it
//! plays (the engine reports playing, not progress, so the ring has no length to fill). Click
//! plays (what a second trigger does is the clip's retrigger mode); right-click opens its
//! settings.

use super::fader::Fader;
use crate::taper::BOOST;
use egui::epaint::PathShape;
use egui::{Align2, Color32, FontId, Sense, Stroke, StrokeKind, pos2, vec2};
use sb_shared::graph::Retrigger;
use sb_shared::ui::{ClipId, ClipSettings};
use std::f32::consts::TAU;

const PALETTE: [Color32; 6] = [
    Color32::from_rgb(214, 96, 77),
    Color32::from_rgb(222, 164, 64),
    Color32::from_rgb(110, 178, 92),
    Color32::from_rgb(70, 160, 190),
    Color32::from_rgb(120, 110, 210),
    Color32::from_rgb(196, 92, 160),
];

pub fn color(id: ClipId) -> Color32 {
    PALETTE[id.0 as usize % PALETTE.len()]
}

pub struct Pad<'a> {
    id: ClipId,
    settings: &'a ClipSettings,
    playing: bool,
    hotkey: Option<&'a str>,
    size: f32,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct PadOut {
    pub play: bool,
    pub stop: bool,
    /// The settings changed in its menu.
    pub settings: Option<ClipSettings>,
    pub remove: bool,
}

impl<'a> Pad<'a> {
    pub fn new(id: ClipId, settings: &'a ClipSettings, playing: bool) -> Self {
        Self {
            id,
            settings,
            playing,
            hotkey: None,
            size: 84.0,
        }
    }

    pub fn hotkey(mut self, label: Option<&'a str>) -> Self {
        self.hotkey = label;
        self
    }

    pub fn size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    pub fn show(self, ui: &mut egui::Ui) -> PadOut {
        let (rect, resp) = ui.allocate_exact_size(vec2(self.size, self.size), Sense::click());
        let mut out = PadOut {
            play: resp.clicked(),
            ..Default::default()
        };
        let hot = resp.hovered();
        let base = color(self.id);
        let p = ui.painter();
        let fill = if hot { base } else { base.gamma_multiply(0.8) };
        p.rect_filled(rect, 10.0, fill);
        if resp.is_pointer_button_down_on() {
            p.rect_filled(rect, 10.0, Color32::from_black_alpha(50));
        }
        p.rect_stroke(
            rect,
            10.0,
            Stroke::new(1.0, Color32::from_black_alpha(90)),
            StrokeKind::Inside,
        );

        // The play mark, or the spinning ring while it plays.
        let c = pos2(rect.center().x, rect.top() + rect.height() * 0.38);
        let r = self.size * 0.17;
        p.circle_filled(c, r + 3.0, Color32::from_black_alpha(60));
        if self.playing {
            let t = ui.input(|i| i.time) as f32;
            let start = t * TAU * 0.8;
            let points: Vec<_> = (0..=24)
                .map(|i| {
                    let a = start + i as f32 / 24.0 * TAU * 0.75;
                    c + vec2(a.cos(), a.sin()) * r
                })
                .collect();
            p.add(PathShape::line(points, Stroke::new(3.0, Color32::WHITE)));
            let s = r * 0.5;
            p.rect_filled(egui::Rect::from_center_size(c, vec2(s, s)), 1.0, Color32::WHITE);
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
        } else {
            let s = r * 0.6;
            let tri = vec![c + vec2(-s * 0.6, -s), c + vec2(s, 0.0), c + vec2(-s * 0.6, s)];
            p.add(egui::Shape::convex_polygon(tri, Color32::WHITE, Stroke::NONE));
        }
        let name = elide(&self.settings.name, 12);
        p.text(
            pos2(rect.center().x, rect.bottom() - self.size * 0.24),
            Align2::CENTER_CENTER,
            name,
            FontId::proportional(13.0),
            Color32::WHITE,
        );
        let mut corner = Vec::new();
        if let Some(k) = self.hotkey {
            corner.push(k.to_string());
        }
        if self.settings.looped {
            corner.push("loop".into());
        }
        if !corner.is_empty() {
            p.text(
                rect.left_bottom() + vec2(6.0, -5.0),
                Align2::LEFT_BOTTOM,
                corner.join(" · "),
                FontId::proportional(10.0),
                Color32::from_white_alpha(200),
            );
        }

        let resp = resp.on_hover_text(format!("{} (right-click for settings)", self.settings.name));
        resp.context_menu(|ui| menu(ui, self.id, self.settings, self.playing, &mut out));
        out
    }
}

fn menu(ui: &mut egui::Ui, id: ClipId, settings: &ClipSettings, playing: bool, out: &mut PadOut) {
    ui.set_min_width(190.0);
    ui.label(egui::RichText::new(&settings.name).strong());
    if ui.add_enabled(playing, egui::Button::new("Stop")).clicked() {
        out.stop = true;
        ui.close();
    }
    ui.separator();
    let mut s = settings.clone();
    ui.label("Played again while playing:");
    for (mode, label) in [
        (Retrigger::Restart, "Start over"),
        (Retrigger::Overlap, "Play on top"),
        (Retrigger::Ignore, "Ignore"),
    ] {
        ui.radio_value(&mut s.retrigger, mode, label);
    }
    ui.checkbox(&mut s.looped, "Loop");
    ui.horizontal(|ui| {
        ui.label("Volume");
        if let Some(g) = Fader::new(("pad", id.0), s.gain, BOOST).horizontal(120.0).show(ui).gain {
            s.gain = g;
        }
    });
    if s != *settings {
        out.settings = Some(s);
    }
    ui.separator();
    if ui
        .button(egui::RichText::new("Remove").color(Color32::from_rgb(235, 110, 95)))
        .clicked()
    {
        out.remove = true;
        ui.close();
    }
}

fn elide(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut s: String = text.chars().take(max - 1).collect();
        s.push('…');
        s
    }
}
