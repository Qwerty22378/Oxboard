//! WASAPI shared-mode streams, one thread per device, event-driven, MMCSS "Pro Audio".
//!
//! Every stream is opened as 48 kHz stereo f32 with `AUTOCONVERTPCM`, so Windows' audio
//! engine converts to and from the device's mix format (rate, channel count, sample type).
//! Our rings therefore always carry [`Frame`]s at [`SAMPLE_RATE`].
//!
//! Each thread creates its own COM objects from the device ID, so nothing COM crosses
//! threads. Setup errors are reported back to the spawner before the stream starts; after
//! that the thread runs until stopped or until the device disappears.

use crate::graph::compile::{EndpointKey, Endpoints};
use crate::graph::spec::NodeId;
use crate::rt::ring::{RingReader, RingSizing, RingWriter, now_ns};
use crate::rt::stats::StreamStats;
use crate::{Frame, SAMPLE_RATE};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, STGM_READ,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject,
};
use windows::core::{Error, HSTRING, PCWSTR, Result, w};

/// Events wake the threads every device period; this timeout only bounds how long a
/// stop request or a silent device can go unnoticed.
const WAIT_MS: u32 = 200;
const KSAUDIO_SPEAKER_STEREO: u32 = 0x3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Capture,
    Render,
}

impl Direction {
    fn flow(self) -> EDataFlow {
        match self {
            Direction::Capture => eCapture,
            Direction::Render => eRender,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Stable endpoint ID string (`{0.0.1.00000000}.{guid}`).
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// What a stream ended up with after setup.
#[derive(Debug, Clone, Copy)]
pub struct StreamInfo {
    /// Device period in frames: how much moves per wake-up.
    pub period_frames: usize,
    /// Size of the WASAPI endpoint buffer in frames.
    pub buffer_frames: usize,
}

/// A running device thread. Dropping it stops the thread and waits for it.
pub struct Stream {
    pub info: StreamInfo,
    pub stats: Arc<StreamStats>,
    thread: RtThread,
}

impl Stream {
    /// True once the thread has exited (stopped, or the device failed).
    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    /// Stop the thread and return how it ended.
    pub fn stop(mut self) -> Result<()> {
        self.thread.join()
    }
}

/// A real-time thread with COM initialized, a stop flag, and a join that reports how it ended.
/// Dropping it stops the thread and waits for it.
pub(crate) struct RtThread {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<()>>>,
}

impl RtThread {
    /// Spawn `name`: COM, then `setup` on the new thread, whose `R` (or error) is handed back
    /// before `run` starts. `S` stays on the thread, so COM objects never cross threads.
    pub(crate) fn spawn<S, R: Send + 'static>(
        name: &str,
        setup: impl FnOnce() -> Result<(S, R)> + Send + 'static,
        run: impl FnOnce(S, &AtomicBool) -> Result<()> + Send + 'static,
    ) -> Result<(Self, R)> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<R>>(1);
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name(name.to_owned())
                .spawn(move || -> Result<()> {
                    let com = match ComGuard::new() {
                        Ok(com) => com,
                        Err(e) => {
                            let _ = ready_tx.send(Err(e.clone()));
                            return Err(e);
                        }
                    };
                    let state = match setup() {
                        Ok((state, ready)) => {
                            let _ = ready_tx.send(Ok(ready));
                            state
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e.clone()));
                            return Err(e);
                        }
                    };
                    let result = run(state, &stop); // consumes `state`: COM objects go before `com`
                    drop(com);
                    result
                })
                .map_err(|e| Error::new(E_FAIL, e.to_string()))?
        };
        let me = Self {
            stop,
            thread: Some(thread),
        };
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(ready)) => Ok((me, ready)),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(Error::new(E_FAIL, format!("{name} thread didn't start"))),
        }
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }

    pub(crate) fn join(&mut self) -> Result<()> {
        self.stop.store(true, Relaxed);
        match self.thread.take().map(|t| t.join()) {
            Some(Ok(result)) => result,
            Some(Err(_)) => Err(Error::new(E_FAIL, "audio thread panicked")),
            None => Ok(()),
        }
    }
}

