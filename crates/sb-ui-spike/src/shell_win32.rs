//! Shell B: our own Win32 window and the CPU renderer. A layered popup (per-pixel alpha
//! through `UpdateLayeredWindow`, so no GPU and no swap chain), egui fed by hand.
//!
//! While hidden the thread blocks in `GetMessageW`: no frames, no timers. While visible it
//! draws ~30 frames a second (the meter), and right away on input.

use crate::overlay::{self, Exclusive, Front, HotkeySource, log};
use crate::panel::{Panel, SIZE};
use crate::raster::Raster;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, SIZE as WSIZE, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CreateCompatibleDC,
    CreateDIBSection, DIB_RGB_COLORS, DeleteObject, HBITMAP, HDC, HGDIOBJ, SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

const WM_TOGGLE: u32 = WM_APP + 1;
const FRAME: Duration = Duration::from_millis(33);

/// Toggle times, from the hotkey thread to the window thread (with a `WM_TOGGLE` each).
static TOGGLES: Mutex<Vec<Instant>> = Mutex::new(Vec::new());

thread_local! {
    /// What the window procedure collected for egui since the last frame.
    static INPUT: RefCell<Input> = RefCell::new(Input::default());
}

#[derive(Default)]
struct Input {
    events: Vec<egui::Event>,
    scale: f32,
    focused: bool,
    /// Focus went away while shown (a click outside, Alt+Tab).
    lost_focus: bool,
    dirty: bool,
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
    let button = |pressed| egui::Event::PointerButton {
        pos: point(lp),
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    match msg {
        WM_MOUSEMOVE => push(egui::Event::PointerMoved(point(lp))),
        WM_LBUTTONDOWN => {
            unsafe { SetCapture(hwnd) };
            push(button(true));
        }
        WM_LBUTTONUP => {
            let _ = unsafe { ReleaseCapture() };
            push(button(false));
        }
        WM_MOUSEWHEEL => {
            let d = (wp.0 >> 16) as i16 as f32 / 120.0;
            push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: egui::vec2(0.0, d),
                phase: egui::TouchPhase::Move,
                modifiers: Default::default(),
            });
        }
        WM_KEYDOWN if wp.0 == VK_ESCAPE.0 as usize => push(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        }),
        WM_ACTIVATE => {
            let active = (wp.0 & 0xffff) as u32 != WA_INACTIVE;
            INPUT.with_borrow_mut(|i| {
                if i.focused && !active {
                    i.lost_focus = true;
                }
                i.focused = active;
                i.dirty = true;
            });
            push(egui::Event::WindowFocused(active));
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
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
            let _ = windows::Win32::Graphics::Gdi::DeleteDC(self.dc);
        }
    }
}

struct Shown {
    front: Front,
    minimized: bool,
    pos: POINT,
    /// The hotkey's time, until the first frame is on screen.
    since: Option<Instant>,
    probe_at: Option<Instant>,
}

