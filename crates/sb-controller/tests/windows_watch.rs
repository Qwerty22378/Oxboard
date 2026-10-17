//! The COM thread against the real Windows audio stack: devices are listed, and this test's
//! own process becomes an app with a session whose volume both sides can change.
#![cfg(windows)]

use sb_controller::actor::Msg;
use sb_controller::state::Observation;
use sb_controller::watch::WinMsg;
use sb_controller::win::com;
use sb_shared::ui::{Flow, Level};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree};

const T: Duration = Duration::from_secs(5);

/// The tests' sessions all belong to one app (this executable), so they take turns.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn turn() -> std::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

/// Observations until one matches; panics with what came if none does in time.
fn until(rx: &Receiver<Msg>, what: &str, mut pred: impl FnMut(&Observation) -> bool) -> Vec<Observation> {
    let deadline = Instant::now() + T;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Msg::ObservedAll(all)) => {
                let hit = all.iter().any(&mut pred);
                seen.extend(all);
                if hit {
                    return seen;
                }
            }
            Ok(_) => {}
            Err(_) => panic!("no {what}; got {seen:#?}"),
        }
    }
}

/// A shared-mode stream on the default output: this process now has an audio session.
struct Stream {
    /// Held so the stream (and so the session) stays open.
    _client: IAudioClient,
    volume: ISimpleAudioVolume,
}

impl Stream {
    fn open() -> Self {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
            let device = en.GetDefaultAudioEndpoint(eRender, eConsole).unwrap();
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).unwrap();
            let format = client.GetMixFormat().unwrap();
            client
                .Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, format, None)
                .unwrap();
            CoTaskMemFree(Some(format as *const _));
            client.Start().unwrap();
            let volume: ISimpleAudioVolume = client.GetService().unwrap();
            Self {
                _client: client,
                volume,
            }
        }
    }

    fn level(&self) -> f32 {
        unsafe { self.volume.GetMasterVolume().unwrap() }
    }
}

fn level(gain: f32) -> Level {
    Level { gain, muted: false }
}

#[test]
fn devices_and_this_processs_session_and_its_volume() {
    let _turn = turn();
    let (inbox, rx) = mpsc::channel();
    let (tx, thread) = com::spawn(inbox);

    // Every active endpoint is reported on the first scan.
    let first = until(
        &rx,
        "a render device",
        |o| matches!(o, Observation::DeviceAdded { info, .. } if info.flow == Flow::Render),
    );
    let names: Vec<_> = first
        .iter()
        .filter_map(|o| match o {
            Observation::DeviceAdded { info, .. } => Some(format!("{} ({:?})", info.name, info.flow)),
            _ => None,
        })
        .collect();
    println!("devices: {names:#?}");

    // Play: the session shows up as this executable, by its path.
    let stream = Stream::open();
    let pid = std::process::id();
    let seen = until(
        &rx,
        "our session",
        |o| matches!(o, Observation::SessionAdded { pid: p, .. } if *p == pid),
    );
    let (app, name) = seen
        .iter()
        .find_map(|o| match o {
            Observation::SessionAdded { app, name, pid: p } if *p == pid => Some((app.clone(), name.clone())),
            _ => None,
        })
        .unwrap();
    println!("this test is {name:?} at {}", app.0);
    let exe = std::env::current_exe().unwrap();
    assert!(
        app.0.eq_ignore_ascii_case(&exe.to_string_lossy()),
        "{} vs {}",
        app.0,
        exe.display()
    );

    // Windows remembers an app's volume between runs: move it from outside (the Windows
    // mixer) to a value it doesn't have yet.
    let start = if (stream.level() - 0.6).abs() < 0.01 { 0.5 } else { 0.6 };
    unsafe { stream.volume.SetMasterVolume(start, std::ptr::null()).unwrap() };
    until(
        &rx,
        "the first outside change",
        |o| matches!(o, Observation::AppLevel { app: a, level } if *a == app && (level.gain - start).abs() < 1e-3),
    );

    // The controller sets 0.3: Windows has it, and it isn't reported back as news.
    tx.send(WinMsg::SetAppLevel {
        app: app.clone(),
        level: level(0.3),
    })
    .unwrap();
    let deadline = Instant::now() + T;
    while (stream.level() - 0.3).abs() > 1e-3 {
        assert!(Instant::now() < deadline, "volume never set: {}", stream.level());
        std::thread::sleep(Duration::from_millis(10));
    }
    // The mixer moves it straight back, before the watcher's next scan: still news, although
    // it's what the last scan saw.
    unsafe { stream.volume.SetMasterVolume(start, std::ptr::null()).unwrap() };
    let seen = until(
        &rx,
        "the outside change",
        |o| matches!(o, Observation::AppLevel { app: a, .. } if *a == app),
    );
    let reported: Vec<f32> = seen
        .iter()
        .filter_map(|o| match o {
            Observation::AppLevel { app: a, level } if *a == app => Some(level.gain),
            _ => None,
        })
        .collect();
    assert_eq!(reported.len(), 1, "only the outside change: {reported:?}");
    assert!((reported[0] - start).abs() < 1e-3, "{reported:?}");

    // A session outlives its stream (Windows lists it as inactive until the process exits,
    // as its mixer does); see the next test for the end.
    drop(stream);

    tx.send(WinMsg::Stop).unwrap();
    thread.join().unwrap();
}

/// Run by the next test as a child process: play for a moment, then exit.
#[test]
#[ignore]
fn child_plays_briefly() {
    let stream = Stream::open();
    std::thread::sleep(Duration::from_millis(1500));
    drop(stream);
}

#[test]
fn an_app_that_exits_is_gone() {
    let _turn = turn();
    let (inbox, rx) = mpsc::channel();
    let (tx, thread) = com::spawn(inbox);
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["child_plays_briefly", "--exact", "--ignored", "--quiet"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    until(
        &rx,
        "the child's session",
        |o| matches!(o, Observation::SessionAdded { pid: p, .. } if *p == pid),
    );
    assert!(child.wait().unwrap().success());
    let t = Instant::now();
    until(
        &rx,
        "the child's session ending",
        |o| matches!(o, Observation::SessionRemoved { pid: p, .. } if *p == pid),
    );
    println!("session gone {:?} after the process", t.elapsed());
    tx.send(WinMsg::Stop).unwrap();
    thread.join().unwrap();
}
