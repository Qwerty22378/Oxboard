//! Simulated devices with their own clocks, driving the real engine (E6 tests).
//!
//! Each device ticks at `48 kHz × (1 + ppm)` in bursts of its period, on its own timeline,
//! and stamps its ring a little late (thread wake-up jitter). The master device's ticks wake
//! the mix. Everything between is the production code: `Engine`, `MixCore`, rings with
//! `RingSizing` targets, and the drift resamplers.

use crate::Frame;
use crate::engine::Engine;
use crate::graph::compile::{EndpointKey, Endpoints, Tables};
use crate::graph::spec::{GraphSpec, NodeId};
use crate::rt::mix_thread::MixCore;
use crate::rt::ring::{RingReader, RingSizing, RingWriter};
use crate::rt::stats::{MixStats, StreamStats};
use crate::testing::{FakeClock, Rng, edge, input, output, tone};
use std::collections::HashMap;
use std::f64::consts::TAU;

#[derive(Clone, Copy)]
struct DeviceSpec {
    period: usize,
    ppm: f64,
}

enum DeviceEnd {
    /// Capture device: writes into the ring the mix reads.
    Capture { writer: RingWriter, phase: f64 },
    /// Render device: reads from the ring the mix writes; keeps what it played.
    Render { reader: RingReader, played: Vec<f32> },
}

struct SimDevice {
    spec: DeviceSpec,
    end: DeviceEnd,
    next_ns: f64,
    stats: StreamStats,
}

struct SimEndpoints {
    block: usize,
    mix_burst: usize,
    devices: HashMap<String, DeviceSpec>,
    open: Vec<(EndpointKey, String, SimDevice)>,
    next_key: EndpointKey,
    /// Frequency the capture devices "hear", in Hz of their own clock.
    tone_hz: f64,
}

impl Endpoints for SimEndpoints {
    fn input(&mut self, _: NodeId, device: &str) -> Result<(RingReader, EndpointKey), String> {
        let spec = self.devices[device];
        let (writer, reader) = RingSizing::for_bursts(spec.period, self.mix_burst).ring();
        self.next_key += 1;
        let dev = SimDevice {
            spec,
            end: DeviceEnd::Capture { writer, phase: 0.0 },
            next_ns: 0.0,
            stats: Default::default(),
        };
        self.open.push((self.next_key, device.into(), dev));
        Ok((reader, self.next_key))
    }

    fn output(&mut self, _: NodeId, device: &str) -> Result<(RingWriter, EndpointKey), String> {
        let spec = self.devices[device];
        let (writer, reader) = RingSizing::for_bursts(self.mix_burst, spec.period).ring();
        self.next_key += 1;
        let dev = SimDevice {
            spec,
            end: DeviceEnd::Render {
                reader,
                played: Vec::new(),
            },
            next_ns: 0.0,
            stats: Default::default(),
        };
        self.open.push((self.next_key, device.into(), dev));
        Ok((writer, self.next_key))
    }

    fn release(&mut self, key: EndpointKey) {
        self.open.retain(|(k, ..)| *k != key);
    }

    fn master_changed(&mut self, device: Option<&str>) {
        self.mix_burst = device.map_or(self.block, |d| self.devices[d].period.div_ceil(self.block) * self.block);
    }
}

struct Sim {
    engine: Engine<SimEndpoints>,
    core: MixCore,
    clock: FakeClock,
    stats: MixStats,
    master_period: usize,
    rng: Rng,
    /// Late stamps: uniform 0..jitter.
    jitter_ns: f64,
}

