//! Settings on disk: [`Desired`], and only that, as a TOML file.
//!
//! - **Where:** `%APPDATA%\Soundboard\config.toml` on Windows, `$XDG_CONFIG_HOME/soundboard/
//!   config.toml` (or `~/.config/…`) elsewhere ([`default_path`]).
//! - **Format:** a schema of its own (the `File*` structs), not `Desired`'s: the file is for
//!   people too (grouped per app, gains written as typed), and internal types can change
//!   without breaking it. `schema_version` names the format; an older file is upgraded step
//!   by step ([`MIGRATIONS`], on the raw TOML) before it's read, and the original is kept
//!   beside it (`config.toml.v1`) for going back to an older build.
//! - **Saving** happens on a worker ([`spawn`]): 500 ms after the last change, so a dragged
//!   fader is one save, but at least every 2 s while changes keep coming. Each save is atomic:
//!   written to `config.toml.tmp`, flushed to disk, renamed over the old file. A crash leaves
//!   the old file or the new one, never half of one. A save that fails is reported in the view
//!   (`SettingsView::save_error`) and tried again.
//! - **A file that can't be read** (bad TOML, a newer format, a value out of range) is moved
//!   aside to `config.toml.bad-<unix time>`, and the controller starts from defaults and says
//!   so (`SettingsView::load_error`). If it can't even be moved, nothing is saved this run, so
//!   it's never overwritten.

use crate::actor::{Inbox, Msg};
use crate::state::{AppState, ClipDef, Desired, DeviceInfo};
use sb_shared::graph::Retrigger;
use sb_shared::ui::{
    AppKey, ClipId, ClipSettings, DeviceKey, Flow, Level, LoadError, MAX_GAIN, Sink, Source, SystemInput, valid_gain,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime};

/// The format this build writes.
pub const SCHEMA_VERSION: u32 = 1;

/// Upgrades a file one format up, in place: `MIGRATIONS[n]` takes format `n + 1` to `n + 2`.
/// A format change appends one and bumps [`SCHEMA_VERSION`].
pub type Migration = fn(&mut toml::Table) -> Result<(), String>;
pub const MIGRATIONS: &[Migration] = &[];
const _: () = assert!(MIGRATIONS.len() as u32 + 1 == SCHEMA_VERSION);

const HEADER: &str = "# Soundboard settings, written by sb-controller. Edit only while it isn't running.\n\n";

/// Where the settings live for this user.
pub fn default_path() -> Option<PathBuf> {
    let var = |k| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        var("APPDATA").map(|d| d.join("Soundboard").join("config.toml"))
    } else {
        var("XDG_CONFIG_HOME")
            .or_else(|| var("HOME").map(|h| h.join(".config")))
            .map(|d| d.join("soundboard").join("config.toml"))
    }
}

// --- The file, format 1 -------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct File {
    schema_version: u32,
    #[serde(default = "one")]
    master_gain: f64,
    #[serde(default)]
    master_muted: bool,
    /// `device:{endpoint id}` or `endpoint:{driver endpoint}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    system_input: Option<String>,
    /// Interface 1: leave Windows' default output alone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    keep_windows_default: bool,
    #[serde(default)]
    outputs: Vec<String>,
    #[serde(default)]
    next_clip: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    devices: Vec<FileDevice>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    apps: Vec<FileApp>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    clips: Vec<FileClip>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    matrix: Vec<FileConnection>,
}

fn one() -> f64 {
    1.0
}

/// A device the settings refer to, as last seen.
#[derive(Debug, Serialize, Deserialize)]
struct FileDevice {
    id: String,
    name: String,
    /// `render` or `capture`.
    flow: String,
}

