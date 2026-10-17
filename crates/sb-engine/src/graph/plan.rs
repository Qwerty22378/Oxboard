//! Render plan: the flat op list the mix thread executes once per block, plus everything it
//! touches (scratch buffers, rings, node state), all allocated up front.
//!
//! The graph compiler (`graph::compile`) emits plans from a `GraphSpec`; [`PlanBuilder`] also
//! works by hand, for tests. The param and meter tables are shared with the controller side.
//!
//! **Handoff (E5):** state that must survive a plan swap (rings, oscillator phase, smoothers,
//! fades, meter RMS) is moved from the running plan into the new one when the mix thread
//! adopts it. The compiler records the moves as slot pairs ([`Move`]); adoption just swaps
//! values, so it never allocates.

use crate::Frame;
use crate::clips::ClipData;
use crate::clips::stream::StreamEnds;
use crate::clock::drift::Drift;
use crate::graph::spec::{MeterId, NodeId, ParamId};
use crate::nodes::clip::{ClipSettings, ClipState};
use crate::rt::meters::{MeterTable, RmsState};
use crate::rt::params::{EDGE_FADE_FRAMES, ParamTable, Smoother};
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

/// A smoother's slot in the plan (one per op that uses a param).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmootherId(pub usize);

/// An edge fade's slot (edges that are appearing or disappearing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FadeId(pub usize);

/// A clip node's slot (its decoded data, settings and voices).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipSlotId(pub usize);

/// A meter op's RMS state slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RmsId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gain {
    Fixed(f32),
    /// Follows `id` in the param table, smoothed per sample by `smoother`.
    Param {
        id: ParamId,
        smoother: SmootherId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    /// Input ring → `dst`. Silence (and an xrun count) if the ring runs dry.
    Pull { ring: InRing, dst: BufId },
    /// A clip's voices → `dst`.
    Clip { slot: ClipSlotId, dst: BufId },
    /// Sine test tone → `dst` (same signal on both channels).
    Sine {
        osc: OscId,
        dst: BufId,
        freq: f32,
        gain: f32,
    },
    /// Zero `dst`, e.g. a bus before mixing into it.
    Clear { dst: BufId },
    /// `dst += src × gain × fade`. `fade` is set while the edge appears or disappears.
    MixAdd {
        src: BufId,
        dst: BufId,
        gain: Gain,
        fade: Option<FadeId>,
    },
    /// Peak + RMS of `src` → meter table slot `meter`.
    Meter { src: BufId, meter: MeterId, state: RmsId },
    /// `src` → output ring.
    Push { src: BufId, ring: OutRing },
    /// `src` is what the master clock's device plays (written directly, no ring).
    Master { src: BufId },
}

/// One state move at adoption: (slot in the running plan, slot in the new plan).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    Input(usize, usize),
    Output(usize, usize),
    Osc(usize, usize),
    Smoother(usize, usize),
    Fade(usize, usize),
    Rms(usize, usize),
    /// A clip's voices and streamer ends (its decoded data stays the new plan's own).
    Clip(usize, usize),
}

/// One clip node in the plan.
pub struct ClipSlot {
    pub node: NodeId,
    pub data: Arc<ClipData>,
    pub settings: ClipSettings,
    pub state: ClipState,
    /// The streamer's ends, for a long clip; `None` until handed over if inherited.
    pub stream: Option<StreamEnds>,
}

/// One input ring as the plan sees it.
pub struct Input {
    pub reader: RingReader,
    /// Mix-side counters: frames pulled, underruns, trims, drift correction.
    pub stats: Arc<StreamStats>,
    /// Drift correction into master time; `None` for a ring with target 0 (same clock).
    pub drift: Option<Drift>,
}

/// One output ring as the plan sees it.
pub struct Output {
    pub writer: RingWriter,
    /// Mix-side counters: frames pushed, overruns, drift correction.
    pub stats: Arc<StreamStats>,
    /// Drift correction out of master time; `None` for a ring with target 0 (same clock).
    pub drift: Option<Drift>,
}

pub struct RenderPlan {
    pub(crate) block: usize,
    pub(crate) ops: Vec<Op>,
    /// `bufs[i]` = [left, right], each `block` samples.
    pub(crate) bufs: Vec<[Vec<f32>; 2]>,
    /// `None` until adoption moves an inherited ring in (and in the old plan afterwards).
    pub(crate) inputs: Vec<Option<Input>>,
    pub(crate) outputs: Vec<Option<Output>>,
    /// Oscillator phase in cycles, 0..1.
    pub(crate) osc_phase: Vec<f64>,
    /// Interleave/deinterleave scratch for ring ops.
    pub(crate) frames: Vec<Frame>,
    /// Played when the plan has no `Master` op.
    pub(crate) silence: [Vec<f32>; 2],
    pub(crate) params: Arc<ParamTable>,
    pub(crate) smoothers: Vec<Smoother>,
    pub(crate) fades: Vec<Smoother>,
    /// Where each fade is heading: 1 (appearing) or 0 (disappearing).
    pub(crate) fade_targets: Vec<f32>,
    pub(crate) meters: Arc<MeterTable>,
    pub(crate) rms: Vec<RmsState>,
    pub(crate) clips: Vec<ClipSlot>,
    pub(crate) handoff: Vec<Move>,
    /// Adopt only once the running plan's fades have finished (a "settle" plan that drops
    /// faded-out edges and nodes).
    pub(crate) wait_for_fades: bool,
    /// Endpoint ID of the device this plan's `Master` op plays on; `None`: timer clock.
    pub(crate) master_device: Option<String>,
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

