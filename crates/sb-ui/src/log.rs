//! A line log for a program without a console: `--log FILE`, wall-clock stamped so it lines
//! up with the controller's.

use std::fs::File;
use std::io::Write;
use std::sync::Mutex;

static FILE: Mutex<Option<File>> = Mutex::new(None);

pub fn open(path: &str) -> std::io::Result<()> {
    *FILE.lock().unwrap() = Some(File::create(path)?);
    Ok(())
}

/// `mm:ss.mmm` within the hour, UTC.
pub fn clock() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let ms = t.as_millis() % 3_600_000;
    format!("{:02}:{:02}.{:03}", ms / 60_000, ms / 1000 % 60, ms % 1000)
}

pub fn line(text: impl AsRef<str>) {
    let line = format!("[{}] {}", clock(), text.as_ref());
    eprintln!("{line}");
    if let Some(f) = FILE.lock().unwrap().as_mut() {
        let _ = writeln!(f, "{line}");
        let _ = f.flush();
    }
}
