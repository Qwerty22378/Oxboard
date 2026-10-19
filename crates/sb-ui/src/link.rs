//! The UI's end of the controller link, on a thread of its own. It connects (and reconnects
//! whenever the controller goes away), says hello, subscribes, and hands every message to the
//! UI thread as a [`LinkEvent`], waking it. Sending is a frame write on the caller's thread.
//!
//! While the overlay is hidden ([`Link::set_visible`]) meters are dropped and nothing wakes
//! the UI thread: state messages wait in the channel until it's shown again. A hidden overlay
//! costs no CPU.

use sb_controller::transport;
use sb_shared::protocol::{read_frame, write_frame};
use sb_shared::ui::{UI_PROTOCOL_VERSION, UiHello, UiMessage, UiRequest};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum LinkEvent {
    /// Connected and subscribed; a snapshot follows.
    Connected {
        controller: String,
    },
    Message(UiMessage),
    /// Not connected (any more), and why; the link keeps trying.
    Lost(String),
}

/// The first wait before connecting again; it doubles up to [`RETRY_MAX`].
pub const RETRY_FIRST: Duration = Duration::from_millis(250);
pub const RETRY_MAX: Duration = Duration::from_secs(2);

type Writer = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

#[derive(Clone)]
pub struct Link {
    writer: Writer,
    visible: Arc<AtomicBool>,
}

impl Link {
    /// Connect to the controller at `address` as `client`, for as long as `events` is read.
    /// `wake` is called (on the link's thread) after each event the UI should see now.
    pub fn spawn(address: String, client: String, events: Sender<LinkEvent>, wake: impl Fn() + Send + 'static) -> Self {
        let link = Self {
            writer: Arc::new(Mutex::new(None)),
            visible: Arc::new(AtomicBool::new(true)),
        };
        let (writer, visible) = (link.writer.clone(), link.visible.clone());
        std::thread::Builder::new()
            .name("sb-ui-link".into())
            .spawn(move || run(&address, &client, &events, &wake, &writer, &visible))
            .expect("spawn the link thread");
        link
    }

    /// Send a request; fails at once when there's no connection.
    pub fn send(&self, req: &UiRequest) -> Result<(), String> {
        let mut w = self.writer.lock().unwrap();
        let Some(writer) = w.as_mut() else {
            return Err("not connected to the controller".into());
        };
        write_frame(writer, req).map_err(|e| {
            // The reader sees the end too and reports it.
            *w = None;
            format!("controller gone: {e}")
        })
    }

    /// Whether the overlay shows: hidden, meters are dropped and the UI isn't woken.
    pub fn set_visible(&self, visible: bool) {
        self.visible.store(visible, Ordering::Relaxed);
    }
}

fn run(
    address: &str,
    client: &str,
    events: &Sender<LinkEvent>,
    wake: &dyn Fn(),
    writer: &Writer,
    visible: &AtomicBool,
) {
    let mut retry = RETRY_FIRST;
    // Said once per outage, not at every attempt.
    let mut said: Option<String> = None;
    let lost = |why: String, said: &mut Option<String>| -> bool {
        if said.as_ref() != Some(&why) {
            *said = Some(why.clone());
            if events.send(LinkEvent::Lost(why)).is_err() {
                return false;
            }
            wake();
        }
        true
    };
    loop {
        let conn = match transport::connect(address, Duration::from_millis(200)) {
            Ok(c) => c,
            Err(e) => {
                if !lost(format!("can't reach the controller ({e})"), &mut said) {
                    return;
                }
                std::thread::sleep(retry);
                retry = (retry * 2).min(RETRY_MAX);
                continue;
            }
        };
        let (mut reader, mut w) = (conn.reader, conn.writer);
        let hello = UiHello {
            protocol: UI_PROTOCOL_VERSION,
            client: client.into(),
        };
        let welcome = write_frame(&mut w, &hello)
            .map_err(|e| e.to_string())
            .and_then(|()| read_frame::<UiMessage>(&mut reader).map_err(|e| e.to_string()));
        let controller = match welcome {
            Ok(Some(UiMessage::Welcome { controller, .. })) => controller,
            Ok(Some(UiMessage::Refused { reason })) => {
                // A different build: trying again soon won't help.
                if !lost(format!("the controller refused this UI: {reason}"), &mut said) {
                    return;
                }
                std::thread::sleep(RETRY_MAX * 5);
                continue;
            }
            Ok(other) => {
                let why = format!("bad handshake: {other:?}");
                if !lost(why, &mut said) {
                    return;
                }
                std::thread::sleep(retry);
                continue;
            }
            Err(e) => {
                if !lost(format!("handshake: {e}"), &mut said) {
                    return;
                }
                std::thread::sleep(retry);
                continue;
            }
        };
        if write_frame(&mut w, &UiRequest::Subscribe).is_err() {
            continue;
        }
        *writer.lock().unwrap() = Some(w);
        (retry, said) = (RETRY_FIRST, None);
        if events.send(LinkEvent::Connected { controller }).is_err() {
            return;
        }
        wake();
        let why = loop {
            match read_frame::<UiMessage>(&mut reader) {
                Ok(Some(UiMessage::Meters(_))) if !visible.load(Ordering::Relaxed) => {}
                Ok(Some(msg)) => {
                    if events.send(LinkEvent::Message(msg)).is_err() {
                        return;
                    }
                    if visible.load(Ordering::Relaxed) {
                        wake();
                    }
                }
                Ok(None) => break "the controller closed the link".to_string(),
                Err(e) => break format!("the link broke: {e}"),
            }
        };
        *writer.lock().unwrap() = None;
        if !lost(why, &mut said) {
            return;
        }
        std::thread::sleep(retry);
    }
}
