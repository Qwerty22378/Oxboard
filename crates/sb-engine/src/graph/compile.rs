//! Graph compiler: `GraphSpec` → `RenderPlan`. Runs off the audio thread.
//!
//! 1. **Validate** ids, references, edge directions, values, the master, meters.
//! 2. **Transition** (when compiling against the running plan's [`Layout`]): edges that are new
//!    fade in; edges and nodes that are gone stay as **ghosts** that fade out. A later
//!    **settle** plan drops the ghosts once the fades are done.
//! 3. **Prune** nodes that feed no sink and aren't metered: they do no work, and their devices
//!    aren't opened.
//! 4. **Sort** topologically (Kahn; ties broken by declaration order, so output is
//!    deterministic). Cycles are rejected with the nodes of one cycle.
//! 5. **Allocate buffers** by liveness: a node's buffer returns to a free list once its last
//!    reader has run, so a long chain needs two buffers, not one per node.
//! 6. **Emit ops.** A bus or sink with a single fixed-unity, steady input forwards that
//!    buffer instead of copying it. State that already exists in the running plan (rings,
//!    oscillator phase, smoothers, fades, meter RMS) is **inherited** by key — node, edge or
//!    meter — through handoff moves; only new device nodes open endpoints.
//!
//! Declared param values are written to the param table before emitting, so new smoothers
//! start settled at them. After that the controller moves params with `ParamTable::set`.

use crate::clips::{CLIP_KEY_BIT, ClipStore};
use crate::graph::plan::{self, BufId, Op, PlanBuilder, RenderPlan};
use crate::graph::spec::{EdgeSpec, GraphError, GraphSpec, IdKind, MeterId, NodeId, NodeKind, NodeSpec, ParamId};
use crate::nodes::clip::ClipSettings;
use crate::rt::meters::MeterTable;
use crate::rt::params::ParamTable;
use crate::rt::ring::{RingReader, RingWriter};
use crate::rt::stats::StreamStats;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

/// Identifies one opened device endpoint (one stream), so it can be released exactly once,
/// even if a node id is reused while the old stream is still fading out.
pub type EndpointKey = u64;

/// Opens and closes the device side of rings. The WASAPI backend starts real streams; tests
/// hand out rings they keep the other end of.
pub trait Endpoints {
    /// Ring fed by the capture device `device`, for input node `node`.
    fn input(&mut self, node: NodeId, device: &str) -> Result<(RingReader, EndpointKey), String>;
    /// Ring drained by the render device `device`, for output node `node` (not the master).
    fn output(&mut self, node: NodeId, device: &str) -> Result<(RingWriter, EndpointKey), String>;
    /// Ring fed by the driver's virtual render endpoint `endpoint` (what apps play into it).
    fn virtual_input(&mut self, _node: NodeId, _endpoint: u32) -> Result<(RingReader, EndpointKey), String> {
        Err("virtual endpoints need the engine's `driver` feature".into())
    }
    /// Ring feeding the driver's virtual microphone `endpoint`.
    fn virtual_output(&mut self, _node: NodeId, _endpoint: u32) -> Result<(RingWriter, EndpointKey), String> {
        Err("virtual endpoints need the engine's `driver` feature".into())
    }
    /// No plan uses this endpoint any more: close it.
    fn release(&mut self, key: EndpointKey);
    /// The master device is about to change (it sets how bursty the mix is, which sizes
    /// new rings).
    fn master_changed(&mut self, _device: Option<&str>) {}
    /// Nodes whose stream died since the last call (unplugged, disabled, invalidated). Each
    /// is reported once; the engine forwards them as `DeviceLost`.
    fn take_lost(&mut self) -> Vec<NodeId> {
        Vec::new()
    }
    /// Device-side counters of every open stream: (node, captures?, stats).
    fn device_stats(&self) -> Vec<(NodeId, bool, crate::rt::stats::StatsSnapshot)> {
        Vec::new()
    }
}

/// What plans share with the controller side: the param and meter tables (fixed size; ids
/// index them directly) and the clip store (decoded clips, long-clip streamers).
#[derive(Clone)]
pub struct Tables {
    pub params: Arc<ParamTable>,
    pub meters: Arc<MeterTable>,
    pub clips: Arc<ClipStore>,
}

impl Tables {
    pub fn new(params: usize, meters: usize) -> Self {
        Self::with_clips(params, meters, ClipStore::default())
    }

