//! The engine pipe protocol: controller ⇄ `sb-engine-host`.
//!
//! Every message is one frame: a little-endian `u32` length, then that many bytes of
//! postcard-encoded payload. Frames above [`MAX_FRAME`] are refused, so a corrupt or hostile
//! length can't make either side allocate gigabytes.
//!
//! 1. The controller sends [`Hello`]; the host answers [`HostMessage::Welcome`] or
//!    [`HostMessage::Refused`] (protocol mismatch: mixed builds).
//! 2. Then the controller sends [`Request`]s and the host sends [`HostMessage`]s: replies
//!    to requests that expect one, and [`EngineEvent`]s at any time.

use crate::events::EngineEvent;
use crate::graph::{ClipAction, GraphError, GraphSpec, NodeId, ParamId};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{self, Read, Write};

/// Bumped on any incompatible change to these types.
pub const PROTOCOL_VERSION: u32 = 1;
/// Largest frame accepted: 16 MiB (a big graph is a few KiB).
pub const MAX_FRAME: usize = 16 << 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    /// Who's connecting, for logs.
    pub client: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Echoed in the reply; 0 for requests that get none.
    pub id: u64,
    pub body: RequestBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RequestBody {
    /// Replace the graph. Replied right away (compiled or `GraphError`); `GraphApplied` follows
    /// once the mix runs it.
    ApplyGraph { graph: GraphSpec, version: u64 },
    /// Move a param. No reply (the fast path).
    SetParam { id: ParamId, value: f32 },
    /// A one-shot action on a clip node. No reply.
    Trigger { node: NodeId, action: ClipAction },
    /// Replied with `Ok`: a round-trip check on top of the heartbeat.
    Ping,
    /// Stop the engine and exit. Replied with `Ok` first.
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HostMessage {
    Welcome { protocol: u32, host: String },
    Refused { reason: String },
    Reply { id: u64, result: Result<(), GraphError> },
    Event(EngineEvent),
}

/// Write one frame.
pub fn write_frame<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let body = postcard::to_allocvec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "message too large"));
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    w.write_all(&frame)?;
    w.flush()
}

/// Read one frame. `Ok(None)` on a clean end of stream (the other side closed between frames).
pub fn read_frame<T: DeserializeOwned>(r: &mut impl Read) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes refused"),
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    postcard::from_bytes(&body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{MeterFrame, MeterLevels, XrunKind};
    use crate::graph::*;

    fn every_node_kind() -> GraphSpec {
        let kinds = vec![
            NodeKind::DeviceInput {
                device: "{0.0.1.00000000}.{mic}".into(),
            },
            NodeKind::Tone {
                freq: 440.0,
                gain: 0.25,
            },
            NodeKind::Bus,
            NodeKind::DeviceOutput {
                device: "{0.0.0.00000000}.{spk}".into(),
            },
            NodeKind::VirtualSource { endpoint: 3 },
            NodeKind::VirtualMic { endpoint: 4 },
            NodeKind::Clip {
                path: r"C:\clips\airhorn.ogg".into(),
                polyphony: 4,
                retrigger: Retrigger::Overlap,
                looped: false,
            },
        ];
        GraphSpec {
            nodes: kinds
                .into_iter()
                .enumerate()
                .map(|(i, kind)| NodeSpec {
                    id: NodeId(i as u32),
                    kind,
                })
                .collect(),
            edges: vec![
                EdgeSpec {
                    from: NodeId(0),
                    to: NodeId(2),
                    gain: Some(ParamId(1)),
                },
                EdgeSpec {
                    from: NodeId(2),
                    to: NodeId(3),
                    gain: None,
                },
            ],
            params: vec![ParamSpec {
                id: ParamId(1),
                value: 0.5,
            }],
            meters: vec![MeterSpec {
                id: MeterId(0),
                node: NodeId(2),
            }],
            master: Some(NodeId(3)),
        }
    }

    fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(msg: &T) {
        let mut buf = Vec::new();
        write_frame(&mut buf, msg).unwrap();
        let back: T = read_frame(&mut buf.as_slice()).unwrap().unwrap();
        assert_eq!(&back, msg);
    }

    #[test]
    fn every_message_round_trips() {
        round_trip(&Hello {
            protocol: PROTOCOL_VERSION,
            client: "controller".into(),
        });
        for body in [
            RequestBody::ApplyGraph {
                graph: every_node_kind(),
                version: 17,
            },
            RequestBody::SetParam {
                id: ParamId(3),
                value: -0.75,
            },
            RequestBody::Trigger {
                node: NodeId(6),
                action: ClipAction::Seek { frame: 48_000 },
            },
            RequestBody::Ping,
            RequestBody::Shutdown,
        ] {
            round_trip(&Request { id: 9, body });
        }
        for msg in [
            HostMessage::Welcome {
                protocol: 1,
                host: "sb-engine-host 0.1".into(),
            },
            HostMessage::Refused {
                reason: "protocol 2, expected 1".into(),
            },
            HostMessage::Reply { id: 9, result: Ok(()) },
            HostMessage::Reply {
                id: 10,
                result: Err(GraphError::Cycle(vec![NodeId(1), NodeId(2)])),
            },
            HostMessage::Reply {
                id: 11,
                result: Err(GraphError::IdOutOfRange {
                    kind: IdKind::Meter,
                    id: 9,
                    capacity: 4,
                }),
            },
            HostMessage::Event(EngineEvent::GraphApplied { version: 17 }),
            HostMessage::Event(EngineEvent::Xrun {
                node: NodeId(0),
                kind: XrunKind::Underrun,
                count: 2,
            }),
            HostMessage::Event(EngineEvent::Meters(MeterFrame {
                meters: vec![(
                    MeterId(0),
                    MeterLevels {
                        peak: [0.5, 0.25],
                        rms: [0.1, 0.05],
                    },
                )],
            })),
            HostMessage::Event(EngineEvent::Heartbeat { uptime_ms: 1500 }),
        ] {
            round_trip(&msg);
        }
    }

    #[test]
    fn frames_stream_back_to_back_and_end_cleanly() {
        let mut buf = Vec::new();
        for i in 0..3 {
            write_frame(
                &mut buf,
                &Request {
                    id: i,
                    body: RequestBody::Ping,
                },
            )
            .unwrap();
        }
        let mut r = buf.as_slice();
        for i in 0..3 {
            let req: Request = read_frame(&mut r).unwrap().unwrap();
            assert_eq!(req.id, i);
        }
        assert_eq!(read_frame::<Request>(&mut r).unwrap(), None, "clean end of stream");
    }

    #[test]
    fn hostile_frames_are_errors_not_allocations_or_panics() {
        let huge = ((MAX_FRAME + 1) as u32).to_le_bytes();
        assert!(read_frame::<Request>(&mut huge.as_slice()).is_err(), "length cap");
        let mut truncated = Vec::new();
        write_frame(
            &mut truncated,
            &Request {
                id: 1,
                body: RequestBody::Ping,
            },
        )
        .unwrap();
        truncated.pop();
        assert!(
            read_frame::<Request>(&mut truncated.as_slice()).is_err(),
            "cut mid-frame"
        );
        let garbage = [3u8, 0, 0, 0, 0xff, 0xff, 0xff];
        assert!(
            read_frame::<Request>(&mut garbage.as_slice()).is_err(),
            "undecodable body"
        );
    }
}
