//! Virtual endpoints: the soundboard driver's render endpoints (what apps play) and virtual
//! microphones (what apps capture), connected to the engine through shared rings (`sb-ring`).
//!
//! For each virtual endpoint the engine allocates a [`SharedRing`] and hands it to the
//! driver through a [`DriverLink`] (the real link is IOCTLs, milestone D6; [`FakeDriver`]
//! plays the driver in user mode). The driver's timer produces into or consumes from that
//! ring and signals a [`Wake`] each tick. A **pump** thread woken by it moves audio between
//! the shared ring and an ordinary engine ring, stamping it like a WASAPI device thread does,
//! so the mix side treats a virtual endpoint exactly like a device: same priming, same drift
//! correction (the driver's clock is just another clock), same plan handoff.

use crate::Frame;
use crate::graph::compile::EndpointKey;
use crate::graph::spec::NodeId;
use crate::rt::ring::{RingReader, RingSizing, RingWriter, now_ns};
use crate::rt::stats::StreamStats;
use sb_ring::ioctl::Direction;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Shared rings hold 100 ms: plenty for pump wake-up jitter; the pumps keep them nearly empty.
pub const SHARED_RING_FRAMES: u32 = 4800;
/// The virtual mic pump keeps this many driver periods queued in the shared ring.
const MIC_QUEUE_PERIODS: usize = 2;
/// Keys handed out for virtual endpoints have this bit set, so they never collide with the
/// keys of the device endpoints they live next to.
pub const VIRTUAL_KEY_BIT: EndpointKey = 1 << 63;

/// An `sb-ring` in page-aligned memory the engine owns. The driver locks these pages while
/// attached; the engine keeps the allocation alive until after detach.
pub struct SharedRing {
    ptr: *mut u8,
    alloc: std::alloc::Layout,
    len: usize,
    layout: sb_ring::Layout,
}

// The memory is only reached through sb-ring's handles, which do the synchronizing.
unsafe impl Send for SharedRing {}
unsafe impl Sync for SharedRing {}

impl SharedRing {
    pub fn new(capacity_frames: u32) -> Result<Self, String> {
        let len = sb_ring::ring_bytes(capacity_frames);
        let alloc = std::alloc::Layout::from_size_align(len.next_multiple_of(4096), 4096).map_err(|e| e.to_string())?;
        // SAFETY: non-zero size; zeroed so nothing reads uninitialized memory.
        let ptr = unsafe { std::alloc::alloc_zeroed(alloc) };
        if ptr.is_null() {
            return Err("out of memory for a shared ring".into());
        }
        // SAFETY: freshly allocated, exclusive, `len` bytes.
        let layout = unsafe { sb_ring::init(ptr, len, capacity_frames) }.map_err(|e| format!("{e:?}"))?;
        Ok(Self {
            ptr,
            alloc,
            len,
            layout,
        })
    }

    /// The engine side's view (already validated by `init`).
    pub fn layout(&self) -> sb_ring::Layout {
        self.layout
    }

    /// Address and length, as handed to the driver (which validates them itself).
    pub fn raw(&self) -> (*mut u8, usize) {
        (self.ptr, self.len)
    }
}

impl Drop for SharedRing {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { std::alloc::dealloc(self.ptr, self.alloc) }
    }
}

/// What the driver signals each tick and the pump waits on.
pub trait Wake: Send + Sync {
    /// Block until signalled or `timeout`; `true` if signalled.
    fn wait(&self, timeout: Duration) -> bool;
    fn signal(&self);
}

/// A user-mode [`Wake`] (the real driver will signal a Windows event instead).
#[derive(Default)]
pub struct StdWake {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Wake for StdWake {
    fn wait(&self, timeout: Duration) -> bool {
        let guard = self.flag.lock().unwrap_or_else(|e| e.into_inner());
        let (mut guard, _) = self
            .cv
            .wait_timeout_while(guard, timeout, |set| !*set)
            .unwrap_or_else(|e| e.into_inner());
        std::mem::replace(&mut *guard, false)
    }

