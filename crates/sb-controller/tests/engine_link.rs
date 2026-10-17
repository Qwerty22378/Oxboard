//! The engine link: spawning, replay, heartbeat, backoff, giving up. Scripted fake hosts make
//! each failure happen on cue; the real host loop (silent backend, over a socket pair) checks
//! the whole controller end to end.
#![cfg(unix)]

use sb_controller::Controller;
use sb_controller::actor::{EngineUpdate, Msg};
use sb_controller::client::UiClient;
use sb_controller::commands;
use sb_controller::compile::{Compiled, Continuity, Update, compile};
use sb_controller::engine_link::{self, EngineReport, HostProcess, LinkConfig, LinkMsg, Spawn, Spawned};
use sb_controller::hub::Hub;
use sb_controller::state::{AppState, DeviceInfo, Observation};
use sb_controller::transport;
use sb_engine_host::host::{HostConfig, serve};
use sb_engine_host::silent::SilentBackend;
use sb_shared::events::EngineEvent;
use sb_shared::graph::GraphError;
use sb_shared::protocol::{Hello, HostMessage, PROTOCOL_VERSION, Request, RequestBody, read_frame, write_frame};
use sb_shared::ui::*;
use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const T: Duration = Duration::from_secs(5);

/// A host thread behind a socket pair; "killing" it shuts its end of the socket.
struct ThreadHost {
    stream: UnixStream,
    thread: Option<JoinHandle<()>>,
}

impl HostProcess for ThreadHost {
    fn end(&mut self, grace: Duration) -> String {
        let deadline = Instant::now() + grace;
        let t = self.thread.take().expect("ended once");
        while !t.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let how = if t.is_finished() { "exited" } else { "killed" };
        let _ = self.stream.shutdown(Shutdown::Both);
        let _ = t.join();
        how.into()
    }
}

/// Every host's end of its socket, to crash it from the test.
type Sockets = Arc<Mutex<Vec<UnixStream>>>;

fn crash_latest(sockets: &Sockets) {
    let s = sockets.lock().unwrap();
    s.last().expect("a host").shutdown(Shutdown::Both).unwrap();
}

fn pair(sockets: &Sockets) -> (UnixStream, UnixStream) {
    let (ours, theirs) = UnixStream::pair().unwrap();
    sockets.lock().unwrap().push(theirs.try_clone().unwrap());
    (ours, theirs)
}

fn spawned(ours: UnixStream, stream: UnixStream, thread: JoinHandle<()>) -> Spawned {
    Spawned {
        reader: Box::new(ours.try_clone().unwrap()),
        writer: Box::new(ours),
        process: Box::new(ThreadHost {
            stream,
            thread: Some(thread),
        }),
    }
}

// ---------------------------------------------------------------------------------------------
// Scripted hosts

#[derive(Default)]
struct Script {
    /// Hang up right after the hello (a host that dies on start).
    hang_up: AtomicBool,
    /// Stop sending heartbeats (a hung host).
    silent: AtomicBool,
    /// Refuse graphs with this version or later (0: none).
    refuse_from: AtomicU64,
}

#[derive(Clone, Default)]
struct FakeHosts {
    script: Arc<Script>,
    /// Every request each host got: (host number from 1, request).
    log: Arc<Mutex<Vec<(usize, RequestBody)>>>,
    started: Arc<Mutex<Vec<Instant>>>,
    sockets: Sockets,
}

impl FakeHosts {
    fn requests_to(&self, host: usize) -> Vec<RequestBody> {
        let log = self.log.lock().unwrap();
        log.iter().filter(|(h, _)| *h == host).map(|(_, r)| r.clone()).collect()
    }

    fn count(&self) -> usize {
        self.started.lock().unwrap().len()
    }
}

impl Spawn for FakeHosts {
    fn spawn(&mut self) -> Result<Spawned, String> {
        let n = {
            let mut s = self.started.lock().unwrap();
            s.push(Instant::now());
            s.len()
        };
        let (ours, theirs) = pair(&self.sockets);
        let (script, log) = (self.script.clone(), self.log.clone());
        let stream = theirs.try_clone().unwrap();
        let thread = std::thread::spawn(move || fake_host(n, theirs, &script, &log));
        Ok(spawned(ours, stream, thread))
    }
}

