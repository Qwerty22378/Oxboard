//! The clip player's voices: allocation-free, on the audio thread.
//!
//! A clip node owns a fixed array of voices. `Play` starts one (subject to the retrigger mode
//! and polyphony), `Stop` fades all of them, `Seek` crossfades each to a new position. Any
//! voice that is cut (stopped, stolen, restarted, seeked) fades out over [`CLIP_FADE_FRAMES`]
//! instead of stopping dead, so nothing clicks. A looped voice wraps seamlessly.
//!
//! Streamed (long) clips play their in-memory head, then chunks from their streamer
//! (`clips::stream`); they have one voice (Overlap behaves like Restart), because the
//! streamer decodes one position at a time.

use crate::clips::ClipData;
use crate::clips::stream::{StreamCmd, StreamEnds};
use crate::graph::spec::{ClipAction, Retrigger};

/// Most voices a clip node may have playing at once.
pub const MAX_POLYPHONY: u8 = 16;
/// Room for a full set of voices plus as many fading out.
const VOICE_SLOTS: usize = 2 * MAX_POLYPHONY as usize;
/// Fade when a voice is cut: 5 ms.
pub const CLIP_FADE_FRAMES: u32 = 240;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipSettings {
    pub polyphony: u8,
    pub retrigger: Retrigger,
    pub looped: bool,
}

#[derive(Debug, Clone, Copy)]
struct Voice {
    /// Next frame to play (48 kHz, from the clip's start).
    pos: u64,
    /// Frames of fade-out left; 0 = not fading.
    fade: u32,
    /// Streamed clips: which streamer start this voice's chunks belong to.
    generation: u32,
    /// Start order, to find the oldest voice when stealing.
    age: u64,
}

/// Per-node playback state; lives in the plan and is handed over on plan swaps.
#[derive(Debug, Clone)]
pub struct ClipState {
    voices: [Option<Voice>; VOICE_SLOTS],
    next_generation: u32,
    started: u64,
    active: bool,
    stopped: bool,
    ended: Option<bool>,
}

impl Default for ClipState {
    fn default() -> Self {
        Self {
            voices: [None; VOICE_SLOTS],
            next_generation: 1,
            started: 0,
            active: false,
            stopped: false,
            ended: None,
        }
    }
}

enum Streamed {
    Frame([f32; 2]),
    /// The streamer hasn't caught up: play silence, don't advance.
    Wait,
    /// No more data for this voice (end of the file, or a newer start replaced it).
    End,
}

impl ClipState {
    /// Voices playing and not fading out.
    pub fn playing(&self) -> usize {
        self.voices.iter().flatten().filter(|v| v.fade == 0).count()
    }

    /// Whether the node went quiet since the last call: `Some(stopped)`.
    pub fn take_ended(&mut self) -> Option<bool> {
        self.ended.take()
    }

    /// Apply a trigger. Real-time safe.
    pub fn apply(&mut self, action: ClipAction, s: ClipSettings, data: &ClipData, mut stream: Option<&mut StreamEnds>) {
        let polyphony = if data.streamed {
            1
        } else {
            s.polyphony.clamp(1, MAX_POLYPHONY) as usize
        };
        let retrigger = match (data.streamed, s.retrigger) {
            (true, Retrigger::Overlap) => Retrigger::Restart,
            (_, r) => r,
        };
        match action {
            ClipAction::Play => {
                let mut live = self.playing();
                match retrigger {
                    Retrigger::Ignore if live > 0 => return,
                    Retrigger::Restart => self.fade_all(),
                    Retrigger::Overlap => {
                        while live >= polyphony {
                            self.fade_oldest();
                            live -= 1;
                        }
                    }
                    Retrigger::Ignore => {}
                }
                self.start(0, data, stream.as_deref_mut());
            }
            ClipAction::Stop => {
                if self.playing() > 0 {
                    self.stopped = true;
                }
                self.fade_all();
            }
            ClipAction::Seek { frame } => {
                let live: usize = self.playing();
                self.fade_all();
                // Past the end of an in-memory clip there's nothing to play.
                if data.streamed || (frame as usize) < data.len() {
                    for _ in 0..live {
                        self.start(frame, data, stream.as_deref_mut());
                    }
                }
            }
        }
    }

