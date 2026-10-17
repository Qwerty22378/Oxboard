//! Interface 2: sending an app to outputs of its own, through a driver endpoint.
//!
//! Routing an app touches three systems, so `Command::RouteApp` is a small state machine
//! in the actor, advanced by worker results (see `Controller Notes` → Multi-step commands):
//!
//! 1. **Pick** a free render endpoint of the pool (one per app) and reserve it.
//! 2. **Enable** it in the driver, named after the app (driver worker).
//! 3. **Point** the app's running processes at its Windows endpoint, their per-app default
//!    output (COM thread, `IAudioPolicyConfigFactory`). Skipped if the app isn't running:
//!    that happens when it starts.
//! 4. **Commit** to desired state; the next graph reads the endpoint into the outputs.
//!
//! A step that fails undoes the ones before it (step 3 failing disables the endpoint again)
//! and the command fails with the reason; desired state only changes at the commit. Sending
//! an app back to the Windows default runs the other way: point it back, disable, commit.
//!
//! Afterwards [`Routing::sync`] keeps it true: an app routed while it wasn't running is
//! pointed at its endpoint when its processes appear, and an endpoint the driver lost (a
//! driver restart) is enabled again.
//!
//! `Completed` doesn't mean the app moved: apps only switch when they open a new stream, and
//! apps with their own device setting never do. The view says `Pending` until a session of
//! the app is on its endpoint, then `Active`.

use crate::driver::DriverJob;
use crate::hub::ClientId;
use crate::loops;
use crate::state::{AppState, DriverStatus};
use crate::watch::WinMsg;
use sb_shared::ui::{AppKey, DeviceKey, ErrorInfo, ErrorKind, Flow, RequestId, UiMessage};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::Sender;

/// What starting a command led to.
#[derive(Debug, PartialEq)]
pub enum Started {
    /// Finished on the spot.
    Done(Result<(), ErrorInfo>),
    /// Running; the reply comes when its last step does.
    Later,
}

#[derive(Debug)]
enum Stage {
    Enabling,
    Pointing,
    /// Undoing after a failed step: disabling the endpoint, then failing with `why`.
    Undoing {
        why: ErrorInfo,
    },
    Releasing,
    Disabling,
}

#[derive(Debug)]
struct Op {
    client: ClientId,
    id: RequestId,
    app: AppKey,
    outputs: BTreeSet<DeviceKey>,
    endpoint: u32,
    stage: Stage,
}

#[derive(Default)]
pub struct Routing {
    driver: Option<Sender<DriverJob>>,
    windows: Option<Sender<WinMsg>>,
    ops: BTreeMap<u64, Op>,
    last_op: u64,
    /// Processes already pointed at their app's endpoint.
    pointed: BTreeSet<(AppKey, u32)>,
    /// Endpoints being enabled again by `sync` (once each until the driver says so).
    reenabling: BTreeSet<u32>,
}

fn err(kind: ErrorKind, msg: impl Into<String>) -> ErrorInfo {
    ErrorInfo::new(kind, msg)
}

impl Routing {
    pub fn set_driver(&mut self, driver: Sender<DriverJob>) {
        self.driver = Some(driver);
    }

    pub fn set_windows(&mut self, windows: Sender<WinMsg>) {
        self.windows = Some(windows);
    }

    /// Whether a multi-step command is running.
    pub fn busy(&self) -> bool {
        !self.ops.is_empty()
    }

    fn op_id(&mut self) -> u64 {
        self.last_op += 1;
        self.last_op
    }

    fn pids(state: &AppState, app: &AppKey) -> Vec<u32> {
        state
            .observed
            .sessions
            .get(app)
            .map(|s| s.pids.iter().copied().collect())
            .unwrap_or_default()
    }

    /// The app's endpoint's Windows device, if it has one and it's enabled.
    pub fn endpoint_device(state: &AppState, app: &AppKey) -> Option<DeviceKey> {
        let id = state.desired.app_endpoints.get(app)?;
        let e = state.observed.endpoints.get(id).filter(|e| e.enabled)?;
        e.device.clone()
    }