fn fake_host(n: usize, stream: UnixStream, script: &Script, log: &Mutex<Vec<(usize, RequestBody)>>) {
    let mut r = stream.try_clone().unwrap();
    let w = Arc::new(Mutex::new(stream));
    let send = |m: &HostMessage| write_frame(&mut *w.lock().unwrap(), m).is_ok();
    let Ok(Some(_hello)) = read_frame::<Hello>(&mut r) else {
        return;
    };
    if script.hang_up.load(SeqCst) {
        let _ = r.shutdown(Shutdown::Both);
        return;
    }
    send(&HostMessage::Welcome {
        protocol: PROTOCOL_VERSION,
        host: format!("fake host {n}"),
    });
    let done = Arc::new(AtomicBool::new(false));
    let beat = {
        let (w, done) = (w.clone(), done.clone());
        let silent = script.silent.load(SeqCst);
        std::thread::spawn(move || {
            let mut uptime = 0;
            while !done.load(SeqCst) {
                std::thread::sleep(Duration::from_millis(50));
                uptime += 50;
                if silent {
                    continue;
                }
                let m = HostMessage::Event(EngineEvent::Heartbeat { uptime_ms: uptime });
                if write_frame(&mut *w.lock().unwrap(), &m).is_err() {
                    break;
                }
            }
        })
    };
    while let Ok(Some(req)) = read_frame::<Request>(&mut r) {
        log.lock().unwrap().push((n, req.body.clone()));
        let reply = |result| HostMessage::Reply { id: req.id, result };
        match req.body {
            RequestBody::ApplyGraph { version, .. } => {
                let from = script.refuse_from.load(SeqCst);
                if from != 0 && version >= from {
                    send(&reply(Err(GraphError::Endpoint {
                        node: sb_shared::graph::NodeId(0),
                        reason: "fake refusal".into(),
                    })));
                } else {
                    send(&reply(Ok(())));
                    send(&HostMessage::Event(EngineEvent::GraphApplied { version }));
                }
            }
            RequestBody::Ping => {
                send(&reply(Ok(())));
            }
            RequestBody::Shutdown => {
                send(&reply(Ok(())));
                break;
            }
            RequestBody::SetParam { .. } | RequestBody::Trigger { .. } => {}
        }
    }
    let _ = w.lock().unwrap().shutdown(Shutdown::Both);
    done.store(true, SeqCst);
    let _ = beat.join();
}

// ---------------------------------------------------------------------------------------------
// The link on its own, with the test as its actor

struct Link {
    tx: mpsc::Sender<LinkMsg>,
    inbox: Receiver<Msg>,
    thread: Option<JoinHandle<()>>,
}

impl Link {
    fn start(spawner: impl Spawn, cfg: LinkConfig) -> Self {
        let (inbox_tx, inbox) = mpsc::channel();
        let (tx, thread) = engine_link::spawn(Box::new(spawner), cfg, Arc::new(Hub::default()), inbox_tx);
        Self {
            tx,
            inbox,
            thread: Some(thread),
        }
    }

    fn send(&self, m: LinkMsg) {
        self.tx.send(m).unwrap();
    }

