//! The layered-window shell (see `mod.rs`). Input reaches egui through the window procedure;
//! frames go out through `UpdateLayeredWindow` from a DIB section the rasterizer fills.

use super::Content;
use super::raster::{Damage, Raster};
use crate::log;
use std::cell::RefCell;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CreateCompatibleDC,
    CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetMonitorInfoW, HBITMAP, HDC, HGDIOBJ,
    MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow, SelectObject,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey, ReleaseCapture, SetCapture, VK_ESCAPE,
};
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

const WM_WAKE: u32 = WM_APP + 1;
const WM_TOGGLE: u32 = WM_APP + 2;
const HOTKEY_ID: i32 = 1;
/// Ctrl+Shift+S, until the controller owns the hotkeys (M6).
const HOTKEY_VK: u32 = b'S' as u32;

pub struct Options {
    /// Register the overlay hotkey here (until the controller owns it).
    pub hotkey: bool,
    /// Show at startup (development; normally it starts hidden).
    pub show_at_start: bool,
}

/// Wakes the shell's thread from anywhere: [`Content::update`] runs, and a frame is drawn if
/// shown.
#[derive(Clone, Copy)]
pub struct Waker(usize);

impl Waker {
    pub fn wake(&self) {
        let _ = unsafe { PostMessageW(Some(HWND(self.0 as *mut _)), WM_WAKE, WPARAM(0), LPARAM(0)) };
    }

    /// The overlay's window (to own dialogs).
    pub fn window(&self) -> HWND {
        HWND(self.0 as *mut _)
    }

    /// Show if hidden, hide if shown (what the controller will send on the hotkey).
    pub fn toggle(&self) {
        let _ = unsafe { PostMessageW(Some(HWND(self.0 as *mut _)), WM_TOGGLE, WPARAM(0), LPARAM(0)) };
    }
}

thread_local! {
    /// What the window procedure collected since the last frame.
    static INPUT: RefCell<Input> = RefCell::new(Input::default());
}

#[derive(Default)]
struct Input {
    events: Vec<egui::Event>,
    scale: f32,
    focused: bool,
    lost_focus: bool,
    toggles: u32,
    woken: bool,
    dirty: bool,
    /// Files dropped on the overlay.
    dropped: Vec<std::path::PathBuf>,
}

fn push(e: egui::Event) {
    INPUT.with_borrow_mut(|i| {
        i.events.push(e);
        i.dirty = true;
    });
}

fn point(lp: LPARAM) -> egui::Pos2 {
    let x = (lp.0 & 0xffff) as i16 as f32;
    let y = (lp.0 >> 16 & 0xffff) as i16 as f32;
    let s = INPUT.with_borrow(|i| i.scale.max(0.1));
    egui::pos2(x / s, y / s)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let button = |button, pressed| egui::Event::PointerButton {
        pos: point(lp),
        button,
        pressed,
        modifiers: Default::default(),
    };
    match msg {
        WM_MOUSEMOVE => push(egui::Event::PointerMoved(point(lp))),
        WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
            unsafe { SetCapture(hwnd) };
            let b = if msg == WM_LBUTTONDOWN {
                egui::PointerButton::Primary
            } else {
                egui::PointerButton::Secondary
            };
            push(button(b, true));
        }
        WM_LBUTTONUP | WM_RBUTTONUP => {
            let _ = unsafe { ReleaseCapture() };
            let b = if msg == WM_LBUTTONUP {
                egui::PointerButton::Primary
            } else {
                egui::PointerButton::Secondary
            };
            push(button(b, false));
        }
        WM_MOUSEWHEEL => push(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, (wp.0 >> 16) as i16 as f32 / 120.0),
            phase: egui::TouchPhase::Move,
            modifiers: Default::default(),
        }),
        WM_KEYDOWN if wp.0 == VK_ESCAPE.0 as usize => push(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        }),
        WM_ACTIVATE => {
            let active = (wp.0 & 0xffff) as u32 != WA_INACTIVE;
            // Focus going to a window of ours (the file dialog) isn't a click outside.
            let other = HWND(lp.0 as *mut _);
            let ours = !other.is_invalid() && unsafe { GetAncestor(other, GA_ROOTOWNER) } == hwnd;
            INPUT.with_borrow_mut(|i| {
                if i.focused && !active && !ours {
                    i.lost_focus = true;
                }
                i.focused = active;
            });
            push(egui::Event::WindowFocused(active));
        }
        WM_HOTKEY | WM_TOGGLE => INPUT.with_borrow_mut(|i| i.toggles += 1),
        WM_DROPFILES => {
            let drop = HDROP(wp.0 as *mut _);
            let files: Vec<_> = unsafe {
                (0..DragQueryFileW(drop, u32::MAX, None))
                    .filter_map(|n| {
                        let mut buf = vec![0u16; DragQueryFileW(drop, n, None) as usize + 1];
                        let len = DragQueryFileW(drop, n, Some(&mut buf)) as usize;
                        (len > 0).then(|| String::from_utf16_lossy(&buf[..len]).into())
                    })
                    .collect()
            };
            unsafe { DragFinish(drop) };
            INPUT.with_borrow_mut(|i| {
                i.dropped.extend(files);
                i.dirty = true;
            });
        }
        WM_WAKE => INPUT.with_borrow_mut(|i| i.woken = true),
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
}

