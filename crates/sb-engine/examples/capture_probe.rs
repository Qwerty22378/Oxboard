//! How a capture device really delivers: opens it the way the engine does (shared, 48 kHz
//! stereo f32, `AUTOCONVERTPCM`, event-driven) and records every packet's size and arrival,
//! then sums up: the device's own format and period, packet sizes, gaps between wake-ups, the
//! real rate against the wall clock, and the worst stretch with no data.
//!
//!   capture_probe [NAME] [--seconds N] [--trace FILE]
//!
//! NAME: part of the device name (default: the default mic). `--trace` writes every packet as
//! `microseconds,frames` (arrival since the first packet), for replaying in `sim.rs`.
//!
//! Written to find out why the CABLE Output ring kept running dry (drift report, M6).

#[cfg(windows)]
fn main() {
    probe::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only");
}

#[cfg(windows)]
mod probe {
    use std::time::{Duration, Instant};
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::Media::Audio::*;
    use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
    use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
    use windows::Win32::System::Com::*;
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    pub fn run() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let seconds: u64 = args
            .iter()
            .position(|a| a == "--seconds")
            .and_then(|i| args.get(i + 1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(20);
        let name = args.first().filter(|a| !a.starts_with("--")).cloned();
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
            let device = match &name {
                None => en.GetDefaultAudioEndpoint(eCapture, eConsole).unwrap(),
                Some(n) => {
                    let all = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE).unwrap();
                    (0..all.GetCount().unwrap())
                        .map(|i| all.Item(i).unwrap())
                        .find(|d| {
                            d.OpenPropertyStore(STGM_READ)
                                .and_then(|s| s.GetValue(&PKEY_Device_FriendlyName))
                                .map(|v| v.to_string().to_lowercase().contains(&n.to_lowercase()))
                                .unwrap_or(false)
                        })
                        .expect("no capture device by that name")
                }
            };
            let friendly = device
                .OpenPropertyStore(STGM_READ)
                .and_then(|s| s.GetValue(&PKEY_Device_FriendlyName))
                .map(|v| v.to_string())
                .unwrap_or_default();
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).unwrap();
            let mix = client.GetMixFormat().unwrap();
            let (mix_rate, mix_ch, mix_bits) = ((*mix).nSamplesPerSec, (*mix).nChannels, (*mix).wBitsPerSample);
            CoTaskMemFree(Some(mix as *const _));
            let (mut default_p, mut min_p) = (0i64, 0i64);
            client.GetDevicePeriod(Some(&mut default_p), Some(&mut min_p)).unwrap();

