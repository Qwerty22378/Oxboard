//! The state actor: the one owner of `AppState`.
//!
//! Everything reaches it as a [`Msg`] on one queue (UI requests, observations, and later
//! worker results and engine events), so there are no locks around state and no races
//! between, say, a UI command and a device notification. Each message is handled quickly:
//! update state, (later) dispatch jobs to workers, publish the change. Nothing here waits on
//! slow work.
//!
//! After every message the actor projects the state to a view, diffs it against the last one
//! published, and broadcasts the diff with the next sequence number. Replies that finish a
//! command go out *after* that diff, so a UI seeing `Completed` already has the new state.
//!
//! It also recompiles the graph and works out what the engine needs: nothing, a few params,
//! or a new graph (with the next `graph_version`), and hands that to the engine link
//! ([`crate::engine_link`]). Without a link (tests), [`Actor::engine_updates`] collects it.
//! App levels changed in desired state go to the Windows side the same way ([`WinMsg`]).
//!
//! Desired state that changed goes to the persistence worker ([`crate::persist`]), which
//! saves it a moment later; the outcome comes back as [`Msg::Saved`] and shows in the view.
//!
//! A device the engine loses while Windows still lists it (held exclusively, a driver
//! hiccup) is left out of the graph and tried again after a wait that doubles each time,
//! from 1 s to 30 s. The output clock moves to another output meanwhile.

use crate::commands;
use crate::compile::{Compiled, Continuity, Update, compile};
use crate::defaults::Defaults;
use crate::driver::DriverJob;
use crate::engine_link::{EngineReport, LinkMsg};
use crate::hub::{ClientId, Hub};
use crate::persist::SaveMsg;
use crate::routing::{Routing, Started};
use crate::state::{AppState, Desired, Failed, Observation};
use crate::view::project;
use crate::watch::WinMsg;
use sb_shared::events::EngineEvent;
use sb_shared::graph::{ClipAction, GraphError, NodeId};
use sb_shared::ui::{
    AppKey, ClipId, Command, EngineStatus, ErrorInfo, ErrorKind, Level, StateView, UiMessage, UiRequest, diff,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// The first wait before trying a lost device again; it doubles per loss up to [`RETRY_MAX`].
pub const RETRY_FIRST: Duration = Duration::from_secs(1);
pub const RETRY_MAX: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum Msg {
    /// A UI finished the handshake (it's in the hub already).
    UiConnected {
        client: ClientId,
        name: String,
        /// Its process (to let it take the foreground).
        pid: u32,
    },
    Ui {
        client: ClientId,
        req: UiRequest,
    },
    UiDisconnected {
        client: ClientId,
    },
    Observed(Observation),
    /// Several at once (a scan of Windows), published as one change.
    ObservedAll(Vec<Observation>),
    /// From the engine link.
    Engine(EngineReport),
    /// A driver job finished (`op` from the job).
    DriverDone {
        op: u64,
        result: Result<(), String>,
    },
    /// A COM-thread job finished.
    WinDone {
        op: u64,
        result: Result<(), String>,
    },
    /// The persistence worker wrote the settings (or couldn't).
    Saved(Result<(), String>),
    /// The overlay hotkey (or the tray icon): show or hide the overlay.
    Hotkey,
    /// A command from the controller itself (the tray menu); no reply.
    Local(Command),
    /// Stop the actor (the controller is exiting).
    Stop,
}

/// How a message gets to the actor.
pub type Inbox = Sender<Msg>;

pub struct Actor {
    state: AppState,
    /// The view as last published.
    view: StateView,
    seq: u64,
    hub: Arc<Hub>,
    /// Replies held until this message's diff is out.
    after_diff: Vec<(ClientId, UiMessage)>,
    log: bool,
    started: Instant,
    continuity: Continuity,
    /// The graph as last compiled.
    compiled: Compiled,
    engine_updates: Vec<EngineUpdate>,
    engine: Option<Sender<LinkMsg>>,
    windows: Option<Sender<WinMsg>>,
    driver: Option<Sender<DriverJob>>,
    routing: Routing,
    /// Windows' default output (Interface 1, M6).
    defaults: Defaults,
    /// App levels as last sent to Windows.
    sent_levels: BTreeMap<AppKey, Level>,
    persist: Option<Sender<SaveMsg>>,
    /// Desired state as last sent to be saved.
    saved: Desired,
    /// Connected UIs: what they call themselves, and their process.
    uis: BTreeMap<ClientId, (String, u32)>,
}

/// What the engine has to be told, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineUpdate {
    Graph { version: u64, compiled: Compiled },
    Params(Vec<(sb_shared::graph::ParamId, f32)>),
}