    pub fn with_clips(params: usize, meters: usize, clips: ClipStore) -> Self {
        Self {
            params: Arc::new(ParamTable::new(params)),
            meters: Arc::new(MeterTable::new(meters)),
            clips: Arc::new(clips),
        }
    }
}

/// Same node for inheritance purposes. A clip is the same clip as long as its file is: its
/// settings (polyphony, retrigger, loop) can change while it plays.
fn same_identity(a: &NodeKind, b: &NodeKind) -> bool {
    match (a, b) {
        (NodeKind::Clip { path: pa, .. }, NodeKind::Clip { path: pb, .. }) => pa == pb,
        _ => a == b,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeStatus {
    Steady,
    FadingIn,
    /// A ghost: gone from the spec, fading out until the settle plan drops it.
    FadingOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Fade new edges in and removed edges (and nodes) out, relative to the previous layout.
    Transition,
    /// Every edge steady, no ghosts. After a transition this plan waits for its fades.
    Settle,
}

/// One opened endpoint as a plan uses it.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub node: NodeId,
    pub key: EndpointKey,
    /// Mix-side counters of the ring.
    pub stats: Arc<StreamStats>,
}

/// An edge's identity: its two nodes and its gain source. Changing an edge's gain from fixed
/// to a param (or to another param) makes it a different edge: the old one fades out while
/// the new one fades in, instead of the gain jumping.
type EdgeKey = (NodeId, NodeId, Option<ParamId>);

/// What a compiled plan contains, by stable key: the next compile inherits from it.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    nodes: Vec<NodeSpec>,
    edges: Vec<(EdgeSpec, EdgeStatus)>,
    ghosts: usize,
    master_device: Option<String>,
    inputs: Vec<Endpoint>,
    outputs: Vec<Endpoint>,
    oscs: Vec<NodeId>,
    smoothers: Vec<EdgeKey>,
    fades: Vec<EdgeKey>,
    rms: Vec<(MeterId, NodeId)>,
    /// Clip slots: node and (long clips) its streamer's key.
    clips: Vec<(NodeId, Option<EndpointKey>)>,
}

impl Layout {
    /// The layout of an empty plan playing (silence) on `master`.
    pub(crate) fn with_master(master: Option<String>) -> Self {
        Self {
            master_device: master,
            ..Self::default()
        }
    }

    /// Fades or ghosts present: a settle plan should follow.
    pub fn in_transition(&self) -> bool {
        self.ghosts > 0 || self.edges.iter().any(|(_, s)| *s != EdgeStatus::Steady)
    }

    /// Nodes that do work (including ghosts), in execution order.
    pub fn nodes(&self) -> &[NodeSpec] {
        &self.nodes
    }

    pub fn edge_status(&self, from: NodeId, to: NodeId) -> Option<EdgeStatus> {
        self.edges
            .iter()
            .find(|(e, _)| e.from == from && e.to == to)
            .map(|(_, s)| *s)
    }

    pub fn master_device(&self) -> Option<&str> {
        self.master_device.as_deref()
    }

    pub fn inputs(&self) -> &[Endpoint] {
        &self.inputs
    }

    pub fn outputs(&self) -> &[Endpoint] {
        &self.outputs
    }

    pub fn uses_endpoint(&self, key: EndpointKey) -> bool {
        self.endpoint_keys().any(|k| k == key)
    }

    /// Device and virtual endpoints, and clip streamers (`CLIP_KEY_BIT` set).
    pub fn endpoint_keys(&self) -> impl Iterator<Item = EndpointKey> + '_ {
        self.inputs
            .iter()
            .chain(&self.outputs)
            .map(|e| e.key)
            .chain(self.clips.iter().filter_map(|c| c.1))
    }
}

pub struct Compiled {
    pub plan: RenderPlan,
    pub layout: Layout,
}

/// Compile with no running plan to inherit from: every edge steady, nothing fades.
pub fn compile(
    spec: &GraphSpec,
    block: usize,
    tables: &Tables,
    endpoints: &mut impl Endpoints,
) -> Result<Compiled, GraphError> {
    compile_against(spec, block, tables, endpoints, &Layout::default(), Mode::Settle)
}

