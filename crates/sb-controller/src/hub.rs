//! The connected UIs, and the one way messages reach them.
//!
//! Each UI has a bounded outgoing queue drained by its own writer thread, so a slow or stuck
//! UI never blocks the actor. A UI whose queue fills up with state messages is dropped: it
//! has missed state, and reconnecting gets it a fresh snapshot. Meters are only skipped when
//! the queue is full (they're replaced 30 times a second anyway).
//!
//! The actor sends replies, snapshots and diffs; the engine link (C3) sends meters without
//! going through the actor. The lock only guards the client table, never `AppState`.

use sb_shared::events::MeterLevels;
use sb_shared::ui::{MeterTarget, UiMessage};
use std::collections::HashMap;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

/// A connected UI, numbered by the server in connection order.
pub type ClientId = u64;

/// Outgoing messages queued per UI before it's considered stuck.
pub const QUEUE: usize = 1024;

/// Ends a UI's connection (both directions), from any thread.
pub type Closer = Arc<dyn Fn() + Send + Sync>;

struct Client {
    tx: SyncSender<UiMessage>,
    close: Closer,
    subscribed: bool,
}

#[derive(Default)]
pub struct Hub {
    clients: Mutex<HashMap<ClientId, Client>>,
}

impl Hub {
    pub fn add(&self, id: ClientId, tx: SyncSender<UiMessage>, close: Closer) {
        self.lock().insert(
            id,
            Client {
                tx,
                close,
                subscribed: false,
            },
        );
    }

    /// Forget a UI and close its connection. Returns whether it was still there.
    pub fn remove(&self, id: ClientId) -> bool {
        let client = self.lock().remove(&id);
        match client {
            Some(c) => {
                (c.close)();
                true
            }
            None => false,
        }
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Send diffs and meters to `id` from now on.
    pub fn subscribe(&self, id: ClientId) {
        if let Some(c) = self.lock().get_mut(&id) {
            c.subscribed = true;
        }
    }

    /// Queue `msg` for one UI; a UI that can't keep up is dropped.
    pub fn send(&self, id: ClientId, msg: UiMessage) {
        let mut clients = self.lock();
        let stuck = match clients.get(&id) {
            Some(c) => c.tx.try_send(msg).is_err(),
            None => false,
        };
        if stuck && let Some(c) = clients.remove(&id) {
            (c.close)();
        }
    }

    /// Queue `msg` for every subscribed UI.
    pub fn broadcast(&self, msg: &UiMessage) {
        let mut clients = self.lock();
        let stuck: Vec<ClientId> = clients
            .iter()
            .filter(|(_, c)| c.subscribed && c.tx.try_send(msg.clone()).is_err())
            .map(|(id, _)| *id)
            .collect();
        for id in stuck {
            if let Some(c) = clients.remove(&id) {
                (c.close)();
            }
        }
    }

    /// Meters for every subscribed UI, skipped for any that's behind.
    pub fn meters(&self, levels: &[(MeterTarget, MeterLevels)]) {
        let mut clients = self.lock();
        let mut gone = Vec::new();
        for (id, c) in clients.iter().filter(|(_, c)| c.subscribed) {
            if let Err(TrySendError::Disconnected(_)) = c.tx.try_send(UiMessage::Meters(levels.to_vec())) {
                gone.push(*id);
            }
        }
        for id in gone {
            clients.remove(&id);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ClientId, Client>> {
        // A panic while holding the lock leaves the table consistent (every operation is one
        // insert or remove), so a poisoned lock is still usable.
        self.clients.lock().unwrap_or_else(|e| e.into_inner())
    }
}
