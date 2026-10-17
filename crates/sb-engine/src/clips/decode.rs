//! Decoding sound files to 48 kHz stereo f32, off the audio thread.
//!
//! [`SourceReader`] decodes packet by packet (symphonia) into stereo frames at the file's own
//! rate: mono is duplicated, more than two channels keep the first two (front left/right).
//! [`To48k`] converts the rate with a windowed-sinc kernel built for that exact ratio
//! (`clock::drift::Kernel::conversion`), streaming, so the same code serves whole-file loads
//! and the long-clip streamer.

use crate::clock::drift::{Kernel, Resampler};
use crate::{Frame, SAMPLE_RATE};
use std::fs::File;
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// One file, decoded a packet at a time.
pub struct SourceReader {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track: u32,
    rate: u32,
    buf: Option<SampleBuffer<f32>>,
}

impl SourceReader {
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
            .map_err(|e| format!("{}: not a supported audio file ({e})", path.display()))?;
        let format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| format!("{}: no audio track", path.display()))?;
        let rate = track
            .codec_params
            .sample_rate
            .ok_or_else(|| format!("{}: unknown sample rate", path.display()))?;
        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .map_err(|e| format!("{}: unsupported codec ({e})", path.display()))?;
        let track = track.id;
        Ok(Self {
            format,
            decoder,
            track,
            rate,
            buf: None,
        })
    }

    /// The file's sample rate.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Decode the next packet and append its frames (stereo, file rate) to `out`. `false` at
    /// the end of the file. Corrupt packets are skipped.
    pub fn next_frames(&mut self, out: &mut Vec<Frame>) -> Result<bool, String> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(Error::ResetRequired) => return Ok(false), // chained streams: stop at the first
                Err(e) => return Err(e.to_string()),
            };
            if packet.track_id() != self.track {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(d) => d,
                Err(Error::DecodeError(_)) => continue,
                Err(e) => return Err(e.to_string()),
            };
            let spec = *decoded.spec();
            let channels = spec.channels.count().max(1);
            let need = decoded.capacity() as u64;
            if self
                .buf
                .as_ref()
                .is_none_or(|b| b.capacity() < need as usize * channels)
            {
                self.buf = Some(SampleBuffer::new(need, spec));
            }
            let buf = self.buf.as_mut().expect("just set");
            buf.copy_interleaved_ref(decoded);
            out.extend(buf.samples().chunks_exact(channels).map(|s| match channels {
                1 => [s[0], s[0]],
                _ => [s[0], s[1]],
            }));
            return Ok(true);
        }
    }
}

/// Streaming rate conversion to [`SAMPLE_RATE`]. A pass-through when the file is already
/// 48 kHz.
pub struct To48k {
    rs: Option<Resampler>,
    step: f64,
    tail_flushed: bool,
}

/// Input frames pushed to the converter at once (bounded so its buffer never grows).
const CONVERT_CHUNK: usize = 4096;

impl To48k {
    pub fn new(from_rate: u32) -> Self {
        let rs = (from_rate != SAMPLE_RATE)
            .then(|| Resampler::with_kernel(Kernel::conversion(from_rate, SAMPLE_RATE), CONVERT_CHUNK));
        Self {
            rs,
            step: from_rate as f64 / SAMPLE_RATE as f64,
            tail_flushed: false,
        }
    }

    /// Convert `input` (file rate), appending 48 kHz frames to `out`.
    pub fn push(&mut self, input: &[Frame], out: &mut Vec<Frame>) {
        let Some(rs) = &mut self.rs else {
            out.extend_from_slice(input);
            return;
        };
        for chunk in input.chunks(CONVERT_CHUNK) {
            rs.push_frames(chunk);
            Self::drain(rs, self.step, out);
        }
    }

    /// End of input: push the filter's lookahead worth of silence so the last real frames
    /// come out.
    pub fn finish(&mut self, out: &mut Vec<Frame>) {
        if let Some(rs) = &mut self.rs
            && !std::mem::replace(&mut self.tail_flushed, true)
        {
            let zeros = vec![[0.0f32; 2]; rs.lookahead() + 1];
            rs.push_frames(&zeros);
            Self::drain(rs, self.step, out);
        }
    }

    fn drain(rs: &mut Resampler, step: f64, out: &mut Vec<Frame>) {
        loop {
            let start = out.len();
            out.resize(start + 1024, [0.0; 2]);
            let n = rs.produce(1024, step, |i, l, r| out[start + i] = [l, r]);
            out.truncate(start + n);
            if n == 0 {
                break;
            }
        }
    }
}

