//! A UI's side of the controller link: handshake, commands, and a [`Mirror`] of the view kept
//! in sync from snapshots and diffs (resubscribing by itself after a gap). The overlay (U2)
//! and the `ctl` example use it.

use crate::hub::Closer;
use crate::ui_server::Connection;
use sb_shared::protocol::{read_frame, write_frame};
use sb_shared::ui::{
    Command, ErrorInfo, Mirror, RequestId, Synced, UI_PROTOCOL_VERSION, UiHello, UiMessage, UiRequest,
};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub struct UiClient<W: Write> {
    writer: W,
    /// Ends the connection when the client is dropped (its reader thread holds the other half).
    close: Option<Closer>,
    incoming: mpsc::Receiver<UiMessage>,
    /// Messages that arrived while [`UiClient::call`] waited for its reply.
    backlog: VecDeque<UiMessage>,
    next_id: RequestId,
    pub mirror: Mirror,
    /// What the controller said about itself.
    pub controller: String,
}

impl UiClient<Box<dyn Write + Send>> {
    /// Connect over a transport connection (`transport::connect`); dropping the client closes it.
    pub fn over(conn: Connection, client: &str) -> Result<Self, String> {
        let mut c = Self::connect(conn.reader, conn.writer, client)?;
        c.close = Some(conn.close);
        Ok(c)
    }
}

impl<W: Write> Drop for UiClient<W> {
    fn drop(&mut self) {
        if let Some(close) = self.close.take() {
            close();
        }
    }
}

impl<W: Write> UiClient<W> {
    /// Say hello and start reading. Fails if the controller refuses or hangs up.
    pub fn connect(mut reader: impl Read + Send + 'static, mut writer: W, client: &str) -> Result<Self, String> {
        let hello = UiHello {
            protocol: UI_PROTOCOL_VERSION,
            client: client.into(),
        };
        write_frame(&mut writer, &hello).map_err(|e| format!("send hello: {e}"))?;
        let controller = match read_frame::<UiMessage>(&mut reader) {
            Ok(Some(UiMessage::Welcome { controller, .. })) => controller,
            Ok(Some(UiMessage::Refused { reason })) => return Err(format!("controller refused: {reason}")),
            Ok(Some(other)) => return Err(format!("unexpected first message: {other:?}")),
            Ok(None) => return Err("controller hung up during the handshake".into()),
            Err(e) => return Err(format!("handshake: {e}")),
        };
        let (tx, incoming) = mpsc::channel();
        std::thread::Builder::new()
            .name("sb-ui-client-reader".into())
            .spawn(move || {
                while let Ok(Some(msg)) = read_frame::<UiMessage>(&mut reader) {
                    if tx.send(msg).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            writer,
            close: None,
            incoming,
            backlog: VecDeque::new(),
            next_id: 0,
            mirror: Mirror::default(),
            controller,
        })
    }

    fn request(&mut self, req: &UiRequest) -> Result<(), String> {
        write_frame(&mut self.writer, req).map_err(|e| format!("controller gone: {e}"))
    }

    /// Ask for a snapshot, then diffs and meters.
    pub fn subscribe(&mut self) -> Result<(), String> {
        self.request(&UiRequest::Subscribe)
    }

    /// Send a command without waiting; its replies arrive through [`UiClient::next`].
    pub fn send(&mut self, cmd: Command) -> Result<RequestId, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.request(&UiRequest::Command { id, cmd })?;
        Ok(id)
    }

    /// Run a command and wait for its outcome. `Ok(Err(..))`: the controller refused it.
    pub fn call(&mut self, cmd: Command, timeout: Duration) -> Result<Result<(), ErrorInfo>, String> {
        let id = self.send(cmd)?;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let msg = match self.incoming.recv_timeout(left) {
                Ok(m) => m,
                Err(mpsc::RecvTimeoutError::Timeout) => return Err("no reply from the controller".into()),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err("controller gone".into()),
            };
            self.sync(&msg)?;
            match msg {
                UiMessage::Completed(got) if got == id => return Ok(Ok(())),
                UiMessage::Failed(got, e) if got == id => return Ok(Err(e)),
                UiMessage::Accepted(got) if got == id => {}
                other => self.backlog.push_back(other),
            }
        }
    }

    /// The next message, waiting up to `timeout`; snapshots and diffs are already applied to
    /// [`UiClient::mirror`]. `None` on timeout or when the controller is gone
    /// (see [`UiClient::is_connected`]).
    pub fn next(&mut self, timeout: Duration) -> Option<UiMessage> {
        if let Some(m) = self.backlog.pop_front() {
            return Some(m);
        }
        let msg = self.incoming.recv_timeout(timeout).ok()?;
        self.sync(&msg).ok()?;
        Some(msg)
    }

    fn sync(&mut self, msg: &UiMessage) -> Result<(), String> {
        if self.mirror.apply(msg) == Synced::Gap {
            self.subscribe()?;
        }
        Ok(())
    }

    /// Whether the controller's side is still there.
    pub fn is_connected(&mut self) -> bool {
        match self.incoming.try_recv() {
            Ok(m) => {
                let _ = self.sync(&m);
                self.backlog.push_back(m);
                true
            }
            Err(mpsc::TryRecvError::Empty) => true,
            Err(mpsc::TryRecvError::Disconnected) => false,
        }
    }
}
