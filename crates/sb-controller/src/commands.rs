//! UI commands that only change state. Each validates first and changes nothing on failure.
//! A command that would close a feedback loop ([`crate::loops`]) is refused too.
//!
//! Commands that also drive Windows, the driver or the engine (app routing, C5) become small
//! state machines advanced by worker results; these ones finish on the spot. The engine's
//! one-shot commands (clip triggers, restarting it) are the actor's.

use crate::loops;
use crate::state::{AppState, ClipDef};
use sb_shared::ui::{
    AppKey, ClipId, ClipSettings, Command, DeviceKey, ErrorInfo, ErrorKind, Flow, MAX_GAIN, Sink, Source, SystemInput,
    valid_gain,
};

type Outcome = Result<(), ErrorInfo>;

fn invalid(msg: impl Into<String>) -> ErrorInfo {
    ErrorInfo::new(ErrorKind::Invalid, msg)
}

fn not_found(msg: impl Into<String>) -> ErrorInfo {
    ErrorInfo::new(ErrorKind::NotFound, msg)
}

fn gain(g: f32) -> Outcome {
    if valid_gain(g) {
        Ok(())
    } else {
        Err(invalid(format!("gain {g} is outside 0 to {MAX_GAIN}")))
    }
}

fn device(state: &AppState, key: &DeviceKey, flow: Flow) -> Outcome {
    match state.device(key) {
        None => Err(not_found(format!("no device {}", key.0))),
        Some(d) if d.flow != flow => Err(invalid(format!(
            "{} is {} device",
            d.name,
            if d.flow == Flow::Render {
                "an output"
            } else {
                "an input"
            }
        ))),
        Some(_) => Ok(()),
    }
}

fn app(state: &AppState, key: &AppKey) -> Outcome {
    match state.app_name(key) {
        Some(_) => Ok(()),
        None => Err(not_found(format!("no app {}", key.0))),
    }
}

fn clip_settings(s: &ClipSettings) -> Outcome {
    if s.name.trim().is_empty() {
        return Err(invalid("a clip needs a name"));
    }
    gain(s.gain)
}

pub fn apply(state: &mut AppState, cmd: Command) -> Outcome {
    // Only these add connections; check them for loops after the change, and undo it if so.
    let adds = matches!(
        cmd,
        Command::SelectOutput { selected: true, .. }
            | Command::SetRoute { gain: Some(_), .. }
            | Command::SetSystemInput(Some(_))
    );
    let before = adds.then(|| state.desired.clone());
    change(state, cmd)?;
    if let Some(before) = before
        && let Some(l) = loops::find(state)
    {
        let why = format!(
            "that would loop the sound back into itself: {}",
            loops::describe(state, &l)
        );
        state.desired = before;
        return Err(invalid(why));
    }
    Ok(())
}