    pub fn route_app(
        &mut self,
        state: &mut AppState,
        client: ClientId,
        id: RequestId,
        app: AppKey,
        outputs: BTreeSet<DeviceKey>,
    ) -> Started {
        let Some(app_name) = state.app_name(&app).map(str::to_owned) else {
            return Started::Done(Err(err(ErrorKind::NotFound, format!("no app {}", app.0))));
        };
        if self.ops.values().any(|o| o.app == app) {
            return Started::Done(Err(err(
                ErrorKind::Unavailable,
                format!("{app_name} is still being routed"),
            )));
        }
        for out in &outputs {
            match state.device(out) {
                None => return Started::Done(Err(err(ErrorKind::NotFound, format!("no device {}", out.0)))),
                Some(d) if d.flow != Flow::Render => {
                    return Started::Done(Err(err(ErrorKind::Invalid, format!("{} is an input", d.name))));
                }
                Some(_) => {}
            }
        }
        let current = state.desired.app_endpoints.get(&app).copied();

        // Back to the Windows default.
        if outputs.is_empty() {
            let Some(endpoint) = current else {
                state.desired.app_routes.remove(&app);
                state.desired.prune_known();
                return Started::Done(Ok(()));
            };
            let op = self.op_id();
            let mut o = Op {
                client,
                id,
                app,
                outputs,
                endpoint,
                stage: Stage::Releasing,
            };
            match (&self.windows, Self::pids(state, &o.app)) {
                (Some(w), pids) if !pids.is_empty() => {
                    let _ = w.send(WinMsg::SetAppOutput {
                        op,
                        app: o.app.clone(),
                        pids,
                        device: None,
                    });
                }
                _ => {
                    o.stage = Stage::Disabling;
                    self.disable(op, endpoint);
                }
            }
            self.ops.insert(op, o);
            return Started::Later;
        }

        // Already has its endpoint: only where it goes changes.
        if let Some(endpoint) = current {
            return Started::Done(Self::commit(state, &app, endpoint, outputs));
        }

        // A new route: check everything before touching anything.
        match &state.observed.driver {
            DriverStatus::Ready => {}
            DriverStatus::Missing(why) => {
                return Started::Done(Err(err(ErrorKind::Unavailable, format!("no app endpoints: {why}"))));
            }
            DriverStatus::Unknown => {
                return Started::Done(Err(err(ErrorKind::Unavailable, "the driver isn't ready yet")));
            }
        }
        if state.observed.app_routing == Some(false) {
            return Started::Done(Err(err(
                ErrorKind::Unsupported,
                "this version of Windows doesn't let apps be sent to another output",
            )));
        }
        if self.driver.is_none() {
            return Started::Done(Err(err(ErrorKind::Unavailable, "no driver worker")));
        }
        let taken: BTreeSet<u32> = state
            .desired
            .app_endpoints
            .values()
            .copied()
            .chain(self.ops.values().map(|o| o.endpoint))
            .chain(match &state.desired.system_input {
                Some(sb_shared::ui::SystemInput::Endpoint(e)) => Some(*e),
                _ => None,
            })
            .collect();
        let render = || state.observed.endpoints.iter().filter(|(_, e)| e.flow == Flow::Render);
        let Some(endpoint) = render().map(|(id, _)| *id).find(|id| !taken.contains(id)) else {
            return Started::Done(Err(err(
                ErrorKind::Unavailable,
                format!("all {} app endpoints are in use", render().count()),
            )));
        };
        let mut trial = state.clone();
        if let Err(e) = Self::commit(&mut trial, &app, endpoint, outputs.clone()) {
            return Started::Done(Err(e));
        }
        let op = self.op_id();
        if let Some(driver) = &self.driver {
            let _ = driver.send(DriverJob::SetEndpoint {
                op,
                id: endpoint,
                enabled: true,
                name: app_name,
            });
        }
        self.ops.insert(
            op,
            Op {
                client,
                id,
                app,
                outputs,
                endpoint,
                stage: Stage::Enabling,
            },
        );
        Started::Later
    }

    /// The route into desired state, unless it would loop.
    fn commit(
        state: &mut AppState,
        app: &AppKey,
        endpoint: u32,
        outputs: BTreeSet<DeviceKey>,
    ) -> Result<(), ErrorInfo> {
        let before = state.desired.clone();
        for out in &outputs {
            state.remember_device(out);
        }
        state.remember_app(app);
        state.desired.app_endpoints.insert(app.clone(), endpoint);
        state.desired.app_routes.insert(app.clone(), outputs);
        if let Some(l) = loops::find(state) {
            let why = format!(
                "that would loop the sound back into itself: {}",
                loops::describe(state, &l)
            );
            state.desired = before;
            return Err(err(ErrorKind::Invalid, why));
        }
        state.desired.prune_known();
        Ok(())
    }

