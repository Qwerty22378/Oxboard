//! Streaming for long clips: a decoder thread per clip node feeds 48 kHz frames to the mix in
//! chunks through an SPSC ring, starting wherever the mix asks.
//!
//! The clip's first seconds are kept in memory (the "head"), so a voice starts instantly
//! from there while the streamer decodes ahead from the end of the head. Every start
//! carries a generation number; chunks are tagged with it, so after a restart or seek the
//! mix just skips chunks from an older generation instead of anyone having to flush the ring.

use super::decode::{SourceReader, To48k};
use crate::Frame;
use rtrb::{Consumer, Producer, RingBuffer};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread::JoinHandle;
use std::time::Duration;

/// Frames per chunk.
pub const CHUNK: usize = 512;
/// Chunks the ring holds: ~2.7 s, more than the head, so a start never waits on the decoder.
const RING_CHUNKS: usize = 256;

pub struct Chunk {
    pub generation: u32,
    /// 48 kHz frame index (from the clip's start) of `frames[0]`.
    pub start: u64,
    pub len: usize,
    /// The clip ends after this chunk.
    pub end: bool,
    pub frames: [Frame; CHUNK],
}

#[derive(Debug, Clone, Copy)]
pub enum StreamCmd {
    /// Decode from 48 kHz frame `from` on, tagging chunks with `generation`.
    Start {
        generation: u32,
        from: u64,
    },
    Stop,
}

/// The mix's ends of a streamer.
pub struct StreamEnds {
    pub cmds: Producer<StreamCmd>,
    pub data: Consumer<Chunk>,
}

/// A running streamer thread. Dropping it stops the thread.
pub struct Streamer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Streamer {
    pub fn spawn(path: PathBuf) -> Result<(Self, StreamEnds), String> {
        SourceReader::open(&path)?; // fail now, not silently on the thread
        let (cmd_tx, mut cmd_rx) = RingBuffer::new(64);
        let (mut data_tx, data_rx) = RingBuffer::new(RING_CHUNKS);
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("sb-clip-stream".into())
                .spawn(move || {
                    let mut job: Option<Job> = None;
                    while !stop.load(Relaxed) {
                        while let Ok(cmd) = cmd_rx.pop() {
                            job = match cmd {
                                StreamCmd::Start { generation, from } => Job::start(&path, generation, from).ok(),
                                StreamCmd::Stop => None,
                            };
                        }
                        let busy = match &mut job {
                            Some(j) if data_tx.slots() > 0 => {
                                let done = j.fill_one(&mut data_tx);
                                if done {
                                    job = None;
                                }
                                true
                            }
                            _ => false,
                        };
                        if !busy {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    }
                })
                .map_err(|e| e.to_string())?
        };
        Ok((
            Self {
                stop,
                thread: Some(thread),
            },
            StreamEnds {
                cmds: cmd_tx,
                data: data_rx,
            },
        ))
    }
}

impl Drop for Streamer {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// One decode pass, from a start position to the end of the file.
struct Job {
    reader: SourceReader,
    conv: To48k,
    generation: u32,
    /// Next 48 kHz frame index the converter will emit.
    pos: u64,
    /// Emit frames from this index on (everything before is decoded and dropped).
    from: u64,
    pending: Vec<Frame>,
    raw: Vec<Frame>,
    eof: bool,
}

impl Job {
    fn start(path: &std::path::Path, generation: u32, from: u64) -> Result<Self, String> {
        let reader = SourceReader::open(path)?;
        let conv = To48k::new(reader.rate());
        Ok(Self {
            reader,
            conv,
            generation,
            pos: 0,
            from,
            pending: Vec::new(),
            raw: Vec::new(),
            eof: false,
        })
    }

    /// Push one chunk (decoding as needed). Returns `true` when the last chunk went out.
    fn fill_one(&mut self, tx: &mut Producer<Chunk>) -> bool {
        while self.pending.len() < CHUNK && !self.eof {
            self.raw.clear();
            match self.reader.next_frames(&mut self.raw) {
                Ok(true) => {}
                _ => {
                    self.eof = true;
                    let before = self.pending.len();
                    self.conv.finish(&mut self.pending);
                    self.skip_to_start(before);
                    break;
                }
            }
            let before = self.pending.len();
            self.conv.push(&self.raw, &mut self.pending);
            self.skip_to_start(before);
        }
        let len = self.pending.len().min(CHUNK);
        let end = self.eof && len == self.pending.len();
        let mut frames = [[0.0; 2]; CHUNK];
        frames[..len].copy_from_slice(&self.pending[..len]);
        let start = self.pos - self.pending.len() as u64;
        self.pending.drain(..len);
        let _ = tx.push(Chunk {
            generation: self.generation,
            start,
            len,
            end,
            frames,
        });
        end
    }

    /// Account for frames appended since `before`, dropping any that precede `from`.
    fn skip_to_start(&mut self, before: usize) {
        let added = (self.pending.len() - before) as u64;
        let first = self.pos;
        self.pos += added;
        if first < self.from {
            let skip = (self.from - first).min(added) as usize;
            self.pending.drain(before..before + skip);
        }
    }
}
