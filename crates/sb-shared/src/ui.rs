//! The UI protocol: UI ⇄ controller.
//!
//! Same framing as the engine pipe ([`crate::protocol::write_frame`] / `read_frame`).
//!
//! 1. The UI sends [`UiHello`]; the controller answers [`UiMessage::Welcome`] or
//!    [`UiMessage::Refused`].
//! 2. [`UiRequest::Subscribe`] asks for state: a [`UiMessage::Snapshot`] right away, then a
//!    [`UiMessage::Diff`] after every change, numbered `seq + 1`, `seq + 2`, ... A UI that
//!    sees a gap subscribes again and gets a fresh snapshot ([`Mirror`] does this check).
//!    Subscribed UIs also get [`UiMessage::Meters`].
//! 3. [`UiRequest::Command`] carries a [`RequestId`] chosen by the UI. Every command gets
//!    [`UiMessage::Accepted`] at once and later exactly one of [`UiMessage::Completed`] or
//!    [`UiMessage::Failed`]. The diff with the command's effect is sent *before* `Completed`,
//!    so a UI that sees `Completed` already shows the new state.
//!
//! The view is a UI-shaped projection of the controller's state, not the state itself: the
//! controller stays the only owner, and the UI only mirrors what it's shown.

use crate::events::MeterLevels;
use crate::graph::Retrigger;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Bumped on any incompatible change to these types.
pub const UI_PROTOCOL_VERSION: u32 = 1;
/// Highest gain anything accepts: +6 dB, the top of the fader taper.
pub const MAX_GAIN: f32 = 2.0;

/// Chosen by the UI, echoed in the replies; unique per connection.
pub type RequestId = u64;

/// A device, by its Windows endpoint ID (`{0.0.0.00000000}.{guid}`): stable across reboots
/// and renames, unlike friendly names or indices.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DeviceKey(pub String);

/// An app, by executable path (or package family name for Store apps), never by PID.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AppKey(pub String);

/// A soundboard clip, assigned by the controller when the clip is added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ClipId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Flow {
    /// An output (speakers, headphones).
    Render,
    /// An input (microphones, line in).
    Capture,
}

/// A row of the routing matrix.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Source {
    /// A capture device.
    Device(DeviceKey),
    App(AppKey),
    /// Every soundboard clip, as one source.
    Clips,
}

/// A column of the routing matrix.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Sink {
    /// A render device.
    Device(DeviceKey),
}

/// A volume: linear gain (0 to [`MAX_GAIN`]) and a mute that keeps the gain.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Level {
    pub gain: f32,
    pub muted: bool,
}

impl Level {
    pub const UNITY: Level = Level {
        gain: 1.0,
        muted: false,
    };

    /// What actually reaches the audio.
    pub fn effective(self) -> f32 {
        if self.muted { 0.0 } else { self.gain }
    }
}

impl Default for Level {
    fn default() -> Self {
        Self::UNITY
    }
}

/// Whether `gain` is something a command may set.
pub fn valid_gain(gain: f32) -> bool {
    gain.is_finite() && (0.0..=MAX_GAIN).contains(&gain)
}

/// Where the PC's own sound enters the mix (what apps play to the Windows default output).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SystemInput {
    /// A capture device that carries it: the VB-Cable stand-in's "CABLE Output" until our
    /// driver exists (M3).
    Device(DeviceKey),
    /// One of the driver's virtual render endpoints, by id.
    Endpoint(u32),
}

