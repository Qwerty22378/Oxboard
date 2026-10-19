//! The UI server: accepts UIs and connects each one to the actor and the hub.
//!
//! Per UI: the handshake, then a reader thread that posts its requests to the actor, and a
//! writer thread that drains its hub queue onto the connection. Transport-agnostic: anything
//! that yields [`Connection`]s (the Windows pipe, a Unix socket in tests and on Linux).

use crate::actor::{Inbox, Msg};
use crate::hub::{ClientId, Closer, Hub, QUEUE};
use sb_shared::protocol::{read_frame, write_frame};
use sb_shared::ui::{UI_PROTOCOL_VERSION, UiHello, UiMessage, UiRequest};
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

/// One accepted UI.
pub struct Connection {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    /// Ends the connection in both directions (the reader sees end of stream). On Windows it
    /// also discards anything the other side hasn't read yet; to say a last word and hang
    /// up, drop the connection instead.
    pub close: Closer,
}

pub trait Listener: Send + 'static {
    fn accept(&mut self) -> io::Result<Connection>;
}

/// Accept UIs forever on a thread of its own.
pub fn spawn(mut listener: impl Listener, hub: Arc<Hub>, inbox: Inbox) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("sb-ui-listener".into())
        .spawn(move || {
            let mut next: ClientId = 0;
            loop {
                match listener.accept() {
                    Ok(conn) => {
                        next += 1;
                        let (id, hub, inbox) = (next, hub.clone(), inbox.clone());
                        let spawned = std::thread::Builder::new()
                            .name(format!("sb-ui-{id}"))
                            .spawn(move || serve(conn, id, &hub, &inbox));
                        if let Err(e) = spawned {
                            eprintln!("ui server: no thread for client {id}: {e}");
                        }
                    }
                    Err(e) => {
                        eprintln!("ui server: accept: {e}");
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        })
        .expect("spawn the UI listener")
}

/// One UI, from handshake to disconnect. Runs on the connection's own thread (as its reader).
pub fn serve(conn: Connection, id: ClientId, hub: &Hub, inbox: &Inbox) {
    let Connection {
        mut reader,
        mut writer,
        close,
    } = conn;
    // Until the UI is in the hub, failing just drops the connection (see `Connection::close`).
    let hello: UiHello = match read_frame(&mut reader) {
        Ok(Some(h)) => h,
        _ => return,
    };
    if hello.protocol != UI_PROTOCOL_VERSION {
        let reason = format!("protocol {} spoken, {UI_PROTOCOL_VERSION} expected", hello.protocol);
        let _ = write_frame(&mut writer, &UiMessage::Refused { reason });
        return;
    }
    let welcome = UiMessage::Welcome {
        protocol: UI_PROTOCOL_VERSION,
        controller: format!("sb-controller {}", env!("CARGO_PKG_VERSION")),
    };
    if write_frame(&mut writer, &welcome).is_err() {
        return;
    }

    let (tx, rx) = mpsc::sync_channel::<UiMessage>(QUEUE);
    hub.add(id, tx, close.clone());
    let writer_close = close.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("sb-ui-{id}-writer"))
        .spawn(move || {
            // Ends when the hub drops the UI (its sender goes away) or the UI stops reading.
            for msg in rx {
                if write_frame(&mut writer, &msg).is_err() {
                    break;
                }
            }
            writer_close();
        });
    if spawned.is_err() {
        hub.remove(id);
        return;
    }
    if inbox
        .send(Msg::UiConnected {
            client: id,
            name: hello.client,
            pid: hello.pid,
        })
        .is_err()
    {
        hub.remove(id);
        return;
    }
    while let Ok(Some(req)) = read_frame::<UiRequest>(&mut reader) {
        if inbox.send(Msg::Ui { client: id, req }).is_err() {
            break;
        }
    }
    // The actor forgets the UI (which also ends the writer); if the actor is gone, do it here.
    if inbox.send(Msg::UiDisconnected { client: id }).is_err() {
        hub.remove(id);
    }
}
