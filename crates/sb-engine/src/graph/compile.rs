//! Graph compiler: `GraphSpec` → `RenderPlan`. Runs off the audio thread.
//!
//! 1. **Validate** ids, references, edge directions, values, the master.
//! 2. **Prune** nodes that feed no sink: they do no work, and their devices aren't opened.
//! 3. **Sort** topologically (Kahn; ties broken by spec order, so output is deterministic).
//!    Cycles are rejected with the nodes of one cycle.
//! 4. **Allocate buffers** by liveness: a node's buffer returns to a free list once its last
//!    reader has run, so a long chain needs two buffers, not one per node.
//! 5. **Emit ops.** Buses and sinks with a single fixed-unity input forward that buffer
//!    instead of copying it. Device rings come from [`Endpoints`].
//!
//! Param-controlled gains use the param's declared value for now; E4 makes them live.

use crate::graph::plan::{BufId, Op, PlanBuilder, RenderPlan};
use crate::graph::spec::{EdgeSpec, GraphError, GraphSpec, NodeId, NodeKind};
use crate::rt::ring::{RingReader, RingWriter};
use crate::rt::stats::StreamStats;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

/// Opens the device side of rings. The WASAPI backend starts real streams; tests hand out
/// rings they keep the other end of.
pub trait Endpoints {
    /// Ring fed by the capture device `device`, for input node `node`.
    fn input(&mut self, node: NodeId, device: &str) -> Result<RingReader, String>;
    /// Ring drained by the render device `device`, for output node `node` (not the master).
    fn output(&mut self, node: NodeId, device: &str) -> Result<RingWriter, String>;
}

pub struct Compiled {
    pub plan: RenderPlan,
    /// Endpoint ID of the master device; the mix thread's clock opens it. `None`: timer.
    pub master_device: Option<String>,
    /// Nodes that do work, in execution order (pruned nodes are absent).
    pub order: Vec<NodeId>,
    /// Mix-side stats of every input ring, by node.
    pub input_stats: Vec<(NodeId, Arc<StreamStats>)>,
    /// Mix-side stats of every output ring, by node.
    pub output_stats: Vec<(NodeId, Arc<StreamStats>)>,
}

/// Validated graph, by index into `spec.nodes`.
struct Graph<'a> {
    spec: &'a GraphSpec,
    /// Incoming edges per node, in spec order: (source index, gain).
    inputs: Vec<Vec<(usize, Gain)>>,
    /// Outgoing edges per node: destination indices.
    outputs: Vec<Vec<usize>>,
    master: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Gain {
    /// No param: fixed unity, so forwarding is allowed.
    Unity,
    Param(f32),
}

pub fn compile(spec: &GraphSpec, block: usize, endpoints: &mut impl Endpoints) -> Result<Compiled, GraphError> {
    let g = validate(spec)?;
    let live = live_nodes(&g);
    let order = topo_order(&g)?;
    emit(&g, &order, &live, block, endpoints)
}

