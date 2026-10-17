//! Per-app outputs on the real Windows audio stack: a playing child process is pointed at the
//! output that *isn't* the default, opens a new stream on "the default output", and that
//! session lands on the other one.
//!
//! Windows refuses (E_INVALIDARG) a process that has never touched audio, so the child plays
//! first, as apps do before the controller sees them.
#![cfg(windows)]

use sb_controller::actor::Msg;
use sb_controller::state::Observation;
use sb_controller::watch::WinMsg;
use sb_controller::win::{com, policy::Policy};
use sb_shared::ui::{AppKey, DeviceKey};
use std::io::{BufRead, Write};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree};

const T: Duration = Duration::from_secs(5);

fn com() {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

fn id_of(d: &IMMDevice) -> String {
    unsafe {
        let p = d.GetId().unwrap();
        let s = p.to_string().unwrap();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

/// (the default output, another active output).
fn outputs() -> (DeviceKey, DeviceKey) {
    com();
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
        let default = id_of(&en.GetDefaultAudioEndpoint(eRender, eConsole).unwrap());
        let all = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE).unwrap();
        let other = (0..all.GetCount().unwrap())
            .map(|i| id_of(&all.Item(i).unwrap()))
            .find(|id| *id != default)
            .expect("this test needs two outputs");
        (DeviceKey(default), DeviceKey(other))
    }
}

/// Open a shared stream on "the default output" and start it.
fn play() -> (IAudioClient, String) {
    com();
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
        let device = en.GetDefaultAudioEndpoint(eRender, eConsole).unwrap();
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).unwrap();
        let format = client.GetMixFormat().unwrap();
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, format, None)
            .unwrap();
        CoTaskMemFree(Some(format as *const _));
        client.Start().unwrap();
        (client, id_of(&device))
    }
}

/// Run as a child: play, wait for a line on stdin, then play a new stream for a while.
#[test]
#[ignore]
fn child_plays_twice() {
    let (first, on) = play();
    println!("first stream: {on}");
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).unwrap();
    drop(first);
    let (_second, on) = play();
    println!("second stream: {on}");
    std::thread::sleep(Duration::from_secs(3));
}

fn until(rx: &Receiver<Msg>, what: &str, mut pred: impl FnMut(&Msg) -> bool) {
    let deadline = Instant::now() + T;
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(m) if pred(&m) => return,
            Ok(_) => {}
            Err(_) => panic!("no {what}"),
        }
    }
}

#[test]
fn an_app_pointed_at_another_output_plays_there() {
    println!("Windows build {:?}", sb_controller::win::policy::build());
    let (default, other) = outputs();
    println!("default {}, other {}", default.0, other.0);

    let (inbox, rx) = mpsc::channel();
    let (tx, thread) = com::spawn(inbox);
    until(
        &rx,
        "the routing probe",
        |m| matches!(m, Msg::Observed(Observation::AppRouting(ok)) if { assert!(*ok, "per-app outputs unsupported"); true }),
    );

    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["child_plays_twice", "--exact", "--ignored", "--nocapture", "--quiet"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let app = AppKey(std::env::current_exe().unwrap().to_string_lossy().into_owned());
    let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
    // The child's own lines (libtest prints some too).
    let mut line = move || loop {
        let l = lines.next().unwrap().unwrap();
        if l.contains("stream:") {
            return l;
        }
    };
    println!("child: {}", line());
    until(&rx, "the child's session", |m| match m {
        Msg::ObservedAll(all) => all
            .iter()
            .any(|o| matches!(o, Observation::SessionAdded { pid: p, .. } if *p == pid)),
        _ => false,
    });

    // Point it, as the controller does with a routed app's new process.
    tx.send(WinMsg::SetAppOutput {
        op: 5,
        app: app.clone(),
        pids: vec![pid],
        device: Some(other.clone()),
    })
    .unwrap();
    until(
        &rx,
        "the reply",
        |m| matches!(m, Msg::WinDone { op: 5, result } if { assert_eq!(result, &Ok(())); true }),
    );
    com();
    let policy = Policy::new().unwrap();
    assert_eq!(policy.get(pid).unwrap(), Some(other.clone()), "Windows has it");

    // A new stream on "the default output" lands on the other one.
    writeln!(child.stdin.as_mut().unwrap(), "go").unwrap();
    let second = line();
    println!("child: {second}");
    until(&rx, "the child's session on the other output", |m| match m {
        Msg::ObservedAll(all) => all.iter().any(|o| match o {
            Observation::AppDevices { app: a, devices } if *a == app => {
                println!("app devices: {devices:?}");
                devices.contains(&other)
            }
            _ => false,
        }),
        _ => false,
    });

    // Undo, so Windows doesn't keep remembering it for this executable.
    tx.send(WinMsg::SetAppOutput {
        op: 6,
        app,
        pids: vec![pid],
        device: None,
    })
    .unwrap();
    until(&rx, "the undo", |m| matches!(m, Msg::WinDone { op: 6, result: Ok(()) }));
    assert_eq!(policy.get(pid).unwrap(), None);
    child.wait().unwrap();
    tx.send(WinMsg::Stop).unwrap();
    thread.join().unwrap();
}