    fn fade_all(&mut self) {
        for v in self.voices.iter_mut().flatten().filter(|v| v.fade == 0) {
            v.fade = CLIP_FADE_FRAMES;
        }
    }

    fn fade_oldest(&mut self) {
        if let Some(v) = self
            .voices
            .iter_mut()
            .flatten()
            .filter(|v| v.fade == 0)
            .min_by_key(|v| v.age)
        {
            v.fade = CLIP_FADE_FRAMES;
        }
    }

    fn start(&mut self, pos: u64, data: &ClipData, stream: Option<&mut StreamEnds>) {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let voice = Voice {
            pos,
            fade: 0,
            generation,
            age: self.started,
        };
        self.started += 1;
        // A free slot, else the fading voice closest to silence (a cut at near-zero gain).
        let slot = match self.voices.iter().position(Option::is_none) {
            Some(i) => i,
            None => (0..VOICE_SLOTS)
                .min_by_key(|&i| self.voices[i].map_or(0, |v| if v.fade == 0 { u32::MAX } else { v.fade }))
                .unwrap_or(0),
        };
        self.voices[slot] = Some(voice);
        if data.streamed
            && let Some(st) = stream
        {
            let from = pos.max(data.len() as u64);
            let _ = st.cmds.push(StreamCmd::Start { generation, from });
        }
        self.active = true;
        self.stopped = false;
    }

    /// Render every voice into `l`/`r` (overwritten). Real-time safe.
    pub fn render(
        &mut self,
        data: &ClipData,
        s: ClipSettings,
        mut stream: Option<&mut StreamEnds>,
        l: &mut [f32],
        r: &mut [f32],
    ) {
        l.fill(0.0);
        r.fill(0.0);
        // Oldest first: a fading voice gets to read its streamed chunks before a newer voice
        // skips past them.
        let mut order = [0usize; VOICE_SLOTS];
        let mut n_live = 0;
        for i in 0..VOICE_SLOTS {
            if self.voices[i].is_some() {
                order[n_live] = i;
                n_live += 1;
            }
        }
        order[..n_live].sort_unstable_by_key(|&i| self.voices[i].map_or(0, |v| v.age));

        for &slot in &order[..n_live] {
            let Some(mut v) = self.voices[slot] else { continue };
            let mut alive = true;
            for (ol, or) in l.iter_mut().zip(r.iter_mut()) {
                let frame = if (v.pos as usize) < data.len() {
                    Some([data.left[v.pos as usize], data.right[v.pos as usize]])
                } else if data.streamed {
                    match stream
                        .as_deref_mut()
                        .map_or(Streamed::End, |st| read_stream(st, v.generation, v.pos))
                    {
                        Streamed::Frame(f) => Some(f),
                        Streamed::Wait => None,
                        Streamed::End => {
                            if s.looped && v.fade == 0 {
                                self.restart_loop(&mut v, data, stream.as_deref_mut());
                                Some([
                                    data.left.first().copied().unwrap_or(0.0),
                                    data.right.first().copied().unwrap_or(0.0),
                                ])
                            } else {
                                alive = false;
                                break;
                            }
                        }
                    }
                } else if s.looped && v.fade == 0 && !data.is_empty() {
                    v.pos = 0;
                    Some([data.left[0], data.right[0]])
                } else {
                    alive = false;
                    break;
                };
                let gain = if v.fade > 0 {
                    let g = v.fade as f32 / CLIP_FADE_FRAMES as f32;
                    v.fade -= 1;
                    if v.fade == 0 {
                        alive = false;
                    }
                    g
                } else {
                    1.0
                };
                if let Some([a, b]) = frame {
                    *ol += a * gain;
                    *or += b * gain;
                    v.pos += 1;
                }
                if !alive {
                    break;
                }
            }
            self.voices[slot] = alive.then_some(v);
        }

        if self.active && self.voices.iter().all(Option::is_none) {
            self.ended = Some(self.stopped);
            self.active = false;
            self.stopped = false;
        }
    }

