//! Milestone E2: one mix thread running a hand-written plan in fixed blocks.
//!
//!   mix [--in NAME|none] [--out NAME|timer] [--out2 NAME] [--tone HZ] [--tone-gain G]
//!       [--mic-gain G] [--block FRAMES] [--seconds N]
//!
//! Plan: mic (capture thread → ring) and a test tone are mixed onto a bus. The bus is the
//! master: written straight into the `--out` device, whose event clocks the mix. With
//! `--out timer` a high-resolution timer clocks it instead and nothing plays directly.
//! `--out2` also sends the bus through a ring to a second device on its own render thread.
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
    use sb_engine::backend::wasapi::{self, ComGuard, Direction, Stream};
    use sb_engine::graph::plan::{Op, PlanBuilder};
    use sb_engine::rt::mix_thread;
    use sb_engine::rt::ring::{RingReader, ring};
    use sb_engine::rt::stats::{MixSnapshot, StreamStats};
    use sb_engine::{frames_to_ms, ms_to_frames};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

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
        let block = args.block;

        let mut b = PlanBuilder::new(block);
        let bus = b.buffer();
        let tone = b.buffer();
        let osc = b.oscillator();
        b.op(Op::Sine {
            osc,
            dst: tone,
            freq: args.tone,
            gain: args.tone_gain,
        })
        .op(Op::Clear { dst: bus })
        .op(Op::MixAdd {
            src: tone,
            dst: bus,
            gain: 1.0,
        });

        let master_dev = match args.timer {
            true => None,
            false => Some(wasapi::find_device(Direction::Render, args.output.as_deref())?),
        };
        // How much the mix moves per wake-up: a device master renders its whole period as
        // back-to-back blocks; the timer wakes once per block.
        let mix_burst = match &master_dev {
            Some(d) => wasapi::default_period_frames(&d.id)?.div_ceil(block) * block,
            None => block,
        };
        // A ring's fill swings by the producer's burst plus the consumer's burst (E1 finding),
        // so that is the least it can start from without underrunning.
        let mut capture = None;
        let mut mic_ring_stats = None;
        if !args.no_input {
            let dev = wasapi::find_device(Direction::Capture, args.input.as_deref())?;
            let target = wasapi::default_period_frames(&dev.id)? + mix_burst;
            let max = target + ms_to_frames(30.0);
            let (writer, consumer) = ring((max + ms_to_frames(200.0)).next_power_of_two());
            let (ring_id, stats) = b.input(RingReader::new(consumer, target, max));
            let mic = b.buffer();
            b.op(Op::Pull {
                ring: ring_id,
                dst: mic,
            })
            .op(Op::MixAdd {
                src: mic,
                dst: bus,
                gain: args.mic_gain,
            });
            println!("in:   {} (ring target {:.1} ms)", dev.name, frames_to_ms(target));
            capture = Some(wasapi::spawn_capture(&dev.id, writer)?);
            mic_ring_stats = Some(stats);
        }

        let master = match master_dev {
            Some(dev) => {
                println!("master: {}", dev.name);
                b.op(Op::Master { src: bus });
                Some(dev.id)
            }
            None => {
                println!("master: timer clock");
                None
            }
        };

        let mut render2 = None;
        let mut out2_ring_stats = None;
        if let Some(name) = &args.output2 {
            let dev = wasapi::find_device(Direction::Render, Some(name))?;
            let target = wasapi::default_period_frames(&dev.id)? + mix_burst;
            let max = target + ms_to_frames(30.0);
            let (writer, consumer) = ring((max + ms_to_frames(200.0)).next_power_of_two());
            let (ring_id, stats) = b.output(writer);
            b.op(Op::Push {
                src: bus,
                ring: ring_id,
            });
            println!("out2: {} (ring target {:.1} ms)", dev.name, frames_to_ms(target));
            render2 = Some(wasapi::spawn_render(&dev.id, RingReader::new(consumer, target, max))?);
            out2_ring_stats = Some(stats);
        }

        let plan = b.build();
        println!("plan ({} frames = {:.2} ms per block):", block, frames_to_ms(block));
        for op in plan.ops() {
            println!("  {op:?}");
        }

        let mix = mix_thread::spawn(plan, master.as_deref())?;
        if let Some(d) = mix.master.device {
            println!(
                "master device: period {:.1} ms, buffer {:.1} ms",
                frames_to_ms(d.period_frames),
                frames_to_ms(d.buffer_frames)
            );
        }
        println!("mix thread MMCSS {}", on_off(mix.stats.snapshot().mmcss));

        let start = Instant::now();
        let mut last = MixSnapshot::default();
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let m = mix.stats.snapshot();
            let blocks = m.blocks - last.blocks;
            let busy = m.busy_ns - last.busy_ns;
            let budget_ns = blocks as f64 * frames_to_ms(block) * 1e6;
            print!(
                "{:>5.0}s  mix {:>4} blk/s load {:>5.2}% max {:>6.1} us late {:<3}{}",
                start.elapsed().as_secs_f64(),
                blocks,
                if budget_ns > 0.0 {
                    busy as f64 / budget_ns * 100.0
                } else {
                    0.0
                },
                m.max_block_ns as f64 / 1000.0,
                m.late_blocks,
                if m.master_fallback { " MASTER LOST→timer" } else { "" },
            );
            if let Some(s) = &mic_ring_stats {
                let r = s.snapshot();
                print!(
                    " | mic ring underruns {:<3} trimmed {:<6} fill {:>5.1} ms",
                    r.xruns,
                    r.trimmed_frames,
                    fill_ms(s)
                );
            }
            if let Some(r) = &render2 {
                let s = r.stats.snapshot();
                print!(" | out2 underruns {:<3} fill {:>5.1} ms", s.xruns, fill_ms(&r.stats));
            }
            println!();
            last = m;

            let ended = mix.is_finished()
                || capture.as_ref().is_some_and(Stream::is_finished)
                || render2.as_ref().is_some_and(Stream::is_finished);
            if ended || args.seconds.is_some_and(|s| start.elapsed().as_secs() >= s) {
                break;
            }
        }

        let m = mix.stats.snapshot();
        let mut failed = false;
        println!(
            "mix: {} blocks, max block {:.1} us, {} late, master fallback {}, {}",
            m.blocks,
            m.max_block_ns as f64 / 1000.0,
            m.late_blocks,
            m.master_fallback,
            ended_how(mix.stop(), &mut failed)
        );
        if let (Some(c), Some(ring)) = (capture, mic_ring_stats) {
            let (s, r) = (c.stats.snapshot(), ring.snapshot());
            println!(
                "capture: {} frames, {} overruns | mic ring: {} underruns ({} frames), {} trimmed | {}",
                s.frames,
                s.xruns,
                r.xruns,
                r.xrun_frames,
                r.trimmed_frames,
                ended_how(c.stop(), &mut failed)
            );
        }
        if let (Some(rd), Some(ring)) = (render2, out2_ring_stats) {
            let (s, w) = (rd.stats.snapshot(), ring.snapshot());
            println!(
                "out2: {} frames, {} underruns ({} frames), {} trimmed | ring overruns {} | {}",
                s.frames,
                s.xruns,
                s.xrun_frames,
                s.trimmed_frames,
                w.xruns,
                ended_how(rd.stop(), &mut failed)
            );
        }
        if failed { Err("a thread failed".into()) } else { Ok(()) }
    }

    fn fill_ms(s: &Arc<StreamStats>) -> f64 {
        frames_to_ms(s.snapshot().ring_fill as usize)
    }

    fn on_off(b: bool) -> &'static str {
        if b { "on" } else { "OFF" }
    }

    fn ended_how(r: windows::core::Result<()>, failed: &mut bool) -> String {
        match r {
            Ok(()) => "ok".into(),
            Err(e) => {
                *failed = true;
                format!("FAILED: {e}")
            }
        }
    }
}