/// What a meter reading belongs to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MeterTarget {
    /// An input's level, or what an output is playing.
    Device(DeviceKey),
    /// What an app with its own endpoint plays (Interfaces 2 and 3).
    App(AppKey),
    /// The PC's sound, as it enters the mix.
    System,
    /// Every clip together.
    Clips,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EngineStatus {
    /// Not started (or deliberately stopped).
    Down,
    /// Spawned, waiting for the first graph to run.
    Starting,
    /// Running the graph with this version.
    Running { applied_version: u64 },
    /// Crashed or hung; being restarted (`attempt` since it last ran fine).
    Restarting { attempt: u32 },
    /// Gave up restarting; the UI offers a retry.
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceView {
    pub name: String,
    pub flow: Flow,
    /// Plugged in right now. Absent devices the user picked stay listed, greyed out, and come
    /// back by themselves.
    pub present: bool,
    /// One of the outputs everything plays to (Interface 1). Render devices only.
    pub selected: bool,
    /// Selecting it would feed the PC's sound back into itself (it loops back into the
    /// system input). The UI greys the toggle out.
    pub loops: bool,
    /// Windows lists it but the engine couldn't keep it open; it's out of the mix and being
    /// retried.
    pub failed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppView {
    pub name: String,
    pub level: Level,
    /// Audio sessions the app has open right now; 0: not running (its settings are kept).
    pub sessions: u32,
}

/// What the user sets on a clip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipSettings {
    pub name: String,
    pub gain: f32,
    pub retrigger: Retrigger,
    pub looped: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipView {
    pub path: String,
    pub settings: ClipSettings,
    pub playing: bool,
}

/// Everything a UI shows. Maps are keyed and ordered, so a diff is per entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateView {
    pub devices: BTreeMap<DeviceKey, DeviceView>,
    pub apps: BTreeMap<AppKey, AppView>,
    pub clips: BTreeMap<ClipId, ClipView>,
    /// The routing matrix: a connection's gain, by (source, sink). No entry: not connected.
    pub routes: BTreeMap<(Source, Sink), f32>,
    /// Matrix cells that would close a feedback loop if connected; the UI greys them out.
    pub would_loop: BTreeSet<(Source, Sink)>,
    pub system_input: Option<SystemInput>,
    pub master: Level,
    pub engine: EngineStatus,
}

impl Default for StateView {
    fn default() -> Self {
        Self {
            devices: BTreeMap::new(),
            apps: BTreeMap::new(),
            clips: BTreeMap::new(),
            routes: BTreeMap::new(),
            would_loop: BTreeSet::new(),
            system_input: None,
            master: Level::UNITY,
            engine: EngineStatus::Down,
        }
    }
}

/// One change to a [`StateView`]. `None` removes the entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DiffOp {
    Device(DeviceKey, Option<DeviceView>),
    App(AppKey, Option<AppView>),
    Clip(ClipId, Option<ClipView>),
    Route(Source, Sink, Option<f32>),
    WouldLoop(Source, Sink, bool),
    SystemInput(Option<SystemInput>),
    Master(Level),
    Engine(EngineStatus),
}

/// The ops that turn `old` into `new` (empty when they're equal).
pub fn diff(old: &StateView, new: &StateView) -> Vec<DiffOp> {
    let mut ops = Vec::new();
    diff_map(&old.devices, &new.devices, |k, v| ops.push(DiffOp::Device(k, v)));
    diff_map(&old.apps, &new.apps, |k, v| ops.push(DiffOp::App(k, v)));
    diff_map(&old.clips, &new.clips, |k, v| ops.push(DiffOp::Clip(k, v)));
    diff_map(&old.routes, &new.routes, |(from, to), v| {
        ops.push(DiffOp::Route(from, to, v))
    });
    for (from, to) in new.would_loop.symmetric_difference(&old.would_loop) {
        let now = new.would_loop.contains(&(from.clone(), to.clone()));
        ops.push(DiffOp::WouldLoop(from.clone(), to.clone(), now));
    }
    if old.system_input != new.system_input {
        ops.push(DiffOp::SystemInput(new.system_input.clone()));
    }
    if old.master != new.master {
        ops.push(DiffOp::Master(new.master));
    }
    if old.engine != new.engine {
        ops.push(DiffOp::Engine(new.engine.clone()));
    }
    ops
}

fn diff_map<K: Ord + Clone, V: PartialEq + Clone>(
    old: &BTreeMap<K, V>,
    new: &BTreeMap<K, V>,
    mut op: impl FnMut(K, Option<V>),
) {
    for (k, v) in new {
        if old.get(k) != Some(v) {
            op(k.clone(), Some(v.clone()));
        }
    }
    for k in old.keys() {
        if !new.contains_key(k) {
            op(k.clone(), None);
        }
    }
}

fn set<K: Ord, V>(map: &mut BTreeMap<K, V>, k: K, v: Option<V>) {
    match v {
        Some(v) => map.insert(k, v),
        None => map.remove(&k),
    };
}

