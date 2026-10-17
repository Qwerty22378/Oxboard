//! What Windows shows the controller, as plain data, and the [`Observation`]s a change makes.
//!
//! The COM thread (`win::com`) doesn't track changes one notification at a time: any
//! notification just makes it rescan, and it reports the difference between the last scan
//! and this one ([`changes`]). The same code handles a missed notification, a burst of them,
//! and the first scan at startup. Everything here is pure, so it's tested on any platform.
//!
//! **App levels** (Interface 1, option A) are Windows session volumes. The COM thread sets
//! them when the user moves a fader here, and Windows reports every volume change back,
//! ours included. [`Levels`] remembers what we set, so only changes made elsewhere (the
//! Windows mixer, the app itself) are reported, and the actor follows those. Changes are
//! judged per session: an app's sessions can disagree (one on each device, or one left over
//! from a process that just exited).

use crate::state::{DeviceInfo, Loopback, Observation};
use sb_shared::ui::{AppKey, DeviceKey, Flow, Level};
use std::collections::{BTreeMap, BTreeSet};

/// Work for the COM thread, and its wake-ups.
#[derive(Debug, Clone, PartialEq)]
pub enum WinMsg {
    /// Set every session of the app to this level, now and whenever it opens a new one.
    SetAppLevel {
        app: AppKey,
        level: Level,
    },
    /// Send the app's processes to `device` from now on (their per-app default output), or
    /// back to the Windows default (`None`). Answered with `Msg::WinDone { op }`; `op` 0 wants
    /// no answer.
    SetAppOutput {
        op: u64,
        app: AppKey,
        pids: Vec<u32>,
        device: Option<DeviceKey>,
    },
    /// Something changed (from a notification callback): scan again.
    Rescan {
        devices: bool,
        sessions: bool,
    },
    Stop,
}

/// An active audio endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenDevice {
    pub name: String,
    pub flow: Flow,
    /// The adapter's name (`PKEY_DeviceInterface_FriendlyName`), e.g. "VB-Audio Virtual Cable".
    pub adapter: String,
}

/// One audio session on a render device.
#[derive(Debug, Clone, PartialEq)]
pub struct SeenSession {
    pub app: AppKey,
    /// The render device it plays on.
    pub device: DeviceKey,
    /// Display name for the app.
    pub name: String,
    pub pid: u32,
    pub level: Level,
}

/// One scan of everything the controller watches.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct World {
    pub devices: BTreeMap<DeviceKey, SeenDevice>,
    /// By session instance identifier (unique per session, across devices).
    pub sessions: BTreeMap<String, SeenSession>,
}

/// The levels the controller set, per app, so its own changes aren't reported back.
#[derive(Debug, Clone, Default)]
pub struct Levels {
    applied: BTreeMap<AppKey, Level>,
}

impl Levels {
    /// The controller sets the app to `level` (the caller applies it to every session).
    pub fn set(&mut self, app: AppKey, level: Level) {
        self.applied.insert(app, level);
    }

    /// The level a new session of the app should get, if the controller manages it.
    pub fn wanted(&self, app: &AppKey) -> Option<Level> {
        self.applied.get(app).copied()
    }
}

/// Session volumes are f32 in 0..1 and come back slightly off what was set.
fn same(a: Level, b: Level) -> bool {
    a.muted == b.muted && (a.gain - b.gain).abs() < 1e-4
}