/// An app's settings, by executable path.
#[derive(Debug, Serialize, Deserialize)]
struct FileApp {
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// Its level (a Windows session volume, 0 to 1), if set here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gain: Option<f64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    muted: bool,
    /// Interface 2: its outputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    outputs: Option<Vec<String>>,
    /// The driver endpoint it's routed into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
struct FileClip {
    id: u32,
    path: String,
    name: String,
    #[serde(default = "one")]
    gain: f64,
    /// `restart`, `overlap` or `ignore`.
    #[serde(default = "restart")]
    retrigger: String,
    #[serde(default)]
    looped: bool,
}

fn restart() -> String {
    "restart".into()
}

/// Interface 3: one connection of the matrix.
#[derive(Debug, Serialize, Deserialize)]
struct FileConnection {
    /// `device:{id}`, `app:{path}` or `clips`.
    from: String,
    /// `device:{id}`.
    to: String,
    gain: f64,
}

/// An `f32` as the `f64` with the same shortest decimal form, so 0.3 is written `0.3`.
fn wide(x: f32) -> f64 {
    x.to_string().parse().unwrap_or(x as f64)
}

fn flow_name(f: Flow) -> &'static str {
    match f {
        Flow::Render => "render",
        Flow::Capture => "capture",
    }
}

fn retrigger_name(r: Retrigger) -> &'static str {
    match r {
        Retrigger::Restart => "restart",
        Retrigger::Overlap => "overlap",
        Retrigger::Ignore => "ignore",
    }
}

fn source_name(s: &Source) -> String {
    match s {
        Source::Device(d) => format!("device:{}", d.0),
        Source::App(a) => format!("app:{}", a.0),
        Source::Clips => "clips".into(),
    }
}

impl File {
    fn from_desired(d: &Desired) -> Self {
        let apps: BTreeSet<&AppKey> = d
            .known_apps
            .keys()
            .chain(d.app_levels.keys())
            .chain(d.app_routes.keys())
            .chain(d.app_endpoints.keys())
            .collect();
        Self {
            schema_version: SCHEMA_VERSION,
            master_gain: wide(d.master.gain),
            master_muted: d.master.muted,
            system_input: d.system_input.as_ref().map(|i| match i {
                SystemInput::Device(k) => format!("device:{}", k.0),
                SystemInput::Endpoint(id) => format!("endpoint:{id}"),
            }),
            keep_windows_default: d.keep_windows_default,
            outputs: d.outputs.iter().map(|k| k.0.clone()).collect(),
            next_clip: d.next_clip,
            devices: d
                .known_devices
                .iter()
                .map(|(k, i)| FileDevice {
                    id: k.0.clone(),
                    name: i.name.clone(),
                    flow: flow_name(i.flow).into(),
                })
                .collect(),
            apps: apps
                .into_iter()
                .map(|k| FileApp {
                    path: k.0.clone(),
                    name: d.known_apps.get(k).cloned(),
                    gain: d.app_levels.get(k).map(|l| wide(l.gain)),
                    muted: d.app_levels.get(k).is_some_and(|l| l.muted),
                    outputs: d.app_routes.get(k).map(|o| o.iter().map(|k| k.0.clone()).collect()),
                    endpoint: d.app_endpoints.get(k).copied(),
                })
                .collect(),
            clips: d
                .clips
                .iter()
                .map(|(id, c)| FileClip {
                    id: id.0,
                    path: c.path.clone(),
                    name: c.settings.name.clone(),
                    gain: wide(c.settings.gain),
                    retrigger: retrigger_name(c.settings.retrigger).into(),
                    looped: c.settings.looped,
                })
                .collect(),
            matrix: d
                .matrix
                .iter()
                .map(|((from, to), g)| FileConnection {
                    from: source_name(from),
                    to: match to {
                        Sink::Device(k) => format!("device:{}", k.0),
                    },
                    gain: wide(*g),
                })
                .collect(),
        }
    }