impl Drop for RtThread {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

/// COM for the calling thread, released on drop. Every thread that uses WASAPI needs one.
pub struct ComGuard(());

impl ComGuard {
    pub fn new() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        Ok(Self(()))
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

fn device_id(device: &IMMDevice) -> Result<String> {
    unsafe {
        let raw = device.GetId()?;
        let id = raw.to_string();
        CoTaskMemFree(Some(raw.0 as *const _));
        id.map_err(|e| Error::new(E_FAIL, e.to_string()))
    }
}

fn device_name(device: &IMMDevice) -> Result<String> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ)?;
        Ok(store.GetValue(&PKEY_Device_FriendlyName)?.to_string())
    }
}

/// Active endpoints in one direction. Needs COM on the calling thread ([`ComGuard`]).
pub fn list_devices(dir: Direction) -> Result<Vec<DeviceInfo>> {
    let en = enumerator()?;
    let default_id = unsafe { en.GetDefaultAudioEndpoint(dir.flow(), eConsole) }
        .and_then(|d| device_id(&d))
        .ok();
    let collection = unsafe { en.EnumAudioEndpoints(dir.flow(), DEVICE_STATE_ACTIVE)? };
    let mut out = Vec::new();
    for i in 0..unsafe { collection.GetCount()? } {
        let device = unsafe { collection.Item(i)? };
        let id = device_id(&device)?;
        out.push(DeviceInfo {
            is_default: default_id.as_deref() == Some(id.as_str()),
            name: device_name(&device)?,
            id,
        });
    }
    Ok(out)
}

/// Pick a device: `None` → the default endpoint; otherwise an exact ID or a
/// case-insensitive substring of the friendly name (must be unambiguous).
pub fn find_device(dir: Direction, query: Option<&str>) -> Result<DeviceInfo> {
    let devices = list_devices(dir)?;
    let not_found = |msg: String| Error::new(AUDCLNT_E_DEVICE_INVALIDATED, msg);
    let Some(q) = query else {
        return devices
            .into_iter()
            .find(|d| d.is_default)
            .ok_or_else(|| not_found(format!("no default {dir:?} device")));
    };
    if let Some(d) = devices.iter().find(|d| d.id == q) {
        return Ok(d.clone());
    }
    let q = q.to_lowercase();
    let matches: Vec<_> = devices
        .into_iter()
        .filter(|d| d.name.to_lowercase().contains(&q))
        .collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        0 => Err(not_found(format!("no {dir:?} device matches '{q}'"))),
        _ => Err(not_found(format!(
            "'{q}' matches several {dir:?} devices: {}",
            matches.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ")
        ))),
    }
}

/// The device's default period in frames: how much audio each wake-up moves.
pub fn default_period_frames(device_id: &str) -> Result<usize> {
    unsafe {
        let client = activate(device_id)?;
        let mut period = 0i64;
        client.GetDevicePeriod(Some(&mut period), None)?;
        Ok(hns_to_frames(period))
    }
}

fn activate(device_id: &str) -> Result<IAudioClient> {
    unsafe {
        enumerator()?
            .GetDevice(PCWSTR(HSTRING::from(device_id).as_ptr()))?
            .Activate(CLSCTX_ALL, None)
    }
}

fn hns_to_frames(hns: i64) -> usize {
    (hns.max(0) as u64 * SAMPLE_RATE as u64 / 10_000_000) as usize
}

/// [`Endpoints`] backed by WASAPI streams: binding a device node opens the device and starts
/// its capture or render thread; releasing it stops the thread. Must outlive the engine's
/// plans (the `Engine` owns it).
pub struct WasapiEndpoints {
    block: usize,
    mix_burst: usize,
    next_key: EndpointKey,
    /// Open device threads: (key, node, stream).
    pub streams: Vec<(EndpointKey, NodeId, Stream)>,
}

impl WasapiEndpoints {
    /// Rings are sized for the timer clock until [`Endpoints::master_changed`] says otherwise.
    pub fn new(block: usize) -> Self {
        Self {
            block,
            mix_burst: block,
            next_key: 0,
            streams: Vec::new(),
        }
    }

