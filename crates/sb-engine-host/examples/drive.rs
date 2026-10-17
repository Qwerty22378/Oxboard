//! Drive `sb-engine-host` like the controller will (milestone E9 / M4), over the named pipe.
//!
//!   drive [--out NAME] [--clip FILE]
//!
//! Build the host too: `cargo build --release -p sb-engine-host --bins --examples`; this looks
//! for `sb-engine-host.exe` next to `examples\`. The script: spawn the host, apply a graph,
//! watch events, move a param, play a clip, send a bad graph, **kill the host** and measure
//! how fast the controller side notices, then respawn it and replay the graph and params (what
//! the controller's supervisor will do, M5), and finally shut it down cleanly.

#[cfg(not(windows))]
fn main() {
    eprintln!("drive spawns the Windows host; it only runs on Windows");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    if let Err(e) = win::run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod win {
    use sb_engine::backend::wasapi::{self, ComGuard, Direction};
    use sb_pipe::{Pipe, PipeWriter};
    use sb_shared::engine_client::EngineClient;
    use sb_shared::events::EngineEvent;
    use sb_shared::graph::*;
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    const TONE_GAIN: ParamId = ParamId(1);
    const APP_GAIN: ParamId = ParamId(2);
    const CLIP: NodeId = NodeId(6);

    type Client = EngineClient<PipeWriter>;

    fn spawn_host() -> Result<(Child, Client, Instant), Box<dyn std::error::Error>> {
        let exe = std::env::current_exe()?;
        let host = exe
            .parent()
            .and_then(|d| d.parent())
            .ok_or("no parent dir")?
            .join("sb-engine-host.exe");
        let name = format!(
            r"\\.\pipe\sb-engine-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos() ^ rand_ish()
        );
        let t = Instant::now();
        let child = Command::new(&host)
            .args(["--pipe", &name, "--fake-driver"])
            .spawn()
            .map_err(|e| format!("{}: {e}", host.display()))?;
        let (r, w) = Pipe::connect(&name, Duration::from_secs(5))?.split()?;
        let client = EngineClient::connect(r, w, "drive example")?;
        println!(
            "host up in {:.0} ms: {} (pid {})",
            t.elapsed().as_secs_f64() * 1000.0,
            client.host,
            child.id()
        );
        Ok((child, client, Instant::now()))
    }

    fn rand_ish() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    }

    fn graph(speakers: &str, clip: Option<&str>) -> GraphSpec {
        let n = |id, kind| NodeSpec { id: NodeId(id), kind };
        let e = |from, to, gain| EdgeSpec {
            from: NodeId(from),
            to: NodeId(to),
            gain,
        };
        let mut g = GraphSpec {
            nodes: vec![
                n(1, NodeKind::Tone { freq: 440.0, gain: 0.3 }),
                n(2, NodeKind::VirtualSource { endpoint: 1 }), // the fake driver's "app": 660 Hz
                n(3, NodeKind::Bus),
                n(
                    4,
                    NodeKind::DeviceOutput {
                        device: speakers.into(),
                    },
                ),
                n(5, NodeKind::VirtualMic { endpoint: 9 }),
            ],
            edges: vec![
                e(1, 3, Some(TONE_GAIN)),
                e(2, 3, Some(APP_GAIN)),
                e(3, 4, None),
                e(3, 5, None),
            ],
            params: vec![
                ParamSpec {
                    id: TONE_GAIN,
                    value: 1.0,
                },
                ParamSpec {
                    id: APP_GAIN,
                    value: 0.5,
                },
            ],
            meters: vec![MeterSpec {
                id: MeterId(0),
                node: NodeId(3),
            }],
            master: Some(NodeId(4)),
        };
        if let Some(path) = clip {
            g.nodes.push(n(
                6,
                NodeKind::Clip {
                    path: path.into(),
                    polyphony: 2,
                    retrigger: Retrigger::Restart,
                    looped: false,
                },
            ));
            g.edges.push(e(6, 3, None));
        }
        g
    }

    /// Watch events for `d`; print a summary line and return them.
    fn watch(client: &mut Client, d: Duration, label: &str) -> Vec<EngineEvent> {
        let end = Instant::now() + d;
        let mut ev = Vec::new();
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            match client.next_event(left) {
                Some(e) => ev.push(e),
                None if !client.is_connected() => break,
                None => {}
            }
        }
        let count = |f: fn(&EngineEvent) -> bool| ev.iter().filter(|e| f(e)).count();
        let peak = ev.iter().rev().find_map(|e| match e {
            EngineEvent::Meters(m) => m.meters.first().map(|(_, l)| l.peak[0].max(l.peak[1])),
            _ => None,
        });
        let others: Vec<String> = ev
            .iter()
            .filter(|e| !matches!(e, EngineEvent::Meters(_) | EngineEvent::Heartbeat { .. }))
            .map(|e| format!("{e:?}"))
            .collect();
        println!(
            "  [{label}] {:.1} s: {} heartbeats, {} meter frames, bus peak {}{}",
            d.as_secs_f64(),
            count(|e| matches!(e, EngineEvent::Heartbeat { .. })),
            count(|e| matches!(e, EngineEvent::Meters(_))),
            peak.map_or("—".into(), |p| format!("{:.1} dB", 20.0 * p.log10())),
            if others.is_empty() {
                String::new()
            } else {
                format!(" | {}", others.join(", "))
            }
        );
        ev
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let mut args = std::env::args().skip(1);
        let (mut out, mut clip) = (None, None);
        while let Some(a) = args.next() {
            match a.as_str() {
                "--out" => out = args.next(),
                "--clip" => clip = args.next(),
                other => return Err(format!("unknown argument {other}").into()),
            }
        }
        let _com = ComGuard::new()?;
        let speakers = wasapi::find_device(Direction::Render, out.as_deref())?;
        println!("speakers: {}", speakers.name);
        let g = graph(&speakers.id, clip.as_deref());

        println!("-- spawn, apply graph v1");
        let (mut child, mut client, _) = spawn_host()?;
        println!("  reply: {:?}", client.apply_graph(g.clone(), 1)?);
        watch(&mut client, Duration::from_secs(2), "running");

        println!("-- tone param to 0, app param to 0 (fader moves, no rebuild)");
        client.set_param(TONE_GAIN, 0.0)?;
        client.set_param(APP_GAIN, 0.0)?;
        watch(&mut client, Duration::from_secs(1), "muted");
        client.set_param(TONE_GAIN, 1.0)?;
        client.set_param(APP_GAIN, 0.5)?;
        watch(&mut client, Duration::from_millis(500), "back");

        if clip.is_some() {
            println!("-- play the clip");
            client.trigger(CLIP, ClipAction::Play)?;
            let ev = watch(&mut client, Duration::from_secs(8), "clip");
            assert!(
                ev.iter().any(|e| matches!(e, EngineEvent::ClipEnded { .. })),
                "no ClipEnded"
            );
        }

        println!("-- a bad graph (edge from a node that doesn't exist)");
        let mut bad = g.clone();
        bad.edges.push(EdgeSpec {
            from: NodeId(99),
            to: NodeId(3),
            gain: None,
        });
        println!("  reply: {:?}", client.apply_graph(bad, 2)?);
        watch(&mut client, Duration::from_millis(500), "still v1");

        println!("-- kill the host (simulated crash)");
        let killed = Instant::now();
        child.kill()?;
        while client.is_connected() && killed.elapsed() < Duration::from_secs(5) {
            client.next_event(Duration::from_millis(5));
        }
        println!(
            "  controller side saw the pipe close after {:.1} ms (heartbeat timeout would be ~2 s)",
            killed.elapsed().as_secs_f64() * 1000.0
        );
        let _ = child.wait();

        println!("-- respawn, replay graph v1 and every param (the supervisor's job, M5)");
        let t = Instant::now();
        let (mut child, mut client, _) = spawn_host()?;
        println!("  reply: {:?}", client.apply_graph(g.clone(), 1)?);
        client.set_param(TONE_GAIN, 1.0)?;
        client.set_param(APP_GAIN, 0.5)?;
        let mut applied = None;
        while applied.is_none() && t.elapsed() < Duration::from_secs(5) {
            if let Some(EngineEvent::GraphApplied { version: 1 }) = client.next_event(Duration::from_millis(50)) {
                applied = Some(t.elapsed());
            }
        }
        match applied {
            Some(d) => println!(
                "  GraphApplied v1 {:.0} ms after the crash was noticed (respawn + handshake + replay)",
                d.as_secs_f64() * 1000.0
            ),
            None => println!("  no GraphApplied within 5 s"),
        }
        watch(&mut client, Duration::from_secs(1), "replayed");

        println!("-- shutdown");
        println!("  reply: {:?}", client.shutdown()?);
        let status = child.wait()?;
        println!("  host exited: {status}");

        println!("-- the controller vanishes (no Shutdown): the host must exit by itself");
        let (mut child, mut client, _) = spawn_host()?;
        println!("  reply: {:?}", client.apply_graph(g, 3)?);
        drop(client);
        let gone = Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break Some(s);
            }
            if gone.elapsed() > Duration::from_secs(5) {
                child.kill()?;
                break None;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        match status {
            Some(s) => println!(
                "  host exited {:.0} ms after the pipe closed: {s}",
                gone.elapsed().as_secs_f64() * 1000.0
            ),
            None => return Err("host still running 5 s after the controller left".into()),
        }
        Ok(())
    }
}
