//! `AppState`: everything the controller knows, owned by the actor alone.
//!
//! - [`Desired`] is what the user asked for. It's the part that gets persisted (C6), and it
//!   survives devices and apps coming and going: an unplugged output stays selected and plays
//!   again when it's back.
//! - [`Observed`] is what's true right now, rebuilt at runtime from Windows (C4), the driver
//!   (C5) and the engine (C3). Never persisted.
//!
//! Keys are the stable ones from `sb_shared::ui` (endpoint IDs, executable paths), never
//! names, indices or PIDs.

use sb_shared::ui::{AppKey, ClipId, ClipSettings, DeviceKey, EngineStatus, Flow, Level, Sink, Source, SystemInput};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub desired: Desired,
    pub observed: Observed,
    /// Version of the last graph compiled from this state; bumped when the graph's shape
    /// changes (a param change keeps it).
    pub graph_version: u64,
}

/// What the user wants.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Desired {
    /// Where the PC's sound enters the mix.
    pub system_input: Option<SystemInput>,
    /// Interface 1: every output the system input plays to.
    pub outputs: BTreeSet<DeviceKey>,
    pub master: Level,
    /// Per-app levels; apps without an entry play at unity.
    pub app_levels: BTreeMap<AppKey, Level>,
    /// Interface 2: each app's outputs. Only apps with an endpoint (`app_endpoints`) play.
    pub app_routes: BTreeMap<AppKey, BTreeSet<DeviceKey>>,
    /// The driver endpoint each app is routed into (Windows sends the app there; C5).
    pub app_endpoints: BTreeMap<AppKey, u32>,
    /// Interface 3: the routing matrix, gain per connection.
    pub matrix: BTreeMap<(Source, Sink), f32>,
    pub clips: BTreeMap<ClipId, ClipDef>,
    /// The next clip id handed out (ids are never reused).
    pub next_clip: u32,
    /// Devices referred to above, as last seen, so an absent one still has a name and a flow.
    pub known_devices: BTreeMap<DeviceKey, DeviceInfo>,
    /// Apps referred to above, by last seen name.
    pub known_apps: BTreeMap<AppKey, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClipDef {
    pub path: String,
    pub settings: ClipSettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub flow: Flow,
}

/// What's true right now.
#[derive(Debug, Clone, PartialEq)]
pub struct Observed {
    /// Devices plugged in and enabled.
    pub devices: BTreeMap<DeviceKey, DeviceInfo>,
    /// Running apps with audio sessions, by app; an app can have several.
    pub sessions: BTreeMap<AppKey, AppSessions>,
    pub engine: EngineStatus,
    /// Clips with voices playing (from the engine's `ClipEnded` and our triggers, C3).
    pub playing: BTreeSet<ClipId>,
    /// The driver's virtual render endpoints, by id (C5).
    pub endpoints: BTreeMap<u32, PoolEndpoint>,
    /// Render devices whose sound comes back in somewhere: a cable's input reappears on its
    /// output, a driver endpoint in the mix. Playing into one from its own source loops.
    pub loopbacks: BTreeMap<DeviceKey, Loopback>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolEndpoint {
    /// Enabled in the driver (a disabled one doesn't exist for Windows or the engine).
    pub enabled: bool,
}

/// Where what's played into a render device comes back in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loopback {
    /// On a capture device (VB-Cable: "CABLE Input" → "CABLE Output").
    Capture(DeviceKey),
    /// On one of the driver's endpoints, as the engine reads it.
    Endpoint(u32),
}

