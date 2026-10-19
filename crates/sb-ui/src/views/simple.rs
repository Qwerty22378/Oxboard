//! Interface 1, Simple mode (`UI Design` → Interface 1): where the PC's sound comes from, the
//! master, the outputs it plays to (cards to toggle, with meters), the apps' levels, and
//! "clips → mic + me". The rules live in [`crate::simple`].
//!
//! State that Simple mode can't show isn't flattened: apps sent to outputs of their own and
//! clips routed in the matrix get a note saying where to change them.

use super::Output;
use crate::simple::{self, ClipsRouting};
use crate::store::Store;
use crate::taper::{self, BOOST, UNITY_MAX};
use crate::widgets::{ACCENT, DIM, OFF, fader::Fader, meter::Meter};
use egui::{Color32, RichText, Sense, Stroke, StrokeKind, vec2};
use sb_shared::ui::{Command, DeviceKey, DeviceView, Flow, MeterTarget, StateView, SystemInput};

pub(super) fn heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(text).size(12.0).color(DIM).strong());
}

pub(super) fn mute_button(ui: &mut egui::Ui, muted: bool) -> bool {
    let text = RichText::new("M").strong();
    let b = egui::Button::new(if muted { text.color(Color32::BLACK) } else { text })
        .fill(if muted {
            Color32::from_rgb(235, 160, 60)
        } else {
            Color32::from_gray(50)
        })
        .min_size(vec2(22.0, 22.0));
    ui.add(b).on_hover_text(if muted { "Unmute" } else { "Mute" }).clicked()
}

fn note(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(11.0).color(Color32::from_rgb(150, 180, 230)));
}

pub fn show(ui: &mut egui::Ui, store: &Store, v: &StateView, out: &mut Output) {
    source_and_master(ui, store, v, out);

    heading(ui, "OUTPUTS");
    let outputs: Vec<_> = v.devices.iter().filter(|(_, d)| d.flow == Flow::Render).collect();
    if outputs.is_empty() {
        ui.label(RichText::new("No outputs").color(DIM));
    }
    let row = egui::Layout::left_to_right(egui::Align::TOP).with_main_wrap(true);
    ui.with_layout(row, |ui| {
        ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
        for (key, d) in outputs {
            let pending = store.is_pending(|c| matches!(c, Command::SelectOutput { device, .. } if device == key));
            let level = store.meters.get(&MeterTarget::Device(key.clone())).copied();
            if output_card(ui, key, d, level, pending) {
                out.commands.extend(simple::select_output(v, key, !d.selected));
            }
        }
    });

    heading(ui, "APPS");
    if v.apps.is_empty() {
        ui.label(RichText::new("No apps playing").color(DIM));
    }
    for (key, a) in &v.apps {
        ui.horizontal(|ui| {
            let color = if a.sessions > 0 { Color32::WHITE } else { DIM };
            ui.allocate_ui_with_layout(
                vec2(120.0, 22.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.set_width(120.0);
                    ui.add(egui::Label::new(RichText::new(&a.name).color(color)).truncate());
                },
            );
            if let Some(g) = Fader::new(("app", &key.0), a.level.gain, UNITY_MAX)
                .horizontal(170.0)
                .muted(a.level.muted)
                .label(&a.name)
                .show(ui)
                .gain
            {
                out.commands.push(Command::SetAppGain {
                    app: key.clone(),
                    gain: g,
                });
            }
            if mute_button(ui, a.level.muted) {
                out.commands.push(Command::SetAppMuted {
                    app: key.clone(),
                    muted: !a.level.muted,
                });
            }
            Meter::new(
                ("app", &key.0),
                store.meters.get(&MeterTarget::App(key.clone())).copied(),
                BOOST,
            )
            .size(8.0, 22.0)
            .show(ui);
            if a.sessions == 0 {
                ui.label(RichText::new("not running").size(11.0).color(DIM).italics());
            }
            if a.route.is_some() {
                ui.label(
                    RichText::new("own outputs")
                        .size(11.0)
                        .color(Color32::from_rgb(150, 180, 230)),
                )
                .on_hover_text("Sent to outputs of its own: change it in Per-app");
            }
        });
    }
    let custom = simple::custom_app_routes(v);
    if custom > 0 {
        let apps = if custom == 1 {
            "1 app is"
        } else {
            &format!("{custom} apps are")
        };
        note(ui, &format!("{apps} sent to outputs of their own — see Per-app"));
    }

    heading(ui, "SOUNDBOARD");
    clips_toggle(ui, store, v, out);
}

fn source_and_master(ui: &mut egui::Ui, store: &Store, v: &StateView, out: &mut Output) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("PC sound from").size(12.0).color(DIM));
        let name = |i: &Option<SystemInput>| match i {
            None => "Nothing".to_string(),
            Some(SystemInput::Device(k)) => v.devices.get(k).map_or(k.0.clone(), |d| d.name.clone()),
            Some(SystemInput::Endpoint(id)) => format!("Soundboard endpoint {id}"),
        };
        let mut chosen = v.system_input.clone();
        egui::ComboBox::from_id_salt("system-input")
            .selected_text(name(&chosen))
            .width(170.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut chosen, None, "Nothing");
                for (k, d) in v.devices.iter().filter(|(_, d)| d.flow == Flow::Capture) {
                    let label = if d.present { d.name.clone() } else { format!("{} (not connected)", d.name) };
                    ui.selectable_value(&mut chosen, Some(SystemInput::Device(k.clone())), label);
                }
            })
            .response
            .on_hover_text("Where the sound of the PC (games, apps) comes in: VB-Cable's \"CABLE Output\" until the soundboard's driver exists");
        if chosen != v.system_input {
            out.commands.push(Command::SetSystemInput(chosen));
        }
        Meter::new("system", store.meters.get(&MeterTarget::System).copied(), BOOST)
            .size(8.0, 22.0)
            .show(ui)
            .on_hover_text("The PC's sound, coming in");

        ui.add_space(16.0);
        ui.label(RichText::new("MASTER").size(12.0).color(DIM).strong());
        if let Some(g) = Fader::new("master", v.master.gain, BOOST)
            .horizontal(150.0)
            .muted(v.master.muted)
            .label("Master")
            .show(ui)
            .gain
        {
            out.commands.push(Command::SetMasterGain(g));
        }
        if mute_button(ui, v.master.muted) {
            out.commands.push(Command::SetMasterMuted(!v.master.muted));
        }
        ui.label(RichText::new(taper::format_db(taper::gain_to_db(v.master.gain))).size(11.0));
    });
}

