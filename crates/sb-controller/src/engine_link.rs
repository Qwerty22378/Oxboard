//! The engine link: keeps an `sb-engine-host` running the controller's current graph.
//!
//! It runs on its own thread, so the actor never waits on the engine. The actor sends it every
//! [`EngineUpdate`] (and clip triggers); it sends them on to the host and reports back to the
//! actor as [`Msg::Engine`]. Meters skip the actor: the link names them for the UI and hands
//! them to the hub.
//!
//! **Replay.** The link keeps the latest graph with every param update folded into it, so it
//! always holds exactly what the engine should be running. A fresh host gets that one graph
//! and is fully caught up: no param history, no ordering questions.
//!
//! **Supervision** (see `Controller Notes` → Engine supervision):
//! - The host sends a heartbeat every 500 ms. Silence for [`LinkConfig::heartbeat_timeout`]
//!   means it hung: kill it. A host that exits or crashes breaks the pipe, which shows up
//!   within milliseconds.
//! - A dead host is respawned after a backoff that doubles with each crash, and the latest
//!   graph is replayed. The UI sees `Restarting { attempt }`, then `Starting`, then `Running`
//!   once the replayed graph is playing.
//! - After [`LinkConfig::give_up_after`] crashes inside [`LinkConfig::crash_window`] the link
//!   stops trying and reports `Failed`; `Command::RestartEngine` starts it again.
//!
//! A refused graph is reported, not a crash (the host keeps the last one), except a fresh
//! host's first graph failing for anything but a device: then the engine couldn't start.
//!
//! While a graph is being applied the link waits for the host's reply (up to 10 s), not for
//! heartbeats: the host's loop is busy loading the graph's clips then, and a big replay can
//! take longer than the heartbeat timeout.

use crate::actor::{EngineUpdate, Inbox, Msg};
use crate::compile::Compiled;
use crate::hub::Hub;
use sb_shared::engine_client::EngineClient;
use sb_shared::events::EngineEvent;
use sb_shared::graph::{ClipAction, GraphError, NodeId};
use sb_shared::ui::EngineStatus;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// From the actor to the link.
#[derive(Debug)]
pub enum LinkMsg {
    Update(EngineUpdate),
    Trigger {
        node: NodeId,
        action: ClipAction,
    },
    /// Kill the host if there is one and start a fresh one now, forgetting past crashes.
    Restart,
    /// Shut the host down cleanly and end the link.
    Stop,
}

/// From the link to the actor.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineReport {
    Status(EngineStatus),
    /// An engine event, except meters and heartbeats (the link handles those).
    Event(EngineEvent),
    /// The host refused a graph the controller built: a bug (every compiled graph should pass
    /// the engine's rules). The previous graph keeps playing.
    Refused {
        version: u64,
        error: GraphError,
    },
}

/// A started host: the connection to it, and the process (or thread) behind it.
pub struct Spawned {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub process: Box<dyn HostProcess>,
}

pub trait HostProcess: Send {
    /// Give it up to `grace` to exit by itself, then kill it; either way, wait until it's
    /// gone. Returns how it ended, for logs.
    fn end(&mut self, grace: Duration) -> String;
}

/// Starts hosts.
pub trait Spawn: Send + 'static {
    fn spawn(&mut self) -> Result<Spawned, String>;
}

#[derive(Debug, Clone)]
pub struct LinkConfig {
    /// No word from the host for this long: it's hung.
    pub heartbeat_timeout: Duration,
    /// The wait before the first restart; doubled for each crash since, up to `backoff_max`.
    pub backoff_first: Duration,
    pub backoff_max: Duration,
    /// Stop restarting after this many crashes inside `crash_window`.
    pub give_up_after: usize,
    pub crash_window: Duration,
    /// How often the link looks for engine events (meters arrive at ~30 Hz).
    pub poll: Duration,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            heartbeat_timeout: Duration::from_secs(2),
            backoff_first: Duration::from_millis(100),
            backoff_max: Duration::from_secs(5),
            give_up_after: 5,
            crash_window: Duration::from_secs(60),
            poll: Duration::from_millis(10),
        }
    }
}

/// Start the link thread. It starts a host once the first graph arrives.
pub fn spawn(
    spawner: Box<dyn Spawn>,
    cfg: LinkConfig,
    hub: Arc<Hub>,
    inbox: Inbox,
) -> (Sender<LinkMsg>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let link = Link {
        spawner,
        cfg,
        hub,
        inbox,
        graph: None,
        phase: Phase::Idle,
        crashes: VecDeque::new(),
        status: EngineStatus::Down,
    };
    let thread = std::thread::Builder::new()
        .name("sb-engine-link".into())
        .spawn(move || link.run(rx))
        .expect("spawn the engine link");
    (tx, thread)
}

struct Host {
    client: EngineClient<Box<dyn Write + Send>>,
    process: Box<dyn HostProcess>,
    last_heard: Instant,
    /// The graph version the mix runs, once it says so.
    applied: Option<u64>,
}

