//! The COM thread: every Core Audio call the controller makes, on one MTA thread.
//!
//! It watches the active audio endpoints (`IMMNotificationClient`) and the audio sessions on
//! every render device (`IAudioSessionNotification`, plus `IAudioSessionEvents` per session
//! for volume and state changes), and sets app levels (`ISimpleAudioVolume`).
//!
//! Callbacks run on Windows' threads and only post [`WinMsg::Rescan`] here; Windows forbids
//! blocking in them, and a rescan is cheap. The thread then scans and reports the difference
//! to the actor as one [`Msg::ObservedAll`] ([`crate::watch::changes`]). It also rescans
//! sessions every second, because an app that exits doesn't always say so, and devices every
//! five. COM objects never leave this thread; only plain data does.
//!
//! `SB_WATCH_TRACE=1` prints every scan that changed something and every level set.

use crate::actor::{Inbox, Msg};
use crate::watch::{self, Levels, SeenDevice, SeenSession, WinMsg, World};
use sb_shared::ui::{AppKey, DeviceKey, Flow, Level};
use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};
use windows::Win32::Devices::FunctionDiscovery::{PKEY_Device_FriendlyName, PKEY_DeviceInterface_FriendlyName};
use windows::Win32::Foundation::{CloseHandle, PROPERTYKEY, S_OK};
use windows::Win32::Media::Audio::*;
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, STGM_READ,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::{BOOL, GUID, HSTRING, Interface, PCWSTR, PWSTR, Ref, Result, implement};

const SESSIONS_EVERY: Duration = Duration::from_secs(1);
const DEVICES_EVERY: Duration = Duration::from_secs(5);

/// Marks volume changes the controller makes, so their callbacks can be skipped.
const OUR_CHANGE: GUID = GUID::from_u128(0x5b0a_7d1e_3c6f_4e28_9a41_c0de_50b0_a4d1);

/// Start the COM thread. It reports to `inbox` until it gets [`WinMsg::Stop`].
pub fn spawn(inbox: Inbox) -> (Sender<WinMsg>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let wake = tx.clone();
    let thread = std::thread::Builder::new()
        .name("sb-com".into())
        .spawn(move || {
            if let Err(e) = run(rx, wake, inbox) {
                eprintln!("windows watcher stopped: {e}");
            }
        })
        .expect("spawn the COM thread");
    (tx, thread)
}

struct Com;

impl Com {
    fn init() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        Ok(Com)
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}