impl Default for Observed {
    fn default() -> Self {
        Self {
            devices: BTreeMap::new(),
            sessions: BTreeMap::new(),
            engine: EngineStatus::Down,
            playing: BTreeSet::new(),
            endpoints: BTreeMap::new(),
            loopbacks: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppSessions {
    /// Display name, from the session (or the executable's file name).
    pub name: String,
    /// One entry per open session, by its owner's PID.
    pub pids: BTreeSet<u32>,
}

/// A change in the world, posted by whoever watches it (the COM thread, C4; tests).
#[derive(Debug, Clone, PartialEq)]
pub enum Observation {
    DeviceAdded {
        key: DeviceKey,
        info: DeviceInfo,
    },
    DeviceRemoved {
        key: DeviceKey,
    },
    SessionAdded {
        app: AppKey,
        name: String,
        pid: u32,
    },
    SessionRemoved {
        app: AppKey,
        pid: u32,
    },
    /// A driver endpoint appeared, changed or (`None`) went away (C5).
    Endpoint {
        id: u32,
        state: Option<PoolEndpoint>,
    },
    /// A render device loops back (`None`: not any more). From device properties or the
    /// driver (C4/C5).
    Loopback {
        render: DeviceKey,
        into: Option<Loopback>,
    },
}

impl Desired {
    /// Whether anything still refers to the device.
    fn uses_device(&self, key: &DeviceKey) -> bool {
        self.outputs.contains(key)
            || self.system_input == Some(SystemInput::Device(key.clone()))
            || self.app_routes.values().any(|outs| outs.contains(key))
            || self.matrix.keys().any(|(from, to)| {
                matches!(from, Source::Device(d) if d == key) || matches!(to, Sink::Device(d) if d == key)
            })
    }

    fn uses_app(&self, key: &AppKey) -> bool {
        self.app_levels.contains_key(key)
            || self.app_routes.contains_key(key)
            || self.app_endpoints.contains_key(key)
            || self
                .matrix
                .keys()
                .any(|(from, _)| matches!(from, Source::App(a) if a == key))
    }

    /// Forget devices and apps nothing refers to any more.
    pub fn prune_known(&mut self) {
        let devices = std::mem::take(&mut self.known_devices);
        self.known_devices = devices.into_iter().filter(|(k, _)| self.uses_device(k)).collect();
        let apps = std::mem::take(&mut self.known_apps);
        self.known_apps = apps.into_iter().filter(|(k, _)| self.uses_app(k)).collect();
    }
}

impl AppState {
    /// A device as known now: observed if present, else as last seen.
    pub fn device(&self, key: &DeviceKey) -> Option<&DeviceInfo> {
        self.observed
            .devices
            .get(key)
            .or_else(|| self.desired.known_devices.get(key))
    }

    /// An app's display name, if the controller has ever seen it or kept settings for it.
    pub fn app_name(&self, key: &AppKey) -> Option<&str> {
        self.observed
            .sessions
            .get(key)
            .map(|s| s.name.as_str())
            .or_else(|| self.desired.known_apps.get(key).map(String::as_str))
    }

    /// Remember the device (and its current name) because desired state now refers to it.
    pub fn remember_device(&mut self, key: &DeviceKey) {
        if let Some(info) = self.observed.devices.get(key) {
            self.desired.known_devices.insert(key.clone(), info.clone());
        }
    }

    pub fn remember_app(&mut self, key: &AppKey) {
        if let Some(s) = self.observed.sessions.get(key) {
            self.desired.known_apps.insert(key.clone(), s.name.clone());
        }
    }

    pub fn observe(&mut self, o: Observation) {
        match o {
            Observation::DeviceAdded { key, info } => {
                // A remembered device that came back may have been renamed.
                if let Some(known) = self.desired.known_devices.get_mut(&key) {
                    *known = info.clone();
                }
                self.observed.devices.insert(key, info);
            }
            Observation::DeviceRemoved { key } => {
                self.observed.devices.remove(&key);
            }
            Observation::SessionAdded { app, name, pid } => {
                if let Some(known) = self.desired.known_apps.get_mut(&app) {
                    known.clone_from(&name);
                }
                let s = self.observed.sessions.entry(app).or_default();
                s.name = name;
                s.pids.insert(pid);
            }
            Observation::SessionRemoved { app, pid } => {
                if let Some(s) = self.observed.sessions.get_mut(&app) {
                    s.pids.remove(&pid);
                    if s.pids.is_empty() {
                        self.observed.sessions.remove(&app);
                    }
                }
            }
            Observation::Endpoint { id, state } => {
                match state {
                    Some(e) => self.observed.endpoints.insert(id, e),
                    None => self.observed.endpoints.remove(&id),
                };
            }
            Observation::Loopback { render, into } => {
                match into {
                    Some(l) => self.observed.loopbacks.insert(render, l),
                    None => self.observed.loopbacks.remove(&render),
                };
            }
        }
    }
}
