//! Default-device safety (`Controller Notes` → Default device safety).
//!
//! Interface 1 hears the PC through the system input: on VB-Cable, apps must play into the
//! cable's input ("CABLE Input") for the sound to come out of its output, where the engine
//! reads it. So while the engine runs, the controller makes that device **Windows' default
//! output** (all three roles), and gives the user's own defaults back whenever it stops being
//! able to deliver the sound. While a dead engine owns the default, the whole PC is silent.
//!
//! - **Taken** when all of these hold: the user lets it (`Desired::keep_windows_default` is
//!   off), the system input has a render side ([`target`]: a cable's input, or the driver
//!   endpoint's device), and the engine runs. A crash or restart gets [`GRACE`] to come back
//!   before the defaults are handed back. Taken again when it's running.
//! - **Previous defaults** are remembered before taking over, in memory and in a **marker
//!   file** written at once (not with the debounced settings). The marker is removed once
//!   they're given back. On startup, a marker left behind means the controller died holding
//!   the default: [`Defaults::new`] reads it and the first sync gives the defaults back, before
//!   anything is taken again.
//! - **Someone else changes the default** while it's held (a headset plugged in, the Windows
//!   settings): ours is put back, at most every [`REASSERT_EVERY`] so two tools can't fight
//!   at full speed. To really change it, the user turns the takeover off.
//! - **On exit** the actor hands back and waits for it ([`Defaults::give_back`]).

use crate::state::{AppState, Role};
use crate::watch::WinMsg;
use sb_shared::ui::{DefaultOutputView, DeviceKey, EngineStatus, SystemInput};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

/// How long the engine may be down before the defaults are handed back.
pub const GRACE: Duration = Duration::from_secs(3);
/// Least time between putting ours back after someone else changed it.
pub const REASSERT_EVERY: Duration = Duration::from_secs(2);

pub type DefaultsMap = BTreeMap<Role, DeviceKey>;