impl Sim {
    fn new(block: usize, devices: &[(&str, usize, f64)], master: &str, tone_hz: f64) -> Self {
        let devices: HashMap<String, DeviceSpec> = devices
            .iter()
            .map(|&(n, period, ppm)| (n.to_string(), DeviceSpec { period, ppm }))
            .collect();
        let master_period = devices[master].period;
        let endpoints = SimEndpoints {
            block,
            mix_burst: block,
            devices,
            open: Vec::new(),
            next_key: 0,
            tone_hz,
        };
        let (engine, core) = Engine::new(block, Tables::new(16, 16), endpoints);
        Self {
            engine,
            core,
            clock: FakeClock::default(),
            stats: MixStats::default(),
            master_period,
            rng: Rng(0x5157_d41f),
            jitter_ns: 500_000.0,
        }
    }

    /// Run `seconds` of simulated time. The master ticks every period at exactly 48 kHz (it
    /// defines mix time); every other device at its own rate.
    fn run(&mut self, seconds: f64) {
        let end_ns = self.clock.now as f64 + seconds * 1e9;
        let master_dt = self.master_period as f64 / 48_000.0 * 1e9;
        let mut next_master = self.clock.now as f64 + master_dt;
        let mut free = 0usize;
        let block = self.core.plan().block();
        loop {
            // Earliest pending device event.
            let dev = self
                .engine
                .endpoints()
                .open
                .iter()
                .enumerate()
                .map(|(i, (_, _, d))| (i, d.next_ns))
                .min_by(|a, b| a.1.total_cmp(&b.1));
            let t = dev.map_or(f64::INFINITY, |d| d.1).min(next_master);
            if t > end_ns {
                break;
            }
            if t == next_master {
                self.clock.now = t as u64;
                free += self.master_period;
                while free >= block {
                    self.core.block(&mut self.clock, &self.stats);
                    free -= block;
                }
                self.engine.maintain();
                next_master += master_dt;
                continue;
            }
            let i = dev.unwrap().0;
            let jitter = self.rng.below(1000) as f64 / 1000.0 * self.jitter_ns;
            let ep = self.engine.endpoints_mut();
            let tone = ep.tone_hz;
            let d = &mut ep.open[i].2;
            let n = d.spec.period;
            match &mut d.end {
                DeviceEnd::Capture { writer, phase } => {
                    let frames: Vec<Frame> = (0..n)
                        .map(|_| {
                            let v = (*phase * TAU).sin() as f32 * 0.5;
                            *phase = (*phase + tone / 48_000.0).fract();
                            [v, v]
                        })
                        .collect();
                    writer.write(Some(&frames), n, &d.stats);
                    writer.stamp((t + jitter) as u64);
                }
                DeviceEnd::Render { reader, played } => {
                    let mut buf = vec![[0.0; 2]; n];
                    reader.read(&mut buf, &d.stats);
                    reader.stamp((t + jitter) as u64);
                    played.extend(buf.iter().map(|f| f[0]));
                }
            }
            d.next_ns += n as f64 / (48_000.0 * (1.0 + d.spec.ppm * 1e-6)) * 1e9;
        }
        self.clock.now = end_ns as u64;
    }

    fn device(&self, name: &str) -> &SimDevice {
        &self
            .engine
            .endpoints()
            .open
            .iter()
            .find(|(_, n, _)| n == name)
            .expect("device not open")
            .2
    }

    fn ring_stats(&self, node: u32) -> crate::rt::stats::StatsSnapshot {
        let l = self.engine.running_layout();
        let ep = l
            .inputs()
            .iter()
            .chain(l.outputs())
            .find(|e| e.node.0 == node)
            .expect("no ring for node");
        ep.stats.snapshot()
    }
}

/// Largest jump between consecutive samples, relative to the steepest a clean sine of
/// `freq` and `amp` can be. A dropout or a skipped chunk shows up as a ratio well above 1.
fn glitch_ratio(samples: &[f32], freq: f64, amp: f64) -> f64 {
    let max_slope = TAU * freq / 48_000.0 * amp * 1.002; // allow for the pitch correction
    samples
        .windows(2)
        .map(|w| (w[1] - w[0]).abs() as f64)
        .fold(0.0, f64::max)
        / max_slope
}

