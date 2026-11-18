//! The driver's control plane: the virtual endpoint pool, through the driver's control device
//! (`\\.\SoundboardControl`, see `sb_ring::control::CONTROL_PATH`).
//!
//! - [`ControlClient`] speaks the ABI in `sb_ring::control` over any [`Transport`].
//! - [`FakeDriver`] is a transport that *is* a driver: it parses and validates requests the
//!   way the kernel side must (D7), keeps a pool, and makes up Windows endpoint IDs. Tests and
//!   `--fake-driver` use it, so the encoding is exercised before the driver exists.
//! - `DeviceTransport` (Windows) opens the control device by path and calls `DeviceIoControl`.
//! - [`spawn`] runs a client on a worker thread: IOCTLs can block, the actor never waits.
//!   Each job reports the pool as it is afterwards (observations), then its result.
//!
//! The control device has no device interface, so nothing announces the driver arriving.
//! The worker keeps **retrying the open** with backoff while the driver is missing, and while
//! connected it **checks on the driver** every few seconds (`GET_INFO`). Any failed check means
//! the driver is going away (it purges its queues on removal): the worker closes its handle at
//! once, since the control device is exclusive and the old driver can't unload while we hold
//! it, reports the pool gone, and goes back to retrying. A reinstalled driver starts with an
//! empty pool, which routing re-enables.

use crate::actor::{Inbox, Msg};
use crate::state::{DriverStatus, Observation, PoolEndpoint};
use sb_ring::control::{self, DriverInfo, EndpointInfo, SetEndpoint, SetFallback, TEXT};
use sb_ring::ioctl::Direction;
use sb_shared::ui::{DeviceKey, Flow};
use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One `DeviceIoControl`. Returns how many bytes of `output` were written.
pub trait Transport: Send {
    fn ioctl(&mut self, code: u32, input: &[u8], output: &mut [u8]) -> Result<usize, String>;
}

/// An endpoint of the pool, as the controller sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub id: u32,
    /// `Render`: apps play into it (the engine reads it). `Capture`: a virtual mic.
    pub flow: Flow,
    pub enabled: bool,
    pub loopback_fallback: bool,
    pub name: String,
    /// Its Windows endpoint while enabled.
    pub device: Option<DeviceKey>,
}

/// The bytes of a `repr(C)` record (no padding in the ABI's records).
fn bytes_of<T: Copy>(v: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts((v as *const T).cast::<u8>(), size_of::<T>()) }
}

/// A `repr(C)` record from bytes, if there are enough. Every bit pattern is a valid record
/// (integers and integer arrays only); the fields are checked by whoever reads them.
fn read<T: Copy>(b: &[u8]) -> Option<T> {
    (b.len() >= size_of::<T>()).then(|| unsafe { std::ptr::read_unaligned(b.as_ptr().cast::<T>()) })
}

fn text_of(t: &[u16; TEXT]) -> String {
    String::from_utf16_lossy(control::text(t))
}

pub struct ControlClient {
    t: Box<dyn Transport>,
    pub info: DriverInfo,
}

impl ControlClient {
    /// Check the driver speaks this ABI.
    pub fn open(mut t: Box<dyn Transport>) -> Result<Self, String> {
        let mut out = [0u8; size_of::<DriverInfo>()];
        let n = t.ioctl(control::IOCTL_SB_GET_INFO, &[], &mut out)?;
        let info: DriverInfo = read(&out[..n]).ok_or("short GET_INFO reply")?;
        if info.abi_version != control::CONTROL_ABI {
            return Err(format!(
                "the driver speaks control ABI {}, this controller {}",
                info.abi_version,
                control::CONTROL_ABI
            ));
        }
        Ok(Self { t, info })
    }