    /// Back to `Desired`, checking everything a command would have; `Err` names the field.
    fn into_desired(self) -> Result<Desired, String> {
        let gain = |at: &str, g: f64, max: f32| {
            let g32 = g as f32;
            if valid_gain(g32) && g32 <= max {
                Ok(g32)
            } else {
                Err(format!("{at}: gain {g} is outside 0 to {max}"))
            }
        };
        let device_ref = |at: &str, s: &str| match s.strip_prefix("device:") {
            Some(id) if !id.is_empty() => Ok(DeviceKey(id.into())),
            _ => Err(format!("{at}: {s:?} isn't \"device:<id>\"")),
        };
        let mut d = Desired {
            master: Level {
                gain: gain("master_gain", self.master_gain, MAX_GAIN)?,
                muted: self.master_muted,
            },
            next_clip: self.next_clip,
            ..Desired::default()
        };
        d.system_input = match self.system_input.as_deref() {
            None => None,
            Some(s) => Some(match s.strip_prefix("endpoint:") {
                Some(id) => SystemInput::Endpoint(
                    id.parse()
                        .map_err(|_| format!("system_input: {s:?} isn't \"endpoint:<number>\""))?,
                ),
                None => {
                    SystemInput::Device(device_ref("system_input", s).map_err(|e| e + " or \"endpoint:<number>\"")?)
                }
            }),
        };
        d.keep_windows_default = self.keep_windows_default;
        d.outputs = self.outputs.into_iter().map(DeviceKey).collect();
        for (i, dev) in self.devices.into_iter().enumerate() {
            let flow = match dev.flow.as_str() {
                "render" => Flow::Render,
                "capture" => Flow::Capture,
                f => return Err(format!("devices[{i}].flow: {f:?} isn't \"render\" or \"capture\"")),
            };
            let info = DeviceInfo { name: dev.name, flow };
            if d.known_devices.insert(DeviceKey(dev.id.clone()), info).is_some() {
                return Err(format!("devices: {} is listed twice", dev.id));
            }
        }
        let mut seen = BTreeSet::new();
        for (i, app) in self.apps.into_iter().enumerate() {
            let key = AppKey(app.path);
            if !seen.insert(key.clone()) {
                return Err(format!("apps: {} is listed twice", key.0));
            }
            if let Some(name) = app.name {
                d.known_apps.insert(key.clone(), name);
            }
            match app.gain {
                Some(g) => {
                    let level = Level {
                        gain: gain(&format!("apps[{i}]"), g, 1.0)?,
                        muted: app.muted,
                    };
                    d.app_levels.insert(key.clone(), level);
                }
                None if app.muted => return Err(format!("apps[{i}]: muted without a gain")),
                None => {}
            }
            if let Some(outs) = app.outputs {
                d.app_routes
                    .insert(key.clone(), outs.into_iter().map(DeviceKey).collect());
            }
            if let Some(id) = app.endpoint {
                if let Some((other, _)) = d.app_endpoints.iter().find(|(_, e)| **e == id) {
                    return Err(format!("apps[{i}]: endpoint {id} is {}'s already", other.0));
                }
                d.app_endpoints.insert(key, id);
            }
        }
        for (i, c) in self.clips.into_iter().enumerate() {
            let retrigger = match c.retrigger.as_str() {
                "restart" => Retrigger::Restart,
                "overlap" => Retrigger::Overlap,
                "ignore" => Retrigger::Ignore,
                r => return Err(format!("clips[{i}].retrigger: {r:?} isn't restart, overlap or ignore")),
            };
            if c.name.trim().is_empty() || c.path.trim().is_empty() {
                return Err(format!("clips[{i}]: a clip needs a name and a file"));
            }
            let settings = ClipSettings {
                name: c.name,
                gain: gain(&format!("clips[{i}]"), c.gain, MAX_GAIN)?,
                retrigger,
                looped: c.looped,
            };
            let def = ClipDef { path: c.path, settings };
            if d.clips.insert(ClipId(c.id), def).is_some() {
                return Err(format!("clips: id {} is used twice", c.id));
            }
            // Ids are never reused, even if the file was edited by hand.
            d.next_clip = d.next_clip.max(c.id.saturating_add(1));
        }
        for (i, c) in self.matrix.into_iter().enumerate() {
            let at = format!("matrix[{i}]");
            let from = match c.from.as_str() {
                "clips" => Source::Clips,
                s => match s.strip_prefix("app:") {
                    Some(path) if !path.is_empty() => Source::App(AppKey(path.into())),
                    _ => Source::Device(device_ref(&at, s).map_err(|e| e + ", \"app:<path>\" or \"clips\"")?),
                },
            };
            let to = Sink::Device(device_ref(&at, &c.to)?);
            d.matrix.insert((from, to), gain(&at, c.gain, MAX_GAIN)?);
        }
        Ok(d)
    }
}

