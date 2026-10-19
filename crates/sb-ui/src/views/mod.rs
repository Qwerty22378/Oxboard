//! What's drawn. Pure, Elm-style: the store in, widgets and [`Command`]s out; nothing here
//! holds state the controller owns.
//!
//! The overlay frame (`frame`): mode tabs, status banner, toasts. Inside it, the mode's view
//! on the left and the pads on the right, in every mode: they're why people open it.
//! - [`Mode::Simple`] (`simple`): Interface 1, U4.
//! - Per-app (U5) and Matrix (U6) are tabs already, with their views to come.
//!
//! [`gallery`] shows every widget without a controller.

mod frame;
pub mod gallery;
mod pads;
mod simple;

use crate::store::Store;
use sb_shared::ui::Command;
use std::path::PathBuf;

/// The panel's size in logical pixels.
pub const SIZE: (f32, f32) = (800.0, 520.0);

/// Which view the overlay shows: the UI's own preference, not the controller's state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Simple,
    PerApp,
    Matrix,
}

#[derive(Debug, Default)]
pub struct Output {
    pub commands: Vec<Command>,
    /// Hide the overlay (Esc, the close button).
    pub close: bool,
    /// Open a file dialog to add clips.
    pub pick_clips: bool,
}

pub fn panel(ui: &mut egui::Ui, store: &Store, mode: &mut Mode) -> Output {
    let mut out = Output::default();
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        out.close = true;
    }
    // Audio files dropped anywhere on the overlay become clips.
    let dropped: Vec<PathBuf> = ui.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
    for path in dropped {
        out.commands.push(Command::AddClip {
            path: path.to_string_lossy().into_owned(),
            name: String::new(),
        });
    }
    frame::show(ui, store, mode, &mut out, |ui, mode, out| {
        let Some(view) = store.view() else {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.spinner();
                ui.label("Waiting for the soundboard service…");
            });
            return;
        };
        let pads_w = pads::WIDTH;
        ui.horizontal_top(|ui| {
            let left = ui.available_width() - pads_w - 16.0;
            ui.allocate_ui_with_layout(
                egui::vec2(left, ui.available_height()),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(left);
                    egui::ScrollArea::vertical()
                        .id_salt("mode")
                        .auto_shrink([false, false])
                        .show(ui, |ui| match mode {
                            Mode::Simple => simple::show(ui, store, view, out),
                            Mode::PerApp => coming(ui, "The per-app grid comes with U5 (Interface 2)."),
                            Mode::Matrix => coming(ui, "The routing matrix comes with U6 (Interface 3)."),
                        });
                },
            );
            ui.separator();
            ui.allocate_ui_with_layout(
                egui::vec2(pads_w, ui.available_height()),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    pads::show(ui, store, view, out);
                },
            );
        });
    });
    out
}

fn coming(ui: &mut egui::Ui, text: &str) {
    ui.add_space(30.0);
    ui.vertical_centered(|ui| ui.label(egui::RichText::new(text).color(crate::widgets::DIM)));
}
