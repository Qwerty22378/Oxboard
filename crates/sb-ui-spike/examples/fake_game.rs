//! A stand-in game for the overlay spike: a D3D11 window that renders flat out and logs its
//! frame rate every second, so the overlay's cost shows as an FPS drop.
//!
//! ```text
//! fake_game [--mode windowed|borderless|exclusive] [--capture] [--vsync] [--seconds N] [--log FILE]
//! ```
//!
//! - `windowed`: a 1280×720 window. `borderless`: a popup covering the primary monitor (flip
//!   model, so Windows treats it like most modern "fullscreen" games). `exclusive`: the old
//!   blt-model swap chain switched to DXGI fullscreen (`SetFullscreenState`).
//! - `--capture`: while focused, clip the cursor to the window and hide it, like a shooter.

// No console window: started from a terminal it still logs with --log.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    game::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("fake_game is Windows only");
}

#[cfg(windows)]
mod game {
    use std::fs::File;
    use std::io::Write;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::{HRESULT, Interface, w};

    /// `Present` couldn't show the frame: the window is covered or minimized.
    const DXGI_STATUS_OCCLUDED: HRESULT = HRESULT(0x087A_0001);

    static LOG: Mutex<Option<(Instant, Option<File>)>> = Mutex::new(None);
    static CAPTURE: Mutex<bool> = Mutex::new(false);
    static RESIZED: Mutex<bool> = Mutex::new(false);