    fn signal(&self) {
        *self.flag.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.cv.notify_one();
    }
}

/// What attaching told us about the endpoint.
#[derive(Debug, Clone, Copy)]
pub struct VirtualInfo {
    /// Frames the driver moves per tick.
    pub period_frames: usize,
}

/// The data-plane link to the driver: hand a ring and a wake-up to an endpoint, take them back.
/// The driver refuses endpoints the controller hasn't enabled; the engine just reports that.
pub trait DriverLink: Send {
    fn attach(
        &mut self,
        endpoint: u32,
        direction: Direction,
        ring: Arc<SharedRing>,
        wake: Arc<dyn Wake>,
    ) -> Result<VirtualInfo, String>;
    fn detach(&mut self, endpoint: u32);
}

impl<L: DriverLink + ?Sized> DriverLink for Box<L> {
    fn attach(
        &mut self,
        endpoint: u32,
        direction: Direction,
        ring: Arc<SharedRing>,
        wake: Arc<dyn Wake>,
    ) -> Result<VirtualInfo, String> {
        (**self).attach(endpoint, direction, ring, wake)
    }
    fn detach(&mut self, endpoint: u32) {
        (**self).detach(endpoint)
    }
}

// ---- pumps: one step per driver tick, also driven directly by the simulation ----

/// Apps → engine: drains the shared ring (the driver produces) into an engine ring.
pub struct SourcePump {
    shared: sb_ring::Consumer,
    writer: RingWriter,
    stats: Arc<StreamStats>,
    scratch: Vec<Frame>,
}

impl SourcePump {
    pub fn new(ring: &SharedRing, writer: RingWriter, stats: Arc<StreamStats>) -> Self {
        Self {
            shared: sb_ring::Consumer::new(ring.layout()),
            writer,
            stats,
            scratch: vec![[0.0; 2]; 1024],
        }
    }

    pub fn step(&mut self, now: u64) {
        loop {
            let n = self.shared.read(&mut self.scratch);
            if n == 0 {
                break;
            }
            self.writer.write(Some(&self.scratch[..n]), n, &self.stats);
        }
        self.writer.stamp(now);
    }
}

/// Engine → apps: keeps the shared ring (the driver consumes) topped up to a couple of
/// driver periods from an engine ring, like a render device keeps its buffer full.
pub struct MicPump {
    shared: sb_ring::Producer,
    reader: RingReader,
    stats: Arc<StreamStats>,
    scratch: Vec<Frame>,
    queue: usize,
}

impl MicPump {
    pub fn new(ring: &SharedRing, reader: RingReader, stats: Arc<StreamStats>, period: usize) -> Self {
        let queue = (period * MIC_QUEUE_PERIODS).min(ring.layout().capacity_frames() as usize);
        Self {
            shared: sb_ring::Producer::new(ring.layout()),
            reader,
            stats,
            scratch: vec![[0.0; 2]; queue],
            queue,
        }
    }

