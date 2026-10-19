//! What both shells share: where toggles come from, what's in front, where to show, focus
//! handoff, and the probes that fill in the test matrix.

use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetProcessTimes, INFINITE, SetEvent, WaitForSingleObject,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey};
use windows::Win32::UI::Shell::{QUERY_USER_NOTIFICATION_STATE, SHQueryUserNotificationState};
use windows::Win32::UI::WindowsAndMessaging::{
    ASFW_ANY, AllowSetForegroundWindow, CURSOR_SHOWING, CURSORINFO, GetClassNameW, GetClipCursor, GetCursorInfo,
    GetForegroundWindow, GetMessageW, GetSystemMetrics, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, MSG,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_MINIMIZE, SW_RESTORE,
    SetForegroundWindow, ShowWindow, WM_HOTKEY,
};
use windows::core::w;

/// Ctrl+Shift+S.
pub const HOTKEY_VK: u32 = b'S' as u32;
const TOGGLE_EVENT: windows::core::PCWSTR = w!("Local\\sb-ui-spike-toggle");

// --- Log ----------------------------------------------------------------------------------

static LOG: OnceLock<(Instant, Mutex<Option<File>>)> = OnceLock::new();

pub fn open_log(path: Option<&str>) {
    let file = path.map(|p| File::create(p).expect("open the log"));
    let _ = LOG.set((Instant::now(), Mutex::new(file)));
}

/// A line with the time since start, to stderr and the log file.
pub fn log(line: impl AsRef<str>) {
    let Some((start, file)) = LOG.get() else { return };
    let _ = start;
    let line = format!("[{}] {}", clock(), line.as_ref());
    eprintln!("{line}");
    if let Some(f) = file.lock().unwrap().as_mut() {
        let _ = writeln!(f, "{line}");
        let _ = f.flush();
    }
}

/// Wall-clock seconds within the hour (`mm:ss.mmm`, UTC), so logs of several processes line up.
pub fn clock() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let ms = t.as_millis() % 3_600_000;
    format!("{:02}:{:02}.{:03}", ms / 60_000, ms / 1000 % 60, ms % 1000)
}

// --- Toggles ------------------------------------------------------------------------------

/// Where the overlay hotkey is caught.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HotkeySource {
    /// This process registers it (simplest).
    Own,
    /// Another process (`--hotkey-server`, standing in for the controller) catches it and
    /// signals a named event: the real process model, where the UI didn't get the input.
    Remote,
}

/// Call `on_toggle` (on a thread of its own) at every press of the overlay hotkey.
pub fn spawn_toggles(source: HotkeySource, on_toggle: impl Fn(Instant) + Send + 'static) {
    std::thread::Builder::new()
        .name("toggles".into())
        .spawn(move || match source {
            HotkeySource::Own => unsafe {
                if let Err(e) = RegisterHotKey(None, 1, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, HOTKEY_VK) {
                    log(format!("RegisterHotKey failed: {e}"));
                    return;
                }
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    if msg.message == WM_HOTKEY {
                        on_toggle(Instant::now());
                    }
                }
            },
            HotkeySource::Remote => unsafe {
                let event = CreateEventW(None, false, false, TOGGLE_EVENT).expect("the toggle event");
                loop {
                    WaitForSingleObject(event, INFINITE);
                    on_toggle(Instant::now());
                }
            },
        })
        .expect("spawn the toggle thread");
}

/// `--hotkey-server`: catch the hotkey and pass it on, as the controller will.
pub fn hotkey_server(allow_foreground: bool) -> ! {
    unsafe {
        let event: HANDLE = CreateEventW(None, false, false, TOGGLE_EVENT).expect("the toggle event");
        RegisterHotKey(None, 1, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, HOTKEY_VK).expect("RegisterHotKey");
        log(format!(
            "hotkey server: Ctrl+Shift+S → event (AllowSetForegroundWindow: {allow_foreground})"
        ));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_HOTKEY {
                // We got the input, so we may hand the right to take the foreground on.
                if allow_foreground {
                    let r = AllowSetForegroundWindow(ASFW_ANY);
                    log(format!("hotkey: AllowSetForegroundWindow → {r:?}"));
                }
                let _ = SetEvent(event);
            }
        }
        let _ = CloseHandle(event);
    }
    std::process::exit(0)
}

// --- What's in front ----------------------------------------------------------------------

/// The window that had the foreground when the overlay was called up.
#[derive(Debug, Clone)]
pub struct Front {
    pub hwnd: HWND,
    pub title: String,
    pub pid: u32,
    /// `SHQueryUserNotificationState`: the shell's own view of fullscreen apps.
    pub quns: &'static str,
    /// The window covers its whole monitor (borderless or exclusive).
    pub covers_monitor: bool,
    /// Its monitor, in physical pixels, and that monitor's DPI.
    pub monitor: RECT,
    pub dpi: u32,
}

fn quns_name(q: QUERY_USER_NOTIFICATION_STATE) -> &'static str {
    match q.0 {
        1 => "not present (screen saver, locked, or fast user switching)",
        2 => "busy (a fullscreen app)",
        3 => "D3D exclusive fullscreen",
        4 => "presentation mode",
        5 => "accepts notifications",
        6 => "quiet time",
        7 => "UWP app fullscreen",
        _ => "?",
    }
}