impl Actor {
    pub fn new(state: AppState, hub: Arc<Hub>) -> Self {
        let view = project(&state);
        let mut a = Self {
            state,
            view,
            seq: 0,
            hub,
            after_diff: Vec::new(),
            log: false,
            started: Instant::now(),
            continuity: Continuity::default(),
            compiled: Compiled::default(),
            engine_updates: Vec::new(),
            engine: None,
            windows: None,
            driver: None,
            routing: Routing::default(),
            defaults: Defaults::default(),
            sent_levels: BTreeMap::new(),
            persist: None,
            saved: Desired::default(),
            uis: BTreeMap::new(),
        };
        a.recompile();
        a
    }

    /// Drive an engine through this link: it gets the current graph now, then every update.
    pub fn with_engine(mut self, link: Sender<LinkMsg>) -> Self {
        self.engine_updates.clear();
        self.state.graph_version += 1;
        let _ = link.send(LinkMsg::Update(EngineUpdate::Graph {
            version: self.state.graph_version,
            compiled: self.compiled.clone(),
        }));
        self.engine = Some(link);
        self
    }

    /// Apply app levels through the Windows side (the COM thread): the current ones now, then
    /// every change.
    pub fn with_windows(mut self, win: Sender<WinMsg>) -> Self {
        self.routing.set_windows(win.clone());
        self.windows = Some(win);
        self.sync_windows();
        self
    }

    /// Hold Windows' default output for Interface 1 (through the COM thread, so after
    /// [`Actor::with_windows`]), remembering the user's in `marker` (see `crate::defaults`).
    pub fn with_default_output(mut self, marker: Option<std::path::PathBuf>) -> Self {
        if let Some(win) = &self.windows {
            self.defaults = Defaults::new(win.clone(), marker);
            self.publish();
        }
        self
    }

    /// Manage the driver's endpoint pool through this worker (Interface 2).
    pub fn with_driver(mut self, driver: Sender<DriverJob>) -> Self {
        self.driver = Some(driver.clone());
        self.routing.set_driver(driver);
        self
    }

    /// Save desired state through this worker whenever it changes (it was just loaded, so it's
    /// on disk already).
    pub fn with_persist(mut self, persist: Sender<SaveMsg>) -> Self {
        self.saved = self.state.desired.clone();
        self.persist = Some(persist);
        self
    }

    /// Updates for the engine since the last call, oldest first (when there's no link).
    pub fn engine_updates(&mut self) -> Vec<EngineUpdate> {
        std::mem::take(&mut self.engine_updates)
    }

    pub fn compiled(&self) -> &Compiled {
        &self.compiled
    }

