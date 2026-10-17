//! Milestone E7: the driver's virtual endpoints, end to end, with a user-mode stand-in for
//! the driver (`FakeDriver`) until the real one exists (D6).
//!
//!   virtual [--in NAME|none] [--out NAME] [--app-ppm P] [--vmic-ppm P] [--seconds N]
//!
//! The fake driver plays an "app" (a 660 Hz tone) into virtual endpoint 1, on a timer that
//! runs `--app-ppm` fast, and consumes virtual microphone 2 the way Discord would capture
//! it, on a timer `--vmic-ppm` off. The engine mixes the app with the mic (`--in`), plays the
//! result on the speakers (`--out`, the master), and feeds the same mix to the virtual mic.
//! Every second: each ring's drift correction, and what "apps" receive on the virtual mic.

#[cfg(not(windows))]
fn main() {
    eprintln!("virtual uses WASAPI and only runs on Windows");
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
    use sb_engine::backend::driver::{FakeDriver, FakeEndpoint};
    use sb_engine::backend::wasapi::{self, ComGuard, Direction};
    use sb_engine::engine::start_wasapi_with_driver;
    use sb_engine::frames_to_ms;
    use sb_engine::graph::compile::Tables;
    use sb_engine::graph::spec::{EdgeSpec, GraphSpec, MeterId, MeterSpec, NodeId, NodeKind, NodeSpec};
    use sb_engine::rt::meters::to_db;
    use sb_ring::ioctl::Direction as Flow;
    use std::sync::atomic::Ordering::Relaxed;
    use std::time::{Duration, Instant};

    const APP: NodeId = NodeId(1);
    const MIC: NodeId = NodeId(2);
    const BUS: NodeId = NodeId(3);
    const OUT: NodeId = NodeId(4);
    const VMIC: NodeId = NodeId(5);
    const APP_ENDPOINT: u32 = 1;
    const VMIC_ENDPOINT: u32 = 2;
    const APP_METER: MeterId = MeterId(0);

    struct Args {
        input: Option<String>,
        no_input: bool,
        output: Option<String>,
        app_ppm: f64,
        vmic_ppm: f64,
        seconds: Option<u64>,
    }

    fn parse_args() -> Result<Args, String> {
        let mut a = Args {
            input: None,
            no_input: false,
            output: None,
            app_ppm: 300.0,
            vmic_ppm: -250.0,
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
                "--out" => a.output = Some(value()?),
                "--app-ppm" => a.app_ppm = num(&arg, value()?)?,
                "--vmic-ppm" => a.vmic_ppm = num(&arg, value()?)?,
                "--seconds" => a.seconds = Some(num(&arg, value()?)?),
                "-h" | "--help" => {
                    println!("usage: virtual [--in NAME|none] [--out NAME] [--app-ppm P] [--vmic-ppm P] [--seconds N]");
                    std::process::exit(0);
                }
                _ => return Err(format!("unknown argument {arg}")),
            }
        }
        Ok(a)
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let args = parse_args()?;
        let _com = ComGuard::new()?;
        let out = wasapi::find_device(Direction::Render, args.output.as_deref())?;
        let mic = (!args.no_input)
            .then(|| wasapi::find_device(Direction::Capture, args.input.as_deref()))
            .transpose()?;

        let driver = FakeDriver::new(vec![
            FakeEndpoint {
                id: APP_ENDPOINT,
                direction: Flow::FromApps,
                period_frames: 480,
                ppm: args.app_ppm,
                tone_hz: 660.0,
            },
            FakeEndpoint {
                id: VMIC_ENDPOINT,
                direction: Flow::ToApps,
                period_frames: 480,
                ppm: args.vmic_ppm,
                tone_hz: 0.0,
            },
        ]);
        let vmic_stats = driver.stats(VMIC_ENDPOINT).expect("configured above").clone();
        println!(
            "app:    fake driver endpoint {APP_ENDPOINT} (660 Hz, {:+} ppm)",
            args.app_ppm
        );
        println!("mic:    {}", mic.as_ref().map_or("—".into(), |d| d.name.clone()));
        println!("master: {}", out.name);
        println!("vmic:   fake driver endpoint {VMIC_ENDPOINT} ({:+} ppm)", args.vmic_ppm);

        let mut spec = GraphSpec {
            nodes: vec![
                NodeSpec {
                    id: APP,
                    kind: NodeKind::VirtualSource { endpoint: APP_ENDPOINT },
                },
                NodeSpec {
                    id: BUS,
                    kind: NodeKind::Bus,
                },
                NodeSpec {
                    id: OUT,
                    kind: NodeKind::DeviceOutput { device: out.id.clone() },
                },
                NodeSpec {
                    id: VMIC,
                    kind: NodeKind::VirtualMic {
                        endpoint: VMIC_ENDPOINT,
                    },
                },
            ],
            edges: vec![(APP, BUS), (BUS, OUT), (BUS, VMIC)]
                .into_iter()
                .map(|(from, to)| EdgeSpec { from, to, gain: None })
                .collect(),
            meters: vec![MeterSpec {
                id: APP_METER,
                node: APP,
            }],
            master: Some(OUT),
            ..Default::default()
        };
        if let Some(d) = &mic {
            spec.nodes.push(NodeSpec {
                id: MIC,
                kind: NodeKind::DeviceInput { device: d.id.clone() },
            });
            spec.edges.push(EdgeSpec {
                from: MIC,
                to: BUS,
                gain: None,
            });
        }

        let (mut engine, mix) = start_wasapi_with_driver(256, Tables::new(16, 16), Some(&out.id), Box::new(driver))?;
        engine.apply_graph(&spec)?;

        let start = Instant::now();
        let mut last_frames = 0;
        loop {
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(50));
                engine.maintain();
            }
            let m = mix.stats.snapshot();
            print!(
                "{:>5.0}s  plans {} late {}",
                start.elapsed().as_secs_f64(),
                m.swaps,
                m.late_blocks
            );
            let layout = engine.running_layout();
            for ep in layout.inputs().iter().chain(layout.outputs()) {
                let s = ep.stats.snapshot();
                let name = match ep.node {
                    APP => "app",
                    MIC => "mic",
                    VMIC => "vmic",
                    _ => "?",
                };
                print!(
                    " | {name} xr {} tr {} drift {:+5.0} ppm",
                    s.xruns, s.trimmed_frames, s.drift_ppm
                );
            }
            if let Some(r) = engine.tables().meters.read(APP_METER) {
                print!(" | app pk {:>6.1} dB", to_db(r.peak[0]));
            }
            let frames = vmic_stats.frames.load(Relaxed);
            let peak = f32::from_bits(vmic_stats.peak.swap(0, Relaxed));
            println!(
                " | apps capture {:>5} fr/s, underruns {}, pk {:>6.1} dB",
                frames - last_frames,
                vmic_stats.underruns.load(Relaxed),
                to_db(peak)
            );
            last_frames = frames;
            if mix.is_finished() || args.seconds.is_some_and(|s| start.elapsed().as_secs() >= s) {
                break;
            }
        }

        let m = mix.stats.snapshot();
        println!(
            "mix: {} blocks, {} late, max block {:.1} us | vmic: {} frames captured by apps, {} underrun frames ({:.1} ms)",
            m.blocks,
            m.late_blocks,
            m.max_block_ns as f64 / 1000.0,
            vmic_stats.frames.load(Relaxed),
            vmic_stats.underruns.load(Relaxed),
            frames_to_ms(vmic_stats.underruns.load(Relaxed) as usize)
        );
        mix.stop()?;
        Ok(())
    }
}
