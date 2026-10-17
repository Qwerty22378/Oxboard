//! `sb-controller [--address ADDR] [--engine EXE | --no-engine] [--config FILE | --no-config]
//!                [--fake-driver] [--fake-devices] [--log]`
//!
//! Serves UIs on `ADDR` (default: the per-user pipe `\\.\pipe\sb-controller-USER` on Windows,
//! a socket in `$XDG_RUNTIME_DIR` elsewhere) until killed. Exits with 1 if it can't serve
//! there (most likely another controller is running).
//!
//! On Windows it runs the engine host `EXE` (default: `sb-engine-host.exe` next to this
//! program), restarting it if it dies. Elsewhere, and with `--no-engine`, there's no engine.
//!
//! It loads the user's settings from `FILE` (default: `%APPDATA%\Soundboard\config.toml` on
//! Windows, `~/.config/soundboard/config.toml` elsewhere) and saves them there as they change.
//! `--no-config` starts from defaults and saves nothing; so does `--fake-devices` unless a
//! `--config` is given, so made-up devices never end up in the real settings.
//!
//! It manages the soundboard driver's endpoint pool (Interface 2). `--fake-driver` stands in
//! for the driver until it exists: a pool of 8 app endpoints and 2 virtual mics here, and the
//! same in the engine host (passed on to it).
//!
//! On Windows it watches the real devices and app sessions, and sets app levels as Windows
//! session volumes. `--fake-devices` makes some up instead (a VB-Cable among them), there and
//! elsewhere. `--log` prints every message the actor handles, and every graph or param update
//! it sends the engine.

use sb_controller::actor::Msg;
use sb_controller::persist;
use sb_controller::state::{AppState, DeviceInfo, Loopback, Observation};
use sb_controller::transport;
use sb_controller::{Controller, Options};
use sb_shared::ui::{AppKey, DeviceKey, Flow};
use std::path::PathBuf;

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
    let (mut config, mut no_config) = (None, false);
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
            "--config" => match args.next() {
                Some(v) => config = Some(PathBuf::from(v)),
                None => usage(),
            },
            "--no-config" => no_config = true,
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
    let config = match config {
        _ if no_config => None,
        Some(path) => Some(path),
        None if fake => None,
        None => persist::default_path(),
    };
    let (state, save_to) = match &config {
        Some(path) => {
            let loaded = persist::load(path);
            match &loaded.error {
                None => eprintln!("settings: {}", path.display()),
                Some(e) => eprintln!(
                    "settings: {} couldn't be read ({}); starting from defaults{}",
                    path.display(),
                    e.reason,
                    e.backup
                        .as_ref()
                        .map_or(String::new(), |b| format!(", the file is now {b}"))
                ),
            }
            let save_to = loaded.writable.then(|| (path.clone(), persist::Timing::default()));
            (loaded.state(), save_to)
        }
        None => {
            eprintln!("settings: defaults, not saved");
            (AppState::default(), None)
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
        driver: open_driver(fake_driver),
        persist: save_to,
    };
    let controller = Controller::launch(listener, state, opts);
    if fake {
        for o in fake_world() {
            let _ = controller.inbox.send(Msg::Observed(o));
        }
    }
    eprintln!("sb-controller {}: serving UIs on {address}", env!("CARGO_PKG_VERSION"));
    // Runs until killed: a change less than 500 ms old isn't saved yet. A clean exit (and
    // restoring Windows' defaults) arrives with M6.
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

fn open_driver(fake: bool) -> Option<sb_controller::OpenDriver> {
    use sb_controller::driver::{FakeDriver, Transport};
    if fake {
        return Some(Box::new(|| Ok(Box::new(FakeDriver::new(8, 2)) as Box<dyn Transport>)));
    }
    #[cfg(windows)]
    return Some(Box::new(|| {
        sb_controller::driver::DeviceTransport::open().map(|t| Box::new(t) as Box<dyn Transport>)
    }));
    #[cfg(not(windows))]
    None
}

fn usage() -> ! {
    eprintln!(
        "usage: sb-controller [--address ADDR] [--engine EXE | --no-engine] [--config FILE | --no-config] \
         [--fake-driver] [--fake-devices] [--log]"
    );
    std::process::exit(2);
}