/// Compile `spec` to replace the plan described by `prev`. On error nothing it opened stays
/// open, and the running plan is untouched.
pub fn compile_against(
    spec: &GraphSpec,
    block: usize,
    tables: &Tables,
    endpoints: &mut impl Endpoints,
    prev: &Layout,
    mode: Mode,
) -> Result<Compiled, GraphError> {
    let mut g = validate(spec, tables)?;
    let order = match mode {
        Mode::Settle => topo_order(&g)?,
        Mode::Transition => {
            transition(&mut g, prev, true);
            match topo_order(&g) {
                Ok(order) => order,
                // Old and new edges together can form a cycle (A→B removed, B→A added): give
                // up the fade-outs rather than the update.
                Err(GraphError::Cycle(_)) => {
                    g = validate(spec, tables)?;
                    transition(&mut g, prev, false);
                    topo_order(&g)?
                }
                Err(e) => return Err(e),
            }
        }
    };
    let live = live_nodes(&g);
    for p in &spec.params {
        tables.params.set(p.id, p.value);
    }
    let wait_for_fades = mode == Mode::Settle && prev.in_transition();
    let mut opened = Vec::new();
    let result = emit(
        &g,
        &order,
        &live,
        block,
        tables,
        endpoints,
        prev,
        wait_for_fades,
        &mut opened,
    );
    if result.is_err() {
        for key in opened {
            release(key, tables, endpoints);
        }
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Gain {
    /// No param: fixed unity, so forwarding is allowed.
    Unity,
    Param(ParamId),
}

#[derive(Debug, Clone, Copy)]
struct InEdge {
    src: usize,
    gain: Gain,
    status: EdgeStatus,
}

/// Validated graph, by node index.
struct Graph {
    nodes: Vec<NodeSpec>,
    /// Incoming edges per node, in declaration order.
    inputs: Vec<Vec<InEdge>>,
    /// Outgoing edges per node: destination indices.
    outputs: Vec<Vec<usize>>,
    master: Option<usize>,
    /// Meters per node.
    meters: Vec<Vec<MeterId>>,
    ghost: Vec<bool>,
}

impl Graph {
    fn add_edge(&mut self, from: usize, to: usize, gain: Gain, status: EdgeStatus) {
        self.inputs[to].push(InEdge {
            src: from,
            gain,
            status,
        });
        self.outputs[from].push(to);
    }
}

fn validate(spec: &GraphSpec, tables: &Tables) -> Result<Graph, GraphError> {
    let in_range = |kind, id: u32, capacity: usize| match (id as usize) < capacity {
        true => Ok(()),
        false => Err(GraphError::IdOutOfRange { kind, id, capacity }),
    };
    let mut index = HashMap::new();
    for (i, n) in spec.nodes.iter().enumerate() {
        if index.insert(n.id, i).is_some() {
            return Err(GraphError::DuplicateNode(n.id));
        }
        if let NodeKind::Clip { polyphony, .. } = n.kind
            && !(1..=crate::nodes::clip::MAX_POLYPHONY).contains(&polyphony)
        {
            return Err(GraphError::InvalidValue(format!(
                "clip node {}: polyphony {polyphony} must be 1..={}",
                n.id.0,
                crate::nodes::clip::MAX_POLYPHONY
            )));
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

    let mut params = HashSet::new();
    for p in &spec.params {
        if !params.insert(p.id) {
            return Err(GraphError::DuplicateParam(p.id));
        }
        in_range(IdKind::Param, p.id.0, tables.params.capacity())?;
        if !p.value.is_finite() {
            return Err(GraphError::InvalidValue(format!("param {} = {}", p.id.0, p.value)));
        }
    }

    let n = spec.nodes.len();
    let mut g = Graph {
        nodes: spec.nodes.clone(),
        inputs: vec![Vec::new(); n],
        outputs: vec![Vec::new(); n],
        master: None,
        meters: vec![Vec::new(); n],
        ghost: vec![false; n],
    };
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
            Some(p) if params.contains(&p) => Gain::Param(p),
            Some(p) => return Err(GraphError::UnknownParam { edge: e, param: p }),
        };
        g.add_edge(f, t, gain, EdgeStatus::Steady);
    }

    g.master = match spec.master {
        None => None,
        Some(m) => match index.get(&m) {
            Some(&i) if matches!(spec.nodes[i].kind, NodeKind::DeviceOutput { .. }) => Some(i),
            _ => return Err(GraphError::BadMaster(m)),
        },
    };

    let mut meter_ids = HashSet::new();
    for m in &spec.meters {
        if !meter_ids.insert(m.id) {
            return Err(GraphError::DuplicateMeter(m.id));
        }
        in_range(IdKind::Meter, m.id.0, tables.meters.capacity())?;
        let i = *index.get(&m.node).ok_or(GraphError::UnknownMeterNode {
            meter: m.id,
            node: m.node,
        })?;
        g.meters[i].push(m.id);
    }
    Ok(g)
}

/// Mark edge statuses relative to `prev`, and (with `ghosts`) add what disappeared as ghosts.
///
/// - An edge that was steady and whose two nodes are unchanged stays steady.
/// - Any other edge in the spec fades in (continuing from wherever an inherited fade is).
/// - An edge of `prev` missing from the spec fades out, together with any missing node it
///   needs. A node that changed kind (or device) under the same id is replaced, not ghosted.
///   The old master sink has no ring to keep feeding, so it isn't ghosted either.
fn transition(g: &mut Graph, prev: &Layout, ghosts: bool) {
    let prev_kind: HashMap<NodeId, &NodeKind> = prev.nodes.iter().map(|n| (n.id, &n.kind)).collect();
    let prev_edges: HashMap<EdgeKey, EdgeStatus> =
        prev.edges.iter().map(|(e, s)| ((e.from, e.to, e.gain), *s)).collect();
    let mut index: HashMap<NodeId, usize> = g.nodes.iter().enumerate().map(|(i, n)| (n.id, i)).collect();
    let unchanged = |g: &Graph, i: usize| {
        prev_kind
            .get(&g.nodes[i].id)
            .is_some_and(|k| same_identity(k, &g.nodes[i].kind))
    };

    for t in 0..g.nodes.len() {
        for k in 0..g.inputs[t].len() {
            let src = g.inputs[t][k].src;
            let key = (g.nodes[src].id, g.nodes[t].id, param_of(g.inputs[t][k].gain));
            let steady = prev_edges.get(&key) == Some(&EdgeStatus::Steady) && unchanged(g, src) && unchanged(g, t);
            g.inputs[t][k].status = if steady {
                EdgeStatus::Steady
            } else {
                EdgeStatus::FadingIn
            };
        }
    }
    if !ghosts {
        return;
    }

    let current: HashSet<EdgeKey> = (0..g.nodes.len())
        .flat_map(|t| g.inputs[t].iter().map(move |e| (e.src, t, e.gain)))
        .map(|(s, t, gain)| (g.nodes[s].id, g.nodes[t].id, param_of(gain)))
        .collect();
    let ghostable = |id: NodeId| match prev_kind.get(&id) {
        Some(NodeKind::DeviceOutput { .. }) => prev.outputs.iter().any(|o| o.node == id),
        Some(_) => true,
        None => false,
    };
    for (edge, _) in &prev.edges {
        if current.contains(&(edge.from, edge.to, edge.gain)) {
            continue;
        }
        // Each end must be the same node it was, or a node that's gone and can be ghosted.
        let usable = |id: NodeId| match index.get(&id) {
            Some(&i) => unchanged(g, i),
            None => ghostable(id),
        };
        if !(usable(edge.from) && usable(edge.to)) {
            continue;
        }
        let mut node = |id: NodeId, g: &mut Graph| match index.get(&id) {
            Some(&i) => i,
            None => {
                let kind = (*prev_kind[&id]).clone();
                g.nodes.push(NodeSpec { id, kind });
                g.inputs.push(Vec::new());
                g.outputs.push(Vec::new());
                g.meters.push(Vec::new());
                g.ghost.push(true);
                index.insert(id, g.nodes.len() - 1);
                g.nodes.len() - 1
            }
        };
        let (f, t) = (node(edge.from, g), node(edge.to, g));
        let gain = edge.gain.map_or(Gain::Unity, Gain::Param);
        g.add_edge(f, t, gain, EdgeStatus::FadingOut);
    }
}

/// Nodes from which some sink or some metered node is reachable.
fn live_nodes(g: &Graph) -> Vec<bool> {
    let mut live = vec![false; g.nodes.len()];
    let mut stack: Vec<usize> = (0..live.len())
        .filter(|&i| g.nodes[i].kind.is_sink() || !g.meters[i].is_empty())
        .collect();
    while let Some(i) = stack.pop() {
        if !std::mem::replace(&mut live[i], true) {
            stack.extend(g.inputs[i].iter().map(|e| e.src));
        }
    }
    live
}

/// Kahn's algorithm over the whole graph (so cycles among dead nodes are still errors).
fn topo_order(g: &Graph) -> Result<Vec<usize>, GraphError> {
    let n = g.nodes.len();
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
            .map(|e| e.src)
            .find(|&s| indegree[s] > 0)
            .unwrap_or(last);
        if let Some(pos) = path.iter().position(|&p| p == prev) {
            let mut cycle: Vec<NodeId> = path[pos..].iter().rev().map(|&i| g.nodes[i].id).collect();
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

/// Where each piece of state lives in the running plan, by key.
struct Inherit<'a> {
    kind: HashMap<NodeId, &'a NodeKind>,
    inputs: HashMap<NodeId, (usize, &'a Endpoint)>,
    outputs: HashMap<NodeId, (usize, &'a Endpoint)>,
    oscs: HashMap<NodeId, usize>,
    smoothers: HashMap<EdgeKey, usize>,
    fades: HashMap<EdgeKey, usize>,
    rms: HashMap<(MeterId, NodeId), usize>,
    clips: HashMap<NodeId, (usize, Option<EndpointKey>)>,
}

impl<'a> Inherit<'a> {
    fn new(prev: &'a Layout) -> Self {
        let slots = |v: &'a [Endpoint]| v.iter().enumerate().map(|(i, e)| (e.node, (i, e))).collect();
        Self {
            kind: prev.nodes.iter().map(|n| (n.id, &n.kind)).collect(),
            inputs: slots(&prev.inputs),
            outputs: slots(&prev.outputs),
            oscs: prev.oscs.iter().enumerate().map(|(i, n)| (*n, i)).collect(),
            smoothers: prev.smoothers.iter().enumerate().map(|(i, k)| (*k, i)).collect(),
            fades: prev.fades.iter().enumerate().map(|(i, k)| (*k, i)).collect(),
            rms: prev.rms.iter().enumerate().map(|(i, k)| (*k, i)).collect(),
            clips: prev.clips.iter().enumerate().map(|(i, (n, k))| (*n, (i, *k))).collect(),
        }
    }

    /// Same node, same kind: its state carries over.
    fn unchanged(&self, node: &NodeSpec) -> bool {
        self.kind.get(&node.id).is_some_and(|k| same_identity(k, &node.kind))
    }
}

#[allow(clippy::too_many_arguments)]
fn emit(
    g: &Graph,
    order: &[usize],
    live: &[bool],
    block: usize,
    tables: &Tables,
    endpoints: &mut impl Endpoints,
    prev: &Layout,
    wait_for_fades: bool,
    opened: &mut Vec<EndpointKey>,
) -> Result<Compiled, GraphError> {
    let prev = Inherit::new(prev);
    let mut b = PlanBuilder::with_tables(block, tables.params.clone(), tables.meters.clone());
    let mut bufs = Buffers {
        pending_reads: Vec::new(),
        free: Vec::new(),
    };
    let mut value: Vec<Option<BufId>> = vec![None; g.nodes.len()];
    let mut layout = Layout::default();

    for &i in order.iter().filter(|&&i| live[i]) {
        let node = &g.nodes[i];
        let same = prev.unchanged(node);
        let endpoint_err = |reason| GraphError::Endpoint { node: node.id, reason };
        // A live node's buffer is read once per live outgoing edge, once per meter, and once
        // more by a sink's own op.
        let reads =
            g.outputs[i].iter().filter(|&&t| live[t]).count() + g.meters[i].len() + node.kind.is_sink() as usize;

        let buf = match &node.kind {
            NodeKind::DeviceInput { .. } | NodeKind::VirtualSource { .. } => {
                let ring = match prev.inputs.get(&node.id).filter(|_| same) {
                    Some(&(slot, ep)) => {
                        layout.inputs.push(ep.clone());
                        b.inherit_input(slot)
                    }
                    None => {
                        let opened_ring = match &node.kind {
                            NodeKind::DeviceInput { device } => endpoints.input(node.id, device),
                            NodeKind::VirtualSource { endpoint } => endpoints.virtual_input(node.id, *endpoint),
                            _ => unreachable!("matched above"),
                        };
                        let (reader, key) = opened_ring.map_err(endpoint_err)?;
                        opened.push(key);
                        let (ring, stats) = b.input(reader);
                        layout.inputs.push(Endpoint {
                            node: node.id,
                            key,
                            stats,
                        });
                        ring
                    }
                };
                let dst = bufs.alloc(&mut b);
                b.op(Op::Pull { ring, dst });
                dst
            }
            NodeKind::Clip {
                path,
                polyphony,
                retrigger,
                looped,
            } => {
                let clip_err = |reason| GraphError::Clip { node: node.id, reason };
                let data = tables.clips.load(path).map_err(clip_err)?;
                let settings = ClipSettings {
                    polyphony: *polyphony,
                    retrigger: *retrigger,
                    looped: *looped,
                };
                let slot = match prev.clips.get(&node.id).filter(|_| same) {
                    Some(&(slot, key)) => {
                        layout.clips.push((node.id, key));
                        b.clip(node.id, data, settings, None, Some(slot))
                    }
                    None => {
                        let (stream, key) = match data.streamed {
                            false => (None, None),
                            true => {
                                let (ends, key) = tables.clips.open_stream(path).map_err(clip_err)?;
                                opened.push(key);
                                (Some(ends), Some(key))
                            }
                        };
                        layout.clips.push((node.id, key));
                        b.clip(node.id, data, settings, stream, None)
                    }
                };
                let dst = bufs.alloc(&mut b);
                b.op(Op::Clip { slot, dst });
                dst
            }
            NodeKind::Tone { freq, gain } => {
                let osc = match prev.oscs.get(&node.id).filter(|_| same) {
                    Some(&slot) => b.inherit_oscillator(slot),
                    None => b.oscillator(),
                };
                layout.oscs.push(node.id);
                let dst = bufs.alloc(&mut b);
                b.op(Op::Sine {
                    osc,
                    dst,
                    freq: *freq,
                    gain: *gain,
                });
                dst
            }
            NodeKind::Bus | NodeKind::DeviceOutput { .. } | NodeKind::VirtualMic { .. } => match g.inputs[i].as_slice()
            {
                // Forward: same buffer, no ops. Its reads are added before ours is consumed,
                // so it can't be freed in between.
                &[
                    InEdge {
                        src,
                        gain: Gain::Unity,
                        status: EdgeStatus::Steady,
                    },
                ] => {
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
                    for e in inputs {
                        let src_buf = value[e.src].expect("inputs come first in topological order");
                        let key = (g.nodes[e.src].id, node.id, param_of(e.gain));
                        let gain = match e.gain {
                            Gain::Unity => plan::Gain::Fixed(1.0),
                            Gain::Param(p) => {
                                layout.smoothers.push(key);
                                match prev.smoothers.get(&key) {
                                    Some(&slot) => b.inherit_param_gain(p, slot),
                                    None => b.param_gain(p),
                                }
                            }
                        };
                        let fade = match e.status {
                            EdgeStatus::Steady => None,
                            status => {
                                let (start, target) = match status {
                                    EdgeStatus::FadingOut => (1.0, 0.0),
                                    _ => (0.0, 1.0),
                                };
                                layout.fades.push(key);
                                Some(match prev.fades.get(&key) {
                                    Some(&slot) => b.inherit_fade(slot, target),
                                    None => b.fade(start, target),
                                })
                            }
                        };
                        layout.edges.push((
                            EdgeSpec {
                                from: key.0,
                                to: key.1,
                                gain: key.2,
                            },
                            e.status,
                        ));
                        b.op(Op::MixAdd {
                            src: src_buf,
                            dst,
                            gain,
                            fade,
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
        } else if let [e] = g.inputs[i].as_slice() {
            // Forwarded edge: still part of the layout.
            let key = (g.nodes[e.src].id, node.id);
            layout.edges.push((
                EdgeSpec {
                    from: key.0,
                    to: key.1,
                    gain: None,
                },
                e.status,
            ));
        }
        for &meter in &g.meters[i] {
            layout.rms.push((meter, node.id));
            b.meter(buf, meter, prev.rms.get(&(meter, node.id)).copied());
            bufs.read(buf);
        }

        if node.kind.is_sink() {
            if let (Some(m), NodeKind::DeviceOutput { device }) = (g.master, &node.kind)
                && m == i
            {
                // `Master` only names the bus; the clock reads it after the whole block, so its
                // read stays pending and the buffer is never handed to a later node.
                b.op(Op::Master { src: buf });
                layout.master_device = Some(device.clone());
            } else {
                let ring = match prev.outputs.get(&node.id).filter(|_| same) {
                    Some(&(slot, ep)) => {
                        layout.outputs.push(ep.clone());
                        b.inherit_output(slot)
                    }
                    None => {
                        let opened_ring = match &node.kind {
                            NodeKind::DeviceOutput { device } => endpoints.output(node.id, device),
                            NodeKind::VirtualMic { endpoint } => endpoints.virtual_output(node.id, *endpoint),
                            _ => unreachable!("only sinks get here"),
                        };
                        let (writer, key) = opened_ring.map_err(endpoint_err)?;
                        opened.push(key);
                        let (ring, stats) = b.output(writer);
                        layout.outputs.push(Endpoint {
                            node: node.id,
                            key,
                            stats,
                        });
                        ring
                    }
                };
                b.op(Op::Push { src: buf, ring });
                bufs.read(buf);
            }
        }
        layout.nodes.push(node.clone());
        layout.ghosts += g.ghost[i] as usize;
    }

    b.master_device(layout.master_device.clone())
        .wait_for_fades(wait_for_fades);
    Ok(Compiled {
        plan: b.build(),
        layout,
    })
}

/// Release an opened key to whoever owns it: the clip store or the device endpoints.
pub(crate) fn release(key: EndpointKey, tables: &Tables, endpoints: &mut impl Endpoints) {
    if key & CLIP_KEY_BIT != 0 {
        tables.clips.release(key);
    } else {
        endpoints.release(key);
    }
}

fn param_of(gain: Gain) -> Option<ParamId> {
    match gain {
        Gain::Unity => None,
        Gain::Param(p) => Some(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::spec::MeterSpec;
    use crate::rt::offline;
    use crate::rt::params::RAMP_FRAMES;
    use crate::testing::*;

    fn order(c: &Compiled) -> Vec<NodeId> {
        c.layout.nodes().iter().map(|n| n.id).collect()
    }

    fn compile_ok(spec: &GraphSpec) -> (Compiled, FakeEndpoints) {
        let mut ep = FakeEndpoints::default();
        let c = compile(spec, 64, &Tables::new(256, 16), &mut ep).unwrap_or_else(|e| panic!("compile failed: {e}"));
        (c, ep)
    }

    fn compile_err(spec: &GraphSpec) -> GraphError {
        match compile(spec, 64, &Tables::new(256, 16), &mut FakeEndpoints::default()) {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        }
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
        assert_eq!(c.plan.master_device(), Some("spk"));
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
            meters: Vec::new(),
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
            meters: Vec::new(),
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
        assert_eq!(c.plan.master_device(), None);
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
        assert_eq!(order(&c), vec![NodeId(3), NodeId(4)]);
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
            meters: Vec::new(),
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
        let a = order(&compile_ok(&spec).0);
        assert_eq!(a, vec![NodeId(2), NodeId(1), NodeId(5), NodeId(9)]);
        assert_eq!(order(&compile_ok(&spec).0), a);
    }

    // ---- params and meters (E4) ----

    fn compile_with(spec: &GraphSpec, tables: &Tables) -> (Compiled, FakeEndpoints) {
        let mut ep = FakeEndpoints::default();
        let c = compile(spec, 64, tables, &mut ep).unwrap_or_else(|e| panic!("compile failed: {e}"));
        (c, ep)
    }

    /// DC input (1.0) → param gain → master: the output *is* the gain, sample by sample.
    fn dc_through_param(initial: f32) -> (Compiled, FakeEndpoints, Tables) {
        let spec = GraphSpec {
            nodes: vec![input(1, "mic"), output(2, "spk")],
            edges: vec![gain_edge(1, 2, 3)],
            params: vec![param(3, initial)],
            master: Some(NodeId(2)),
            ..Default::default()
        };
        let tables = Tables::new(8, 8);
        let (c, mut ep) = compile_with(&spec, &tables);
        ep.feed(1, &vec![[1.0, 1.0]; 4000]);
        (c, ep, tables)
    }

    #[test]
    fn new_plan_starts_at_the_param_value_without_fading_in() {
        let (mut c, _, _) = dc_through_param(0.5);
        let out = offline::render(&mut c.plan, 1);
        assert!(out.iter().all(|f| *f == [0.5, 0.5]), "first samples: {:?}", &out[..4]);
    }

    #[test]
    fn param_changes_ramp_without_steps_and_land_exactly() {
        let (mut c, _, tables) = dc_through_param(1.0);
        offline::render(&mut c.plan, 1);
        tables.params.set(ParamId(3), 0.0); // fader down, no recompile
        let ramp_blocks = (RAMP_FRAMES as usize).div_ceil(64) + 1;
        let out: Vec<f32> = offline::render(&mut c.plan, ramp_blocks).iter().map(|f| f[0]).collect();
        let max_step = 1.0 / RAMP_FRAMES as f32 + 1e-5;
        let mut prev = 1.0;
        for (i, &v) in out.iter().enumerate() {
            assert!((prev - v).abs() <= max_step, "step at {i}: {prev} → {v}");
            prev = v;
        }
        assert_eq!(out[RAMP_FRAMES as usize - 1], 0.0, "reaches the target after the ramp");
        assert!(out[RAMP_FRAMES as usize..].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn meters_read_levels_and_taps_cost_no_buffers() {
        let spec = GraphSpec {
            nodes: vec![tone(1, 1000.0, 0.5), tone(2, 1000.0, 0.25), bus(3), output(4, "spk")],
            edges: vec![edge(1, 3), edge(2, 3), edge(3, 4)],
            meters: vec![
                MeterSpec {
                    id: MeterId(0),
                    node: NodeId(1),
                },
                MeterSpec {
                    id: MeterId(5),
                    node: NodeId(4),
                },
            ],
            master: Some(NodeId(4)),
            ..Default::default()
        };
        let tables = Tables::new(8, 8);
        let (mut c, _) = compile_with(&spec, &tables);
        assert_eq!(
            c.plan.ops().iter().filter(|op| matches!(op, Op::Meter { .. })).count(),
            2
        );
        offline::render(&mut c.plan, 2000); // ~2.7 s: RMS settles
        let tone = tables.meters.read(MeterId(0)).unwrap();
        let sum = tables.meters.read(MeterId(5)).unwrap();
        assert!((tone.peak[0] - 0.5).abs() < 0.01, "{tone:?}");
        assert!(
            (sum.peak[0] - 0.75).abs() < 0.01,
            "sink meter reads the mixed input: {sum:?}"
        );
        assert!((sum.rms[1] - 0.75 / 2f32.sqrt()).abs() < 0.01, "{sum:?}");
    }

    #[test]
    fn a_meter_keeps_an_unrouted_input_alive() {
        let spec = GraphSpec {
            nodes: vec![input(1, "mic"), output(2, "spk")],
            meters: vec![MeterSpec {
                id: MeterId(1),
                node: NodeId(1),
            }],
            master: Some(NodeId(2)),
            ..Default::default()
        };
        let tables = Tables::new(8, 8);
        let (mut c, mut ep) = compile_with(&spec, &tables);
        assert_eq!(ep.opened, vec!["mic".to_string()]);
        ep.feed(1, &vec![[0.3, -0.6]; 64]);
        let master = offline::render(&mut c.plan, 1);
        assert!(master.iter().all(|f| *f == [0.0; 2]), "metered, not routed");
        assert_eq!(tables.meters.read(MeterId(1)).unwrap().peak, [0.3, 0.6]);
    }

    #[test]
    fn rejects_bad_meters_and_out_of_range_ids() {
        let base = || GraphSpec {
            nodes: vec![tone(1, 440.0, 1.0), output(2, "spk")],
            edges: vec![edge(1, 2)],
            master: Some(NodeId(2)),
            ..Default::default()
        };
        let m = |id, node| MeterSpec {
            id: MeterId(id),
            node: NodeId(node),
        };
        let tables = Tables::new(4, 4);
        let err = |s: GraphSpec| compile(&s, 64, &tables, &mut FakeEndpoints::default()).err().unwrap();
        assert_eq!(
            err(GraphSpec {
                meters: vec![m(1, 1), m(1, 2)],
                ..base()
            }),
            GraphError::DuplicateMeter(MeterId(1))
        );
        assert_eq!(
            err(GraphSpec {
                meters: vec![m(1, 9)],
                ..base()
            }),
            GraphError::UnknownMeterNode {
                meter: MeterId(1),
                node: NodeId(9)
            }
        );
        assert_eq!(
            err(GraphSpec {
                meters: vec![m(4, 1)],
                ..base()
            }),
            GraphError::IdOutOfRange {
                kind: IdKind::Meter,
                id: 4,
                capacity: 4
            }
        );
        assert_eq!(
            err(GraphSpec {
                params: vec![param(7, 1.0)],
                ..base()
            }),
            GraphError::IdOutOfRange {
                kind: IdKind::Param,
                id: 7,
                capacity: 4
            }
        );
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
        match compile(&spec, 64, &Tables::new(256, 16), &mut ep) {
            Err(GraphError::Endpoint { node, reason }) => {
                assert_eq!(node, NodeId(1));
                assert!(reason.contains("not found"));
            }
            other => panic!("expected Endpoint error, got {:?}", other.err()),
        }
    }

    // ---- randomized: compiled output == direct evaluation ----

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