    /// Is the driver still there (and still the one we checked)?
    pub fn ping(&mut self) -> Result<(), String> {
        let mut out = [0u8; size_of::<DriverInfo>()];
        let n = self.t.ioctl(control::IOCTL_SB_GET_INFO, &[], &mut out)?;
        let info: DriverInfo = read(&out[..n]).ok_or("short GET_INFO reply")?;
        if info != self.info {
            return Err("the driver changed under us".into());
        }
        Ok(())
    }

    pub fn list(&mut self) -> Result<Vec<Endpoint>, String> {
        let mut out = vec![0u8; control::MAX_ENDPOINTS * size_of::<EndpointInfo>()];
        let n = self.t.ioctl(control::IOCTL_SB_LIST_ENDPOINTS, &[], &mut out)?;
        let records = out[..n.min(out.len())].chunks_exact(size_of::<EndpointInfo>());
        Ok(records
            .filter_map(read::<EndpointInfo>)
            .filter_map(|e| {
                let flow = match e.direction {
                    d if d == Direction::FromApps as u32 => Flow::Render,
                    d if d == Direction::ToApps as u32 => Flow::Capture,
                    _ => return None,
                };
                let enabled = e.state == control::STATE_ENABLED;
                let id = text_of(&e.windows_id);
                Some(Endpoint {
                    id: e.id,
                    flow,
                    enabled,
                    loopback_fallback: e.fallback == control::FALLBACK_LOOPBACK,
                    name: text_of(&e.name),
                    device: (enabled && !id.is_empty()).then_some(DeviceKey(id)),
                })
            })
            .collect())
    }

    /// Enable (named `name`; empty keeps the name) or disable an endpoint.
    pub fn set_endpoint(&mut self, id: u32, enabled: bool, name: &str) -> Result<(), String> {
        let mut req = SetEndpoint {
            id,
            state: if enabled {
                control::STATE_ENABLED
            } else {
                control::STATE_DISABLED
            },
            name: [0; TEXT],
        };
        // Names longer than Windows shows anyway are cut, not refused.
        let short: String = name.chars().take(TEXT / 2).collect();
        control::set_text(&mut req.name, &short);
        self.t
            .ioctl(control::IOCTL_SB_SET_ENDPOINT, bytes_of(&req), &mut [])
            .map(drop)
    }

    pub fn set_fallback(&mut self, id: u32, loopback: bool) -> Result<(), String> {
        let req = SetFallback {
            id,
            fallback: if loopback {
                control::FALLBACK_LOOPBACK
            } else {
                control::FALLBACK_SILENCE
            },
        };
        self.t
            .ioctl(control::IOCTL_SB_SET_FALLBACK, bytes_of(&req), &mut [])
            .map(drop)
    }
}

/// A driver in user mode: the pool, behind the same ABI. Render endpoints 1..=`render`, then
/// capture endpoints.
#[derive(Clone)]
pub struct FakeDriver {
    inner: Arc<Mutex<FakePool>>,
}

struct FakePool {
    endpoints: BTreeMap<u32, EndpointInfo>,
    /// Fail every `SET_ENDPOINT` with this (tests).
    fail_set: Option<String>,
    /// Removed (tests): every request fails, like a driver purging its queues on the way out.
    removed: bool,
    /// The pool as a freshly loaded driver has it.
    fresh: BTreeMap<u32, EndpointInfo>,
}

impl FakeDriver {
    pub fn new(render: u32, capture: u32) -> Self {
        let mut endpoints = BTreeMap::new();
        for id in 1..=render + capture {
            let direction = if id <= render {
                Direction::FromApps
            } else {
                Direction::ToApps
            };
            let mut e = EndpointInfo {
                id,
                direction: direction as u32,
                state: control::STATE_DISABLED,
                fallback: control::FALLBACK_SILENCE,
                name: [0; TEXT],
                windows_id: [0; TEXT],
            };
            control::set_text(&mut e.name, &format!("Soundboard {id}"));
            endpoints.insert(id, e);
        }
        Self {
            inner: Arc::new(Mutex::new(FakePool {
                fresh: endpoints.clone(),
                endpoints,
                fail_set: None,
                removed: false,
            })),
        }
    }

