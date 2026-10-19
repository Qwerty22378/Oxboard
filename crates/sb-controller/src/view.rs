//! `AppState` → `StateView`: what the UI is shown. Pure; the actor diffs consecutive views.

use crate::loops;
use crate::routing::Routing;
use crate::state::AppState;
use sb_shared::ui::{AppRoute, AppView, ClipView, DeviceView, Flow, Level, RouteStatus, Source, StateView};

pub fn project(state: &AppState) -> StateView {
    let (d, o) = (&state.desired, &state.observed);
    let mut view = StateView {
        master: d.master,
        engine: o.engine.clone(),
        routes: d.matrix.clone(),
        system_input: d.system_input.clone(),
        settings: o.settings.clone(),
        default_output: o.default_output.clone(),
        ..StateView::default()
    };
    // Present devices, plus the absent ones the user still has picked (greyed out).
    for (key, info, present) in o.devices.iter().map(|(k, i)| (k, i, true)).chain(
        d.known_devices
            .iter()
            .filter(|(k, _)| !o.devices.contains_key(*k))
            .map(|(k, i)| (k, i, false)),
    ) {
        view.devices.insert(
            key.clone(),
            DeviceView {
                name: info.name.clone(),
                flow: info.flow,
                present,
                selected: info.flow == Flow::Render && d.outputs.contains(key),
                loops: false,
                failed: o.failed.get(key).is_some_and(|f| f.retry_at.is_some()),
                // One way in per cable: the one clips should use.
                mic: info.flow == Flow::Render
                    && match o.loopbacks.get(key) {
                        Some(crate::state::Loopback::Capture(c)) => state.feed_of(c).as_ref() == Some(key),
                        _ => false,
                    },
            },
        );
    }
    // Running apps, plus the ones with settings kept for their next launch.
    let apps = o
        .sessions
        .keys()
        .chain(d.known_apps.keys().filter(|k| !o.sessions.contains_key(*k)));
    for key in apps {
        view.apps.insert(
            key.clone(),
            AppView {
                name: state.app_name(key).unwrap_or(&key.0).to_owned(),
                level: d
                    .app_levels
                    .get(key)
                    .copied()
                    .or_else(|| o.sessions.get(key).and_then(|s| s.level))
                    .unwrap_or(Level::UNITY),
                sessions: o.sessions.get(key).map_or(0, |s| s.pids.len() as u32),
                route: d.app_routes.get(key).map(|outputs| AppRoute {
                    outputs: outputs.clone(),
                    status: match (o.sessions.get(key), Routing::endpoint_device(state, key)) {
                        (None, _) => RouteStatus::NotRunning,
                        (Some(s), Some(dev)) if s.devices.contains(&dev) => RouteStatus::Active,
                        (Some(_), _) => RouteStatus::Pending,
                    },
                }),
            },
        );
    }
    for (id, clip) in &d.clips {
        view.clips.insert(
            *id,
            ClipView {
                path: clip.path.clone(),
                settings: clip.settings.clone(),
                playing: o.playing.contains(id),
            },
        );
    }
    // Matrix rows: inputs, apps, clips.
    let mut sources: Vec<Source> = view
        .devices
        .iter()
        .filter(|(_, d)| d.flow == Flow::Capture)
        .map(|(k, _)| Source::Device(k.clone()))
        .collect();
    sources.extend(view.apps.keys().map(|k| Source::App(k.clone())));
    sources.push(Source::Clips);
    let w = loops::would_loop(state, &sources);
    for k in &w.outputs {
        if let Some(dev) = view.devices.get_mut(k) {
            dev.loops = true;
        }
    }
    view.would_loop = w.cells;
    view
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DeviceInfo, Loopback, Observation};
    use sb_shared::ui::DeviceKey;

    #[test]
    fn a_cable_input_feeds_a_mic_and_speakers_dont() {
        let mut s = AppState::default();
        let dev = |id: &str, flow| Observation::DeviceAdded {
            key: DeviceKey(id.into()),
            info: DeviceInfo { name: id.into(), flow },
        };
        for o in [
            dev("cable-out", Flow::Capture),
            dev("speakers", Flow::Render),
            dev("CABLE In 16ch", Flow::Render),
            dev("CABLE Input", Flow::Render),
            Observation::Loopback {
                render: DeviceKey("CABLE In 16ch".into()),
                into: Some(Loopback::Capture(DeviceKey("cable-out".into()))),
            },
            Observation::Loopback {
                render: DeviceKey("CABLE Input".into()),
                into: Some(Loopback::Capture(DeviceKey("cable-out".into()))),
            },
        ] {
            s.observe(o);
        }
        let v = project(&s);
        let mic = |k: &str| v.devices[&DeviceKey(k.into())].mic;
        assert!(mic("CABLE Input"), "the cable's main way in");
        assert!(!mic("CABLE In 16ch"), "only one way in per cable gets the clips");
        assert!(!mic("speakers") && !mic("cable-out"));
    }
}