pub fn front() -> Front {
    unsafe {
        let hwnd = GetForegroundWindow();
        let mut title = [0u16; 128];
        let n = GetWindowTextW(hwnd, &mut title);
        let mut class = [0u16; 64];
        let c = GetClassNameW(hwnd, &mut class);
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let quns = SHQueryUserNotificationState().map_or("(failed)", quns_name);
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(monitor, &mut info);
        let (mut dx, mut dy) = (96, 96);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        let m = info.rcMonitor;
        let covers_monitor = r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom;
        Front {
            hwnd,
            title: format!(
                "{} [{}]",
                String::from_utf16_lossy(&title[..n as usize]),
                String::from_utf16_lossy(&class[..c as usize])
            ),
            pid,
            quns,
            covers_monitor,
            monitor: m,
            dpi: dx,
        }
    }
}

/// Where a panel of `size` logical pixels goes on the front window's monitor: centered,
/// in physical pixels. Returns (x, y, w, h, scale).
pub fn placement(f: &Front, size: (f32, f32)) -> (i32, i32, i32, i32, f32) {
    let scale = f.dpi as f32 / 96.0;
    let (w, h) = ((size.0 * scale).round() as i32, (size.1 * scale).round() as i32);
    let m = f.monitor;
    let x = m.left + ((m.right - m.left) - w) / 2;
    let y = m.top + ((m.bottom - m.top) - h) / 2;
    (x, y, w, h, scale)
}

/// The test matrix's columns, right after showing: is the overlay in front, and is the
/// cursor free (not clipped, visible)?
pub fn probe(ours: HWND, label: &str) {
    unsafe {
        let fg = GetForegroundWindow();
        let mut clip = RECT::default();
        let _ = GetClipCursor(&mut clip);
        let screen = RECT {
            left: GetSystemMetrics(SM_XVIRTUALSCREEN),
            top: GetSystemMetrics(SM_YVIRTUALSCREEN),
            right: GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN),
            bottom: GetSystemMetrics(SM_YVIRTUALSCREEN) + GetSystemMetrics(SM_CYVIRTUALSCREEN),
        };
        let clipped = clip != screen;
        let mut ci = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetCursorInfo(&mut ci);
        let shown = ci.flags.0 & CURSOR_SHOWING.0 != 0;
        log(format!(
            "{label}: foreground is {} | cursor {} and {} at ({}, {})",
            if fg == ours {
                "the overlay".to_string()
            } else {
                describe(fg)
            },
            if clipped {
                format!("CLIPPED to {clip:?}")
            } else {
                "not clipped".into()
            },
            if shown { "visible" } else { "HIDDEN" },
            ci.ptScreenPos.x,
            ci.ptScreenPos.y,
        ));
    }
}

fn describe(hwnd: HWND) -> String {
    unsafe {
        let mut title = [0u16; 128];
        let n = GetWindowTextW(hwnd, &mut title);
        format!("{:?} \"{}\"", hwnd.0, String::from_utf16_lossy(&title[..n as usize]))
    }
}

/// How the overlay deals with a game in exclusive fullscreen ([`Front::quns`] says D3D).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Exclusive {
    /// Show anyway (to see what happens).
    Show,
    /// Minimize the game first, restore it on hide ("alt-tab style").
    Minimize,
}

/// Hand the foreground back to the window that had it (and un-minimize it if we did that).
pub fn give_back(f: &Front, minimized: bool) {
    unsafe {
        if minimized {
            let _ = ShowWindow(f.hwnd, SW_RESTORE);
        }
        let ok = SetForegroundWindow(f.hwnd).as_bool();
        // Let the switch land before looking.
        std::thread::sleep(Duration::from_millis(50));
        let fg = GetForegroundWindow();
        log(format!(
            "hidden: gave the foreground back to {} → {} (foreground now {})",
            describe(f.hwnd),
            if ok { "accepted" } else { "REFUSED" },
            if fg == f.hwnd {
                "the game".to_string()
            } else {
                describe(fg)
            }
        ));
    }
}

pub fn minimize(f: &Front) {
    unsafe {
        let _ = ShowWindow(f.hwnd, SW_MINIMIZE);
    }
}

/// Take the foreground for `ours`; logs whether Windows let us.
pub fn take_foreground(ours: HWND) -> bool {
    let ok = unsafe { SetForegroundWindow(ours).as_bool() };
    if !ok {
        log("SetForegroundWindow(overlay) REFUSED");
    }
    ok
}

// --- Process cost -------------------------------------------------------------------------

/// Every `every`, log this process's CPU use since the last line and its working set,
/// tagged with whether the overlay was visible.
pub fn spawn_stats(visible: Arc<AtomicBool>, every: Duration) {
    std::thread::Builder::new()
        .name("stats".into())
        .spawn(move || {
            let cpu = || unsafe {
                let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
                let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
                let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
                t(k) + t(u)
            };
            let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
            let (mut last_cpu, mut last) = (cpu(), Instant::now());
            loop {
                std::thread::sleep(every);
                let (now_cpu, now) = (cpu(), Instant::now());
                let pct = (now_cpu - last_cpu) / now.duration_since(last).as_secs_f64() * 100.0;
                let mut mem = PROCESS_MEMORY_COUNTERS {
                    cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                    ..Default::default()
                };
                let _ = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut mem, mem.cb) };
                log(format!(
                    "stats ({}): CPU {pct:.1}% of one core ({:.2}% of {cores} cores), working set {:.1} MB, private {:.1} MB",
                    if visible.load(Ordering::Relaxed) { "visible" } else { "hidden" },
                    pct / cores,
                    mem.WorkingSetSize as f64 / 1048576.0,
                    mem.PagefileUsage as f64 / 1048576.0,
                ));
                (last_cpu, last) = (now_cpu, now);
            }
        })
        .expect("spawn the stats thread");
}