/// Mic → bus → master + two more outputs, all on different clocks and period sizes.
fn drifting_rig() -> Sim {
    let devices = [
        ("spk", 480, 0.0),      // master
        ("mic", 441, 700.0),    // 9.2 ms periods, fast
        ("spk2", 480, -400.0),  // slow
        ("hdmi", 1024, 1500.0), // 21.3 ms periods, very fast
    ];
    let mut sim = Sim::new(256, &devices, "spk", 1000.0);
    let spec = GraphSpec {
        nodes: vec![
            input(1, "mic"),
            crate::testing::bus(2),
            output(3, "spk"),
            output(4, "spk2"),
            output(5, "hdmi"),
            tone(6, 1.0, 0.0),
        ],
        edges: vec![edge(1, 2), edge(2, 3), edge(2, 4), edge(2, 5)],
        master: Some(NodeId(3)),
        ..Default::default()
    };
    sim.engine.apply_graph(&spec).unwrap();
    sim
}

fn check_converged(sim: &mut Sim, after_s: f64, check_s: f64) {
    sim.run(after_s);
    let before: Vec<_> = [1, 4, 5].iter().map(|&n| sim.ring_stats(n)).collect();
    let before_dev: Vec<_> = ["spk2", "hdmi"]
        .iter()
        .map(|&d| sim.device(d).stats.snapshot())
        .collect();
    let played_from: Vec<usize> = ["spk2", "hdmi"]
        .iter()
        .map(|&d| match &sim.device(d).end {
            DeviceEnd::Render { played, .. } => played.len(),
            _ => 0,
        })
        .collect();
    let master_from = sim.clock.out.len();
    sim.run(check_s);

    // Rings: no underruns, overruns or trims while converged; controllers locked to the drift.
    // ppm convention: positive = take frames out of the ring faster.
    for ((node, want_ppm), b) in [(1, 700.0), (4, 400.0), (5, -1500.0)].iter().zip(&before) {
        let s = sim.ring_stats(*node);
        assert_eq!(s.xruns, b.xruns, "node {node}: mix-side xruns while converged");
        assert_eq!(
            s.trimmed_frames, b.trimmed_frames,
            "node {node}: trimmed while converged"
        );
        assert!(
            (s.drift_ppm as f64 - want_ppm).abs() < 40.0,
            "node {node}: ppm {} vs {want_ppm}",
            s.drift_ppm
        );
    }
    for (dev, b) in ["spk2", "hdmi"].iter().zip(&before_dev) {
        let s = sim.device(dev).stats.snapshot();
        assert_eq!(s.xruns, b.xruns, "{dev}: render underruns while converged");
        assert_eq!(
            s.trimmed_frames, b.trimmed_frames,
            "{dev}: render trims while converged"
        );
    }
    // Audio: clean sines everywhere (the mic's 1 kHz, resampled twice on the way out).
    let master = &sim.clock.out[master_from..];
    let m: Vec<f32> = master.iter().map(|f| f[0]).collect();
    assert!(
        glitch_ratio(&m, 1000.0 * 1.0007, 0.5) < 1.05,
        "master glitch {}",
        glitch_ratio(&m, 1000.7, 0.5)
    );
    for (dev, from) in ["spk2", "hdmi"].iter().zip(played_from) {
        if let DeviceEnd::Render { played, .. } = &sim.device(dev).end {
            let r = glitch_ratio(&played[from..], 1000.0 * 1.0007, 0.5);
            assert!(r < 1.05, "{dev}: glitch ratio {r}");
        }
    }
}

#[test]
fn three_drifting_devices_converge_and_run_clean() {
    let mut sim = drifting_rig();
    check_converged(&mut sim, 120.0, 60.0);
}

/// The E6 milestone: an hour, two extra outputs and an input, no drift. Slow in debug;
/// run with `cargo test --release -p sb-engine -- --ignored one_hour`.
#[test]
#[ignore]
fn one_hour_without_drift() {
    let mut sim = drifting_rig();
    check_converged(&mut sim, 120.0, 3600.0);
}