    pub fn step(&mut self, now: u64) {
        let n = self.queue.saturating_sub(self.shared.fill());
        if n > 0 {
            self.reader.read(&mut self.scratch[..n], &self.stats);
            self.shared.write(&self.scratch[..n]);
        }
        self.reader.stamp(now);
    }
}

enum Pump {
    Source(SourcePump),
    Mic(MicPump),
}

impl Pump {
    fn step(&mut self, now: u64) {
        match self {
            Pump::Source(p) => p.step(now),
            Pump::Mic(p) => p.step(now),
        }
    }
}

/// A pump on its own thread, woken by the driver. Dropping it stops the thread.
struct PumpThread {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl PumpThread {
    fn spawn(mut pump: Pump, wake: Arc<dyn Wake>, stats: Arc<StreamStats>) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("sb-virtual".into())
                .spawn(move || {
                    #[cfg(windows)]
                    let _mmcss = crate::backend::wasapi::Mmcss::register(&stats.mmcss);
                    #[cfg(not(windows))]
                    let _ = &stats;
                    while !stop.load(Relaxed) {
                        if wake.wait(Duration::from_millis(200)) {
                            pump.step(now_ns());
                        }
                    }
                })
                .map_err(|e| e.to_string())?
        };
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for PumpThread {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// One attached virtual endpoint: its pump thread, ring and stats.
pub struct VirtualStream {
    pub endpoint: u32,
    pub node: NodeId,
    /// Apps → engine (a virtual source), rather than a virtual mic.
    pub is_source: bool,
    /// Device-side counters of the engine ring (what the pump moved).
    pub stats: Arc<StreamStats>,
    pub ring: Arc<SharedRing>,
    thread: PumpThread,
}

/// A freshly attached endpoint: its ring, the wake the driver signals, its period.
struct Attached {
    ring: Arc<SharedRing>,
    wake: Arc<dyn Wake>,
    period: usize,
}

/// The `Endpoints` half for virtual endpoints, for a device backend to delegate to.
pub struct VirtualEndpoints<L: DriverLink> {
    link: L,
    block: usize,
    mix_burst: usize,
    next_key: EndpointKey,
    pub streams: Vec<(EndpointKey, VirtualStream)>,
}

impl<L: DriverLink> VirtualEndpoints<L> {
    pub fn new(link: L, block: usize) -> Self {
        Self {
            link,
            block,
            mix_burst: block,
            next_key: 0,
            streams: Vec::new(),
        }
    }

    pub fn link(&self) -> &L {
        &self.link
    }

    /// The mix moves this many frames per wake-up (sizes the engine rings).
    pub fn set_mix_burst(&mut self, frames: usize) {
        self.mix_burst = frames.max(self.block);
    }

    fn attach(&mut self, endpoint: u32, direction: Direction) -> Result<Attached, String> {
        if self.streams.iter().any(|(_, s)| s.endpoint == endpoint) {
            return Err(format!("virtual endpoint {endpoint} is already attached"));
        }
        let ring = Arc::new(SharedRing::new(SHARED_RING_FRAMES)?);
        let wake: Arc<dyn Wake> = Arc::new(StdWake::default());
        let info = self.link.attach(endpoint, direction, ring.clone(), wake.clone())?;
        Ok(Attached {
            ring,
            wake,
            period: info.period_frames.max(1),
        })
    }

    fn add(
        &mut self,
        endpoint: u32,
        node: NodeId,
        is_source: bool,
        stats: Arc<StreamStats>,
        ring: Arc<SharedRing>,
        thread: PumpThread,
    ) -> EndpointKey {
        self.next_key += 1;
        let key = VIRTUAL_KEY_BIT | self.next_key;
        self.streams.push((
            key,
            VirtualStream {
                endpoint,
                node,
                is_source,
                stats,
                ring,
                thread,
            },
        ));
        key
    }

    /// Attach `endpoint` for apps → engine; returns the engine ring's reading end.
    pub fn open_source(&mut self, node: NodeId, endpoint: u32) -> Result<(RingReader, EndpointKey), String> {
        let Attached { ring, wake, period } = self.attach(endpoint, Direction::FromApps)?;
        let (writer, reader) = RingSizing::for_bursts(period, self.mix_burst).ring();
        let stats = Arc::new(StreamStats::default());
        let pump = Pump::Source(SourcePump::new(&ring, writer, stats.clone()));
        let thread = PumpThread::spawn(pump, wake, stats.clone()).inspect_err(|_| self.link.detach(endpoint))?;
        Ok((reader, self.add(endpoint, node, true, stats, ring, thread)))
    }

    /// Attach `endpoint` for engine → apps; returns the engine ring's writing end.
    pub fn open_mic(&mut self, node: NodeId, endpoint: u32) -> Result<(RingWriter, EndpointKey), String> {
        let Attached { ring, wake, period } = self.attach(endpoint, Direction::ToApps)?;
        let (writer, reader) = RingSizing::for_bursts(self.mix_burst, period).ring();
        let stats = Arc::new(StreamStats::default());
        let pump = Pump::Mic(MicPump::new(&ring, reader, stats.clone(), period));
        let thread = PumpThread::spawn(pump, wake, stats.clone()).inspect_err(|_| self.link.detach(endpoint))?;
        Ok((writer, self.add(endpoint, node, false, stats, ring, thread)))
    }

    /// Pump-side counters: (node, apps → engine?, stats).
    pub fn device_stats(&self) -> Vec<(NodeId, bool, crate::rt::stats::StatsSnapshot)> {
        self.streams
            .iter()
            .map(|(_, s)| (s.node, s.is_source, s.stats.snapshot()))
            .collect()
    }

    /// Release a virtual endpoint key. `false` if it isn't one of ours.
    pub fn release(&mut self, key: EndpointKey) -> bool {
        let Some(i) = self.streams.iter().position(|(k, _)| *k == key) else {
            return false;
        };
        let (_, s) = self.streams.remove(i);
        drop(s.thread); // stop the pump first, then the driver lets go of the ring
        self.link.detach(s.endpoint);
        drop(s.ring);
        true
    }
}

// ---- FakeDriver: the driver's role in user mode ----

/// One fake endpoint's settings.
#[derive(Debug, Clone, Copy)]
pub struct FakeEndpoint {
    pub id: u32,
    pub direction: Direction,
    /// Frames per tick (the driver's timer period).
    pub period_frames: usize,
    /// Clock error of this endpoint's timer.
    pub ppm: f64,
    /// For `FromApps`: the tone "apps" play, in Hz of the driver's clock.
    pub tone_hz: f64,
}

/// What a fake endpoint has seen (for `ToApps`: what apps would have captured).
#[derive(Debug, Default)]
pub struct FakeStats {
    pub ticks: AtomicU64,
    pub frames: AtomicU64,
    /// Frames of silence the driver had to insert (ring empty).
    pub underruns: AtomicU64,
    /// Peak of the captured signal since the last read (f32 bits, like the meters).
    pub peak: AtomicU32,
}

/// The driver side of one attached endpoint, one tick at a time. Uses the same
/// `sb_ring::attach` validation the real driver will.
pub struct FakeTicker {
    pub cfg: FakeEndpoint,
    pub stats: Arc<FakeStats>,
    side: TickerSide,
    phase: f64,
    scratch: Vec<Frame>,
    /// Keeps the memory alive while attached.
    _ring: Arc<SharedRing>,
}

enum TickerSide {
    Produce(sb_ring::Producer),
    Consume(sb_ring::Consumer),
}

impl FakeTicker {
    pub fn new(cfg: FakeEndpoint, ring: Arc<SharedRing>, stats: Arc<FakeStats>) -> Result<Self, String> {
        let (ptr, len) = ring.raw();
        // SAFETY: the engine keeps `ring` alive (we hold a clone) and it is `len` bytes.
        let layout = unsafe { sb_ring::attach(ptr, len) }.map_err(|e| format!("{e:?}"))?;
        let side = match cfg.direction {
            Direction::FromApps => TickerSide::Produce(sb_ring::Producer::new(layout)),
            Direction::ToApps => TickerSide::Consume(sb_ring::Consumer::new(layout)),
        };
        Ok(Self {
            cfg,
            stats,
            side,
            phase: 0.0,
            scratch: vec![[0.0; 2]; cfg.period_frames],
            _ring: ring,
        })
    }

    /// The period the last tick produced or consumed.
    pub fn last_period(&self) -> &[Frame] {
        &self.scratch
    }

    /// Seconds between ticks on this endpoint's (drifting) clock.
    pub fn tick_seconds(&self) -> f64 {
        self.cfg.period_frames as f64 / (48_000.0 * (1.0 + self.cfg.ppm * 1e-6))
    }

    /// One timer tick: produce or consume one period.
    pub fn tick(&mut self) {
        self.stats.ticks.fetch_add(1, Relaxed);
        match &mut self.side {
            TickerSide::Produce(p) => {
                let step = self.cfg.tone_hz / 48_000.0;
                for f in &mut self.scratch {
                    let v = (self.phase * std::f64::consts::TAU).sin() as f32 * 0.5;
                    *f = [v, v];
                    self.phase = (self.phase + step).fract();
                }
                p.write(&self.scratch);
                self.stats.frames.fetch_add(self.scratch.len() as u64, Relaxed);
            }
            TickerSide::Consume(c) => {
                let n = c.read_or_silence(&mut self.scratch);
                self.stats.frames.fetch_add(n as u64, Relaxed);
                StreamStats::add(&self.stats.underruns, self.scratch.len() - n);
                let peak = self
                    .scratch
                    .iter()
                    .fold(0.0f32, |m, f| m.max(f[0].abs()).max(f[1].abs()));
                self.stats.peak.fetch_max(peak.to_bits(), Relaxed);
            }
        }
    }
}

/// A [`DriverLink`] that plays the driver in user mode: each attached endpoint gets a timer
/// thread ticking at its own (drifting) rate and signalling the engine's wake.
pub struct FakeDriver {
    endpoints: Vec<FakeEndpoint>,
    running: Vec<(u32, Arc<AtomicBool>, JoinHandle<()>)>,
    /// Stats per endpoint id, kept across re-attaches.
    pub stats: Vec<(u32, Arc<FakeStats>)>,
}

impl FakeDriver {
    pub fn new(endpoints: Vec<FakeEndpoint>) -> Self {
        let stats = endpoints
            .iter()
            .map(|e| (e.id, Arc::new(FakeStats::default())))
            .collect();
        Self {
            endpoints,
            running: Vec::new(),
            stats,
        }
    }

    pub fn stats(&self, id: u32) -> Option<&Arc<FakeStats>> {
        self.stats.iter().find(|(i, _)| *i == id).map(|(_, s)| s)
    }
}

impl DriverLink for FakeDriver {
    fn attach(
        &mut self,
        endpoint: u32,
        direction: Direction,
        ring: Arc<SharedRing>,
        wake: Arc<dyn Wake>,
    ) -> Result<VirtualInfo, String> {
        let cfg = *self
            .endpoints
            .iter()
            .find(|e| e.id == endpoint)
            .ok_or(format!("no virtual endpoint {endpoint}"))?;
        if cfg.direction != direction {
            return Err(format!(
                "virtual endpoint {endpoint} is {:?}, not {direction:?}",
                cfg.direction
            ));
        }
        if self.running.iter().any(|(id, ..)| *id == endpoint) {
            return Err(format!("virtual endpoint {endpoint} is already attached"));
        }
        let stats = self.stats(endpoint).cloned().unwrap_or_default();
        let mut ticker = FakeTicker::new(cfg, ring, stats)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name(format!("fake-driver-{endpoint}"))
                .spawn(move || {
                    // Deadlines from the start time, so sleep jitter never accumulates into drift.
                    let start = std::time::Instant::now();
                    let dt = ticker.tick_seconds();
                    let mut n = 0u64;
                    while !stop.load(Relaxed) {
                        n += 1;
                        let due = start + Duration::from_secs_f64(dt * n as f64);
                        if let Some(wait) = due.checked_duration_since(std::time::Instant::now()) {
                            std::thread::sleep(wait);
                        }
                        ticker.tick();
                        wake.signal();
                    }
                })
                .map_err(|e| e.to_string())?
        };
        self.running.push((endpoint, stop, thread));
        Ok(VirtualInfo {
            period_frames: cfg.period_frames,
        })
    }

    fn detach(&mut self, endpoint: u32) {
        if let Some(i) = self.running.iter().position(|(id, ..)| *id == endpoint) {
            let (_, stop, thread) = self.running.remove(i);
            stop.store(true, Relaxed);
            let _ = thread.join();
        }
    }
}

impl Drop for FakeDriver {
    fn drop(&mut self) {
        let ids: Vec<u32> = self.running.iter().map(|(id, ..)| *id).collect();
        for id in ids {
            self.detach(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_ring_is_page_aligned_and_attachable() {
        let r = SharedRing::new(SHARED_RING_FRAMES).unwrap();
        let (ptr, len) = r.raw();
        assert_eq!(ptr as usize % 4096, 0);
        assert_eq!(len, sb_ring::ring_bytes(SHARED_RING_FRAMES));
        assert!(unsafe { sb_ring::attach(ptr, len) }.is_ok());
    }

    #[test]
    fn std_wake_signals_and_times_out() {
        let w = StdWake::default();
        assert!(!w.wait(Duration::from_millis(5)));
        w.signal();
        assert!(w.wait(Duration::from_millis(5)));
        assert!(!w.wait(Duration::from_millis(5)), "a signal is consumed once");
    }

    #[test]
    fn source_and_mic_pumps_move_audio_through_shared_rings() {
        // Source: fake driver produces one period, pump moves it into the engine ring.
        let ring = Arc::new(SharedRing::new(960).unwrap());
        let cfg = FakeEndpoint {
            id: 1,
            direction: Direction::FromApps,
            period_frames: 480,
            ppm: 0.0,
            tone_hz: 1000.0,
        };
        let mut driver = FakeTicker::new(cfg, ring.clone(), Arc::default()).unwrap();
        let (writer, mut reader) = crate::rt::ring::ring(4096);
        let mut pump = SourcePump::new(&ring, writer, Arc::default());
        driver.tick();
        pump.step(0);
        assert_eq!(reader.slots(), 480);
        let first = reader.pop().unwrap();
        assert_eq!(first, [0.0, 0.0], "sine starts at 0");

        // Mic: pump queues two periods, fake driver consumes one per tick.
        let ring = Arc::new(SharedRing::new(4800).unwrap());
        let cfg = FakeEndpoint {
            id: 2,
            direction: Direction::ToApps,
            period_frames: 480,
            ppm: 0.0,
            tone_hz: 0.0,
        };
        let stats = Arc::new(FakeStats::default());
        let mut driver = FakeTicker::new(cfg, ring.clone(), stats.clone()).unwrap();
        let (mut writer, consumer) = crate::rt::ring::ring(4096);
        writer.write(Some(&vec![[0.25, -0.25]; 2000]), 2000, &StreamStats::default());
        let mut pump = MicPump::new(&ring, RingReader::new(consumer, 0, 4096), Arc::default(), 480);
        pump.step(0);
        driver.tick();
        assert_eq!(stats.frames.load(Relaxed), 480);
        assert_eq!(stats.underruns.load(Relaxed), 0);
        assert_eq!(f32::from_bits(stats.peak.load(Relaxed)), 0.25);
    }

    #[test]
    fn fake_driver_refuses_unknown_or_mismatched_endpoints() {
        let mut d = FakeDriver::new(vec![FakeEndpoint {
            id: 1,
            direction: Direction::FromApps,
            period_frames: 480,
            ppm: 0.0,
            tone_hz: 440.0,
        }]);
        let ring = || Arc::new(SharedRing::new(960).unwrap());
        let wake = || -> Arc<dyn Wake> { Arc::new(StdWake::default()) };
        assert!(d.attach(9, Direction::FromApps, ring(), wake()).is_err());
        assert!(d.attach(1, Direction::ToApps, ring(), wake()).is_err());
        assert!(d.attach(1, Direction::FromApps, ring(), wake()).is_ok());
        assert!(
            d.attach(1, Direction::FromApps, ring(), wake()).is_err(),
            "already attached"
        );
        d.detach(1);
        assert!(
            d.attach(1, Direction::FromApps, ring(), wake()).is_ok(),
            "re-attach after detach"
        );
    }

    #[test]
    fn virtual_endpoints_attach_pump_and_release() {
        let driver = FakeDriver::new(vec![
            FakeEndpoint {
                id: 1,
                direction: Direction::FromApps,
                period_frames: 480,
                ppm: 0.0,
                tone_hz: 440.0,
            },
            FakeEndpoint {
                id: 2,
                direction: Direction::ToApps,
                period_frames: 480,
                ppm: 0.0,
                tone_hz: 0.0,
            },
        ]);
        let mut v = VirtualEndpoints::new(driver, 256);
        let (mut reader, src_key) = v.open_source(NodeId(1), 1).unwrap();
        let (_writer, mic_key) = v.open_mic(NodeId(2), 2).unwrap();
        assert!(src_key & VIRTUAL_KEY_BIT != 0 && mic_key != src_key);
        assert!(v.open_source(NodeId(3), 1).is_err(), "one attach per endpoint");
        std::thread::sleep(Duration::from_millis(100)); // fake driver ticks every 10 ms
        let mut out = vec![[0.0; 2]; 64];
        reader.read(&mut out, &StreamStats::default());
        assert!(
            reader.fill() > 0 || reader.is_primed(),
            "audio arrived from the fake driver"
        );
        assert!(
            v.link().stats(2).unwrap().ticks.load(Relaxed) > 3,
            "mic endpoint is ticking"
        );
        assert!(v.release(src_key) && v.release(mic_key));
        assert!(!v.release(src_key), "released once");
        assert!(v.streams.is_empty());
    }
}