impl StateView {
    pub fn apply(&mut self, ops: impl IntoIterator<Item = DiffOp>) {
        for op in ops {
            match op {
                DiffOp::Device(k, v) => set(&mut self.devices, k, v),
                DiffOp::App(k, v) => set(&mut self.apps, k, v),
                DiffOp::Clip(k, v) => set(&mut self.clips, k, v),
                DiffOp::Route(from, to, v) => set(&mut self.routes, (from, to), v),
                DiffOp::WouldLoop(from, to, true) => {
                    self.would_loop.insert((from, to));
                }
                DiffOp::WouldLoop(from, to, false) => {
                    self.would_loop.remove(&(from, to));
                }
                DiffOp::SystemInput(i) => self.system_input = i,
                DiffOp::Master(l) => self.master = l,
                DiffOp::Engine(e) => self.engine = e,
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    /// Interface 1: play everything to this output too (or stop).
    SelectOutput {
        device: DeviceKey,
        selected: bool,
    },
    /// Where the PC's sound comes from (`None`: nowhere; Interface 1 plays nothing).
    SetSystemInput(Option<SystemInput>),
    SetMasterGain(f32),
    SetMasterMuted(bool),
    SetAppGain {
        app: AppKey,
        gain: f32,
    },
    SetAppMuted {
        app: AppKey,
        muted: bool,
    },
    /// Interface 3: connect `from` to `to` with `gain`, or disconnect (`None`).
    SetRoute {
        from: Source,
        to: Sink,
        gain: Option<f32>,
    },
    /// A new clip; an empty name is taken from the file name.
    AddClip {
        path: String,
        name: String,
    },
    RemoveClip {
        id: ClipId,
    },
    UpdateClip {
        id: ClipId,
        settings: ClipSettings,
    },
    /// Start the clip (what happens to voices already playing is its `retrigger`).
    PlayClip {
        id: ClipId,
    },
    /// Fade out every voice of the clip.
    StopClip {
        id: ClipId,
    },
    /// Restart the engine now: after it gave up (`EngineStatus::Failed`), or to start afresh.
    RestartEngine,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiHello {
    pub protocol: u32,
    /// Who's connecting, for logs.
    pub client: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UiRequest {
    /// Send a snapshot, then diffs and meters. Sent again after a gap: a fresh snapshot.
    Subscribe,
    Command {
        id: RequestId,
        cmd: Command,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorKind {
    /// The command makes no sense (a gain out of range, an input picked as an output).
    Invalid,
    /// It names a device, app or clip the controller doesn't know.
    NotFound,
    /// This system can't do it (e.g. an undocumented API missing on this Windows build).
    Unsupported,
    /// It can't be done right now (the engine is restarting, the driver is busy).
    Unavailable,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorInfo {
    pub kind: ErrorKind,
    /// For people: shown in the UI's toast.
    pub message: String,
}

impl ErrorInfo {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ErrorInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UiMessage {
    Welcome {
        protocol: u32,
        controller: String,
    },
    Refused {
        reason: String,
    },
    /// The whole view as of diff number `seq`; the next diff is `seq + 1`.
    Snapshot {
        seq: u64,
        view: StateView,
    },
    Diff {
        seq: u64,
        ops: Vec<DiffOp>,
    },
    Accepted(RequestId),
    Completed(RequestId),
    Failed(RequestId, ErrorInfo),
    /// Throttled (~30 Hz) and separate from diffs, so meters never delay state. Only what's
    /// in the running graph is metered.
    Meters(Vec<(MeterTarget, MeterLevels)>),
}

/// What [`Mirror::apply`] did with a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Synced {
    /// The view changed (a snapshot or the next diff).
    Updated,
    /// Not a state message; nothing to do.
    Ignored,
    /// A diff arrived before any snapshot or out of sequence: the view is stale until a new
    /// snapshot. Send [`UiRequest::Subscribe`].
    Gap,
}

/// A UI's copy of the view, kept in sync from snapshots and diffs.
#[derive(Debug, Clone, Default)]
pub struct Mirror {
    pub view: StateView,
    /// Last diff applied (or the snapshot's).
    pub seq: u64,
    /// A snapshot arrived and no diff has been missed since.
    pub synced: bool,
}

impl Mirror {
    pub fn apply(&mut self, msg: &UiMessage) -> Synced {
        match msg {
            UiMessage::Snapshot { seq, view } => {
                self.view = view.clone();
                self.seq = *seq;
                self.synced = true;
                Synced::Updated
            }
            UiMessage::Diff { seq, ops } => {
                if !self.synced {
                    // Still waiting for the snapshot asked for after a gap.
                    return Synced::Ignored;
                }
                if *seq != self.seq + 1 {
                    self.synced = false;
                    return Synced::Gap;
                }
                self.view.apply(ops.iter().cloned());
                self.seq = *seq;
                Synced::Updated
            }
            _ => Synced::Ignored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{read_frame, write_frame};

    /// xorshift64*: deterministic, no dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        fn gain(&mut self) -> f32 {
            self.below(201) as f32 / 100.0
        }
    }

    fn dev(i: u64) -> DeviceKey {
        DeviceKey(format!("{{0.0.0.00000000}}.{{dev-{i}}}"))
    }

    fn app(i: u64) -> AppKey {
        AppKey(format!(r"C:\Apps\app{i}.exe"))
    }

    /// One random edit: insert, change or remove an entry somewhere, or change a scalar.
    fn mutate(v: &mut StateView, r: &mut Rng) {
        let remove = r.below(3) == 0;
        match r.below(6) {
            0 => {
                let k = dev(r.below(6));
                if remove {
                    v.devices.remove(&k);
                } else {
                    let flow = if r.below(2) == 0 { Flow::Render } else { Flow::Capture };
                    v.devices.insert(
                        k,
                        DeviceView {
                            name: format!("Device {}", r.below(100)),
                            flow,
                            present: r.below(2) == 0,
                            selected: flow == Flow::Render && r.below(2) == 0,
                            loops: r.below(5) == 0,
                            failed: r.below(7) == 0,
                        },
                    );
                }
            }
            1 => {
                let k = app(r.below(6));
                if remove {
                    v.apps.remove(&k);
                } else {
                    v.apps.insert(
                        k,
                        AppView {
                            name: format!("App {}", r.below(100)),
                            level: Level {
                                gain: r.gain(),
                                muted: r.below(4) == 0,
                            },
                            sessions: r.below(3) as u32,
                        },
                    );
                }
            }
            2 => {
                let k = ClipId(r.below(6) as u32);
                if remove {
                    v.clips.remove(&k);
                } else {
                    v.clips.insert(
                        k,
                        ClipView {
                            path: format!(r"C:\clips\{}.ogg", r.below(10)),
                            settings: ClipSettings {
                                name: format!("Clip {}", r.below(100)),
                                gain: r.gain(),
                                retrigger: [Retrigger::Restart, Retrigger::Overlap, Retrigger::Ignore]
                                    [r.below(3) as usize],
                                looped: r.below(2) == 0,
                            },
                            playing: r.below(2) == 0,
                        },
                    );
                }
            }
            3 => {
                let from = match r.below(3) {
                    0 => Source::Device(dev(r.below(6))),
                    1 => Source::App(app(r.below(6))),
                    _ => Source::Clips,
                };
                let key = (from, Sink::Device(dev(r.below(6))));
                match (r.below(2), remove) {
                    (0, true) => drop(v.routes.remove(&key)),
                    (0, false) => drop(v.routes.insert(key, r.gain())),
                    (_, true) => drop(v.would_loop.remove(&key)),
                    (_, false) => drop(v.would_loop.insert(key)),
                }
            }
            4 => {
                v.master = Level {
                    gain: r.gain(),
                    muted: r.below(2) == 0,
                };
                v.system_input = match r.below(3) {
                    0 => None,
                    1 => Some(SystemInput::Device(dev(r.below(6)))),
                    _ => Some(SystemInput::Endpoint(r.below(4) as u32)),
                };
            }
            _ => {
                v.engine = match r.below(5) {
                    0 => EngineStatus::Down,
                    1 => EngineStatus::Starting,
                    2 => EngineStatus::Running {
                        applied_version: r.below(50),
                    },
                    3 => EngineStatus::Restarting {
                        attempt: r.below(5) as u32,
                    },
                    _ => EngineStatus::Failed {
                        reason: "crashed".into(),
                    },
                }
            }
        }
    }

    #[test]
    fn applying_a_diff_reproduces_the_new_view() {
        let mut r = Rng(0x9e37_79b9_7f4a_7c15);
        let mut old = StateView::default();
        for _ in 0..2000 {
            let mut new = old.clone();
            for _ in 0..r.below(4) {
                mutate(&mut new, &mut r);
            }
            let ops = diff(&old, &new);
            assert_eq!(ops.is_empty(), old == new);
            let mut applied = old.clone();
            applied.apply(ops);
            assert_eq!(applied, new);
            assert!(diff(&new, &new).is_empty());
            old = new;
        }
        assert!(
            !old.devices.is_empty() && !old.routes.is_empty(),
            "the walk covered the maps"
        );
    }

    #[test]
    fn a_diff_only_names_what_changed() {
        let mut r = Rng(7);
        let mut old = StateView::default();
        for _ in 0..40 {
            mutate(&mut old, &mut r);
        }
        let mut new = old.clone();
        new.apps.values_mut().next().unwrap().level.gain = 0.123;
        let ops = diff(&old, &new);
        assert_eq!(ops.len(), 1);
        assert!(matches!(&ops[0], DiffOp::App(_, Some(a)) if a.level.gain == 0.123));
    }

    #[test]
    fn mirror_follows_the_sequence_and_flags_gaps() {
        let mut view = StateView::default();
        view.master.gain = 0.5;
        let mut m = Mirror::default();
        let diff_to = |seq, gain| UiMessage::Diff {
            seq,
            ops: vec![DiffOp::Master(Level { gain, muted: false })],
        };
        assert_eq!(m.apply(&diff_to(1, 0.1)), Synced::Ignored, "no snapshot yet");
        assert_eq!(m.apply(&UiMessage::Snapshot { seq: 4, view }), Synced::Updated);
        assert_eq!(m.apply(&diff_to(5, 0.7)), Synced::Updated);
        assert_eq!(m.view.master.gain, 0.7);
        assert_eq!(m.apply(&UiMessage::Accepted(3)), Synced::Ignored);
        assert_eq!(m.apply(&diff_to(7, 0.9)), Synced::Gap, "6 is missing");
        assert!(!m.synced);
        assert_eq!(m.apply(&diff_to(8, 0.9)), Synced::Ignored, "stale until a snapshot");
        assert_eq!(m.view.master.gain, 0.7);
        let mut fresh = StateView::default();
        fresh.master.gain = 0.9;
        assert_eq!(m.apply(&UiMessage::Snapshot { seq: 8, view: fresh }), Synced::Updated);
        assert_eq!(m.apply(&diff_to(9, 0.2)), Synced::Updated);
        assert_eq!((m.seq, m.view.master.gain), (9, 0.2));
    }

    fn round_trip<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(msg: &T) {
        let mut buf = Vec::new();
        write_frame(&mut buf, msg).unwrap();
        let back: T = read_frame(&mut buf.as_slice()).unwrap().unwrap();
        assert_eq!(&back, msg);
    }

    #[test]
    fn every_message_round_trips() {
        let mut r = Rng(11);
        let mut view = StateView::default();
        for _ in 0..60 {
            mutate(&mut view, &mut r);
        }
        round_trip(&UiHello {
            protocol: UI_PROTOCOL_VERSION,
            client: "overlay".into(),
        });
        let settings = ClipSettings {
            name: "Airhorn".into(),
            gain: 0.8,
            retrigger: Retrigger::Overlap,
            looped: false,
        };
        for cmd in [
            Command::SelectOutput {
                device: dev(1),
                selected: true,
            },
            Command::SetMasterGain(0.5),
            Command::SetSystemInput(Some(SystemInput::Device(dev(4)))),
            Command::SetSystemInput(Some(SystemInput::Endpoint(0))),
            Command::SetSystemInput(None),
            Command::SetMasterMuted(true),
            Command::SetAppGain { app: app(2), gain: 1.5 },
            Command::SetAppMuted {
                app: app(2),
                muted: true,
            },
            Command::SetRoute {
                from: Source::Clips,
                to: Sink::Device(dev(3)),
                gain: Some(0.25),
            },
            Command::SetRoute {
                from: Source::App(app(1)),
                to: Sink::Device(dev(3)),
                gain: None,
            },
            Command::AddClip {
                path: r"C:\clips\airhorn.ogg".into(),
                name: String::new(),
            },
            Command::RemoveClip { id: ClipId(4) },
            Command::UpdateClip {
                id: ClipId(4),
                settings: settings.clone(),
            },
        ] {
            round_trip(&UiRequest::Command { id: 42, cmd });
        }
        round_trip(&UiRequest::Subscribe);
        let empty = StateView::default();
        for msg in [
            UiMessage::Welcome {
                protocol: 1,
                controller: "sb-controller 0.1".into(),
            },
            UiMessage::Refused { reason: "old".into() },
            UiMessage::Snapshot {
                seq: 3,
                view: view.clone(),
            },
            UiMessage::Diff {
                seq: 4,
                ops: diff(&empty, &view),
            },
            UiMessage::Diff {
                seq: 5,
                ops: diff(&view, &empty),
            },
            UiMessage::Accepted(42),
            UiMessage::Completed(42),
            UiMessage::Failed(43, ErrorInfo::new(ErrorKind::NotFound, "no such app")),
            UiMessage::Meters(vec![
                (
                    MeterTarget::Device(dev(1)),
                    MeterLevels {
                        peak: [0.5, 0.4],
                        rms: [0.2, 0.1],
                    },
                ),
                (MeterTarget::App(app(2)), MeterLevels::default()),
                (MeterTarget::System, MeterLevels::default()),
                (MeterTarget::Clips, MeterLevels::default()),
            ]),
        ] {
            round_trip(&msg);
        }
    }
}
