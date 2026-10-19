//! The widgets under synthetic input: egui run headless, frame by frame, with made-up mouse
//! and keyboard events, checking what each widget reports. (egui only builds on Windows here.)
#![cfg(windows)]

use egui::{Event, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, pos2, vec2};
use sb_shared::graph::Retrigger;
use sb_shared::ui::{ClipId, ClipSettings};
use sb_ui::taper::{BOOST, gain_to_db};
use sb_ui::widgets::{cell::Cell, fader::Fader, pad::Pad};

/// A headless egui with a clock and an input queue.
struct Harness {
    ctx: egui::Context,
    t: f64,
    events: Vec<Event>,
    /// Where the widget landed last frame.
    at: Rect,
}

impl Harness {
    fn new() -> Self {
        Self {
            ctx: egui::Context::default(),
            t: 0.0,
            events: Vec::new(),
            at: Rect::NOTHING,
        }
    }

    /// One frame (`dt` seconds after the last) with whatever input is queued; `show` draws the
    /// widget and returns what it reported.
    fn frame<R>(&mut self, dt: f64, mut show: impl FnMut(&mut egui::Ui) -> R) -> R {
        self.t += dt;
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0))),
            time: Some(self.t),
            events: std::mem::take(&mut self.events),
            ..Default::default()
        };
        let mut result = None;
        let mut at = Rect::NOTHING;
        let mut out = self.ctx.run_ui(input, |ui| {
            egui::CentralPanel::no_frame().show(ui, |ui| {
                let r = ui.scope(|ui| show(ui));
                at = r.response.rect;
                result = Some(r.inner);
            });
        });
        // Nothing paints here; egui wants its texture changes handled or cleared.
        out.textures_delta.clear();
        self.at = at;
        result.unwrap()
    }

    fn move_to(&mut self, p: Pos2) {
        self.events.push(Event::PointerMoved(p));
    }

    fn button(&mut self, p: Pos2, pressed: bool) {
        self.events.push(Event::PointerMoved(p));
        self.events.push(Event::PointerButton {
            pos: p,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::default(),
        });
    }

    fn key(&mut self, key: Key) {
        self.events.push(Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::default(),
        });
    }
}

fn db(g: f32) -> f32 {
    gain_to_db(g)
}

#[test]
fn a_fader_drags_relatively_shows_its_own_value_then_hands_back() {
    let mut h = Harness::new();
    let given = 0.5;
    let show = |ui: &mut egui::Ui| Fader::new("f", given, BOOST).length(140.0).show(ui).gain;
    assert_eq!(h.frame(0.0, show), None);
    let knob = pos2(
        h.at.center().x,
        h.at.bottom() - 6.0 - BOOST.gain_to_position(given) * 128.0,
    );

    // Press on the knob and drag up 30 px: louder, step by step. egui decides it's a drag
    // after a few pixels; those aren't lost (moves are measured from where it was pressed).
    h.button(knob, true);
    h.frame(0.02, show);
    let mut last = given;
    let mut sent = 0;
    for i in 1..=6 {
        h.move_to(knob - vec2(0.0, 5.0 * i as f32));
        if let Some(g) = h.frame(0.02, show) {
            assert!(g > last, "{g} after {last}");
            (last, sent) = (g, sent + 1);
        }
    }
    assert!(sent >= 5, "{sent} values for 6 moves");
    let expected = BOOST.position_to_gain(BOOST.gain_to_position(given) + 30.0 / 128.0);
    assert!(
        (db(last) - db(expected)).abs() < 0.01,
        "{} vs {}",
        db(last),
        db(expected)
    );
    h.button(knob - vec2(0.0, 30.0), false);
    h.frame(0.02, show);

    // The controller hasn't echoed yet (still given 0.5): a wheel step now starts from the
    // fader's own value, not the stale one.
    h.events.push(Event::MouseWheel {
        unit: egui::MouseWheelUnit::Line,
        delta: vec2(0.0, 1.0),
        phase: egui::TouchPhase::Move,
        modifiers: Modifiers::default(),
    });
    h.move_to(h.at.center());
    let g = h.frame(0.05, show).expect("a wheel notch");
    assert!(
        (db(g) - (db(last) + 1.0)).abs() < 0.05,
        "+1 dB from {}: {}",
        db(last),
        db(g)
    );
    // Once the grace is over, the given value is the truth again: a wheel step starts there.
    h.frame(1.0, show);
    h.events.push(Event::MouseWheel {
        unit: egui::MouseWheelUnit::Line,
        delta: vec2(0.0, -1.0),
        phase: egui::TouchPhase::Move,
        modifiers: Modifiers::default(),
    });
    let g = h.frame(0.05, show).unwrap();
    assert!((db(g) - (db(given) - 1.0)).abs() < 0.05, "{}", db(g));
}

