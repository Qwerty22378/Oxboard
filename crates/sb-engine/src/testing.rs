//! Test support shared by the compiler and engine tests: fake endpoints, a fake clock, and
//! spec builders.

use crate::Frame;
use crate::clock::master::MasterClock;
use crate::graph::compile::{EndpointKey, Endpoints};
use crate::graph::spec::{EdgeSpec, NodeId, NodeKind, NodeSpec, ParamId, ParamSpec};
use crate::rt::ring::{RingReader, RingWriter, ring};
use rtrb::{Consumer, Producer};

/// Hands out rings and keeps the device ends: producers for inputs, consumers for outputs.
#[derive(Default)]
pub struct FakeEndpoints {
    pub inputs: Vec<(EndpointKey, NodeId, Producer<Frame>)>,
    pub outputs: Vec<(EndpointKey, NodeId, Consumer<Frame>)>,
    /// Every open, in order (device names).
    pub opened: Vec<String>,
    pub released: Vec<EndpointKey>,
    /// Device name that fails to open.
    pub fail: Option<String>,
    pub next_key: EndpointKey,
}

impl Endpoints for FakeEndpoints {
    fn input(&mut self, node: NodeId, device: &str) -> Result<(RingReader, EndpointKey), String> {
        if self.fail.as_deref() == Some(device) {
            return Err(format!("{device} not found"));
        }
        self.opened.push(device.into());
        let (p, c) = rtrb::RingBuffer::new(1 << 16);
        self.next_key += 1;
        self.inputs.push((self.next_key, node, p));
        Ok((RingReader::new(c, 0, 1 << 16), self.next_key))
    }

    fn output(&mut self, node: NodeId, device: &str) -> Result<(RingWriter, EndpointKey), String> {
        if self.fail.as_deref() == Some(device) {
            return Err(format!("{device} not found"));
        }
        self.opened.push(device.into());
        let (w, c) = ring(1 << 16);
        self.next_key += 1;
        self.outputs.push((self.next_key, node, c));
        Ok((w, self.next_key))
    }

    fn release(&mut self, key: EndpointKey) {
        assert!(!self.released.contains(&key), "endpoint {key} released twice");
        self.released.push(key);
        self.inputs.retain(|(k, ..)| *k != key);
        self.outputs.retain(|(k, ..)| *k != key);
    }
}

impl FakeEndpoints {
    /// Push frames into the (open) input of `node`.
    pub fn feed(&mut self, node: u32, frames: &[Frame]) {
        let p = &mut self
            .inputs
            .iter_mut()
            .find(|(_, n, _)| n.0 == node)
            .expect("no such input")
            .2;
        frames.iter().for_each(|f| p.push(*f).unwrap());
    }

    /// Feed every open input `frames` copies of its node's constant.
    pub fn feed_all(&mut self, frames: usize, value: impl Fn(NodeId) -> f32) {
        for (_, node, p) in &mut self.inputs {
            let v = value(*node);
            for _ in 0..frames {
                let _ = p.push([v, v]);
            }
        }
    }

    /// Everything the output of `node` received so far (from its newest open endpoint).
    pub fn drain(&mut self, node: u32) -> Vec<Frame> {
        let c = &mut self
            .outputs
            .iter_mut()
            .rev()
            .find(|(_, n, _)| n.0 == node)
            .expect("no such output")
            .2;
        std::iter::from_fn(|| c.pop().ok()).collect()
    }

    pub fn open_count(&self) -> usize {
        self.inputs.len() + self.outputs.len()
    }
}

/// Records the master bus; one block per call. `now` is whatever the test sets.
#[derive(Default)]
pub struct FakeClock {
    pub out: Vec<Frame>,
    pub masters: Vec<Option<String>>,
    pub now: u64,
}

impl MasterClock for FakeClock {
    fn wait(&mut self) -> usize {
        1
    }
    fn deliver(&mut self, l: &[f32], r: &[f32]) {
        self.out.extend(l.iter().zip(r).map(|(l, r)| [*l, *r]));
    }
    fn set_master(&mut self, device: Option<&str>) {
        self.masters.push(device.map(str::to_owned));
    }
    fn now_ns(&self) -> u64 {
        self.now
    }
}

pub fn node(id: u32, kind: NodeKind) -> NodeSpec {
    NodeSpec { id: NodeId(id), kind }
}
pub fn tone(id: u32, freq: f32, gain: f32) -> NodeSpec {
    node(id, NodeKind::Tone { freq, gain })
}
pub fn input(id: u32, dev: &str) -> NodeSpec {
    node(id, NodeKind::DeviceInput { device: dev.into() })
}
pub fn bus(id: u32) -> NodeSpec {
    node(id, NodeKind::Bus)
}
pub fn output(id: u32, dev: &str) -> NodeSpec {
    node(id, NodeKind::DeviceOutput { device: dev.into() })
}
pub fn edge(from: u32, to: u32) -> EdgeSpec {
    EdgeSpec {
        from: NodeId(from),
        to: NodeId(to),
        gain: None,
    }
}
pub fn gain_edge(from: u32, to: u32, param: u32) -> EdgeSpec {
    EdgeSpec {
        from: NodeId(from),
        to: NodeId(to),
        gain: Some(ParamId(param)),
    }
}
pub fn param(id: u32, value: f32) -> ParamSpec {
    ParamSpec { id: ParamId(id), value }
}

pub fn sine_at(i: usize, freq: f32, gain: f32) -> f32 {
    (i as f64 * freq as f64 / crate::SAMPLE_RATE as f64 * std::f64::consts::TAU).sin() as f32 * gain
}

/// Tiny deterministic PRNG (xorshift), so tests need no dependencies.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}
