//! `sb-controller`: the soundboard's brain. It owns all authoritative state, turns it into
//! engine graphs, and orchestrates every command from the UI. Current state: C1, the actor
//! skeleton (state, commands, UI server with snapshot + diffs); C2, state → graph; C3, the
//! engine link that runs it (spawn, apply, params, heartbeat, crash replay); C4, the Windows
//! side (devices, app sessions and their volumes, VB-Cable loops); C5, Interface 2 (the
//! driver's endpoint pool, per-app outputs, multi-step commands that undo themselves); C6,
//! the settings saved and loaded.
//!
//! - [`state`]: `AppState` = what the user wants (`Desired`) + what's true now (`Observed`).
//! - [`actor`]: the single owner of that state; everything reaches it as a message.
//! - [`commands`]: UI commands applied to state, validated first.
//! - [`compile`]: state → `GraphSpec` (+ what changed since the last one); [`loops`]: the
//!   feedback loop check.
//! - [`engine_link`]: keeps an `sb-engine-host` running the current graph.
//! - [`watch`] and `win` (Windows only): what Windows shows, scanned on a COM thread and
//!   reported as observations; app levels set through session volumes.
//! - [`driver`]: the driver's control plane (endpoint pool), on a worker; [`routing`]: apps
//!   sent to outputs of their own through it.
//! - [`persist`]: the settings file: desired state, loaded at startup, saved on change.
//! - [`view`]: state → the `StateView` the UI is shown; the actor diffs consecutive views.
//! - [`hub`], [`ui_server`], [`transport`]: connected UIs, their threads, and where they
//!   connect.
//! - [`client`]: a UI's end of the link.

pub mod actor;
pub mod client;
pub mod commands;
pub mod compile;
pub mod driver;
pub mod engine_link;
pub mod hub;
pub mod loops;
pub mod persist;
pub mod routing;
pub mod state;
pub mod transport;
pub mod ui_server;
pub mod view;
pub mod watch;
#[cfg(windows)]
pub mod win;

use actor::{Actor, Inbox, Msg};
use engine_link::{LinkConfig, LinkMsg, Spawn};
use hub::Hub;
use state::AppState;
use std::sync::Arc;
use std::sync::mpsc;

/// A running controller: its actor, UIs being served from a listener, the engine link and
/// the Windows side.
pub struct Controller {
    pub inbox: Inbox,
    pub hub: Arc<Hub>,
    actor: std::thread::JoinHandle<AppState>,
    link: Option<(mpsc::Sender<LinkMsg>, std::thread::JoinHandle<()>)>,
    windows: Option<std::thread::JoinHandle<()>>,
    driver: Option<std::thread::JoinHandle<()>>,
    persist: Option<std::thread::JoinHandle<()>>,
}

/// Opens the driver's control interface (on the driver worker's thread).
pub type OpenDriver = Box<dyn FnOnce() -> Result<Box<dyn driver::Transport>, String> + Send>;

/// What a controller runs besides the actor and the UI server.
#[derive(Default)]
pub struct Options {
    /// Print every message the actor handles.
    pub log: bool,
    /// Run engines from this spawner.
    pub engine: Option<(Box<dyn Spawn>, LinkConfig)>,
    /// Watch Windows' devices and app sessions, and set app levels and outputs (Windows only).
    pub windows: bool,
    /// Manage the driver's endpoint pool (Interface 2).
    pub driver: Option<OpenDriver>,
    /// Save the settings here whenever they change (load them with [`persist::load`] first).
    pub persist: Option<(std::path::PathBuf, persist::Timing)>,
}

impl Controller {
    /// A controller without an engine (the UI side only).
    pub fn start(listener: impl ui_server::Listener, state: AppState, log: bool) -> Self {
        Self::launch(
            listener,
            state,
            Options {
                log,
                ..Default::default()
            },
        )
    }

    /// A controller that runs engines from `spawner`.
    pub fn with_engine(
        listener: impl ui_server::Listener,
        state: AppState,
        log: bool,
        spawner: impl Spawn,
        cfg: LinkConfig,
    ) -> Self {
        let opts = Options {
            log,
            engine: Some((Box::new(spawner), cfg)),
            ..Default::default()
        };
        Self::launch(listener, state, opts)
    }

    pub fn launch(listener: impl ui_server::Listener, state: AppState, opts: Options) -> Self {
        let hub = Arc::new(Hub::default());
        let (inbox, rx) = mpsc::channel::<Msg>();
        let mut actor = Actor::new(state, hub.clone()).with_log(opts.log);
        let link = opts
            .engine
            .map(|(spawner, cfg)| engine_link::spawn(spawner, cfg, hub.clone(), inbox.clone()));
        if let Some((tx, _)) = &link {
            actor = actor.with_engine(tx.clone());
        }
        #[cfg(windows)]
        let windows = if opts.windows {
            let (tx, thread) = win::com::spawn(inbox.clone());
            actor = actor.with_windows(tx);
            Some(thread)
        } else {
            None
        };
        #[cfg(not(windows))]
        let windows = {
            if opts.windows {
                eprintln!("controller: there's no Windows side to watch on this platform");
            }
            None
        };
        let driver = match opts.driver {
            Some(open) => {
                let (tx, thread) = driver::spawn(open, inbox.clone());
                actor = actor.with_driver(tx);
                Some(thread)
            }
            None => None,
        };
        let persist = match opts.persist {
            Some((path, timing)) => {
                let (tx, thread) = persist::spawn(path, timing, inbox.clone());
                actor = actor.with_persist(tx);
                Some(thread)
            }
            None => None,
        };
        let actor = std::thread::Builder::new()
            .name("sb-actor".into())
            .spawn(move || actor.run(rx))
            .expect("spawn the actor");
        ui_server::spawn(listener, hub.clone(), inbox.clone());
        Self {
            inbox,
            hub,
            actor,
            link,
            windows,
            driver,
            persist,
        }
    }

    /// Wait for the actor to stop (something sent [`Msg::Stop`]), then shut the engine down.
    pub fn join(self) -> AppState {
        // The actor stops the COM thread, the driver worker and the persistence worker (after
        // its last save) on its way out.
        let state = self.actor.join().expect("the actor panicked");
        for thread in [self.windows, self.driver, self.persist].into_iter().flatten() {
            let _ = thread.join();
        }
        if let Some((tx, thread)) = self.link {
            let _ = tx.send(LinkMsg::Stop);
            let _ = thread.join();
        }
        state
    }

    /// Stop the actor and the engine, and return the final state. (The UI listener thread
    /// stays blocked in `accept` until the process exits.)
    pub fn stop(self) -> AppState {
        let _ = self.inbox.send(Msg::Stop);
        self.join()
    }
}
