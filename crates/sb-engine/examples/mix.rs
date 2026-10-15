//! Milestones E2–E6: the engine running a `GraphSpec` on the mix thread, with live params,
//! meters, glitch-free graph updates, and drift correction on every device ring.
//!
//!   mix [--in NAME|none] [--out NAME|timer] [--out2 NAME] [--tone HZ] [--tone-gain G]
//!       [--mic-gain G] [--pulse] [--reroute] [--detail] [--block FRAMES] [--seconds N]
//!
//! Graph: mic and a test tone feed a bus; the bus feeds the `--out` device (the master: its
//! event clocks the mix and the mix writes it directly) and optionally `--out2` (through a
//! ring, on its own render thread). With `--out timer` a high-resolution timer clocks the mix.
//! Device NAME: part of the name or the full ID; omitted means the default device.
//!
//! The tone reaches the bus through a param edge; `--pulse` switches that param between 1
//! and 0 every second while running (smoothed, no clicks). `--reroute` applies a new graph
//! every 2 seconds, swapping the tone for a different tone node (a 5 ms crossfade); the mic
//! stays connected and its device is never reopened. Each status line shows the mic and bus
//! meters in dBFS, how many plans the mix thread has adopted, and each ring's drift
//! correction. `--detail` adds the device side of every ring and reports every 0.25 s for
//! the first minute (to watch the drift controllers converge).