fn change(state: &mut AppState, cmd: Command) -> Outcome {
    match cmd {
        Command::SelectOutput { device: key, selected } => {
            if selected {
                // Only something plugged in can be picked; an absent one can still be dropped.
                if !state.observed.devices.contains_key(&key) {
                    return Err(not_found(format!("device {} isn't connected", key.0)));
                }
                device(state, &key, Flow::Render)?;
                state.remember_device(&key);
                state.desired.outputs.insert(key);
            } else {
                state.desired.outputs.remove(&key);
            }
        }
        Command::SetSystemInput(None) => state.desired.system_input = None,
        Command::SetSystemInput(Some(SystemInput::Device(key))) => {
            device(state, &key, Flow::Capture)?;
            state.remember_device(&key);
            state.desired.system_input = Some(SystemInput::Device(key));
        }
        Command::SetSystemInput(Some(SystemInput::Endpoint(id))) => {
            if !state.observed.endpoints.contains_key(&id) {
                return Err(not_found(format!("no driver endpoint {id}")));
            }
            state.desired.system_input = Some(SystemInput::Endpoint(id));
        }
        Command::SetMasterGain(g) => {
            gain(g)?;
            state.desired.master.gain = g;
        }
        Command::SetMasterMuted(muted) => state.desired.master.muted = muted,
        Command::SetAppGain { app: key, gain: g } => {
            app(state, &key)?;
            gain(g)?;
            if g > 1.0 {
                return Err(invalid(format!(
                    "an app's level is its Windows volume, so it goes up to 1 (asked for {g})"
                )));
            }
            state.remember_app(&key);
            state.desired.app_levels.entry(key).or_default().gain = g;
        }
        Command::SetAppMuted { app: key, muted } => {
            app(state, &key)?;
            state.remember_app(&key);
            state.desired.app_levels.entry(key).or_default().muted = muted;
        }
        Command::SetRoute { from, to, gain: None } => {
            state.desired.matrix.remove(&(from, to));
        }
        Command::SetRoute {
            from,
            to,
            gain: Some(g),
        } => {
            gain(g)?;
            match &from {
                Source::Device(d) => device(state, d, Flow::Capture)?,
                Source::App(a) => app(state, a)?,
                Source::Clips => {}
            }
            let Sink::Device(out) = &to;
            device(state, out, Flow::Render)?;
            for d in [&from, &Source::Device(out.clone())] {
                match d {
                    Source::Device(d) => state.remember_device(d),
                    Source::App(a) => state.remember_app(a),
                    Source::Clips => {}
                }
            }
            state.desired.matrix.insert((from, to), g);
        }
        Command::AddClip { path, name } => {
            if path.trim().is_empty() {
                return Err(invalid("a clip needs a file"));
            }
            // The engine opens the file when a graph uses it; a bad file is reported then.
            let name = if name.trim().is_empty() {
                file_stem(&path).to_owned()
            } else {
                name
            };
            let settings = ClipSettings {
                name,
                gain: 1.0,
                retrigger: sb_shared::graph::Retrigger::Restart,
                looped: false,
            };
            clip_settings(&settings)?;
            let id = ClipId(state.desired.next_clip);
            state.desired.next_clip += 1;
            state.desired.clips.insert(id, ClipDef { path, settings });
        }
        Command::RemoveClip { id } => {
            if state.desired.clips.remove(&id).is_none() {
                return Err(not_found(format!("no clip {}", id.0)));
            }
        }
        Command::UpdateClip { id, settings } => {
            clip_settings(&settings)?;
            match state.desired.clips.get_mut(&id) {
                Some(c) => c.settings = settings,
                None => return Err(not_found(format!("no clip {}", id.0))),
            }
        }
        Command::DismissLoadError => state.observed.settings.load_error = None,
        Command::SetKeepWindowsDefault(keep) => state.desired.keep_windows_default = keep,
        Command::PlayClip { .. } | Command::StopClip { .. } | Command::RestartEngine | Command::RouteApp { .. } => {
            return Err(ErrorInfo::new(
                ErrorKind::Internal,
                "the actor runs engine and routing commands",
            ));
        }
    }
    state.desired.prune_known();
    Ok(())
}

