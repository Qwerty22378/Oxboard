//! `AppState` → `StateView`: what the UI is shown. Pure; the actor diffs consecutive views.

use crate::loops;
use crate::state::AppState;
use sb_shared::ui::{AppView, ClipView, DeviceView, Flow, Level, Source, StateView};

pub fn project(state: &AppState) -> StateView {
    let (d, o) = (&state.desired, &state.observed);
    let mut view = StateView {
        master: d.master,
        engine: o.engine.clone(),
        routes: d.matrix.clone(),
        system_input: d.system_input.clone(),
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
                level: d.app_levels.get(key).copied().unwrap_or(Level::UNITY),
                sessions: o.sessions.get(key).map_or(0, |s| s.pids.len() as u32),
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