pub fn run(hotkeys: HotkeySource, exclusive: Exclusive) {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = GetModuleHandleW(None).expect("module handle");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("sb-ui-spike"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("sb-ui-spike"),
            w!("sb overlay spike"),
            WS_POPUP,
            0,
            0,
            SIZE.0 as i32,
            SIZE.1 as i32,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .expect("the overlay window");
        let raw = hwnd.0 as usize;
        overlay::spawn_toggles(hotkeys, move |at| {
            TOGGLES.lock().unwrap().push(at);
            let _ = PostMessageW(Some(HWND(raw as *mut _)), WM_TOGGLE, WPARAM(0), LPARAM(0));
        });
        let visible = Arc::new(AtomicBool::new(false));
        overlay::spawn_stats(visible.clone(), Duration::from_secs(5));
        log("win32 + CPU: ready, hidden; Ctrl+Shift+S shows it");

        let ctx = egui::Context::default();
        let mut panel = Panel::default();
        let mut raster = Raster::default();
        let mut surface: Option<Surface> = None;
        let mut shown: Option<Shown> = None;
        let mut next_frame = Instant::now();
        let mut frame_times = Vec::new();
        let start = Instant::now();
        let mut msg = MSG::default();
        loop {
            // Hidden: block. Visible: wait until the next frame is due, or input.
            if shown.is_none() {
                if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    return;
                }
                handle(&msg);
            } else {
                let wait = next_frame.saturating_duration_since(Instant::now());
                MsgWaitForMultipleObjects(None, false, wait.as_millis() as u32, QS_ALLINPUT);
            }
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return;
                }
                handle(&msg);
            }

            for at in std::mem::take(&mut *TOGGLES.lock().unwrap()) {
                match shown.take() {
                    Some(s) => {
                        hide(hwnd, s, "hotkey", &visible);
                    }
                    None => {
                        log(format!("hotkey reached the UI thread after {:?}", at.elapsed()));
                        let front = overlay::front();
                        log(format!(
                            "show (win32 + CPU): in front: {} (pid {}), shell says {}, covers its monitor: {}, {} dpi",
                            front.title, front.pid, front.quns, front.covers_monitor, front.dpi
                        ));
                        let minimized = exclusive == Exclusive::Minimize && front.quns.starts_with("D3D");
                        if minimized {
                            overlay::minimize(&front);
                        }
                        let (x, y, w, h, scale) = overlay::placement(&front, SIZE);
                        if surface.as_ref().is_none_or(|s| (s.w, s.h) != (w, h)) {
                            surface = Some(Surface::new(w, h));
                        }
                        INPUT.with_borrow_mut(|i| {
                            *i = Input {
                                scale,
                                dirty: true,
                                ..Input::default()
                            }
                        });
                        panel.close = false;
                        shown = Some(Shown {
                            front,
                            minimized,
                            pos: POINT { x, y },
                            since: Some(at),
                            probe_at: Some(at + Duration::from_millis(300)),
                        });
                        next_frame = Instant::now();
                    }
                }
            }

            let Some(s) = shown.as_mut() else { continue };
            let lost = INPUT.with_borrow_mut(|i| std::mem::take(&mut i.lost_focus));
            if lost {
                hide(hwnd, shown.take().unwrap(), "focus lost: a click outside", &visible);
                continue;
            }
            let dirty = INPUT.with_borrow(|i| i.dirty);
            if Instant::now() < next_frame && !dirty {
                continue;
            }
            next_frame = Instant::now() + FRAME;

            // One egui pass, rasterized on the CPU, handed to the compositor.
            let t0 = Instant::now();
            let surf = surface.as_mut().unwrap();
            let (events, scale, focused) = INPUT.with_borrow_mut(|i| {
                i.dirty = false;
                (std::mem::take(&mut i.events), i.scale, i.focused)
            });
            let mut raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(SIZE.0, SIZE.1))),
                time: Some(start.elapsed().as_secs_f64()),
                events,
                focused,
                ..Default::default()
            };
            raw.viewports
                .entry(egui::ViewportId::ROOT)
                .or_default()
                .native_pixels_per_point = Some(scale);
            panel.status = vec![
                "win32 + CPU (layered window)".into(),
                format!("over: {}", s.front.title),
            ];
            let out = ctx.run_ui(raw, |ui| panel.ui(ui));
            raster.update(&out.textures_delta);
            let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
            let (w, h) = (surf.w as usize, surf.h as usize);
            let px = surf.pixels();
            px.fill(0);
            raster.draw(&prims, out.pixels_per_point, px, w, h);
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let size = WSIZE { cx: surf.w, cy: surf.h };
            let r = UpdateLayeredWindow(
                hwnd,
                None,
                Some(&s.pos),
                Some(&size),
                Some(surf.dc),
                Some(&POINT::default()),
                windows::Win32::Foundation::COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            if let Err(e) = r {
                log(format!("UpdateLayeredWindow: {e}"));
            }
            frame_times.push(t0.elapsed());
            if let Some(at) = s.since.take() {
                // The content is in place before the window appears: no empty first frame.
                let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
                let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
                let fg = overlay::take_foreground(hwnd);
                let _ = DwmFlush();
                visible.store(true, Ordering::Relaxed);
                log(format!(
                    "first frame on screen {:?} after the hotkey (SetForegroundWindow → {fg})",
                    at.elapsed()
                ));
            }
            if s.probe_at.is_some_and(|t| Instant::now() >= t) {
                s.probe_at = None;
                overlay::probe(hwnd, "300 ms after show");
            }
            if frame_times.len() == 90 {
                let mut t = std::mem::take(&mut frame_times);
                t.sort();
                log(format!(
                    "CPU frame time over 90 frames: median {:?}, worst {:?} ({}×{} px)",
                    t[45], t[89], surf.w, surf.h
                ));
            }
            if panel.close {
                hide(hwnd, shown.take().unwrap(), "Esc", &visible);
            }
        }
    }
}

fn handle(msg: &MSG) {
    unsafe {
        let _ = TranslateMessage(msg);
        DispatchMessageW(msg);
    }
}

fn hide(hwnd: HWND, s: Shown, why: &str, visible: &AtomicBool) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    visible.store(false, Ordering::Relaxed);
    log(format!("hide ({why})"));
    overlay::give_back(&s.front, s.minimized);
}