    /// Take the driver away (`true`): every request fails until it's back. Coming back is a
    /// fresh load: the pool is as `new` made it (the real driver forgets everything).
    pub fn set_removed(&self, removed: bool) {
        let mut pool = self.lock();
        if pool.removed && !removed {
            pool.endpoints = pool.fresh.clone();
        }
        pool.removed = removed;
    }

    pub fn is_removed(&self) -> bool {
        self.lock().removed
    }

    /// Make `SET_ENDPOINT` fail (or succeed again with `None`).
    pub fn fail_set_endpoint(&self, reason: Option<&str>) {
        self.lock().fail_set = reason.map(str::to_owned);
    }

    /// The Windows endpoint ID the fake gives an enabled endpoint.
    pub fn windows_id(id: u32, flow: Flow) -> DeviceKey {
        let f = (flow == Flow::Capture) as u8;
        DeviceKey(format!(
            "{{0.0.{f}.00000000}}.{{5b0a{id:04x}-0000-4000-8000-50756e64626f}}"
        ))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakePool> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Transport for FakeDriver {
    fn ioctl(&mut self, code: u32, input: &[u8], output: &mut [u8]) -> Result<usize, String> {
        let mut pool = self.lock();
        if pool.removed {
            return Err("the device has been removed".into());
        }
        match code {
            control::IOCTL_SB_GET_INFO => {
                let info = DriverInfo {
                    abi_version: control::CONTROL_ABI,
                    driver_version: 0,
                    endpoints: pool.endpoints.len() as u32,
                    _reserved: 0,
                };
                let b = bytes_of(&info);
                if output.len() < b.len() {
                    return Err("buffer too small".into());
                }
                output[..b.len()].copy_from_slice(b);
                Ok(b.len())
            }
            control::IOCTL_SB_LIST_ENDPOINTS => {
                let size = size_of::<EndpointInfo>();
                let mut n = 0;
                for e in pool.endpoints.values().take(output.len() / size) {
                    output[n..n + size].copy_from_slice(bytes_of(e));
                    n += size;
                }
                Ok(n)
            }
            control::IOCTL_SB_SET_ENDPOINT => {
                let req: SetEndpoint = read(input)
                    .filter(|_| input.len() == size_of::<SetEndpoint>())
                    .ok_or("bad SET_ENDPOINT size")?;
                if let Some(why) = &pool.fail_set {
                    return Err(why.clone());
                }
                if req.state > control::STATE_ENABLED {
                    return Err(format!("bad state {}", req.state));
                }
                let e = pool
                    .endpoints
                    .get_mut(&req.id)
                    .ok_or(format!("no endpoint {}", req.id))?;
                let name = control::text(&req.name);
                if !name.is_empty() {
                    if name.len() == TEXT {
                        return Err("unterminated name".into());
                    }
                    e.name = req.name;
                }
                e.state = req.state;
                e.windows_id = [0; TEXT];
                if req.state == control::STATE_ENABLED {
                    let flow = if e.direction == Direction::FromApps as u32 {
                        Flow::Render
                    } else {
                        Flow::Capture
                    };
                    control::set_text(&mut e.windows_id, &Self::windows_id(e.id, flow).0);
                }
                Ok(0)
            }
            control::IOCTL_SB_SET_FALLBACK => {
                let req: SetFallback = read(input)
                    .filter(|_| input.len() == size_of::<SetFallback>())
                    .ok_or("bad SET_FALLBACK size")?;
                if req.fallback > control::FALLBACK_LOOPBACK {
                    return Err(format!("bad fallback {}", req.fallback));
                }
                let e = pool
                    .endpoints
                    .get_mut(&req.id)
                    .ok_or(format!("no endpoint {}", req.id))?;
                e.fallback = req.fallback;
                Ok(0)
            }
            other => Err(format!("unknown IOCTL {other:#x}")),
        }
    }
}

/// The real driver: its control device, opened by path (`sb_ring::control::CONTROL_PATH`).
#[cfg(windows)]
pub struct DeviceTransport {
    handle: windows::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
unsafe impl Send for DeviceTransport {}

#[cfg(windows)]
impl DeviceTransport {
    pub fn open() -> Result<Self, String> {
        use windows::Win32::Foundation::{
            ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GENERIC_READ, GENERIC_WRITE,
        };
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING,
        };
        use windows::core::HSTRING;
        let path = HSTRING::from(control::CONTROL_PATH);
        let handle = unsafe {
            CreateFileW(
                &path,
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .map_err(|e| {
            let code = e.code();
            if code == ERROR_FILE_NOT_FOUND.to_hresult() || code == ERROR_PATH_NOT_FOUND.to_hresult() {
                "the soundboard driver isn't installed".to_string()
            } else if code == ERROR_ACCESS_DENIED.to_hresult() {
                // The driver's control device is exclusive and SDDL-protected.
                "the soundboard driver refused the controller: another controller has it open, \
                 or this account isn't allowed to"
                    .to_string()
            } else {
                format!("open the soundboard driver: {e}")
            }
        })?;
        Ok(Self { handle })
    }
}

#[cfg(windows)]
impl Transport for DeviceTransport {
    fn ioctl(&mut self, code: u32, input: &[u8], output: &mut [u8]) -> Result<usize, String> {
        use windows::Win32::System::IO::DeviceIoControl;
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                self.handle,
                code,
                (!input.is_empty()).then_some(input.as_ptr().cast()),
                input.len() as u32,
                (!output.is_empty()).then_some(output.as_mut_ptr().cast()),
                output.len() as u32,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|e| e.to_string())?;
        Ok(returned as usize)
    }
}

#[cfg(windows)]
impl Drop for DeviceTransport {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.handle) };
    }
}