    /// Reports until one matches; panics (showing them) if none does in time.
    fn until(&self, what: &str, mut pred: impl FnMut(&EngineReport) -> bool) -> Vec<EngineReport> {
        let deadline = Instant::now() + T;
        let mut seen = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.inbox.recv_timeout(left) {
                Ok(Msg::Engine(r)) => {
                    let done = pred(&r);
                    seen.push(r);
                    if done {
                        return seen;
                    }
                }
                Ok(_) => {}
                Err(_) => panic!("no {what}; got {seen:#?}"),
            }
        }
    }

    fn until_status(&self, s: EngineStatus) -> Vec<EngineReport> {
        let want = EngineReport::Status(s.clone());
        self.until(&format!("{s:?}"), |r| *r == want)
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.tx.send(LinkMsg::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn fast() -> LinkConfig {
    LinkConfig {
        heartbeat_timeout: Duration::from_millis(300),
        backoff_first: Duration::from_millis(20),
        backoff_max: Duration::from_millis(200),
        give_up_after: 3,
        crash_window: Duration::from_secs(60),
        poll: Duration::from_millis(5),
    }
}

fn speakers() -> DeviceKey {
    DeviceKey("{0.0.0.00000000}.{speakers}".into())
}

fn mic() -> DeviceKey {
    DeviceKey("{0.0.1.00000000}.{mic}".into())
}

/// A state whose graph has params: the mic played to the speakers.
fn state() -> AppState {
    let mut s = AppState::default();
    for (key, name, flow) in [(speakers(), "Speakers", Flow::Render), (mic(), "Mic", Flow::Capture)] {
        s.observe(Observation::DeviceAdded {
            key,
            info: DeviceInfo {
                name: name.into(),
                flow,
            },
        });
    }
    let cmds = [
        Command::SetSystemInput(Some(SystemInput::Device(mic()))),
        Command::SelectOutput {
            device: speakers(),
            selected: true,
        },
    ];
    for c in cmds {
        commands::apply(&mut s, c).unwrap();
    }
    s
}

/// The first graph, then a fader move: (graph, its params update, the graph after it).
fn graph_then_fader() -> (Compiled, Vec<(sb_shared::graph::ParamId, f32)>, Compiled) {
    let mut s = state();
    let mut ids = Continuity::default();
    let first = compile(&s, &mut ids).unwrap();
    assert!(!first.graph.params.is_empty());
    commands::apply(&mut s, Command::SetMasterGain(0.25)).unwrap();
    let after = compile(&s, &mut ids).unwrap();
    let Update::Params(moved) = after.update_from(&first) else {
        panic!("a fader is a param update")
    };
    (first, moved, after)
}

fn running(v: u64) -> EngineStatus {
    EngineStatus::Running { applied_version: v }
}

#[test]
fn nothing_starts_until_there_is_a_graph_then_it_runs() {
    let hosts = FakeHosts::default();
    let link = Link::start(hosts.clone(), fast());
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(hosts.count(), 0);
    let (first, ..) = graph_then_fader();
    link.send(LinkMsg::Update(EngineUpdate::Graph {
        version: 1,
        compiled: first,
    }));
    let seen = link.until_status(running(1));
    assert_eq!(seen[0], EngineReport::Status(EngineStatus::Starting));
    assert_eq!(hosts.count(), 1);
}

#[test]
fn a_crashed_host_is_replaced_and_replayed_the_latest_graph_with_every_param() {
    let hosts = FakeHosts::default();
    let link = Link::start(hosts.clone(), fast());
    let (first, moved, after) = graph_then_fader();
    link.send(LinkMsg::Update(EngineUpdate::Graph {
        version: 1,
        compiled: first,
    }));
    link.until_status(running(1));
    link.send(LinkMsg::Update(EngineUpdate::Params(moved.clone())));
    let deadline = Instant::now() + T;
    while !hosts
        .requests_to(1)
        .iter()
        .any(|r| matches!(r, RequestBody::SetParam { .. }))
    {
        assert!(Instant::now() < deadline, "the fader reaches the host");
        std::thread::sleep(Duration::from_millis(5));
    }

    crash_latest(&hosts.sockets);
    let seen = link.until_status(running(1));
    let statuses: Vec<_> = seen
        .iter()
        .filter_map(|r| match r {
            EngineReport::Status(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        statuses,
        [
            EngineStatus::Restarting { attempt: 1 },
            EngineStatus::Starting,
            running(1)
        ]
    );
    // One request replays everything: the graph as it is now, the fader's value included.
    let replay = hosts.requests_to(2);
    assert_eq!(
        replay[0],
        RequestBody::ApplyGraph {
            graph: after.graph,
            version: 1
        }
    );
    assert_eq!(replay.len(), 1, "no param history: {replay:?}");
}

#[test]
fn a_hung_host_is_killed_after_the_heartbeat_timeout() {
    let hosts = FakeHosts::default();
    hosts.script.silent.store(true, SeqCst);
    let link = Link::start(hosts.clone(), fast());
    let (first, ..) = graph_then_fader();
    link.send(LinkMsg::Update(EngineUpdate::Graph {
        version: 1,
        compiled: first,
    }));
    link.until_status(running(1));
    let t = Instant::now();
    link.until_status(EngineStatus::Restarting { attempt: 1 });
    let took = t.elapsed();
    assert!(
        took >= Duration::from_millis(200) && took < Duration::from_secs(2),
        "{took:?}"
    );
    hosts.script.silent.store(false, SeqCst);
    link.until_status(running(1));
    assert_eq!(hosts.count(), 2);
}

#[test]
fn repeated_crashes_back_off_then_give_up_until_asked_to_restart() {
    let hosts = FakeHosts::default();
    hosts.script.hang_up.store(true, SeqCst);
    let link = Link::start(hosts.clone(), fast());
    let (first, ..) = graph_then_fader();
    link.send(LinkMsg::Update(EngineUpdate::Graph {
        version: 1,
        compiled: first,
    }));
    let seen = link.until("Failed", |r| {
        matches!(r, EngineReport::Status(EngineStatus::Failed { .. }))
    });
    let attempts: Vec<u32> = seen
        .iter()
        .filter_map(|r| match r {
            EngineReport::Status(EngineStatus::Restarting { attempt }) => Some(*attempt),
            _ => None,
        })
        .collect();
    assert_eq!(attempts, [1, 2]);
    let Some(EngineReport::Status(EngineStatus::Failed { reason })) = seen.last() else {
        unreachable!()
    };
    assert!(reason.contains("3 crashes"), "{reason}");
    // The waits double: 20 ms, then 40 ms.
    let started = hosts.started.lock().unwrap().clone();
    assert_eq!(started.len(), 3);
    let (a, b) = (started[1] - started[0], started[2] - started[1]);
    assert!(
        a >= Duration::from_millis(20) && b >= Duration::from_millis(40),
        "{a:?} {b:?}"
    );
    // It stays down...
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(hosts.count(), 3);
    // ...until the user retries.
    hosts.script.hang_up.store(false, SeqCst);
    link.send(LinkMsg::Restart);
    link.until_status(running(1));
}

#[test]
fn a_refused_graph_is_reported_and_the_host_keeps_the_last_one() {
    let hosts = FakeHosts::default();
    hosts.script.refuse_from.store(2, SeqCst);
    let link = Link::start(hosts.clone(), fast());
    let (first, _, after) = graph_then_fader();
    link.send(LinkMsg::Update(EngineUpdate::Graph {
        version: 1,
        compiled: first,
    }));
    link.until_status(running(1));
    link.send(LinkMsg::Update(EngineUpdate::Graph {
        version: 2,
        compiled: after,
    }));
    link.until("the refusal", |r| matches!(r, EngineReport::Refused { version: 2, .. }));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(hosts.count(), 1, "not a crash");
}

#[test]
fn a_host_that_cant_run_its_first_graph_counts_as_a_crash() {
    let hosts = FakeHosts::default();
    hosts.script.refuse_from.store(1, SeqCst);
    let link = Link::start(hosts.clone(), fast());
    let (first, ..) = graph_then_fader();
    link.send(LinkMsg::Update(EngineUpdate::Graph {
        version: 1,
        compiled: first,
    }));
    link.until_status(EngineStatus::Restarting { attempt: 1 });
}

// ---------------------------------------------------------------------------------------------
// The whole controller, on the real host loop

#[derive(Clone, Default)]
struct SilentHosts {
    sockets: Sockets,
}

impl Spawn for SilentHosts {
    fn spawn(&mut self) -> Result<Spawned, String> {
        let (ours, theirs) = pair(&self.sockets);
        let stream = theirs.try_clone().unwrap();
        let cfg = HostConfig {
            heartbeat_every: Duration::from_millis(100),
            ..Default::default()
        };
        let thread = std::thread::spawn(move || {
            let reader = theirs.try_clone().unwrap();
            serve(SilentBackend, reader, theirs, cfg);
        });
        Ok(spawned(ours, stream, thread))
    }
}

type Client = UiClient<Box<dyn Write + Send>>;

fn wait_view(c: &mut Client, what: &str, mut cond: impl FnMut(&StateView) -> bool) {
    let deadline = Instant::now() + T;
    while !(c.mirror.synced && cond(&c.mirror.view)) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {:#?}",
            c.mirror.view
        );
        c.next(Duration::from_millis(20));
    }
}

fn is_running(v: &StateView) -> bool {
    matches!(v.engine, EngineStatus::Running { .. })
}

fn clips_peak(c: &mut Client, d: Duration) -> f32 {
    let end = Instant::now() + d;
    let mut peak: f32 = 0.0;
    while Instant::now() < end {
        if let Some(UiMessage::Meters(m)) = c.next(Duration::from_millis(20)) {
            for (target, levels) in m {
                if target == MeterTarget::Clips {
                    peak = peak.max(levels.peak[0]);
                }
            }
        }
    }
    peak
}

#[test]
fn clips_play_through_a_real_host_and_survive_its_crash() {
    let dir = std::env::temp_dir().join(format!("sb-controller-engine-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let wav = dir.join("beep.wav");
    write_test_wav(&wav, 9600); // 0.2 s
    let addr = dir.join("ui.sock").to_string_lossy().into_owned();
    let hosts = SilentHosts::default();
    let controller = Controller::with_engine(
        transport::bind(&addr).unwrap(),
        AppState::default(),
        false,
        hosts.clone(),
        fast(),
    );
    controller
        .inbox
        .send(Msg::Observed(Observation::DeviceAdded {
            key: speakers(),
            info: DeviceInfo {
                name: "Speakers".into(),
                flow: Flow::Render,
            },
        }))
        .unwrap();
    let mut c = Client::over(transport::connect(&addr, T).unwrap(), "test").unwrap();
    c.subscribe().unwrap();
    wait_view(&mut c, "the engine", is_running);

    let call = |c: &mut Client, cmd| c.call(cmd, T).unwrap();
    call(
        &mut c,
        Command::AddClip {
            path: wav.to_string_lossy().into(),
            name: String::new(),
        },
    )
    .unwrap();
    call(
        &mut c,
        Command::SetRoute {
            from: Source::Clips,
            to: Sink::Device(speakers()),
            gain: Some(1.0),
        },
    )
    .unwrap();
    let id = *c.mirror.view.clips.keys().next().unwrap();
    // The clips meter only exists in the new graph, and meters only flow once it runs.
    let deadline = Instant::now() + T;
    loop {
        assert!(Instant::now() < deadline, "no clips meter");
        if let Some(UiMessage::Meters(m)) = c.next(Duration::from_millis(20))
            && m.iter().any(|(t, _)| *t == MeterTarget::Clips)
        {
            break;
        }
    }

    call(&mut c, Command::PlayClip { id }).unwrap();
    assert!(c.mirror.view.clips[&id].playing);
    assert!(clips_peak(&mut c, Duration::from_millis(150)) > 0.05, "the clip meters");
    wait_view(&mut c, "the clip to end", |v| !v.clips[&id].playing);

    // Kill the host: the controller notices, restarts it, and the clip plays again.
    crash_latest(&hosts.sockets);
    wait_view(&mut c, "a restart", |v| {
        matches!(v.engine, EngineStatus::Restarting { .. })
    });
    wait_view(&mut c, "the engine again", is_running);
    call(&mut c, Command::PlayClip { id }).unwrap();
    assert!(
        clips_peak(&mut c, Duration::from_millis(150)) > 0.05,
        "plays after the restart"
    );

    // A clip that doesn't exist, and a restart on request.
    let err = call(&mut c, Command::PlayClip { id: ClipId(99) }).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotFound);
    call(&mut c, Command::RestartEngine).unwrap();
    wait_view(&mut c, "the restart", |v| !is_running(v));
    wait_view(&mut c, "the engine after the restart", is_running);
    assert_eq!(hosts.sockets.lock().unwrap().len(), 3);

    controller.stop();
}

#[test]
fn without_an_engine_clip_commands_say_so() {
    let dir = std::env::temp_dir().join(format!("sb-controller-noengine-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let addr = dir.join("ui.sock").to_string_lossy().into_owned();
    let controller = Controller::start(transport::bind(&addr).unwrap(), AppState::default(), false);
    let mut c = Client::over(transport::connect(&addr, T).unwrap(), "test").unwrap();
    c.subscribe().unwrap();
    c.call(
        Command::AddClip {
            path: "/nowhere/x.wav".into(),
            name: String::new(),
        },
        T,
    )
    .unwrap()
    .unwrap();
    let id = *c.mirror.view.clips.keys().next().unwrap();
    for cmd in [Command::PlayClip { id }, Command::RestartEngine] {
        let err = c.call(cmd, T).unwrap().unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unavailable);
    }
    controller.stop();
}

fn write_test_wav(path: &std::path::Path, frames: usize) {
    let data: Vec<u8> = (0..frames)
        .flat_map(|i| (((i as f32 / 10.0).sin() * 16000.0) as i16).to_le_bytes())
        .collect();
    let mut f = std::fs::File::create(path).unwrap();
    let mut w = |b: &[u8]| f.write_all(b).unwrap();
    w(b"RIFF");
    w(&(36 + data.len() as u32).to_le_bytes());
    w(b"WAVEfmt ");
    w(&16u32.to_le_bytes());
    w(&1u16.to_le_bytes());
    w(&1u16.to_le_bytes());
    w(&48_000u32.to_le_bytes());
    w(&96_000u32.to_le_bytes());
    w(&2u16.to_le_bytes());
    w(&16u16.to_le_bytes());
    w(b"data");
    w(&(data.len() as u32).to_le_bytes());
    w(&data);
}
