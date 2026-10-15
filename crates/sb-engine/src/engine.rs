//! The control side of the engine: `apply_graph`, `set_param`, meters, and the bookkeeping
//! that makes graph updates glitch-free.
//!
//! `apply_graph` compiles the new spec against the layout of the last plan sent to the mix
//! thread, so state carries over by key; new edges fade in, removed ones fade out as ghosts.
//! Once that plan is running and nothing newer is queued, [`Engine::maintain`] queues a
//! **settle** plan that drops the ghosts; the mix thread adopts it only after the fades end.
//! A newer update before then simply continues the fades, so bursts of updates coalesce.
//! `maintain` also drops retired plans (here, off the audio thread) and closes endpoints no
//! queued plan uses any more, so call it regularly (every few tens of ms).
//!
//! The audio side is a [`MixCore`]: run it on the mix thread (`mix_thread::spawn` on
//! Windows), or drive it block by block in tests.

use crate::graph::compile::{Endpoints, Layout, Mode, Tables, compile_against};
use crate::graph::plan::PlanBuilder;
use crate::graph::spec::{GraphError, GraphSpec, NodeKind, ParamId};
use crate::rt::mix_thread::{MixCore, PlanQueue, plan_channel};
use std::collections::VecDeque;

pub struct Engine<E: Endpoints> {
    block: usize,
    tables: Tables,
    endpoints: E,
    queue: PlanQueue,
    /// Layout of the running plan (front) and of every plan queued after it, in order.
    layouts: VecDeque<Layout>,
    /// The last spec applied; settle plans are compiled from it.
    spec: GraphSpec,
}

impl<E: Endpoints> Engine<E> {
    /// An engine running an empty plan (silence, timer clock), and the core to run on the
    /// mix thread.
    pub fn new(block: usize, tables: Tables, endpoints: E) -> (Self, MixCore) {
        Self::with_master(block, tables, endpoints, None)
    }

    /// Like [`Engine::new`], but the empty plan already plays on `master` (an endpoint ID).
    /// The mix thread then opens that device during setup, so the first graph with the same
    /// master doesn't switch clocks mid-stream (opening a device stalls the mix ~20 ms).
    pub fn with_master(block: usize, tables: Tables, mut endpoints: E, master: Option<&str>) -> (Self, MixCore) {
        endpoints.master_changed(master);
        let mut first = PlanBuilder::with_tables(block, tables.params.clone(), tables.meters.clone());
        first.master_device(master.map(str::to_owned));
        let (queue, core) = plan_channel(first.build());
        let layouts = VecDeque::from([Layout::with_master(master.map(str::to_owned))]);
        (
            Self {
                block,
                tables,
                endpoints,
                queue,
                layouts,
                spec: GraphSpec::default(),
            },
            core,
        )
    }

    pub fn tables(&self) -> &Tables {
        &self.tables
    }

    pub fn endpoints(&self) -> &E {
        &self.endpoints
    }

    pub fn endpoints_mut(&mut self) -> &mut E {
        &mut self.endpoints
    }

    /// Move a param. Lock-free, no rebuild, smoothed on the audio thread.
    pub fn set_param(&self, id: ParamId, value: f32) -> bool {
        self.tables.params.set(id, value)
    }

    /// Layout of the last plan sent (what the next `apply_graph` compiles against).
    pub fn latest_layout(&self) -> &Layout {
        self.layouts.back().expect("never empty")
    }

    /// Layout of the plan the mix thread is running (as of the last `maintain`).
    pub fn running_layout(&self) -> &Layout {
        self.layouts.front().expect("never empty")
    }

    /// Plans sent but not yet adopted.
    pub fn pending(&self) -> usize {
        self.layouts.len() - 1
    }

    /// Replace the graph. On error the running graph is untouched and nothing stays opened.
    pub fn apply_graph(&mut self, spec: &GraphSpec) -> Result<(), GraphError> {
        self.maintain();
        if self.queue.incoming.slots() == 0 {
            return Err(GraphError::Busy);
        }
        let master = master_device(spec);
        if master != self.latest_layout().master_device() {
            self.endpoints.master_changed(master);
        }
        let prev = self.layouts.back().expect("never empty");
        let c = compile_against(
            spec,
            self.block,
            &self.tables,
            &mut self.endpoints,
            prev,
            Mode::Transition,
        )?;
        self.submit(c.plan, c.layout);
        self.spec = spec.clone();
        Ok(())
    }