            let block_align = 8u16;
            let format = WAVEFORMATEXTENSIBLE {
                Format: WAVEFORMATEX {
                    wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
                    nChannels: 2,
                    nSamplesPerSec: 48_000,
                    nAvgBytesPerSec: 48_000 * block_align as u32,
                    nBlockAlign: block_align,
                    wBitsPerSample: 32,
                    cbSize: 22,
                },
                Samples: WAVEFORMATEXTENSIBLE_0 {
                    wValidBitsPerSample: 32,
                },
                dwChannelMask: 3,
                SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
            };
            let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            client
                .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, default_p, 0, &format.Format, None)
                .unwrap();
            let event = CreateEventW(None, false, false, None).unwrap();
            client.SetEventHandle(event).unwrap();
            let buffer = client.GetBufferSize().unwrap();
            let cap: IAudioCaptureClient = client.GetService().unwrap();
            println!("device: {friendly}");
            println!(
                "its own format: {mix_rate} Hz, {mix_ch} ch, {mix_bits} bit; period default {:.2} ms, min {:.2} ms; our buffer {buffer} frames",
                default_p as f64 / 1e4,
                min_p as f64 / 1e4
            );

            // (arrival, frames, silent, discontinuity) per packet; wake-up times.
            let mut packets: Vec<(Instant, u32, bool, bool)> = Vec::new();
            let mut wakes: Vec<Instant> = Vec::new();
            let start = Instant::now();
            client.Start().unwrap();
            while start.elapsed() < Duration::from_secs(seconds) {
                if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
                    wakes.push(Instant::now());
                    continue;
                }
                let now = Instant::now();
                wakes.push(now);
                loop {
                    let n = cap.GetNextPacketSize().unwrap();
                    if n == 0 {
                        break;
                    }
                    let mut data = std::ptr::null_mut();
                    let (mut len, mut fl) = (0u32, 0u32);
                    cap.GetBuffer(&mut data, &mut len, &mut fl, None, None).unwrap();
                    packets.push((
                        now,
                        len,
                        fl & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0,
                        fl & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0,
                    ));
                    cap.ReleaseBuffer(len).unwrap();
                }
            }
            let _ = client.Stop();
            report(&packets, &wakes, start);
            if let Some(path) = args.iter().position(|a| a == "--trace").and_then(|i| args.get(i + 1)) {
                let t0 = packets[0].0;
                let text: String = packets
                    .iter()
                    .map(|p| format!("{},{}\n", p.0.duration_since(t0).as_micros(), p.1))
                    .collect();
                std::fs::write(path, text).unwrap();
                println!("trace: {} packets in {path}", packets.len());
            }
        }
    }

    fn report(packets: &[(Instant, u32, bool, bool)], wakes: &[Instant], start: Instant) {
        if packets.len() < 2 {
            println!("too few packets: {}", packets.len());
            return;
        }
        // Skip the first second (start-up).
        let settled: Vec<_> = packets
            .iter()
            .filter(|p| p.0.duration_since(start) > Duration::from_secs(1))
            .collect();
        let (first, last) = (settled[0].0, settled[settled.len() - 1].0);
        let frames: u64 = settled.iter().skip(1).map(|p| p.1 as u64).sum();
        let span = last.duration_since(first).as_secs_f64();
        let rate = frames as f64 / span;
        println!(
            "{} packets over {:.1} s: {:.1} frames/s = {:+.0} ppm against the wall clock",
            settled.len(),
            span,
            rate,
            (rate / 48_000.0 - 1.0) * 1e6
        );
        let mut sizes: Vec<u32> = settled.iter().map(|p| p.1).collect();
        sizes.sort();
        let pct = |v: &[u32], q: f64| v[((v.len() - 1) as f64 * q) as usize];
        println!(
            "packet frames: min {} median {} p99 {} max {}",
            sizes[0],
            pct(&sizes, 0.5),
            pct(&sizes, 0.99),
            sizes[sizes.len() - 1]
        );
        let silent = settled.iter().filter(|p| p.2).count();
        let disc = settled.iter().filter(|p| p.3).count();
        println!("silent-flagged packets {silent}, discontinuities {disc}");
        // Gaps between wake-ups that brought data.
        let mut arrivals: Vec<Instant> = settled.iter().map(|p| p.0).collect();
        arrivals.dedup();
        let mut gaps: Vec<u32> = arrivals
            .windows(2)
            .map(|w| w[1].duration_since(w[0]).as_micros() as u32)
            .collect();
        gaps.sort();
        println!(
            "gap between data wake-ups (ms): min {:.2} median {:.2} p99 {:.2} max {:.2}",
            gaps[0] as f64 / 1e3,
            pct(&gaps, 0.5) as f64 / 1e3,
            pct(&gaps, 0.99) as f64 / 1e3,
            gaps[gaps.len() - 1] as f64 / 1e3
        );
        // How far the delivered count falls behind a steady 48 kHz from the first packet: the
        // ring margin a steady reader would need (the worst lag), and the lead it gets ahead.
        let t0 = settled[0].0;
        let (mut got, mut worst_lag, mut worst_lead) = (0f64, 0f64, 0f64);
        for p in &settled {
            let expect = p.0.duration_since(t0).as_secs_f64() * rate;
            worst_lag = worst_lag.max(expect - got);
            got += p.1 as f64;
            worst_lead = worst_lead.max(got - expect);
        }
        println!(
            "delivery vs a steady clock at that rate: up to {:.1} ms behind, {:.1} ms ahead",
            worst_lag / 48.0,
            worst_lead / 48.0
        );
        let empty = wakes.len() - arrivals.len();
        println!("wake-ups {} (without data: {empty})", wakes.len());
    }
}