/// Work for the driver worker.
#[derive(Debug, Clone, PartialEq)]
pub enum DriverJob {
    /// Enable (with a name) or disable an endpoint; answered with `Msg::DriverDone { op }`.
    SetEndpoint {
        op: u64,
        id: u32,
        enabled: bool,
        name: String,
    },
    Stop,
}

impl From<&Endpoint> for PoolEndpoint {
    fn from(e: &Endpoint) -> Self {
        PoolEndpoint {
            enabled: e.enabled,
            flow: e.flow,
            name: e.name.clone(),
            device: e.device.clone(),
        }
    }
}

/// How the worker waits for the driver.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// First wait after a failed open; doubles up to `retry_max`.
    pub retry_first: Duration,
    pub retry_max: Duration,
    /// How often a connected worker checks the driver is still there.
    pub heartbeat: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            retry_first: Duration::from_millis(250),
            retry_max: Duration::from_secs(5),
            heartbeat: Duration::from_secs(2),
        }
    }
}

/// Run the driver worker: open with `open` (again and again while it fails), report the
/// pool, then take jobs.
pub fn spawn(
    open: impl FnMut() -> Result<Box<dyn Transport>, String> + Send + 'static,
    inbox: Inbox,
) -> (Sender<DriverJob>, std::thread::JoinHandle<()>) {
    spawn_with(open, Timing::default(), inbox)
}

pub fn spawn_with(
    mut open: impl FnMut() -> Result<Box<dyn Transport>, String> + Send + 'static,
    timing: Timing,
    inbox: Inbox,
) -> (Sender<DriverJob>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("sb-driver".into())
        .spawn(move || {
            // What the actor was last told is wrong, so a retry that fails the same way is quiet.
            let mut missing: Option<String> = None;
            let mut wait = timing.retry_first;
            loop {
                match open().and_then(ControlClient::open) {
                    Ok(client) => {
                        wait = timing.retry_first;
                        match connected(client, &rx, &inbox, timing.heartbeat) {
                            Gone::Stopped => return,
                            Gone::Lost(why) => {
                                eprintln!("driver: {why}");
                                missing = Some(why);
                            }
                        }
                    }
                    Err(e) => {
                        if missing.as_ref() != Some(&e) {
                            let _ = inbox.send(Msg::Observed(Observation::Driver(DriverStatus::Missing(e.clone()))));
                            missing = Some(e);
                        }
                    }
                }
                let why = missing.clone().unwrap_or_default();
                if !refuse_until(Instant::now() + wait, &rx, &inbox, &why) {
                    return;
                }
                wait = (wait * 2).min(timing.retry_max);
            }
        })
        .expect("spawn the driver worker");
    (tx, thread)
}