/// Decode a whole file (or its first `max_frames` 48 kHz frames). Returns the frames and
/// whether that was the whole file.
pub fn decode(path: &Path, max_frames: Option<usize>) -> Result<(Vec<Frame>, bool), String> {
    let mut reader = SourceReader::open(path)?;
    let mut conv = To48k::new(reader.rate());
    let (mut raw, mut out) = (Vec::new(), Vec::new());
    loop {
        raw.clear();
        if !reader.next_frames(&mut raw)? {
            conv.finish(&mut out);
            return Ok((out, true));
        }
        conv.push(&raw, &mut out);
        if let Some(max) = max_frames
            && out.len() > max
        {
            out.truncate(max);
            return Ok((out, false));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    /// Write a 16-bit PCM WAV of `frames` (per-channel samples, interleaved) for tests.
    pub(crate) fn write_wav(path: &Path, rate: u32, channels: u16, samples: &[f32]) {
        let data: Vec<u8> = samples
            .iter()
            .flat_map(|&s| ((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes())
            .collect();
        let mut f = File::create(path).unwrap();
        let byte_rate = rate * channels as u32 * 2;
        f.write_all(b"RIFF").unwrap();
        f.write_all(&(36 + data.len() as u32).to_le_bytes()).unwrap();
        f.write_all(b"WAVEfmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        f.write_all(&channels.to_le_bytes()).unwrap();
        f.write_all(&rate.to_le_bytes()).unwrap();
        f.write_all(&byte_rate.to_le_bytes()).unwrap();
        f.write_all(&(channels * 2).to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&(data.len() as u32).to_le_bytes()).unwrap();
        f.write_all(&data).unwrap();
    }

    pub(crate) fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sb-engine-tests-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn sine(n: usize, freq: f64, rate: u32, amp: f64) -> Vec<f32> {
        (0..n)
            .map(|i| ((i as f64 * freq / rate as f64) * std::f64::consts::TAU).sin() as f32 * amp as f32)
            .collect()
    }

    /// Frequency from zero crossings (rising), in Hz at 48 kHz.
    fn frequency(frames: &[Frame]) -> f64 {
        let x: Vec<f32> = frames.iter().map(|f| f[0]).collect();
        let ups: Vec<usize> = (1..x.len()).filter(|&i| x[i - 1] < 0.0 && x[i] >= 0.0).collect();
        (ups.len() - 1) as f64 * 48_000.0 / (ups[ups.len() - 1] - ups[0]) as f64
    }

    #[test]
    fn decodes_48k_stereo_wav_exactly() {
        let p = tmp("stereo48.wav");
        let samples: Vec<f32> = (0..2000)
            .flat_map(|i| [i as f32 / 4000.0, -(i as f32) / 4000.0])
            .collect();
        write_wav(&p, 48_000, 2, &samples);
        let (frames, complete) = decode(&p, None).unwrap();
        assert!(complete);
        assert_eq!(frames.len(), 2000);
        for (i, f) in frames.iter().enumerate() {
            assert!(
                (f[0] - i as f32 / 4000.0).abs() < 1e-4 && (f[1] + i as f32 / 4000.0).abs() < 1e-4,
                "frame {i}"
            );
        }
    }

    #[test]
    fn converts_44k1_mono_to_48k_stereo_keeping_pitch_and_level() {
        let p = tmp("mono441.wav");
        write_wav(&p, 44_100, 1, &sine(44_100, 1000.0, 44_100, 0.5));
        let (frames, _) = decode(&p, None).unwrap();
        // One second in, one second out (± the filter's edges).
        assert!((frames.len() as i64 - 48_000).abs() < 4, "{} frames", frames.len());
        assert!(frames.iter().all(|f| f[0] == f[1]), "mono is duplicated");
        let mid = &frames[2000..46_000];
        assert!((frequency(mid) - 1000.0).abs() < 0.5, "{} Hz", frequency(mid));
        let peak = mid.iter().fold(0.0f32, |m, f| m.max(f[0].abs()));
        assert!((peak - 0.5).abs() < 0.005, "peak {peak}");
    }

    #[test]
    fn downsampling_96k_does_not_alias() {
        // 30 kHz is above the 48 kHz Nyquist: it must be filtered out, not fold to 18 kHz.
        let p = tmp("hi96.wav");
        write_wav(&p, 96_000, 1, &sine(96_000, 30_000.0, 96_000, 0.5));
        let (frames, _) = decode(&p, None).unwrap();
        let rms = (frames[2000..46_000].iter().map(|f| (f[0] * f[0]) as f64).sum::<f64>() / 44_000.0).sqrt();
        assert!(rms < 0.5 / 2f64.sqrt() * 0.01, "aliased energy {rms}"); // below −40 dB of the input
    }

    #[test]
    fn max_frames_stops_early() {
        let p = tmp("long.wav");
        write_wav(&p, 48_000, 1, &vec![0.1; 48_000]);
        let (frames, complete) = decode(&p, Some(1000)).unwrap();
        assert_eq!(frames.len(), 1000);
        assert!(!complete);
    }

    #[test]
    fn bad_files_are_errors_not_panics() {
        assert!(decode(Path::new("/nonexistent/clip.wav"), None).is_err());
        let p = tmp("garbage.wav");
        std::fs::write(&p, b"not audio at all").unwrap();
        assert!(decode(&p, None).is_err());
    }

    /// The CC BY-SA `Example.ogg` from Wikimedia Commons (stereo Vorbis, 44.1 kHz, 6.1 s),
    /// if it was downloaded to `target/test-clips/` (it isn't committed). Skipped otherwise.
    #[test]
    fn decodes_the_downloaded_vorbis_example_if_present() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-clips/Example.ogg");
        if !p.exists() {
            eprintln!("skipped: {} not downloaded", p.display());
            return;
        }
        let (frames, complete) = decode(&p, None).unwrap();
        assert!(complete);
        let secs = frames.len() as f64 / 48_000.0;
        assert!((secs - 6.1).abs() < 0.2, "{secs:.2} s");
        let peak = frames.iter().fold(0.0f32, |m, f| m.max(f[0].abs()).max(f[1].abs()));
        assert!(peak > 0.1 && peak <= 1.05, "peak {peak}");
        assert!(frames.iter().any(|f| (f[0] - f[1]).abs() > 1e-3), "really stereo");
    }
}
