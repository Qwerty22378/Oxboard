//! `sb-ui [--address ADDR] [--show] [--hotkey] [--gallery] [--log FILE]`
//!
//! The overlay: starts hidden and connects to the controller at `ADDR` (default: the per-user
//! pipe, as `sb-controller`), reconnecting whenever it's gone. The controller starts it, owns
//! the hotkey (Ctrl+Shift+S) and tells it to show or hide; Esc and a click outside hide it too.
//!
//! `--show` shows it at startup (development). `--hotkey` registers Ctrl+Shift+S here instead,
//! to run it without a controller's shell (`sb-controller --no-shell`).
//! `--gallery` shows every widget against made-up state instead, with no controller.
//! There's no console: `--log FILE` says what it's doing.

// No console window.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    use sb_ui::{app::App, log, shell};
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let flag = |f: &str| args.iter().any(|a| a == f);
    if let Some(path) = value("--log")
        && let Err(e) = log::open(&path)
    {
        eprintln!("can't log to {path}: {e}");
    }
    let address = value("--address").unwrap_or_else(sb_controller::transport::default_address);
    log::line(format!("sb-ui {}: controller at {address}", env!("CARGO_PKG_VERSION")));
    let mut shell = shell::Shell::new();
    let mut gallery;
    let mut app;
    let content: &mut dyn shell::Content = if flag("--gallery") {
        gallery = sb_ui::views::gallery::Gallery::default();
        &mut gallery
    } else {
        app = App::new(address, shell.waker());
        &mut app
    };
    shell.run(
        content,
        shell::Options {
            hotkey: flag("--hotkey"),
            show_at_start: flag("--show"),
        },
    );
}

#[cfg(not(windows))]
fn main() {
    eprintln!("sb-ui is the Windows overlay; on this platform only its store and link build (for tests)");
}