enum Gone {
    Stopped,
    /// The driver went away; its pool has been reported gone, and the actor told this.
    Lost(String),
}

/// No driver until `deadline`: jobs fail with `why`. `false` once told to stop.
fn refuse_until(deadline: Instant, rx: &Receiver<DriverJob>, inbox: &Inbox, why: &str) -> bool {
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(DriverJob::SetEndpoint { op, .. }) => {
                let _ = inbox.send(Msg::DriverDone {
                    op,
                    result: Err(why.to_owned()),
                });
            }
            Ok(DriverJob::Stop) | Err(RecvTimeoutError::Disconnected) => return false,
            Err(RecvTimeoutError::Timeout) => return true,
        }
    }
}

/// Serve jobs over an open driver until it goes away or the worker is stopped.
fn connected(mut client: ControlClient, rx: &Receiver<DriverJob>, inbox: &Inbox, heartbeat: Duration) -> Gone {
    let mut known: BTreeMap<u32, Endpoint> = BTreeMap::new();
    let mut report = |client: &mut ControlClient, first: bool| {
        let mut out = Vec::new();
        if first {
            out.push(Observation::Driver(DriverStatus::Ready));
        }
        match client.list() {
            Ok(list) => {
                let now: BTreeMap<u32, Endpoint> = list.into_iter().map(|e| (e.id, e)).collect();
                for id in known.keys().filter(|id| !now.contains_key(id)) {
                    out.push(Observation::Endpoint { id: *id, state: None });
                }
                for (id, e) in &now {
                    if known.get(id) != Some(e) {
                        out.push(Observation::Endpoint {
                            id: *id,
                            state: Some(e.into()),
                        });
                    }
                }
                known = now;
            }
            Err(e) => eprintln!("driver: listing endpoints: {e}"),
        }
        if !out.is_empty() {
            let _ = inbox.send(Msg::ObservedAll(out));
        }
    };
    report(&mut client, true);
    let mut next_check = Instant::now() + heartbeat;
    let lost = loop {
        match rx.recv_timeout(next_check.saturating_duration_since(Instant::now())) {
            Ok(DriverJob::SetEndpoint { op, id, enabled, name }) => {
                let result = client.set_endpoint(id, enabled, &name);
                // A failure may be the driver leaving rather than a refusal: check before
                // reporting the pool.
                let gone = result.as_ref().err().and_then(|_| client.ping().err());
                if gone.is_none() {
                    report(&mut client, false);
                }
                let _ = inbox.send(Msg::DriverDone { op, result });
                if let Some(why) = gone {
                    break why;
                }
            }
            Ok(DriverJob::Stop) | Err(RecvTimeoutError::Disconnected) => return Gone::Stopped,
            Err(RecvTimeoutError::Timeout) => {
                if let Err(why) = client.ping() {
                    break why;
                }
                next_check = Instant::now() + heartbeat;
            }
        }
    };
    // Close the handle before anything else: the old driver can't unload while we hold it.
    drop(client);
    let why = format!("the soundboard driver went away ({lost})");
    let mut out: Vec<Observation> = known
        .keys()
        .map(|id| Observation::Endpoint { id: *id, state: None })
        .collect();
    out.push(Observation::Driver(DriverStatus::Missing(why.clone())));
    let _ = inbox.send(Msg::ObservedAll(out));
    Gone::Lost(why)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(d: &FakeDriver) -> ControlClient {
        ControlClient::open(Box::new(d.clone())).unwrap()
    }

    #[test]
    fn the_pool_through_the_abi() {
        let d = FakeDriver::new(8, 2);
        let mut c = client(&d);
        assert_eq!(c.info.endpoints, 10);
        let list = c.list().unwrap();
        assert_eq!(list.len(), 10);
        assert!(list.iter().all(|e| !e.enabled && e.device.is_none()));
        assert_eq!(list.iter().filter(|e| e.flow == Flow::Render).count(), 8);

        c.set_endpoint(3, true, "Discord").unwrap();
        let e = c.list().unwrap().into_iter().find(|e| e.id == 3).unwrap();
        assert!(e.enabled);
        assert_eq!(e.name, "Discord");
        assert_eq!(e.device, Some(FakeDriver::windows_id(3, Flow::Render)));
        c.set_fallback(3, true).unwrap();
        assert!(c.list().unwrap()[2].loopback_fallback);

        // Disabling keeps the name, drops the Windows endpoint.
        c.set_endpoint(3, false, "").unwrap();
        let e = c.list().unwrap().into_iter().find(|e| e.id == 3).unwrap();
        assert_eq!((e.enabled, e.name.as_str(), e.device), (false, "Discord", None));
        assert!(c.set_endpoint(99, true, "x").unwrap_err().contains("no endpoint 99"));
        // A very long name is cut to fit, not refused.
        c.set_endpoint(1, true, &"y".repeat(200)).unwrap();
        assert_eq!(c.list().unwrap()[0].name.len(), TEXT / 2);
    }

    #[test]
    fn the_fake_refuses_what_the_driver_must_refuse() {
        let mut d = FakeDriver::new(2, 0);
        let mut out = [0u8; 0];
        // Short, long and garbage requests.
        assert!(d.ioctl(control::IOCTL_SB_SET_ENDPOINT, &[1, 2, 3], &mut out).is_err());
        let mut big = vec![0u8; size_of::<SetEndpoint>() + 1];
        assert!(d.ioctl(control::IOCTL_SB_SET_ENDPOINT, &big, &mut out).is_err());
        big.truncate(size_of::<SetEndpoint>());
        big[4] = 7; // state 7
        assert!(d.ioctl(control::IOCTL_SB_SET_ENDPOINT, &big, &mut out).is_err());
        let mut unterminated = SetEndpoint {
            id: 1,
            state: control::STATE_ENABLED,
            name: [b'a' as u16; TEXT],
        };
        assert!(
            d.ioctl(control::IOCTL_SB_SET_ENDPOINT, bytes_of(&unterminated), &mut out)
                .is_err()
        );
        unterminated.name[TEXT - 1] = 0;
        assert!(
            d.ioctl(control::IOCTL_SB_SET_ENDPOINT, bytes_of(&unterminated), &mut out)
                .is_ok()
        );
        assert!(d.ioctl(0x1234, &[], &mut out).is_err());
        // A list into a small buffer returns what fits.
        let mut one = vec![0u8; size_of::<EndpointInfo>() + 10];
        assert_eq!(
            d.ioctl(control::IOCTL_SB_LIST_ENDPOINTS, &[], &mut one).unwrap(),
            size_of::<EndpointInfo>()
        );
    }

    const FAST: Timing = Timing {
        retry_first: Duration::from_millis(5),
        retry_max: Duration::from_millis(20),
        heartbeat: Duration::from_millis(20),
    };

    /// The next message the worker sends, within a second.
    fn next(rx: &Receiver<Msg>) -> Msg {
        rx.recv_timeout(Duration::from_secs(1)).expect("a message from the driver worker")
    }

    /// Messages until one matches; panics after a second.
    fn until(rx: &Receiver<Msg>, mut want: impl FnMut(&Msg) -> bool) -> Vec<Msg> {
        let mut seen = Vec::new();
        loop {
            let m = next(rx);
            let done = want(&m);
            seen.push(m);
            if done {
                return seen;
            }
        }
    }

    fn says_ready(m: &Msg) -> bool {
        matches!(m, Msg::ObservedAll(all) if all.first() == Some(&Observation::Driver(DriverStatus::Ready)))
    }

    fn says_missing(m: &Msg) -> Option<&str> {
        fn missing(o: &Observation) -> Option<&str> {
            match o {
                Observation::Driver(DriverStatus::Missing(why)) => Some(why.as_str()),
                _ => None,
            }
        }
        match m {
            Msg::Observed(o) => missing(o),
            Msg::ObservedAll(all) => all.iter().find_map(missing),
            _ => None,
        }
    }

    /// A handle on a fake driver that counts itself, so tests see when the worker closes it.
    struct Handle {
        d: FakeDriver,
        open: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Transport for Handle {
        fn ioctl(&mut self, code: u32, input: &[u8], output: &mut [u8]) -> Result<usize, String> {
            self.d.ioctl(code, input, output)
        }
    }
    impl Drop for Handle {
        fn drop(&mut self) {
            self.open.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Opens `d` like `DeviceTransport` would: fails while it's removed.
    fn opener(
        d: &FakeDriver,
        open: &Arc<std::sync::atomic::AtomicUsize>,
    ) -> impl FnMut() -> Result<Box<dyn Transport>, String> + Send + 'static {
        let (d, open) = (d.clone(), open.clone());
        move || {
            if d.is_removed() {
                return Err("the soundboard driver isn't installed".into());
            }
            open.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::new(Handle {
                d: d.clone(),
                open: open.clone(),
            }))
        }
    }

    fn set(op: u64, id: u32, enabled: bool) -> DriverJob {
        DriverJob::SetEndpoint {
            op,
            id,
            enabled,
            name: "Discord".into(),
        }
    }

    fn done(m: &Msg, want: u64) -> bool {
        matches!(m, Msg::DriverDone { op, .. } if *op == want)
    }

    #[test]
    fn a_driver_installed_later_is_found() {
        let d = FakeDriver::new(2, 0);
        d.set_removed(true);
        let open = Arc::default();
        let (inbox, rx) = mpsc::channel();
        let (jobs, thread) = spawn_with(opener(&d, &open), FAST, inbox);

        assert_eq!(says_missing(&next(&rx)), Some("the soundboard driver isn't installed"));
        // Meanwhile jobs fail at once, with why.
        jobs.send(set(1, 1, true)).unwrap();
        let Msg::DriverDone { op: 1, result: Err(why) } = next(&rx) else { panic!() };
        assert!(why.contains("isn't installed"));
        // Several retries fail the same way: the actor isn't told again.
        std::thread::sleep(FAST.retry_max * 3);
        assert!(rx.try_recv().is_err(), "the same failure is reported once");

        d.set_removed(false);
        let seen = until(&rx, says_ready);
        assert!(seen.iter().all(|m| says_missing(m).is_none()));
        jobs.send(set(2, 1, true)).unwrap();
        let seen = until(&rx, |m| done(m, 2));
        assert!(matches!(seen.last(), Some(Msg::DriverDone { result: Ok(()), .. })));

        jobs.send(DriverJob::Stop).unwrap();
        thread.join().unwrap();
        assert_eq!(open.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn a_driver_that_goes_away_is_let_go_and_found_again() {
        use std::sync::atomic::Ordering::SeqCst;
        let d = FakeDriver::new(2, 1);
        let open = Arc::default();
        let (inbox, rx) = mpsc::channel();
        let (jobs, thread) = spawn_with(opener(&d, &open), FAST, inbox);
        until(&rx, says_ready);
        jobs.send(set(1, 2, true)).unwrap();
        until(&rx, |m| done(m, 1));

        // The driver is being removed (an update): the heartbeat notices, and the handle is
        // closed before the actor hears, so the old driver can unload.
        d.set_removed(true);
        let seen = until(&rx, |m| says_missing(m).is_some());
        assert_eq!(open.load(SeqCst), 0, "the handle is closed");
        let Some(Msg::ObservedAll(all)) = seen.last() else { panic!() };
        for id in 1..=3 {
            assert!(all.contains(&Observation::Endpoint { id, state: None }), "endpoint {id} gone");
        }
        assert!(says_missing(seen.last().unwrap()).unwrap().contains("went away"));
        // Then it's simply not installed, until the new one is.
        assert_eq!(says_missing(&next(&rx)), Some("the soundboard driver isn't installed"));

        d.set_removed(false);
        let seen = until(&rx, says_ready);
        let Some(Msg::ObservedAll(all)) = seen.last() else { panic!() };
        // A fresh driver: every endpoint reported again, all disabled (routing re-enables).
        let endpoints: Vec<_> = all
            .iter()
            .filter_map(|o| match o {
                Observation::Endpoint { id, state: Some(e) } => Some((*id, e.enabled)),
                _ => None,
            })
            .collect();
        assert_eq!(endpoints, [(1, false), (2, false), (3, false)]);
        assert_eq!(open.load(SeqCst), 1);

        jobs.send(DriverJob::Stop).unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn a_refused_job_keeps_the_driver() {
        let d = FakeDriver::new(2, 0);
        let open = Arc::default();
        let (inbox, rx) = mpsc::channel();
        let (jobs, thread) = spawn_with(opener(&d, &open), FAST, inbox);
        until(&rx, says_ready);
        d.fail_set_endpoint(Some("busy"));
        jobs.send(set(1, 1, true)).unwrap();
        let seen = until(&rx, |m| done(m, 1));
        assert!(matches!(seen.last(), Some(Msg::DriverDone { result: Err(why), .. }) if why == "busy"));
        // Still connected through a few heartbeats.
        std::thread::sleep(FAST.heartbeat * 4);
        while let Ok(m) = rx.try_recv() {
            assert!(says_missing(&m).is_none(), "{m:?}");
        }
        d.fail_set_endpoint(None);
        jobs.send(set(2, 1, true)).unwrap();
        let seen = until(&rx, |m| done(m, 2));
        assert!(matches!(seen.last(), Some(Msg::DriverDone { result: Ok(()), .. })));
        jobs.send(DriverJob::Stop).unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn a_job_that_finds_the_driver_gone_fails_and_lets_go() {
        let d = FakeDriver::new(2, 0);
        let open = Arc::default();
        let (inbox, rx) = mpsc::channel();
        // A heartbeat too slow to notice first: the job does.
        let slow = Timing {
            heartbeat: Duration::from_secs(60),
            ..FAST
        };
        let (jobs, thread) = spawn_with(opener(&d, &open), slow, inbox);
        until(&rx, says_ready);
        d.set_removed(true);
        jobs.send(set(1, 1, true)).unwrap();
        let seen = until(&rx, |m| says_missing(m).is_some());
        assert!(seen.iter().any(|m| matches!(m, Msg::DriverDone { op: 1, result: Err(_) })));
        assert_eq!(open.load(std::sync::atomic::Ordering::SeqCst), 0);
        jobs.send(DriverJob::Stop).unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn a_driver_with_another_abi_is_refused() {
        struct Old;
        impl Transport for Old {
            fn ioctl(&mut self, _: u32, _: &[u8], out: &mut [u8]) -> Result<usize, String> {
                let info = DriverInfo {
                    abi_version: 99,
                    driver_version: 0,
                    endpoints: 0,
                    _reserved: 0,
                };
                out[..16].copy_from_slice(bytes_of(&info));
                Ok(16)
            }
        }
        assert!(ControlClient::open(Box::new(Old)).err().unwrap().contains("ABI 99"));
    }
}