/// VB-Audio's cables: what's played into the render end ("CABLE Input") comes out of the
/// capture end ("CABLE Output"). Both endpoints sit on one adapter named for the cable, so an
/// adapter with "cable" in its name and exactly one endpoint each way is a cable. (A USB
/// headset also has one each way, which is why the name matters.)
pub fn cable_pairs(devices: &BTreeMap<DeviceKey, SeenDevice>) -> BTreeMap<DeviceKey, DeviceKey> {
    let mut by_adapter: BTreeMap<&str, (Vec<&DeviceKey>, Vec<&DeviceKey>)> = BTreeMap::new();
    for (key, d) in devices {
        if d.adapter.to_lowercase().contains("cable") {
            let e = by_adapter.entry(&d.adapter).or_default();
            match d.flow {
                Flow::Render => e.0.push(key),
                Flow::Capture => e.1.push(key),
            }
        }
    }
    by_adapter
        .into_values()
        .filter_map(|(r, c)| match (r.as_slice(), c.as_slice()) {
            ([r], [c]) => Some(((*r).clone(), (*c).clone())),
            _ => None,
        })
        .collect()
}

/// What changed between two scans. Changes to app levels the controller made itself are
/// left out; any other change to a managed app's level becomes the level it wants from now on.
pub fn changes(old: &World, new: &World, levels: &mut Levels) -> Vec<Observation> {
    let mut out = Vec::new();
    for key in old.devices.keys().filter(|k| !new.devices.contains_key(*k)) {
        out.push(Observation::DeviceRemoved { key: key.clone() });
    }
    for (key, d) in &new.devices {
        if old
            .devices
            .get(key)
            .is_none_or(|o| o.name != d.name || o.flow != d.flow)
        {
            out.push(Observation::DeviceAdded {
                key: key.clone(),
                info: DeviceInfo {
                    name: d.name.clone(),
                    flow: d.flow,
                },
            });
        }
    }
    let (old_pairs, new_pairs) = (cable_pairs(&old.devices), cable_pairs(&new.devices));
    for render in old_pairs.keys().filter(|r| !new_pairs.contains_key(*r)) {
        out.push(Observation::Loopback {
            render: render.clone(),
            into: None,
        });
    }
    for (render, capture) in &new_pairs {
        if old_pairs.get(render) != Some(capture) {
            out.push(Observation::Loopback {
                render: render.clone(),
                into: Some(Loopback::Capture(capture.clone())),
            });
        }
    }

    // Sessions, as (app, pid): an app playing on two devices is one process, once.
    let procs = |w: &World| -> BTreeMap<(AppKey, u32), String> {
        w.sessions
            .values()
            .map(|s| ((s.app.clone(), s.pid), s.name.clone()))
            .collect()
    };
    let (old_procs, new_procs) = (procs(old), procs(new));
    for (app, pid) in old_procs.keys().filter(|k| !new_procs.contains_key(*k)) {
        out.push(Observation::SessionRemoved {
            app: app.clone(),
            pid: *pid,
        });
    }
    for ((app, pid), name) in &new_procs {
        if old_procs.get(&(app.clone(), *pid)) != Some(name) {
            out.push(Observation::SessionAdded {
                app: app.clone(),
                name: name.clone(),
                pid: *pid,
            });
        }
    }

    // Where each app plays.
    let devices = |w: &World| -> BTreeMap<AppKey, BTreeSet<DeviceKey>> {
        let mut m: BTreeMap<AppKey, BTreeSet<DeviceKey>> = BTreeMap::new();
        for s in w.sessions.values() {
            m.entry(s.app.clone()).or_default().insert(s.device.clone());
        }
        m
    };
    let old_devices = devices(old);
    for (app, d) in devices(new) {
        if old_devices.get(&app) != Some(&d) {
            out.push(Observation::AppDevices { app, devices: d });
        }
    }

    // Levels, per session: a session whose volume moved to something the controller didn't
    // set was changed outside (the Windows mixer, the app). A new app reports the level it
    // starts with. One report per app.
    let old_apps: BTreeSet<&AppKey> = old.sessions.values().map(|s| &s.app).collect();
    let mut reported = BTreeSet::new();
    for (id, s) in &new.sessions {
        let moved = match old.sessions.get(id) {
            Some(o) => !same(o.level, s.level),
            None => !old_apps.contains(&s.app),
        };
        let ours = levels.wanted(&s.app).is_some_and(|w| same(w, s.level));
        if moved && !ours && reported.insert(s.app.clone()) {
            if levels.wanted(&s.app).is_some() {
                levels.set(s.app.clone(), s.level);
            }
            out.push(Observation::AppLevel {
                app: s.app.clone(),
                level: s.level,
            });
        }
    }
    out
}