#[cfg(not(windows))]
fn main() {
    eprintln!("mix uses WASAPI and only runs on Windows");
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
    use sb_engine::backend::wasapi::{self, ComGuard, DeviceInfo, Direction};
    use sb_engine::engine::start_wasapi;
    use sb_engine::frames_to_ms;
    use sb_engine::graph::compile::Tables;
    use sb_engine::graph::spec::{
        EdgeSpec, GraphSpec, MeterId, MeterSpec, NodeId, NodeKind, NodeSpec, ParamId, ParamSpec,
    };
    use sb_engine::rt::meters::to_db;
    use sb_engine::rt::stats::MixSnapshot;
    use std::time::{Duration, Instant};

    const TONE: NodeId = NodeId(1);
    /// The other tone `--reroute` switches to (a different node, a fifth higher).
    const TONE_ALT: NodeId = NodeId(6);
    const BUS: NodeId = NodeId(2);
    const MIC: NodeId = NodeId(3);
    const OUT: NodeId = NodeId(4);
    const OUT2: NodeId = NodeId(5);
    const MIC_GAIN: ParamId = ParamId(1);
    const TONE_ON: ParamId = ParamId(2);
    const MIC_METER: MeterId = MeterId(0);
    const BUS_METER: MeterId = MeterId(1);

    struct Args {
        input: Option<String>,
        no_input: bool,
        output: Option<String>,
        timer: bool,
        output2: Option<String>,
        tone: f32,
        tone_gain: f32,
        mic_gain: f32,
        block: usize,
        seconds: Option<u64>,
        pulse: bool,
        reroute: bool,
        detail: bool,
    }

    fn parse_args() -> Result<Args, String> {
        let mut a = Args {
            input: None,
            no_input: false,
            output: None,
            timer: false,
            output2: None,
            tone: 440.0,
            tone_gain: 0.1,
            mic_gain: 1.0,
            block: 256,
            seconds: None,
            pulse: false,
            reroute: false,
            detail: false,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            let mut value = || it.next().ok_or(format!("{arg} needs a value"));
            fn num<T: std::str::FromStr>(arg: &str, v: String) -> Result<T, String> {
                v.parse().map_err(|_| format!("{arg}: not a number"))
            }
            match arg.as_str() {
                "--in" => match value()?.as_str() {
                    "none" => a.no_input = true,
                    v => a.input = Some(v.into()),
                },
                "--out" => match value()?.as_str() {
                    "timer" => a.timer = true,
                    v => a.output = Some(v.into()),
                },
                "--out2" => a.output2 = Some(value()?),
                "--tone" => a.tone = num(&arg, value()?)?,
                "--tone-gain" => a.tone_gain = num(&arg, value()?)?,
                "--mic-gain" => a.mic_gain = num(&arg, value()?)?,
                "--block" => a.block = num(&arg, value()?)?,
                "--seconds" => a.seconds = Some(num(&arg, value()?)?),
                "--pulse" => a.pulse = true,
                "--reroute" => a.reroute = true,
                "--detail" => a.detail = true,
                "-h" | "--help" => {
                    println!(
                        "usage: mix [--in NAME|none] [--out NAME|timer] [--out2 NAME] [--tone HZ] [--tone-gain G] \
                         [--mic-gain G] [--pulse] [--reroute] [--detail] [--block FRAMES] [--seconds N]"
                    );
                    std::process::exit(0);
                }
                _ => return Err(format!("unknown argument {arg}")),
            }
        }
        if a.block == 0 {
            return Err("--block must be > 0".into());
        }
        Ok(a)
    }

    struct Devices {
        mic: Option<DeviceInfo>,
        out: Option<DeviceInfo>,
        out2: Option<DeviceInfo>,
    }

    /// The demo graph. `alt` uses the other tone node (for `--reroute`).
    fn build_spec(args: &Args, dev: &Devices, alt: bool) -> GraphSpec {
        let mut spec = GraphSpec::default();
        let (tone, freq) = if alt {
            (TONE_ALT, args.tone * 1.5)
        } else {
            (TONE, args.tone)
        };
        spec.nodes.push(NodeSpec {
            id: tone,
            kind: NodeKind::Tone {
                freq,
                gain: args.tone_gain,
            },
        });
        spec.nodes.push(NodeSpec {
            id: BUS,
            kind: NodeKind::Bus,
        });
        let mut edges = vec![(tone, BUS, Some(TONE_ON))];
        if let Some(d) = &dev.mic {
            spec.nodes.push(NodeSpec {
                id: MIC,
                kind: NodeKind::DeviceInput { device: d.id.clone() },
            });
            edges.push((MIC, BUS, Some(MIC_GAIN)));
            spec.meters.push(MeterSpec {
                id: MIC_METER,
                node: MIC,
            });
        }
        if let Some(d) = &dev.out {
            spec.nodes.push(NodeSpec {
                id: OUT,
                kind: NodeKind::DeviceOutput { device: d.id.clone() },
            });
            edges.push((BUS, OUT, None));
            spec.master = Some(OUT);
        }
        if let Some(d) = &dev.out2 {
            spec.nodes.push(NodeSpec {
                id: OUT2,
                kind: NodeKind::DeviceOutput { device: d.id.clone() },
            });
            edges.push((BUS, OUT2, None));
        }
        spec.edges = edges
            .into_iter()
            .map(|(from, to, gain)| EdgeSpec { from, to, gain })
            .collect();
        spec.params = vec![
            ParamSpec {
                id: MIC_GAIN,
                value: args.mic_gain,
            },
            ParamSpec {
                id: TONE_ON,
                value: 1.0,
            },
        ];
        spec.meters.push(MeterSpec {
            id: BUS_METER,
            node: BUS,
        });
        spec
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let args = parse_args()?;
        let _com = ComGuard::new()?;
        let dev = Devices {
            mic: (!args.no_input)
                .then(|| wasapi::find_device(Direction::Capture, args.input.as_deref()))
                .transpose()?,
            out: (!args.timer)
                .then(|| wasapi::find_device(Direction::Render, args.output.as_deref()))
                .transpose()?,
            out2: args
                .output2
                .as_deref()
                .map(|n| wasapi::find_device(Direction::Render, Some(n)))
                .transpose()?,
        };
        for (label, d) in [("in", &dev.mic), ("master", &dev.out), ("out2", &dev.out2)] {
            println!("{label:>6}: {}", d.as_ref().map_or("—".into(), |d| d.name.clone()));
        }

        let master = dev.out.as_ref().map(|d| d.id.as_str());
        let (mut engine, mix) = start_wasapi(args.block, Tables::new(16, 16), master)?;
        let mut alt = false;
        engine.apply_graph(&build_spec(&args, &dev, alt))?;
        println!(
            "mix thread MMCSS {}",
            if mix.stats.snapshot().mmcss { "on" } else { "OFF" }
        );

        let start = Instant::now();
        let mut last = MixSnapshot::default();
        let mut second = 0;
        loop {
            // maintain() often: it retires plans and queues settle plans after fades.
            let ticks = if args.detail && second < 60 { 5 } else { 20 };
            for _ in 0..ticks {
                std::thread::sleep(Duration::from_millis(50));
                engine.maintain();
            }
            second += 1;
            if second == 1 {
                // The first plan has settled by now: show what's running.
                for (i, n) in engine.running_layout().nodes().iter().enumerate() {
                    println!("  node {i}: {} {:?}", n.id.0, n.kind);
                }
            }
            let m = mix.stats.snapshot();
            let blocks = m.blocks - last.blocks;
            let budget_ns = blocks as f64 * frames_to_ms(args.block) * 1e6;
            print!(
                "{:>5.0}s  mix {:>4} blk/s load {:>5.2}% max {:>6.1} us late {:<3} plans {:<3}{}",
                start.elapsed().as_secs_f64(),
                blocks,
                if budget_ns > 0.0 {
                    (m.busy_ns - last.busy_ns) as f64 / budget_ns * 100.0
                } else {
                    0.0
                },
                m.max_block_ns as f64 / 1000.0,
                m.late_blocks,
                m.swaps,
                if m.master_fallback { " MASTER LOST→timer" } else { "" },
            );
            for ep in engine.running_layout().inputs() {
                let r = ep.stats.snapshot();
                print!(
                    " | mic ring xr {:<2} trim {:<5} fill {:>5.1} ms drift {:>+6.0} ppm",
                    r.xruns,
                    r.trimmed_frames,
                    frames_to_ms(r.ring_fill as usize),
                    r.drift_ppm
                );
            }
            for ep in engine.running_layout().outputs() {
                let r = ep.stats.snapshot();
                print!(
                    " | out2 ring est {:>5.1} ms drift {:>+6.0} ppm",
                    frames_to_ms(r.ring_fill as usize),
                    r.drift_ppm
                );
            }
            let db = |v: f32| format!("{:>6.1}", to_db(v));
            if let Some(r) = engine.tables().meters.read(BUS_METER) {
                print!(
                    " | bus pk {} rms {} dB",
                    db(r.peak[0].max(r.peak[1])),
                    db(r.rms[0].max(r.rms[1]))
                );
            }
            if dev.mic.is_some()
                && let Some(r) = engine.tables().meters.read(MIC_METER)
            {
                print!(" | mic pk {} dB", db(r.peak[0].max(r.peak[1])));
            }
            if args.detail {
                for (_, node, st) in &engine.endpoints().streams {
                    let r = st.stats.snapshot();
                    print!(
                        " | dev{} fill {:>5.1} trim {} xr {}",
                        node.0,
                        frames_to_ms(r.ring_fill as usize),
                        r.trimmed_frames,
                        r.xruns
                    );
                }
            }
            println!(" | streams open {}", engine.endpoints().streams.len());
            last = m;

            if args.pulse {
                engine.set_param(TONE_ON, if second % 2 == 0 { 1.0 } else { 0.0 });
            }
            if args.reroute && second % 2 == 0 {
                alt = !alt;
                engine.apply_graph(&build_spec(&args, &dev, alt))?;
            }
            let ended = mix.is_finished() || engine.endpoints().streams.iter().any(|(_, _, s)| s.is_finished());
            if ended || args.seconds.is_some_and(|s| start.elapsed().as_secs() >= s) {
                break;
            }
        }

        let m = mix.stats.snapshot();
        println!(
            "mix: {} blocks, {} plans adopted, max block {:.1} us, {} late, master fallback {}",
            m.blocks,
            m.swaps,
            m.max_block_ns as f64 / 1000.0,
            m.late_blocks,
            m.master_fallback
        );
        for ep in engine.running_layout().inputs() {
            let r = ep.stats.snapshot();
            println!(
                "mic ring (mix side): {} underruns ({} frames), {} trimmed",
                r.xruns, r.xrun_frames, r.trimmed_frames
            );
        }
        let mut failed = mix.stop().map_err(|e| println!("mix thread FAILED: {e}")).is_err();
        for (key, node, stream) in std::mem::take(&mut engine.endpoints_mut().streams) {
            let s = stream.stats.snapshot();
            let how = match stream.stop() {
                Ok(()) => "ok".to_string(),
                Err(e) => {
                    failed = true;
                    format!("FAILED: {e}")
                }
            };
            println!(
                "endpoint {key} (node {}): {} frames, {} xruns ({} frames), {} trimmed | {how}",
                node.0, s.frames, s.xruns, s.xrun_frames, s.trimmed_frames
            );
        }
        if failed { Err("a thread failed".into()) } else { Ok(()) }
    }
}
