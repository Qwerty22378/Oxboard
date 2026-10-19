//! `sb-ui [--address ADDR] [--show] [--no-hotkey] [--gallery] [--log FILE]`
//!
//! The overlay: starts hidden and connects to the controller at `ADDR` (default: the per-user
//! pipe, as `sb-controller`), reconnecting whenever it's gone. Ctrl+Shift+S shows it over
//! whatever is in front, and hides it again (so do Esc and a click outside).
//!
//! `--show` shows it at startup (development). `--no-hotkey` leaves the hotkey to someone else.
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
            hotkey: !flag("--no-hotkey"),
            show_at_start: flag("--show"),
        },
    );
}

#[cfg(not(windows))]
fn main() {
    eprintln!("sb-ui is the Windows overlay; on this platform only its store and link build (for tests)");
}
