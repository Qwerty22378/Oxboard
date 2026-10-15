//! Render plan: the flat op list the mix thread executes once per block, plus everything it
//! touches (scratch buffers, rings, node state), all allocated up front.
//!
//! The graph compiler (`graph::compile`) emits plans from a `GraphSpec`; [`PlanBuilder`] also
//! works by hand, for tests. Gains are constants until the param table arrives (E4).

use crate::Frame;
use crate::rt::ring::{RingReader, RingWriter};
use crate::rt::stats::StreamStats;
use std::sync::Arc;

/// A planar stereo scratch buffer, one block long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufId(pub usize);

/// An input ring (capture → mix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InRing(pub usize);

/// An output ring (mix → render).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutRing(pub usize);

/// A test-tone oscillator's state slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OscId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    /// Input ring → `dst`. Silence (and an xrun count) if the ring runs dry.
    Pull { ring: InRing, dst: BufId },
    /// Sine test tone → `dst` (same signal on both channels).
    Sine {
        osc: OscId,
        dst: BufId,
        freq: f32,
        gain: f32,
    },
    /// Zero `dst`, e.g. a bus before mixing into it.
    Clear { dst: BufId },
    /// `dst += src × gain`.
    MixAdd { src: BufId, dst: BufId, gain: f32 },
    /// `src` → output ring.
    Push { src: BufId, ring: OutRing },
    /// `src` is what the master clock's device plays (written directly, no ring).
    Master { src: BufId },
}

/// One input ring as the plan sees it.
pub struct Input {
    pub reader: RingReader,
    /// Mix-side counters: frames pulled, underruns, trims.
    pub stats: Arc<StreamStats>,
}

/// One output ring as the plan sees it.
pub struct Output {
    pub writer: RingWriter,
    /// Mix-side counters: frames pushed, overruns.
    pub stats: Arc<StreamStats>,
}

pub struct RenderPlan {
    pub(crate) block: usize,
    pub(crate) ops: Vec<Op>,
    /// `bufs[i]` = [left, right], each `block` samples.
    pub(crate) bufs: Vec<[Vec<f32>; 2]>,
    pub(crate) inputs: Vec<Input>,
    pub(crate) outputs: Vec<Output>,
    /// Oscillator phase in cycles, 0..1.
    pub(crate) osc_phase: Vec<f64>,
    /// Interleave/deinterleave scratch for ring ops.
    pub(crate) frames: Vec<Frame>,
    /// Played when the plan has no `Master` op.
    pub(crate) silence: [Vec<f32>; 2],
}

impl RenderPlan {
    /// Frames per block.
    pub fn block(&self) -> usize {
        self.block
    }

    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    /// Scratch buffers the plan allocated.
    pub fn buffer_count(&self) -> usize {
        self.bufs.len()
    }
}

/// Builds a [`RenderPlan`] by hand. Every `add_*` allocates now, so nothing allocates later.
pub struct PlanBuilder {
    plan: RenderPlan,
}

impl PlanBuilder {
    pub fn new(block: usize) -> Self {
        assert!(block > 0, "block size must be > 0");
        Self {
            plan: RenderPlan {
                block,
                ops: Vec::new(),
                bufs: Vec::new(),
                inputs: Vec::new(),
                outputs: Vec::new(),
                osc_phase: Vec::new(),
                frames: vec![[0.0; 2]; block],
                silence: [vec![0.0; block], vec![0.0; block]],
            },
        }
    }

    pub fn buffer(&mut self) -> BufId {
        let n = self.plan.block;
        self.plan.bufs.push([vec![0.0; n], vec![0.0; n]]);
        BufId(self.plan.bufs.len() - 1)
    }

    /// Register an input ring; the returned stats are the mix side's view of it.
    pub fn input(&mut self, reader: RingReader) -> (InRing, Arc<StreamStats>) {
        let stats = Arc::new(StreamStats::default());
        self.plan.inputs.push(Input {
            reader,
            stats: stats.clone(),
        });
        (InRing(self.plan.inputs.len() - 1), stats)
    }

    /// Register an output ring; the returned stats are the mix side's view of it.
    pub fn output(&mut self, writer: RingWriter) -> (OutRing, Arc<StreamStats>) {
        let stats = Arc::new(StreamStats::default());
        self.plan.outputs.push(Output {
            writer,
            stats: stats.clone(),
        });
        (OutRing(self.plan.outputs.len() - 1), stats)
    }

    pub fn oscillator(&mut self) -> OscId {
        self.plan.osc_phase.push(0.0);
        OscId(self.plan.osc_phase.len() - 1)
    }

    /// Append an op. Panics on references to things this builder didn't create, so a bad
    /// hand-written plan fails here rather than on the audio thread.
    pub fn op(&mut self, op: Op) -> &mut Self {
        let p = &self.plan;
        let buf = |b: BufId| assert!(b.0 < p.bufs.len(), "{op:?}: unknown buffer {b:?}");
        match op {
            Op::Pull { ring, dst } => {
                assert!(ring.0 < p.inputs.len(), "{op:?}: unknown input ring");
                buf(dst);
            }
            Op::Sine { osc, dst, .. } => {
                assert!(osc.0 < p.osc_phase.len(), "{op:?}: unknown oscillator");
                buf(dst);
            }
            Op::Clear { dst } => buf(dst),
            Op::MixAdd { src, dst, .. } => {
                buf(src);
                buf(dst);
                assert_ne!(src, dst, "{op:?}: src and dst must differ");
            }
            Op::Push { src, ring } => {
                buf(src);
                assert!(ring.0 < p.outputs.len(), "{op:?}: unknown output ring");
            }
            Op::Master { src } => buf(src),
        }
        self.plan.ops.push(op);
        self
    }

    pub fn build(self) -> RenderPlan {
        self.plan
    }
}