enum Phase {
    /// No graph yet, so nothing to run.
    Idle,
    Up(Host),
    /// Restarting after a crash at `at`.
    Waiting {
        at: Instant,
    },
    /// Gave up; waits for `Restart`.
    Failed,
}

struct Link {
    spawner: Box<dyn Spawn>,
    cfg: LinkConfig,
    hub: Arc<Hub>,
    inbox: Inbox,
    /// The latest graph, with every param update since folded in: what the engine should run.
    graph: Option<(u64, Compiled)>,
    phase: Phase,
    /// Recent crashes, oldest first, inside the crash window.
    crashes: VecDeque<Instant>,
    /// As last reported.
    status: EngineStatus,
}

impl Link {
    fn run(mut self, rx: Receiver<LinkMsg>) {
        loop {
            let wait = match &self.phase {
                Phase::Waiting { at } => at.saturating_duration_since(Instant::now()).min(self.cfg.poll),
                _ => self.cfg.poll,
            };
            match rx.recv_timeout(wait) {
                Ok(LinkMsg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(LinkMsg::Update(u)) => self.update(u),
                Ok(LinkMsg::Trigger { node, action }) => {
                    if let Phase::Up(h) = &mut self.phase
                        && let Err(e) = h.client.trigger(node, action)
                    {
                        self.crashed(e);
                    }
                }
                Ok(LinkMsg::Restart) => {
                    if let Phase::Up(mut h) = std::mem::replace(&mut self.phase, Phase::Idle) {
                        h.process.end(Duration::ZERO);
                    }
                    self.crashes.clear();
                    if self.graph.is_some() {
                        self.start();
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.tick();
        }
        if let Phase::Up(mut h) = std::mem::replace(&mut self.phase, Phase::Idle) {
            let _ = h.client.shutdown();
            h.process.end(Duration::from_secs(2));
        }
    }

    fn report(&mut self, r: EngineReport) {
        if let EngineReport::Status(s) = &r {
            if *s == self.status {
                return;
            }
            self.status = s.clone();
        }
        let _ = self.inbox.send(Msg::Engine(r));
    }

    fn update(&mut self, u: EngineUpdate) {
        match u {
            EngineUpdate::Graph { version, compiled } => {
                self.graph = Some((version, compiled));
                match &self.phase {
                    Phase::Idle => self.start(),
                    Phase::Up(_) => self.apply(),
                    Phase::Waiting { .. } | Phase::Failed => {}
                }
            }
            EngineUpdate::Params(moved) => {
                if let Some((_, c)) = &mut self.graph {
                    for (id, value) in &moved {
                        if let Some(p) = c.graph.params.iter_mut().find(|p| p.id == *id) {
                            p.value = *value;
                        }
                    }
                }
                if let Phase::Up(h) = &mut self.phase {
                    let sent = moved.iter().try_for_each(|&(id, v)| h.client.set_param(id, v));
                    if let Err(e) = sent {
                        self.crashed(e);
                    }
                }
            }
        }
    }

    /// Spawn a host and give it the latest graph.
    fn start(&mut self) {
        self.report(EngineStatus::Starting.into());
        let spawned = self.spawner.spawn();
        let host = spawned.and_then(|s| {
            let mut process = s.process;
            match EngineClient::connect(s.reader, s.writer, "sb-controller") {
                Ok(client) => Ok(Host {
                    client,
                    process,
                    last_heard: Instant::now(),
                    applied: None,
                }),
                Err(e) => {
                    process.end(Duration::ZERO);
                    Err(e)
                }
            }
        });
        match host {
            Ok(h) => {
                self.phase = Phase::Up(h);
                self.apply();
            }
            Err(e) => self.crashed(format!("couldn't start the engine: {e}")),
        }
    }

    /// Send the latest graph to the running host.
    fn apply(&mut self) {
        let (Phase::Up(h), Some((version, c))) = (&mut self.phase, &self.graph) else {
            return;
        };
        let version = *version;
        let fresh = h.applied.is_none();
        match h.client.apply_graph(c.graph.clone(), version) {
            Ok(Ok(())) => h.last_heard = Instant::now(),
            // A host that can't run its first graph can't run anything (the audio side
            // didn't start): count it as a crash. Unless a device in it is to blame: the
            // controller leaves that one out and sends another graph.
            Ok(Err(error)) if fresh && !matches!(error, GraphError::Endpoint { .. }) => {
                self.crashed(format!("the engine can't start: {error}"))
            }
            Ok(Err(error)) => self.report(EngineReport::Refused { version, error }),
            Err(e) => self.crashed(e),
        }
    }

    fn tick(&mut self) {
        match &mut self.phase {
            Phase::Up(h) => {
                let mut events = Vec::new();
                while let Some(e) = h.client.next_event(Duration::ZERO) {
                    h.last_heard = Instant::now();
                    events.push(e);
                }
                for e in events {
                    self.event(e);
                }
                let Phase::Up(h) = &mut self.phase else { return };
                if !h.client.is_connected() {
                    self.crashed("the engine host exited".into());
                } else if h.last_heard.elapsed() > self.cfg.heartbeat_timeout {
                    let silent = h.last_heard.elapsed();
                    self.crashed(format!(
                        "the engine stopped responding ({} ms silent)",
                        silent.as_millis()
                    ));
                }
            }
            Phase::Waiting { at } if Instant::now() >= *at => self.start(),
            _ => {}
        }
    }

    fn event(&mut self, e: EngineEvent) {
        let Phase::Up(h) = &mut self.phase else { return };
        let latest = self.graph.as_ref();
        match e {
            EngineEvent::Heartbeat { .. } => {}
            EngineEvent::Meters(frame) => {
                // Until the latest graph runs, meter IDs may still mean the previous graph's.
                if let Some((v, c)) = latest
                    && h.applied == Some(*v)
                {
                    self.hub.meters(&c.meters_for_ui(&frame));
                }
            }
            EngineEvent::GraphApplied { version } => {
                h.applied = Some(version);
                self.report(
                    EngineStatus::Running {
                        applied_version: version,
                    }
                    .into(),
                );
            }
            other => self.report(EngineReport::Event(other)),
        }
    }

    /// The host died (or is unusable): end it, then restart after a backoff or give up.
    fn crashed(&mut self, reason: String) {
        let how = match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Up(mut h) => format!(" ({})", h.process.end(Duration::from_millis(500))),
            _ => String::new(),
        };
        eprintln!("engine link: {reason}{how}");
        let now = Instant::now();
        self.crashes.push_back(now);
        while self
            .crashes
            .front()
            .is_some_and(|t| now.duration_since(*t) > self.cfg.crash_window)
        {
            self.crashes.pop_front();
        }
        let attempt = self.crashes.len();
        if attempt >= self.cfg.give_up_after {
            self.phase = Phase::Failed;
            self.report(
                EngineStatus::Failed {
                    reason: format!("{reason}; gave up after {attempt} crashes in a row"),
                }
                .into(),
            );
            return;
        }
        let backoff = self
            .cfg
            .backoff_first
            .saturating_mul(1 << (attempt - 1).min(16))
            .min(self.cfg.backoff_max);
        self.phase = Phase::Waiting { at: now + backoff };
        self.report(
            EngineStatus::Restarting {
                attempt: attempt as u32,
            }
            .into(),
        );
    }
}

impl From<EngineStatus> for EngineReport {
    fn from(s: EngineStatus) -> Self {
        Self::Status(s)
    }
}

/// Hosts as child processes: `sb-engine-host --pipe NAME`, on a fresh pipe name each time.
#[cfg(windows)]
pub struct ProcessSpawner {
    pub exe: std::path::PathBuf,
    /// Passed after `--pipe NAME` (e.g. `--fake-driver`).
    pub args: Vec<String>,
    spawned: u64,
}

#[cfg(windows)]
impl ProcessSpawner {
    pub fn new(exe: std::path::PathBuf, args: Vec<String>) -> Self {
        Self { exe, args, spawned: 0 }
    }
}

#[cfg(windows)]
impl Spawn for ProcessSpawner {
    fn spawn(&mut self) -> Result<Spawned, String> {
        self.spawned += 1;
        let name = format!(r"\\.\pipe\sb-engine-{}-{}", std::process::id(), self.spawned);
        let mut child = std::process::Command::new(&self.exe)
            .arg("--pipe")
            .arg(&name)
            .args(&self.args)
            .spawn()
            .map_err(|e| format!("{}: {e}", self.exe.display()))?;
        let pipe = sb_pipe::Pipe::connect(&name, Duration::from_secs(5)).and_then(|p| {
            let closer = p.closer();
            p.split().map(|rw| (rw, closer))
        });
        match pipe {
            Ok(((reader, writer), closer)) => Ok(Spawned {
                reader: Box::new(reader),
                writer: Box::new(writer),
                process: Box::new(ChildHost { child, closer }),
            }),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(format!("connect to the engine host: {e}"))
            }
        }
    }
}

#[cfg(windows)]
struct ChildHost {
    child: std::process::Child,
    closer: sb_pipe::PipeCloser,
}

#[cfg(windows)]
impl HostProcess for ChildHost {
    fn end(&mut self, grace: Duration) -> String {
        let deadline = Instant::now() + grace;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(s)) => break Ok((s, "exited")),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    let _ = self.child.kill();
                    break self.child.wait().map(|s| (s, "killed"));
                }
                Err(e) => break Err(e),
            }
        };
        // Unblocks the client's reader thread if the pipe somehow outlives the process.
        self.closer.close();
        match status {
            Ok((s, how)) => format!("pid {} {how}, {s}", self.child.id()),
            Err(e) => format!("pid {}: {e}", self.child.id()),
        }
    }
}
