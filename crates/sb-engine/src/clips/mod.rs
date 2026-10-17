//! Clips: sound files the engine loads (from a path in the `GraphSpec`), keeps decoded at
//! 48 kHz stereo, and plays when triggered (`nodes::clip` has the voices).
//!
//! - **Short clips** (up to [`ClipStore::stream_threshold_s`]) are decoded completely into
//!   memory; the audio thread just indexes them.
//! - **Long clips** keep only their first seconds in memory (the head) and get a streamer
//!   thread per clip node ([`stream`]), opened and released like a device endpoint.
//!
//! Decoded clips are cached by path, so rebuilding a graph never decodes a file twice.

pub mod decode;
pub mod stream;

use crate::Frame;
use crate::graph::compile::EndpointKey;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use stream::{StreamEnds, Streamer};

/// Keys of clip streamers have this bit set (device endpoints don't), so the engine can route
/// a release to the right owner.
pub const CLIP_KEY_BIT: EndpointKey = 1 << 62;

/// A decoded clip, planar. For a streamed clip only the head is here.
pub struct ClipData {
    pub left: Box<[f32]>,
    pub right: Box<[f32]>,
    /// The rest comes from a streamer (the file is longer than what's in memory).
    pub streamed: bool,
}

impl ClipData {
    pub fn from_frames(frames: &[Frame], streamed: bool) -> Self {
        Self {
            left: frames.iter().map(|f| f[0]).collect(),
            right: frames.iter().map(|f| f[1]).collect(),
            streamed,
        }
    }

    /// Frames in memory.
    pub fn len(&self) -> usize {
        self.left.len()
    }

    pub fn is_empty(&self) -> bool {
        self.left.is_empty()
    }
}

/// Loads and caches clips; owns the streamers of long-clip nodes.
pub struct ClipStore {
    cache: Mutex<HashMap<String, Arc<ClipData>>>,
    streamers: Mutex<Vec<(EndpointKey, Streamer)>>,
    next_key: Mutex<EndpointKey>,
    stream_threshold_frames: usize,
    head_frames: usize,
}

impl Default for ClipStore {
    fn default() -> Self {
        Self::new(60.0, 2.0)
    }
}

impl ClipStore {
    /// Clips longer than `stream_threshold_s` stream, keeping `head_s` in memory.
    pub fn new(stream_threshold_s: f64, head_s: f64) -> Self {
        Self {
            cache: Mutex::default(),
            streamers: Mutex::default(),
            next_key: Mutex::new(0),
            stream_threshold_frames: crate::ms_to_frames(stream_threshold_s * 1000.0),
            head_frames: crate::ms_to_frames(head_s * 1000.0),
        }
    }

    pub fn stream_threshold_s(&self) -> f64 {
        crate::frames_to_ms(self.stream_threshold_frames) / 1000.0
    }

    /// The decoded clip at `path` (cached). Blocks while decoding; call off the audio thread.
    pub fn load(&self, path: &str) -> Result<Arc<ClipData>, String> {
        if let Some(c) = lock(&self.cache).get(path) {
            return Ok(c.clone());
        }
        let (frames, complete) = decode::decode(Path::new(path), Some(self.stream_threshold_frames))?;
        let clip = Arc::new(match complete {
            true => ClipData::from_frames(&frames, false),
            false => ClipData::from_frames(&frames[..self.head_frames.min(frames.len())], true),
        });
        lock(&self.cache).insert(path.to_owned(), clip.clone());
        Ok(clip)
    }

    /// Start a streamer for one node playing the long clip at `path`.
    pub fn open_stream(&self, path: &str) -> Result<(StreamEnds, EndpointKey), String> {
        let (streamer, ends) = Streamer::spawn(path.into())?;
        let key = {
            let mut k = lock(&self.next_key);
            *k += 1;
            CLIP_KEY_BIT | *k
        };
        lock(&self.streamers).push((key, streamer));
        Ok((ends, key))
    }

    /// No plan uses this streamer any more.
    pub fn release(&self, key: EndpointKey) {
        lock(&self.streamers).retain(|(k, _)| *k != key);
    }

    /// Streamers running (for tests and stats).
    pub fn streams(&self) -> usize {
        lock(&self.streamers).len()
    }

    /// Forget cached decodes (e.g. after files changed on disk). Plans keep their own copies.
    pub fn clear_cache(&self) {
        lock(&self.cache).clear();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