    fn disable(&self, op: u64, endpoint: u32) {
        if let Some(d) = &self.driver {
            let _ = d.send(DriverJob::SetEndpoint {
                op,
                id: endpoint,
                enabled: false,
                name: String::new(),
            });
        }
    }

    /// A driver job finished: the next step, or the reply.
    pub fn driver_done(
        &mut self,
        state: &mut AppState,
        op: u64,
        result: Result<(), String>,
    ) -> Option<(ClientId, UiMessage)> {
        let mut o = self.ops.remove(&op)?;
        match (&o.stage, result) {
            (Stage::Enabling, Err(e)) => Some(Self::fail(
                &o,
                err(
                    ErrorKind::Unavailable,
                    format!("couldn't enable app endpoint {}: {e}", o.endpoint),
                ),
            )),
            (Stage::Enabling, Ok(())) => {
                let device = state.observed.endpoints.get(&o.endpoint).and_then(|e| e.device.clone());
                let pids = Self::pids(state, &o.app);
                match (&self.windows, device) {
                    (Some(w), Some(device)) if !pids.is_empty() => {
                        let _ = w.send(WinMsg::SetAppOutput {
                            op,
                            app: o.app.clone(),
                            pids,
                            device: Some(device),
                        });
                        o.stage = Stage::Pointing;
                        self.ops.insert(op, o);
                        None
                    }
                    // Not running (or no Windows side): pointed when its processes appear.
                    _ => Some(self.finish_route(state, o)),
                }
            }
            (Stage::Undoing { why }, result) => {
                if let Err(e) = result {
                    eprintln!("routing: couldn't disable endpoint {} again: {e}", o.endpoint);
                }
                Some(Self::fail(&o, why.clone()))
            }
            (Stage::Disabling, result) => {
                if let Err(e) = result {
                    // The app is back on the default already; an idle enabled endpoint is harmless.
                    eprintln!("routing: couldn't disable endpoint {}: {e}", o.endpoint);
                }
                state.desired.app_endpoints.remove(&o.app);
                state.desired.app_routes.remove(&o.app);
                state.desired.prune_known();
                self.pointed.retain(|(a, _)| *a != o.app);
                Some((o.client, UiMessage::Completed(o.id)))
            }
            (stage, _) => {
                eprintln!("routing: a driver result for op {op} at {stage:?}");
                None
            }
        }
    }

    /// A COM-thread job finished: the next step, or the reply.
    pub fn win_done(
        &mut self,
        state: &mut AppState,
        op: u64,
        result: Result<(), String>,
    ) -> Option<(ClientId, UiMessage)> {
        let mut o = self.ops.remove(&op)?;
        match (&o.stage, result) {
            (Stage::Pointing, Ok(())) => {
                for pid in Self::pids(state, &o.app) {
                    self.pointed.insert((o.app.clone(), pid));
                }
                Some(self.finish_route(state, o))
            }
            (Stage::Pointing, Err(e)) => {
                let why = err(
                    ErrorKind::Unavailable,
                    format!("Windows wouldn't send the app there: {e}"),
                );
                o.stage = Stage::Undoing { why };
                self.disable(op, o.endpoint);
                self.ops.insert(op, o);
                None
            }
            (Stage::Releasing, Ok(())) => {
                o.stage = Stage::Disabling;
                self.disable(op, o.endpoint);
                self.ops.insert(op, o);
                None
            }
            (Stage::Releasing, Err(e)) => Some(Self::fail(
                &o,
                err(
                    ErrorKind::Unavailable,
                    format!("Windows wouldn't send the app back: {e}"),
                ),
            )),
            (stage, _) => {
                eprintln!("routing: a Windows result for op {op} at {stage:?}");
                None
            }
        }
    }

    fn finish_route(&mut self, state: &mut AppState, o: Op) -> (ClientId, UiMessage) {
        match Self::commit(state, &o.app, o.endpoint, o.outputs.clone()) {
            Ok(()) => (o.client, UiMessage::Completed(o.id)),
            // Something changed meanwhile so that it loops now: undo the endpoint too.
            Err(why) => {
                self.disable(0, o.endpoint);
                Self::fail(&o, why)
            }
        }
    }

    fn fail(o: &Op, why: ErrorInfo) -> (ClientId, UiMessage) {
        (o.client, UiMessage::Failed(o.id, why))
    }

