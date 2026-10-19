//! `sb-ui --gallery`: every widget against made-up state, with no controller. Commands are
//! applied locally after a delay, like a controller's echo, so the faders' optimistic hand-over
//! shows; the meters move on a fake signal at 30 Hz. For screenshots and trying things by hand.

use crate::coalesce::Coalescer;
use crate::log;
use crate::shell::Content;
use crate::taper::{BOOST, UNITY_MAX, format_db, gain_to_db};
use crate::widgets::{DIM, cell::Cell, fader::Fader, meter::Meter, pad::Pad};
use egui::RichText;
use sb_shared::events::MeterLevels;
use sb_shared::graph::Retrigger;
use sb_shared::ui::{ClipId, ClipSettings, Command};
use std::time::{Duration, Instant};

/// How late the made-up controller's echo comes.
const ECHO: Duration = Duration::from_millis(120);

pub struct Gallery {
    start: Instant,
    master: f32,
    apps: Vec<(String, f32)>,
    clips: Vec<(ClipSettings, bool)>,
    /// rows × columns of gain (None: off); [1][2] would loop.
    cells: Vec<Vec<Option<f32>>>,
    coalescer: Coalescer,
    /// Commands on their way "to the controller".
    in_flight: Vec<(Instant, Command)>,
    sent: usize,
}

const ROWS: [&str; 3] = ["Mic", "Game", "Clips"];
const COLS: [&str; 3] = ["Headphones", "Speakers", "Soundboard Mic"];

impl Default for Gallery {
    fn default() -> Self {
        let clip = |name: &str, looped| {
            (
                ClipSettings {
                    name: name.into(),
                    gain: 1.0,
                    retrigger: Retrigger::Restart,
                    looped,
                },
                false,
            )
        };
        let mut clips = vec![
            clip("Airhorn", false),
            clip("Drum roll", false),
            clip("Applause", true),
            clip("Sad trombone", false),
        ];
        clips[1].1 = true;
        Self {
            start: Instant::now(),
            master: 0.8,
            apps: vec![("Game".into(), 1.0), ("Discord".into(), 0.6), ("Spotify".into(), 0.3)],
            clips,
            cells: vec![
                vec![None, None, Some(1.0)],
                vec![Some(1.0), Some(0.5), None],
                vec![Some(0.7), None, Some(1.0)],
            ],
            coalescer: Coalescer::default(),
            in_flight: Vec::new(),
            sent: 0,
        }
    }
}

impl Gallery {
    fn levels(&self, phase: f32) -> MeterLevels {
        let t = self.start.elapsed().as_secs_f32() + phase;
        let env = ((t * 1.3).sin() * 0.5 + 0.5) * (0.5 + 0.5 * (t * 7.0).sin().abs());
        let peak = (env * 1.1).min(1.2);
        MeterLevels {
            peak: [peak, peak * 0.9],
            rms: [env * 0.5, env * 0.45],
        }
    }

    fn send(&mut self, cmd: Command, now: Instant) {
        if let Some(c) = self.coalescer.push(cmd, now) {
            self.sent += 1;
            self.in_flight.push((now + ECHO, c));
        }
    }

    /// The made-up controller applies what's arrived.
    fn echo(&mut self, now: Instant) {
        for c in self.coalescer.due(now) {
            self.sent += 1;
            self.in_flight.push((now + ECHO, c));
        }
        let (ready, later): (Vec<_>, Vec<_>) = self.in_flight.drain(..).partition(|(t, _)| *t <= now);
        self.in_flight = later;
        for (_, c) in ready {
            match c {
                Command::SetMasterGain(g) => self.master = g,
                Command::SetAppGain { app, gain } => {
                    if let Some(a) = self.apps.iter_mut().find(|(n, _)| *n == app.0) {
                        a.1 = gain;
                    }
                }
                Command::SetRoute { .. } => {}
                Command::PlayClip { id } => self.clips[id.0 as usize].1 = true,
                Command::StopClip { id } => self.clips[id.0 as usize].1 = false,
                Command::UpdateClip { id, settings } => self.clips[id.0 as usize].0 = settings,
                other => log::line(format!("gallery: {other:?}")),
            }
        }
    }
}

impl Content for Gallery {
    fn size(&self) -> (f32, f32) {
        (560.0, 520.0)
    }

    fn update(&mut self) -> bool {
        false
    }