/// An app's key and name from its executable path: `C:\Program Files\Discord\Discord.exe` →
/// "Discord" (when the session and the file have no better name).
pub fn exe_name(path: &str) -> &str {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    match file.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.eq_ignore_ascii_case("exe") => stem,
        _ => file,
    }
}

/// `KSCATEGORY_AUDIO` render device interface class, as Windows' policy paths end.
const RENDER_INTERFACE: &str = "{e6327cad-dcec-4949-ae8a-991e976a79d2}";

/// How the per-app output API names a render endpoint: its device interface path,
/// `\\?\SWD#MMDEVAPI#{0.0.0.00000000}.{guid}#{e6327cad-…}`.
pub fn policy_path(device: &DeviceKey) -> String {
    format!(r"\\?\SWD#MMDEVAPI#{}#{RENDER_INTERFACE}", device.0)
}

/// The endpoint in such a path (`None` for an empty one: no per-app output).
pub fn device_of_policy_path(path: &str) -> Option<DeviceKey> {
    let rest = path.split("MMDEVAPI#").nth(1)?;
    let id = rest.split('#').next()?;
    (!id.is_empty()).then(|| DeviceKey(id.to_owned()))
}

/// The soundboard's own processes play too (the engine host renders to every output), but
/// they aren't apps to route or level.
pub fn is_ours(path: &str) -> bool {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    ["sb-engine-host.exe", "sb-controller.exe"]
        .iter()
        .any(|ours| file.eq_ignore_ascii_case(ours))
}

/// The executable in a session identifier, for processes the controller can't open
/// (elevated ones): `{0.0.0.00000000}.{guid}|\Device\HarddiskVolume3\Windows\x.exe%b{guid}`.
pub fn session_exe(identifier: &str) -> Option<&str> {
    let (_, rest) = identifier.split_once('|')?;
    let path = rest.split("%b").next()?;
    (!path.is_empty() && path.contains('\\')).then_some(path)
}

