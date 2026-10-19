//! The controller's presence on the desktop, on a thread of its own: a hidden window that
//! - owns the **overlay hotkey** (Ctrl+Shift+S, `RegisterHotKey`): the actor lets the overlay
//!   take the foreground and tells it to show (the UI didn't get the input, so it couldn't);
//! - puts the **tray icon** up: click for the overlay; right-click for Open, Restore Windows
//!   audio, Quit (put back if Explorer restarts);
//! - hears Windows **logging off or shutting down** (`WM_ENDSESSION`) and holds that up until
//!   the controller has stopped cleanly: defaults given back, settings written.
//!
//! The window is a hidden top-level one: message-only windows don't get those broadcasts.

use crate::Stopped;
use crate::actor::{Inbox, Msg};
use sb_shared::ui::Command;
use std::cell::RefCell;
use std::time::Duration;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

const WM_TRAY: u32 = WM_APP + 1;
const HOTKEY_OVERLAY: i32 = 1;
/// Ctrl+Shift+S (rebinding comes with the settings page).
const HOTKEY_VK: u32 = b'S' as u32;

const MENU_OPEN: usize = 1;
const MENU_RESTORE_AUDIO: usize = 2;
const MENU_QUIT: usize = 3;

/// How long a logoff waits for the controller to finish.
const ENDSESSION_WAIT: Duration = Duration::from_secs(5);

struct Ctx {
    inbox: Inbox,
    stopped: Stopped,
    taskbar_created: u32,
    tray: bool,
}

thread_local! {
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&Ctx) -> R) -> Option<R> {
    CTX.with_borrow(|c| c.as_ref().map(f))
}

fn send(msg: Msg) {
    with(|c| {
        let _ = c.inbox.send(msg);
    });
}

/// The running shell: stop it with [`Shell::stop`].
pub struct Shell {
    hwnd: usize,
    thread: std::thread::JoinHandle<()>,
}

impl Shell {
    pub fn stop(self) {
        let _ = unsafe { PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        let _ = self.thread.join();
    }
}

/// Start the window thread. `hotkey`: register the overlay hotkey; `tray`: show the icon.
pub fn spawn(inbox: Inbox, stopped: Stopped, hotkey: bool, tray: bool) -> Shell {
    let (tx, rx) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("sb-shell".into())
        .spawn(move || run(inbox, stopped, hotkey, tray, tx))
        .expect("spawn the shell thread");
    let hwnd = rx.recv().unwrap_or(0);
    Shell { hwnd, thread }
}

fn run(inbox: Inbox, stopped: Stopped, hotkey: bool, tray: bool, ready: std::sync::mpsc::Sender<usize>) {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("sb-controller"),
            ..Default::default()
        };
        RegisterClassW(&class);
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        CTX.with_borrow_mut(|c| {
            *c = Some(Ctx {
                inbox,
                stopped,
                taskbar_created,
                tray,
            })
        });
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("sb-controller"),
            w!("Soundboard"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        ) else {
            eprintln!("shell: no window; no hotkey, tray or logoff handling");
            let _ = ready.send(0);
            return;
        };
        let _ = ready.send(hwnd.0 as usize);
        if hotkey {
            match RegisterHotKey(
                Some(hwnd),
                HOTKEY_OVERLAY,
                MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT,
                HOTKEY_VK,
            ) {
                Ok(()) => eprintln!("shell: overlay hotkey Ctrl+Shift+S"),
                Err(e) => eprintln!("shell: can't register Ctrl+Shift+S ({e}); it's taken"),
            }
        }
        if tray {
            add_icon(hwnd);
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default(),
        ..Default::default()
    };
    let tip: Vec<u16> = "Soundboard".encode_utf16().collect();
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    nid
}

fn add_icon(hwnd: HWND) {
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd)) }.as_bool() {
        eprintln!("shell: no tray icon (no taskbar yet?)");
    }
}

fn menu(hwnd: HWND) {
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(m, MF_STRING, MENU_OPEN, w!("Open soundboard\tCtrl+Shift+S"));
        let _ = AppendMenuW(m, MF_STRING, MENU_RESTORE_AUDIO, w!("Restore Windows audio"));
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(m, MF_STRING, MENU_QUIT, w!("Quit"));
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        // The menu only closes on a click outside if its owner is in front.
        let _ = SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, p.x, p.y, Some(0), hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(m);
        match chosen.0 as usize {
            MENU_OPEN => send(Msg::Hotkey),
            MENU_RESTORE_AUDIO => send(Msg::Local(Command::SetKeepWindowsDefault(true))),
            MENU_QUIT => send(Msg::Stop),
            _ => {}
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY if wp.0 as i32 == HOTKEY_OVERLAY => send(Msg::Hotkey),
        WM_TRAY => match (lp.0 & 0xffff) as u32 {
            WM_LBUTTONUP => send(Msg::Hotkey),
            WM_RBUTTONUP | WM_CONTEXTMENU => menu(hwnd),
            _ => {}
        },
        // Logoff or shutdown: stop, and don't return until the controller has (Windows ends
        // the process once every window has answered).
        WM_QUERYENDSESSION => return LRESULT(1),
        WM_ENDSESSION if wp.0 != 0 => {
            eprintln!("shell: Windows is ending the session; stopping");
            send(Msg::Stop);
            if with(|c| c.stopped.wait(ENDSESSION_WAIT)) == Some(false) {
                eprintln!("shell: still not stopped; Windows ends us anyway");
            }
        }
        WM_CLOSE => {
            if with(|c| c.tray) == Some(true) {
                let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd)) };
            }
            let _ = unsafe { DestroyWindow(hwnd) };
        }
        WM_DESTROY => unsafe { PostQuitMessage(0) },
        m if Some(m) == with(|c| c.taskbar_created) && with(|c| c.tray) == Some(true) => add_icon(hwnd),
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
}