    /// Print every message handled to stderr: the ordered log of everything that happened.
    pub fn with_log(mut self, log: bool) -> Self {
        self.log = log;
        self
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// Handle messages until [`Msg::Stop`] or until every sender is gone. Returns the final
    /// state (for the clean-exit work: saving, restoring defaults).
    pub fn run(mut self, inbox: Receiver<Msg>) -> AppState {
        loop {
            let wake = [self.next_retry(), self.defaults.next_deadline()]
                .into_iter()
                .flatten()
                .min();
            let msg = match wake {
                Some(at) => match inbox.recv_timeout(at.saturating_duration_since(Instant::now())) {
                    Ok(m) => m,
                    Err(RecvTimeoutError::Timeout) => {
                        self.retry_due(Instant::now());
                        // The default output's grace may be over.
                        self.publish();
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                None => match inbox.recv() {
                    Ok(m) => m,
                    Err(_) => break,
                },
            };
            if matches!(msg, Msg::Stop) {
                self.trace(&msg);
                break;
            }
            self.handle(msg);
        }
        self.give_back_defaults(&inbox);
        if let Some(w) = &self.windows {
            let _ = w.send(WinMsg::Stop);
        }
        if let Some(d) = &self.driver {
            let _ = d.send(DriverJob::Stop);
        }
        // The last change was sent with its message; this writes it now.
        if let Some(p) = &self.persist {
            let _ = p.send(SaveMsg::Stop);
        }
        self.state
    }

    /// The overlay hotkey: every overlay may take the foreground (it didn't get the input, we
    /// did), then is told to show or hide.
    fn toggle_overlay(&mut self) {
        let overlays: Vec<(ClientId, u32)> = self
            .uis
            .iter()
            .filter(|(_, (name, _))| name == "overlay")
            .map(|(c, (_, pid))| (*c, *pid))
            .collect();
        if overlays.is_empty() && self.log {
            eprintln!("             hotkey, but no overlay is connected");
        }
        for (client, pid) in overlays {
            #[cfg(windows)]
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(pid);
            }
            #[cfg(not(windows))]
            let _ = pid;
            self.hub.send(client, UiMessage::ToggleOverlay);
        }
    }

    /// On the way out: hand Windows' default output back, and wait (a little) for it to be
    /// done, so the PC isn't left playing into a cable nobody reads.
    fn give_back_defaults(&mut self, inbox: &Receiver<Msg>) {
        let Some(op) = self.defaults.give_back() else { return };
        let deadline = Instant::now() + Duration::from_secs(3);
        while let Ok(m) = inbox.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            if let Msg::WinDone { op: done, result } = m
                && done == op
            {
                self.defaults.done(op, result);
                return;
            }
        }
        eprintln!("controller: Windows didn't confirm giving the default output back");
    }

    /// When the next failed device is due to be tried again.
    pub fn next_retry(&self) -> Option<Instant> {
        self.state.observed.failed.values().filter_map(|f| f.retry_at).min()
    }

    /// Put failed devices whose wait is over back into the graph.
    pub fn retry_due(&mut self, now: Instant) {
        let mut any = false;
        for (key, f) in &mut self.state.observed.failed {
            if f.retry_at.is_some_and(|t| t <= now) {
                f.retry_at = None;
                any = true;
                if self.log {
                    eprintln!("             trying {} again", key.0);
                }
            }
        }
        if any {
            self.publish();
        }
    }

    pub fn handle(&mut self, msg: Msg) {
        self.trace(&msg);
        match msg {
            Msg::UiConnected { client, name, pid } => {
                self.uis.insert(client, (name, pid));
            }
            Msg::Hotkey => self.toggle_overlay(),
            Msg::Local(cmd) => {
                if let Err(e) = self.command(cmd) {
                    eprintln!("controller: {e}");
                }
            }
            Msg::Ui {
                client,
                req: UiRequest::Subscribe,
            } => {
                // The snapshot is what's been published so far; the next diff is seq + 1.
                // Only the actor sends state, so nothing can slip in between the two.
                self.hub.send(
                    client,
                    UiMessage::Snapshot {
                        seq: self.seq,
                        view: Box::new(self.view.clone()),
                    },
                );
                self.hub.subscribe(client);
            }
            Msg::Ui {
                client,
                req: UiRequest::Command { id, cmd },
            } => {
                self.hub.send(client, UiMessage::Accepted(id));
                let started = match cmd {
                    Command::RouteApp { app, outputs } => {
                        self.routing.route_app(&mut self.state, client, id, app, outputs)
                    }
                    cmd => Started::Done(self.command(cmd)),
                };
                if let Started::Done(result) = started {
                    let reply = match result {
                        Ok(()) => UiMessage::Completed(id),
                        Err(e) => UiMessage::Failed(id, e),
                    };
                    self.after_diff.push((client, reply));
                }
            }
            Msg::UiDisconnected { client } => {
                self.uis.remove(&client);
                self.hub.remove(client);
            }
            Msg::Observed(o) => self.state.observe(o),
            Msg::ObservedAll(all) => all.into_iter().for_each(|o| self.state.observe(o)),
            Msg::Engine(r) => self.engine_report(r),
            Msg::DriverDone { op, result } => {
                if let Some(reply) = self.routing.driver_done(&mut self.state, op, result) {
                    self.after_diff.push(reply);
                }
            }
            Msg::WinDone { op, result } => {
                // The default output's jobs, else routing's.
                if !self.defaults.done(op, result.clone())
                    && let Some(reply) = self.routing.win_done(&mut self.state, op, result)
                {
                    self.after_diff.push(reply);
                }
            }
            Msg::Saved(result) => {
                let error = result.err();
                if self.log
                    && let Some(e) = &error
                {
                    eprintln!("             can't save the settings: {e}");
                }
                self.state.observed.settings.save_error = error;
            }
            Msg::Stop => {}
        }
        self.publish();
    }

    fn command(&mut self, cmd: Command) -> Result<(), ErrorInfo> {
        match cmd {
            Command::PlayClip { id } => self.trigger(id, ClipAction::Play),
            Command::StopClip { id } => self.trigger(id, ClipAction::Stop),
            Command::RestartEngine => match &self.engine {
                Some(link) => {
                    let _ = link.send(LinkMsg::Restart);
                    Ok(())
                }
                None => Err(ErrorInfo::new(
                    ErrorKind::Unavailable,
                    "this controller runs without an engine",
                )),
            },
            Command::RemoveClip { id } => {
                commands::apply(&mut self.state, cmd)?;
                self.state.observed.playing.remove(&id);
                Ok(())
            }
            cmd => commands::apply(&mut self.state, cmd),
        }
    }

    fn trigger(&mut self, id: ClipId, action: ClipAction) -> Result<(), ErrorInfo> {
        let unavailable = |m: &str| Err(ErrorInfo::new(ErrorKind::Unavailable, m));
        if !self.state.desired.clips.contains_key(&id) {
            return Err(ErrorInfo::new(ErrorKind::NotFound, format!("no clip {}", id.0)));
        }
        let Some(link) = &self.engine else {
            return unavailable("this controller runs without an engine");
        };
        if !matches!(self.state.observed.engine, EngineStatus::Running { .. }) {
            return unavailable("the engine isn't running");
        }
        // Every clip has a node (compiled in the same message that added it, sent before this).
        let Some(&node) = self.compiled.clips.get(&id) else {
            return unavailable("the clip isn't in the engine's graph");
        };
        let _ = link.send(LinkMsg::Trigger { node, action });
        if action == ClipAction::Play {
            self.state.observed.playing.insert(id);
        }
        Ok(())
    }

    fn engine_report(&mut self, r: EngineReport) {
        match r {
            EngineReport::Status(s) => {
                if !matches!(s, EngineStatus::Running { .. }) {
                    // A new host starts silent.
                    self.state.observed.playing.clear();
                }
                self.state.observed.engine = s;
            }
            EngineReport::Event(EngineEvent::ClipEnded { node, .. }) => {
                if let Some((id, _)) = self.compiled.clips.iter().find(|(_, n)| **n == node) {
                    self.state.observed.playing.remove(id);
                }
            }
            EngineReport::Event(EngineEvent::DeviceLost { node }) => {
                self.device_failed(node, "lost");
            }
            // Xrun, MasterClockFallback: facts for the log. (The master moves on its own when
            // its device fails: the graph leaves the device out and picks another clock.)
            EngineReport::Event(_) => {}
            // A device that won't open (Windows lists it, but it's held exclusively, or
            // broken) makes the engine refuse the whole graph; the last one keeps playing.
            // Leave the device out like a lost one, and the next graph goes through.
            EngineReport::Refused {
                error: GraphError::Endpoint { node, ref reason },
                ..
            } if self.compiled.devices.contains_key(&node) => {
                let why = format!("won't open ({reason})");
                self.device_failed(node, &why);
            }
            EngineReport::Refused { version, error } => {
                eprintln!("controller: BUG: the engine refused graph v{version}: {error}");
            }
        }
    }

    /// The engine couldn't use a device node's device: out of the graph, tried again later.
    fn device_failed(&mut self, node: NodeId, why: &str) {
        let Some(key) = self.compiled.devices.get(&node) else {
            return;
        };
        let f = self.state.observed.failed.entry(key.clone()).or_insert(Failed {
            attempts: 0,
            retry_at: None,
        });
        // Again while out of the graph: about an old graph.
        if f.retry_at.is_none() {
            f.attempts += 1;
            let wait = RETRY_FIRST.saturating_mul(1 << (f.attempts - 1).min(8)).min(RETRY_MAX);
            f.retry_at = Some(Instant::now() + wait);
            if self.log {
                eprintln!("             {} {why}; trying again in {wait:?}", key.0);
            }
        }
    }

    fn recompile(&mut self) {
        let next = match compile(&self.state, &mut self.continuity) {
            Ok(c) => c,
            Err(e) => {
                // The last graph keeps playing; the command that got here already succeeded,
                // so this only happens with thousands of connections.
                eprintln!("controller: can't compile the graph: {e}");
                return;
            }
        };
        let update = match next.update_from(&self.compiled) {
            Update::None => return,
            Update::Params(p) => EngineUpdate::Params(p),
            Update::Graph => {
                self.state.graph_version += 1;
                EngineUpdate::Graph {
                    version: self.state.graph_version,
                    compiled: next.clone(),
                }
            }
        };
        if self.log {
            match &update {
                EngineUpdate::Graph { version, compiled } => eprintln!(
                    "             graph v{version}: {} nodes, {} edges, {} meters, master {:?}",
                    compiled.graph.nodes.len(),
                    compiled.graph.edges.len(),
                    compiled.graph.meters.len(),
                    compiled.graph.master
                ),
                EngineUpdate::Params(p) => eprintln!("             params: {p:?}"),
            }
        }
        // Until the engine link drains them, keep only what's needed to catch up: the last
        // graph and the params after it.
        match (update, self.engine_updates.last_mut()) {
            (u @ EngineUpdate::Graph { .. }, _) => {
                self.engine_updates.clear();
                self.engine_updates.push(u);
            }
            (EngineUpdate::Params(new), Some(EngineUpdate::Params(old))) => {
                for (id, v) in new {
                    match old.iter_mut().find(|(o, _)| *o == id) {
                        Some(slot) => slot.1 = v,
                        None => old.push((id, v)),
                    }
                }
            }
            (u, _) => self.engine_updates.push(u),
        }
        self.compiled = next;
    }

    fn sync_windows(&mut self) {
        let Some(win) = &self.windows else { return };
        for (app, level) in &self.state.desired.app_levels {
            if self.sent_levels.get(app) != Some(level) {
                let _ = win.send(WinMsg::SetAppLevel {
                    app: app.clone(),
                    level: *level,
                });
                self.sent_levels.insert(app.clone(), *level);
            }
        }
    }

    fn publish(&mut self) {
        self.recompile();
        if let Some(link) = &self.engine {
            for u in self.engine_updates.drain(..) {
                let _ = link.send(LinkMsg::Update(u));
            }
        }
        self.sync_windows();
        self.routing.sync(&self.state);
        self.defaults.sync(&self.state, Instant::now());
        self.state.observed.default_output = self.defaults.status(&self.state);
        if let Some(p) = &self.persist
            && self.state.desired != self.saved
        {
            self.saved = self.state.desired.clone();
            let _ = p.send(SaveMsg::Save(Box::new(self.saved.clone())));
        }
        let view = project(&self.state);
        let ops = diff(&self.view, &view);
        if !ops.is_empty() {
            self.seq += 1;
            self.view = view;
            self.hub.broadcast(&UiMessage::Diff { seq: self.seq, ops });
        }
        for (client, msg) in self.after_diff.drain(..) {
            self.hub.send(client, msg);
        }
    }

    fn trace(&self, msg: &Msg) {
        if self.log {
            eprintln!("[{:>10.3}] {msg:?}", self.started.elapsed().as_secs_f64());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::DeviceInfo;
    use sb_shared::ui::{Command, DeviceKey, ErrorKind, Flow, Mirror, Synced};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, Receiver};

    struct FakeUi {
        rx: Receiver<UiMessage>,
        closed: Arc<AtomicBool>,
    }

    impl FakeUi {
        fn drain(&self) -> Vec<UiMessage> {
            self.rx.try_iter().collect()
        }
    }

    fn connect(hub: &Hub, id: ClientId, queue: usize) -> FakeUi {
        let (tx, rx) = mpsc::sync_channel(queue);
        let closed = Arc::new(AtomicBool::new(false));
        let c = closed.clone();
        hub.add(id, tx, Arc::new(move || c.store(true, Ordering::SeqCst)));
        FakeUi { rx, closed }
    }

    fn speakers() -> DeviceKey {
        DeviceKey("{0.0.0.00000000}.{speakers}".into())
    }

    fn with_speakers() -> (Actor, Arc<Hub>) {
        let hub = Arc::new(Hub::default());
        let mut a = Actor::new(AppState::default(), hub.clone());
        a.handle(Msg::Observed(Observation::DeviceAdded {
            key: speakers(),
            info: DeviceInfo {
                name: "Speakers".into(),
                flow: Flow::Render,
            },
        }));
        (a, hub)
    }

    fn cmd(client: ClientId, id: u64, cmd: Command) -> Msg {
        Msg::Ui {
            client,
            req: UiRequest::Command { id, cmd },
        }
    }

    fn subscribe(client: ClientId) -> Msg {
        Msg::Ui {
            client,
            req: UiRequest::Subscribe,
        }
    }

    #[test]
    fn a_command_is_accepted_then_diffed_then_completed() {
        let (mut a, hub) = with_speakers();
        let ui = connect(&hub, 1, 64);
        a.handle(subscribe(1));
        let select = Command::SelectOutput {
            device: speakers(),
            selected: true,
        };
        a.handle(cmd(1, 42, select));
        let msgs = ui.drain();
        assert!(matches!(&msgs[0], UiMessage::Snapshot { seq: 1, view } if view.devices.len() == 1));
        assert_eq!(msgs[1], UiMessage::Accepted(42));
        assert!(matches!(&msgs[2], UiMessage::Diff { seq: 2, ops } if ops.len() == 1));
        assert_eq!(msgs[3], UiMessage::Completed(42));
        assert_eq!(msgs.len(), 4);
    }

    #[test]
    fn a_failed_command_changes_nothing_and_says_why() {
        let (mut a, hub) = with_speakers();
        let ui = connect(&hub, 1, 64);
        a.handle(subscribe(1));
        ui.drain();
        a.handle(cmd(1, 7, Command::SetMasterGain(f32::NAN)));
        let msgs = ui.drain();
        assert_eq!(msgs[0], UiMessage::Accepted(7));
        assert!(matches!(&msgs[1], UiMessage::Failed(7, e) if e.kind == ErrorKind::Invalid));
        assert_eq!(msgs.len(), 2, "no diff");
    }

    #[test]
    fn replies_go_to_the_sender_and_state_to_every_subscriber() {
        let (mut a, hub) = with_speakers();
        let (one, two, quiet) = (connect(&hub, 1, 64), connect(&hub, 2, 64), connect(&hub, 3, 64));
        a.handle(subscribe(1));
        a.handle(subscribe(2));
        a.handle(cmd(1, 5, Command::SetMasterMuted(true)));
        a.handle(cmd(3, 9, Command::SetMasterGain(0.5)));
        let (mut m1, mut m2) = (Mirror::default(), Mirror::default());
        let msgs1 = one.drain();
        let msgs2 = two.drain();
        for m in &msgs1 {
            assert_ne!(m1.apply(m), Synced::Gap);
        }
        for m in &msgs2 {
            assert_ne!(m2.apply(m), Synced::Gap);
        }
        assert!(msgs1.contains(&UiMessage::Completed(5)) && !msgs2.contains(&UiMessage::Completed(5)));
        assert_eq!(quiet.drain(), vec![UiMessage::Accepted(9), UiMessage::Completed(9)]);
        assert_eq!(m1.view, project(a.state()));
        assert_eq!(m2.view, m1.view);
        assert!(m1.view.master.muted && m1.view.master.gain == 0.5);
    }

    #[test]
    fn late_subscribers_get_the_current_snapshot_then_continue_the_sequence() {
        let (mut a, hub) = with_speakers();
        let early = connect(&hub, 1, 64);
        a.handle(subscribe(1));
        for i in 0..5 {
            a.handle(cmd(1, i, Command::SetMasterGain(i as f32 / 10.0)));
        }
        let late = connect(&hub, 2, 64);
        a.handle(subscribe(2));
        a.handle(cmd(1, 99, Command::SetMasterMuted(true)));
        let mut m = Mirror::default();
        for msg in late.drain() {
            assert_ne!(m.apply(&msg), Synced::Gap);
        }
        assert_eq!(m.view, project(a.state()));
        let mut e = Mirror::default();
        for msg in early.drain() {
            e.apply(&msg);
        }
        assert_eq!((e.seq, &e.view), (m.seq, &m.view));
    }

    #[test]
    fn a_stuck_ui_is_dropped_and_the_others_carry_on() {
        let (mut a, hub) = with_speakers();
        let fine = connect(&hub, 1, 1024);
        let stuck = connect(&hub, 2, 4);
        a.handle(subscribe(1));
        a.handle(subscribe(2));
        for i in 0..20 {
            a.handle(cmd(1, i, Command::SetMasterGain(i as f32 / 20.0)));
        }
        assert!(stuck.closed.load(Ordering::SeqCst), "closed when its queue filled");
        assert!(!fine.closed.load(Ordering::SeqCst));
        assert_eq!(hub.len(), 1);
        let mut m = Mirror::default();
        for msg in fine.drain() {
            assert_ne!(m.apply(&msg), Synced::Gap);
        }
        assert_eq!(m.view.master.gain, 19.0 / 20.0);
        // Its reader notices the close and reports it; that's harmless now.
        a.handle(Msg::UiDisconnected { client: 2 });
    }

    fn mic() -> DeviceKey {
        DeviceKey("{0.0.1.00000000}.{mic}".into())
    }

    fn game() -> AppKey {
        AppKey(r"C:\Games\game.exe".into())
    }

    fn level(gain: f32) -> Level {
        Level { gain, muted: false }
    }

    #[test]
    fn app_levels_go_to_windows_and_changes_made_there_are_followed() {
        let (a, _hub) = with_speakers();
        let (tx, rx) = mpsc::channel();
        let mut a = a.with_windows(tx);
        a.handle(Msg::ObservedAll(vec![
            Observation::SessionAdded {
                app: game(),
                name: "Game".into(),
                pid: 7,
            },
            Observation::AppLevel {
                app: game(),
                level: level(0.4),
            },
        ]));
        // Windows' own level shows, and nothing is sent back: the user hasn't set one here.
        assert_eq!(project(a.state()).apps[&game()].level, level(0.4));
        assert!(rx.try_recv().is_err());
        a.handle(cmd(
            1,
            1,
            Command::SetAppGain {
                app: game(),
                gain: 0.25,
            },
        ));
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            [WinMsg::SetAppLevel {
                app: game(),
                level: level(0.25)
            }]
        );
        // Above 1 isn't a session volume.
        a.handle(cmd(1, 2, Command::SetAppGain { app: game(), gain: 1.5 }));
        assert_eq!(a.state().desired.app_levels[&game()], level(0.25));
        // The user drags the app in the Windows mixer: that's the level now.
        a.handle(Msg::Observed(Observation::AppLevel {
            app: game(),
            level: level(0.9),
        }));
        assert_eq!(a.state().desired.app_levels[&game()], level(0.9));
        assert_eq!(project(a.state()).apps[&game()].level, level(0.9));
        // Saying it back is harmless (Windows has it already) and keeps the queue in order
        // behind any fader move still on its way.
        assert_eq!(rx.try_iter().count(), 1);
    }

    #[test]
    fn a_lost_device_leaves_the_graph_and_is_tried_again_later_and_later() {
        let (mut a, _hub) = with_speakers();
        a.handle(Msg::Observed(Observation::DeviceAdded {
            key: mic(),
            info: DeviceInfo {
                name: "Mic".into(),
                flow: Flow::Capture,
            },
        }));
        a.handle(cmd(
            1,
            1,
            Command::SetSystemInput(Some(sb_shared::ui::SystemInput::Device(mic()))),
        ));
        a.handle(cmd(
            1,
            2,
            Command::SelectOutput {
                device: speakers(),
                selected: true,
            },
        ));
        let node = *a.compiled().devices.iter().find(|(_, k)| **k == speakers()).unwrap().0;
        let lose = |a: &mut Actor| a.handle(Msg::Engine(EngineReport::Event(EngineEvent::DeviceLost { node })));
        lose(&mut a);
        assert!(
            !a.compiled().devices.values().any(|k| *k == speakers()),
            "out of the graph"
        );
        assert!(project(a.state()).devices[&speakers()].failed);
        let first = a.next_retry().unwrap();
        let wait = first - Instant::now();
        assert!(wait > Duration::from_millis(900) && wait <= RETRY_FIRST, "{wait:?}");
        // A late event about the same node changes nothing while it's out.
        lose(&mut a);
        assert_eq!(a.next_retry(), Some(first));

        a.retry_due(first);
        assert!(a.compiled().devices.values().any(|k| *k == speakers()), "back in");
        assert!(!project(a.state()).devices[&speakers()].failed);
        assert_eq!(a.next_retry(), None);
        let node = *a.compiled().devices.iter().find(|(_, k)| **k == speakers()).unwrap().0;
        a.handle(Msg::Engine(EngineReport::Event(EngineEvent::DeviceLost { node })));
        let wait = a.next_retry().unwrap() - Instant::now();
        assert!(wait > Duration::from_millis(1900), "doubled: {wait:?}");

        // Unplugged and plugged back in: a fresh start.
        a.handle(Msg::Observed(Observation::DeviceRemoved { key: speakers() }));
        a.handle(Msg::Observed(Observation::DeviceAdded {
            key: speakers(),
            info: DeviceInfo {
                name: "Speakers".into(),
                flow: Flow::Render,
            },
        }));
        assert_eq!(a.next_retry(), None);
        assert!(a.compiled().devices.values().any(|k| *k == speakers()));
    }

    #[test]
    fn a_graph_refused_over_a_device_is_sent_again_without_it() {
        let (mut a, _hub) = with_speakers();
        a.handle(Msg::Observed(Observation::DeviceAdded {
            key: mic(),
            info: DeviceInfo {
                name: "Mic".into(),
                flow: Flow::Capture,
            },
        }));
        a.handle(cmd(
            1,
            1,
            Command::SetSystemInput(Some(sb_shared::ui::SystemInput::Device(mic()))),
        ));
        a.handle(cmd(
            1,
            2,
            Command::SelectOutput {
                device: speakers(),
                selected: true,
            },
        ));
        a.engine_updates();
        let node = *a.compiled().devices.iter().find(|(_, k)| **k == speakers()).unwrap().0;
        a.handle(Msg::Engine(EngineReport::Refused {
            version: a.state().graph_version,
            error: GraphError::Endpoint {
                node,
                reason: "Element not found.".into(),
            },
        }));
        assert!(project(a.state()).devices[&speakers()].failed);
        let updates = a.engine_updates();
        assert!(
            matches!(&updates[..], [EngineUpdate::Graph { compiled, .. }] if !compiled.devices.values().any(|k| *k == speakers())),
            "a new graph without it: {updates:?}"
        );
        assert!(a.next_retry().is_some());
        // A refusal that names no device is still a bug, and changes nothing.
        a.handle(Msg::Engine(EngineReport::Refused {
            version: 9,
            error: GraphError::UnknownNode { edge: 0, node },
        }));
        assert!(a.engine_updates().is_empty());
    }

    #[test]
    fn changed_settings_are_sent_to_be_saved_and_failures_shown() {
        let (a, hub) = with_speakers();
        let (tx, rx) = mpsc::channel();
        let mut a = a.with_persist(tx);
        let ui = connect(&hub, 1, 64);
        a.handle(subscribe(1));
        a.handle(cmd(1, 1, Command::SetMasterGain(0.5)));
        let saves: Vec<_> = rx.try_iter().collect();
        assert!(
            matches!(&saves[..], [SaveMsg::Save(d)] if d.master.gain == 0.5 && **d == a.state().desired),
            "{saves:?}"
        );
        // What Windows shows isn't a setting; a failed command changes nothing.
        a.handle(Msg::Observed(Observation::DeviceRemoved { key: speakers() }));
        a.handle(cmd(1, 2, Command::SetMasterGain(9.0)));
        assert!(rx.try_recv().is_err());

        ui.drain();
        a.handle(Msg::Saved(Err("disk full".into())));
        a.handle(Msg::Saved(Err("disk full".into())));
        let msgs = ui.drain();
        assert_eq!(msgs.len(), 1, "said once: {msgs:?}");
        assert_eq!(project(a.state()).settings.save_error.as_deref(), Some("disk full"));
        a.handle(Msg::Saved(Ok(())));
        assert_eq!(project(a.state()).settings.save_error, None);
    }

    #[test]
    fn a_load_error_shows_until_dismissed() {
        let mut state = AppState::default();
        state.observed.settings.load_error = Some(sb_shared::ui::LoadError {
            reason: "line 3".into(),
            backup: Some("config.toml.bad-1".into()),
        });
        let mut a = Actor::new(state, Arc::new(Hub::default()));
        assert!(project(a.state()).settings.load_error.is_some());
        a.handle(cmd(1, 1, Command::DismissLoadError));
        assert_eq!(project(a.state()).settings.load_error, None);
    }

    #[test]
    fn observations_reach_subscribers_as_diffs() {
        let (mut a, hub) = with_speakers();
        let ui = connect(&hub, 1, 64);
        a.handle(subscribe(1));
        a.handle(Msg::Observed(Observation::DeviceRemoved { key: speakers() }));
        let msgs = ui.drain();
        assert!(matches!(&msgs[1], UiMessage::Diff { ops, .. } if ops.len() == 1));
        // A no-op observation sends nothing.
        a.handle(Msg::Observed(Observation::DeviceRemoved { key: speakers() }));
        assert!(ui.drain().is_empty());
    }
}
