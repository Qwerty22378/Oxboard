//! Talk to a running `sb-controller` like a UI would.
//!
//!   ctl [--address ADDR] show
//!   ctl watch                          print every diff as it arrives (Ctrl+C to stop)
//!   ctl system DEVICE|off              where the PC's sound comes in (VB-Cable's "CABLE Output")
//!   ctl select DEVICE on|off           Interface 1 output selection
//!   ctl master GAIN | ctl master mute|unmute
//!   ctl level APP GAIN | ctl level APP mute|unmute
//!   ctl send APP OUTPUT[,OUTPUT...]    Interface 2: the app plays there; "default" undoes it
//!   ctl route SOURCE SINK GAIN|off     SOURCE: a device, an app, or "clips"
//!   ctl clip-add PATH [NAME] | ctl clip-rm ID
//!   ctl play ID | ctl stop ID          trigger a clip
//!   ctl restart-engine
//!   ctl dismiss                        clear the "settings couldn't be read" notice
//!
//! DEVICE, APP, SOURCE and SINK match names, case-insensitively, by substring.

use sb_controller::client::UiClient;
use sb_controller::transport;
use sb_shared::ui::*;
use std::io::Write;
use std::time::Duration;

type Client = UiClient<Box<dyn Write + Send>>;

const T: Duration = Duration::from_secs(5);

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut address = transport::default_address();
    if args.first().map(String::as_str) == Some("--address") {
        if args.len() < 2 {
            return Err("--address needs a value".into());
        }
        address = args.remove(1);
        args.remove(0);
    }
    let conn = transport::connect(&address, T).map_err(|e| format!("connect to {address}: {e}"))?;
    let mut c = Client::over(conn, "ctl")?;
    c.subscribe()?;
    while !c.mirror.synced {
        if c.next(T).is_none() {
            return Err("no snapshot from the controller".into());
        }
    }
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let cmd = match a.as_slice() {
        [] | ["show"] => {
            show(&c.mirror.view);
            return Ok(());
        }
        ["watch"] => return watch(&mut c),
        ["system", "off"] => Command::SetSystemInput(None),
        ["system", dev] => Command::SetSystemInput(Some(SystemInput::Device(device(
            &c.mirror.view,
            dev,
            Some(Flow::Capture),
        )?))),
        ["select", dev, on] => Command::SelectOutput {
            device: device(&c.mirror.view, dev, Some(Flow::Render))?,
            selected: switch(on)?,
        },
        ["master", "mute"] => Command::SetMasterMuted(true),
        ["master", "unmute"] => Command::SetMasterMuted(false),
        ["master", g] => Command::SetMasterGain(number(g)?),
        ["level", app, "mute"] => Command::SetAppMuted {
            app: find_app(&c.mirror.view, app)?,
            muted: true,
        },
        ["level", app, "unmute"] => Command::SetAppMuted {
            app: find_app(&c.mirror.view, app)?,
            muted: false,
        },
        ["level", app, g] => Command::SetAppGain {
            app: find_app(&c.mirror.view, app)?,
            gain: number(g)?,
        },
        ["route", from, to, g] => Command::SetRoute {
            from: source(&c.mirror.view, from)?,
            to: Sink::Device(device(&c.mirror.view, to, Some(Flow::Render))?),
            gain: if *g == "off" { None } else { Some(number(g)?) },
        },
        ["clip-add", path] => Command::AddClip {
            path: path.to_string(),
            name: String::new(),
        },
        ["clip-add", path, name] => Command::AddClip {
            path: path.to_string(),
            name: name.to_string(),
        },
        ["send", app, "default"] => Command::RouteApp {
            app: find_app(&c.mirror.view, app)?,
            outputs: Default::default(),
        },
        ["send", app, outs] => Command::RouteApp {
            app: find_app(&c.mirror.view, app)?,
            outputs: outs
                .split(',')
                .map(|o| device(&c.mirror.view, o.trim(), Some(Flow::Render)))
                .collect::<Result<_, _>>()?,
        },
        ["clip-rm", id] => Command::RemoveClip { id: clip(id)? },
        ["play", id] => Command::PlayClip { id: clip(id)? },
        ["stop", id] => Command::StopClip { id: clip(id)? },
        ["restart-engine"] => Command::RestartEngine,
        ["dismiss"] => Command::DismissLoadError,
        _ => return Err("unknown command (see the top of examples/ctl.rs)".into()),
    };
    let before = c.mirror.view.clone();
    match c.call(cmd, T)? {
        Ok(()) => {
            for op in diff(&before, &c.mirror.view) {
                println!("{}", describe(&c.mirror.view, &op));
            }
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

fn switch(s: &str) -> Result<bool, String> {
    match s {
        "on" | "true" | "1" => Ok(true),
        "off" | "false" | "0" => Ok(false),
        _ => Err(format!("{s}: on or off")),
    }
}

fn clip(s: &str) -> Result<ClipId, String> {
    s.trim_start_matches('#')
        .parse()
        .map(ClipId)
        .map_err(|_| format!("bad clip id {s}"))
}

fn number(s: &str) -> Result<f32, String> {
    s.parse().map_err(|_| format!("{s} isn't a number"))
}

fn one<K: Clone>(what: &str, pattern: &str, found: Vec<(K, String)>) -> Result<K, String> {
    match found.as_slice() {
        [(k, _)] => Ok(k.clone()),
        [] => Err(format!("no {what} matches \"{pattern}\"")),
        many => Err(format!(
            "\"{pattern}\" matches several: {}",
            many.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

fn matches(name: &str, pattern: &str) -> bool {
    name.to_lowercase().contains(&pattern.to_lowercase())
}

fn device(v: &StateView, pattern: &str, flow: Option<Flow>) -> Result<DeviceKey, String> {
    let found = v
        .devices
        .iter()
        .filter(|(_, d)| matches(&d.name, pattern) && flow.is_none_or(|f| f == d.flow))
        .map(|(k, d)| (k.clone(), d.name.clone()))
        .collect();
    one("device", pattern, found)
}

fn find_app(v: &StateView, pattern: &str) -> Result<AppKey, String> {
    let found = v
        .apps
        .iter()
        .filter(|(k, a)| matches(&a.name, pattern) || matches(&k.0, pattern))
        .map(|(k, a)| (k.clone(), a.name.clone()))
        .collect();
    one("app", pattern, found)
}

fn source(v: &StateView, pattern: &str) -> Result<Source, String> {
    if pattern.eq_ignore_ascii_case("clips") {
        return Ok(Source::Clips);
    }
    let mut found: Vec<(Source, String)> = v
        .devices
        .iter()
        .filter(|(_, d)| d.flow == Flow::Capture && matches(&d.name, pattern))
        .map(|(k, d)| (Source::Device(k.clone()), d.name.clone()))
        .collect();
    found.extend(
        v.apps
            .iter()
            .filter(|(_, a)| matches(&a.name, pattern))
            .map(|(k, a)| (Source::App(k.clone()), a.name.clone())),
    );
    one("source", pattern, found)
}

fn db(gain: f32) -> String {
    if gain <= 0.0 {
        "-inf dB".into()
    } else {
        format!("{:+.1} dB", 20.0 * gain.log10())
    }
}

fn level(l: Level) -> String {
    format!("{:.2} ({}){}", l.gain, db(l.gain), if l.muted { " muted" } else { "" })
}

fn source_name(v: &StateView, s: &Source) -> String {
    match s {
        Source::Device(k) => v.devices.get(k).map_or(k.0.clone(), |d| d.name.clone()),
        Source::App(k) => v.apps.get(k).map_or(k.0.clone(), |a| a.name.clone()),
        Source::Clips => "Clips".into(),
    }
}

fn sink_name(v: &StateView, s: &Sink) -> String {
    let Sink::Device(k) = s;
    v.devices.get(k).map_or(k.0.clone(), |d| d.name.clone())
}

fn outputs(v: &StateView, outs: &std::collections::BTreeSet<DeviceKey>) -> String {
    let names: Vec<String> = outs
        .iter()
        .map(|k| v.devices.get(k).map_or(k.0.clone(), |d| d.name.clone()))
        .collect();
    names.join(", ")
}

fn settings(s: &SettingsView) -> String {
    let mut out = Vec::new();
    if let Some(e) = &s.load_error {
        out.push(format!(
            "couldn't be read ({}), started from defaults; {}",
            e.reason,
            e.backup
                .as_ref()
                .map_or("the file was left in place".into(), |b| format!("moved to {b}"))
        ));
    }
    if let Some(e) = &s.save_error {
        out.push(format!("can't be saved ({e})"));
    }
    if out.is_empty() { "ok".into() } else { out.join("; ") }
}

fn show(v: &StateView) {
    println!("engine: {:?}", v.engine);
    println!("settings: {}", settings(&v.settings));
    println!("master: {}", level(v.master));
    println!(
        "system input: {}",
        match &v.system_input {
            None => "none".into(),
            Some(SystemInput::Device(k)) => v.devices.get(k).map_or(k.0.clone(), |d| d.name.clone()),
            Some(SystemInput::Endpoint(id)) => format!("driver endpoint {id}"),
        }
    );
    println!("devices:");
    for d in v.devices.values() {
        println!(
            "  {} {:<24} {:?}{}{}{}{}",
            if d.selected { "[x]" } else { "[ ]" },
            d.name,
            d.flow,
            if d.present { "" } else { "  (not connected)" },
            if d.loops { "  (would loop)" } else { "" },
            if d.failed { "  (won't open, retrying)" } else { "" },
            if d.mic { "  (feeds a mic)" } else { "" }
        );
    }
    println!("apps:");
    for a in v.apps.values() {
        println!(
            "  {:<24} {}{}",
            a.name,
            level(a.level),
            if a.sessions == 0 { "  (not running)" } else { "" }
        );
        if let Some(r) = &a.route {
            println!("      -> {} ({:?})", outputs(v, &r.outputs), r.status);
        }
    }
    println!("routes:");
    for ((from, to), g) in &v.routes {
        println!("  {} -> {}: {}", source_name(v, from), sink_name(v, to), db(*g));
    }
    if !v.would_loop.is_empty() {
        println!("would loop:");
        for (from, to) in &v.would_loop {
            println!("  {} -> {}", source_name(v, from), sink_name(v, to));
        }
    }
    println!("clips:");
    for (id, c) in &v.clips {
        println!(
            "  #{} {:<20} {} {}{}",
            id.0,
            c.settings.name,
            db(c.settings.gain),
            c.path,
            if c.playing { "  (playing)" } else { "" }
        );
    }
}

fn describe(v: &StateView, op: &DiffOp) -> String {
    match op {
        DiffOp::Device(_, Some(d)) => format!(
            "device {}: {}{}{}{}",
            d.name,
            if d.selected { "selected" } else { "not selected" },
            if d.present { "" } else { ", not connected" },
            if d.loops { ", would loop" } else { "" },
            if d.failed { ", won't open (retrying)" } else { "" }
        ),
        DiffOp::Device(k, None) => format!("device {} gone", k.0),
        DiffOp::App(_, Some(a)) => format!(
            "app {}: {} ({} sessions){}",
            a.name,
            level(a.level),
            a.sessions,
            a.route.as_ref().map_or(String::new(), |r| format!(
                ", -> {} ({:?})",
                outputs(v, &r.outputs),
                r.status
            ))
        ),
        DiffOp::App(k, None) => format!("app {} gone", k.0),
        DiffOp::Clip(id, Some(c)) => format!(
            "clip #{} {}: {} {}{}",
            id.0,
            c.settings.name,
            db(c.settings.gain),
            c.path,
            if c.playing { " (playing)" } else { "" }
        ),
        DiffOp::Clip(id, None) => format!("clip #{} removed", id.0),
        DiffOp::Route(from, to, g) => format!(
            "route {} -> {}: {}",
            source_name(v, from),
            sink_name(v, to),
            g.map_or("off".into(), db)
        ),
        DiffOp::WouldLoop(from, to, now) => format!(
            "{} -> {} {}",
            source_name(v, from),
            sink_name(v, to),
            if *now { "would loop" } else { "is safe again" }
        ),
        DiffOp::SystemInput(i) => format!("system input: {i:?}"),
        DiffOp::Master(l) => format!("master: {}", level(*l)),
        DiffOp::Engine(e) => format!("engine: {e:?}"),
        DiffOp::Settings(s) => format!("settings: {}", settings(s)),
    }
}

fn watch(c: &mut Client) -> Result<(), String> {
    show(&c.mirror.view);
    println!("-- watching (seq {})", c.mirror.seq);
    loop {
        match c.next(Duration::from_millis(500)) {
            Some(UiMessage::Diff { seq, ops }) => {
                for op in &ops {
                    println!("#{seq} {}", describe(&c.mirror.view, op));
                }
            }
            Some(UiMessage::Snapshot { seq, .. }) => println!("-- resynced at seq {seq}"),
            Some(_) => {}
            None if !c.is_connected() => return Err("controller gone".into()),
            None => {}
        }
    }
}
