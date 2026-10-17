//! Milestone E8: clip playback through the engine.
//!
//!   clips <file> [--out NAME] [--overlap] [--loop] [--stream-after S]
//!
//! Loads the file (any format symphonia reads: Ogg/Vorbis, FLAC, WAV, MP3), routes the clip
//! node to the speakers, and runs a short script of triggers, printing each `ClipEnded`
//! event and the clip's meter:
//!   1. play it to the end;
//!   2. play, and retrigger it halfway (restart crossfade, or a second voice with --overlap);
//!   3. play, and stop it after a second (fade-out).
//!
//! `--stream-after S` streams clips longer than S seconds instead of decoding them whole.

#[cfg(not(windows))]
fn main() {
    eprintln!("clips uses WASAPI and only runs on Windows");
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
    use sb_engine::clips::ClipStore;
    use sb_engine::engine::start_wasapi;
    use sb_engine::graph::compile::Tables;
    use sb_engine::graph::spec::{
        ClipAction, EdgeSpec, GraphSpec, MeterId, MeterSpec, NodeId, NodeKind, NodeSpec, Retrigger,
    };
    use sb_engine::rt::meters::to_db;
    use std::time::{Duration, Instant};

    const CLIP: NodeId = NodeId(1);
    const OUT: NodeId = NodeId(2);
    const METER: MeterId = MeterId(0);

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let mut args = std::env::args().skip(1);
        let mut file = None;
        let (mut out_name, mut overlap, mut looped, mut stream_after) = (None, false, false, 60.0);
        while let Some(a) = args.next() {
            match a.as_str() {
                "--out" => out_name = args.next(),
                "--overlap" => overlap = true,
                "--loop" => looped = true,
                "--stream-after" => stream_after = args.next().ok_or("--stream-after needs seconds")?.parse()?,
                "-h" | "--help" => {
                    println!("usage: clips <file> [--out NAME] [--overlap] [--loop] [--stream-after S]");
                    return Ok(());
                }
                f => file = Some(f.to_owned()),
            }
        }
        let file = file.ok_or("usage: clips <file> [--out NAME] [--overlap] [--loop] [--stream-after S]")?;
        let _com = ComGuard::new()?;
        let out = wasapi::find_device(Direction::Render, out_name.as_deref())?;
        println!("clip:   {file}\nmaster: {}", out.name);

        let tables = Tables::with_clips(4, 4, ClipStore::new(stream_after, 2.0));
        let t = Instant::now();
        let clip = tables.clips.load(&file)?;
        println!(
            "loaded in {:.0} ms: {:.2} s in memory{}",
            t.elapsed().as_secs_f64() * 1000.0,
            clip.len() as f64 / 48_000.0,
            if clip.streamed { " (head; the rest streams)" } else { "" }
        );

        let retrigger = if overlap {
            Retrigger::Overlap
        } else {
            Retrigger::Restart
        };
        let spec = GraphSpec {
            nodes: vec![
                NodeSpec {
                    id: CLIP,
                    kind: NodeKind::Clip {
                        path: file.clone(),
                        polyphony: 4,
                        retrigger,
                        looped,
                    },
                },
                NodeSpec {
                    id: OUT,
                    kind: NodeKind::DeviceOutput { device: out.id.clone() },
                },
            ],
            edges: vec![EdgeSpec {
                from: CLIP,
                to: OUT,
                gain: None,
            }],
            meters: vec![MeterSpec { id: METER, node: CLIP }],
            master: Some(OUT),
            ..Default::default()
        };
        let (mut engine, mix) = start_wasapi(256, tables, Some(&out.id))?;
        engine.apply_graph(&spec)?;

        let start = Instant::now();
        let clip_secs = clip.len() as f64 / 48_000.0;
        let wait = |engine: &mut sb_engine::engine::Engine<_>, secs: f64, until_end: bool| {
            let deadline = Instant::now() + Duration::from_secs_f64(secs);
            let mut next_meter = Instant::now();
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
                engine.maintain();
                for ev in engine.poll_events() {
                    println!("{:>6.2}s  event {ev:?}", start.elapsed().as_secs_f64());
                    if until_end {
                        return;
                    }
                }
                if Instant::now() >= next_meter {
                    next_meter += Duration::from_millis(500);
                    if let Some(r) = engine.tables().meters.read(METER) {
                        let pk = r.peak[0].max(r.peak[1]);
                        println!(
                            "{:>6.2}s  clip pk {:>6.1} dB rms {:>6.1} dB",
                            start.elapsed().as_secs_f64(),
                            to_db(pk),
                            to_db(r.rms[0].max(r.rms[1]))
                        );
                    }
                }
            }
        };

        println!("-- 1. play to the end");
        engine.trigger(CLIP, ClipAction::Play);
        wait(&mut engine, if looped { 8.0 } else { clip_secs + 30.0 }, !looped);
        if looped {
            engine.trigger(CLIP, ClipAction::Stop);
            wait(&mut engine, 1.0, true);
        }

        println!(
            "-- 2. play, retrigger halfway ({})",
            if overlap { "second voice" } else { "restart" }
        );
        engine.trigger(CLIP, ClipAction::Play);
        wait(&mut engine, (clip_secs / 2.0).min(3.0), false);
        engine.trigger(CLIP, ClipAction::Play);
        wait(&mut engine, if looped { 4.0 } else { clip_secs + 30.0 }, !looped);
        if looped {
            engine.trigger(CLIP, ClipAction::Stop);
            wait(&mut engine, 1.0, true);
        }

        println!("-- 3. play, stop after 1 s");
        engine.trigger(CLIP, ClipAction::Play);
        wait(&mut engine, 1.0, false);
        engine.trigger(CLIP, ClipAction::Stop);
        wait(&mut engine, 2.0, true);

        let m = mix.stats.snapshot();
        println!(
            "mix: {} blocks, {} late, max block {:.1} us",
            m.blocks,
            m.late_blocks,
            m.max_block_ns as f64 / 1000.0
        );
        mix.stop()?;
        Ok(())
    }
}
