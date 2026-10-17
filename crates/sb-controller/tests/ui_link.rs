//! The whole UI link over the real transport (a Unix socket; the named pipe on Windows):
//! controller (actor + UI server) and `UiClient`s.

use sb_controller::Controller;
use sb_controller::actor::Msg;
use sb_controller::client::UiClient;
use sb_controller::state::{AppState, DeviceInfo, Observation};
use sb_controller::transport;
use sb_shared::protocol::{read_frame, write_frame};
use sb_shared::ui::*;
use std::io::Write;
use std::time::{Duration, Instant};

const T: Duration = Duration::from_secs(5);

#[cfg(windows)]
fn address(name: &str) -> String {
    format!(r"\\.\pipe\sb-controller-test-{name}-{}", std::process::id())
}

#[cfg(unix)]
fn address(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("sb-controller-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{name}.sock")).to_string_lossy().into_owned()
}

fn speakers() -> DeviceKey {
    DeviceKey("{0.0.0.00000000}.{speakers}".into())
}

fn start(name: &str) -> (Controller, String) {
    let addr = address(name);
    let controller = Controller::start(transport::bind(&addr).unwrap(), AppState::default(), false);
    controller
        .inbox
        .send(Msg::Observed(Observation::DeviceAdded {
            key: speakers(),
            info: DeviceInfo {
                name: "Speakers".into(),
                flow: Flow::Render,
            },
        }))
        .unwrap();
    (controller, addr)
}

fn client(addr: &str, name: &str) -> UiClient<Box<dyn Write + Send>> {
    UiClient::over(transport::connect(addr, T).unwrap(), name).unwrap()
}

/// Read until the mirror has a snapshot and has seen diff `seq`.
fn sync_to(c: &mut UiClient<Box<dyn Write + Send>>, seq: u64) {
    let deadline = Instant::now() + T;
    while !(c.mirror.synced && c.mirror.seq >= seq) {
        assert!(
            Instant::now() < deadline,
            "never reached seq {seq} (at {})",
            c.mirror.seq
        );
        c.next(Duration::from_millis(50));
    }
}

fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + T;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn two_uis_see_the_same_state() {
    let (controller, addr) = start("two");
    let mut a = client(&addr, "overlay");
    let mut b = client(&addr, "stream deck");
    assert!(a.controller.starts_with("sb-controller"));
    a.subscribe().unwrap();
    b.subscribe().unwrap();
    sync_to(&mut a, 1);
    sync_to(&mut b, 1);
    assert_eq!(a.mirror.view.devices[&speakers()].name, "Speakers");

    let select = Command::SelectOutput {
        device: speakers(),
        selected: true,
    };
    assert_eq!(a.call(select, T).unwrap(), Ok(()));
    // The diff came before `Completed`: A already shows it.
    assert!(a.mirror.view.devices[&speakers()].selected);
    assert_eq!(a.call(Command::SetMasterGain(0.25), T).unwrap(), Ok(()));
    let bad = b.call(Command::SetMasterGain(9.0), T).unwrap().unwrap_err();
    assert_eq!(bad.kind, ErrorKind::Invalid);
    sync_to(&mut b, a.mirror.seq);
    assert_eq!(b.mirror.view, a.mirror.view);
    assert_eq!(b.mirror.view.master.gain, 0.25);

    // A third UI, late, starts from a snapshot of the same state.
    let mut c = client(&addr, "late");
    c.subscribe().unwrap();
    sync_to(&mut c, a.mirror.seq);
    assert_eq!(c.mirror.view, a.mirror.view);

    let state = controller.stop();
    assert!(state.desired.outputs.contains(&speakers()));
}

#[test]
fn a_ui_that_leaves_is_forgotten() {
    let (controller, addr) = start("leave");
    let mut a = client(&addr, "a");
    let b = client(&addr, "b");
    a.subscribe().unwrap();
    sync_to(&mut a, 1);
    wait_for("both registered", || controller.hub.len() == 2);
    drop(b);
    wait_for("b removed", || controller.hub.len() == 1);
    // A carries on.
    assert_eq!(a.call(Command::SetMasterMuted(true), T).unwrap(), Ok(()));
    assert!(a.mirror.view.master.muted);
    drop(a);
    wait_for("a removed", || controller.hub.is_empty());
    controller.stop();
}

#[test]
fn commands_work_without_subscribing() {
    // A hotkey tool fires commands and never looks at state.
    let (controller, addr) = start("fire");
    let mut tool = client(&addr, "hotkeys");
    let added = Command::AddClip {
        path: "/clips/airhorn.ogg".into(),
        name: String::new(),
    };
    assert_eq!(tool.call(added, T).unwrap(), Ok(()));
    assert!(!tool.mirror.synced, "no state was sent");
    let state = controller.stop();
    assert_eq!(state.desired.clips[&ClipId(0)].settings.name, "airhorn");
}