    pub fn master_device(&self) -> Option<&str> {
        self.master_device.as_deref()
    }

    pub fn waits_for_fades(&self) -> bool {
        self.wait_for_fades
    }

    /// The clip slot of `node`, if this plan has one.
    pub fn clip_mut(&mut self, node: NodeId) -> Option<&mut ClipSlot> {
        self.clips.iter_mut().find(|c| c.node == node)
    }

    pub fn clips_mut(&mut self) -> impl Iterator<Item = &mut ClipSlot> {
        self.clips.iter_mut()
    }

    /// Move inherited state out of `old` (the running plan) into this one. Real-time safe:
    /// only swaps, and skips a move whose slots don't exist rather than panicking.
    pub fn adopt_from(&mut self, old: &mut RenderPlan) {
        fn swap<T>(new: &mut [T], old: &mut [T], from: usize, to: usize) {
            if let (Some(a), Some(b)) = (new.get_mut(to), old.get_mut(from)) {
                std::mem::swap(a, b);
            }
        }
        for &m in &self.handoff {
            match m {
                Move::Input(f, t) => swap(&mut self.inputs, &mut old.inputs, f, t),
                Move::Output(f, t) => swap(&mut self.outputs, &mut old.outputs, f, t),
                Move::Osc(f, t) => swap(&mut self.osc_phase, &mut old.osc_phase, f, t),
                Move::Smoother(f, t) => swap(&mut self.smoothers, &mut old.smoothers, f, t),
                Move::Fade(f, t) => swap(&mut self.fades, &mut old.fades, f, t),
                Move::Rms(f, t) => swap(&mut self.rms, &mut old.rms, f, t),
                Move::Clip(f, t) => {
                    if let (Some(a), Some(b)) = (self.clips.get_mut(t), old.clips.get_mut(f)) {
                        std::mem::swap(&mut a.state, &mut b.state);
                        std::mem::swap(&mut a.stream, &mut b.stream);
                    }
                }
            }
        }
    }

    /// Every edge fade has reached its target.
    pub fn fades_done(&self) -> bool {
        self.fades
            .iter()
            .zip(&self.fade_targets)
            .all(|(f, &t)| !f.is_ramping() && f.value() == t)
    }
}

/// Builds a [`RenderPlan`]. Every call allocates now, so nothing allocates later.
pub struct PlanBuilder {
    plan: RenderPlan,
}

impl PlanBuilder {
    /// A builder with empty param and meter tables, for plans that use neither.
    pub fn new(block: usize) -> Self {
        Self::with_tables(block, Arc::new(ParamTable::new(0)), Arc::new(MeterTable::new(0)))
    }

