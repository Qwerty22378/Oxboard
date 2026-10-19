//! U1, the overlay spike (throwaway). Ctrl+Shift+S toggles a transparent, topmost panel with
//! one fader and one meter over whatever is in front, and the log records what the test
//! matrix in `UI Design` asks: did it show on top, is the cursor free, does focus come back,
//! how long from hotkey to first frame, what it costs hidden and shown.
//!
//! ```text
//! sb-ui-spike [--shell eframe|win32] [--renderer wgpu|glow] [--hotkey own|remote]
//!             [--exclusive show|minimize] [--log FILE]
//! sb-ui-spike --hotkey-server [--no-allow] [--log FILE]
//! ```
//!
//! - `--shell eframe` (default): eframe/winit with `--renderer` (wgpu default).
//!   `--shell win32`: our own layered window, drawn on the CPU (`raster.rs`).
//! - `--hotkey remote`: another process catches the hotkey (`--hotkey-server`, standing in for
//!   the controller) and signals this one, which is how the real UI gets it. `--no-allow`
//!   leaves out its `AllowSetForegroundWindow`, to see what that call is worth.
//! - `--exclusive minimize`: over a game in D3D exclusive fullscreen, minimize it first and
//!   restore it on hide ("alt-tab style"); `show` (default) just tries.
//!
//! The fake game to test against is `examples/fake_game.rs`.

// No console window: started from a terminal it still logs with --log.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod overlay;
#[cfg(windows)]
mod panel;
#[cfg(windows)]
mod raster;
#[cfg(windows)]
mod shell_eframe;
#[cfg(windows)]
mod shell_win32;

#[cfg(windows)]
fn main() {
    use overlay::{Exclusive, HotkeySource};
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
    };
    overlay::open_log(value("--log"));
    if args.iter().any(|a| a == "--hotkey-server") {
        overlay::hotkey_server(!args.iter().any(|a| a == "--no-allow"));
    }
    let hotkeys = match value("--hotkey") {
        Some("remote") => HotkeySource::Remote,
        _ => HotkeySource::Own,
    };
    let exclusive = match value("--exclusive") {
        Some("minimize") => Exclusive::Minimize,
        _ => Exclusive::Show,
    };
    overlay::log(format!("args: {args:?}"));
    match value("--shell") {
        Some("win32") => shell_win32::run(hotkeys, exclusive),
        _ => {
            let renderer = match value("--renderer") {
                Some("glow") => eframe::Renderer::Glow,
                _ => eframe::Renderer::Wgpu,
            };
            shell_eframe::run(renderer, hotkeys, exclusive)
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("sb-ui-spike is a Windows overlay spike; it does nothing here");
}