#[test]
fn a_mismatched_ui_is_refused() {
    let (controller, addr) = start("refuse");
    let conn = transport::connect(&addr, T).unwrap();
    let (mut r, mut w) = (conn.reader, conn.writer);
    write_frame(
        &mut w,
        &UiHello {
            protocol: UI_PROTOCOL_VERSION + 1,
            client: "future".into(),
        },
    )
    .unwrap();
    match read_frame::<UiMessage>(&mut r).unwrap() {
        Some(UiMessage::Refused { reason }) => assert!(reason.contains("expected"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(read_frame::<UiMessage>(&mut r).unwrap(), None, "then closed");
    // Garbage instead of a hello: closed without a word.
    let conn = transport::connect(&addr, T).unwrap();
    let (mut r, mut w) = (conn.reader, conn.writer);
    w.write_all(&[2, 0, 0, 0, 0xff, 0xff]).unwrap();
    assert!(read_frame::<UiMessage>(&mut r).map_or(true, |m| m.is_none()));
    assert!(controller.hub.is_empty());
    // The controller still serves.
    let mut ok = client(&addr, "ok");
    assert_eq!(ok.call(Command::SetMasterMuted(false), T).unwrap(), Ok(()));
    controller.stop();
}

#[test]
fn a_second_controller_cant_take_the_address() {
    let (controller, addr) = start("twice");
    let err = transport::bind(&addr).err().expect("bound twice");
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    controller.stop();
}

#[test]
fn diffs_arrive_while_a_fader_is_dragged() {
    // 60 Hz fader updates from one UI; another UI follows every step in order.
    let (controller, addr) = start("drag");
    let mut fader = client(&addr, "fader");
    let mut watcher = client(&addr, "watcher");
    watcher.subscribe().unwrap();
    sync_to(&mut watcher, 1);
    let mut last = 0;
    for i in 1..=200 {
        last = fader.send(Command::SetMasterGain(i as f32 / 100.0)).unwrap();
    }
    let deadline = Instant::now() + T;
    loop {
        match fader.next(Duration::from_millis(100)) {
            Some(UiMessage::Completed(id)) if id == last => break,
            Some(UiMessage::Failed(id, e)) => panic!("{id}: {e}"),
            _ => assert!(Instant::now() < deadline),
        }
    }
    sync_to(&mut watcher, 201);
    assert_eq!(watcher.mirror.view.master.gain, 2.0);
    controller.stop();
}

#[test]
fn settings_outlive_the_controller() {
    let dir = std::env::temp_dir().join(format!("sb-controller-settings-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("config.toml");
    let launch = |name: &str| {
        let loaded = sb_controller::persist::load(&path);
        assert!(loaded.error.is_none(), "{:?}", loaded.error);
        let addr = address(name);
        let opts = sb_controller::Options {
            persist: Some((path.clone(), sb_controller::persist::Timing::default())),
            ..Default::default()
        };
        let controller = Controller::launch(transport::bind(&addr).unwrap(), loaded.state(), opts);
        (controller, addr)
    };

    let (controller, addr) = launch("settings-1");
    controller
        .inbox
        .send(Msg::Observed(Observation::DeviceAdded {
            key: speakers(),
            info: DeviceInfo {
                name: "Speakers".into(),
                flow: Flow::Render,
            },
        }))
        .unwrap();
    let mut ui = client(&addr, "test");
    ui.subscribe().unwrap();
    let select = Command::SelectOutput {
        device: speakers(),
        selected: true,
    };
    assert_eq!(ui.call(select, T).unwrap(), Ok(()));
    assert_eq!(ui.call(Command::SetMasterGain(0.4), T).unwrap(), Ok(()));
    // Stopped within the debounce: the last change is written on the way out.
    controller.stop();
    assert!(std::fs::read_to_string(&path).unwrap().contains("master_gain = 0.4"));

    // Back with the speakers unplugged: still selected, by their remembered name.
    let (controller, addr) = launch("settings-2");
    let mut ui = client(&addr, "test");
    ui.subscribe().unwrap();
    sync_to(&mut ui, 0);
    let v = &ui.mirror.view;
    assert_eq!(v.master.gain, 0.4);
    let d = &v.devices[&speakers()];
    assert!(d.selected && !d.present && d.name == "Speakers", "{d:?}");
    assert_eq!(v.settings, SettingsView::default());
    controller.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
