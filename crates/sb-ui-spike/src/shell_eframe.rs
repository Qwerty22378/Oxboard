//! Shell A: eframe (winit + wgpu or glow), driven through viewport commands where eframe has
//! them, Win32 where it doesn't (tool window style, placement in physical pixels).

use crate::overlay::{self, Exclusive, Front, HotkeySource, log};
use crate::panel::{Panel, SIZE};
use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetWindowLongPtrW, HWND_TOPMOST, SWP_NOACTIVATE, SetWindowLongPtrW, SetWindowPos, WS_EX_APPWINDOW,
    WS_EX_TOOLWINDOW,
};

struct App {
    hwnd: HWND,
    panel: Panel,
    toggles: Arc<Mutex<Vec<Instant>>>,
    visible: Arc<AtomicBool>,
    exclusive: Exclusive,
    /// While shown: who had the foreground, and whether we minimized it.
    shown: Option<(Front, bool)>,
    /// When the last toggle that showed it came in, until its first frame.
    showing_since: Option<Instant>,
    probe_at: Option<Instant>,
    was_focused: bool,
    label: String,
}

pub fn run(renderer: eframe::Renderer, hotkeys: HotkeySource, exclusive: Exclusive) {
    let label = format!("eframe + {renderer:?}");
    let visible = Arc::new(AtomicBool::new(false));
    overlay::spawn_stats(visible.clone(), Duration::from_secs(5));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("sb overlay spike")
            .with_inner_size(SIZE)
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_taskbar(false)
            .with_resizable(false)
            .with_visible(false),
        renderer,
        ..Default::default()
    };
    let r = eframe::run_native(
        "sb-ui-spike",
        options,
        Box::new(move |cc| {
            let hwnd = match cc.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(h)) => HWND(h.hwnd.get() as *mut _),
                other => panic!("not a Win32 window: {other:?}"),
            };
            unsafe {
                // Not in the taskbar or Alt+Tab.
                let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                SetWindowLongPtrW(
                    hwnd,
                    GWL_EXSTYLE,
                    (ex | WS_EX_TOOLWINDOW.0 as isize) & !(WS_EX_APPWINDOW.0 as isize),
                );
            }
            if let Some(info) = cc.wgpu_render_state.as_ref().map(|s| s.adapter.get_info()) {
                log(format!(
                    "wgpu adapter: {} ({:?}, {:?})",
                    info.name, info.device_type, info.backend
                ));
            }
            let toggles = Arc::new(Mutex::new(Vec::new()));
            let (t, ctx) = (toggles.clone(), cc.egui_ctx.clone());
            overlay::spawn_toggles(hotkeys, move |at| {
                t.lock().unwrap().push(at);
                ctx.request_repaint();
            });
            log(format!("{label}: ready, hidden; Ctrl+Shift+S shows it"));
            Ok(Box::new(App {
                hwnd,
                panel: Panel::default(),
                toggles,
                visible,
                exclusive,
                shown: None,
                showing_since: None,
                probe_at: None,
                was_focused: false,
                label,
            }))
        }),
    );
    if let Err(e) = r {
        log(format!("eframe failed: {e}"));
    }
}

impl App {
    fn show(&mut self, ctx: &egui::Context, at: Instant) {
        let front = overlay::front();
        log(format!(
            "show ({}): in front: {} (pid {}), shell says {}, covers its monitor: {}, {} dpi",
            self.label, front.title, front.pid, front.quns, front.covers_monitor, front.dpi
        ));
        let minimize = self.exclusive == Exclusive::Minimize && front.quns.starts_with("D3D");
        if minimize {
            overlay::minimize(&front);
        }
        let (x, y, w, h, _) = overlay::placement(&front, SIZE);
        unsafe {
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.panel.close = false;
        self.shown = Some((front, minimize));
        self.showing_since = Some(at);
        self.probe_at = Some(at + Duration::from_millis(300));
        self.was_focused = false;
        self.visible.store(true, Ordering::Relaxed);
    }

    fn hide(&mut self, ctx: &egui::Context, why: &str) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        self.visible.store(false, Ordering::Relaxed);
        log(format!("hide ({why})"));
        if let Some((front, minimized)) = self.shown.take() {
            overlay::give_back(&front, minimized);
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let toggles: Vec<Instant> = std::mem::take(&mut *self.toggles.lock().unwrap());
        for at in toggles {
            let wait = at.elapsed();
            if self.shown.is_some() {
                self.hide(ctx, "hotkey");
            } else {
                log(format!("hotkey reached the UI thread after {wait:?}"));
                self.show(ctx, at);
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.shown.is_none() {
            return;
        }
        if let Some(at) = self.showing_since.take() {
            log(format!("first frame drawn {:?} after the hotkey", at.elapsed()));
        }
        if self.probe_at.is_some_and(|t| Instant::now() >= t) {
            self.probe_at = None;
            overlay::probe(self.hwnd, "300 ms after show");
            if !ctx.input(|i| i.viewport().focused.unwrap_or(false)) {
                let ok = overlay::take_foreground(self.hwnd);
                log(format!("not focused by eframe's Focus; SetForegroundWindow → {ok}"));
            }
        }
        let since = self
            .shown
            .as_ref()
            .map(|(f, _)| format!("over: {}", f.title))
            .unwrap_or_default();
        self.panel.status = vec![self.label.clone(), since];
        self.panel.ui(ui);
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(false));
        if self.panel.close {
            self.hide(&ctx, "Esc");
        } else if self.was_focused && !focused {
            self.hide(&ctx, "focus lost: a click outside");
        }
        self.was_focused = focused;
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }
}
