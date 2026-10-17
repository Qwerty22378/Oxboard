//! `sb-controller [--address ADDR] [--engine EXE | --no-engine] [--fake-driver] [--fake-devices] [--log]`
//!
//! Serves UIs on `ADDR` (default: the per-user pipe `\\.\pipe\sb-controller-USER` on Windows,
//! a socket in `$XDG_RUNTIME_DIR` elsewhere) until killed. Exits with 1 if it can't serve
//! there (most likely another controller is running).
//!
//! On Windows it runs the engine host `EXE` (default: `sb-engine-host.exe` next to this
//! program), restarting it if it dies; `--fake-driver` is passed on to it. Elsewhere, and with
//! `--no-engine`, there's no engine: the UI side only.
//!
//! On Windows it watches the real devices and app sessions, and sets app levels as Windows
//! session volumes. `--fake-devices` makes some up instead (a VB-Cable among them), there and
//! elsewhere. `--log` prints every message the actor handles, and every graph or param update
//! it sends the engine.

use sb_controller::actor::Msg;
use sb_controller::state::{AppState, DeviceInfo, Loopback, Observation};
use sb_controller::transport;
use sb_controller::{Controller, Options};
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
    let (mut engine, mut no_engine, mut fake_driver) = (None, false, false);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--address" => match args.next() {
                Some(v) => address = v,
                None => usage(),
            },
            "--engine" => match args.next() {
                Some(v) => engine = Some(std::path::PathBuf::from(v)),
                None => usage(),
            },
            "--no-engine" => no_engine = true,
            "--fake-driver" => fake_driver = true,
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
    let opts = Options {
        log,
        engine: if no_engine {
            None
        } else {
            engine_spawner(engine, fake_driver)
        },
        windows: !fake && cfg!(windows),
    };
    let controller = Controller::launch(listener, AppState::default(), opts);
    if fake {
        for o in fake_world() {
            let _ = controller.inbox.send(Msg::Observed(o));
        }
    }
    eprintln!("sb-controller {}: serving UIs on {address}", env!("CARGO_PKG_VERSION"));
    // Runs until killed; a clean exit (saving, restoring defaults) arrives with C6 and M6.
    controller.join();
}

#[cfg(windows)]
fn engine_spawner(
    engine: Option<std::path::PathBuf>,
    fake_driver: bool,
) -> Option<(
    Box<dyn sb_controller::engine_link::Spawn>,
    sb_controller::engine_link::LinkConfig,
)> {
    use sb_controller::engine_link::{LinkConfig, ProcessSpawner};
    let exe = engine.unwrap_or_else(|| {
        let me = std::env::current_exe().unwrap_or_default();
        me.with_file_name("sb-engine-host.exe")
    });
    let args = if fake_driver {
        vec!["--fake-driver".to_string()]
    } else {
        vec![]
    };
    eprintln!("engine host: {}", exe.display());
    Some((Box::new(ProcessSpawner::new(exe, args)), LinkConfig::default()))
}

#[cfg(not(windows))]
fn engine_spawner(
    engine: Option<std::path::PathBuf>,
    _fake_driver: bool,
) -> Option<(
    Box<dyn sb_controller::engine_link::Spawn>,
    sb_controller::engine_link::LinkConfig,
)> {
    if engine.is_some() {
        eprintln!("the engine host only runs on Windows; running without an engine");
    }
    None
}

fn usage() -> ! {
    eprintln!(
        "usage: sb-controller [--address ADDR] [--engine EXE | --no-engine] [--fake-driver] [--fake-devices] [--log]"
    );
    std::process::exit(2);
}
