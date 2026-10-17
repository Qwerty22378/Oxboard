//! Where UIs find the controller: a named pipe on Windows, a Unix socket elsewhere (tests and
//! development on Linux).
//!
//! The Windows pipe is owner-only and local-only (`sb_pipe`), and its name is per user. The
//! controller creates the first instance, so a second controller fails to start instead of
//! sharing the name.

use crate::ui_server::{Connection, Listener};
use std::io;
use std::time::Duration;

/// UIs connected at once, at most (Windows; one pipe instance each).
pub const MAX_UIS: u32 = 8;

#[cfg(windows)]
pub fn default_address() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    format!(r"\\.\pipe\sb-controller-{user}")
}

#[cfg(unix)]
pub fn default_address() -> String {
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(dir) if !dir.is_empty() => format!("{dir}/sb-controller.sock"),
        _ => format!(
            "/tmp/sb-controller-{}.sock",
            std::env::var("USER").unwrap_or_else(|_| "user".into())
        ),
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use sb_pipe::{Pipe, PipeListener};
    use std::sync::Arc;

    impl Listener for PipeListener {
        fn accept(&mut self) -> io::Result<Connection> {
            let pipe = PipeListener::accept(self)?;
            let closer = pipe.closer();
            let (reader, writer) = pipe.split()?;
            Ok(Connection {
                reader: Box::new(reader),
                writer: Box::new(writer),
                close: Arc::new(move || closer.close()),
            })
        }
    }

    pub fn bind(address: &str) -> io::Result<PipeListener> {
        PipeListener::bind(address, MAX_UIS).map_err(|e| {
            // FILE_FLAG_FIRST_PIPE_INSTANCE on a name that exists: access denied.
            if e.kind() == io::ErrorKind::PermissionDenied {
                io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("{address}: a controller is already running ({e})"),
                )
            } else {
                e
            }
        })
    }

    pub fn connect(address: &str, timeout: Duration) -> io::Result<Connection> {
        let pipe = Pipe::connect(address, timeout)?;
        let closer = pipe.closer();
        let (reader, writer) = pipe.split()?;
        Ok(Connection {
            reader: Box::new(reader),
            writer: Box::new(writer),
            close: Arc::new(move || closer.close()),
        })
    }
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::Arc;
    use std::time::Instant;

    pub struct SocketListener(UnixListener);

    fn connection(stream: UnixStream) -> io::Result<Connection> {
        let reader = stream.try_clone()?;
        let closer = stream.try_clone()?;
        Ok(Connection {
            reader: Box::new(reader),
            writer: Box::new(stream),
            close: Arc::new(move || {
                let _ = closer.shutdown(std::net::Shutdown::Both);
            }),
        })
    }

    impl Listener for SocketListener {
        fn accept(&mut self) -> io::Result<Connection> {
            connection(self.0.accept()?.0)
        }
    }

    pub fn bind(address: &str) -> io::Result<SocketListener> {
        // A socket file left by a controller that crashed is stale; one that answers isn't.
        if UnixStream::connect(address).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("{address}: a controller is already running"),
            ));
        }
        let _ = std::fs::remove_file(address);
        let listener = UnixListener::bind(address)?;
        // Owner only, like the Windows pipe's DACL.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(address, std::fs::Permissions::from_mode(0o600))?;
        Ok(SocketListener(listener))
    }

    pub fn connect(address: &str, timeout: Duration) -> io::Result<Connection> {
        let deadline = Instant::now() + timeout;
        loop {
            match UnixStream::connect(address) {
                Ok(s) => return connection(s),
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => return Err(io::Error::new(e.kind(), format!("{address}: {e}"))),
            }
        }
    }
}

/// Start serving UIs on `address`.
pub fn bind(address: &str) -> io::Result<impl Listener> {
    imp::bind(address)
}

/// Connect to the controller at `address`, retrying while it starts up.
pub fn connect(address: &str, timeout: Duration) -> io::Result<Connection> {
    imp::connect(address, timeout)
}