    fn add(&mut self, node: NodeId, stream: Stream) -> EndpointKey {
        self.next_key += 1;
        self.streams.push((self.next_key, node, stream));
        self.next_key
    }
}

impl Endpoints for WasapiEndpoints {
    fn input(&mut self, node: NodeId, device: &str) -> std::result::Result<(RingReader, EndpointKey), String> {
        let mut open = || -> Result<(RingReader, EndpointKey)> {
            let (writer, reader) = RingSizing::for_bursts(default_period_frames(device)?, self.mix_burst).ring();
            let stream = spawn_capture(device, writer)?;
            Ok((reader, self.add(node, stream)))
        };
        open().map_err(|e| e.message())
    }

    fn output(&mut self, node: NodeId, device: &str) -> std::result::Result<(RingWriter, EndpointKey), String> {
        let mut open = || -> Result<(RingWriter, EndpointKey)> {
            let (writer, reader) = RingSizing::for_bursts(self.mix_burst, default_period_frames(device)?).ring();
            let stream = spawn_render(device, reader)?;
            Ok((writer, self.add(node, stream)))
        };
        open().map_err(|e| e.message())
    }

    fn release(&mut self, key: EndpointKey) {
        // Dropping the stream stops its thread.
        self.streams.retain(|(k, ..)| *k != key);
    }

    /// A device master renders its whole period as back-to-back blocks; the timer one block.
    fn master_changed(&mut self, device: Option<&str>) {
        self.mix_burst = device
            .and_then(|id| default_period_frames(id).ok())
            .map_or(self.block, |period| period.div_ceil(self.block) * self.block);
    }
}

/// Start a capture thread that writes the device's audio into `writer`.
pub fn spawn_capture(device_id: &str, writer: RingWriter) -> Result<Stream> {
    spawn_stream(device_id, "sb-capture", move |client, stats, stop| {
        capture_loop(client, writer, stats, stop)
    })
}

/// Start a render thread that plays what `reader` delivers.
pub fn spawn_render(device_id: &str, reader: RingReader) -> Result<Stream> {
    spawn_stream(device_id, "sb-render", move |client, stats, stop| {
        render_loop(client, reader, stats, stop)
    })
}

fn spawn_stream(
    device_id: &str,
    name: &str,
    run: impl FnOnce(&Client, &StreamStats, &AtomicBool) -> Result<()> + Send + 'static,
) -> Result<Stream> {
    let id = device_id.to_owned();
    let stats = Arc::new(StreamStats::default());
    let thread_stats = stats.clone();
    let (thread, info) = RtThread::spawn(
        name,
        move || {
            let client = Client::open(&id)?;
            let info = client.info;
            Ok(((Mmcss::register(&thread_stats.mmcss), client, thread_stats), info))
        },
        move |(_mmcss, client, stats), stop| run(&client, &stats, stop),
    )?;
    Ok(Stream { info, stats, thread })
}

/// MMCSS "Pro Audio" registration for the current thread, reverted on drop. `ok` records
/// whether Windows granted it.
pub(crate) struct Mmcss(Option<HANDLE>);

impl Mmcss {
    pub(crate) fn register(ok: &AtomicBool) -> Self {
        let mut task_index = 0u32;
        let handle = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) }.ok();
        ok.store(handle.is_some(), Relaxed);
        Self(handle)
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        if let Some(h) = self.0 {
            let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
        }
    }
}

/// An initialized, event-driven shared-mode IAudioClient.
pub(crate) struct Client {
    client: IAudioClient,
    event: HANDLE,
    pub(crate) info: StreamInfo,
}

