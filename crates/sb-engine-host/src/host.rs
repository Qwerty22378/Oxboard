//! The host loop, independent of the transport and of the audio backend.
//!
//! [`serve`] does the handshake, then runs on the calling thread (the engine's control
//! thread) while a reader thread turns incoming frames into requests. Besides answering
//! requests it does the engine's periodic work and reports it:
//! - `maintain()` every loop (retires plans, queues settle plans, releases endpoints);
//! - engine events (`GraphApplied`, `ClipEnded`, `DeviceLost`) as they appear;
//! - `MasterClockFallback` when the mix thread reports it;
//! - `Xrun` from ring counters every [`HostConfig::xrun_every`], rate-limited by design;
//! - `Meters` for every meter of the running graph every [`HostConfig::meters_every`];
//! - `Heartbeat` every [`HostConfig::heartbeat_every`], whatever else is going on.
//!
//! The engine starts with the first graph (using its master device, so the mix opens it up
//! front). The host exits when the controller disconnects or asks it to, or if the mix
//! thread dies.

use sb_engine::engine::Engine;
use sb_engine::graph::compile::{Endpoints, Tables};
use sb_engine::graph::spec::{GraphSpec, MeterId, NodeId, NodeKind};
use sb_engine::rt::stats::MixSnapshot;
use sb_shared::events::{EngineEvent, MeterFrame, MeterLevels, XrunKind};
use sb_shared::protocol::{HostMessage, PROTOCOL_VERSION, Request, RequestBody, read_frame, write_frame};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A started engine: its control side and the running mix thread.
pub type Started<E> = (Engine<E>, Box<dyn MixHandle>);

/// Starts the audio side of an engine.
pub trait Backend {
    type E: Endpoints;
    /// Start an engine whose mix runs on `master` (endpoint ID) or a timer.
    fn start(&mut self, block: usize, tables: Tables, master: Option<&str>) -> Result<Started<Self::E>, String>;
}

/// The running mix thread, as the host sees it.
pub trait MixHandle {
    fn stats(&self) -> MixSnapshot;
    /// The mix thread has exited (it only does when stopped or after a fatal error).
    fn is_finished(&self) -> bool;
}

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub block: usize,
    pub params: usize,
    pub meters: usize,
    pub heartbeat_every: Duration,
    pub meters_every: Duration,
    pub xrun_every: Duration,
    /// Streams clips longer than this many seconds.
    pub stream_clips_after_s: f64,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            block: 256,
            params: 4096,
            meters: 256,
            heartbeat_every: Duration::from_millis(500),
            meters_every: Duration::from_millis(33),
            xrun_every: Duration::from_millis(500),
            stream_clips_after_s: 60.0,
        }
    }
}

/// Why `serve` returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// The controller sent `Shutdown`.
    Shutdown,
    /// The controller disconnected (or the stream broke).
    Disconnected,
    /// The handshake failed (wrong protocol, or garbage).
    Refused(String),
    /// The mix thread died.
    MixFailed,
}

