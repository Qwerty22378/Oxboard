//! Interface 1's rules, kept out of the drawing code (and tested on every platform).
//!
//! **"Clips → mic + me"** is one toggle over matrix routes: clips play into every selected
//! output ("me") and every device that feeds a microphone ("mic", e.g. VB-Cable's input), at
//! 0 dB. It reads:
//! - **On** when clips are routed to exactly those (whatever the gains),
//! - **Off** when they're routed nowhere,
//! - **Custom** otherwise: someone set them up in the matrix. Simple mode leaves that alone
//!   and says where to change it (switching modes never destroys state).
//!
//! While it's on, selecting an output adds the clips route to it, and deselecting one removes
//! it, so it stays on.

use sb_shared::ui::{Command, DeviceKey, Flow, Sink, Source, StateView};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipsRouting {
    Off,
    On,
    Custom,
}

/// Where "mic + me" sends clips: selected outputs and mic feeds, unless that would loop.
pub fn clip_targets(v: &StateView) -> BTreeSet<DeviceKey> {
    v.devices
        .iter()
        .filter(|(_, d)| d.flow == Flow::Render && (d.selected || d.mic))
        .filter(|(k, _)| !v.would_loop.contains(&(Source::Clips, Sink::Device((*k).clone()))))
        .map(|(k, _)| k.clone())
        .collect()
}

/// Where clips are routed now.
pub fn clip_routes(v: &StateView) -> BTreeSet<DeviceKey> {
    v.routes
        .keys()
        .filter(|(from, _)| *from == Source::Clips)
        .map(|(_, Sink::Device(to))| to.clone())
        .collect()
}

pub fn clips_routing(v: &StateView) -> ClipsRouting {
    let routes = clip_routes(v);
    if routes.is_empty() {
        ClipsRouting::Off
    } else if routes == clip_targets(v) {
        ClipsRouting::On
    } else {
        ClipsRouting::Custom
    }
}

fn route(to: &DeviceKey, gain: Option<f32>) -> Command {
    Command::SetRoute {
        from: Source::Clips,
        to: Sink::Device(to.clone()),
        gain,
    }
}

/// The commands that turn "mic + me" on or off (nothing while it's custom).
pub fn set_clips(v: &StateView, on: bool) -> Vec<Command> {
    match (clips_routing(v), on) {
        (ClipsRouting::Off, true) => clip_targets(v).iter().map(|k| route(k, Some(1.0))).collect(),
        (ClipsRouting::On, false) => clip_routes(v).iter().map(|k| route(k, None)).collect(),
        _ => Vec::new(),
    }
}

/// Select or deselect an output; with "mic + me" on, its clips route follows.
pub fn select_output(v: &StateView, device: &DeviceKey, selected: bool) -> Vec<Command> {
    let mut cmds = vec![Command::SelectOutput {
        device: device.clone(),
        selected,
    }];
    if clips_routing(v) == ClipsRouting::On {
        let is_mic = v.devices.get(device).is_some_and(|d| d.mic);
        let looping = v.would_loop.contains(&(Source::Clips, Sink::Device(device.clone())));
        if selected && !looping {
            cmds.push(route(device, Some(1.0)));
        } else if !selected && !is_mic && clip_routes(v).contains(device) {
            cmds.push(route(device, None));
        }
    }
    cmds
}

/// How many apps are sent somewhere of their own (Interface 2): Simple mode only says so.
pub fn custom_app_routes(v: &StateView) -> usize {
    v.apps.values().filter(|a| a.route.is_some()).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_shared::ui::DeviceView;

    fn dev(name: &str, flow: Flow, selected: bool, mic: bool) -> (DeviceKey, DeviceView) {
        (
            DeviceKey(name.into()),
            DeviceView {
                name: name.into(),
                flow,
                present: true,
                selected,
                loops: false,
                failed: false,
                mic,
            },
        )
    }

    fn k(name: &str) -> DeviceKey {
        DeviceKey(name.into())
    }

    fn view() -> StateView {
        let mut v = StateView::default();
        v.devices.extend([
            dev("headphones", Flow::Render, true, false),
            dev("speakers", Flow::Render, false, false),
            dev("cable-in", Flow::Render, false, true),
            dev("mic", Flow::Capture, false, false),
        ]);
        v
    }

    /// What the controller would end up with after `cmds` (routes and selections only).
    fn apply(v: &mut StateView, cmds: &[Command]) {
        for c in cmds {
            match c {
                Command::SetRoute { from, to, gain } => {
                    match gain {
                        Some(g) => v.routes.insert((from.clone(), to.clone()), *g),
                        None => v.routes.remove(&(from.clone(), to.clone())),
                    };
                }
                Command::SelectOutput { device, selected } => {
                    v.devices.get_mut(device).unwrap().selected = *selected;
                }
                _ => {}
            }
        }
    }

    #[test]
    fn on_means_the_mic_and_every_selected_output() {
        let mut v = view();
        assert_eq!(clips_routing(&v), ClipsRouting::Off);
        assert_eq!(clip_targets(&v), [k("cable-in"), k("headphones")].into());
        let cmds = set_clips(&v, true);
        assert_eq!(cmds.len(), 2);
        apply(&mut v, &cmds);
        assert_eq!(clips_routing(&v), ClipsRouting::On);
        let c = set_clips(&v, false);
        apply(&mut v, &c);
        assert_eq!(clips_routing(&v), ClipsRouting::Off);
        assert!(v.routes.is_empty());
    }

    #[test]
    fn selecting_outputs_keeps_it_on() {
        let mut v = view();
        let c = set_clips(&v, true);
        apply(&mut v, &c);
        let c = select_output(&v, &k("speakers"), true);
        apply(&mut v, &c);
        assert_eq!(clips_routing(&v), ClipsRouting::On);
        assert!(clip_routes(&v).contains(&k("speakers")));
        let c = select_output(&v, &k("headphones"), false);
        apply(&mut v, &c);
        assert_eq!(clips_routing(&v), ClipsRouting::On);
        assert_eq!(clip_routes(&v), [k("cable-in"), k("speakers")].into());
        // Off: selecting touches only the output.
        let c = set_clips(&v, false);
        apply(&mut v, &c);
        assert_eq!(select_output(&v, &k("headphones"), true).len(), 1);
    }

    #[test]
    fn matrix_routing_is_custom_and_left_alone() {
        let mut v = view();
        v.routes.insert((Source::Clips, Sink::Device(k("speakers"))), 0.5);
        assert_eq!(clips_routing(&v), ClipsRouting::Custom);
        assert!(set_clips(&v, true).is_empty() && set_clips(&v, false).is_empty());
    }

    #[test]
    fn a_target_that_would_loop_is_left_out() {
        let mut v = view();
        v.would_loop.insert((Source::Clips, Sink::Device(k("cable-in"))));
        assert_eq!(clip_targets(&v), [k("headphones")].into());
        let c = set_clips(&v, true);
        apply(&mut v, &c);
        assert_eq!(clips_routing(&v), ClipsRouting::On);
    }
}