    /// Keep routes true after the fact: point new processes of routed apps at their
    /// endpoints, and enable endpoints the driver lost.
    pub fn sync(&mut self, state: &AppState) {
        let busy: BTreeSet<&AppKey> = self.ops.values().map(|o| &o.app).collect();
        let running: BTreeSet<(AppKey, u32)> = state
            .observed
            .sessions
            .iter()
            .flat_map(|(a, s)| s.pids.iter().map(move |p| (a.clone(), *p)))
            .collect();
        self.pointed.retain(|k| running.contains(k));
        if let Some(w) = &self.windows {
            for app in state.desired.app_endpoints.keys().filter(|a| !busy.contains(a)) {
                let Some(device) = Self::endpoint_device(state, app) else {
                    continue;
                };
                let new: Vec<u32> = running
                    .iter()
                    .filter(|(a, p)| a == app && !self.pointed.contains(&(a.clone(), *p)))
                    .map(|(_, p)| *p)
                    .collect();
                if !new.is_empty() {
                    let _ = w.send(WinMsg::SetAppOutput {
                        op: 0,
                        app: app.clone(),
                        pids: new.clone(),
                        device: Some(device),
                    });
                    self.pointed.extend(new.into_iter().map(|p| (app.clone(), p)));
                }
            }
        }
        if state.observed.driver == DriverStatus::Ready
            && let Some(d) = &self.driver
        {
            self.reenabling
                .retain(|id| state.observed.endpoints.get(id).is_some_and(|e| !e.enabled));
            for (app, id) in &state.desired.app_endpoints {
                let lost = state.observed.endpoints.get(id).is_some_and(|e| !e.enabled);
                if lost && !busy.contains(app) && self.reenabling.insert(*id) {
                    let _ = d.send(DriverJob::SetEndpoint {
                        op: 0,
                        id: *id,
                        enabled: true,
                        name: state.app_name(app).unwrap_or(&app.0).to_owned(),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::{Continuity, compile};
    use crate::driver::FakeDriver;
    use crate::state::{DeviceInfo, Observation, PoolEndpoint};
    use crate::view::project;
    use sb_shared::graph::NodeKind;
    use sb_shared::ui::RouteStatus;
    use std::sync::mpsc::{self, Receiver};

    struct Rig {
        state: AppState,
        r: Routing,
        driver: Receiver<DriverJob>,
        win: Receiver<WinMsg>,
    }

    fn dev(name: &str) -> DeviceKey {
        DeviceKey(format!("{{0.0.0.00000000}}.{{{name}}}"))
    }

    fn app(name: &str) -> AppKey {
        AppKey(format!(r"C:\{name}.exe"))
    }

    fn endpoint(id: u32, enabled: bool) -> Observation {
        Observation::Endpoint {
            id,
            state: Some(PoolEndpoint {
                enabled,
                flow: Flow::Render,
                name: format!("Soundboard {id}"),
                device: enabled.then(|| FakeDriver::windows_id(id, Flow::Render)),
            }),
        }
    }

    /// Speakers and headphones, Discord running (pid 7), Game not; three app endpoints.
    fn rig() -> Rig {
        let mut state = AppState::default();
        for d in ["speakers", "headphones"] {
            state.observe(Observation::DeviceAdded {
                key: dev(d),
                info: DeviceInfo {
                    name: d.into(),
                    flow: Flow::Render,
                },
            });
        }
        state.observe(Observation::SessionAdded {
            app: app("discord"),
            name: "Discord".into(),
            pid: 7,
        });
        state.desired.known_apps.insert(app("game"), "Game".into());
        state.observe(Observation::Driver(DriverStatus::Ready));
        state.observe(Observation::AppRouting(true));
        for id in 1..=3 {
            state.observe(endpoint(id, false));
        }
        let (dtx, driver) = mpsc::channel();
        let (wtx, win) = mpsc::channel();
        let mut r = Routing::default();
        r.set_driver(dtx);
        r.set_windows(wtx);
        Rig { state, r, driver, win }
    }

    impl Rig {
        fn route(&mut self, a: &str, outs: &[&str]) -> Started {
            let outputs = outs.iter().map(|o| dev(o)).collect();
            self.r.route_app(&mut self.state, 1, 42, app(a), outputs)
        }

        /// The driver job sent, carried out on the pool (the worker's observation first).
        fn drive(&mut self) -> (u64, u32, bool) {
            let DriverJob::SetEndpoint { op, id, enabled, .. } = self.driver.try_recv().expect("a driver job") else {
                panic!("not a SetEndpoint")
            };
            self.state.observe(endpoint(id, enabled));
            (op, id, enabled)
        }

        fn pointed(&mut self) -> (u64, Vec<u32>, Option<DeviceKey>) {
            match self.win.try_recv().expect("a Windows job") {
                WinMsg::SetAppOutput { op, pids, device, .. } => (op, pids, device),
                other => panic!("{other:?}"),
            }
        }

        fn status(&self, a: &str) -> Option<RouteStatus> {
            project(&self.state).apps[&app(a)].route.as_ref().map(|r| r.status)
        }
    }

    #[test]
    fn a_running_app_is_given_an_endpoint_pointed_at_it_and_then_committed() {
        let mut g = rig();
        assert_eq!(g.route("discord", &["headphones"]), Started::Later);
        assert!(g.state.desired.app_endpoints.is_empty(), "nothing committed yet");
        let (op, id, enabled) = g.drive();
        assert_eq!((id, enabled), (1, true));
        assert_eq!(g.r.driver_done(&mut g.state, op, Ok(())), None, "next step");
        let (op2, pids, device) = g.pointed();
        assert_eq!((op2, pids), (op, vec![7]));
        assert_eq!(device, Some(FakeDriver::windows_id(1, Flow::Render)));
        assert_eq!(
            g.r.win_done(&mut g.state, op, Ok(())),
            Some((1, UiMessage::Completed(42)))
        );
        assert_eq!(g.state.desired.app_endpoints[&app("discord")], 1);
        // The graph reads the endpoint into the headphones.
        let c = compile(&g.state, &mut Continuity::default()).unwrap();
        assert!(
            c.graph
                .nodes
                .iter()
                .any(|n| n.kind == NodeKind::VirtualSource { endpoint: 1 })
        );
        // Pending until Discord really plays through it.
        assert_eq!(g.status("discord"), Some(RouteStatus::Pending));
        g.state.observe(Observation::AppDevices {
            app: app("discord"),
            devices: [FakeDriver::windows_id(1, Flow::Render)].into(),
        });
        assert_eq!(g.status("discord"), Some(RouteStatus::Active));
        // Sync knows pid 7 is done: nothing more is sent.
        g.r.sync(&g.state);
        assert!(g.win.try_recv().is_err());
        // Changing its outputs is immediate: the endpoint stays.
        assert_eq!(g.route("discord", &["speakers", "headphones"]), Started::Done(Ok(())));
        assert!(g.driver.try_recv().is_err());
        assert_eq!(g.state.desired.app_routes[&app("discord")].len(), 2);
    }

    #[test]
    fn if_windows_refuses_the_endpoint_is_disabled_again_and_nothing_changes() {
        let mut g = rig();
        let before = g.state.desired.clone();
        g.route("discord", &["headphones"]);
        let (op, ..) = g.drive();
        g.r.driver_done(&mut g.state, op, Ok(()));
        g.pointed();
        assert_eq!(g.r.win_done(&mut g.state, op, Err("access denied".into())), None);
        let (op2, id, enabled) = g.drive();
        assert_eq!((op2, id, enabled), (op, 1, false), "the compensation");
        let Some((1, UiMessage::Failed(42, e))) = g.r.driver_done(&mut g.state, op, Ok(())) else {
            panic!("fails")
        };
        assert!(e.message.contains("access denied"), "{}", e.message);
        assert_eq!(g.state.desired, before);
        assert!(!g.r.busy());
    }

    #[test]
    fn if_the_driver_refuses_nothing_else_happens() {
        let mut g = rig();
        g.route("discord", &["headphones"]);
        let DriverJob::SetEndpoint { op, .. } = g.driver.try_recv().unwrap() else {
            unreachable!()
        };
        let Some((_, UiMessage::Failed(42, e))) = g.r.driver_done(&mut g.state, op, Err("pool locked".into())) else {
            panic!("fails")
        };
        assert!(e.message.contains("pool locked"));
        assert!(g.win.try_recv().is_err(), "Windows never asked");
        assert!(g.state.desired.app_endpoints.is_empty());
    }

    #[test]
    fn an_app_that_isnt_running_is_pointed_when_it_starts() {
        let mut g = rig();
        g.route("game", &["speakers"]);
        let (op, id, _) = g.drive();
        assert_eq!(id, 1);
        assert_eq!(
            g.r.driver_done(&mut g.state, op, Ok(())),
            Some((1, UiMessage::Completed(42))),
            "nothing to point yet"
        );
        assert!(g.win.try_recv().is_err());
        assert_eq!(g.status("game"), Some(RouteStatus::NotRunning));
        g.state.observe(Observation::SessionAdded {
            app: app("game"),
            name: "Game".into(),
            pid: 9,
        });
        g.r.sync(&g.state);
        assert_eq!(g.pointed(), (0, vec![9], Some(FakeDriver::windows_id(1, Flow::Render))));
        g.r.sync(&g.state);
        assert!(g.win.try_recv().is_err(), "once per process");
        assert_eq!(g.status("game"), Some(RouteStatus::Pending));
    }

    #[test]
    fn sending_an_app_back_points_it_back_disables_and_forgets() {
        let mut g = rig();
        g.route("discord", &["headphones"]);
        let (op, ..) = g.drive();
        g.r.driver_done(&mut g.state, op, Ok(()));
        g.pointed();
        g.r.win_done(&mut g.state, op, Ok(()));

        assert_eq!(g.route("discord", &[]), Started::Later);
        let (op, pids, device) = g.pointed();
        assert_eq!((pids, device), (vec![7], None));
        assert_eq!(g.r.win_done(&mut g.state, op, Ok(())), None);
        let (op2, id, enabled) = g.drive();
        assert_eq!((op2, id, enabled), (op, 1, false));
        assert_eq!(
            g.r.driver_done(&mut g.state, op, Ok(())),
            Some((1, UiMessage::Completed(42)))
        );
        assert!(g.state.desired.app_endpoints.is_empty() && g.state.desired.app_routes.is_empty());
        assert_eq!(g.status("discord"), None);
    }

    #[test]
    fn refusals_before_anything_is_touched() {
        let mut g = rig();
        // Busy with the same app.
        g.route("discord", &["headphones"]);
        let Started::Done(Err(e)) = g.route("discord", &["speakers"]) else {
            panic!()
        };
        assert_eq!(e.kind, ErrorKind::Unavailable);
        // The pool runs out: one endpoint reserved by the running op, two taken.
        g.state.desired.app_endpoints.insert(app("x"), 2);
        g.state.desired.app_endpoints.insert(app("y"), 3);
        let Started::Done(Err(e)) = g.route("game", &["speakers"]) else {
            panic!()
        };
        assert!(e.message.contains("all 3 app endpoints are in use"), "{}", e.message);
        // Not an output, not a device, not an app.
        let mut g = rig();
        g.state.observe(Observation::DeviceAdded {
            key: dev("mic"),
            info: DeviceInfo {
                name: "Mic".into(),
                flow: Flow::Capture,
            },
        });
        for (a, out, kind) in [
            ("discord", "mic", ErrorKind::Invalid),
            ("discord", "nowhere", ErrorKind::NotFound),
            ("nobody", "speakers", ErrorKind::NotFound),
        ] {
            let Started::Done(Err(e)) = g.route(a, &[out]) else {
                panic!("{a} → {out}")
            };
            assert_eq!(e.kind, kind, "{a} → {out}: {}", e.message);
        }
        // No driver; a Windows without per-app outputs.
        g.state.observe(Observation::Driver(DriverStatus::Missing(
            "the soundboard driver isn't installed".into(),
        )));
        let Started::Done(Err(e)) = g.route("discord", &["speakers"]) else {
            panic!()
        };
        assert!(e.message.contains("isn't installed"));
        g.state.observe(Observation::Driver(DriverStatus::Ready));
        g.state.observe(Observation::AppRouting(false));
        let Started::Done(Err(e)) = g.route("discord", &["speakers"]) else {
            panic!()
        };
        assert_eq!(e.kind, ErrorKind::Unsupported);
        assert!(g.driver.try_recv().is_err(), "the driver was never asked");
    }

    #[test]
    fn an_endpoint_the_driver_lost_is_enabled_again_once() {
        let mut g = rig();
        g.route("discord", &["headphones"]);
        let (op, ..) = g.drive();
        g.r.driver_done(&mut g.state, op, Ok(()));
        g.pointed();
        g.r.win_done(&mut g.state, op, Ok(()));
        // The driver restarted: its pool is all disabled.
        g.state.observe(endpoint(1, false));
        g.r.sync(&g.state);
        let (op, id, enabled) = g.drive();
        assert_eq!((op, id, enabled), (0, 1, true));
        g.r.sync(&g.state);
        assert!(g.driver.try_recv().is_err(), "once");
        // Back, so Discord's processes are pointed again (pid 7 is still marked done).
        assert!(g.win.try_recv().is_err());
    }
}