// --- Reading ------------------------------------------------------------------------------

/// Bring a file of any older format up to [`SCHEMA_VERSION`] with `migrations` (see
/// [`MIGRATIONS`]); returns the format it was in.
pub fn upgrade(table: &mut toml::Table, migrations: &[Migration]) -> Result<u32, String> {
    let current = migrations.len() as u32 + 1;
    let from = match table.get("schema_version") {
        Some(toml::Value::Integer(v)) if *v >= 1 => u32::try_from(*v).unwrap_or(u32::MAX),
        Some(v) => return Err(format!("schema_version {v} isn't a format number")),
        None => return Err("no schema_version".into()),
    };
    if from > current {
        return Err(format!(
            "it's from a newer Soundboard (format {from}; this one reads up to {current})"
        ));
    }
    for (step, m) in migrations.iter().enumerate().skip(from as usize - 1) {
        m(table).map_err(|e| format!("upgrading it from format {}: {e}", step + 1))?;
        table.insert("schema_version".into(), toml::Value::Integer(step as i64 + 2));
    }
    Ok(from)
}

/// The settings in `text`, and the format they were in.
pub fn parse(text: &str) -> Result<(Desired, u32), String> {
    parse_with(text, MIGRATIONS)
}

fn parse_with(text: &str, migrations: &[Migration]) -> Result<(Desired, u32), String> {
    let mut table: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    let from = upgrade(&mut table, migrations)?;
    let file: File = table.try_into().map_err(|e: toml::de::Error| e.to_string())?;
    Ok((file.into_desired()?, from))
}

/// The settings file's text for `desired`.
pub fn render(desired: &Desired) -> String {
    // Plain strings, numbers and arrays of tables: nothing that can fail to serialize.
    let body = toml::to_string(&File::from_desired(desired)).expect("settings serialize");
    format!("{HEADER}{body}")
}

/// What [`load`] found.
#[derive(Debug)]
pub struct Loaded {
    pub desired: Desired,
    /// Why the file couldn't be used (the controller starts from defaults).
    pub error: Option<LoadError>,
    /// Whether saving over the file is safe: false when an unreadable file couldn't be moved
    /// out of the way.
    pub writable: bool,
}

impl Loaded {
    /// A fresh state with these settings, and any trouble shown in the view.
    pub fn state(&self) -> AppState {
        let mut s = AppState {
            desired: self.desired.clone(),
            ..AppState::default()
        };
        s.observed.settings.load_error = self.error.clone();
        if !self.writable {
            s.observed.settings.save_error = Some("not saving, so the unreadable file isn't overwritten".into());
        }
        s
    }
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Read the settings at `path`. A missing file is a first run: defaults, no error.
pub fn load(path: &Path) -> Loaded {
    load_with(path, MIGRATIONS)
}

fn load_with(path: &Path, migrations: &[Migration]) -> Loaded {
    let fresh = |error, writable| Loaded {
        desired: Desired::default(),
        error,
        writable,
    };
    let text = match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).map_err(|_| "it isn't UTF-8 text".to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return fresh(None, true),
        Err(e) => Err(e.to_string()),
    };
    match text.and_then(|t| parse_with(&t, migrations)) {
        Ok((desired, from)) => {
            if from <= migrations.len() as u32 {
                // The next save writes the new format; keep the old one for an older build.
                let _ = std::fs::copy(path, sibling(path, &format!(".v{from}")));
            }
            Loaded {
                desired,
                error: None,
                writable: true,
            }
        }
        Err(reason) => {
            let secs = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let backup = sibling(path, &format!(".bad-{secs}"));
            let moved = std::fs::rename(path, &backup).is_ok();
            let error = LoadError {
                reason,
                backup: moved.then(|| backup.display().to_string()),
            };
            fresh(Some(error), moved)
        }
    }
}

