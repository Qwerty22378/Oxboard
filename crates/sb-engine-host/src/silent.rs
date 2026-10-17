//! A backend without audio: no device opens (each device node reports `DeviceLost` and stays
//! silent), and the mix runs on a timer at about real time, playing nowhere. Tones, clips,
//! params and meters all work, so tests can drive the whole host, the controller's included.

use crate::host::{Backend, MixHandle, Started};
use sb_engine::clock::master::MasterClock;
use sb_engine::engine::Engine;
use sb_engine::graph::compile::{EndpointKey, Endpoints, Tables};
use sb_engine::graph::spec::NodeId;
use sb_engine::rt::mix_thread::MixCore;
use sb_engine::rt::ring::{RingReader, RingWriter};
use sb_engine::rt::stats::{MixSnapshot, MixStats};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// No devices at all.
pub struct NoDevices;

impl Endpoints for NoDevices {
    fn input(&mut self, _: NodeId, d: &str) -> Result<(RingReader, EndpointKey), String> {
        Err(format!("no device {d}: running silent"))
    }
    fn output(&mut self, _: NodeId, d: &str) -> Result<(RingWriter, EndpointKey), String> {
        Err(format!("no device {d}: running silent"))
    }
    fn release(&mut self, _: EndpointKey) {}
}

/// Plays the master bus nowhere, one 256-frame block every ~5.3 ms.
struct SilentClock(Instant);

impl MasterClock for SilentClock {
    fn wait(&mut self) -> usize {
        std::thread::sleep(Duration::from_micros(5333));
        1
    }
    fn deliver(&mut self, _: &[f32], _: &[f32]) {}
    fn set_master(&mut self, _: Option<&str>) {}
    fn now_ns(&self) -> u64 {
        self.0.elapsed().as_nanos() as u64
    }
}

struct SilentMix {
    stats: Arc<MixStats>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MixHandle for SilentMix {
    fn stats(&self) -> MixSnapshot {
        self.stats.snapshot()
    }
    fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

impl Drop for SilentMix {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub struct SilentBackend;

impl Backend for SilentBackend {
    type E = NoDevices;
    fn start(&mut self, block: usize, tables: Tables, master: Option<&str>) -> Result<Started<NoDevices>, String> {
        let (engine, mut core): (_, MixCore) = Engine::with_master(block, tables, NoDevices, master);
        let stats = Arc::new(MixStats::default());
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (stats, stop) = (stats.clone(), stop.clone());
            std::thread::Builder::new()
                .name("sb-silent-mix".into())
                .spawn(move || {
                    let mut clock = SilentClock(Instant::now());
                    sb_engine::rt::mix_thread::run(&mut core, &mut clock, &stats, &stop);
                })
                .map_err(|e| e.to_string())?
        };
        Ok((
            engine,
            Box::new(SilentMix {
                stats,
                stop,
                thread: Some(thread),
            }),
        ))
    }
}