    pub fn with_tables(block: usize, params: Arc<ParamTable>, meters: Arc<MeterTable>) -> Self {
        assert!(block > 0, "block size must be > 0");
        Self {
            plan: RenderPlan {
                block,
                ops: Vec::new(),
                bufs: Vec::new(),
                inputs: Vec::new(),
                outputs: Vec::new(),
                osc_phase: Vec::new(),
                frames: vec![[0.0; 2]; Drift::max_frames(block)],
                silence: [vec![0.0; block], vec![0.0; block]],
                params,
                smoothers: Vec::new(),
                fades: Vec::new(),
                fade_targets: Vec::new(),
                meters,
                rms: Vec::new(),
                clips: Vec::new(),
                handoff: Vec::new(),
                wait_for_fades: false,
                master_device: None,
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
        let drift = (reader.target() > 0).then(|| Drift::new(reader.target(), self.plan.block));
        self.plan.inputs.push(Some(Input {
            reader,
            stats: stats.clone(),
            drift,
        }));
        (InRing(self.plan.inputs.len() - 1), stats)
    }

    /// An input ring taken over from slot `from` of the running plan at adoption.
    pub fn inherit_input(&mut self, from: usize) -> InRing {
        self.plan.inputs.push(None);
        let to = self.plan.inputs.len() - 1;
        self.plan.handoff.push(Move::Input(from, to));
        InRing(to)
    }

    /// Register an output ring; the returned stats are the mix side's view of it.
    pub fn output(&mut self, writer: RingWriter) -> (OutRing, Arc<StreamStats>) {
        let stats = Arc::new(StreamStats::default());
        let drift = (writer.target() > 0).then(|| Drift::new(writer.target(), self.plan.block));
        self.plan.outputs.push(Some(Output {
            writer,
            stats: stats.clone(),
            drift,
        }));
        (OutRing(self.plan.outputs.len() - 1), stats)
    }

    /// An output ring taken over from slot `from` of the running plan at adoption.
    pub fn inherit_output(&mut self, from: usize) -> OutRing {
        self.plan.outputs.push(None);
        let to = self.plan.outputs.len() - 1;
        self.plan.handoff.push(Move::Output(from, to));
        OutRing(to)
    }

    pub fn oscillator(&mut self) -> OscId {
        self.plan.osc_phase.push(0.0);
        OscId(self.plan.osc_phase.len() - 1)
    }

    /// An oscillator whose phase is taken over from slot `from`.
    pub fn inherit_oscillator(&mut self, from: usize) -> OscId {
        let id = self.oscillator();
        self.plan.handoff.push(Move::Osc(from, id.0));
        id
    }

    /// A gain that follows param `id`, with its own smoother starting at the param's current
    /// value (so a new plan doesn't fade in).
    pub fn param_gain(&mut self, id: ParamId) -> Gain {
        assert!(
            (id.0 as usize) < self.plan.params.capacity(),
            "param {id:?} out of range"
        );
        self.plan.smoothers.push(Smoother::new(self.plan.params.get(id)));
        Gain::Param {
            id,
            smoother: SmootherId(self.plan.smoothers.len() - 1),
        }
    }

    /// A param gain whose smoother (mid-ramp or not) is taken over from slot `from`.
    pub fn inherit_param_gain(&mut self, id: ParamId, from: usize) -> Gain {
        let gain = self.param_gain(id);
        if let Gain::Param { smoother, .. } = gain {
            self.plan.handoff.push(Move::Smoother(from, smoother.0));
        }
        gain
    }

    /// A fade starting at `start`, heading for `target` over the edge fade time.
    pub fn fade(&mut self, start: f32, target: f32) -> FadeId {
        self.plan.fades.push(Smoother::with_ramp(start, EDGE_FADE_FRAMES));
        self.plan.fade_targets.push(target);
        FadeId(self.plan.fades.len() - 1)
    }

    /// A fade taken over from slot `from` (continuing from wherever it is), heading for `target`.
    pub fn inherit_fade(&mut self, from: usize, target: f32) -> FadeId {
        let id = self.fade(target, target);
        self.plan.handoff.push(Move::Fade(from, id.0));
        id
    }

    /// Append a meter op on `src`, with fresh RMS state or state taken over from slot `from`.
    pub fn meter(&mut self, src: BufId, meter: MeterId, inherit_from: Option<usize>) -> &mut Self {
        self.plan.rms.push(RmsState::default());
        let state = RmsId(self.plan.rms.len() - 1);
        if let Some(from) = inherit_from {
            self.plan.handoff.push(Move::Rms(from, state.0));
        }
        self.op(Op::Meter { src, meter, state })
    }

    /// A clip node: its data, settings and (long clips) streamer ends; or, with
    /// `inherit_from`, voices and streamer taken over from that slot of the running plan.
    pub fn clip(
        &mut self,
        node: NodeId,
        data: Arc<ClipData>,
        settings: ClipSettings,
        stream: Option<StreamEnds>,
        inherit_from: Option<usize>,
    ) -> ClipSlotId {
        self.plan.clips.push(ClipSlot {
            node,
            data,
            settings,
            state: ClipState::default(),
            stream,
        });
        let id = self.plan.clips.len() - 1;
        if let Some(from) = inherit_from {
            self.plan.handoff.push(Move::Clip(from, id));
        }
        ClipSlotId(id)
    }

    pub fn wait_for_fades(&mut self, wait: bool) -> &mut Self {
        self.plan.wait_for_fades = wait;
        self
    }

    pub fn master_device(&mut self, device: Option<String>) -> &mut Self {
        self.plan.master_device = device;
        self
    }

    /// Append an op. Panics on references to things this builder didn't create, so a bad
    /// plan fails here rather than on the audio thread.
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
            Op::Clip { slot, dst } => {
                assert!(slot.0 < p.clips.len(), "{op:?}: unknown clip slot");
                buf(dst);
            }
            Op::MixAdd { src, dst, gain, fade } => {
                buf(src);
                buf(dst);
                assert_ne!(src, dst, "{op:?}: src and dst must differ");
                if let Gain::Param { smoother, .. } = gain {
                    assert!(
                        smoother.0 < p.smoothers.len(),
                        "{op:?}: unknown smoother (use param_gain)"
                    );
                }
                if let Some(f) = fade {
                    assert!(f.0 < p.fades.len(), "{op:?}: unknown fade");
                }
            }
            Op::Meter { src, meter, state } => {
                buf(src);
                assert!((meter.0 as usize) < p.meters.capacity(), "{op:?}: meter out of range");
                assert!(state.0 < p.rms.len(), "{op:?}: unknown RMS state (use meter)");
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