// --- Writing ------------------------------------------------------------------------------

/// Write `desired` to `path` atomically: a temp file beside it, flushed, renamed over it.
pub fn save(path: &Path, desired: &Desired) -> Result<(), String> {
    let text = render(desired);
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = sibling(path, ".tmp");
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {e}", tmp.display()));
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
    // The rename itself reaches the disk with the directory.
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
    Ok(())
}

/// To the persistence worker.
#[derive(Debug)]
pub enum SaveMsg {
    /// The settings changed to these.
    Save(Box<Desired>),
    /// Write what's pending now and exit.
    Stop,
}

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Quiet time after the last change before saving.
    pub debounce: Duration,
    /// Longest a change waits while more keep coming.
    pub max_wait: Duration,
    /// Wait before trying a failed save again.
    pub retry: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(500),
            max_wait: Duration::from_secs(2),
            retry: Duration::from_secs(5),
        }
    }
}

/// Start the persistence worker. Each save's outcome goes to the actor as [`Msg::Saved`].
pub fn spawn(path: PathBuf, timing: Timing, inbox: Inbox) -> (Sender<SaveMsg>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("sb-persist".into())
        .spawn(move || run(&path, timing, &rx, &inbox))
        .expect("spawn the persistence worker");
    (tx, thread)
}