/// A card per output: click to play to it too (or stop). Returns whether it was clicked.
fn output_card(
    ui: &mut egui::Ui,
    key: &DeviceKey,
    d: &DeviceView,
    level: Option<sb_shared::events::MeterLevels>,
    pending: bool,
) -> bool {
    let size = vec2(150.0, 54.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    // An output that would feed the PC's sound back into itself can't be turned on.
    let enabled = d.selected || !d.loops;
    let p = ui.painter();
    let fill = if d.selected { ACCENT.gamma_multiply(0.28) } else { OFF };
    p.rect_filled(rect, 8.0, fill);
    let border = if d.selected { ACCENT } else { Color32::from_gray(60) };
    let hot = resp.hovered() && enabled;
    p.rect_stroke(
        rect,
        8.0,
        Stroke::new(if hot { 2.0 } else { 1.0 }, border),
        StrokeKind::Inside,
    );
    // The tick box.
    let tick = egui::Rect::from_min_size(rect.min + vec2(10.0, 10.0), vec2(14.0, 14.0));
    p.rect_stroke(tick, 3.0, Stroke::new(1.5, Color32::from_gray(170)), StrokeKind::Inside);
    if d.selected {
        p.rect_filled(tick.shrink(3.0), 1.5, ACCENT);
    }
    let text_color = if d.present && enabled { Color32::WHITE } else { DIM };
    let name = if d.name.chars().count() > 17 {
        format!("{}…", d.name.chars().take(16).collect::<String>())
    } else {
        d.name.clone()
    };
    p.text(
        rect.min + vec2(32.0, 17.0),
        egui::Align2::LEFT_CENTER,
        name,
        egui::FontId::proportional(13.0),
        text_color,
    );
    let status = if !d.present {
        "not connected"
    } else if d.failed {
        "won't open, retrying"
    } else if d.loops && !d.selected {
        "would loop"
    } else if d.mic {
        "feeds a mic"
    } else {
        ""
    };
    p.text(
        rect.min + vec2(32.0, 37.0),
        egui::Align2::LEFT_CENTER,
        status,
        egui::FontId::proportional(10.5),
        DIM,
    );
    // The meter, along the right edge.
    let meter_rect = egui::Rect::from_min_size(rect.right_top() + vec2(-18.0, 8.0), vec2(8.0, size.y - 16.0));
    // In child uis of their own: placed inside the card, without moving the row's cursor.
    let mut inside = |r: egui::Rect| ui.new_child(egui::UiBuilder::new().max_rect(r));
    Meter::new(("out", &key.0), level, BOOST)
        .size(8.0, size.y - 16.0)
        .show(&mut inside(meter_rect));
    if pending {
        let r = egui::Rect::from_min_size(rect.right_top() + vec2(-40.0, 8.0), vec2(16.0, 16.0));
        inside(r).add(egui::Spinner::new().size(14.0));
    }
    let tip = if !enabled {
        "This would feed the PC's sound back into itself".to_string()
    } else if d.selected {
        format!("Playing to {} (click to stop)", d.name)
    } else {
        format!("Click to play to {} too", d.name)
    };
    let resp = resp.on_hover_text(tip);
    resp.clicked() && enabled
}

fn clips_toggle(ui: &mut egui::Ui, store: &Store, v: &StateView, out: &mut Output) {
    let state = simple::clips_routing(v);
    let pending = store.is_pending(|c| {
        matches!(
            c,
            Command::SetRoute {
                from: sb_shared::ui::Source::Clips,
                ..
            }
        )
    });
    ui.horizontal(|ui| {
        let mut on = state == ClipsRouting::On;
        let enabled = state != ClipsRouting::Custom;
        let resp = ui.add_enabled(enabled, egui::Checkbox::new(&mut on, "Clips into mic + me"));
        if resp.changed() {
            out.commands.extend(simple::set_clips(v, on));
        }
        if pending {
            ui.spinner();
        }
    });
    let targets = simple::clip_targets(v);
    let mics: Vec<&str> = targets
        .iter()
        .filter_map(|k| v.devices.get(k).filter(|d| d.mic).map(|d| d.name.as_str()))
        .collect();
    let explain = match state {
        ClipsRouting::Custom => "Clips are routed in the Matrix — change them there".to_string(),
        _ if mics.is_empty() => {
            "No mic feed found (VB-Cable's \"CABLE Input\"): clips go to your outputs only".to_string()
        }
        _ => format!("Into {} and every output you play to", mics.join(", ")),
    };
    if state == ClipsRouting::Custom {
        note(ui, &explain);
    } else {
        ui.label(RichText::new(explain).size(11.0).color(DIM));
    }
}
