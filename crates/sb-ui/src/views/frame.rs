//! The overlay frame: title bar with the mode tabs, a status banner only when something's
//! wrong, the body, and toasts.

use super::{Mode, Output};
use crate::store::{Connection, Store};
use egui::{Color32, CornerRadius, RichText, Stroke};
use sb_shared::ui::{Command, EngineStatus};
use std::time::Instant;

pub const BG: Color32 = Color32::from_rgba_premultiplied(17, 17, 24, 240);
const WARN: Color32 = Color32::from_rgb(240, 180, 70);
const BAD: Color32 = Color32::from_rgb(235, 95, 80);

pub fn show(
    ui: &mut egui::Ui,
    store: &Store,
    mode: &mut Mode,
    out: &mut Output,
    body: impl FnOnce(&mut egui::Ui, &mut Mode, &mut Output),
) {
    ui.ctx().set_visuals(egui::Visuals::dark());
    let frame = egui::Frame::new()
        .fill(BG)
        .stroke(Stroke::new(1.0, Color32::from_gray(70)))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(14.0);
    egui::CentralPanel::no_frame().show(ui, |ui| {
        frame.show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            title(ui, store, mode, out);
            banner(ui, store, out);
            ui.separator();
            let toasts = store.toasts.len() as f32 * 26.0;
            let body_h = ui.available_height() - toasts;
            ui.allocate_ui(egui::vec2(ui.available_width(), body_h), |ui| {
                ui.set_height(body_h);
                body(ui, mode, out);
            });
            for t in &store.toasts {
                toast(ui, &t.text);
            }
            if let Some(next) = store.toasts.iter().map(|t| t.until).min() {
                ui.ctx()
                    .request_repaint_after(next.saturating_duration_since(Instant::now()));
            }
        });
    });
}

fn title(ui: &mut egui::Ui, store: &Store, mode: &mut Mode, out: &mut Output) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Soundboard").size(18.0).strong());
        ui.add_space(12.0);
        for (m, label) in [
            (Mode::Simple, "Simple"),
            (Mode::PerApp, "Per-app"),
            (Mode::Matrix, "Matrix"),
        ] {
            ui.selectable_value(mode, m, label);
        }
        ui.add_space(8.0);
        let (dot, tip) = match &store.connection {
            Connection::Connected { controller } => (Color32::from_rgb(80, 200, 120), controller.clone()),
            Connection::Connecting => (WARN, "connecting".into()),
            Connection::Lost { reason } => (BAD, reason.clone()),
        };
        // Drawn, not a glyph: egui's bundled fonts have no "●".
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, dot);
        resp.on_hover_text(tip);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("×").on_hover_text("Hide (Esc)").clicked() {
                out.close = true;
            }
        });
    });
}

/// One line per problem, with the action that fixes it where there is one.
fn banner(ui: &mut egui::Ui, store: &Store, out: &mut Output) {
    match &store.connection {
        Connection::Connected { .. } => {}
        Connection::Connecting => warn(ui, WARN, "Connecting to the soundboard service…"),
        Connection::Lost { reason } => {
            ui.label(RichText::new("⚠ The soundboard service isn't running").color(BAD))
                .on_hover_text(reason);
        }
    }
    let Some(v) = store.view() else { return };
    match &v.engine {
        EngineStatus::Running { .. } => {}
        EngineStatus::Starting => warn(ui, WARN, "Audio engine starting…"),
        EngineStatus::Down => warn(ui, WARN, "Audio engine not running"),
        EngineStatus::Restarting { attempt } => {
            warn(ui, WARN, &format!("Audio engine restarting (attempt {attempt})…"))
        }
        EngineStatus::Failed { reason } => {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("⚠ Audio engine stopped: {reason}")).color(BAD));
                let busy = store.is_pending(|c| matches!(c, Command::RestartEngine));
                if ui.add_enabled(!busy, egui::Button::new("Restart")).clicked() {
                    out.commands.push(Command::RestartEngine);
                }
            });
        }
    }
    if let Some(e) = &v.settings.load_error {
        ui.horizontal_wrapped(|ui| {
            let moved = e
                .backup
                .as_ref()
                .map_or(String::new(), |b| format!(" It was moved to {b}."));
            ui.label(
                RichText::new(format!(
                    "⚠ Your settings couldn't be read ({}), so the soundboard started fresh.{moved}",
                    e.reason
                ))
                .color(WARN),
            );
            if ui.small_button("Dismiss").clicked() {
                out.commands.push(Command::DismissLoadError);
            }
        });
    }
    if let Some(e) = &v.settings.save_error {
        warn(ui, BAD, &format!("Settings can't be saved: {e}"));
    }
}

fn warn(ui: &mut egui::Ui, color: Color32, text: &str) {
    ui.label(RichText::new(format!("⚠ {text}")).color(color));
}

fn toast(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(Color32::from_rgb(60, 32, 32))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(Color32::from_rgb(255, 210, 200)));
        });
}