    fn ui(&mut self, ui: &mut egui::Ui) -> bool {
        let now = Instant::now();
        self.echo(now);
        let mut close = ui.input(|i| i.key_pressed(egui::Key::Escape));
        let mut cmds = Vec::new();
        let frame = egui::Frame::new()
            .fill(super::frame::BG)
            .corner_radius(14)
            .inner_margin(14.0)
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(70)));
        ui.ctx().set_visuals(egui::Visuals::dark());
        egui::CentralPanel::no_frame().show(ui, |ui| {
            frame.show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Widget gallery").size(18.0).strong());
                    ui.label(
                        RichText::new(format!(
                            "{} commands sent, {} in flight",
                            self.sent,
                            self.in_flight.len()
                        ))
                        .color(DIM)
                        .size(11.0),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        close |= ui.button("×").clicked();
                    });
                });
                ui.separator();
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new("MASTER").size(12.0).color(DIM));
                        ui.horizontal(|ui| {
                            if let Some(g) = Fader::new("g-master", self.master, BOOST).length(160.0).show(ui).gain {
                                cmds.push(Command::SetMasterGain(g));
                            }
                            Meter::new("g-master", Some(self.levels(0.0)), BOOST)
                                .size(12.0, 160.0)
                                .show(ui);
                        });
                        ui.label(RichText::new(format_db(gain_to_db(self.master))).size(11.0));
                    });
                    ui.add_space(10.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new("APPS").size(12.0).color(DIM));
                        for i in 0..self.apps.len() {
                            let (name, gain) = self.apps[i].clone();
                            ui.horizontal(|ui| {
                                ui.add_sized([70.0, 20.0], egui::Label::new(&name));
                                if let Some(g) = Fader::new(("g-app", i), gain, UNITY_MAX)
                                    .horizontal(170.0)
                                    .show(ui)
                                    .gain
                                {
                                    cmds.push(Command::SetAppGain {
                                        app: sb_shared::ui::AppKey(name.clone()),
                                        gain: g,
                                    });
                                }
                                Meter::new(("g-app", i), Some(self.levels(i as f32)), BOOST)
                                    .size(8.0, 22.0)
                                    .show(ui);
                            });
                        }
                        ui.add_space(8.0);
                        ui.label(RichText::new("MATRIX").size(12.0).color(DIM));
                        egui::Grid::new("g-matrix").spacing([4.0, 4.0]).show(ui, |ui| {
                            ui.label("");
                            for c in COLS {
                                ui.label(RichText::new(c).size(10.0).color(DIM));
                            }
                            ui.end_row();
                            for (r, row) in ROWS.iter().enumerate() {
                                ui.label(RichText::new(*row).size(11.0));
                                for (c, col) in COLS.iter().enumerate() {
                                    let would_loop = (r, c) == (1, 2);
                                    let o = Cell::new(("g-cell", r, c), self.cells[r][c], would_loop).show(ui);
                                    if let Some(g) = o.set {
                                        self.cells[r][c] = g;
                                        log::line(format!("cell {row} → {col}: {g:?}"));
                                    }
                                }
                                ui.end_row();
                            }
                        });
                    });
                });
                ui.add_space(8.0);
                ui.label(RichText::new("SOUNDBOARD").size(12.0).color(DIM));
                ui.horizontal_wrapped(|ui| {
                    for i in 0..self.clips.len() {
                        let (settings, playing) = self.clips[i].clone();
                        let id = ClipId(i as u32);
                        let hotkey = (i < 3).then(|| ["F1", "F2", "F3"][i]);
                        let o = Pad::new(id, &settings, playing).hotkey(hotkey).show(ui);
                        if o.play {
                            cmds.push(Command::PlayClip { id });
                        }
                        if o.stop {
                            cmds.push(Command::StopClip { id });
                        }
                        if let Some(s) = o.settings {
                            cmds.push(Command::UpdateClip { id, settings: s });
                        }
                        if o.remove {
                            log::line(format!("gallery: remove {}", settings.name));
                        }
                    }
                });
            });
        });
        for c in cmds {
            self.send(c, now);
        }
        // The echo, the coalescer's trailing value and the meters all need frames.
        ui.ctx().request_repaint_after(Duration::from_millis(33));
        close
    }

    fn visibility(&mut self, _shown: bool) {}
}