/// `C:\clips\air horn.ogg` → `air horn` (either slash: paths come from Windows).
fn file_stem(path: &str) -> &str {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    match file.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => file,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DeviceInfo, Observation};
    use crate::view::project;
    use sb_shared::ui::Level;

    fn speakers() -> DeviceKey {
        DeviceKey("{0.0.0.00000000}.{speakers}".into())
    }
    fn headphones() -> DeviceKey {
        DeviceKey("{0.0.0.00000000}.{headphones}".into())
    }
    fn mic() -> DeviceKey {
        DeviceKey("{0.0.1.00000000}.{mic}".into())
    }
    fn discord() -> AppKey {
        AppKey(r"C:\Users\me\AppData\Local\Discord\Discord.exe".into())
    }

    fn world() -> AppState {
        let mut s = AppState::default();
        for (key, name, flow) in [
            (speakers(), "Speakers", Flow::Render),
            (headphones(), "Headphones", Flow::Render),
            (mic(), "Microphone", Flow::Capture),
        ] {
            s.observe(Observation::DeviceAdded {
                key,
                info: DeviceInfo {
                    name: name.into(),
                    flow,
                },
            });
        }
        s.observe(Observation::SessionAdded {
            app: discord(),
            name: "Discord".into(),
            pid: 100,
        });
        s
    }

    fn kind(r: Outcome) -> ErrorKind {
        r.unwrap_err().kind
    }

    #[test]
    fn selecting_outputs() {
        let mut s = world();
        apply(
            &mut s,
            Command::SelectOutput {
                device: speakers(),
                selected: true,
            },
        )
        .unwrap();
        let v = project(&s);
        assert!(v.devices[&speakers()].selected && !v.devices[&headphones()].selected);
        let pick_mic = Command::SelectOutput {
            device: mic(),
            selected: true,
        };
        assert_eq!(
            kind(apply(&mut s, pick_mic)),
            ErrorKind::Invalid,
            "an input isn't an output"
        );
        let unknown = Command::SelectOutput {
            device: DeviceKey("nope".into()),
            selected: true,
        };
        assert_eq!(kind(apply(&mut s, unknown)), ErrorKind::NotFound);
        assert_eq!(s.desired.outputs.len(), 1, "failed commands change nothing");
    }

    #[test]
    fn an_unplugged_output_stays_picked_greyed_out_and_comes_back() {
        let mut s = world();
        apply(
            &mut s,
            Command::SelectOutput {
                device: headphones(),
                selected: true,
            },
        )
        .unwrap();
        s.observe(Observation::DeviceRemoved { key: headphones() });
        s.observe(Observation::DeviceRemoved { key: speakers() });
        let v = project(&s);
        let h = &v.devices[&headphones()];
        assert!(h.selected && !h.present && h.name == "Headphones");
        assert!(!v.devices.contains_key(&speakers()), "nothing refers to the speakers");
        // Back, renamed by the user in Windows meanwhile.
        s.observe(Observation::DeviceAdded {
            key: headphones(),
            info: DeviceInfo {
                name: "My Headphones".into(),
                flow: Flow::Render,
            },
        });
        let h = &project(&s).devices[&headphones()];
        assert!(h.selected && h.present && h.name == "My Headphones");
        // Deselected while absent: forgotten.
        s.observe(Observation::DeviceRemoved { key: headphones() });
        apply(
            &mut s,
            Command::SelectOutput {
                device: headphones(),
                selected: false,
            },
        )
        .unwrap();
        assert!(!project(&s).devices.contains_key(&headphones()));
        assert!(s.desired.known_devices.is_empty());
    }

    #[test]
    fn app_levels_outlive_the_app() {
        let mut s = world();
        apply(
            &mut s,
            Command::SetAppGain {
                app: discord(),
                gain: 0.4,
            },
        )
        .unwrap();
        apply(
            &mut s,
            Command::SetAppMuted {
                app: discord(),
                muted: true,
            },
        )
        .unwrap();
        s.observe(Observation::SessionRemoved {
            app: discord(),
            pid: 100,
        });
        let a = &project(&s).apps[&discord()];
        assert_eq!((a.sessions, a.level), (0, Level { gain: 0.4, muted: true }));
        assert_eq!(a.name, "Discord");
        assert_eq!(
            kind(apply(
                &mut s,
                Command::SetAppGain {
                    app: discord(),
                    gain: 2.5
                }
            )),
            ErrorKind::Invalid
        );
        assert_eq!(
            kind(apply(
                &mut s,
                Command::SetAppGain {
                    app: AppKey("other.exe".into()),
                    gain: 0.5
                }
            )),
            ErrorKind::NotFound
        );
        // Two sessions: the app counts as running until both end.
        for pid in [7, 8] {
            s.observe(Observation::SessionAdded {
                app: discord(),
                name: "Discord".into(),
                pid,
            });
        }
        s.observe(Observation::SessionRemoved { app: discord(), pid: 7 });
        assert_eq!(project(&s).apps[&discord()].sessions, 1);
        assert_eq!(project(&s).apps[&discord()].level.gain, 0.4, "kept across relaunch");
    }

    #[test]
    fn matrix_routes_check_both_ends() {
        let mut s = world();
        let route = |from, to, gain| Command::SetRoute { from, to, gain };
        apply(
            &mut s,
            route(Source::Device(mic()), Sink::Device(headphones()), Some(0.5)),
        )
        .unwrap();
        apply(&mut s, route(Source::Clips, Sink::Device(speakers()), Some(1.0))).unwrap();
        apply(
            &mut s,
            route(Source::App(discord()), Sink::Device(speakers()), Some(1.0)),
        )
        .unwrap();
        assert_eq!(
            kind(apply(
                &mut s,
                route(Source::Device(speakers()), Sink::Device(headphones()), Some(1.0))
            )),
            ErrorKind::Invalid,
            "an output isn't a source"
        );
        assert_eq!(
            kind(apply(
                &mut s,
                route(Source::Device(mic()), Sink::Device(mic()), Some(1.0))
            )),
            ErrorKind::Invalid
        );
        assert_eq!(project(&s).routes.len(), 3);
        // The mic is unplugged: its route stays, and so does its name.
        s.observe(Observation::DeviceRemoved { key: mic() });
        let v = project(&s);
        assert!(!v.devices[&mic()].present);
        assert_eq!(v.routes[&(Source::Device(mic()), Sink::Device(headphones()))], 0.5);
        apply(&mut s, route(Source::Device(mic()), Sink::Device(headphones()), None)).unwrap();
        assert!(!project(&s).devices.contains_key(&mic()), "unrouted and absent: gone");
    }

    #[test]
    fn clips() {
        let mut s = world();
        apply(
            &mut s,
            Command::AddClip {
                path: r"C:\clips\air horn.ogg".into(),
                name: String::new(),
            },
        )
        .unwrap();
        apply(
            &mut s,
            Command::AddClip {
                path: "/home/me/boom".into(),
                name: "Boom".into(),
            },
        )
        .unwrap();
        let v = project(&s);
        assert_eq!(v.clips[&ClipId(0)].settings.name, "air horn");
        assert_eq!(v.clips[&ClipId(1)].settings.name, "Boom");
        let mut settings = v.clips[&ClipId(0)].settings.clone();
        settings.looped = true;
        settings.gain = 0.5;
        apply(
            &mut s,
            Command::UpdateClip {
                id: ClipId(0),
                settings: settings.clone(),
            },
        )
        .unwrap();
        assert_eq!(project(&s).clips[&ClipId(0)].settings, settings);
        apply(&mut s, Command::RemoveClip { id: ClipId(0) }).unwrap();
        assert_eq!(
            kind(apply(&mut s, Command::RemoveClip { id: ClipId(0) })),
            ErrorKind::NotFound
        );
        apply(
            &mut s,
            Command::AddClip {
                path: "x.wav".into(),
                name: String::new(),
            },
        )
        .unwrap();
        assert!(project(&s).clips.contains_key(&ClipId(2)), "ids aren't reused");
        assert_eq!(
            kind(apply(
                &mut s,
                Command::AddClip {
                    path: " ".into(),
                    name: "x".into()
                }
            )),
            ErrorKind::Invalid
        );
    }

    #[test]
    fn file_stems() {
        assert_eq!(file_stem(r"C:\a\b.c.ogg"), "b.c");
        assert_eq!(file_stem("/x/.hidden"), ".hidden");
        assert_eq!(file_stem("plain"), "plain");
    }
}
