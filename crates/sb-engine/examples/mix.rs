//! Milestones E2 + E3: a `GraphSpec`, compiled to a render plan, run by the mix thread.
//!
//!   mix [--in NAME|none] [--out NAME|timer] [--out2 NAME] [--tone HZ] [--tone-gain G]
//!       [--mic-gain G] [--block FRAMES] [--seconds N]
//!
//! Graph: mic and a test tone feed a bus; the bus feeds the `--out` device (the master: its
//! event clocks the mix and the mix writes it directly) and optionally `--out2` (through a
//! ring, on its own render thread). With `--out timer` a high-resolution timer clocks the mix.
//! Device NAME: part of the name or the full ID; omitted means the default device.

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
    use sb_engine::backend::wasapi::{self, ComGuard, Direction, WasapiEndpoints};
    use sb_engine::frames_to_ms;
    use sb_engine::graph::compile::compile;
    use sb_engine::graph::spec::{EdgeSpec, GraphSpec, NodeId, NodeKind, NodeSpec, ParamId, ParamSpec};
    use sb_engine::rt::mix_thread;
    use sb_engine::rt::stats::MixSnapshot;
    use std::time::{Duration, Instant};

    const TONE: NodeId = NodeId(1);
    const BUS: NodeId = NodeId(2);
    const MIC: NodeId = NodeId(3);
    const OUT: NodeId = NodeId(4);
    const OUT2: NodeId = NodeId(5);
    const MIC_GAIN: ParamId = ParamId(1);

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
                "-h" | "--help" => {
                    println!(
                        "usage: mix [--in NAME|none] [--out NAME|timer] [--out2 NAME] [--tone HZ] [--tone-gain G] \
                         [--mic-gain G] [--block FRAMES] [--seconds N]"
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

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let args = parse_args()?;
        let _com = ComGuard::new()?;

        let mut spec = GraphSpec::default();
        let mut add = |id, kind| spec.nodes.push(NodeSpec { id, kind });
        let mic = match args.no_input {
            true => None,
            false => Some(wasapi::find_device(Direction::Capture, args.input.as_deref())?),
        };
        let out = match args.timer {
            true => None,
            false => Some(wasapi::find_device(Direction::Render, args.output.as_deref())?),
        };
        let out2 = args
            .output2
            .as_deref()
            .map(|n| wasapi::find_device(Direction::Render, Some(n)))
            .transpose()?;

        add(
            TONE,
            NodeKind::Tone {
                freq: args.tone,
                gain: args.tone_gain,
            },
        );
        add(BUS, NodeKind::Bus);
        let mut edges = vec![(TONE, BUS, None)];
        if let Some(d) = &mic {
            add(MIC, NodeKind::DeviceInput { device: d.id.clone() });
            edges.push((MIC, BUS, Some(MIC_GAIN)));
            println!("in:     {}", d.name);
        }
        if let Some(d) = &out {
            add(OUT, NodeKind::DeviceOutput { device: d.id.clone() });
            edges.push((BUS, OUT, None));
            println!("master: {}", d.name);
        } else {
            println!("master: timer clock");
        }
        if let Some(d) = &out2 {
            add(OUT2, NodeKind::DeviceOutput { device: d.id.clone() });
            edges.push((BUS, OUT2, None));
            println!("out2:   {}", d.name);
        }
        spec.edges = edges
            .into_iter()
            .map(|(from, to, gain)| EdgeSpec { from, to, gain })
            .collect();
        spec.params = vec![ParamSpec {
            id: MIC_GAIN,
            value: args.mic_gain,
        }];
        spec.master = out.as_ref().map(|_| OUT);

        let mut endpoints = WasapiEndpoints::new(args.block, out.as_ref().map(|d| d.id.as_str()))?;
        let compiled = compile(&spec, args.block, &mut endpoints)?;
        let plan = &compiled.plan;
        println!(
            "plan: {} ops, {} buffers, {} frames ({:.2} ms) per block",
            plan.ops().len(),
            plan.buffer_count(),
            plan.block(),
            frames_to_ms(plan.block())
        );
        for op in plan.ops() {
            println!("  {op:?}");
        }

        let mix = mix_thread::spawn(compiled.plan, compiled.master_device.as_deref())?;
        if let Some(d) = mix.master.device {
            println!(
                "master device: period {:.1} ms, buffer {:.1} ms",
                frames_to_ms(d.period_frames),
                frames_to_ms(d.buffer_frames)
            );
        }
        println!(
            "mix thread MMCSS {}",
            if mix.stats.snapshot().mmcss { "on" } else { "OFF" }
        );

        let start = Instant::now();
        let mut last = MixSnapshot::default();
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let m = mix.stats.snapshot();
            let blocks = m.blocks - last.blocks;
            let budget_ns = blocks as f64 * frames_to_ms(args.block) * 1e6;
            print!(
                "{:>5.0}s  mix {:>4} blk/s load {:>5.2}% max {:>6.1} us late {:<3}{}",
                start.elapsed().as_secs_f64(),
                blocks,
                if budget_ns > 0.0 {
                    (m.busy_ns - last.busy_ns) as f64 / budget_ns * 100.0
                } else {
                    0.0
                },
                m.max_block_ns as f64 / 1000.0,
                m.late_blocks,
                if m.master_fallback { " MASTER LOST→timer" } else { "" },
            );
            for (_, s) in &compiled.input_stats {
                let r = s.snapshot();
                print!(
                    " | mic ring underruns {:<3} trimmed {:<6} fill {:>5.1} ms",
                    r.xruns,
                    r.trimmed_frames,
                    frames_to_ms(r.ring_fill as usize)
                );
            }
            for (node, stream) in endpoints.streams.iter().filter(|(n, _)| *n == OUT2) {
                let s = stream.stats.snapshot();
                print!(
                    " | out2 {} underruns {:<3} fill {:>5.1} ms",
                    node.0,
                    s.xruns,
                    frames_to_ms(s.ring_fill as usize)
                );
            }
            println!();
            last = m;

            let ended = mix.is_finished() || endpoints.streams.iter().any(|(_, s)| s.is_finished());
            if ended || args.seconds.is_some_and(|s| start.elapsed().as_secs() >= s) {
                break;
            }
        }

        let m = mix.stats.snapshot();
        let mut failed = false;
        let mut report = |what: String, r: windows::core::Result<()>| {
            let how = match r {
                Ok(()) => "ok".to_string(),
                Err(e) => {
                    failed = true;
                    format!("FAILED: {e}")
                }
            };
            println!("{what} | {how}");
        };
        report(
            format!(
                "mix: {} blocks, max block {:.1} us, {} late, master fallback {}",
                m.blocks,
                m.max_block_ns as f64 / 1000.0,
                m.late_blocks,
                m.master_fallback
            ),
            mix.stop(),
        );
        for (_, s) in &compiled.input_stats {
            let r = s.snapshot();
            println!(
                "mic ring (mix side): {} underruns ({} frames), {} trimmed",
                r.xruns, r.xrun_frames, r.trimmed_frames
            );
        }
        for (node, stream) in endpoints.streams {
            let s = stream.stats.snapshot();
            let what = format!(
                "node {} device thread: {} frames, {} xruns ({} frames), {} trimmed",
                node.0, s.frames, s.xruns, s.xrun_frames, s.trimmed_frames
            );
            report(what, stream.stop());
        }
        if failed { Err("a thread failed".into()) } else { Ok(()) }
    }
}