fn run(path: &Path, timing: Timing, rx: &Receiver<SaveMsg>, inbox: &Inbox) {
    // What's waiting to be written, and when it's due.
    let mut pending: Option<(Box<Desired>, Instant)> = None;
    // When the oldest unsaved change came in.
    let mut since: Option<Instant> = None;
    let write = |d: &Desired| {
        let result = save(path, d);
        let ok = result.is_ok();
        let _ = inbox.send(Msg::Saved(result));
        ok
    };
    loop {
        let msg = match &pending {
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some((_, due)) => rx.recv_timeout(due.saturating_duration_since(Instant::now())),
        };
        match msg {
            Ok(SaveMsg::Save(d)) => {
                let now = Instant::now();
                let first = *since.get_or_insert(now);
                pending = Some((d, (now + timing.debounce).min(first + timing.max_wait)));
            }
            Err(RecvTimeoutError::Timeout) => {
                let (d, _) = pending.take().expect("a timeout means something is pending");
                if write(&d) {
                    since = None;
                } else {
                    pending = Some((d, Instant::now() + timing.retry));
                }
            }
            Ok(SaveMsg::Stop) | Err(RecvTimeoutError::Disconnected) => {
                if let Some((d, _)) = pending.take() {
                    write(&d);
                }
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn dev(n: &str) -> DeviceKey {
        DeviceKey(format!("{{0.0.0.00000000}}.{{{n}}}"))
    }

    fn mic() -> DeviceKey {
        DeviceKey("{0.0.1.00000000}.{mic}".into())
    }

    fn game() -> AppKey {
        AppKey(r"C:\Games\game.exe".into())
    }

    /// Something in every field.
    fn full() -> Desired {
        let mut d = Desired {
            system_input: Some(SystemInput::Device(mic())),
            outputs: [dev("speakers"), dev("gone")].into(),
            master: Level { gain: 0.3, muted: true },
            next_clip: 7,
            keep_windows_default: true,
            ..Desired::default()
        };
        d.app_levels.insert(
            game(),
            Level {
                gain: 0.25,
                muted: true,
            },
        );
        // An app with a level but never seen (no name), and one with only a route.
        d.app_levels.insert(AppKey(r"C:\Other\x.exe".into()), Level::UNITY);
        d.known_apps.insert(game(), "Game".into());
        d.known_apps
            .insert(AppKey(r"C:\Spotify\Spotify.exe".into()), "Spotify".into());
        d.app_routes
            .insert(AppKey(r"C:\Spotify\Spotify.exe".into()), [dev("speakers")].into());
        d.app_endpoints.insert(AppKey(r"C:\Spotify\Spotify.exe".into()), 3);
        d.matrix
            .insert((Source::Device(mic()), Sink::Device(dev("speakers"))), 0.8);
        d.matrix
            .insert((Source::App(game()), Sink::Device(dev("speakers"))), 1.5);
        d.matrix.insert((Source::Clips, Sink::Device(dev("gone"))), 0.1);
        d.clips.insert(
            ClipId(2),
            ClipDef {
                path: r"C:\clips\air horn.ogg".into(),
                settings: ClipSettings {
                    name: "Air horn \"loud\"".into(),
                    gain: 0.7,
                    retrigger: Retrigger::Overlap,
                    looped: true,
                },
            },
        );
        d.known_devices.insert(
            dev("speakers"),
            DeviceInfo {
                name: "Speakers".into(),
                flow: Flow::Render,
            },
        );
        d.known_devices.insert(
            mic(),
            DeviceInfo {
                name: "Mic".into(),
                flow: Flow::Capture,
            },
        );
        d
    }

    #[test]
    fn everything_survives_the_round_trip() {
        for d in [Desired::default(), full()] {
            let text = render(&d);
            assert_eq!(parse(&text).unwrap(), (d, SCHEMA_VERSION), "{text}");
        }
        let mut d = full();
        d.system_input = Some(SystemInput::Endpoint(4));
        assert_eq!(parse(&render(&d)).unwrap().0, d);
    }

    #[test]
    fn the_file_reads_like_settings() {
        let text = render(&full());
        println!("{text}");
        assert!(text.starts_with("# Soundboard settings"));
        for line in [
            "schema_version = 1",
            "master_gain = 0.3",
            "system_input = \"device:{0.0.1.00000000}.{mic}\"",
            "[[apps]]",
            "name = \"Game\"",
            "gain = 0.25",
            "endpoint = 3",
            "retrigger = \"overlap\"",
            "from = \"clips\"",
        ] {
            assert!(text.lines().any(|l| l == line), "no line {line:?} in\n{text}");
        }
        // A fresh install writes little.
        let empty = render(&Desired::default());
        assert!(!empty.contains("[["), "{empty}");
    }

    #[test]
    fn a_hand_written_file_with_only_some_fields_reads() {
        let (d, _) = parse(
            r#"
            schema_version = 1
            outputs = ["{0.0.0.00000000}.{speakers}"]

            [[clips]]
            id = 4
            path = 'C:\clips\a.ogg'
            name = "A"
            "#,
        )
        .unwrap();
        assert_eq!(d.master, Level::UNITY);
        assert_eq!(d.outputs, [dev("speakers")].into());
        assert_eq!(d.clips[&ClipId(4)].settings.retrigger, Retrigger::Restart);
        assert_eq!(d.next_clip, 5, "ids are never reused, even when next_clip is left out");
    }

    #[test]
    fn bad_files_say_where() {
        let cases = [
            ("schema_version = 1\nmaster_gain = 9", "master_gain: gain 9"),
            ("master_gain = 1", "no schema_version"),
            ("schema_version = \"one\"", "isn't a format number"),
            (
                "schema_version = 99",
                "newer Soundboard (format 99; this one reads up to 1)",
            ),
            (
                "schema_version = 1\n[[apps]]\npath = 'a'\ngain = 1.5",
                "apps[0]: gain 1.5 is outside 0 to 1",
            ),
            (
                "schema_version = 1\n[[apps]]\npath = 'a'\n[[apps]]\npath = 'a'",
                "a is listed twice",
            ),
            (
                "schema_version = 1\n[[apps]]\npath = 'a'\nendpoint = 1\n[[apps]]\npath = 'b'\nendpoint = 1",
                "endpoint 1 is a's already",
            ),
            (
                "schema_version = 1\n[[devices]]\nid = 'x'\nname = 'X'\nflow = 'sideways'",
                "devices[0].flow",
            ),
            (
                "schema_version = 1\nsystem_input = 'mic'",
                "system_input: \"mic\" isn't",
            ),
            ("schema_version = 1\nsystem_input = 'endpoint:x'", "endpoint:<number>"),
            (
                "schema_version = 1\n[[matrix]]\nfrom = 'app:'\nto = 'device:x'\ngain = 1",
                "matrix[0]",
            ),
            (
                "schema_version = 1\n[[matrix]]\nfrom = 'clips'\nto = 'x'\ngain = 1",
                "matrix[0]",
            ),
            (
                "schema_version = 1\n[[clips]]\nid = 1\npath = 'p'\nname = ' '",
                "clips[0]: a clip needs",
            ),
            (
                "schema_version = 1\n[[clips]]\nid = 1\npath = 'p'\nname = 'n'\nretrigger = 'twice'",
                "retrigger",
            ),
            ("schema_version = 1\noutputs = 3", "outputs"),
            (
                "schema_version = 1\n[[apps]]\npath = 'a'\nmuted = true",
                "muted without a gain",
            ),
            ("not toml at all", "expected"),
        ];
        for (text, want) in cases {
            let e = parse(text).unwrap_err();
            assert!(e.contains(want), "{text:?}: {e:?} lacks {want:?}");
        }
    }

    #[test]
    fn older_formats_are_upgraded_step_by_step() {
        // Pretend format 3 is current: 1 called the master "volume", 2 kept it in percent.
        fn rename(t: &mut toml::Table) -> Result<(), String> {
            let v = t.remove("volume").ok_or("no volume")?;
            t.insert("master_percent".into(), v);
            Ok(())
        }
        fn percent(t: &mut toml::Table) -> Result<(), String> {
            let p = t
                .remove("master_percent")
                .and_then(|v| v.as_integer())
                .ok_or("no master_percent")?;
            t.insert("master_gain".into(), toml::Value::Float(p as f64 / 100.0));
            Ok(())
        }
        let steps: &[Migration] = &[rename, percent];
        let v1 = "schema_version = 1\nvolume = 50\n";
        let (d, from) = parse_with(v1, steps).unwrap();
        assert_eq!((d.master.gain, from), (0.5, 1));
        let (d, from) = parse_with("schema_version = 2\nmaster_percent = 25\n", steps).unwrap();
        assert_eq!((d.master.gain, from), (0.25, 2));
        // The current format goes through untouched; a broken old file says which step.
        assert_eq!(parse_with("schema_version = 3\nmaster_gain = 2.0", steps).unwrap().1, 3);
        let e = parse_with("schema_version = 1\n", steps).unwrap_err();
        assert_eq!(e, "upgrading it from format 1: no volume");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sb-persist-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("nested").join("config.toml")
    }

    fn files(path: &Path) -> BTreeMap<String, String> {
        std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let text = std::fs::read_to_string(e.path()).unwrap_or_default();
                (e.file_name().to_string_lossy().into_owned(), text)
            })
            .collect()
    }

    #[test]
    fn saving_creates_the_folder_and_leaves_no_temp_file() {
        let path = scratch("save");
        assert!(load(&path).error.is_none(), "a missing file is a first run");
        save(&path, &full()).unwrap();
        save(&path, &Desired::default()).unwrap();
        assert_eq!(files(&path).keys().collect::<Vec<_>>(), ["config.toml"]);
        let loaded = load(&path);
        assert_eq!(
            (loaded.desired, loaded.error, loaded.writable),
            (Desired::default(), None, true)
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn an_unreadable_file_is_moved_aside_and_defaults_used() {
        let path = scratch("bad");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "schema_version = 1\nmaster_gain = -1\n").unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.desired, Desired::default());
        assert!(loaded.writable);
        let e = loaded.error.clone().unwrap();
        assert!(e.reason.contains("master_gain"), "{e:?}");
        let backup = PathBuf::from(e.backup.clone().unwrap());
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            "schema_version = 1\nmaster_gain = -1\n",
            "kept as it was"
        );
        assert!(!path.exists());
        let state = loaded.state();
        assert_eq!(state.observed.settings.load_error, Some(e));
        assert_eq!(state.observed.settings.save_error, None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn an_upgraded_file_keeps_its_old_self() {
        let path = scratch("upgrade");
        save(&path, &full()).unwrap();
        let loaded = load(&path);
        assert!(loaded.error.is_none());
        assert_eq!(files(&path).len(), 1, "the current format isn't copied");
        // Pretend a format 2 exists, and this file is format 1.
        fn noop(_: &mut toml::Table) -> Result<(), String> {
            Ok(())
        }
        let loaded = load_with(&path, &[noop]);
        assert_eq!(loaded.desired, full());
        let names = files(&path);
        assert_eq!(names.keys().collect::<Vec<_>>(), ["config.toml", "config.toml.v1"]);
        assert_eq!(names["config.toml"], names["config.toml.v1"]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    fn fast() -> Timing {
        Timing {
            debounce: Duration::from_millis(100),
            max_wait: Duration::from_millis(400),
            retry: Duration::from_millis(100),
        }
    }

    fn with_master(g: f32) -> Box<Desired> {
        Box::new(Desired {
            master: Level { gain: g, muted: false },
            ..Desired::default()
        })
    }

    fn saves(rx: &Receiver<Msg>, within: Duration) -> Vec<Result<(), String>> {
        let deadline = Instant::now() + within;
        let mut out = Vec::new();
        while let Ok(m) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            if let Msg::Saved(r) = m {
                out.push(r);
            }
        }
        out
    }

    #[test]
    fn a_burst_of_changes_is_one_save_of_the_last() {
        let path = scratch("burst");
        let (inbox, rx) = mpsc::channel();
        let (tx, thread) = spawn(path.clone(), fast(), inbox);
        for i in 0..10 {
            tx.send(SaveMsg::Save(with_master(i as f32 / 10.0))).unwrap();
        }
        assert_eq!(saves(&rx, Duration::from_millis(400)), [Ok(())]);
        assert_eq!(load(&path).desired.master.gain, 0.9);
        tx.send(SaveMsg::Stop).unwrap();
        thread.join().unwrap();
        assert!(rx.try_recv().is_err(), "nothing pending, nothing written at the end");
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_long_drag_is_still_saved_now_and_then() {
        let path = scratch("drag");
        let (inbox, rx) = mpsc::channel();
        let (tx, thread) = spawn(path.clone(), fast(), inbox);
        let start = Instant::now();
        let mut i = 0;
        // A change every 50 ms for a second never goes quiet for 100 ms.
        while start.elapsed() < Duration::from_secs(1) {
            tx.send(SaveMsg::Save(with_master((i % 20) as f32 / 10.0))).unwrap();
            i += 1;
            std::thread::sleep(Duration::from_millis(50));
        }
        let during = saves(&rx, Duration::ZERO).len();
        assert!((2..=3).contains(&during), "every 400 ms or so: {during}");
        tx.send(SaveMsg::Stop).unwrap();
        thread.join().unwrap();
        assert_eq!(saves(&rx, Duration::ZERO), [Ok(())], "the last one, on the way out");
        assert_eq!(load(&path).desired.master.gain, ((i - 1) % 20) as f32 / 10.0);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_failed_save_is_reported_and_tried_again() {
        let path = scratch("fail");
        // A directory where the file should be: the rename can't replace it.
        std::fs::create_dir_all(&path).unwrap();
        let (inbox, rx) = mpsc::channel();
        let (tx, thread) = spawn(path.clone(), fast(), inbox);
        tx.send(SaveMsg::Save(with_master(0.5))).unwrap();
        let first = saves(&rx, Duration::from_millis(150));
        assert!(matches!(&first[..], [Err(_)]), "{first:?}");
        std::fs::remove_dir(&path).unwrap();
        assert_eq!(
            saves(&rx, Duration::from_millis(200)),
            [Ok(())],
            "tried again by itself"
        );
        assert_eq!(load(&path).desired.master.gain, 0.5);
        tx.send(SaveMsg::Stop).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }
}
