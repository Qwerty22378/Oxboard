//! `GraphSpec`: what the audio graph should be. The controller builds one from its state;
//! the engine validates it and compiles it into a render plan.
//!
//! Every node has one stereo output, except sinks. Buses and sinks are mix points: their
//! input is the sum of their incoming edges, each scaled by its gain.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Stable node identity. The controller derives it from its own keys, so an unchanged node
/// keeps its id (and later its state) across graph updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NodeId(pub u32);

/// A parameter slot (edge gains so far). Values change without a graph rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ParamId(pub u32);

/// A meter slot: peak and RMS of one node's output, read by the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MeterId(pub u32);

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GraphSpec {
    pub nodes: Vec<NodeSpec>,
    pub edges: Vec<EdgeSpec>,
    /// Every param an edge refers to, with its initial value.
    pub params: Vec<ParamSpec>,
    /// Taps on node outputs. A metered node is kept running even if it feeds no sink, so an
    /// input can show a level before it's routed anywhere.
    pub meters: Vec<MeterSpec>,
    /// The device output whose clock drives the mix. `None`: a timer drives it.
    pub master: Option<NodeId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeSpec {
    pub id: NodeId,
    pub kind: NodeKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NodeKind {
    /// Source: a capture endpoint, by WASAPI endpoint ID.
    DeviceInput { device: String },
    /// Source: sine test tone, same on both channels.
    Tone { freq: f32, gain: f32 },
    /// Mix point.
    Bus,
    /// Sink and mix point: a render endpoint, by WASAPI endpoint ID.
    DeviceOutput { device: String },
    /// Source: what apps play into one of the driver's virtual render endpoints, by the
    /// driver's endpoint id (the controller enables and names it).
    VirtualSource { endpoint: u32 },
    /// Sink and mix point: a virtual microphone; apps capture what the engine writes here.
    VirtualMic { endpoint: u32 },
    /// Source: a sound file, played when triggered (`Engine::trigger`). The engine loads and
    /// decodes it itself; changing the settings keeps playing voices, a new path is a new node.
    Clip {
        path: String,
        polyphony: u8,
        retrigger: Retrigger,
        looped: bool,
    },
}

/// What a clip does when it's triggered while already playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Retrigger {
    /// Fade out what's playing and start again from the top.
    Restart,
    /// Start another voice on top (up to `polyphony`; past that the oldest fades out).
    Overlap,
    /// Keep playing; ignore the trigger.
    Ignore,
}

/// One-shot actions on a clip node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipAction {
    Play,
    /// Fade out every voice.
    Stop,
    /// Jump every voice to `frame` (48 kHz frames from the start), with a short crossfade.
    Seek {
        frame: u64,
    },
}

impl NodeKind {
    pub fn is_source(&self) -> bool {
        matches!(
            self,
            NodeKind::DeviceInput { .. }
                | NodeKind::Tone { .. }
                | NodeKind::VirtualSource { .. }
                | NodeKind::Clip { .. }
        )
    }

    pub fn is_sink(&self) -> bool {
        matches!(self, NodeKind::DeviceOutput { .. } | NodeKind::VirtualMic { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EdgeSpec {
    pub from: NodeId,
    pub to: NodeId,
    /// `None`: unity gain, fixed (lets the engine forward without copying).
    pub gain: Option<ParamId>,
}

/// A param an edge refers to. `value` is written to the engine's param table when the graph
/// is applied; afterwards `set_param` changes it without a rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ParamSpec {
    pub id: ParamId,
    pub value: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MeterSpec {
    pub id: MeterId,
    /// For a bus or sink, the meter reads its mixed input.
    pub node: NodeId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GraphError {
    DuplicateNode(NodeId),
    DuplicateParam(ParamId),
    DuplicateMeter(MeterId),
    /// A param or meter id is past the end of the engine's fixed-size table.
    IdOutOfRange {
        kind: IdKind,
        id: u32,
        capacity: usize,
    },
    /// A meter taps a node that doesn't exist.
    UnknownMeterNode {
        meter: MeterId,
        node: NodeId,
    },
    /// Edge `edge` (index into `edges`) names a node that doesn't exist.
    UnknownNode {
        edge: usize,
        node: NodeId,
    },
    UnknownParam {
        edge: usize,
        param: ParamId,
    },
    DuplicateEdge {
        from: NodeId,
        to: NodeId,
    },
    EdgeIntoSource {
        edge: usize,
        node: NodeId,
    },
    EdgeFromSink {
        edge: usize,
        node: NodeId,
    },
    /// The nodes of one cycle, in edge order.
    Cycle(Vec<NodeId>),
    /// `master` is unknown or not a device output.
    BadMaster(NodeId),
    InvalidValue(String),
    /// A device endpoint couldn't be opened.
    Endpoint {
        node: NodeId,
        reason: String,
    },
    /// The engine's queue of pending plans is full (the mix thread isn't adopting them).
    Busy,
    /// A clip couldn't be loaded or decoded.
    Clip {
        node: NodeId,
        reason: String,
    },
}

/// Which fixed-size table an out-of-range id was meant for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdKind {
    Param,
    Meter,
}

impl fmt::Display for IdKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            IdKind::Param => "param",
            IdKind::Meter => "meter",
        })
    }
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use GraphError::*;
        match self {
            DuplicateNode(n) => write!(f, "node {} is defined twice", n.0),
            DuplicateParam(p) => write!(f, "param {} is defined twice", p.0),
            DuplicateMeter(m) => write!(f, "meter {} is defined twice", m.0),
            IdOutOfRange { kind, id, capacity } => write!(f, "{kind} id {id} is out of range (table holds {capacity})"),
            UnknownMeterNode { meter, node } => write!(f, "meter {} taps unknown node {}", meter.0, node.0),
            UnknownNode { edge, node } => write!(f, "edge {edge} refers to unknown node {}", node.0),
            UnknownParam { edge, param } => write!(f, "edge {edge} refers to undeclared param {}", param.0),
            DuplicateEdge { from, to } => write!(f, "more than one edge from node {} to node {}", from.0, to.0),
            EdgeIntoSource { edge, node } => write!(f, "edge {edge} goes into source node {}", node.0),
            EdgeFromSink { edge, node } => write!(f, "edge {edge} comes out of sink node {}", node.0),
            Cycle(nodes) => {
                let path: Vec<String> = nodes.iter().map(|n| n.0.to_string()).collect();
                write!(f, "cycle: {} → {}", path.join(" → "), nodes[0].0)
            }
            BadMaster(n) => write!(f, "master node {} is not a device output", n.0),
            InvalidValue(what) => write!(f, "invalid value: {what}"),
            Endpoint { node, reason } => write!(f, "node {}: {reason}", node.0),
            Busy => write!(f, "engine busy: too many graph updates pending"),
            Clip { node, reason } => write!(f, "clip node {}: {reason}", node.0),
        }
    }
}

impl std::error::Error for GraphError {}
