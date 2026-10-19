//! The soundboard column, in every mode: the pads, an "add" tile (a file dialog; audio files
//! can also be dropped anywhere on the overlay), stop-all, and the clips' meter.

use super::Output;
use crate::store::Store;
use crate::taper::BOOST;
use crate::widgets::{DIM, meter::Meter, pad::Pad};
use egui::{Color32, RichText, Sense, Stroke, vec2};
use sb_shared::ui::{Command, MeterTarget, StateView};

/// The column's width: three pads.
pub const WIDTH: f32 = 3.0 * PAD + 2.0 * GAP + 4.0;
const PAD: f32 = 76.0;
const GAP: f32 = 6.0;

pub fn show(ui: &mut egui::Ui, store: &Store, v: &StateView, out: &mut Output) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("SOUNDBOARD").size(12.0).color(DIM).strong());
        Meter::new("clips", store.meters.get(&MeterTarget::Clips).copied(), BOOST)
            .size(8.0, 16.0)
            .show(ui)
            .on_hover_text("All clips together");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let playing: Vec<_> = v.clips.iter().filter(|(_, c)| c.playing).map(|(id, _)| *id).collect();
            if ui
                .add_enabled(!playing.is_empty(), egui::Button::new("Stop all").small())
                .clicked()
            {
                out.commands
                    .extend(playing.into_iter().map(|id| Command::StopClip { id }));
            }
        });
    });
    ui.add_space(4.0);
    egui::ScrollArea::vertical()
        .id_salt("pads")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("pads").spacing([GAP, GAP]).show(ui, |ui| {
                let mut n = 0;
                for (id, c) in &v.clips {
                    let o = Pad::new(*id, &c.settings, c.playing).size(PAD).show(ui);
                    if o.play {
                        out.commands.push(Command::PlayClip { id: *id });
                    }
                    if o.stop {
                        out.commands.push(Command::StopClip { id: *id });
                    }
                    if let Some(settings) = o.settings {
                        out.commands.push(Command::UpdateClip { id: *id, settings });
                    }
                    if o.remove {
                        out.commands.push(Command::RemoveClip { id: *id });
                    }
                    n += 1;
                    if n % 3 == 0 {
                        ui.end_row();
                    }
                }
                if add_tile(ui) {
                    out.pick_clips = true;
                }
            });
            if v.clips.is_empty() {
                ui.add_space(6.0);
                ui.label(
                    RichText::new("Add sounds, or drop audio files here")
                        .size(11.0)
                        .color(DIM),
                );
            }
        });
}

fn add_tile(ui: &mut egui::Ui) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(PAD, PAD), Sense::click());
    let p = ui.painter();
    let color = if resp.hovered() {
        Color32::from_gray(200)
    } else {
        Color32::from_gray(110)
    };
    // A dashed outline.
    let r = rect.shrink(1.0);
    let corners = [
        r.left_top(),
        r.right_top(),
        r.right_bottom(),
        r.left_bottom(),
        r.left_top(),
    ];
    for w in corners.windows(2) {
        p.add(egui::Shape::dashed_line(
            &[w[0], w[1]],
            Stroke::new(1.0, color),
            5.0,
            4.0,
        ));
    }
    p.text(
        rect.center() - vec2(0.0, 8.0),
        egui::Align2::CENTER_CENTER,
        "+",
        egui::FontId::proportional(26.0),
        color,
    );
    p.text(
        rect.center() + vec2(0.0, 16.0),
        egui::Align2::CENTER_CENTER,
        "Add sounds",
        egui::FontId::proportional(11.0),
        color,
    );
    resp.on_hover_text("Pick audio files (or drop them on the overlay)")
        .clicked()
}