fn validate(spec: &GraphSpec) -> Result<Graph<'_>, GraphError> {
    let mut index = HashMap::new();
    for (i, n) in spec.nodes.iter().enumerate() {
        if index.insert(n.id, i).is_some() {
            return Err(GraphError::DuplicateNode(n.id));
        }
        if let NodeKind::Tone { freq, gain } = n.kind {
            let nyquist = crate::SAMPLE_RATE as f32 / 2.0;
            if !(freq.is_finite() && (0.0..nyquist).contains(&freq) && gain.is_finite()) {
                return Err(GraphError::InvalidValue(format!(
                    "tone node {}: freq {freq} must be in 0..{nyquist}, gain {gain} finite",
                    n.id.0
                )));
            }
        }
    }

    let mut params = HashMap::new();
    for p in &spec.params {
        if params.insert(p.id, p.value).is_some() {
            return Err(GraphError::DuplicateParam(p.id));
        }
        if !p.value.is_finite() {
            return Err(GraphError::InvalidValue(format!("param {} = {}", p.id.0, p.value)));
        }
    }

    let n = spec.nodes.len();
    let (mut inputs, mut outputs) = (vec![Vec::new(); n], vec![Vec::new(); n]);
    let mut seen = HashSet::new();
    for (e, &EdgeSpec { from, to, gain }) in spec.edges.iter().enumerate() {
        let lookup = |node: NodeId| {
            index
                .get(&node)
                .copied()
                .ok_or(GraphError::UnknownNode { edge: e, node })
        };
        let (f, t) = (lookup(from)?, lookup(to)?);
        if spec.nodes[f].kind.is_sink() {
            return Err(GraphError::EdgeFromSink { edge: e, node: from });
        }
        if spec.nodes[t].kind.is_source() {
            return Err(GraphError::EdgeIntoSource { edge: e, node: to });
        }
        if !seen.insert((f, t)) {
            return Err(GraphError::DuplicateEdge { from, to });
        }
        let gain = match gain {
            None => Gain::Unity,
            Some(p) => Gain::Param(*params.get(&p).ok_or(GraphError::UnknownParam { edge: e, param: p })?),
        };
        inputs[t].push((f, gain));
        outputs[f].push(t);
    }

    let master = match spec.master {
        None => None,
        Some(m) => match index.get(&m) {
            Some(&i) if matches!(spec.nodes[i].kind, NodeKind::DeviceOutput { .. }) => Some(i),
            _ => return Err(GraphError::BadMaster(m)),
        },
    };
    Ok(Graph {
        spec,
        inputs,
        outputs,
        master,
    })
}

/// Nodes from which some sink is reachable.
fn live_nodes(g: &Graph) -> Vec<bool> {
    let mut live = vec![false; g.spec.nodes.len()];
    let mut stack: Vec<usize> = (0..live.len()).filter(|&i| g.spec.nodes[i].kind.is_sink()).collect();
    while let Some(i) = stack.pop() {
        if !std::mem::replace(&mut live[i], true) {
            stack.extend(g.inputs[i].iter().map(|&(src, _)| src));
        }
    }
    live
}

/// Kahn's algorithm over the whole graph (so cycles among dead nodes are still errors).
fn topo_order(g: &Graph) -> Result<Vec<usize>, GraphError> {
    let n = g.spec.nodes.len();
    let mut indegree: Vec<usize> = g.inputs.iter().map(Vec::len).collect();
    let mut ready: BinaryHeap<Reverse<usize>> = (0..n).filter(|&i| indegree[i] == 0).map(Reverse).collect();
    let mut order = Vec::with_capacity(n);
    while let Some(Reverse(i)) = ready.pop() {
        order.push(i);
        for &t in &g.outputs[i] {
            indegree[t] -= 1;
            if indegree[t] == 0 {
                ready.push(Reverse(t));
            }
        }
    }
    if order.len() == n {
        return Ok(order);
    }
    // Every node left has an unsorted predecessor. Walk predecessors until one repeats.
    let mut path = vec![(0..n).find(|&i| indegree[i] > 0).unwrap_or(0)];
    loop {
        let last = *path.last().unwrap_or(&0);
        let prev = g.inputs[last]
            .iter()
            .map(|&(s, _)| s)
            .find(|&s| indegree[s] > 0)
            .unwrap_or(last);
        if let Some(pos) = path.iter().position(|&p| p == prev) {
            let mut cycle: Vec<NodeId> = path[pos..].iter().rev().map(|&i| g.spec.nodes[i].id).collect();
            cycle.rotate_right(1); // start at `prev`, then follow edges forward
            return Err(GraphError::Cycle(cycle));
        }
        path.push(prev);
    }
}

/// Buffer free list with read counts.
struct Buffers {
    pending_reads: Vec<usize>,
    free: Vec<BufId>,
}