#[test]
fn a_double_click_resets_a_fader_to_0_db() {
    let mut h = Harness::new();
    let show = |ui: &mut egui::Ui| Fader::new("f", 0.2, BOOST).show(ui).gain;
    h.frame(0.0, show);
    let p = h.at.center();
    let mut got = None;
    for _ in 0..2 {
        h.button(p, true);
        got = got.or(h.frame(0.03, show));
        h.button(p, false);
        got = h.frame(0.03, show).or(got);
    }
    assert_eq!(got, Some(1.0));
}

#[test]
fn a_cell_toggles_and_a_double_click_types_a_level() {
    let mut h = Harness::new();
    let mut gain: Option<f32> = None;
    let show = |ui: &mut egui::Ui, g: Option<f32>| Cell::new("c", g, false).show(ui);
    h.frame(0.0, |ui| show(ui, gain));
    let p = h.at.center();

    // A click: on at 0 dB.
    h.button(p, true);
    h.frame(0.03, |ui| show(ui, gain));
    h.button(p, false);
    let o = h.frame(0.03, |ui| show(ui, gain));
    assert_eq!(o.set, Some(Some(1.0)));
    gain = Some(1.0);

    // Another click soon after is a double-click: back to off (the first click undone), and
    // the dB box opens.
    h.button(p, true);
    h.frame(0.05, |ui| show(ui, gain));
    h.button(p, false);
    let o = h.frame(0.03, |ui| show(ui, gain));
    assert_eq!(o.set, Some(None), "the double-click's first click is undone");
    gain = None;
    // Type −6 and press Enter (select-all first, as the box opens with "off" in it).
    h.frame(0.03, |ui| show(ui, gain));
    h.events.push(Event::Key {
        key: Key::A,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::COMMAND,
    });
    h.events.push(Event::Text("-6".into()));
    h.frame(0.03, |ui| show(ui, gain));
    h.key(Key::Enter);
    let o = h.frame(0.03, |ui| show(ui, gain));
    let g = o.set.flatten().expect("a level");
    assert!((db(g) + 6.0).abs() < 0.01, "{}", db(g));
}

#[test]
fn a_cell_that_would_loop_cant_be_switched_on() {
    let mut h = Harness::new();
    let show = |ui: &mut egui::Ui| Cell::new("c", None, true).show(ui);
    h.frame(0.0, show);
    let p = h.at.center();
    h.button(p, true);
    h.frame(0.03, show);
    h.button(p, false);
    assert_eq!(h.frame(0.03, show).set, None);
}

#[test]
fn a_cell_drag_sets_the_level_in_db_steps() {
    let mut h = Harness::new();
    let show = |ui: &mut egui::Ui| Cell::new("c", Some(1.0), false).show(ui);
    h.frame(0.0, show);
    let p = h.at.center();
    h.button(p, true);
    h.frame(0.03, show);
    // 12 px down: −3 dB from 0 (1 dB per 4 px), relative to the value given.
    let mut last = None;
    for i in 1..=3 {
        h.move_to(p + vec2(0.0, 4.0 * i as f32));
        if let Some(s) = h.frame(0.03, show).set {
            last = s;
        }
    }
    // The controller hasn't echoed (the cell is still given 0 dB): the steps still add up, from
    // what the cell last sent. 12 px down is −3 dB.
    let g = last.expect("a level");
    assert!((db(g) + 3.0).abs() < 0.01, "{}", db(g));
}

#[test]
fn a_pad_click_plays() {
    let mut h = Harness::new();
    let settings = ClipSettings {
        name: "Airhorn".into(),
        gain: 1.0,
        retrigger: Retrigger::Restart,
        looped: false,
    };
    let show = |ui: &mut egui::Ui| Pad::new(ClipId(0), &settings, false).show(ui);
    h.frame(0.0, show);
    let p = h.at.center();
    h.button(p, true);
    h.frame(0.03, show);
    h.button(p, false);
    let o = h.frame(0.03, show);
    assert!(o.play && !o.stop && o.settings.is_none() && !o.remove, "{o:?}");
}
