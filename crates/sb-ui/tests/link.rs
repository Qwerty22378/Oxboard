//! The link against a real controller over the real transport: it connects, syncs, sends a
//! command, notices the controller going away, and reconnects to the next one by itself.

use sb_controller::Controller;
use sb_controller::actor::Msg;
use sb_controller::state::{AppState, DeviceInfo, Observation};
use sb_controller::transport;
use sb_shared::ui::{Command, DeviceKey, Flow, UiRequest};
use sb_ui::link::{Link, LinkEvent};
use sb_ui::store::{Connection, Store};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const T: Duration = Duration::from_secs(5);

#[cfg(windows)]
fn address() -> String {
    format!(r"\\.\pipe\sb-ui-test-{}", std::process::id())
}

#[cfg(unix)]
fn address() -> String {
    let dir = std::env::temp_dir().join(format!("sb-ui-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("link.sock").to_string_lossy().into_owned()
}

fn speakers() -> DeviceKey {
    DeviceKey("{0.0.0.00000000}.{speakers}".into())
}

fn controller(addr: &str) -> Controller {
    let c = Controller::start(transport::bind(addr).unwrap(), AppState::default(), false);
    c.inbox
        .send(Msg::Observed(Observation::DeviceAdded {
            key: speakers(),
            info: DeviceInfo {
                name: "Speakers".into(),
                flow: Flow::Render,
            },
        }))
        .unwrap();
    c
}

/// Feed the store until `done`; replies (resubscribes) go back over the link.
fn until(store: &mut Store, link: &Link, rx: &Receiver<LinkEvent>, what: &str, done: impl Fn(&Store) -> bool) {
    let deadline = Instant::now() + T;
    while !done(store) {
        let left = deadline.saturating_duration_since(Instant::now());
        let ev = rx
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
        if let Some(req) = store.apply(ev, Instant::now()) {
            link.send(&req).unwrap();
        }
    }
}

#[test]
fn syncs_commands_and_reconnects() {
    let addr = address();
    let first = controller(&addr);
    let (tx, rx) = mpsc::channel();
    let wakes = Arc::new(AtomicUsize::new(0));
    let w = wakes.clone();
    let link = Link::spawn(addr.clone(), "test".into(), tx, move || {
        w.fetch_add(1, Ordering::Relaxed);
    });
    let mut store = Store::default();
    until(&mut store, &link, &rx, "the snapshot", |s| {
        s.view().is_some_and(|v| v.devices.contains_key(&speakers()))
    });
    assert!(
        matches!(&store.connection, Connection::Connected { controller } if controller.starts_with("sb-controller"))
    );

    let select = Command::SelectOutput {
        device: speakers(),
        selected: true,
    };
    let req = store.command(select, Instant::now()).unwrap();
    link.send(&req).unwrap();
    until(&mut store, &link, &rx, "the change", |s| {
        s.pending.is_empty() && s.view().unwrap().devices[&speakers()].selected
    });
    assert!(wakes.load(Ordering::Relaxed) >= 3);

    // The controller goes away: the store hears it, and sending fails at once.
    first.stop();
    until(&mut store, &link, &rx, "the loss", |s| {
        matches!(s.connection, Connection::Lost { .. })
    });
    assert!(link.send(&UiRequest::Subscribe).is_err());

    // A new controller at the same address: the link finds it and the view comes back.
    // (Unix only: on Windows a stopped in-process controller's listener keeps the pipe name
    // until the process exits; the VM demo restarts a real controller process instead.)
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(&addr);
        let second = controller(&addr);
        until(&mut store, &link, &rx, "the reconnection", |s| {
            s.connected() && s.view().is_some_and(|v| !v.devices[&speakers()].selected)
        });
        second.stop();
    }
}

#[test]
fn hidden_the_ui_is_not_woken_for_state() {
    // Without a controller: the link reports it can't connect (once), and keeps trying.
    let addr = format!("{}-none", address());
    let (tx, rx) = mpsc::channel();
    let link = Link::spawn(addr, "test".into(), tx, || {});
    link.set_visible(false);
    let first = rx.recv_timeout(T).unwrap();
    assert!(
        matches!(first, LinkEvent::Lost(ref why) if why.contains("can't reach")),
        "{first:?}"
    );
    assert!(
        rx.recv_timeout(Duration::from_millis(800)).is_err(),
        "the same outage isn't reported again"
    );
}
