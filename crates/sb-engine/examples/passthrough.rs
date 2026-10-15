//! Milestone E1: capture device → ring → render device, each on an MMCSS thread.
//!
//!   passthrough --list
//!   passthrough [--in NAME] [--out NAME] [--seconds N] [--target-ms MS]
//!
//! The ring target defaults to one capture period plus one render period: capture arrives
//! in whole packets and render takes whole periods, so the fill swings by a packet and
//! anything lower underruns.
//!
//! NAME is a case-insensitive part of the device's name (or its full ID); omitted means the
//! default device. Runs until Enter is pressed, or for N seconds. Prints stats every second
//! and exits non-zero if a stream failed.

#[cfg(not(windows))]
fn main() {
    eprintln!("passthrough uses WASAPI and only runs on Windows");
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
    use sb_engine::rt::ring::{RingReader, ring};
    use sb_engine::rt::stats::StatsSnapshot;
    use sb_engine::{frames_to_ms, ms_to_frames};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    struct Args {
        list: bool,
        input: Option<String>,
        output: Option<String>,
        seconds: Option<u64>,
        target_ms: Option<f64>,
    }

    fn parse_args() -> Result<Args, String> {
        let mut a = Args {
            list: false,
            input: None,
            output: None,
            seconds: None,
            target_ms: None,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            let mut value = || it.next().ok_or(format!("{arg} needs a value"));
            match arg.as_str() {
                "--list" => a.list = true,
                "--in" => a.input = Some(value()?),
                "--out" => a.output = Some(value()?),
                "--seconds" => a.seconds = Some(value()?.parse().map_err(|_| "--seconds: not a number")?),
                "--target-ms" => a.target_ms = Some(value()?.parse().map_err(|_| "--target-ms: not a number")?),
                "-h" | "--help" => {
                    println!("usage: passthrough [--list] [--in NAME] [--out NAME] [--seconds N] [--target-ms MS]");
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

        if args.list {
            for dir in [Direction::Capture, Direction::Render] {
                println!("{dir:?} devices:");
                for d in wasapi::list_devices(dir)? {
                    println!("  {} {}\n      {}", if d.is_default { "*" } else { " " }, d.name, d.id);
                }
            }
            return Ok(());
        }

        let input = wasapi::find_device(Direction::Capture, args.input.as_deref())?;
        let output = wasapi::find_device(Direction::Render, args.output.as_deref())?;
        println!("in:  {}\nout: {}", input.name, output.name);

        // Ring sizing: start at target, trim back to it past target + 30 ms, room for 200 ms.
        let target = match args.target_ms {
            Some(ms) => ms_to_frames(ms),
            None => wasapi::default_period_frames(&input.id)? + wasapi::default_period_frames(&output.id)?,
        };
        let max = target + ms_to_frames(30.0);
        let (writer, consumer) = ring((max + ms_to_frames(200.0)).next_power_of_two());

        // Capture first, so packets are already flowing steadily when render starts priming.
        let capture = wasapi::spawn_capture(&input.id, writer)?;
        let render = wasapi::spawn_render(&output.id, RingReader::new(consumer, target, max))?;
        for (label, s) in [("capture", &capture), ("render", &render)] {
            println!(
                "{label}: period {:.1} ms, buffer {:.1} ms, MMCSS {}",
                frames_to_ms(s.info.period_frames),
                frames_to_ms(s.info.buffer_frames),
                if s.stats.snapshot().mmcss { "on" } else { "OFF" },
            );
        }
        println!(
            "ring target {:.1} ms, trim above {:.1} ms",
            frames_to_ms(target),
            frames_to_ms(max)
        );
        println!("running; press Enter to stop");

        let (enter_tx, enter_rx) = mpsc::channel();
        std::thread::spawn(move || {
            // EOF (no console, e.g. over ssh) means "no keyboard", not "stop".
            if std::io::stdin().read_line(&mut String::new()).is_ok_and(|n| n > 0) {
                let _ = enter_tx.send(());
            }
        });

        let start = Instant::now();
        let (mut last_in, mut last_out) = (StatsSnapshot::default(), StatsSnapshot::default());
        loop {
            let enter = match enter_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(()) => true,
                Err(mpsc::RecvTimeoutError::Timeout) => false,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(Duration::from_secs(1)); // no keyboard: just tick
                    false
                }
            };
            let stop = enter
                || args.seconds.is_some_and(|s| start.elapsed().as_secs() >= s)
                || capture.is_finished()
                || render.is_finished();
            let (i, o) = (capture.stats.snapshot(), render.stats.snapshot());
            println!(
                "{:>5.0}s  in {:>6} fr/s  overruns {:<3} disc {:<3} | out {:>6} fr/s  underruns {:<3} trimmed {:<6} fill {:>5.1} ms",
                start.elapsed().as_secs_f64(),
                i.frames - last_in.frames,
                i.xruns,
                i.discontinuities,
                o.frames - last_out.frames,
                o.xruns,
                o.trimmed_frames,
                frames_to_ms(o.ring_fill as usize),
            );
            (last_in, last_out) = (i, o);
            if stop {
                break;
            }
        }

        let failed = report("capture", capture) | report("render", render);
        if failed { Err("a stream failed".into()) } else { Ok(()) }
    }

    fn report(label: &str, stream: Stream) -> bool {
        let s = stream.stats.snapshot();
        let result = stream.stop();
        println!(
            "{label}: {} frames, {} xruns ({} frames), {} trimmed, {} discontinuities, max chunk {}, {}",
            s.frames,
            s.xruns,
            s.xrun_frames,
            s.trimmed_frames,
            s.discontinuities,
            s.max_chunk,
            match &result {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("FAILED: {e}"),
            }
        );
        result.is_err()
    }
}
