//! The mix thread: wait for the master clock, run the render plan once per due block, hand
//! the master bus to the clock. [`render_block`] is the op interpreter; [`run`] is the loop.
//! Both are platform-independent and tested with a fake clock; [`spawn`] (Windows) puts the
//! loop on an MMCSS thread with denormals flushed.

use crate::SAMPLE_RATE;
use crate::clock::master::MasterClock;
use crate::graph::plan::{Op, RenderPlan};
use crate::rt::stats::MixStats;
use std::f64::consts::TAU;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Instant;

/// Run every op in the plan once. Returns the master bus (left, right), or silence if the
/// plan has no `Master` op. Real-time safe: the builder validated every index.
pub fn render_block(plan: &mut RenderPlan) -> [&[f32]; 2] {
    let RenderPlan {
        block,
        ops,
        bufs,
        inputs,
        outputs,
        osc_phase,
        frames,
        silence,
    } = plan;
    let mut master = None;
    for op in ops.iter() {
        match *op {
            Op::Pull { ring, dst } => {
                let input = &mut inputs[ring.0];
                input.reader.read(frames, &input.stats);
                let [l, r] = &mut bufs[dst.0];
                for ((f, l), r) in frames.iter().zip(l.iter_mut()).zip(r.iter_mut()) {
                    (*l, *r) = (f[0], f[1]);
                }
            }
            Op::Sine { osc, dst, freq, gain } => {
                let phase = &mut osc_phase[osc.0];
                let step = freq as f64 / SAMPLE_RATE as f64;
                let [l, r] = &mut bufs[dst.0];
                for (l, r) in l.iter_mut().zip(r.iter_mut()) {
                    let v = (*phase * TAU).sin() as f32 * gain;
                    (*l, *r) = (v, v);
                    *phase = (*phase + step).fract();
                }
            }
            Op::Clear { dst } => bufs[dst.0].iter_mut().for_each(|ch| ch.fill(0.0)),
            Op::MixAdd { src, dst, gain } => {
                if let Ok([s, d]) = bufs.get_disjoint_mut([src.0, dst.0]) {
                    for (s, d) in s.iter().zip(d.iter_mut()) {
                        for (s, d) in s.iter().zip(d.iter_mut()) {
                            *d += s * gain;
                        }
                    }
                }
            }
            Op::Push { src, ring } => {
                let [l, r] = &bufs[src.0];
                for ((f, l), r) in frames.iter_mut().zip(l).zip(r) {
                    *f = [*l, *r];
                }
                let output = &mut outputs[ring.0];
                output.writer.write(Some(frames), *block, &output.stats);
            }
            Op::Master { src } => master = Some(src.0),
        }
    }
    match master {
        Some(i) => [&bufs[i][0], &bufs[i][1]],
        None => [&silence[0], &silence[1]],
    }
}

/// The block loop. Runs until `stop` is set.
pub fn run(plan: &mut RenderPlan, clock: &mut impl MasterClock, stats: &MixStats, stop: &AtomicBool) {
    while !stop.load(Relaxed) {
        for _ in 0..clock.wait() {
            let start = Instant::now();
            let [l, r] = render_block(plan);
            clock.deliver(l, r);
            stats.block_done(start.elapsed().as_nanos() as u64);
        }
    }
}

/// Flush denormals to zero on this thread (FTZ + DAZ). Quiet filter and reverb tails decay
/// into denormals, which are ~100× slower on x86.
pub fn flush_denormals() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let mut csr = 0u32;
        std::arch::asm!("stmxcsr [{}]", in(reg) &mut csr, options(nostack));
        csr |= 0x8040; // FTZ (bit 15) | DAZ (bit 6)
        std::arch::asm!("ldmxcsr [{}]", in(reg) &csr, options(nostack));
    }
}

#[cfg(windows)]
pub use win::{MixThread, spawn};

#[cfg(windows)]
mod win {
    use super::{flush_denormals, run};
    use crate::backend::wasapi::{Mmcss, RtThread};
    use crate::clock::master::{Clock, MasterInfo};
    use crate::graph::plan::RenderPlan;
    use crate::rt::stats::MixStats;
    use std::sync::Arc;
    use windows::core::Result;

    /// The running mix thread. Dropping it stops the thread and waits for it.
    pub struct MixThread {
        pub master: MasterInfo,
        pub stats: Arc<MixStats>,
        thread: RtThread,
    }

    impl MixThread {
        pub fn is_finished(&self) -> bool {
            self.thread.is_finished()
        }

        pub fn stop(mut self) -> Result<()> {
            self.thread.join()
        }
    }