/// The render device that feeds the system input: where apps must play for Interface 1 to
/// hear them.
pub fn target(state: &AppState) -> Option<DeviceKey> {
    let o = &state.observed;
    let key = match state.desired.system_input.as_ref()? {
        SystemInput::Device(capture) => state.feed_of(capture)?,
        SystemInput::Endpoint(id) => o.endpoints.get(id).filter(|e| e.enabled)?.device.clone()?,
    };
    o.devices.contains_key(&key).then_some(key)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Goal {
    Take,
    GiveBack,
}

#[derive(Default)]
pub struct Defaults {
    windows: Option<Sender<WinMsg>>,
    marker: Option<PathBuf>,
    /// The user's defaults from before the takeover, while it lasts (or left by a crash).
    previous: Option<DefaultsMap>,
    /// The job in flight.
    pending: Option<(u64, Goal)>,
    last_op: u64,
    /// The engine has run since the controller started (nothing is taken before).
    engine_ran: bool,
    /// Since when the engine isn't running.
    down_since: Option<Instant>,
    last_take: Option<Instant>,
    /// Why the last attempt failed, for the view.
    pub problem: Option<String>,
}

impl Defaults {
    /// Manage the defaults through the COM thread, remembering the user's in `marker`. A marker
    /// left by a run that died holding the default is read, so the first sync gives them back.
    pub fn new(windows: Sender<WinMsg>, marker: Option<PathBuf>) -> Self {
        let previous = marker.as_deref().and_then(read_marker);
        if previous.is_some() {
            eprintln!("controller: Windows' default output was left with the soundboard; giving it back");
        }
        Self {
            windows: Some(windows),
            marker,
            previous,
            ..Self::default()
        }
    }

    /// Whether the user's defaults are being kept for later (the takeover is on).
    pub fn holding(&self) -> bool {
        self.previous.is_some()
    }

    fn engine_ok(&mut self, state: &AppState, now: Instant) -> bool {
        if matches!(state.observed.engine, EngineStatus::Running { .. }) {
            (self.engine_ran, self.down_since) = (true, None);
            return true;
        }
        let since = *self.down_since.get_or_insert(now);
        // Never ran: nothing to wait for. Failed: no point waiting.
        self.engine_ran
            && !matches!(state.observed.engine, EngineStatus::Failed { .. })
            && now.duration_since(since) < GRACE
    }

    fn wanted(&mut self, state: &AppState, now: Instant) -> Option<DeviceKey> {
        let ok = self.engine_ok(state, now);
        (!state.desired.keep_windows_default && ok)
            .then(|| target(state))
            .flatten()
    }

    fn op(&mut self, goal: Goal, defaults: DefaultsMap) {
        let Some(w) = &self.windows else { return };
        self.last_op += 1;
        let op = self.last_op;
        if w.send(WinMsg::SetDefaults { op, defaults }).is_ok() {
            self.pending = Some((op, goal));
        }
    }

    /// Bring Windows' defaults in line with the state. Called after every message.
    pub fn sync(&mut self, state: &AppState, now: Instant) {
        if self.windows.is_none() || self.pending.is_some() {
            return;
        }
        let current = &state.observed.defaults;
        match (self.wanted(state, now), &self.previous) {
            (Some(t), _) => {
                if Role::ALL.iter().all(|r| current.get(r) == Some(&t)) {
                    return;
                }
                if self.previous.is_some() && self.last_take.is_some_and(|at| now.duration_since(at) < REASSERT_EVERY) {
                    return;
                }
                if self.previous.is_none() {
                    // Remember the user's before touching anything, on disk first.
                    let previous: DefaultsMap = current.clone();
                    if let Some(path) = &self.marker
                        && let Err(e) = write_marker(path, &previous)
                    {
                        eprintln!("controller: can't write {}: {e}", path.display());
                    }
                    self.previous = Some(previous);
                }
                self.last_take = Some(now);
                self.op(Goal::Take, Role::ALL.iter().map(|r| (*r, t.clone())).collect());
            }
            (None, Some(previous)) => {
                let previous = previous.clone();
                self.op(Goal::GiveBack, previous);
            }
            (None, None) => {}
        }
    }

    /// A `WinMsg::SetDefaults` finished; `false` if it wasn't ours.
    pub fn done(&mut self, op: u64, result: Result<(), String>) -> bool {
        let Some((pending, goal)) = self.pending else {
            return false;
        };
        if pending != op {
            return false;
        }
        self.pending = None;
        match (&result, goal) {
            (Ok(()), Goal::GiveBack) => {
                self.previous = None;
                self.remove_marker();
                self.problem = None;
            }
            (Ok(()), Goal::Take) => self.problem = None,
            (Err(e), _) => {
                eprintln!("controller: setting Windows' default output: {e}");
                self.problem = Some(e.clone());
                if goal == Goal::GiveBack {
                    // Don't retry at full speed; the next change of state tries again.
                    self.previous = None;
                    self.remove_marker();
                }
            }
        }
        true
    }

    fn remove_marker(&self) {
        if let Some(path) = &self.marker {
            let _ = std::fs::remove_file(path);
        }
    }

    /// When the actor should look again: the engine's grace running out.
    pub fn next_deadline(&self) -> Option<Instant> {
        (self.previous.is_some() && self.engine_ran)
            .then_some(self.down_since?)
            .map(|t| t + GRACE)
    }

    /// For the exit: start handing the defaults back; the op to wait for, if any.
    pub fn give_back(&mut self) -> Option<u64> {
        let previous = self.previous.clone()?;
        self.pending = None;
        self.op(Goal::GiveBack, previous);
        self.pending.map(|(op, _)| op)
    }

    pub fn status(&self, state: &AppState) -> DefaultOutputView {
        let target = target(state);
        let owned = target
            .as_ref()
            .is_some_and(|t| Role::ALL.iter().all(|r| state.observed.defaults.get(r) == Some(t)));
        let wanted = !state.desired.keep_windows_default && target.is_some();
        DefaultOutputView {
            wanted,
            owned,
            handed_back: wanted && self.engine_ran && self.previous.is_none() && !owned,
            target,
            problem: self.problem.clone(),
        }
    }
}

// --- The marker -----------------------------------------------------------------------------

/// The marker's place: beside the settings file.
pub fn marker_path(config: &Path) -> PathBuf {
    config.with_file_name("default-output.toml")
}

/// Written at once (no debounce): it has to be on disk before Windows' default changes.
pub fn write_marker(path: &Path, previous: &DefaultsMap) -> std::io::Result<()> {
    let mut text = String::from(
        "# Windows' default outputs before the soundboard took over. If this file is here when\n\
         # sb-controller starts, it died holding the default and puts these back.\n",
    );
    for (role, device) in previous {
        text.push_str(&format!("{} = {:?}\n", role.name(), device.0));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

pub fn read_marker(path: &Path) -> Option<DefaultsMap> {
    let text = std::fs::read_to_string(path).ok()?;
    let table: toml::Table = text.parse().ok()?;
    let map: DefaultsMap = table
        .iter()
        .filter_map(|(k, v)| Some((Role::from_name(k)?, DeviceKey(v.as_str()?.to_string()))))
        .collect();
    Some(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DeviceInfo, Loopback, Observation};
    use sb_shared::ui::Flow;
    use std::sync::mpsc::{self, Receiver};

    fn k(s: &str) -> DeviceKey {
        DeviceKey(s.into())
    }

    fn all(d: &str) -> DefaultsMap {
        Role::ALL.iter().map(|r| (*r, k(d))).collect()
    }

    /// Speakers are the user's default; a cable is there and is the system input.
    fn rig(marker: Option<PathBuf>) -> (AppState, Defaults, Receiver<WinMsg>) {
        let mut s = AppState::default();
        let dev = |id: &str, flow| Observation::DeviceAdded {
            key: k(id),
            info: DeviceInfo { name: id.into(), flow },
        };
        for o in [
            dev("speakers", Flow::Render),
            dev("cable-in", Flow::Render),
            dev("cable-out", Flow::Capture),
            Observation::Loopback {
                render: k("cable-in"),
                into: Some(Loopback::Capture(k("cable-out"))),
            },
            Observation::Defaults(all("speakers")),
        ] {
            s.observe(o);
        }
        s.desired.system_input = Some(SystemInput::Device(k("cable-out")));
        let (tx, rx) = mpsc::channel();
        (s, Defaults::new(tx, marker), rx)
    }

    fn running(s: &mut AppState) {
        s.observed.engine = EngineStatus::Running { applied_version: 1 };
    }

    /// The job sent, answered `result`, and Windows updated as asked if it worked.
    fn answer(s: &mut AppState, d: &mut Defaults, rx: &Receiver<WinMsg>, result: Result<(), String>) -> DefaultsMap {
        let Ok(WinMsg::SetDefaults { op, defaults }) = rx.try_recv() else {
            panic!("no job")
        };
        if result.is_ok() {
            s.observe(Observation::Defaults(defaults.clone()));
        }
        assert!(d.done(op, result));
        defaults
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sb-defaults-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("default-output.toml")
    }

    #[test]
    fn taken_while_the_engine_runs_and_given_back_after_the_grace() {
        let marker = scratch("grace");
        let (mut s, mut d, rx) = rig(Some(marker.clone()));
        let t0 = Instant::now();
        d.sync(&s, t0);
        assert!(rx.try_recv().is_err(), "not before the engine runs");
        assert_eq!(target(&s), Some(k("cable-in")));

        running(&mut s);
        d.sync(&s, t0);
        assert_eq!(answer(&mut s, &mut d, &rx, Ok(())), all("cable-in"));
        assert_eq!(read_marker(&marker), Some(all("speakers")), "remembered on disk first");
        assert!(d.status(&s).owned);
        d.sync(&s, t0);
        assert!(rx.try_recv().is_err(), "nothing more to do");

        // The engine crashes: held through the grace, handed back after.
        s.observed.engine = EngineStatus::Restarting { attempt: 1 };
        d.sync(&s, t0 + Duration::from_secs(1));
        assert!(rx.try_recv().is_err());
        assert_eq!(d.next_deadline(), Some(t0 + Duration::from_secs(1) + GRACE));
        d.sync(&s, t0 + Duration::from_secs(5));
        assert_eq!(answer(&mut s, &mut d, &rx, Ok(())), all("speakers"));
        assert!(!marker.exists(), "the marker goes with the takeover");
        assert!(d.status(&s).handed_back);

        // Back up: taken again.
        running(&mut s);
        d.sync(&s, t0 + Duration::from_secs(6));
        assert_eq!(answer(&mut s, &mut d, &rx, Ok(())), all("cable-in"));
        let _ = std::fs::remove_dir_all(marker.parent().unwrap());
    }

    #[test]
    fn a_failed_engine_hands_back_at_once() {
        let (mut s, mut d, rx) = rig(None);
        running(&mut s);
        d.sync(&s, Instant::now());
        answer(&mut s, &mut d, &rx, Ok(()));
        s.observed.engine = EngineStatus::Failed {
            reason: "crashed".into(),
        };
        d.sync(&s, Instant::now());
        assert_eq!(answer(&mut s, &mut d, &rx, Ok(())), all("speakers"));
    }

    #[test]
    fn the_user_can_say_no_and_a_changed_default_is_put_back_slowly() {
        let (mut s, mut d, rx) = rig(None);
        running(&mut s);
        let t0 = Instant::now();
        d.sync(&s, t0);
        answer(&mut s, &mut d, &rx, Ok(()));
        // Something else picks the speakers: not fought over at once, put back later.
        s.observe(Observation::Defaults(all("speakers")));
        d.sync(&s, t0 + Duration::from_millis(500));
        assert!(rx.try_recv().is_err());
        d.sync(&s, t0 + REASSERT_EVERY);
        let back = answer(&mut s, &mut d, &rx, Ok(()));
        assert_eq!(back, all("cable-in"));
        // The previous defaults are still the user's original ones, not ours.
        s.desired.keep_windows_default = true;
        d.sync(&s, t0 + REASSERT_EVERY * 2);
        assert_eq!(answer(&mut s, &mut d, &rx, Ok(())), all("speakers"));
        assert!(!d.status(&s).wanted);
    }

    #[test]
    fn without_a_cable_nothing_is_touched() {
        let (mut s, mut d, rx) = rig(None);
        running(&mut s);
        s.desired.system_input = Some(SystemInput::Device(k("mic")));
        d.sync(&s, Instant::now());
        assert!(rx.try_recv().is_err());
        assert_eq!(d.status(&s).target, None);
    }

    #[test]
    fn a_marker_left_by_a_crash_is_given_back_first() {
        let marker = scratch("crash");
        let mut prev = all("speakers");
        prev.insert(Role::Communications, k("headset"));
        write_marker(&marker, &prev).unwrap();
        let (mut s, mut d, rx) = rig(Some(marker.clone()));
        // Windows still has the cable from the crashed run.
        s.observe(Observation::Defaults(all("cable-in")));
        d.sync(&s, Instant::now());
        assert_eq!(answer(&mut s, &mut d, &rx, Ok(())), prev, "before the engine even runs");
        assert!(!marker.exists());
        let _ = std::fs::remove_dir_all(marker.parent().unwrap());
    }

    #[test]
    fn on_exit_the_defaults_go_back() {
        let (mut s, mut d, rx) = rig(None);
        running(&mut s);
        d.sync(&s, Instant::now());
        answer(&mut s, &mut d, &rx, Ok(()));
        let op = d.give_back().expect("held");
        let Ok(WinMsg::SetDefaults { op: sent, defaults }) = rx.try_recv() else {
            panic!()
        };
        assert_eq!((sent, defaults), (op, all("speakers")));
        assert!(d.done(op, Ok(())));
        assert_eq!(d.give_back(), None, "nothing left to give");
    }
}
