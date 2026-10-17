//! The controller's side of the engine pipe: handshake, requests, and the event stream.
//! Transport-agnostic (any `Read` + `Write` pair); `pipe::connect` gives the Windows one.

use sb_shared::events::EngineEvent;
use sb_shared::graph::{ClipAction, GraphError, GraphSpec, NodeId, ParamId};
use sb_shared::protocol::{Hello, HostMessage, PROTOCOL_VERSION, Request, RequestBody, read_frame, write_frame};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::Duration;

/// How long a request waits for its reply.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

pub struct EngineClient<W: Write> {
    writer: W,
    incoming: mpsc::Receiver<HostMessage>,
    /// Events that arrived while waiting for a reply.
    backlog: VecDeque<EngineEvent>,
    next_id: u64,
    /// What the host said about itself.
    pub host: String,
}

impl<W: Write> EngineClient<W> {
    /// Say hello and start reading. Fails if the host refuses (mismatched builds) or hangs up.
    pub fn connect(mut reader: impl Read + Send + 'static, mut writer: W, client: &str) -> Result<Self, String> {
        let hello = Hello {
            protocol: PROTOCOL_VERSION,
            client: client.into(),
        };
        write_frame(&mut writer, &hello).map_err(|e| format!("send hello: {e}"))?;
        let host = match read_frame::<HostMessage>(&mut reader) {
            Ok(Some(HostMessage::Welcome { host, .. })) => host,
            Ok(Some(HostMessage::Refused { reason })) => return Err(format!("host refused: {reason}")),
            Ok(Some(other)) => return Err(format!("unexpected first message: {other:?}")),
            Ok(None) => return Err("host hung up during the handshake".into()),
            Err(e) => return Err(format!("handshake: {e}")),
        };
        let (tx, incoming) = mpsc::channel();
        std::thread::Builder::new()
            .name("sb-client-reader".into())
            .spawn(move || {
                while let Ok(Some(msg)) = read_frame::<HostMessage>(&mut reader) {
                    if tx.send(msg).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            writer,
            incoming,
            backlog: VecDeque::new(),
            next_id: 1,
            host,
        })
    }

    fn send(&mut self, body: RequestBody, wants_reply: bool) -> Result<u64, String> {
        let id = if wants_reply {
            self.next_id += 1;
            self.next_id
        } else {
            0
        };
        write_frame(&mut self.writer, &Request { id, body }).map_err(|e| format!("host gone: {e}"))?;
        Ok(id)
    }

    fn wait_reply(&mut self, id: u64) -> Result<Result<(), GraphError>, String> {
        loop {
            match self.incoming.recv_timeout(REPLY_TIMEOUT) {
                Ok(HostMessage::Reply { id: got, result }) if got == id => return Ok(result),
                Ok(HostMessage::Event(e)) => self.backlog.push_back(e),
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => return Err("no reply from the host".into()),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err("host gone".into()),
            }
        }
    }

    /// Replace the graph. `Ok(Err(..))`: the host is fine but refused this graph.
    pub fn apply_graph(&mut self, graph: GraphSpec, version: u64) -> Result<Result<(), GraphError>, String> {
        let id = self.send(RequestBody::ApplyGraph { graph, version }, true)?;
        self.wait_reply(id)
    }

    pub fn set_param(&mut self, id: ParamId, value: f32) -> Result<(), String> {
        self.send(RequestBody::SetParam { id, value }, false).map(drop)
    }

    pub fn trigger(&mut self, node: NodeId, action: ClipAction) -> Result<(), String> {
        self.send(RequestBody::Trigger { node, action }, false).map(drop)
    }

    pub fn ping(&mut self) -> Result<Result<(), GraphError>, String> {
        let id = self.send(RequestBody::Ping, true)?;
        self.wait_reply(id)
    }

    pub fn shutdown(&mut self) -> Result<Result<(), GraphError>, String> {
        let id = self.send(RequestBody::Shutdown, true)?;
        self.wait_reply(id)
    }

    /// The next event, waiting up to `timeout`. `None` on timeout or when the host is gone
    /// (see [`EngineClient::is_connected`]).
    pub fn next_event(&mut self, timeout: Duration) -> Option<EngineEvent> {
        if let Some(e) = self.backlog.pop_front() {
            return Some(e);
        }
        loop {
            match self.incoming.recv_timeout(timeout) {
                Ok(HostMessage::Event(e)) => return Some(e),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }

    /// Whether the host's side is still there (checked by trying to read).
    pub fn is_connected(&mut self) -> bool {
        match self.incoming.try_recv() {
            Ok(HostMessage::Event(e)) => {
                self.backlog.push_back(e);
                true
            }
            Ok(_) => true,
            Err(mpsc::TryRecvError::Empty) => true,
            Err(mpsc::TryRecvError::Disconnected) => false,
        }
    }
}
