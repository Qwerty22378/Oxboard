//! `sb-controller`: the soundboard's brain. It owns all authoritative state, turns it into
//! engine graphs, and orchestrates every command from the UI. Current state: C1, the actor
//! skeleton (state, commands, UI server with snapshot + diffs) and C2, state → graph; the
//! engine link that sends the graph is C3.
//!
//! - [`state`]: `AppState` = what the user wants (`Desired`) + what's true now (`Observed`).
//! - [`actor`]: the single owner of that state; everything reaches it as a message.
//! - [`commands`]: UI commands applied to state, validated first.
//! - [`compile`]: state → `GraphSpec` (+ what changed since the last one); [`loops`]: the
//!   feedback loop check.
//! - [`view`]: state → the `StateView` the UI is shown; the actor diffs consecutive views.
//! - [`hub`], [`ui_server`], [`transport`]: connected UIs, their threads, and where they
//!   connect.
//! - [`client`]: a UI's end of the link.

pub mod actor;
pub mod client;
pub mod commands;
pub mod compile;
pub mod hub;
pub mod loops;
pub mod state;
pub mod transport;
pub mod ui_server;
pub mod view;

use actor::{Actor, Inbox, Msg};
use hub::Hub;
use state::AppState;
use std::sync::Arc;
use std::sync::mpsc;

/// A running controller: its actor, and UIs being served from `listener`.
pub struct Controller {
    pub inbox: Inbox,
    pub hub: Arc<Hub>,
    actor: std::thread::JoinHandle<AppState>,
}

impl Controller {
    pub fn start(listener: impl ui_server::Listener, state: AppState, log: bool) -> Self {
        let hub = Arc::new(Hub::default());
        let (inbox, rx) = mpsc::channel::<Msg>();
        let actor = Actor::new(state, hub.clone()).with_log(log);
        let actor = std::thread::Builder::new()
            .name("sb-actor".into())
            .spawn(move || actor.run(rx))
            .expect("spawn the actor");
        ui_server::spawn(listener, hub.clone(), inbox.clone());
        Self { inbox, hub, actor }
    }

    /// Wait for the actor to stop (something sent [`Msg::Stop`]).
    pub fn join(self) -> AppState {
        self.actor.join().expect("the actor panicked")
    }

    /// Stop the actor and return its final state. (The UI listener thread stays blocked in
    /// `accept` until the process exits.)
    pub fn stop(self) -> AppState {
        let _ = self.inbox.send(Msg::Stop);
        self.actor.join().expect("the actor panicked")
    }
}
