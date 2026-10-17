//! `sb-controller [--address ADDR] [--fake-devices] [--log]`
//!
//! Serves UIs on `ADDR` (default: the per-user pipe `\\.\pipe\sb-controller-USER` on Windows,
//! a socket in `$XDG_RUNTIME_DIR` elsewhere) until killed. Exits with 1 if it can't serve
//! there (most likely another controller is running).
//!
//! `--fake-devices` makes up a few devices and running apps until device enumeration exists
//! (C4), so a UI has something to show (a VB-Cable among them). `--log` prints every message
//! the actor handles, and every graph or param update it would send the engine.

use sb_controller::Controller;
use sb_controller::actor::Msg;
use sb_controller::state::{AppState, DeviceInfo, Loopback, Observation};
use sb_controller::transport;
use sb_shared::ui::{AppKey, DeviceKey, Flow};

fn fake_world() -> Vec<Observation> {
    let device = |id: &str, name: &str, flow| Observation::DeviceAdded {
        key: DeviceKey(format!(
            "{{0.0.{}.00000000}}.{{fake-{id}}}",
            (flow == Flow::Capture) as u8
        )),
        info: DeviceInfo {
            name: name.into(),
            flow,
        },
    };
    let app = |exe: &str, name: &str, pid| Observation::SessionAdded {
        app: AppKey(format!(r"C:\Fake\{exe}")),
        name: name.into(),
        pid,
    };
    vec![
        device("speakers", "Speakers", Flow::Render),
        device("headphones", "Headphones", Flow::Render),
        device("hdmi", "Monitor HDMI", Flow::Render),
        device("mic", "Microphone", Flow::Capture),
        // The VB-Cable stand-in: what's played into its input comes out of its output.
        device("cable-in", "CABLE Input", Flow::Render),
        device("cable-out", "CABLE Output", Flow::Capture),
        Observation::Loopback {
            render: DeviceKey("{0.0.0.00000000}.{fake-cable-in}".into()),
            into: Some(Loopback::Capture(DeviceKey("{0.0.1.00000000}.{fake-cable-out}".into()))),
        },
        app("game.exe", "Game", 1001),
        app("Discord.exe", "Discord", 1002),
        app("Spotify.exe", "Spotify", 1003),
    ]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut address, mut fake, mut log) = (transport::default_address(), false, false);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--address" => match args.next() {
                Some(v) => address = v,
                None => usage(),
            },
            "--fake-devices" => fake = true,
            "--log" => log = true,
            _ => usage(),
        }
    }
    let listener = match transport::bind(&address) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("can't serve UIs: {e}");
            std::process::exit(1);
        }
    };
    let controller = Controller::start(listener, AppState::default(), log);
    if fake {
        for o in fake_world() {
            let _ = controller.inbox.send(Msg::Observed(o));
        }
    }
    eprintln!("sb-controller {}: serving UIs on {address}", env!("CARGO_PKG_VERSION"));
    // Runs until killed; a clean exit (saving, restoring defaults) arrives with C6 and M6.
    controller.join();
}

fn usage() -> ! {
    eprintln!("usage: sb-controller [--address ADDR] [--fake-devices] [--log]");
    std::process::exit(2);
}
