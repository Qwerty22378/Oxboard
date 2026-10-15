//! The mix thread: wait for the master clock, adopt a new plan if one is waiting, run the
//! plan once per due block, hand the master bus to the clock.
//!
//! - [`render_block`] is the op interpreter.
//! - [`MixCore`] owns the running plan and the two plan queues (new plans in, old plans out
//!   to be dropped off the audio thread), and swaps plans at block boundaries.
//! - [`run`] is the loop. All three are platform-independent and tested with a fake clock;
//!   [`spawn`] (Windows) puts the loop on an MMCSS thread with denormals flushed.

use crate::SAMPLE_RATE;
use crate::clock::master::MasterClock;
use crate::graph::plan::{Gain, Input, Op, Output, RenderPlan};
use crate::rt::stats::MixStats;
use rtrb::{Consumer, Producer, RingBuffer};
use std::f64::consts::TAU;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Instant;

/// Run every op in the plan once. Returns the master bus (left, right), or silence if the
/// plan has no `Master` op. `now` (ns, `rt::ring::now_ns` time base) dates the drift
/// controllers' fill estimates. Real-time safe: the builder validated every index.
pub fn render_block(plan: &mut RenderPlan, now: u64) -> [&[f32]; 2] {
    let RenderPlan {
        block,
        ops,
        bufs,
        inputs,
        outputs,
        osc_phase,
        frames,
        silence,
        params,
        smoothers,
        fades,
        fade_targets,
        meters,
        rms,
        ..
    } = plan;
    let mut master = None;
    for op in ops.iter() {
        match *op {
            Op::Pull { ring, dst } => {
                let [l, r] = &mut bufs[dst.0];
                match &mut inputs[ring.0] {
                    Some(Input {
                        reader,
                        stats,
                        drift: Some(d),
                    }) => {
                        let need = d.input_needed().min(frames.len());
                        reader.read(&mut frames[..need], stats);
                        d.pull(&frames[..need], l, r);
                        if let Some(fill) = reader.estimated_fill(now) {
                            d.measure(fill, now);
                        }
                        stats.drift_ppm.store((d.ppm() as f32).to_bits(), Relaxed);
                    }
                    Some(Input {
                        reader,
                        stats,
                        drift: None,
                    }) => {
                        let frames = &mut frames[..*block];
                        reader.read(frames, stats);
                        for ((f, l), r) in frames.iter().zip(l.iter_mut()).zip(r.iter_mut()) {
                            (*l, *r) = (f[0], f[1]);
                        }
                    }
                    // Not handed over (can't happen with a consistent handoff): silence.
                    None => {
                        l.fill(0.0);
                        r.fill(0.0);
                    }
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
            Op::MixAdd { src, dst, gain, fade } => {
                let Ok([s, d]) = bufs.get_disjoint_mut([src.0, dst.0]) else {
                    continue;
                };
                let mut gain = match gain {
                    Gain::Fixed(g) => Ramp::Fixed(g),
                    Gain::Param { id, smoother } => {
                        let sm = &mut smoothers[smoother.0];
                        sm.set_target(params.get(id));
                        Ramp::of(sm)
                    }
                };
                let mut fade = match fade {
                    None => Ramp::Fixed(1.0),
                    Some(f) => {
                        let sm = &mut fades[f.0];
                        sm.set_target(fade_targets[f.0]);
                        Ramp::of(sm)
                    }
                };
                match (&mut gain, &mut fade) {
                    (Ramp::Fixed(g), Ramp::Fixed(f)) => mix_add(s, d, *g * *f),
                    _ => {
                        let ([sl, sr], [dl, dr]) = (s, d);
                        for (((sl, sr), dl), dr) in sl.iter().zip(sr.iter()).zip(dl.iter_mut()).zip(dr) {
                            let g = gain.tick() * fade.tick();
                            *dl += sl * g;
                            *dr += sr * g;
                        }
                    }
                }
            }
            Op::Meter { src, meter, state } => {
                let [l, r] = &bufs[src.0];
                meters.measure(meter, [l, r], &mut rms[state.0]);
            }
            Op::Push { src, ring } => {
                let Some(Output { writer, stats, drift }) = &mut outputs[ring.0] else {
                    continue;
                };
                let [l, r] = &bufs[src.0];
                let n = match drift {
                    Some(d) => d.push_block(l, r, frames),
                    None => {
                        for ((f, l), r) in frames.iter_mut().zip(l).zip(r) {
                            *f = [*l, *r];
                        }
                        *block
                    }
                };
                writer.write(Some(&frames[..n]), n, stats);
                if let Some(d) = drift {
                    if let Some(fill) = writer.estimated_fill(now) {
                        d.measure(fill, now);
                        stats.ring_fill.store(fill as u32, Relaxed);
                    }
                    stats.drift_ppm.store((d.ppm() as f32).to_bits(), Relaxed);
                }
            }
            Op::Master { src } => master = Some(src.0),
        }
    }
    match master {
        Some(i) => [&bufs[i][0], &bufs[i][1]],
        None => [&silence[0], &silence[1]],
    }
}

/// A per-sample multiplier: constant for the block, or a smoother still ramping.
enum Ramp<'a> {
    Fixed(f32),
    Moving(&'a mut crate::rt::params::Smoother),
}

impl<'a> Ramp<'a> {
    fn of(sm: &'a mut crate::rt::params::Smoother) -> Self {
        if sm.is_ramping() {
            Ramp::Moving(sm)
        } else {
            Ramp::Fixed(sm.value())
        }
    }

    #[inline]
    fn tick(&mut self) -> f32 {
        match self {
            Ramp::Fixed(v) => *v,
            Ramp::Moving(sm) => sm.tick(),
        }
    }
}

/// `dst += src × g` on both channels.
fn mix_add(src: &[Vec<f32>; 2], dst: &mut [Vec<f32>; 2], g: f32) {
    for (s, d) in src.iter().zip(dst.iter_mut()) {
        for (s, d) in s.iter().zip(d.iter_mut()) {
            *d += s * g;
        }
    }
}

/// Plans waiting in the queue at most. The control side gets `GraphError::Busy` beyond this.
pub const PLAN_QUEUE_DEPTH: usize = 8;

/// The control side's ends of the plan queues.
pub struct PlanQueue {
    /// New plans, adopted in order, one per block at most.
    pub incoming: Producer<Box<RenderPlan>>,
    /// Plans the mix thread is done with; drop them here, off the audio thread.
    pub retired: Consumer<Box<RenderPlan>>,
}

/// The audio side: the running plan and its ends of the plan queues.
pub struct MixCore {
    plan: Box<RenderPlan>,
    incoming: Consumer<Box<RenderPlan>>,
    retired: Producer<Box<RenderPlan>>,
}

/// Connect a control side and a mix side, with `first` running.
pub fn plan_channel(first: RenderPlan) -> (PlanQueue, MixCore) {
    let (in_tx, in_rx) = RingBuffer::new(PLAN_QUEUE_DEPTH);
    // One more than can be in flight, so the mix thread always has room to retire a plan.
    let (out_tx, out_rx) = RingBuffer::new(PLAN_QUEUE_DEPTH + 1);
    (
        PlanQueue {
            incoming: in_tx,
            retired: out_rx,
        },
        MixCore {
            plan: Box::new(first),
            incoming: in_rx,
            retired: out_tx,
        },
    )
}

impl MixCore {
    pub fn plan(&self) -> &RenderPlan {
        &self.plan
    }

    /// Adopt the next plan if one is waiting and may go now: a settle plan waits until the
    /// running plan's fades are done. Moves inherited state over, switches the clock if the
    /// master device changed, and retires the old plan. Real-time safe, except that a master
    /// device change opens the new device (rare, and the output changes anyway).
    pub fn adopt(&mut self, clock: &mut impl MasterClock, stats: &MixStats) -> bool {
        if self.retired.slots() == 0 {
            return false; // can't hand the old plan back yet; dropping it here would free memory
        }
        let ready = match self.incoming.peek() {
            Ok(next) => !next.wait_for_fades || self.plan.fades_done(),
            Err(_) => false,
        };
        if !ready {
            return false;
        }
        let Ok(mut next) = self.incoming.pop() else {
            return false;
        };
        next.adopt_from(&mut self.plan);
        if next.master_device != self.plan.master_device {
            clock.set_master(next.master_device.as_deref());
        }
        let old = std::mem::replace(&mut self.plan, next);
        if let Err(rtrb::PushError::Full(old)) = self.retired.push(old) {
            std::mem::forget(old); // unreachable (slot checked above); leak rather than free here
        }
        stats.swaps.fetch_add(1, Relaxed);
        true
    }

    /// One block: adopt if due, render, deliver the master bus.
    pub fn block(&mut self, clock: &mut impl MasterClock, stats: &MixStats) {
        let start = Instant::now();
        self.adopt(clock, stats);
        let [l, r] = render_block(&mut self.plan, clock.now_ns());
        clock.deliver(l, r);
        stats.block_done(start.elapsed().as_nanos() as u64);
    }
}

/// The block loop. Runs until `stop` is set.
pub fn run(core: &mut MixCore, clock: &mut impl MasterClock, stats: &MixStats, stop: &AtomicBool) {
    while !stop.load(Relaxed) {
        for _ in 0..clock.wait() {
            core.block(clock, stats);
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
    use super::{MixCore, flush_denormals, run};
    use crate::backend::wasapi::{Mmcss, RtThread};
    use crate::clock::master::{Clock, MasterInfo};
    use crate::rt::stats::MixStats;
    use std::sync::Arc;
    use windows::core::Result;

    /// The running mix thread. Dropping it stops the thread and waits for it.
    pub struct MixThread {
        /// The clock it started with (plans can change the master later).
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

    /// Start the mix thread on `core`. The clock starts on the running plan's master device
    /// (or the timer) and follows later plans' master changes.
    pub fn spawn(core: MixCore) -> Result<MixThread> {
        let stats = Arc::new(MixStats::default());
        let thread_stats = stats.clone();
        let (thread, info) = RtThread::spawn(
            "sb-mix",
            move || {
                let mmcss = Mmcss::register(&thread_stats.mmcss);
                flush_denormals();
                let plan = core.plan();
                let clock = Clock::new(plan.master_device(), plan.block(), thread_stats.clone())?;
                let info = clock.info();
                Ok(((mmcss, clock, core, thread_stats), info))
            },
            |(_mmcss, mut clock, mut core, stats), stop| {
                run(&mut core, &mut clock, &stats, stop);
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
        fn set_master(&mut self, _: Option<&str>) {}
    }

    fn render(plan: RenderPlan, blocks: usize) -> (Vec<[f32; 2]>, MixStats) {
        let (_queue, mut core) = plan_channel(plan);
        let stop = Arc::new(AtomicBool::new(false));
        let mut clock = FakeClock {
            left: blocks,
            stop: stop.clone(),
            out: Vec::new(),
        };
        let stats = MixStats::default();
        run(&mut core, &mut clock, &stats, &stop);
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
        let (out, stats) = render(b.build(), 10);
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
        let (out, _) = render(b.build(), 3);
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
            gain: Gain::Fixed(0.5),
            fade: None,
        })
        .op(Op::MixAdd {
            src: tone,
            dst: bus,
            gain: Gain::Fixed(1.0),
            fade: None,
        })
        .op(Op::MixAdd {
            src: mic,
            dst: bus,
            gain: Gain::Fixed(0.25),
            fade: None,
        })
        .op(Op::Master { src: bus });
        let plan = b.build();

        let mut w = in_w;
        let input: Vec<[f32; 2]> = (0..16).map(|i| [i as f32, -1.0]).collect();
        w.write(Some(&input), 16, &StreamStats::default());
        let (out, _) = render(plan, 1);
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
                gain: Gain::Fixed(1.0),
                fade: None,
            })
            .op(Op::Master { src: bus });
        let (out, _) = render(b.build(), 20);
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
        render(b.build(), 3);

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