    /// Start the mix thread. `master`: render endpoint ID whose event drives the mix and whose
    /// buffer gets the plan's `Master` bus, or `None` to run on the timer clock.
    pub fn spawn(plan: RenderPlan, master: Option<&str>) -> Result<MixThread> {
        let master = master.map(str::to_owned);
        let stats = Arc::new(MixStats::default());
        let thread_stats = stats.clone();
        let (thread, info) = RtThread::spawn(
            "sb-mix",
            move || {
                let mmcss = Mmcss::register(&thread_stats.mmcss);
                flush_denormals();
                let clock = Clock::new(master.as_deref(), plan.block(), thread_stats.clone())?;
                let info = clock.info();
                Ok(((mmcss, clock, plan, thread_stats), info))
            },
            |(_mmcss, mut clock, mut plan, stats), stop| {
                run(&mut plan, &mut clock, &stats, stop);
                Ok(())
            },
        )?;
        Ok(MixThread {
            master: info,
            stats,
            thread,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::plan::{BufId, PlanBuilder};
    use crate::rt::ring::{RingReader, ring};
    use crate::rt::stats::StreamStats;
    use std::sync::Arc;

    /// Due one block per wait for `blocks` waits, then sets `stop`. Records the master bus.
    struct FakeClock {
        left: usize,
        stop: Arc<AtomicBool>,
        out: Vec<[f32; 2]>,
    }

    impl MasterClock for FakeClock {
        fn wait(&mut self) -> usize {
            if self.left == 0 {
                self.stop.store(true, Relaxed);
                return 0;
            }
            self.left -= 1;
            1
        }
        fn deliver(&mut self, l: &[f32], r: &[f32]) {
            self.out.extend(l.iter().zip(r).map(|(l, r)| [*l, *r]));
        }
    }

    fn render(plan: &mut RenderPlan, blocks: usize) -> (Vec<[f32; 2]>, MixStats) {
        let stop = Arc::new(AtomicBool::new(false));
        let mut clock = FakeClock {
            left: blocks,
            stop: stop.clone(),
            out: Vec::new(),
        };
        let stats = MixStats::default();
        run(plan, &mut clock, &stats, &stop);
        (clock.out, stats)
    }

    fn sine(b: &mut PlanBuilder, freq: f32, gain: f32) -> BufId {
        let (osc, dst) = (b.oscillator(), b.buffer());
        b.op(Op::Sine { osc, dst, freq, gain });
        dst
    }

    #[test]
    fn sine_to_master() {
        let mut b = PlanBuilder::new(64);
        let s = sine(&mut b, 1000.0, 0.5);
        b.op(Op::Master { src: s });
        let (out, stats) = render(&mut b.build(), 10);
        assert_eq!(out.len(), 640);
        assert_eq!(stats.snapshot().blocks, 10);
        // 1 kHz at 48 kHz: period of 48 samples, continuous across block boundaries.
        for (i, f) in out.iter().enumerate() {
            let want = (i as f64 / 48.0 * TAU).sin() as f32 * 0.5;
            assert!(
                (f[0] - want).abs() < 1e-4 && f[0] == f[1],
                "sample {i}: {f:?} vs {want}"
            );
        }
    }

    #[test]
    fn no_master_op_plays_silence() {
        let mut b = PlanBuilder::new(32);
        sine(&mut b, 440.0, 1.0);
        let (out, _) = render(&mut b.build(), 3);
        assert!(out.iter().all(|f| *f == [0.0, 0.0]));
    }

    #[test]
    fn mixes_with_gains_onto_a_bus() {
        let mut b = PlanBuilder::new(16);
        let (in_w, in_c) = ring(64);
        let (ring_id, _) = b.input(RingReader::new(in_c, 0, 64));
        let (mic, bus) = (b.buffer(), b.buffer());
        let tone = sine(&mut b, 0.0, 1.0); // freq 0 at phase 0 → constant sin(0) = 0
        b.op(Op::Pull {
            ring: ring_id,
            dst: mic,
        })
        .op(Op::Clear { dst: bus })
        .op(Op::MixAdd {
            src: mic,
            dst: bus,
            gain: 0.5,
        })
        .op(Op::MixAdd {
            src: tone,
            dst: bus,
            gain: 1.0,
        })
        .op(Op::MixAdd {
            src: mic,
            dst: bus,
            gain: 0.25,
        })
        .op(Op::Master { src: bus });
        let mut plan = b.build();

        let mut w = in_w;
        let input: Vec<[f32; 2]> = (0..16).map(|i| [i as f32, -1.0]).collect();
        w.write(Some(&input), 16, &StreamStats::default());
        let (out, _) = render(&mut plan, 1);
        for (i, f) in out.iter().enumerate() {
            assert_eq!(*f, [i as f32 * 0.75, -0.75]);
        }
    }

    #[test]
    fn clear_resets_the_bus_every_block() {
        let mut b = PlanBuilder::new(8);
        let s = sine(&mut b, 6000.0, 1.0);
        let bus = b.buffer();
        b.op(Op::Clear { dst: bus })
            .op(Op::MixAdd {
                src: s,
                dst: bus,
                gain: 1.0,
            })
            .op(Op::Master { src: bus });
        let (out, _) = render(&mut b.build(), 20);
        assert!(out.iter().all(|f| f[0].abs() <= 1.0), "bus accumulated across blocks");
    }

    #[test]
    fn push_feeds_an_output_ring_and_pull_underrun_is_silent() {
        let mut b = PlanBuilder::new(8);
        let (_w, in_c) = ring(64);
        let (in_id, in_stats) = b.input(RingReader::new(in_c, 0, 64));
        let (out_w, mut out_c) = ring(64);
        let (out_id, out_stats) = b.output(out_w);
        let buf = b.buffer();
        b.op(Op::Pull { ring: in_id, dst: buf })
            .op(Op::Push { src: buf, ring: out_id });
        render(&mut b.build(), 3);

        assert_eq!(out_stats.snapshot().frames, 24);
        assert_eq!(in_stats.snapshot().xruns, 3, "empty input ring underruns every block");
        let got: Vec<_> = (0..24).map(|_| out_c.pop().unwrap()).collect();
        assert!(got.iter().all(|f| *f == [0.0, 0.0]));
    }

    #[test]
    #[should_panic(expected = "unknown buffer")]
    fn builder_rejects_unknown_buffers() {
        PlanBuilder::new(8).op(Op::Clear { dst: BufId(3) });
    }

    #[test]
    fn flush_denormals_zeroes_tiny_results() {
        // Separate thread: MXCSR is per thread, so other tests are unaffected.
        let flushed = std::thread::spawn(|| {
            flush_denormals();
            std::hint::black_box(f32::MIN_POSITIVE) / std::hint::black_box(4.0)
        })
        .join()
        .unwrap();
        if cfg!(target_arch = "x86_64") {
            assert_eq!(flushed, 0.0);
        }
    }
}