/// A file dropped on the overlay, as egui takes it.
#[derive(Debug)]
struct Dropped(std::path::PathBuf);

impl egui::DroppedFile for Dropped {
    fn path(&self) -> &std::path::Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|e| e.to_string())
    }
}

/// A DIB section to draw into, selected into a memory DC.
struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    w: i32,
    h: i32,
}

impl Surface {
    fn new(w: i32, h: i32) -> Self {
        unsafe {
            let dc = CreateCompatibleDC(None);
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0).expect("a DIB section");
            let old = SelectObject(dc, bitmap.into());
            Self {
                dc,
                bitmap,
                old,
                bits: bits.cast(),
                w,
                h,
            }
        }
    }

    fn pixels(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, (self.w * self.h) as usize) }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

/// While shown: where, at what scale, and who to give the foreground back to.
struct Shown {
    front: HWND,
    pos: POINT,
    scale: f32,
    /// Not on screen yet: shown after the first frame is drawn.
    first: Option<Instant>,
}

pub struct Shell {
    hwnd: HWND,
    ctx: egui::Context,
    raster: Raster,
    damage: Damage,
    surface: Option<Surface>,
    shown: Option<Shown>,
    start: Instant,
    stats: FrameStats,
}

/// What frames cost, logged every [`FrameStats::EVERY`] frames.
#[derive(Default)]
struct FrameStats {
    frames: u32,
    /// Frames redrawn whole, and with nothing to redraw.
    full: u32,
    idle: u32,
    /// Pixels redrawn, and the panel's size.
    pixels: u64,
    area: u64,
    egui: Duration,
    raster: Duration,
}

impl FrameStats {
    const EVERY: u32 = 90;

    fn add(&mut self, egui: Duration, raster: Duration, damage: &[[i32; 4]], w: usize, h: usize) {
        self.frames += 1;
        self.egui += egui;
        self.raster += raster;
        let area = (w * h) as u64;
        let px: u64 = damage.iter().map(|d| ((d[2] - d[0]) * (d[3] - d[1])) as u64).sum();
        self.full += (px >= area) as u32;
        self.idle += damage.is_empty() as u32;
        (self.pixels, self.area) = (self.pixels + px, self.area + area);
        if self.frames == Self::EVERY {
            let n = self.frames;
            log::line(format!(
                "frames: {n}, {} whole, {} with nothing to draw; {:.1} % of the pixels redrawn; egui {:?}, raster {:?} a frame",
                self.full,
                self.idle,
                self.pixels as f64 * 100.0 / self.area.max(1) as f64,
                self.egui / n,
                self.raster / n,
            ));
            *self = Self::default();
        }
    }
}

impl Shell {
    /// Create the (hidden) window. Call on the thread that will run it.
    pub fn new() -> Self {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let instance = GetModuleHandleW(None).expect("module handle");
            let class = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: w!("sb-ui"),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                w!("sb-ui"),
                w!("Soundboard"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .expect("the overlay window");
            // Audio files can be dropped on it; COM for the file dialog.
            DragAcceptFiles(hwnd, true);
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            Self {
                hwnd,
                ctx: egui::Context::default(),
                raster: Raster::default(),
                damage: Damage::default(),
                surface: None,
                shown: None,
                start: Instant::now(),
                stats: FrameStats::default(),
            }
        }
    }