fn run(rx: Receiver<WinMsg>, wake: Sender<WinMsg>, inbox: Inbox) -> Result<()> {
    let _com = Com::init()?;
    let en: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let client: IMMNotificationClient = DeviceEvents { wake: wake.clone() }.into();
    unsafe { en.RegisterEndpointNotificationCallback(&client)? };
    let mut w = Watcher {
        en,
        wake,
        inbox,
        managers: BTreeMap::new(),
        sessions: BTreeMap::new(),
        apps: HashMap::new(),
        levels: Levels::default(),
        trace: std::env::var_os("SB_WATCH_TRACE").is_some(),
        world: World::default(),
    };
    w.scan(true);
    let mut last_devices = Instant::now();
    loop {
        let mut msgs = Vec::new();
        let polled = match rx.recv_timeout(SESSIONS_EVERY) {
            Ok(m) => {
                msgs.push(m);
                false
            }
            Err(RecvTimeoutError::Timeout) => true,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        msgs.extend(rx.try_iter());
        let (mut devices, mut sessions, mut stop) = (false, polled, false);
        for m in msgs {
            match m {
                WinMsg::SetAppLevel { app, level } => w.set_level(app, level),
                WinMsg::Rescan {
                    devices: d,
                    sessions: s,
                } => {
                    devices |= d;
                    sessions |= s;
                }
                WinMsg::Stop => stop = true,
            }
        }
        if stop {
            break;
        }
        if last_devices.elapsed() >= DEVICES_EVERY {
            devices = true;
        }
        if devices {
            last_devices = Instant::now();
        }
        if devices || sessions {
            w.scan(devices);
        }
    }
    w.shutdown();
    unsafe { w.en.UnregisterEndpointNotificationCallback(&client)? };
    Ok(())
}

/// A render device's session manager, and our notification on it.
struct Manager {
    mgr: IAudioSessionManager2,
    created: IAudioSessionNotification,
}

/// A session being watched.
struct Tracked {
    control: IAudioSessionControl,
    volume: ISimpleAudioVolume,
    events: IAudioSessionEvents,
    app: AppKey,
}

struct Watcher {
    en: IMMDeviceEnumerator,
    wake: Sender<WinMsg>,
    inbox: Inbox,
    managers: BTreeMap<DeviceKey, Manager>,
    /// By session instance identifier.
    sessions: BTreeMap<String, Tracked>,
    /// Each process's app (key and name), or `None` if it can't be identified. Kept for the
    /// processes seen in the last scan only (PIDs get reused).
    apps: HashMap<u32, Option<(AppKey, String)>>,
    levels: Levels,
    trace: bool,
    /// As last reported.
    world: World,
}

impl Watcher {
    /// Scan (devices too if `devices`), and report what changed.
    fn scan(&mut self, devices: bool) {
        let mut new = self.world.clone();
        if devices {
            match self.devices() {
                Ok(d) => {
                    self.sync_managers(&d);
                    new.devices = d;
                }
                Err(e) => eprintln!("windows watcher: listing devices: {e}"),
            }
        }
        new.sessions = self.sessions();
        if self.trace && new.sessions != self.world.sessions {
            for (id, s) in &new.sessions {
                let tail = id.get(id.len().saturating_sub(12)..).unwrap_or(id);
                eprintln!(
                    "watch: session …{tail} pid {} {:.3}{}",
                    s.pid,
                    s.level.gain,
                    if s.level.muted { " muted" } else { "" }
                );
            }
        }
        let observed = watch::changes(&self.world, &new, &mut self.levels);
        if self.trace && !observed.is_empty() {
            eprintln!("watch: {observed:?}");
        }
        self.world = new;
        if !observed.is_empty() {
            let _ = self.inbox.send(Msg::ObservedAll(observed));
        }
    }

    fn devices(&self) -> Result<BTreeMap<DeviceKey, SeenDevice>> {
        let mut out = BTreeMap::new();
        let all = unsafe { self.en.EnumAudioEndpoints(eAll, DEVICE_STATE_ACTIVE)? };
        for i in 0..unsafe { all.GetCount()? } {
            // One odd device mustn't hide the others.
            let seen = (|| -> Result<(DeviceKey, SeenDevice)> {
                let device = unsafe { all.Item(i)? };
                let id = take_string(unsafe { device.GetId()? });
                let flow = match unsafe { device.cast::<IMMEndpoint>()?.GetDataFlow()? } {
                    f if f == eCapture => Flow::Capture,
                    _ => Flow::Render,
                };
                let store = unsafe { device.OpenPropertyStore(STGM_READ)? };
                let prop = |key: &PROPERTYKEY| unsafe { store.GetValue(key) }.map(|v| v.to_string());
                Ok((
                    DeviceKey(id),
                    SeenDevice {
                        name: prop(&PKEY_Device_FriendlyName)?,
                        flow,
                        adapter: prop(&PKEY_DeviceInterface_FriendlyName).unwrap_or_default(),
                    },
                ))
            })();
            if let Ok((k, d)) = seen {
                out.insert(k, d);
            }
        }
        Ok(out)
    }

    /// A session manager per render device, each with our notification on it.
    fn sync_managers(&mut self, devices: &BTreeMap<DeviceKey, SeenDevice>) {
        let gone: Vec<DeviceKey> = self
            .managers
            .keys()
            .filter(|k| !devices.contains_key(*k))
            .cloned()
            .collect();
        for k in gone {
            if let Some(m) = self.managers.remove(&k) {
                let _ = unsafe { m.mgr.UnregisterSessionNotification(&m.created) };
            }
        }
        for (key, d) in devices {
            if d.flow != Flow::Render || self.managers.contains_key(key) {
                continue;
            }
            let made = (|| -> Result<Manager> {
                let device = unsafe { self.en.GetDevice(&HSTRING::from(key.0.as_str()))? };
                let mgr: IAudioSessionManager2 = unsafe { device.Activate(CLSCTX_ALL, None)? };
                let created: IAudioSessionNotification = SessionCreated {
                    wake: self.wake.clone(),
                }
                .into();
                unsafe { mgr.RegisterSessionNotification(&created)? };
                // Notifications only start once the session list has been asked for.
                let _ = unsafe { mgr.GetSessionEnumerator() };
                Ok(Manager { mgr, created })
            })();
            match made {
                Ok(m) => {
                    self.managers.insert(key.clone(), m);
                }
                Err(e) => eprintln!("windows watcher: sessions of {}: {e}", d.name),
            }
        }
    }

    /// Every live app session on every render device. New ones get our events (and the
    /// level the controller wants for their app); ended ones lose them.
    fn sessions(&mut self) -> BTreeMap<String, SeenSession> {
        let mut out = BTreeMap::new();
        let mut live: HashMap<String, (IAudioSessionControl, IAudioSessionControl2)> = HashMap::new();
        for m in self.managers.values() {
            let Ok(list) = (unsafe { m.mgr.GetSessionEnumerator() }) else {
                continue;
            };
            for i in 0..unsafe { list.GetCount() }.unwrap_or(0) {
                let Ok(control) = (unsafe { list.GetSession(i) }) else {
                    continue;
                };
                let Ok(c2) = control.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                if unsafe { c2.IsSystemSoundsSession() } == S_OK {
                    continue;
                }
                if unsafe { control.GetState() }.is_ok_and(|s| s == AudioSessionStateExpired) {
                    continue;
                }
                let Ok(id) = (unsafe { c2.GetSessionInstanceIdentifier() }).map(take_string) else {
                    continue;
                };
                live.insert(id, (control, c2));
            }
        }
        let pids: HashMap<String, u32> = live
            .iter()
            .filter_map(|(id, (_, c2))| unsafe { c2.GetProcessId() }.ok().map(|p| (id.clone(), p)))
            .collect();
        self.apps.retain(|pid, _| pids.values().any(|p| p == pid));

        let ended: Vec<String> = self
            .sessions
            .keys()
            .filter(|k| !live.contains_key(*k))
            .cloned()
            .collect();
        for id in ended {
            if let Some(t) = self.sessions.remove(&id) {
                let _ = unsafe { t.control.UnregisterAudioSessionNotification(&t.events) };
            }
        }
        for (id, (control, c2)) in live {
            let Some(&pid) = pids.get(&id).filter(|p| **p != 0) else {
                continue;
            };
            let Some((app, exe_name)) = self.app_of(pid, &c2) else {
                continue;
            };
            if !self.sessions.contains_key(&id) {
                let Ok(volume) = control.cast::<ISimpleAudioVolume>() else {
                    continue;
                };
                let events: IAudioSessionEvents = SessionEvents {
                    wake: self.wake.clone(),
                }
                .into();
                if unsafe { control.RegisterAudioSessionNotification(&events) }.is_err() {
                    continue;
                }
                if let Some(l) = self.levels.wanted(&app) {
                    set_volume(&volume, l);
                }
                let tracked = Tracked {
                    control: control.clone(),
                    volume,
                    events,
                    app: app.clone(),
                };
                self.sessions.insert(id.clone(), tracked);
            }
            let t = &self.sessions[&id];
            let level = Level {
                gain: unsafe { t.volume.GetMasterVolume() }.unwrap_or(1.0),
                muted: unsafe { t.volume.GetMute() }.is_ok_and(BOOL::as_bool),
            };
            let display = unsafe { control.GetDisplayName() }.map(take_string).unwrap_or_default();
            let name = watch::usable_display_name(&display).map_or(exe_name, str::to_owned);
            out.insert(id, SeenSession { app, name, pid, level });
        }
        out
    }

    /// The app behind a process: its executable path and a name for it.
    fn app_of(&mut self, pid: u32, c2: &IAudioSessionControl2) -> Option<(AppKey, String)> {
        self.apps
            .entry(pid)
            .or_insert_with(|| {
                let path = process_path(pid).or_else(|| {
                    let id = unsafe { c2.GetSessionIdentifier() }.map(take_string).ok()?;
                    watch::session_exe(&id).map(str::to_owned)
                })?;
                if watch::is_ours(&path) {
                    return None;
                }
                let name = exe_description(&path).unwrap_or_else(|| watch::exe_name(&path).to_owned());
                Some((AppKey(path), name))
            })
            .clone()
    }

    fn set_level(&mut self, app: AppKey, level: Level) {
        for (id, t) in self.sessions.iter().filter(|(_, t)| t.app == app) {
            set_volume(&t.volume, level);
            // What we set is now the last known level: a change *back* to the old one (the
            // Windows mixer, before the next scan) must still count as a change.
            if let Some(seen) = self.world.sessions.get_mut(id) {
                seen.level = Level {
                    gain: level.gain.clamp(0.0, 1.0),
                    ..level
                };
            }
        }
        if self.trace {
            let n = self.sessions.values().filter(|t| t.app == app).count();
            eprintln!("watch: set {} to {level:?} on {n} session(s)", app.0);
        }
        self.levels.set(app, level);
    }

    fn shutdown(&mut self) {
        for (_, t) in std::mem::take(&mut self.sessions) {
            let _ = unsafe { t.control.UnregisterAudioSessionNotification(&t.events) };
        }
        for (_, m) in std::mem::take(&mut self.managers) {
            let _ = unsafe { m.mgr.UnregisterSessionNotification(&m.created) };
        }
    }
}

fn set_volume(v: &ISimpleAudioVolume, level: Level) {
    unsafe {
        let _ = v.SetMasterVolume(level.gain.clamp(0.0, 1.0), &OUR_CHANGE);
        let _ = v.SetMute(level.muted, &OUR_CHANGE);
    }
}

/// A COM-allocated string, freed.
fn take_string(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let s = unsafe { p.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

fn process_path(pid: u32) -> Option<String> {
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) };
    let _ = unsafe { CloseHandle(h) };
    ok.ok()?;
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// The "File description" in an executable's version resource ("Google Chrome" for
/// chrome.exe), the name Windows' own mixer shows.
fn exe_description(path: &str) -> Option<String> {
    let file = HSTRING::from(path);
    let size = unsafe { GetFileVersionInfoSizeW(&file, None) };
    if size == 0 {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    unsafe { GetFileVersionInfoW(&file, None, size, data.as_mut_ptr().cast()) }.ok()?;
    let query = |sub: &str| -> Option<(*const u8, u32)> {
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        let found = unsafe { VerQueryValueW(data.as_ptr().cast(), &HSTRING::from(sub), &mut ptr, &mut len) };
        (found.as_bool() && !ptr.is_null() && len > 0).then_some((ptr as *const u8, len))
    };
    let (t, len) = query(r"\VarFileInfo\Translation")?;
    if len < 4 {
        return None;
    }
    let (lang, page) = unsafe { (*(t as *const u16), *(t as *const u16).add(1)) };
    let (s, chars) = query(&format!(r"\StringFileInfo\{lang:04x}{page:04x}\FileDescription"))?;
    let text = unsafe { std::slice::from_raw_parts(s as *const u16, chars as usize) };
    let text = String::from_utf16_lossy(text);
    let text = text.trim_end_matches('\0').trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[implement(IMMNotificationClient)]
struct DeviceEvents {
    wake: Sender<WinMsg>,
}

impl DeviceEvents {
    fn rescan(&self) {
        let _ = self.wake.send(WinMsg::Rescan {
            devices: true,
            sessions: true,
        });
    }
}

impl IMMNotificationClient_Impl for DeviceEvents_Impl {
    fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> Result<()> {
        self.rescan();
        Ok(())
    }
    fn OnDeviceAdded(&self, _: &PCWSTR) -> Result<()> {
        self.rescan();
        Ok(())
    }
    fn OnDeviceRemoved(&self, _: &PCWSTR) -> Result<()> {
        self.rescan();
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, _: EDataFlow, _: ERole, _: &PCWSTR) -> Result<()> {
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _: &PCWSTR, key: &PROPERTYKEY) -> Result<()> {
        // Many properties change all the time; only names matter here.
        if *key == PKEY_Device_FriendlyName || *key == PKEY_DeviceInterface_FriendlyName {
            self.rescan();
        }
        Ok(())
    }
}

#[implement(IAudioSessionNotification)]
struct SessionCreated {
    wake: Sender<WinMsg>,
}

impl IAudioSessionNotification_Impl for SessionCreated_Impl {
    fn OnSessionCreated(&self, _: Ref<IAudioSessionControl>) -> Result<()> {
        let _ = self.wake.send(WinMsg::Rescan {
            devices: false,
            sessions: true,
        });
        Ok(())
    }
}

#[implement(IAudioSessionEvents)]
struct SessionEvents {
    wake: Sender<WinMsg>,
}

impl SessionEvents {
    fn rescan(&self) {
        let _ = self.wake.send(WinMsg::Rescan {
            devices: false,
            sessions: true,
        });
    }
}

impl IAudioSessionEvents_Impl for SessionEvents_Impl {
    fn OnDisplayNameChanged(&self, _: &PCWSTR, _: *const GUID) -> Result<()> {
        self.rescan();
        Ok(())
    }
    fn OnIconPathChanged(&self, _: &PCWSTR, _: *const GUID) -> Result<()> {
        Ok(())
    }
    fn OnSimpleVolumeChanged(&self, _: f32, _: BOOL, context: *const GUID) -> Result<()> {
        if context.is_null() || unsafe { *context } != OUR_CHANGE {
            self.rescan();
        }
        Ok(())
    }
    fn OnChannelVolumeChanged(&self, _: u32, _: *const f32, _: u32, _: *const GUID) -> Result<()> {
        Ok(())
    }
    fn OnGroupingParamChanged(&self, _: *const GUID, _: *const GUID) -> Result<()> {
        Ok(())
    }
    fn OnStateChanged(&self, _: AudioSessionState) -> Result<()> {
        self.rescan();
        Ok(())
    }
    fn OnSessionDisconnected(&self, _: AudioSessionDisconnectReason) -> Result<()> {
        self.rescan();
        Ok(())
    }
}