    fn log(line: impl AsRef<str>) {
        let mut g = LOG.lock().unwrap();
        let Some((start, file)) = g.as_mut() else { return };
        let _ = start;
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let ms = t.as_millis() % 3_600_000;
        let line = format!(
            "[{:02}:{:02}.{:03}] game: {}",
            ms / 60_000,
            ms / 1000 % 60,
            ms % 1000,
            line.as_ref()
        );
        eprintln!("{line}");
        if let Some(f) = file {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }

    /// Clip and hide the cursor while focused, as games do; let it go otherwise.
    fn capture(hwnd: HWND, on: bool) {
        unsafe {
            if on {
                let mut r = RECT::default();
                let _ = GetClientRect(hwnd, &mut r);
                let mut tl = POINT { x: r.left, y: r.top };
                let mut br = POINT {
                    x: r.right,
                    y: r.bottom,
                };
                let _ = ClientToScreen(hwnd, &mut tl);
                let _ = ClientToScreen(hwnd, &mut br);
                let _ = ClipCursor(Some(&RECT {
                    left: tl.x,
                    top: tl.y,
                    right: br.x,
                    bottom: br.y,
                }));
                while ShowCursor(false) >= 0 {}
            } else {
                let _ = ClipCursor(None);
                while ShowCursor(true) < 0 {}
            }
        }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        match msg {
            WM_ACTIVATE => {
                let active = (wp.0 & 0xffff) as u32 != WA_INACTIVE;
                log(format!("WM_ACTIVATE {}", if active { "active" } else { "inactive" }));
                LRESULT(0)
            }
            WM_SIZE => {
                *RESIZED.lock().unwrap() = true;
                LRESULT(0)
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
        }
    }

    pub fn run() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let value = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
        let mode = value("--mode").unwrap_or_else(|| "windowed".into());
        let seconds: u64 = value("--seconds").and_then(|s| s.parse().ok()).unwrap_or(120);
        let vsync = args.iter().any(|a| a == "--vsync");
        *CAPTURE.lock().unwrap() = args.iter().any(|a| a == "--capture");
        let file = value("--log").map(|p| File::create(p).expect("the log"));
        *LOG.lock().unwrap() = Some((Instant::now(), file));
        log(format!("args: {args:?}"));

        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let instance = GetModuleHandleW(None).unwrap();
            let class = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: w!("fake-game"),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            RegisterClassW(&class);
            let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
            let (style, x, y, w, h) = match mode.as_str() {
                "windowed" => (WS_OVERLAPPEDWINDOW | WS_VISIBLE, 100, 100, 1280, 720),
                _ => (WS_POPUP | WS_VISIBLE, 0, 0, sw, sh),
            };
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("fake-game"),
                w!("Fake Game"),
                style,
                x,
                y,
                w,
                h,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            let exclusive = mode == "exclusive";
            let desc = DXGI_SWAP_CHAIN_DESC {
                BufferDesc: DXGI_MODE_DESC {
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    ..Default::default()
                },
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: if exclusive { 1 } else { 2 },
                OutputWindow: hwnd,
                Windowed: true.into(),
                SwapEffect: if exclusive {
                    DXGI_SWAP_EFFECT_DISCARD
                } else {
                    DXGI_SWAP_EFFECT_FLIP_DISCARD
                },
                Flags: DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH.0 as u32,
            };
            let (mut chain, mut device, mut context) = (None, None, None);
            D3D11CreateDeviceAndSwapChain(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&desc),
                Some(&mut chain),
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .expect("a D3D11 device");
            let (chain, device, context): (IDXGISwapChain, ID3D11Device, ID3D11DeviceContext) =
                (chain.unwrap(), device.unwrap(), context.unwrap());
            if let Ok(a) = device.cast::<IDXGIDevice>().and_then(|d| d.GetAdapter()) {
                if let Ok(d) = a.GetDesc() {
                    let name = String::from_utf16_lossy(&d.Description);
                    log(format!("adapter: {}", name.trim_end_matches('\0')));
                }
            }
            if exclusive {
                match chain.SetFullscreenState(true, None) {
                    Ok(()) => log("SetFullscreenState(true): ok"),
                    Err(e) => log(format!("SetFullscreenState(true) FAILED: {e}")),
                }
            }
            let _ = SetForegroundWindow(hwnd);
            let target = |chain: &IDXGISwapChain| -> ID3D11RenderTargetView {
                let tex: ID3D11Texture2D = chain.GetBuffer(0).unwrap();
                let mut rtv = None;
                device.CreateRenderTargetView(&tex, None, Some(&mut rtv)).unwrap();
                rtv.unwrap()
            };
            let mut rtv = Some(target(&chain));
            let start = Instant::now();
            let (mut frames, mut second) = (0u32, Instant::now());
            let mut occluded = 0u32;
            let mut msg = MSG::default();
            let mut in_front = false;
            while start.elapsed() < Duration::from_secs(seconds) {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        return;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                if std::mem::take(&mut *RESIZED.lock().unwrap()) {
                    drop(rtv.take()); // ResizeBuffers needs every reference to the buffers gone
                    context.ClearState();
                    let r = chain.ResizeBuffers(0, 0, 0, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH);
                    if let Err(e) = r {
                        log(format!("ResizeBuffers: {e}"));
                    }
                    rtv = Some(target(&chain));
                }
                // Like a shooter: while in front, the cursor is ours (clipped, hidden).
                let front = GetForegroundWindow() == hwnd;
                if front != in_front {
                    in_front = front;
                    log(format!("foreground {}", if front { "gained" } else { "lost" }));
                    if *CAPTURE.lock().unwrap() {
                        capture(hwnd, front);
                    }
                }
                let t = start.elapsed().as_secs_f32();
                let color = [(t * 0.7).sin() * 0.5 + 0.5, (t * 0.5).cos() * 0.3 + 0.3, 0.4, 1.0];
                context.ClearRenderTargetView(rtv.as_ref().unwrap(), &color);
                let hr = chain.Present(vsync as u32, DXGI_PRESENT(0));
                if hr == DXGI_STATUS_OCCLUDED {
                    occluded += 1;
                    std::thread::sleep(Duration::from_millis(5));
                }
                frames += 1;
                if second.elapsed() >= Duration::from_secs(1) {
                    let fs = if exclusive {
                        let mut on = windows::core::BOOL(0);
                        let _ = chain.GetFullscreenState(Some(&mut on), None);
                        if on.as_bool() {
                            ", fullscreen"
                        } else {
                            ", NOT fullscreen"
                        }
                    } else {
                        ""
                    };
                    let fg = GetForegroundWindow() == hwnd;
                    log(format!(
                        "fps {:.0}{fs}{}{}",
                        frames as f64 / second.elapsed().as_secs_f64(),
                        if fg { ", in front" } else { ", BEHIND" },
                        if occluded > 0 {
                            format!(", occluded x{occluded}")
                        } else {
                            String::new()
                        }
                    ));
                    (frames, occluded, second) = (0, 0, Instant::now());
                }
            }
            if exclusive {
                let _ = chain.SetFullscreenState(false, None);
            }
            log("done");
        }
    }
}