    pub fn waker(&self) -> Waker {
        Waker(self.hwnd.0 as usize)
    }

    /// Run until the process ends.
    pub fn run(&mut self, content: &mut dyn Content, opts: Options) {
        if opts.hotkey {
            let r = unsafe {
                RegisterHotKey(
                    Some(self.hwnd),
                    HOTKEY_ID,
                    MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT,
                    HOTKEY_VK,
                )
            };
            match r {
                Ok(()) => log::line("overlay hotkey: Ctrl+Shift+S"),
                Err(e) => log::line(format!("can't register Ctrl+Shift+S: {e}")),
            }
        }
        let mut next_frame: Option<Instant> = None;
        if opts.show_at_start {
            self.show(content);
            next_frame = Some(Instant::now());
        }
        let mut msg = MSG::default();
        loop {
            unsafe {
                match next_frame {
                    // Hidden, or nothing to animate: sleep until a message.
                    None => {
                        if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                            return;
                        }
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                    Some(at) => {
                        let wait = at.saturating_duration_since(Instant::now());
                        MsgWaitForMultipleObjects(None, false, wait.as_millis() as u32, QS_ALLINPUT);
                    }
                }
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        return;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            let (toggles, woken, lost) = INPUT.with_borrow_mut(|i| {
                let t = (
                    std::mem::take(&mut i.toggles),
                    std::mem::take(&mut i.woken),
                    i.lost_focus,
                );
                i.lost_focus = false;
                t
            });
            let changed = woken && content.update();
            for _ in 0..toggles {
                if self.shown.is_some() {
                    self.hide(content, "hotkey");
                } else {
                    self.show(content);
                }
            }
            if lost && self.shown.as_ref().is_some_and(|s| s.first.is_none()) {
                self.hide(content, "focus lost");
            }
            if self.shown.is_none() {
                next_frame = None;
                continue;
            }
            let due = next_frame.is_some_and(|t| Instant::now() >= t);
            let dirty = INPUT.with_borrow(|i| i.dirty) || changed || toggles > 0;
            if !(due || dirty) {
                continue;
            }
            let (close, delay) = self.frame(content);
            if close {
                self.hide(content, "Esc");
                next_frame = None;
            } else {
                next_frame = (delay < Duration::from_secs(3600)).then(|| Instant::now() + delay);
            }
        }
    }

    fn show(&mut self, content: &mut dyn Content) {
        let front = unsafe { GetForegroundWindow() };
        let (pos, size, scale) = placement(front, content.size());
        if self.surface.as_ref().is_none_or(|s| (s.w, s.h) != (size.cx, size.cy)) {
            self.surface = Some(Surface::new(size.cx, size.cy));
        }
        INPUT.with_borrow_mut(|i| {
            *i = Input {
                scale,
                dirty: true,
                ..Input::default()
            }
        });
        self.shown = Some(Shown {
            front,
            pos,
            scale,
            first: Some(Instant::now()),
        });
        content.visibility(true);
    }

    fn hide(&mut self, content: &mut dyn Content, why: &str) {
        let Some(s) = self.shown.take() else { return };
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            // Give the game its foreground back (allowed: we have it right now).
            if !s.front.is_invalid() && s.front != self.hwnd {
                let _ = SetForegroundWindow(s.front);
            }
        }
        log::line(format!("hidden ({why})"));
        content.visibility(false);
    }