/// Serve one controller over `reader`/`writer` until it leaves. Blocks.
pub fn serve<B: Backend>(
    mut backend: B,
    mut reader: impl Read + Send + 'static,
    mut writer: impl Write,
    cfg: HostConfig,
) -> Exit {
    // Handshake, before anything else.
    let hello: sb_shared::protocol::Hello = match read_frame(&mut reader) {
        Ok(Some(h)) => h,
        Ok(None) => return Exit::Disconnected,
        Err(e) => return Exit::Refused(format!("bad hello: {e}")),
    };
    if hello.protocol != PROTOCOL_VERSION {
        let reason = format!("protocol {} spoken, {PROTOCOL_VERSION} expected", hello.protocol);
        let _ = write_frame(&mut writer, &HostMessage::Refused { reason: reason.clone() });
        return Exit::Refused(reason);
    }
    let welcome = HostMessage::Welcome {
        protocol: PROTOCOL_VERSION,
        host: format!("sb-engine-host {}", env!("CARGO_PKG_VERSION")),
    };
    if write_frame(&mut writer, &welcome).is_err() {
        return Exit::Disconnected;
    }

    // Requests arrive on their own thread; the control loop below never blocks on the pipe.
    let (tx, rx) = mpsc::channel::<Request>();
    std::thread::Builder::new()
        .name("sb-host-reader".into())
        .spawn(move || {
            while let Ok(Some(req)) = read_frame::<Request>(&mut reader) {
                if tx.send(req).is_err() {
                    break;
                }
            }
        })
        .expect("spawn the reader thread");

    let mut host = Host::new(cfg);
    let exit = loop {
        match rx.recv_timeout(Duration::from_millis(5)) {
            Ok(req) => match host.handle(req, &mut backend) {
                Handled::Continue(reply) => {
                    if let Some(r) = reply
                        && write_frame(&mut writer, &r).is_err()
                    {
                        break Exit::Disconnected;
                    }
                }
                Handled::Exit(reply) => {
                    let _ = write_frame(&mut writer, &reply);
                    break Exit::Shutdown;
                }
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break Exit::Disconnected,
        }
        let (events, mix_dead) = host.tick();
        if events
            .into_iter()
            .any(|e| write_frame(&mut writer, &HostMessage::Event(e)).is_err())
        {
            break Exit::Disconnected;
        }
        if mix_dead {
            break Exit::MixFailed;
        }
    };
    drop(host); // stops the engine: endpoints, streamers, the mix thread
    exit
}

enum Handled {
    Continue(Option<HostMessage>),
    Exit(HostMessage),
}

struct Running<E: Endpoints> {
    engine: Engine<E>,
    mix: Box<dyn MixHandle>,
}

struct Host<E: Endpoints> {
    cfg: HostConfig,
    tables: Tables,
    running: Option<Running<E>>,
    meters: Vec<MeterId>,
    started: Instant,
    next_heartbeat: Instant,
    next_meters: Instant,
    next_xruns: Instant,
    /// Xrun counters as last reported, per (node, kind, which side of the ring).
    xrun_seen: HashMap<(NodeId, XrunKind, bool), u64>,
    fallback_reported: bool,
}

impl<E: Endpoints> Host<E> {
    fn new(cfg: HostConfig) -> Self {
        let clips = sb_engine::clips::ClipStore::new(cfg.stream_clips_after_s, 2.0);
        let now = Instant::now();
        Self {
            tables: Tables::with_clips(cfg.params, cfg.meters, clips),
            cfg,
            running: None,
            meters: Vec::new(),
            started: now,
            next_heartbeat: now,
            next_meters: now,
            next_xruns: now,
            xrun_seen: HashMap::new(),
            fallback_reported: false,
        }
    }

    fn handle(&mut self, req: Request, backend: &mut impl Backend<E = E>) -> Handled {
        let reply = |result| Some(HostMessage::Reply { id: req.id, result });
        match req.body {
            RequestBody::ApplyGraph { graph, version } => {
                if self.running.is_none() {
                    match backend.start(self.cfg.block, self.tables.clone(), master_device(&graph)) {
                        Ok((engine, mix)) => self.running = Some(Running { engine, mix }),
                        Err(reason) => {
                            let err = sb_engine::graph::spec::GraphError::Endpoint {
                                node: graph.master.unwrap_or(NodeId(u32::MAX)),
                                reason,
                            };
                            return Handled::Continue(reply(Err(err)));
                        }
                    }
                }
                let r = self.running.as_mut().expect("started above");
                let result = r.engine.apply_graph_versioned(&graph, version);
                if result.is_ok() {
                    self.meters = graph.meters.iter().map(|m| m.id).collect();
                }
                Handled::Continue(reply(result))
            }
            RequestBody::SetParam { id, value } => {
                self.tables.params.set(id, value);
                Handled::Continue(None)
            }
            RequestBody::Trigger { node, action } => {
                if let Some(r) = &mut self.running {
                    r.engine.trigger(node, action);
                }
                Handled::Continue(None)
            }
            RequestBody::Ping => Handled::Continue(reply(Ok(()))),
            RequestBody::Shutdown => Handled::Exit(HostMessage::Reply {
                id: req.id,
                result: Ok(()),
            }),
        }
    }

    /// Periodic work. Returns the events to send, and whether the mix thread died.
    fn tick(&mut self) -> (Vec<EngineEvent>, bool) {
        let now = Instant::now();
        let mut out = Vec::new();
        let mut dead = false;
        if let Some(r) = &mut self.running {
            r.engine.maintain();
            out.extend(r.engine.poll_events());
            let stats = r.mix.stats();
            if stats.master_fallback && !self.fallback_reported {
                out.push(EngineEvent::MasterClockFallback);
            }
            self.fallback_reported = stats.master_fallback;
            dead = r.mix.is_finished();

            if now >= self.next_meters && !self.meters.is_empty() {
                self.next_meters = next_due(self.next_meters, self.cfg.meters_every, now);
                let meters = self
                    .meters
                    .iter()
                    .filter_map(|&id| {
                        self.tables.meters.read(id).map(|m| {
                            (
                                id,
                                MeterLevels {
                                    peak: m.peak,
                                    rms: m.rms,
                                },
                            )
                        })
                    })
                    .collect();
                out.push(EngineEvent::Meters(MeterFrame { meters }));
            }
            if now >= self.next_xruns {
                self.next_xruns = next_due(self.next_xruns, self.cfg.xrun_every, now);
                out.extend(Self::xruns(&r.engine, &mut self.xrun_seen));
            }
        }
        if now >= self.next_heartbeat {
            self.next_heartbeat = next_due(self.next_heartbeat, self.cfg.heartbeat_every, now);
            out.push(EngineEvent::Heartbeat {
                uptime_ms: self.started.elapsed().as_millis() as u64,
            });
        }
        (out, dead)
    }

    /// New glitches since the last scan, from both sides of every ring.
    fn xruns(engine: &Engine<E>, seen: &mut HashMap<(NodeId, XrunKind, bool), u64>) -> Vec<EngineEvent> {
        let layout = engine.running_layout();
        let mut counts: Vec<(NodeId, XrunKind, bool, u64)> = Vec::new();
        for ep in layout.inputs() {
            let s = ep.stats.snapshot();
            counts.push((ep.node, XrunKind::Underrun, false, s.xruns));
            counts.push((ep.node, XrunKind::Trimmed, false, s.trimmed_frames));
        }
        for ep in layout.outputs() {
            counts.push((ep.node, XrunKind::Overrun, false, ep.stats.snapshot().xruns));
        }
        for (node, capture, s) in engine.endpoints().device_stats() {
            let kind = if capture { XrunKind::Overrun } else { XrunKind::Underrun };
            counts.push((node, kind, true, s.xruns));
            if !capture {
                counts.push((node, XrunKind::Trimmed, true, s.trimmed_frames));
            }
        }
        let mut out = Vec::new();
        for (node, kind, device_side, now) in counts {
            let prev = seen.insert((node, kind, device_side), now).unwrap_or(0);
            if now > prev {
                let count = (now - prev).min(u32::MAX as u64) as u32;
                out.push(EngineEvent::Xrun { node, kind, count });
            }
        }
        out
    }
}

/// The next deadline after `prev`, keeping the average period exact even when the loop wakes
/// late (Windows timers tick every ~15.6 ms): skip ahead only if a whole period was missed.
fn next_due(prev: Instant, period: Duration, now: Instant) -> Instant {
    let next = prev + period;
    if next < now { now } else { next }
}

fn master_device(spec: &GraphSpec) -> Option<&str> {
    let m = spec.master?;
    match &spec.nodes.iter().find(|n| n.id == m)?.kind {
        NodeKind::DeviceOutput { device } => Some(device),
        _ => None,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::client::EngineClient;
    use sb_engine::clock::master::MasterClock;
    use sb_engine::graph::compile::EndpointKey;
    use sb_engine::graph::spec::{
        ClipAction, EdgeSpec, GraphError, MeterSpec, NodeSpec, ParamId, ParamSpec, Retrigger,
    };
    use sb_engine::rt::mix_thread::MixCore;
    use sb_engine::rt::ring::{RingReader, RingWriter};
    use sb_engine::rt::stats::MixStats;
    use sb_shared::protocol::Hello;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

    /// No devices: tests use tones and clips, mastered on a device the clock "opens".
    struct NoDevices;

    impl Endpoints for NoDevices {
        fn input(&mut self, _: NodeId, d: &str) -> Result<(RingReader, EndpointKey), String> {
            Err(format!("no device {d} in tests"))
        }
        fn output(&mut self, _: NodeId, d: &str) -> Result<(RingWriter, EndpointKey), String> {
            Err(format!("no device {d} in tests"))
        }
        fn release(&mut self, _: EndpointKey) {}
    }

    /// Plays the master bus nowhere, at about real time.
    struct SilentClock(Instant);

    impl MasterClock for SilentClock {
        fn wait(&mut self) -> usize {
            std::thread::sleep(Duration::from_micros(5333));
            1
        }
        fn deliver(&mut self, _: &[f32], _: &[f32]) {}
        fn set_master(&mut self, _: Option<&str>) {}
        fn now_ns(&self) -> u64 {
            self.0.elapsed().as_nanos() as u64
        }
    }

    struct TestMix {
        stats: Arc<MixStats>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl MixHandle for TestMix {
        fn stats(&self) -> MixSnapshot {
            self.stats.snapshot()
        }
        fn is_finished(&self) -> bool {
            self.thread.as_ref().is_none_or(|t| t.is_finished())
        }
    }

    impl Drop for TestMix {
        fn drop(&mut self) {
            self.stop.store(true, Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    struct TestBackend;

    impl Backend for TestBackend {
        type E = NoDevices;
        fn start(&mut self, block: usize, tables: Tables, master: Option<&str>) -> Result<Started<NoDevices>, String> {
            let (engine, mut core): (_, MixCore) = Engine::with_master(block, tables, NoDevices, master);
            let stats = Arc::new(MixStats::default());
            let stop = Arc::new(AtomicBool::new(false));
            let thread = {
                let (stats, stop) = (stats.clone(), stop.clone());
                std::thread::spawn(move || {
                    let mut clock = SilentClock(Instant::now());
                    sb_engine::rt::mix_thread::run(&mut core, &mut clock, &stats, &stop);
                })
            };
            Ok((
                engine,
                Box::new(TestMix {
                    stats,
                    stop,
                    thread: Some(thread),
                }),
            ))
        }
    }

    fn start_host(cfg: HostConfig) -> (EngineClient<UnixStream>, std::thread::JoinHandle<Exit>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let reader = theirs.try_clone().unwrap();
        let host = std::thread::spawn(move || serve(TestBackend, reader, theirs, cfg));
        let client = EngineClient::connect(ours.try_clone().unwrap(), ours, "test").unwrap();
        (client, host)
    }

    fn graph(tone_gain_param: bool) -> GraphSpec {
        GraphSpec {
            nodes: vec![
                NodeSpec {
                    id: NodeId(1),
                    kind: NodeKind::Tone { freq: 440.0, gain: 0.5 },
                },
                NodeSpec {
                    id: NodeId(2),
                    kind: NodeKind::Bus,
                },
                NodeSpec {
                    id: NodeId(3),
                    kind: NodeKind::DeviceOutput { device: "spk".into() },
                },
            ],
            edges: vec![
                EdgeSpec {
                    from: NodeId(1),
                    to: NodeId(2),
                    gain: tone_gain_param.then_some(ParamId(1)),
                },
                EdgeSpec {
                    from: NodeId(2),
                    to: NodeId(3),
                    gain: None,
                },
            ],
            params: if tone_gain_param {
                vec![ParamSpec {
                    id: ParamId(1),
                    value: 1.0,
                }]
            } else {
                vec![]
            },
            meters: vec![MeterSpec {
                id: MeterId(0),
                node: NodeId(2),
            }],
            master: Some(NodeId(3)),
        }
    }

    /// Collect events for `d`.
    fn events_for(client: &mut EngineClient<UnixStream>, d: Duration) -> Vec<EngineEvent> {
        let end = Instant::now() + d;
        let mut out = Vec::new();
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            if let Some(e) = client.next_event(left) {
                out.push(e);
            }
        }
        out
    }

    fn peak(events: &[EngineEvent]) -> f32 {
        events
            .iter()
            .rev()
            .find_map(|e| match e {
                EngineEvent::Meters(f) => f.meters.first().map(|(_, l)| l.peak[0]),
                _ => None,
            })
            .unwrap_or(-1.0)
    }

    #[test]
    fn a_full_session_over_a_socket() {
        let cfg = HostConfig {
            heartbeat_every: Duration::from_millis(100),
            ..Default::default()
        };
        let (mut client, host) = start_host(cfg);

        // Apply: compiled reply right away, GraphApplied once the mix runs it.
        assert_eq!(client.apply_graph(graph(true), 7).unwrap(), Ok(()));
        let ev = events_for(&mut client, Duration::from_millis(400));
        assert!(ev.contains(&EngineEvent::GraphApplied { version: 7 }), "{ev:?}");
        assert!(
            ev.iter().filter(|e| matches!(e, EngineEvent::Heartbeat { .. })).count() >= 3,
            "heartbeats"
        );
        let frames = ev.iter().filter(|e| matches!(e, EngineEvent::Meters(_))).count();
        assert!((6..=16).contains(&frames), "~30 Hz meters, got {frames} in 0.4 s");
        assert!(
            (peak(&ev) - 0.5).abs() < 0.01,
            "the tone reaches the bus meter: {}",
            peak(&ev)
        );

        // A param moves it without a reply or a rebuild.
        client.set_param(ParamId(1), 0.0).unwrap();
        let ev = events_for(&mut client, Duration::from_millis(300));
        assert!(peak(&ev) < 1e-3, "faded out: {}", peak(&ev));

        // A bad graph is refused with the reason; the running graph stays.
        let mut bad = graph(true);
        bad.edges.push(EdgeSpec {
            from: NodeId(9),
            to: NodeId(2),
            gain: None,
        });
        let err = client.apply_graph(bad, 8).unwrap().unwrap_err();
        assert!(
            matches!(err, GraphError::UnknownNode { node: NodeId(9), .. }),
            "{err:?}"
        );
        let ev = events_for(&mut client, Duration::from_millis(200));
        assert!(!ev.iter().any(|e| matches!(e, EngineEvent::GraphApplied { version: 8 })));

        assert_eq!(client.ping().unwrap(), Ok(()));
        assert_eq!(client.shutdown().unwrap(), Ok(()));
        assert_eq!(host.join().unwrap(), Exit::Shutdown);
    }

    #[test]
    fn clip_trigger_over_the_pipe_reports_clip_ended() {
        let path = std::env::temp_dir().join(format!("sb-host-test-{}.wav", std::process::id()));
        write_test_wav(&path, 4800); // 0.1 s
        let (mut client, host) = start_host(HostConfig::default());
        let mut g = graph(false);
        g.nodes.push(NodeSpec {
            id: NodeId(4),
            kind: NodeKind::Clip {
                path: path.to_string_lossy().into(),
                polyphony: 1,
                retrigger: Retrigger::Restart,
                looped: false,
            },
        });
        g.edges.push(EdgeSpec {
            from: NodeId(4),
            to: NodeId(2),
            gain: None,
        });
        assert_eq!(client.apply_graph(g, 1).unwrap(), Ok(()));
        client.trigger(NodeId(4), ClipAction::Play).unwrap();
        let ev = events_for(&mut client, Duration::from_millis(500));
        assert!(
            ev.contains(&EngineEvent::ClipEnded {
                node: NodeId(4),
                stopped: false
            }),
            "{ev:?}"
        );
        drop(client); // controller goes away: the host must exit on its own
        assert_eq!(host.join().unwrap(), Exit::Disconnected);
    }

    #[test]
    fn mismatched_protocol_is_refused() {
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        let reader = theirs.try_clone().unwrap();
        let host = std::thread::spawn(move || serve(TestBackend, reader, theirs, HostConfig::default()));
        write_frame(
            &mut ours,
            &Hello {
                protocol: PROTOCOL_VERSION + 1,
                client: "old".into(),
            },
        )
        .unwrap();
        let reply: HostMessage = read_frame(&mut ours).unwrap().unwrap();
        assert!(matches!(reply, HostMessage::Refused { .. }));
        assert!(matches!(host.join().unwrap(), Exit::Refused(_)));
    }

    #[test]
    fn garbage_instead_of_hello_is_refused_without_panicking() {
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        let reader = theirs.try_clone().unwrap();
        let host = std::thread::spawn(move || serve(TestBackend, reader, theirs, HostConfig::default()));
        ours.write_all(&[0xff, 0xff, 0xff, 0x7f, 1, 2, 3]).unwrap();
        assert!(matches!(host.join().unwrap(), Exit::Refused(_)));
    }

    fn write_test_wav(path: &std::path::Path, frames: usize) {
        let data: Vec<u8> = (0..frames)
            .flat_map(|i| (((i as f32 / 10.0).sin() * 8000.0) as i16).to_le_bytes())
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
}