impl Buffers {
    fn alloc(&mut self, b: &mut PlanBuilder) -> BufId {
        self.free.pop().unwrap_or_else(|| {
            self.pending_reads.push(0);
            b.buffer()
        })
    }

    fn add_reads(&mut self, buf: BufId, n: usize) {
        self.pending_reads[buf.0] += n;
    }

    /// One read done; the buffer is free once nobody else will read it this block.
    fn read(&mut self, buf: BufId) {
        self.pending_reads[buf.0] -= 1;
        if self.pending_reads[buf.0] == 0 {
            self.free.push(buf);
        }
    }
}

fn emit(
    g: &Graph,
    order: &[usize],
    live: &[bool],
    block: usize,
    endpoints: &mut impl Endpoints,
) -> Result<Compiled, GraphError> {
    let mut b = PlanBuilder::new(block);
    let mut bufs = Buffers {
        pending_reads: Vec::new(),
        free: Vec::new(),
    };
    let mut value: Vec<Option<BufId>> = vec![None; g.spec.nodes.len()];
    let mut out = Compiled {
        plan: PlanBuilder::new(block).build(), // replaced below
        master_device: None,
        order: Vec::new(),
        input_stats: Vec::new(),
        output_stats: Vec::new(),
    };

    for &i in order.iter().filter(|&&i| live[i]) {
        let node = &g.spec.nodes[i];
        let endpoint_err = |reason| GraphError::Endpoint { node: node.id, reason };
        // Every live node is read once per live outgoing edge; sinks also by their own op.
        let reads = g.outputs[i].iter().filter(|&&t| live[t]).count() + node.kind.is_sink() as usize;

        let buf = match &node.kind {
            NodeKind::DeviceInput { device } => {
                let (ring, stats) = b.input(endpoints.input(node.id, device).map_err(endpoint_err)?);
                out.input_stats.push((node.id, stats));
                let dst = bufs.alloc(&mut b);
                b.op(Op::Pull { ring, dst });
                dst
            }
            NodeKind::Tone { freq, gain } => {
                let osc = b.oscillator();
                let dst = bufs.alloc(&mut b);
                b.op(Op::Sine {
                    osc,
                    dst,
                    freq: *freq,
                    gain: *gain,
                });
                dst
            }
            NodeKind::Bus | NodeKind::DeviceOutput { .. } => match g.inputs[i].as_slice() {
                // Forward: same buffer, no ops. Its reads are added before ours is consumed,
                // so it can't be freed in between.
                &[(src, Gain::Unity)] => {
                    let shared = value[src].expect("inputs come first in topological order");
                    bufs.add_reads(shared, reads);
                    bufs.read(shared);
                    value[i] = Some(shared);
                    shared
                }
                inputs => {
                    // Allocated before any input is released, so dst never aliases a source.
                    let dst = bufs.alloc(&mut b);
                    b.op(Op::Clear { dst });
                    for &(src, gain) in inputs {
                        let src_buf = value[src].expect("inputs come first in topological order");
                        let gain = match gain {
                            Gain::Unity => 1.0,
                            Gain::Param(v) => v,
                        };
                        b.op(Op::MixAdd {
                            src: src_buf,
                            dst,
                            gain,
                        });
                        bufs.read(src_buf);
                    }
                    dst
                }
            },
        };
        if value[i].is_none() {
            bufs.add_reads(buf, reads);
            value[i] = Some(buf);
        }

        if let NodeKind::DeviceOutput { device } = &node.kind {
            if g.master == Some(i) {
                // `Master` only names the bus; the clock reads it after the whole block, so its
                // read stays pending and the buffer is never handed to a later node.
                b.op(Op::Master { src: buf });
                out.master_device = Some(device.clone());
            } else {
                let (ring, stats) = b.output(endpoints.output(node.id, device).map_err(endpoint_err)?);
                out.output_stats.push((node.id, stats));
                b.op(Op::Push { src: buf, ring });
                bufs.read(buf);
            }
        }
        out.order.push(node.id);
    }

    out.plan = b.build();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Frame;
    use crate::graph::spec::{NodeSpec, ParamId, ParamSpec};
    use crate::rt::offline;
    use crate::rt::ring::ring;
    use rtrb::{Consumer, Producer};

    /// Hands out rings and keeps the device ends: producers for inputs, consumers for outputs.
    #[derive(Default)]
    struct FakeEndpoints {
        inputs: Vec<(NodeId, Producer<Frame>)>,
        outputs: Vec<(NodeId, Consumer<Frame>)>,
        opened: Vec<String>,
        fail: Option<String>,
    }

    impl Endpoints for FakeEndpoints {
        fn input(&mut self, node: NodeId, device: &str) -> Result<RingReader, String> {
            if self.fail.as_deref() == Some(device) {
                return Err(format!("{device} not found"));
            }
            self.opened.push(device.into());
            let (p, c) = rtrb::RingBuffer::new(4096);
            self.inputs.push((node, p));
            Ok(RingReader::new(c, 0, 4096))
        }
        fn output(&mut self, node: NodeId, device: &str) -> Result<RingWriter, String> {
            self.opened.push(device.into());
            let (w, c) = ring(1 << 16);
            self.outputs.push((node, c));
            Ok(w)
        }
    }

    impl FakeEndpoints {
        fn feed(&mut self, node: u32, frames: &[Frame]) {
            let p = &mut self.inputs.iter_mut().find(|(n, _)| n.0 == node).unwrap().1;
            frames.iter().for_each(|f| p.push(*f).unwrap());
        }
        fn drain(&mut self, node: u32) -> Vec<Frame> {
            let c = &mut self.outputs.iter_mut().find(|(n, _)| n.0 == node).unwrap().1;
            std::iter::from_fn(|| c.pop().ok()).collect()
        }
    }

    // ---- spec helpers ----
    fn node(id: u32, kind: NodeKind) -> NodeSpec {
        NodeSpec { id: NodeId(id), kind }
    }
    fn tone(id: u32, freq: f32, gain: f32) -> NodeSpec {
        node(id, NodeKind::Tone { freq, gain })
    }
    fn input(id: u32, dev: &str) -> NodeSpec {
        node(id, NodeKind::DeviceInput { device: dev.into() })
    }
    fn bus(id: u32) -> NodeSpec {
        node(id, NodeKind::Bus)
    }
    fn output(id: u32, dev: &str) -> NodeSpec {
        node(id, NodeKind::DeviceOutput { device: dev.into() })
    }
    fn edge(from: u32, to: u32) -> EdgeSpec {
        EdgeSpec {
            from: NodeId(from),
            to: NodeId(to),
            gain: None,
        }
    }
    fn gain_edge(from: u32, to: u32, param: u32) -> EdgeSpec {
        EdgeSpec {
            from: NodeId(from),
            to: NodeId(to),
            gain: Some(ParamId(param)),
        }
    }
    fn param(id: u32, value: f32) -> ParamSpec {
        ParamSpec { id: ParamId(id), value }
    }

    fn compile_ok(spec: &GraphSpec) -> (Compiled, FakeEndpoints) {
        let mut ep = FakeEndpoints::default();
        let c = compile(spec, 64, &mut ep).unwrap_or_else(|e| panic!("compile failed: {e}"));
        (c, ep)
    }

    fn compile_err(spec: &GraphSpec) -> GraphError {
        match compile(spec, 64, &mut FakeEndpoints::default()) {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        }
    }

    fn sine_at(i: usize, freq: f32, gain: f32) -> f32 {
        (i as f64 * freq as f64 / crate::SAMPLE_RATE as f64 * std::f64::consts::TAU).sin() as f32 * gain
    }

    // ---- emission ----

    #[test]
    fn single_unity_edge_forwards_without_ops() {
        let spec = GraphSpec {
            nodes: vec![tone(1, 1000.0, 0.5), output(2, "spk")],
            edges: vec![edge(1, 2)],
            master: Some(NodeId(2)),
            ..Default::default()
        };
        let (mut c, _) = compile_ok(&spec);
        assert!(
            matches!(c.plan.ops(), [Op::Sine { .. }, Op::Master { src: BufId(0) }]),
            "{:?}",
            c.plan.ops()
        );
        assert_eq!(c.master_device.as_deref(), Some("spk"));
        let out = offline::render(&mut c.plan, 3);
        for (i, f) in out.iter().enumerate() {
            assert!((f[0] - sine_at(i, 1000.0, 0.5)).abs() < 1e-4);
        }
    }

    #[test]
    fn fan_in_mixes_with_param_gains() {
        let spec = GraphSpec {
            nodes: vec![input(1, "mic"), tone(2, 0.0, 1.0), bus(3), output(4, "spk")],
            edges: vec![gain_edge(1, 3, 10), edge(2, 3), gain_edge(3, 4, 11)],
            params: vec![param(10, 0.5), param(11, 2.0)],
            master: Some(NodeId(4)),
        };
        let (mut c, mut ep) = compile_ok(&spec);
        ep.feed(1, &(0..64).map(|i| [i as f32, -1.0]).collect::<Vec<_>>());
        let out = offline::render(&mut c.plan, 1);
        for (i, f) in out.iter().enumerate() {
            assert_eq!(*f, [i as f32 * 0.5 * 2.0, -1.0], "frame {i}");
        }
    }

    #[test]
    fn param_edge_is_never_forwarded_even_at_unity() {
        // A param can change later (E4), so the bus must own its buffer.
        let spec = GraphSpec {
            nodes: vec![tone(1, 100.0, 1.0), bus(2), output(3, "spk")],
            edges: vec![gain_edge(1, 2, 7), edge(2, 3)],
            params: vec![param(7, 1.0)],
            master: Some(NodeId(3)),
        };
        let (c, _) = compile_ok(&spec);
        assert!(c.plan.ops().iter().any(|op| matches!(op, Op::MixAdd { .. })));
    }

    #[test]
    fn fan_out_reads_one_buffer_without_copies() {
        let spec = GraphSpec {
            nodes: vec![tone(1, 440.0, 0.25), output(2, "spk"), output(3, "cable")],
            edges: vec![edge(1, 2), edge(1, 3)],
            master: Some(NodeId(2)),
            ..Default::default()
        };
        let (mut c, mut ep) = compile_ok(&spec);
        assert_eq!(c.plan.ops().len(), 3, "{:?}", c.plan.ops()); // Sine, Master, Push
        assert_eq!(c.plan.buffer_count(), 1);
        let master = offline::render(&mut c.plan, 4);
        assert_eq!(ep.drain(3), master, "both sinks get the same signal");
    }

    #[test]
    fn sink_without_inputs_plays_silence() {
        let spec = GraphSpec {
            nodes: vec![output(1, "spk")],
            master: Some(NodeId(1)),
            ..Default::default()
        };
        let (mut c, _) = compile_ok(&spec);
        assert!(offline::render(&mut c.plan, 2).iter().all(|f| *f == [0.0; 2]));
    }

    #[test]
    fn master_bus_survives_later_nodes() {
        // Found by the randomized test: an input-less master ran first, its buffer was
        // released after `Master`, and a later tone overwrote it.
        let spec = GraphSpec {
            nodes: vec![output(1, "spk"), tone(2, 440.0, 1.0), output(3, "cable")],
            edges: vec![edge(2, 3)],
            master: Some(NodeId(1)),
            ..Default::default()
        };
        let (mut c, _) = compile_ok(&spec);
        assert!(
            offline::render(&mut c.plan, 2).iter().all(|f| *f == [0.0; 2]),
            "{:?}",
            c.plan.ops()
        );
    }

    #[test]
    fn no_master_means_timer_and_rings_only() {
        let spec = GraphSpec {
            nodes: vec![tone(1, 440.0, 1.0), output(2, "spk")],
            edges: vec![edge(1, 2)],
            ..Default::default()
        };
        let (mut c, mut ep) = compile_ok(&spec);
        assert_eq!(c.master_device, None);
        assert!(
            offline::render(&mut c.plan, 2).iter().all(|f| *f == [0.0; 2]),
            "master bus is silent"
        );
        assert_eq!(ep.drain(2).len(), 128);
    }

    #[test]
    fn dead_nodes_are_pruned_and_their_devices_never_opened() {
        let spec = GraphSpec {
            nodes: vec![input(1, "unused-mic"), bus(2), tone(3, 440.0, 1.0), output(4, "spk")],
            edges: vec![edge(1, 2), edge(3, 4)],
            master: Some(NodeId(4)),
            ..Default::default()
        };
        let (c, ep) = compile_ok(&spec);
        assert_eq!(c.order, vec![NodeId(3), NodeId(4)]);
        assert!(ep.opened.is_empty(), "opened {:?}", ep.opened);
    }

    #[test]
    fn chains_reuse_buffers() {
        // tone → 20 buses, each with a gain (so no forwarding) → output.
        let mut nodes = vec![tone(0, 300.0, 1.0)];
        let mut edges = Vec::new();
        let mut params = Vec::new();
        for i in 1..=20 {
            nodes.push(bus(i));
            edges.push(gain_edge(i - 1, i, i));
            params.push(param(i, 0.9));
        }
        nodes.push(output(21, "spk"));
        edges.push(edge(20, 21));
        let spec = GraphSpec {
            nodes,
            edges,
            params,
            master: Some(NodeId(21)),
        };
        let (mut c, _) = compile_ok(&spec);
        assert_eq!(c.plan.buffer_count(), 2, "liveness should ping-pong two buffers");
        let out = offline::render(&mut c.plan, 2);
        let g = 0.9f32.powi(20);
        for (i, f) in out.iter().enumerate() {
            assert!((f[0] - sine_at(i, 300.0, g)).abs() < 1e-4, "frame {i}");
        }
    }

    #[test]
    fn order_is_deterministic_and_respects_edges() {
        let spec = GraphSpec {
            // Declared out of order: the bus comes before its sources.
            nodes: vec![output(9, "spk"), bus(5), tone(2, 1.0, 1.0), tone(1, 2.0, 1.0)],
            edges: vec![edge(5, 9), edge(2, 5), edge(1, 5)],
            master: Some(NodeId(9)),
            ..Default::default()
        };
        let a = compile_ok(&spec).0.order;
        assert_eq!(a, vec![NodeId(2), NodeId(1), NodeId(5), NodeId(9)]);
        assert_eq!(compile_ok(&spec).0.order, a);
    }

    // ---- validation ----

    #[test]
    fn rejects_cycles_naming_the_cycle() {
        let spec = GraphSpec {
            nodes: vec![tone(1, 440.0, 1.0), bus(2), bus(3), bus(4), output(5, "spk")],
            edges: vec![edge(1, 2), edge(2, 3), edge(3, 4), edge(4, 2), edge(4, 5)],
            master: Some(NodeId(5)),
            ..Default::default()
        };
        match compile_err(&spec) {
            GraphError::Cycle(c) => {
                let mut sorted = c.clone();
                sorted.sort();
                assert_eq!(sorted, vec![NodeId(2), NodeId(3), NodeId(4)], "{c:?}");
                // Each step follows a real edge, and it closes back on the first node.
                for w in c.iter().chain(c.first()).collect::<Vec<_>>().windows(2) {
                    assert!(spec.edges.iter().any(|e| e.from == *w[0] && e.to == *w[1]), "{c:?}");
                }
            }
            e => panic!("expected Cycle, got {e}"),
        }
    }

    #[test]
    fn rejects_self_loop_and_dead_cycles() {
        let self_loop = GraphSpec {
            nodes: vec![bus(1)],
            edges: vec![edge(1, 1)],
            ..Default::default()
        };
        assert_eq!(compile_err(&self_loop), GraphError::Cycle(vec![NodeId(1)]));
        let dead = GraphSpec {
            nodes: vec![bus(1), bus(2)],
            edges: vec![edge(1, 2), edge(2, 1)],
            ..Default::default()
        };
        assert!(matches!(compile_err(&dead), GraphError::Cycle(_)));
    }

    #[test]
    fn rejects_bad_specs() {
        let base = || GraphSpec {
            nodes: vec![tone(1, 440.0, 1.0), bus(2), output(3, "spk")],
            edges: vec![edge(1, 2), edge(2, 3)],
            master: Some(NodeId(3)),
            ..Default::default()
        };
        let with = |f: &dyn Fn(&mut GraphSpec)| {
            let mut s = base();
            f(&mut s);
            compile_err(&s)
        };
        assert_eq!(with(&|s| s.nodes.push(bus(2))), GraphError::DuplicateNode(NodeId(2)));
        assert_eq!(
            with(&|s| s.edges.push(edge(1, 7))),
            GraphError::UnknownNode {
                edge: 2,
                node: NodeId(7)
            }
        );
        assert_eq!(
            with(&|s| s.edges.push(gain_edge(1, 3, 4))),
            GraphError::UnknownParam {
                edge: 2,
                param: ParamId(4)
            }
        );
        assert_eq!(
            with(&|s| s.params.extend([param(1, 1.0), param(1, 0.5)])),
            GraphError::DuplicateParam(ParamId(1))
        );
        assert_eq!(
            with(&|s| s.edges.push(edge(1, 2))),
            GraphError::DuplicateEdge {
                from: NodeId(1),
                to: NodeId(2)
            }
        );
        assert_eq!(
            with(&|s| s.edges.push(edge(2, 1))),
            GraphError::EdgeIntoSource {
                edge: 2,
                node: NodeId(1)
            }
        );
        assert_eq!(
            with(&|s| {
                s.nodes.push(bus(4));
                s.edges.push(edge(3, 4));
            }),
            GraphError::EdgeFromSink {
                edge: 2,
                node: NodeId(3)
            }
        );
        assert_eq!(with(&|s| s.master = Some(NodeId(2))), GraphError::BadMaster(NodeId(2)));
        assert_eq!(
            with(&|s| s.master = Some(NodeId(42))),
            GraphError::BadMaster(NodeId(42))
        );
        assert!(matches!(
            with(&|s| s.nodes[0] = tone(1, 30_000.0, 1.0)),
            GraphError::InvalidValue(_)
        ));
        assert!(matches!(
            with(&|s| s.nodes[0] = tone(1, f32::NAN, 1.0)),
            GraphError::InvalidValue(_)
        ));
        assert!(matches!(
            with(&|s| s.params.push(param(9, f32::INFINITY))),
            GraphError::InvalidValue(_)
        ));
    }

    #[test]
    fn endpoint_failure_names_the_node() {
        let spec = GraphSpec {
            nodes: vec![input(1, "gone"), output(2, "spk")],
            edges: vec![edge(1, 2)],
            master: Some(NodeId(2)),
            ..Default::default()
        };
        let mut ep = FakeEndpoints {
            fail: Some("gone".into()),
            ..Default::default()
        };
        match compile(&spec, 64, &mut ep) {
            Err(GraphError::Endpoint { node, reason }) => {
                assert_eq!(node, NodeId(1));
                assert!(reason.contains("not found"));
            }
            other => panic!("expected Endpoint error, got {:?}", other.err()),
        }
    }

    // ---- randomized: compiled output == direct evaluation ----

    /// Tiny deterministic PRNG (xorshift), so the test needs no dependencies.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Random DAG: tones, then buses, then outputs; edges only go "forward", so no cycles.
    fn random_spec(rng: &mut Rng) -> GraphSpec {
        let (tones, buses, outs) = (1 + rng.below(4), rng.below(6), 1 + rng.below(3));
        let mut spec = GraphSpec::default();
        let mut id = 0;
        for _ in 0..tones {
            let freq = 50.0 + rng.below(5000) as f32;
            spec.nodes.push(tone(id, freq, 0.1 + rng.below(9) as f32 / 10.0));
            id += 1;
        }
        for _ in 0..buses {
            spec.nodes.push(bus(id));
            id += 1;
        }
        for k in 0..outs {
            spec.nodes.push(output(id, &format!("out{k}")));
            id += 1;
        }
        let n = spec.nodes.len();
        for to in tones..n {
            for from in 0..to.min(tones + buses) {
                if rng.below(3) == 0 {
                    let gain = match rng.below(2) {
                        0 => None,
                        _ => {
                            let p = spec.params.len() as u32;
                            spec.params.push(param(p, rng.below(20) as f32 / 10.0 - 1.0));
                            Some(ParamId(p))
                        }
                    };
                    spec.edges.push(EdgeSpec {
                        from: NodeId(from as u32),
                        to: NodeId(to as u32),
                        gain,
                    });
                }
            }
        }
        // Shuffle declaration order: the compiler must not depend on it.
        for i in (1..spec.nodes.len()).rev() {
            spec.nodes.swap(i, rng.below(i + 1));
        }
        spec.master = Some(NodeId((n - 1) as u32));
        spec
    }

    fn dump(spec: &GraphSpec) -> String {
        let mut s = String::new();
        for n in &spec.nodes {
            s += &format!("node {} {:?}\n", n.id.0, n.kind);
        }
        for e in &spec.edges {
            let g = e.gain.map(|p| spec.params.iter().find(|q| q.id == p).unwrap().value);
            s += &format!("edge {} -> {} gain {g:?}\n", e.from.0, e.to.0);
        }
        s + &format!("master {:?}\n", spec.master)
    }

    /// Direct recursive evaluation of node `id` at sample `i`.
    fn eval(spec: &GraphSpec, id: NodeId, i: usize) -> f32 {
        let node = spec.nodes.iter().find(|n| n.id == id).unwrap();
        match node.kind {
            NodeKind::Tone { freq, gain } => sine_at(i, freq, gain),
            _ => spec
                .edges
                .iter()
                .filter(|e| e.to == id)
                .map(|e| {
                    let g = e
                        .gain
                        .map_or(1.0, |p| spec.params.iter().find(|q| q.id == p).unwrap().value);
                    eval(spec, e.from, i) * g
                })
                .sum(),
        }
    }

    #[test]
    fn random_graphs_render_like_direct_evaluation() {
        let mut rng = Rng(0x5eed_cafe_f00d);
        for case in 0..1000 {
            let spec = random_spec(&mut rng);
            let (mut c, mut ep) = compile_ok(&spec);
            let master = offline::render(&mut c.plan, 2);
            for out in spec.nodes.iter().filter(|n| n.kind.is_sink()) {
                let got = if Some(out.id) == spec.master {
                    master.clone()
                } else {
                    ep.drain(out.id.0)
                };
                for (i, f) in got.iter().enumerate() {
                    let want = eval(&spec, out.id, i);
                    assert!(
                        (f[0] - want).abs() < 1e-3 && f[0] == f[1],
                        "case {case}, node {}, sample {i}: got {f:?}, want {want}\n{}ops: {:?}",
                        out.id.0,
                        dump(&spec),
                        c.plan.ops()
                    );
                }
            }
        }
    }
}