    /// One egui pass, rasterized and handed to the compositor. Returns whether the content
    /// asked to hide, and when egui wants the next frame.
    fn frame(&mut self, content: &mut dyn Content) -> (bool, Duration) {
        let shown = self.shown.as_mut().expect("shown");
        let surface = self.surface.as_mut().expect("a surface while shown");
        let (events, focused, dropped) = INPUT.with_borrow_mut(|i| {
            i.dirty = false;
            (std::mem::take(&mut i.events), i.focused, std::mem::take(&mut i.dropped))
        });
        let (w, h) = content.size();
        let mut raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w, h))),
            time: Some(self.start.elapsed().as_secs_f64()),
            events,
            focused,
            dropped_files: dropped
                .into_iter()
                .map(|path| std::sync::Arc::new(Dropped(path)) as egui::DroppedFileHandle)
                .collect(),
            ..Default::default()
        };
        raw.viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point = Some(shown.scale);
        let mut close = false;
        let t0 = Instant::now();
        let mut out = self.ctx.run_ui(raw, |ui| close = content.ui(ui));
        let textures_changed = !out.textures_delta.is_empty();
        self.raster.update(std::mem::take(&mut out.textures_delta));
        // One shape at a time: egui would merge everything sharing a clip rect and texture
        // into one mesh, so a moving meter would mark the whole panel as changed. The CPU
        // rasterizer gains nothing from the merging anyway.
        let ppp = out.pixels_per_point;
        let prims: Vec<_> = std::mem::take(&mut out.shapes)
            .into_iter()
            .flat_map(|shape| self.ctx.tessellate(vec![shape], ppp))
            .collect();
        let (sw, sh) = (surface.w as usize, surface.h as usize);
        // A new window shows everything; after that, only what changed.
        let damage = if shown.first.is_some() {
            self.damage.track(&prims, out.pixels_per_point, sw, sh, true);
            vec![[0, 0, sw as i32, sh as i32]]
        } else {
            self.damage
                .track(&prims, out.pixels_per_point, sw, sh, textures_changed)
        };
        let delay = out
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map_or(Duration::MAX, |v| v.repaint_delay);
        let t1 = Instant::now();
        let Some(&first) = damage.first() else {
            self.stats.add(t1 - t0, Duration::ZERO, &damage, sw, sh);
            return (close, delay);
        };
        let px = surface.pixels();
        for d in &damage {
            for y in d[1]..d[3] {
                let row = y as usize * sw;
                px[row + d[0] as usize..row + d[2] as usize].fill(0);
            }
            self.raster.draw(&prims, out.pixels_per_point, px, sw, sh, *d);
        }
        self.stats.add(t1 - t0, t1.elapsed(), &damage, sw, sh);
        // The compositor takes one dirty rect: all of them.
        let d = damage.iter().fold(first, |u, r| {
            [u[0].min(r[0]), u[1].min(r[1]), u[2].max(r[2]), u[3].max(r[3])]
        });
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let size = SIZE {
            cx: surface.w,
            cy: surface.h,
        };
        let dirty = RECT {
            left: d[0],
            top: d[1],
            right: d[2],
            bottom: d[3],
        };
        let src = POINT::default();
        let info = UPDATELAYEREDWINDOWINFO {
            cbSize: size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
            hdcDst: HDC::default(),
            pptDst: &shown.pos,
            psize: &size,
            hdcSrc: surface.dc,
            pptSrc: &src,
            crKey: COLORREF(0),
            pblend: &blend,
            dwFlags: ULW_ALPHA,
            prcDirty: &dirty,
        };
        if !unsafe { UpdateLayeredWindowIndirect(self.hwnd, &info) }.as_bool() {
            log::line(format!(
                "UpdateLayeredWindowIndirect: {}",
                windows::core::Error::from_thread()
            ));
        }
        if let Some(at) = shown.first.take() {
            // The first frame is in place: now appear, on top, with the foreground.
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOWNORMAL);
                let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
                let fg = SetForegroundWindow(self.hwnd).as_bool();
                let _ = DwmFlush();
                log::line(format!(
                    "shown in {:?}{}",
                    at.elapsed(),
                    if fg { "" } else { " (Windows refused it the foreground)" }
                ));
            }
        }
        (close, delay)
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

/// Centered on `front`'s monitor, at that monitor's DPI: (position, size in pixels, scale).
fn placement(front: HWND, logical: (f32, f32)) -> (POINT, SIZE, f32) {
    unsafe {
        let monitor = MonitorFromWindow(front, MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(monitor, &mut info);
        let (mut dx, mut dy) = (96, 96);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let scale = dx as f32 / 96.0;
        let (w, h) = ((logical.0 * scale).round() as i32, (logical.1 * scale).round() as i32);
        let m: RECT = info.rcMonitor;
        let pos = POINT {
            x: m.left + ((m.right - m.left) - w) / 2,
            y: m.top + ((m.bottom - m.top) - h) / 2,
        };
        (pos, SIZE { cx: w, cy: h }, scale)
    }
}
