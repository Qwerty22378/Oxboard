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
//! or a new graph (with the next `graph_version`). Sending that is the engine link's job (C3);
//! until then [`Actor::engine_updates`] collects it.

use crate::commands;
use crate::compile::{Compiled, Continuity, Update, compile};
use crate::hub::{ClientId, Hub};
use crate::state::{AppState, Observation};
use crate::view::project;
use sb_shared::ui::{StateView, UiMessage, UiRequest, diff};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

#[derive(Debug)]
pub enum Msg {
    /// A UI finished the handshake (it's in the hub already).
    UiConnected {
        client: ClientId,
        name: String,
    },
    Ui {
        client: ClientId,
        req: UiRequest,
    },
    UiDisconnected {
        client: ClientId,
    },
    Observed(Observation),
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
        };
        a.recompile();
        a
    }

    /// Updates for the engine since the last call, oldest first.
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
        for msg in inbox {
            if matches!(msg, Msg::Stop) {
                self.trace(&msg);
                break;
            }
            self.handle(msg);
        }
        self.state
    }

    pub fn handle(&mut self, msg: Msg) {
        self.trace(&msg);
        match msg {
            Msg::UiConnected { .. } => {}
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
                        view: self.view.clone(),
                    },
                );
                self.hub.subscribe(client);
            }
            Msg::Ui {
                client,
                req: UiRequest::Command { id, cmd },
            } => {
                self.hub.send(client, UiMessage::Accepted(id));
                let reply = match commands::apply(&mut self.state, cmd) {
                    Ok(()) => UiMessage::Completed(id),
                    Err(e) => UiMessage::Failed(id, e),
                };
                self.after_diff.push((client, reply));
            }
            Msg::UiDisconnected { client } => {
                self.hub.remove(client);
            }
            Msg::Observed(o) => self.state.observe(o),
            Msg::Stop => {}
        }
        self.publish();
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

    fn publish(&mut self) {
        self.recompile();
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
