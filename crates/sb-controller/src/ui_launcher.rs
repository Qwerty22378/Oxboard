//! The overlay is the controller's to run (`UI Design` → Process model): started with it,
//! started again if it dies (after 1 s, doubling to 30 s; back to 1 s once it has run a
//! minute), and stopped with it.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FIRST_WAIT: Duration = Duration::from_secs(1);
const MAX_WAIT: Duration = Duration::from_secs(30);
/// Running this long counts as having worked: the next crash waits [`FIRST_WAIT`] again.
const HEALTHY: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(250);

pub struct UiLauncher {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    thread: std::thread::JoinHandle<()>,
}

impl UiLauncher {
    /// Keep `exe ARGS` running.
    pub fn spawn(exe: PathBuf, args: Vec<String>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let (s, c) = (stop.clone(), child.clone());
        let thread = std::thread::Builder::new()
            .name("sb-ui-launcher".into())
            .spawn(move || run(&exe, &args, &s, &c))
            .expect("spawn the UI launcher");
        Self { stop, child, thread }
    }

    /// Stop the UI and stop restarting it.
    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = self.thread.join();
    }
}

fn run(exe: &PathBuf, args: &[String], stop: &AtomicBool, slot: &Mutex<Option<Child>>) {
    let mut wait = FIRST_WAIT;
    while !stop.load(Ordering::SeqCst) {
        let started = Instant::now();
        match Command::new(exe).args(args).spawn() {
            Ok(c) => {
                eprintln!("ui launcher: started {} (pid {})", exe.display(), c.id());
                *slot.lock().unwrap() = Some(c);
            }
            Err(e) => eprintln!("ui launcher: can't start {}: {e}", exe.display()),
        }
        // Until it ends (or we're told to stop).
        loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let mut guard = slot.lock().unwrap();
            let Some(c) = guard.as_mut() else { break };
            match c.try_wait() {
                Ok(None) => {}
                Ok(Some(status)) => {
                    eprintln!("ui launcher: the overlay exited ({status})");
                    *guard = None;
                    break;
                }
                Err(_) => {
                    *guard = None;
                    break;
                }
            }
            drop(guard);
            std::thread::sleep(POLL);
        }
        if started.elapsed() >= HEALTHY {
            wait = FIRST_WAIT;
        }
        let until = Instant::now() + wait;
        while Instant::now() < until {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(POLL);
        }
        wait = (wait * 2).min(MAX_WAIT);
    }
}