/// A session's display name is often empty, or a resource reference (`@%SystemRoot%\...`).
pub fn usable_display_name(name: &str) -> Option<&str> {
    let n = name.trim();
    (!n.is_empty() && !n.starts_with('@')).then_some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, flow: Flow, adapter: &str) -> (DeviceKey, SeenDevice) {
        (
            DeviceKey(format!("{{{name}}}")),
            SeenDevice {
                name: format!("{name} ({adapter})"),
                flow,
                adapter: adapter.into(),
            },
        )
    }

    fn session(id: &str, app: &str, pid: u32, gain: f32) -> (String, SeenSession) {
        (
            id.into(),
            SeenSession {
                app: AppKey(format!(r"C:\{app}.exe")),
                device: DeviceKey("{spk}".into()),
                name: app.into(),
                pid,
                level: Level { gain, muted: false },
            },
        )
    }

    fn key(app: &str) -> AppKey {
        AppKey(format!(r"C:\{app}.exe"))
    }

    fn lvl(gain: f32) -> Level {
        Level { gain, muted: false }
    }

    #[test]
    fn devices_come_go_and_get_renamed() {
        let mut levels = Levels::default();
        let a = World {
            devices: [
                dev("spk", Flow::Render, "Realtek"),
                dev("mic", Flow::Capture, "Realtek"),
            ]
            .into(),
            ..Default::default()
        };
        let first = changes(&World::default(), &a, &mut levels);
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|o| matches!(o, Observation::DeviceAdded { .. })));
        assert!(changes(&a, &a, &mut levels).is_empty());
        let mut b = a.clone();
        b.devices.remove(&DeviceKey("{mic}".into()));
        b.devices.get_mut(&DeviceKey("{spk}".into())).unwrap().name = "Desk speakers".into();
        let ch = changes(&a, &b, &mut levels);
        assert_eq!(
            ch[0],
            Observation::DeviceRemoved {
                key: DeviceKey("{mic}".into())
            }
        );
        assert!(matches!(&ch[1], Observation::DeviceAdded { info, .. } if info.name == "Desk speakers"));
    }

    #[test]
    fn vb_cables_loop_back_and_headsets_dont() {
        let devices: BTreeMap<_, _> = [
            dev("cable-in", Flow::Render, "VB-Audio Virtual Cable"),
            dev("cable-out", Flow::Capture, "VB-Audio Virtual Cable"),
            dev("a-in", Flow::Render, "VB-Audio Virtual Cable A"),
            dev("a-out", Flow::Capture, "VB-Audio Virtual Cable A"),
            dev("headset-spk", Flow::Render, "USB Headset"),
            dev("headset-mic", Flow::Capture, "USB Headset"),
        ]
        .into();
        let pairs = cable_pairs(&devices);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[&DeviceKey("{cable-in}".into())], DeviceKey("{cable-out}".into()));
        assert_eq!(pairs[&DeviceKey("{a-in}".into())], DeviceKey("{a-out}".into()));

        let mut levels = Levels::default();
        let w = World {
            devices,
            ..Default::default()
        };
        let ch = changes(&World::default(), &w, &mut levels);
        let loops = ch.iter().filter(|o| matches!(o, Observation::Loopback { .. })).count();
        assert_eq!(loops, 2);
        // Unplugging one cable's output ends its loop.
        let mut w2 = w.clone();
        w2.devices.remove(&DeviceKey("{a-out}".into()));
        let ch = changes(&w, &w2, &mut levels);
        assert!(ch.contains(&Observation::Loopback {
            render: DeviceKey("{a-in}".into()),
            into: None
        }));
    }

    #[test]
    fn sessions_are_reported_per_process_not_per_device() {
        let mut levels = Levels::default();
        let a = World {
            sessions: [
                session("s1", "discord", 10, 1.0),
                session("s2", "discord", 10, 1.0), // the same process on a second device
                session("s3", "game", 20, 0.5),
            ]
            .into(),
            ..Default::default()
        };
        let mut a = a;
        a.sessions.get_mut("s2").unwrap().device = DeviceKey("{headphones}".into());
        let ch = changes(&World::default(), &a, &mut levels);
        let added: Vec<_> = ch
            .iter()
            .filter(|o| matches!(o, Observation::SessionAdded { .. }))
            .collect();
        assert_eq!(added.len(), 2);
        // Then where each app plays, then each app's level once.
        let speakers = DeviceKey("{spk}".into());
        let headphones = DeviceKey("{headphones}".into());
        assert_eq!(
            ch[2],
            Observation::AppDevices {
                app: key("discord"),
                devices: [headphones.clone(), speakers.clone()].into()
            }
        );
        assert_eq!(ch.len(), 6);
        assert_eq!(
            ch[5],
            Observation::AppLevel {
                app: key("game"),
                level: lvl(0.5)
            }
        );
        let mut b = a.clone();
        b.sessions.remove("s2");
        assert_eq!(
            changes(&a, &b, &mut levels),
            [Observation::AppDevices {
                app: key("discord"),
                devices: [speakers].into()
            }],
            "discord still has a session, on one device now"
        );
        b.sessions.remove("s1");
        assert_eq!(
            changes(&a, &b, &mut levels),
            [Observation::SessionRemoved {
                app: key("discord"),
                pid: 10
            }]
        );
    }

    #[test]
    fn the_controllers_own_level_changes_are_not_reported_back() {
        let mut levels = Levels::default();
        let a = World {
            sessions: [session("s1", "game", 20, 1.0), session("s2", "game", 20, 1.0)].into(),
            ..Default::default()
        };
        changes(&World::default(), &a, &mut levels);
        // The controller sets 0.3: Windows reports it (slightly off), and that's not news.
        levels.set(key("game"), lvl(0.3));
        let mut b = a.clone();
        for s in b.sessions.values_mut() {
            s.level = lvl(0.300_001);
        }
        assert!(changes(&a, &b, &mut levels).is_empty());
        // The user drags one of the sessions in the Windows mixer: that's reported, and the
        // controller now wants that level.
        let mut c = b.clone();
        c.sessions.get_mut("s2").unwrap().level = lvl(0.8);
        assert_eq!(
            changes(&b, &c, &mut levels),
            [Observation::AppLevel {
                app: key("game"),
                level: lvl(0.8)
            }]
        );
        assert_eq!(levels.wanted(&key("game")), Some(lvl(0.8)));
        // A mute from Windows too.
        let mut d = c.clone();
        for s in d.sessions.values_mut() {
            s.level = Level { gain: 0.8, muted: true };
        }
        assert_eq!(changes(&c, &d, &mut levels).len(), 1);
    }

    #[test]
    fn a_change_on_one_session_counts_even_when_the_apps_sessions_disagree() {
        // Found on Windows: a session left over from an exited process (same app) keeps its
        // own volume. Judging the app by one session hid the change on the live one.
        let mut levels = Levels::default();
        let a = World {
            sessions: [session("s1-old", "game", 20, 1.0), session("s2-live", "game", 21, 1.0)].into(),
            ..Default::default()
        };
        changes(&World::default(), &a, &mut levels);
        levels.set(key("game"), lvl(0.3));
        let mut b = a.clone();
        b.sessions.get_mut("s2-live").unwrap().level = lvl(0.3); // the old one didn't take it
        assert!(changes(&a, &b, &mut levels).is_empty());
        let mut c = b.clone();
        c.sessions.get_mut("s2-live").unwrap().level = lvl(0.7);
        assert_eq!(
            changes(&b, &c, &mut levels),
            [Observation::AppLevel {
                app: key("game"),
                level: lvl(0.7)
            }]
        );
    }

    #[test]
    fn names_from_paths_and_session_identifiers() {
        assert_eq!(exe_name(r"C:\Program Files\Discord\Discord.exe"), "Discord");
        assert_eq!(exe_name(r"C:\Games\game.EXE"), "game");
        assert_eq!(exe_name("weird"), "weird");
        let id = r"{0.0.0.00000000}.{5e1c}|\Device\HarddiskVolume3\Windows\System32\svchost.exe%b{00000000-0000-0000-0000-000000000000}";
        assert_eq!(
            session_exe(id),
            Some(r"\Device\HarddiskVolume3\Windows\System32\svchost.exe")
        );
        assert_eq!(session_exe("{0.0.0.00000000}.{5e1c}|#%b{guid}"), None);
        assert_eq!(usable_display_name("  "), None);
        assert_eq!(usable_display_name("@%SystemRoot%\\System32\\AudioSrv.Dll,-202"), None);
        assert_eq!(usable_display_name("Spotify"), Some("Spotify"));
        assert!(is_ours(r"C:\Program Files\Soundboard\SB-Engine-Host.exe"));
        let spk = DeviceKey("{0.0.0.00000000}.{55928b0d-9863-4020-9c6d-18d443fe3af6}".into());
        let path = policy_path(&spk);
        assert_eq!(
            path,
            r"\\?\SWD#MMDEVAPI#{0.0.0.00000000}.{55928b0d-9863-4020-9c6d-18d443fe3af6}#{e6327cad-dcec-4949-ae8a-991e976a79d2}"
        );
        assert_eq!(device_of_policy_path(&path), Some(spk));
        assert_eq!(device_of_policy_path(""), None);
        assert!(!is_ours(r"C:\Games\sb-engine-host-clone.exe"));
    }
}
