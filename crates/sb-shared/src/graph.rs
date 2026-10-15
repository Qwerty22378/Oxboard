//! `GraphSpec`: what the audio graph should be. The controller builds one from its state;
//! the engine validates it and compiles it into a render plan.
//!
//! Every node has one stereo output, except sinks. Buses and sinks are mix points: their
//! input is the sum of their incoming edges, each scaled by its gain.

use std::fmt;

/// Stable node identity. The controller derives it from its own keys, so an unchanged node
/// keeps its id (and later its state) across graph updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u32);

/// A parameter slot (edge gains so far). Values change without a graph rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ParamId(pub u32);

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GraphSpec {
    pub nodes: Vec<NodeSpec>,
    pub edges: Vec<EdgeSpec>,
    /// Every param an edge refers to, with its initial value.
    pub params: Vec<ParamSpec>,
    /// The device output whose clock drives the mix. `None`: a timer drives it.
    pub master: Option<NodeId>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeSpec {
    pub id: NodeId,
    pub kind: NodeKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NodeKind {
    /// Source: a capture endpoint, by WASAPI endpoint ID.
    DeviceInput { device: String },
    /// Source: sine test tone, same on both channels.
    Tone { freq: f32, gain: f32 },
    /// Mix point.
    Bus,
    /// Sink and mix point: a render endpoint, by WASAPI endpoint ID.
    DeviceOutput { device: String },
}

impl NodeKind {
    pub fn is_source(&self) -> bool {
        matches!(self, NodeKind::DeviceInput { .. } | NodeKind::Tone { .. })
    }

    pub fn is_sink(&self) -> bool {
        matches!(self, NodeKind::DeviceOutput { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeSpec {
    pub from: NodeId,
    pub to: NodeId,
    /// `None`: unity gain, fixed (lets the engine forward without copying).
    pub gain: Option<ParamId>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamSpec {
    pub id: ParamId,
    pub value: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GraphError {
    DuplicateNode(NodeId),
    DuplicateParam(ParamId),
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
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use GraphError::*;
        match self {
            DuplicateNode(n) => write!(f, "node {} is defined twice", n.0),
            DuplicateParam(p) => write!(f, "param {} is defined twice", p.0),
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
        }
    }
}

impl std::error::Error for GraphError {}