    fn restart_loop(&mut self, v: &mut Voice, data: &ClipData, stream: Option<&mut StreamEnds>) {
        v.generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        v.pos = 0;
        if let Some(st) = stream {
            let _ = st.cmds.push(StreamCmd::Start {
                generation: v.generation,
                from: data.len() as u64,
            });
        }
    }
}

/// Frame `pos` of generation `generation` from the stream, discarding stale chunks.
fn read_stream(st: &mut StreamEnds, generation: u32, pos: u64) -> Streamed {
    loop {
        let Ok(chunk) = st.data.peek() else {
            return Streamed::Wait;
        };
        if chunk.generation != generation {
            // Generations only grow: an older chunk is garbage, a newer one means this
            // voice was replaced.
            if is_older(chunk.generation, generation) {
                let _ = st.data.pop();
                continue;
            }
            return Streamed::End;
        }
        if pos < chunk.start {
            return Streamed::Wait;
        }
        let off = (pos - chunk.start) as usize;
        if off < chunk.len {
            return Streamed::Frame(chunk.frames[off]);
        }
        if chunk.end {
            return Streamed::End;
        }
        let _ = st.data.pop();
    }
}

fn is_older(a: u32, b: u32) -> bool {
    (b.wrapping_sub(a) as i32) > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(len: usize) -> ClipData {
        // A ramp, so positions are easy to check: frame i = i / len.
        let f: Vec<[f32; 2]> = (0..len)
            .map(|i| [i as f32 / len as f32, -(i as f32) / len as f32])
            .collect();
        ClipData::from_frames(&f, false)
    }

    fn dc(len: usize, v: f32) -> ClipData {
        ClipData::from_frames(&vec![[v, v]; len], false)
    }

    fn settings(polyphony: u8, retrigger: Retrigger, looped: bool) -> ClipSettings {
        ClipSettings {
            polyphony,
            retrigger,
            looped,
        }
    }

    fn render(st: &mut ClipState, data: &ClipData, s: ClipSettings, n: usize) -> Vec<f32> {
        let (mut l, mut r) = (vec![0.0; n], vec![0.0; n]);
        st.render(data, s, None, &mut l, &mut r);
        l
    }

    fn max_step(x: &[f32]) -> f32 {
        x.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
    }

    #[test]
    fn plays_once_then_reports_ended() {
        let (data, s) = (clip(100), settings(1, Retrigger::Restart, false));
        let mut st = ClipState::default();
        st.apply(ClipAction::Play, s, &data, None);
        let out = render(&mut st, &data, s, 64);
        assert_eq!(out[10], 10.0 / 100.0);
        assert_eq!(st.take_ended(), None);
        let out = render(&mut st, &data, s, 64);
        assert_eq!(out[35], 99.0 / 100.0, "last frame");
        assert!(out[36..].iter().all(|&v| v == 0.0), "silence after the end");
        assert_eq!(st.take_ended(), Some(false), "finished, not stopped");
        assert_eq!(st.take_ended(), None, "reported once");
    }

    #[test]
    fn stop_fades_out_and_reports_stopped() {
        let (data, s) = (dc(10_000, 1.0), settings(1, Retrigger::Restart, false));
        let mut st = ClipState::default();
        st.apply(ClipAction::Play, s, &data, None);
        render(&mut st, &data, s, 64);
        st.apply(ClipAction::Stop, s, &data, None);
        let out = render(&mut st, &data, s, 512);
        assert!(max_step(&out) <= 1.0 / CLIP_FADE_FRAMES as f32 + 1e-6, "no click");
        assert!(out[CLIP_FADE_FRAMES as usize..].iter().all(|&v| v == 0.0));
        assert_eq!(st.take_ended(), Some(true));
    }

    #[test]
    fn restart_crossfades_without_a_click() {
        let (data, s) = (clip(48_000), settings(1, Retrigger::Restart, false));
        let mut st = ClipState::default();
        st.apply(ClipAction::Play, s, &data, None);
        let before = render(&mut st, &data, s, 24_000);
        st.apply(ClipAction::Play, s, &data, None);
        let after = render(&mut st, &data, s, 512);
        // The old voice (at 0.5) fades out while the new one starts at 0.
        let joined: Vec<f32> = before[before.len() - 1..].iter().chain(&after).copied().collect();
        assert!(
            max_step(&joined) < 0.5 / CLIP_FADE_FRAMES as f32 + 2.0 / 48_000.0,
            "click {}",
            max_step(&joined)
        );
        assert!((after[400] - 400.0 / 48_000.0).abs() < 1e-5, "new voice from the top");
        assert_eq!(st.playing(), 1);
    }

    #[test]
    fn overlap_respects_polyphony_by_stealing_the_oldest() {
        let (data, s) = (dc(48_000, 0.25), settings(3, Retrigger::Overlap, false));
        let mut st = ClipState::default();
        for _ in 0..5 {
            st.apply(ClipAction::Play, s, &data, None);
            render(&mut st, &data, s, 64);
        }
        let out = render(&mut st, &data, s, 512);
        assert_eq!(st.playing(), 3);
        assert!((out[500] - 0.75).abs() < 1e-6, "three voices of 0.25: {}", out[500]);
    }

    #[test]
    fn ignore_mode_keeps_playing() {
        let (data, s) = (clip(48_000), settings(4, Retrigger::Ignore, false));
        let mut st = ClipState::default();
        st.apply(ClipAction::Play, s, &data, None);
        render(&mut st, &data, s, 1000);
        st.apply(ClipAction::Play, s, &data, None);
        let out = render(&mut st, &data, s, 10);
        assert_eq!(st.playing(), 1);
        assert_eq!(out[0], 1000.0 / 48_000.0, "position kept");
    }

    #[test]
    fn loop_wraps_seamlessly_and_never_ends() {
        let (data, s) = (clip(100), settings(1, Retrigger::Restart, true));
        let mut st = ClipState::default();
        st.apply(ClipAction::Play, s, &data, None);
        let out = render(&mut st, &data, s, 350);
        assert_eq!(out[99], 0.99);
        assert_eq!(out[100], 0.0, "back to the top");
        assert_eq!(out[349], 0.49);
        assert_eq!(st.take_ended(), None);
    }

    #[test]
    fn seek_jumps_with_a_crossfade() {
        let (data, s) = (clip(48_000), settings(1, Retrigger::Restart, false));
        let mut st = ClipState::default();
        st.apply(ClipAction::Play, s, &data, None);
        render(&mut st, &data, s, 64);
        st.apply(ClipAction::Seek { frame: 24_000 }, s, &data, None);
        let out = render(&mut st, &data, s, 512);
        assert!((out[300] - 24_300.0 / 48_000.0).abs() < 1e-5, "now at the seek target");
        assert!(
            max_step(&out) < 0.5 / CLIP_FADE_FRAMES as f32 + 2.0 / 48_000.0,
            "fades in from the crossfade"
        );
        st.apply(ClipAction::Seek { frame: 99_999_999 }, s, &data, None);
        render(&mut st, &data, s, 512);
        assert_eq!(st.playing(), 0, "seeking past the end stops");
    }

    #[test]
    fn voice_slots_never_run_out() {
        let (data, s) = (dc(48_000, 0.1), settings(MAX_POLYPHONY, Retrigger::Overlap, false));
        let mut st = ClipState::default();
        for _ in 0..200 {
            st.apply(ClipAction::Play, s, &data, None); // no render between: fades pile up
        }
        assert!(st.playing() <= MAX_POLYPHONY as usize);
        render(&mut st, &data, s, 64);
    }
}