    /// Drop plans the mix thread has retired and close endpoints no queued plan uses any
    /// more; then, if the running plan is mid-transition and nothing newer is queued, queue
    /// its settle plan. Call it regularly; `apply_graph` also does.
    pub fn maintain(&mut self) {
        while let Ok(old) = self.queue.retired.pop() {
            drop(old);
            let Some(gone) = self.layouts.pop_front() else { continue };
            for key in gone.endpoint_keys() {
                if !self.layouts.iter().any(|l| l.uses_endpoint(key)) {
                    self.endpoints.release(key);
                }
            }
        }
        if self.pending() == 0 && self.running_layout().in_transition() && self.queue.incoming.slots() > 0 {
            let prev = self.layouts.back().expect("never empty");
            // Same spec as the running transition, so it validates and opens nothing new.
            if let Ok(c) = compile_against(
                &self.spec,
                self.block,
                &self.tables,
                &mut self.endpoints,
                prev,
                Mode::Settle,
            ) {
                self.submit(c.plan, c.layout);
            }
        }
    }

    fn submit(&mut self, plan: crate::graph::plan::RenderPlan, layout: Layout) {
        self.layouts.push_back(layout);
        if self.queue.incoming.push(Box::new(plan)).is_err() {
            unreachable!("a free slot was checked before compiling");
        }
    }
}

/// Start an engine on WASAPI: the control side, and the mix thread running its core. Pass the
/// master device the first graph will use (if known), so the mix thread opens it up front.
#[cfg(windows)]
pub fn start_wasapi(
    block: usize,
    tables: Tables,
    master: Option<&str>,
) -> windows::core::Result<(
    Engine<crate::backend::wasapi::WasapiEndpoints>,
    crate::rt::mix_thread::MixThread,
)> {
    let endpoints = crate::backend::wasapi::WasapiEndpoints::new(block);
    let (engine, core) = Engine::with_master(block, tables, endpoints, master);
    let mix = crate::rt::mix_thread::spawn(core)?;
    Ok((engine, mix))
}