impl Client {
    pub(crate) fn open(device_id: &str) -> Result<Self> {
        unsafe {
            let client = activate(device_id)?;

            let mut default_period = 0i64; // 100 ns units
            client.GetDevicePeriod(Some(&mut default_period), None)?;

            let format = stereo_f32();
            let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, default_period, 0, &format.Format, None)?;

            let event = CreateEventW(None, false, false, None)?;
            let client = Self {
                info: StreamInfo {
                    period_frames: hns_to_frames(default_period),
                    buffer_frames: client.GetBufferSize()? as usize,
                },
                client,
                event,
            };
            client.client.SetEventHandle(client.event)?;
            Ok(client)
        }
    }

    /// Wait for the device's event. `false` on timeout.
    pub(crate) fn wait(&self) -> bool {
        unsafe { WaitForSingleObject(self.event, WAIT_MS) == WAIT_OBJECT_0 }
    }

    /// Frames of free space in the endpoint buffer (render).
    pub(crate) fn free_frames(&self) -> Result<usize> {
        let padding = unsafe { self.client.GetCurrentPadding()? } as usize;
        Ok(self.info.buffer_frames.saturating_sub(padding))
    }

    /// Get the render service, fill the endpoint buffer with silence (so the first period
    /// doesn't glitch) and start the stream.
    pub(crate) fn start_render(&self) -> Result<RenderBuffer> {
        unsafe {
            let render: IAudioRenderClient = self.client.GetService()?;
            render.GetBuffer(self.info.buffer_frames as u32)?;
            render.ReleaseBuffer(self.info.buffer_frames as u32, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
            self.client.Start()?;
            Ok(RenderBuffer(render))
        }
    }
}

/// Writes frames into a started render stream.
pub(crate) struct RenderBuffer(IAudioRenderClient);

impl RenderBuffer {
    /// Lend `len` frames of the endpoint buffer to `fill`, then hand them to the device.
    /// `len` must not exceed [`Client::free_frames`].
    pub(crate) fn write(&self, len: usize, fill: impl FnOnce(&mut [Frame])) -> Result<()> {
        unsafe {
            let data = self.0.GetBuffer(len as u32)?;
            // Stream format is stereo f32 (we asked for it), so the buffer is `len` Frames.
            fill(std::slice::from_raw_parts_mut(data as *mut Frame, len));
            self.0.ReleaseBuffer(len as u32, 0)
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

fn stereo_f32() -> WAVEFORMATEXTENSIBLE {
    let block_align = 2 * size_of::<f32>() as u16;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: 2,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * block_align as u32,
            nBlockAlign: block_align,
            wBitsPerSample: 32,
            cbSize: (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: 32,
        },
        dwChannelMask: KSAUDIO_SPEAKER_STEREO,
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

// ---- real-time loops: no allocation, no locks, no panics past this point ----

fn capture_loop(c: &Client, mut writer: RingWriter, stats: &StreamStats, stop: &AtomicBool) -> Result<()> {
    let capture: IAudioCaptureClient = unsafe { c.client.GetService()? };
    unsafe { c.client.Start()? };
    while !stop.load(Relaxed) {
        if !c.wait() {
            continue;
        }
        loop {
            let packet = unsafe { capture.GetNextPacketSize()? };
            if packet == 0 {
                break;
            }
            let mut data = std::ptr::null_mut();
            let (mut len, mut flags) = (0u32, 0u32);
            unsafe { capture.GetBuffer(&mut data, &mut len, &mut flags, None, None)? };
            let len = len as usize;
            if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                StreamStats::add(&stats.discontinuities, 1);
            }
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                writer.write(None, len, stats);
            } else {
                // Stream format is stereo f32 (we asked for it), so the buffer is `len` Frames.
                let frames = unsafe { std::slice::from_raw_parts(data as *const Frame, len) };
                writer.write(Some(frames), len, stats);
            }
            unsafe { capture.ReleaseBuffer(len as u32)? };
            writer.stamp(now_ns());
        }
    }
    Ok(())
}

fn render_loop(c: &Client, mut reader: RingReader, stats: &StreamStats, stop: &AtomicBool) -> Result<()> {
    let render = c.start_render()?;
    while !stop.load(Relaxed) {
        if !c.wait() {
            continue;
        }
        let len = c.free_frames()?;
        if len > 0 {
            render.write(len, |out| reader.read(out, stats))?;
            reader.stamp(now_ns());
        }
    }
    Ok(())
}
