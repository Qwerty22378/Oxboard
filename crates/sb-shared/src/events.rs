//! What the engine reports back to the controller. Facts, not decisions: the controller
//! decides what to do about each (see `Controller Notes` → Reconciliation).

use crate::graph::{MeterId, NodeId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EngineEvent {
    /// The mix thread is now running the plan built from graph `version`.
    GraphApplied { version: u64 },
    /// A clip node went quiet: every voice finished (`stopped: false`) or was stopped.
    ClipEnded { node: NodeId, stopped: bool },
    /// A device node's stream died (unplugged, disabled, format changed). The node now plays
    /// or captures silence until the controller sends a graph without it or with a new device.
    DeviceLost { node: NodeId },
    /// The master device failed; a timer clocks the mix until a graph names a new master.
    MasterClockFallback,
    /// Glitches on a node's ring since the last report (rate-limited to ~2 per second).
    Xrun { node: NodeId, kind: XrunKind, count: u32 },
    /// Meter readings, ~30 Hz, for every meter in the running graph.
    Meters(MeterFrame),
    /// Sent every ~500 ms whatever else happens; silence means the host is stuck or gone.
    Heartbeat { uptime_ms: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum XrunKind {
    /// Ran dry: silence was inserted.
    Underrun,
    /// Overflowed: audio was dropped.
    Overrun,
    /// Skipped audio to restore latency (after a stall or a lost period).
    Trimmed,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MeterFrame {
    pub meters: Vec<(MeterId, MeterLevels)>,
}

/// Linear levels; peak is the maximum since the previous frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct MeterLevels {
    pub peak: [f32; 2],
    pub rms: [f32; 2],
}