fn master_device(spec: &GraphSpec) -> Option<&str> {
    let m = spec.master?;
    match &spec.nodes.iter().find(|n| n.id == m)?.kind {
        NodeKind::DeviceOutput { device } => Some(device),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Frame;
    use crate::graph::compile::EdgeStatus;
    use crate::graph::plan::Op;
    use crate::graph::spec::{EdgeSpec, NodeId, NodeSpec};
    use crate::rt::params::{EDGE_FADE_FRAMES, RAMP_FRAMES};
    use crate::rt::stats::MixStats;
    use crate::testing::*;
    use std::collections::HashMap;

    const BLOCK: usize = 64;

    /// Engine + mix core + fake clock, driven block by block. Inputs get a constant (DC)
    /// signal per node, so every level change is easy to see.
    struct Rig {
        engine: Engine<FakeEndpoints>,
        core: MixCore,
        clock: FakeClock,
        stats: MixStats,
        dc: HashMap<NodeId, f32>,
    }

    impl Rig {
        fn new() -> Self {
            let (engine, core) = Engine::new(BLOCK, Tables::new(1024, 16), FakeEndpoints::default());
            Self {
                engine,
                core,
                clock: FakeClock::default(),
                stats: MixStats::default(),
                dc: HashMap::new(),
            }
        }

        fn apply(&mut self, spec: &GraphSpec) {
            self.engine
                .apply_graph(spec)
                .unwrap_or_else(|e| panic!("apply failed: {e}"));
        }

        /// Run `blocks` blocks; return the master bus they produced.
        fn run(&mut self, blocks: usize) -> Vec<Frame> {
            let start = self.clock.out.len();
            for _ in 0..blocks {
                let dc = &self.dc;
                self.engine
                    .endpoints_mut()
                    .feed_all(BLOCK, |n| dc.get(&n).copied().unwrap_or(0.0));
                self.core.block(&mut self.clock, &self.stats);
                self.engine.maintain();
            }
            self.clock.out[start..].to_vec()
        }

        /// Run until nothing is pending and nothing fades.
        fn settle(&mut self) -> Vec<Frame> {
            let mut out = Vec::new();
            for _ in 0..100 {
                out.extend(self.run(1));
                if self.engine.pending() == 0 && !self.engine.running_layout().in_transition() {
                    return out;
                }
            }
            panic!("didn't settle");
        }
    }

    fn left(frames: &[Frame]) -> Vec<f32> {
        frames.iter().map(|f| f[0]).collect()
    }

    fn max_step(xs: &[f32]) -> f32 {
        xs.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
    }

    fn spec(nodes: Vec<NodeSpec>, edges: Vec<EdgeSpec>, master: u32) -> GraphSpec {
        GraphSpec {
            nodes,
            edges,
            master: Some(NodeId(master)),
            ..Default::default()
        }
    }

    #[test]
    fn first_graph_fades_in_from_silence() {
        let mut rig = Rig::new();
        rig.dc.insert(NodeId(1), 1.0);
        rig.apply(&spec(vec![input(1, "mic"), output(2, "spk")], vec![edge(1, 2)], 2));
        let out = left(&rig.settle());
        assert_eq!(out[0], 1.0 / EDGE_FADE_FRAMES as f32, "first sample is one fade step");
        assert!(max_step(&out) <= 1.0 / EDGE_FADE_FRAMES as f32 + 1e-6);
        assert_eq!(*out.last().unwrap(), 1.0);
        // Settled: the edge forwards again, no MixAdd left.
        assert!(!rig.core.plan().ops().iter().any(|op| matches!(op, Op::MixAdd { .. })));
    }

    #[test]
    fn reapplying_the_same_graph_is_seamless() {
        let mut rig = Rig::new();
        let s = spec(
            vec![tone(1, 440.0, 0.5), bus(2), output(3, "spk")],
            vec![edge(1, 2), edge(2, 3)],
            3,
        );
        rig.apply(&s);
        rig.settle();
        let before = rig.stats.swaps.load(std::sync::atomic::Ordering::Relaxed);
        let start = rig.clock.out.len();
        for _ in 0..20 {
            rig.apply(&s);
            rig.run(1);
        }
        assert_eq!(
            rig.stats.swaps.load(std::sync::atomic::Ordering::Relaxed) - before,
            20,
            "one plan per apply, no settle"
        );
        let out = left(&rig.clock.out[start..]);
        for (k, v) in out.iter().enumerate() {
            let want = sine_at(start + k, 440.0, 0.5);
            assert!(
                (v - want).abs() < 1e-4,
                "sample {}: {v} vs {want} (phase must carry over)",
                start + k
            );
        }
    }

    #[test]
    fn edges_fade_in_and_out_and_ghosts_are_dropped() {
        let mut rig = Rig::new();
        rig.dc.insert(NodeId(1), 1.0);
        rig.dc.insert(NodeId(2), 0.5);
        let nodes = vec![input(1, "a"), input(2, "b"), bus(3), output(4, "spk")];
        let one = spec(nodes.clone(), vec![edge(1, 3), edge(3, 4)], 4);
        let both = spec(nodes.clone(), vec![edge(1, 3), edge(2, 3), edge(3, 4)], 4);
        rig.apply(&one);
        rig.settle();

        rig.apply(&both);
        let up = left(&rig.settle());
        assert!(
            max_step(&up) <= 0.5 / EDGE_FADE_FRAMES as f32 + 1e-6,
            "fade in, no step"
        );
        assert_eq!(*up.last().unwrap(), 1.5);

        rig.apply(&one);
        let first = left(&rig.run(1));
        assert_eq!(
            rig.engine.running_layout().edge_status(NodeId(2), NodeId(3)),
            Some(EdgeStatus::FadingOut),
            "removed edge runs on as a ghost"
        );
        let down = [first, left(&rig.settle())].concat();
        assert!(
            max_step(&down) <= 0.5 / EDGE_FADE_FRAMES as f32 + 1e-6,
            "fade out, no step"
        );
        assert_eq!(*down.last().unwrap(), 1.0);
        assert_eq!(
            rig.engine.running_layout().edge_status(NodeId(2), NodeId(3)),
            None,
            "ghost dropped"
        );
    }

    #[test]
    fn removed_device_is_released_only_after_its_fade() {
        let mut rig = Rig::new();
        rig.dc.insert(NodeId(1), 1.0);
        rig.dc.insert(NodeId(2), 1.0);
        let nodes = vec![input(1, "a"), input(2, "b"), bus(3), output(4, "spk")];
        rig.apply(&spec(nodes, vec![edge(1, 3), edge(2, 3), edge(3, 4)], 4));
        rig.settle();
        let b_key = rig
            .engine
            .running_layout()
            .inputs()
            .iter()
            .find(|e| e.node == NodeId(2))
            .unwrap()
            .key;

        // Node 2 is gone from the spec: it becomes a ghost and fades out.
        rig.apply(&spec(
            vec![input(1, "a"), bus(3), output(4, "spk")],
            vec![edge(1, 3), edge(3, 4)],
            4,
        ));
        let first = left(&rig.run(1));
        assert!(
            rig.engine.endpoints().released.is_empty(),
            "still fading: not released yet"
        );
        assert!(first[0] > 1.9, "fade-out starts from the old level, no drop");
        let rest = left(&rig.settle());
        assert!(max_step(&[first, rest.clone()].concat()) <= 1.0 / EDGE_FADE_FRAMES as f32 + 1e-6);
        assert_eq!(*rest.last().unwrap(), 1.0);
        assert_eq!(
            rig.engine.endpoints().released,
            vec![b_key],
            "released exactly once, after the fade"
        );
    }

    #[test]
    fn unchanged_device_nodes_keep_their_streams() {
        let mut rig = Rig::new();
        for k in 0..5 {
            let mut nodes = vec![input(1, "mic"), bus(2), output(3, "spk")];
            let mut edges = vec![edge(1, 2), edge(2, 3)];
            if k % 2 == 1 {
                nodes.push(tone(9, 300.0, 0.1));
                edges.push(edge(9, 2));
            }
            rig.apply(&spec(nodes, edges, 3));
            rig.run(2);
        }
        rig.settle();
        assert_eq!(
            rig.engine.endpoints().opened,
            vec!["mic".to_string()],
            "opened once, inherited after"
        );
        assert!(rig.engine.endpoints().released.is_empty());
    }

    #[test]
    fn replacing_a_node_device_under_the_same_id_opens_the_new_and_releases_the_old() {
        let mut rig = Rig::new();
        rig.apply(&spec(vec![input(1, "micA"), output(2, "spk")], vec![edge(1, 2)], 2));
        rig.settle();
        let old = rig.engine.running_layout().inputs()[0].key;
        rig.apply(&spec(vec![input(1, "micB"), output(2, "spk")], vec![edge(1, 2)], 2));
        rig.settle();
        assert_eq!(
            rig.engine.endpoints().opened,
            vec!["micA".to_string(), "micB".to_string()]
        );
        assert_eq!(rig.engine.endpoints().released, vec![old]);
        assert_eq!(rig.engine.endpoints().open_count(), 1);
    }

    #[test]
    fn failed_update_keeps_the_running_graph_and_releases_what_it_opened() {
        let mut rig = Rig::new();
        rig.dc.insert(NodeId(1), 1.0);
        rig.apply(&spec(vec![input(1, "mic"), output(2, "spk")], vec![edge(1, 2)], 2));
        rig.settle();
        rig.engine.endpoints_mut().fail = Some("broken".into());
        let bad = spec(
            vec![
                input(1, "mic"),
                input(5, "newmic"),
                output(2, "spk"),
                output(6, "broken"),
            ],
            vec![edge(1, 2), edge(5, 6)],
            2,
        );
        assert!(matches!(rig.engine.apply_graph(&bad), Err(GraphError::Endpoint { .. })));
        let ep = rig.engine.endpoints();
        assert_eq!(ep.opened, vec!["mic".to_string(), "newmic".to_string()]);
        assert_eq!(ep.released.len(), 1, "newmic closed again");
        assert_eq!(rig.engine.pending(), 0);
        assert!(left(&rig.run(3)).iter().all(|&v| v == 1.0), "old graph still playing");
    }

    #[test]
    fn param_ramp_continues_across_a_swap() {
        let mut rig = Rig::new();
        rig.dc.insert(NodeId(1), 1.0);
        let mut s = spec(vec![input(1, "mic"), output(2, "spk")], vec![gain_edge(1, 2, 7)], 2);
        s.params = vec![param(7, 1.0)];
        rig.apply(&s);
        rig.settle();
        rig.engine.set_param(ParamId(7), 0.0);
        let a = left(&rig.run(1));
        // Unrelated change mid-ramp: the smoother must carry on, not restart or jump.
        s.nodes.push(tone(9, 100.0, 0.1));
        s.nodes.push(output(10, "cable"));
        s.edges.push(edge(9, 10));
        s.params = vec![]; // not re-declared: the table keeps the controller's value (0)
        s.params.push(param(7, 0.0));
        rig.apply(&s);
        let b = left(&rig.run(8));
        let all = [a, b].concat();
        let step = 1.0 / RAMP_FRAMES as f32;
        assert!(
            all.windows(2)
                .all(|w| ((w[0] - w[1]) - step).abs() < 1e-5 || w[1] == 0.0),
            "steady ramp"
        );
        assert_eq!(*all.last().unwrap(), 0.0);
    }

    #[test]
    fn master_change_switches_the_clock() {
        let mut rig = Rig::new();
        rig.apply(&spec(vec![tone(1, 440.0, 0.1), output(2, "spk")], vec![edge(1, 2)], 2));
        rig.settle();
        rig.apply(&spec(vec![tone(1, 440.0, 0.1), output(3, "hdmi")], vec![edge(1, 3)], 3));
        rig.settle();
        assert_eq!(
            rig.clock.masters,
            vec![Some("spk".to_string()), Some("hdmi".to_string())]
        );
    }

    #[test]
    fn full_queue_is_busy_not_blocking() {
        let mut rig = Rig::new();
        let mut busy = false;
        for k in 0..20 {
            let s = spec(vec![tone(k, 440.0, 0.1), output(100, "spk")], vec![edge(k, 100)], 100);
            match rig.engine.apply_graph(&s) {
                Ok(()) => {}
                Err(GraphError::Busy) => busy = true,
                Err(e) => panic!("{e}"),
            }
        }
        assert!(busy, "mix thread never ran, so the queue must fill");
        rig.settle(); // and it drains fine afterwards
    }

    #[test]
    fn opposite_edge_between_two_buses_falls_back_to_a_hard_switch() {
        let mut rig = Rig::new();
        let nodes = vec![tone(1, 440.0, 0.1), bus(2), bus(3), output(4, "spk")];
        rig.apply(&spec(nodes.clone(), vec![edge(1, 2), edge(2, 3), edge(3, 4)], 4));
        rig.settle();
        // 3 → 2 now: with the old 2 → 3 as a ghost this would be a cycle.
        rig.apply(&spec(nodes, vec![edge(1, 3), edge(3, 2), edge(2, 4)], 4));
        rig.settle();
    }

    // ---- randomized: updates never click, and always settle to the direct evaluation ----

    const INPUTS: [(u32, f32); 3] = [(0, 1.0), (1, -0.5), (2, 0.25)];
    const BUSES: [u32; 3] = [10, 11, 12];
    const OUTS: [u32; 3] = [20, 21, 22];
    const MASTER: u32 = 20;

    /// Random spec over a fixed node universe. Edges only go from lower to higher rank
    /// (inputs < buses by id < outputs), so even the union of several specs is acyclic.
    fn random_spec(rng: &mut Rng) -> GraphSpec {
        let mut s = GraphSpec {
            master: Some(NodeId(MASTER)),
            ..Default::default()
        };
        for (id, _) in INPUTS {
            if rng.below(10) < 7 {
                s.nodes.push(input(id, &format!("in{id}")));
            }
        }
        for id in BUSES {
            if rng.below(10) < 7 {
                s.nodes.push(bus(id));
            }
        }
        for id in OUTS {
            if id == MASTER || rng.below(2) == 0 {
                s.nodes.push(output(id, &format!("out{id}")));
            }
        }
        let ids: Vec<u32> = s.nodes.iter().map(|n| n.id.0).collect();
        for &from in ids.iter().filter(|&&i| i < 20) {
            for &to in ids.iter().filter(|&&t| t >= 10 && t > from) {
                if rng.below(3) == 0 {
                    let gain = match rng.below(3) {
                        0 => None,
                        _ => {
                            let p = from * 32 + to;
                            s.params.push(param(p, [0.25, 0.5, 1.0, -0.75][rng.below(4)]));
                            Some(ParamId(p))
                        }
                    };
                    s.edges.push(EdgeSpec {
                        from: NodeId(from),
                        to: NodeId(to),
                        gain,
                    });
                }
            }
        }
        s
    }

    fn gain_of(s: &GraphSpec, e: &EdgeSpec) -> f32 {
        e.gain
            .map_or(1.0, |p| s.params.iter().find(|q| q.id == p).unwrap().value)
    }

    /// Direct evaluation of node `id` (DC, so one number).
    fn eval(s: &GraphSpec, id: u32) -> f32 {
        if let Some((_, v)) = INPUTS.iter().find(|(i, _)| *i == id) {
            return if s.nodes.iter().any(|n| n.id.0 == id) { *v } else { 0.0 };
        }
        s.edges
            .iter()
            .filter(|e| e.to.0 == id)
            .map(|e| eval(s, e.from.0) * gain_of(s, e))
            .sum()
    }

    /// Largest |value| node `id` could reach mixing any edges from `specs` with |gains|.
    fn abs_bound(specs: &[GraphSpec], id: u32) -> f32 {
        if let Some((_, v)) = INPUTS.iter().find(|(i, _)| *i == id) {
            return v.abs();
        }
        let mut best: HashMap<u32, f32> = HashMap::new();
        for s in specs {
            for e in s.edges.iter().filter(|e| e.to.0 == id) {
                let g = best.entry(e.from.0).or_insert(0.0);
                *g = g.max(gain_of(s, e).abs());
            }
        }
        best.iter().map(|(&from, &g)| g * abs_bound(specs, from)).sum()
    }

    #[test]
    fn random_updates_never_click_and_settle_to_direct_evaluation() {
        let mut rng = Rng(0x0fad_e0ff_5eed);
        for case in 0..150 {
            let mut rig = Rig::new();
            rig.dc = INPUTS.iter().map(|&(i, v)| (NodeId(i), v)).collect();
            let mut history: Vec<GraphSpec> = Vec::new();
            let mut prev_out: Option<f32> = None;
            for step in 0..25 {
                let s = random_spec(&mut rng);
                rig.apply(&s);
                history.push(s);
                // Several updates can overlap (fades take ~4 blocks), so bound by recent specs.
                let window = &history[history.len().saturating_sub(6)..];
                // Up to four ramps in series (in → bus → bus → out) plus a param ramp.
                let limit = abs_bound(window, MASTER) * 5.0 / EDGE_FADE_FRAMES as f32 + 1e-5;
                let out = left(&rig.run(1 + rng.below(6)));
                let samples: Vec<f32> = prev_out.into_iter().chain(out.iter().copied()).collect();
                let worst = max_step(&samples);
                assert!(
                    worst <= limit,
                    "case {case} step {step}: jump {worst} > {limit}\n{:#?}",
                    history.last()
                );
                prev_out = out.last().copied();
            }
            rig.settle();
            let last = history.last().unwrap();
            let out = left(&rig.run(10)); // param ramps (10 ms) may still be finishing
            let want = eval(last, MASTER);
            assert!(
                (out.last().unwrap() - want).abs() < 1e-4,
                "case {case}: settled to {out:?}, want {want}"
            );
            for o in last.nodes.iter().filter(|n| n.kind.is_sink() && n.id.0 != MASTER) {
                let got = rig.engine.endpoints_mut().drain(o.id.0);
                let want = eval(last, o.id.0);
                assert!(
                    (got.last().unwrap()[0] - want).abs() < 1e-4,
                    "case {case}: out {} = {:?}, want {want}",
                    o.id.0,
                    got.last()
                );
            }
            // Exactly the endpoints the final graph needs are open; everything else released once.
            assert!(!rig.engine.running_layout().in_transition());
            let needed = rig.engine.running_layout().inputs().len() + rig.engine.running_layout().outputs().len();
            assert_eq!(
                rig.engine.endpoints().open_count(),
                needed,
                "case {case}: leaked endpoints"
            );
        }
    }
}
