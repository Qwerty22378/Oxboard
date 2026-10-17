//! Windows named pipes for the soundboard's links: controller ⇄ engine host, UI ⇄ controller.
//!
//! - **Local only, owner only.** Every instance has `PIPE_REJECT_REMOTE_CLIENTS` and an
//!   explicit DACL granting access to SYSTEM and the creating user only (the default DACL lets
//!   *Everyone* read). The first instance is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so
//!   nobody can squat the name first and impersonate the server; a second server on the same
//!   name fails instead.
//! - [`Pipe::serve`]: one instance, one client (the engine host; the controller also picks
//!   an unguessable name per launch). [`PipeListener`]: up to N clients at once, one instance
//!   each (the controller's UI pipe).
//! - **Overlapped I/O.** Synchronous I/O on one pipe handle is serialized by Windows, so a
//!   thread blocked in `ReadFile` would block every `WriteFile` too. Each half here does its
//!   own overlapped operation with its own event, so reading and writing never wait on each
//!   other.
//! - [`PipeCloser`] ends a connection from any thread: both halves see end of stream.

#![cfg(windows)]

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_OPERATION_ABORTED,
    ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL,
    LocalFree,
};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_SHARE_MODE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::CreateEventW;
use windows::core::{HRESULT, HSTRING, PCWSTR};

/// SYSTEM and the owner (the user who created the pipe) get full access; nobody else any.
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;OW)";
const BUFFER: u32 = 64 * 1024;

struct Handle {
    h: HANDLE,
    /// The server's end (so closing it means disconnecting the client).
    server: bool,
    closed: AtomicBool,
}

// A kernel handle; both halves use it concurrently with their own OVERLAPPED, which is the
// supported way to share a pipe between threads.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.h) };
    }
}

impl Handle {
    fn new(h: HANDLE, server: bool) -> Arc<Self> {
        Arc::new(Self {
            h,
            server,
            closed: AtomicBool::new(false),
        })
    }

    /// End the connection: pending reads and writes on both halves return, later ones fail.
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        unsafe {
            if self.server {
                let _ = DisconnectNamedPipe(self.h);
            }
            let _ = CancelIoEx(self.h, None);
        }
    }
}

fn is(e: &windows::core::Error, code: windows::Win32::Foundation::WIN32_ERROR) -> bool {
    e.code() == HRESULT::from_win32(code.0)
}

fn io_err(e: windows::core::Error) -> io::Error {
    io::Error::other(e.message())
}

/// One side of a connected pipe; [`Pipe::split`] gives a reader and a writer.
pub struct Pipe {
    handle: Arc<Handle>,
}

/// Create one instance of the pipe `name`. `first`: fail if the pipe already exists.
fn create(name: &str, first: bool, max_instances: u32) -> io::Result<HANDLE> {
    unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(&HSTRING::from(PIPE_SDDL), SDDL_REVISION_1, &mut sd, None)
            .map_err(io_err)?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if first {
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let h = CreateNamedPipeW(
            &HSTRING::from(name),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            max_instances,
            BUFFER,
            BUFFER,
            0,
            Some(&sa),
        );
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        if h.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        Ok(h)
    }
}

/// Wait for a client on a fresh instance.
fn wait_client(handle: Arc<Handle>) -> io::Result<Pipe> {
    let event = Event::new()?;
    let mut ov = OVERLAPPED {
        hEvent: event.0,
        ..Default::default()
    };
    unsafe {
        match ConnectNamedPipe(handle.h, Some(&mut ov)) {
            Ok(()) => {}
            Err(e) if is(&e, ERROR_PIPE_CONNECTED) => {}
            Err(e) if is(&e, ERROR_IO_PENDING) => {
                let mut n = 0;
                GetOverlappedResult(handle.h, &ov, &mut n, true).map_err(io_err)?;
            }
            Err(e) => return Err(io_err(e)),
        }
    }
    Ok(Pipe { handle })
}

impl Pipe {
    /// Create the pipe `name` (`\\.\pipe\...`) and wait for the one client.
    pub fn serve(name: &str) -> io::Result<Pipe> {
        wait_client(Handle::new(create(name, true, 1)?, true))
    }

    /// Connect to the pipe `name`, retrying while the host is still starting up.
    pub fn connect(name: &str, timeout: Duration) -> io::Result<Pipe> {
        let deadline = Instant::now() + timeout;
        loop {
            let opened = unsafe {
                CreateFileW(
                    &HSTRING::from(name),
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    None,
                )
            };
            match opened {
                Ok(h) => {
                    return Ok(Pipe {
                        handle: Handle::new(h, false),
                    });
                }
                Err(e) if (is(&e, ERROR_FILE_NOT_FOUND) || is(&e, ERROR_PIPE_BUSY)) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(io_err(e)),
            }
        }
    }

    /// A handle that ends this connection from anywhere (see [`PipeCloser`]).
    pub fn closer(&self) -> PipeCloser {
        PipeCloser(self.handle.clone())
    }

    pub fn split(self) -> io::Result<(PipeReader, PipeWriter)> {
        Ok((
            PipeReader {
                handle: self.handle.clone(),
                event: Event::new()?,
            },
            PipeWriter {
                handle: self.handle,
                event: Event::new()?,
            },
        ))
    }
}

/// Ends a connection: the reader sees end of stream, the writer a broken pipe. On the
/// server's end it also disconnects the client.
#[derive(Clone)]
pub struct PipeCloser(Arc<Handle>);

impl PipeCloser {
    pub fn close(&self) {
        self.0.close();
    }
}

/// The server's end of a pipe that takes up to `max_instances` clients at once.
pub struct PipeListener {
    name: String,
    max_instances: u32,
    /// The instance the next client will connect to. `None` after every instance was taken.
    next: Option<Arc<Handle>>,
}

impl PipeListener {
    /// Create the pipe `name`. Fails if something already serves it.
    pub fn bind(name: &str, max_instances: u32) -> io::Result<Self> {
        let first = create(name, true, max_instances)?;
        Ok(Self {
            name: name.into(),
            max_instances,
            next: Some(Handle::new(first, true)),
        })
    }

    /// Wait for the next client.
    pub fn accept(&mut self) -> io::Result<Pipe> {
        let handle = loop {
            if let Some(h) = self.next.take() {
                break h;
            }
            match create(&self.name, false, self.max_instances) {
                Ok(h) => break Handle::new(h, true),
                // Every instance is in use: wait for a client to leave.
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => {
                    std::thread::sleep(Duration::from_millis(100))
                }
                Err(e) => return Err(e),
            }
        };
        let pipe = wait_client(handle)?;
        // Open the next instance right away, so a client never finds the pipe missing.
        self.next = create(&self.name, false, self.max_instances)
            .ok()
            .map(|h| Handle::new(h, true));
        Ok(pipe)
    }
}

struct Event(HANDLE);

unsafe impl Send for Event {}

impl Event {
    fn new() -> io::Result<Self> {
        unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .map(Event)
            .map_err(io_err)
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The other side going away (or a [`PipeCloser`]) reads as end of stream.
fn closed(e: &windows::core::Error) -> bool {
    is(e, ERROR_BROKEN_PIPE)
        || is(e, ERROR_PIPE_NOT_CONNECTED)
        || is(e, ERROR_NO_DATA)
        || is(e, ERROR_OPERATION_ABORTED)
}

pub struct PipeReader {
    handle: Arc<Handle>,
    event: Event,
}

impl io::Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.handle.closed.load(Ordering::SeqCst) {
            return Ok(0);
        }
        let h = self.handle.h;
        let mut ov = OVERLAPPED {
            hEvent: self.event.0,
            ..Default::default()
        };
        unsafe {
            match ReadFile(h, Some(buf), None, Some(&mut ov)) {
                Ok(()) => {}
                Err(e) if is(&e, ERROR_IO_PENDING) => {}
                Err(e) if closed(&e) => return Ok(0),
                Err(e) => return Err(io_err(e)),
            }
            let mut n = 0;
            match GetOverlappedResult(h, &ov, &mut n, true) {
                Ok(()) => Ok(n as usize),
                Err(e) if closed(&e) => Ok(0),
                Err(e) => Err(io_err(e)),
            }
        }
    }
}

pub struct PipeWriter {
    handle: Arc<Handle>,
    event: Event,
}

impl io::Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.handle.closed.load(Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"));
        }
        let h = self.handle.h;
        let mut ov = OVERLAPPED {
            hEvent: self.event.0,
            ..Default::default()
        };
        unsafe {
            match WriteFile(h, Some(buf), None, Some(&mut ov)) {
                Ok(()) => {}
                Err(e) if is(&e, ERROR_IO_PENDING) => {}
                Err(e) if closed(&e) => return Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed")),
                Err(e) => return Err(io_err(e)),
            }
            let mut n = 0;
            match GetOverlappedResult(h, &ov, &mut n, true) {
                Ok(()) => Ok(n as usize),
                Err(e) if closed(&e) => Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed")),
                Err(e) => Err(io_err(e)),
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn name(tag: &str) -> String {
        format!(r"\\.\pipe\sb-pipe-test-{tag}-{}", std::process::id())
    }

    fn echo_once(pipe: Pipe) {
        let (mut r, mut w) = pipe.split().unwrap();
        let mut buf = [0u8; 5];
        r.read_exact(&mut buf).unwrap();
        w.write_all(&buf).unwrap();
    }

    #[test]
    fn a_listener_serves_several_clients_at_once() {
        let n = name("many");
        let mut listener = PipeListener::bind(&n, 4).unwrap();
        let server = std::thread::spawn(move || {
            let pipes: Vec<Pipe> = (0..3).map(|_| listener.accept().unwrap()).collect();
            for p in pipes {
                echo_once(p);
            }
        });
        let clients: Vec<_> = (0..3)
            .map(|_| Pipe::connect(&n, Duration::from_secs(5)).unwrap().split().unwrap())
            .collect();
        for (i, (mut r, mut w)) in clients.into_iter().enumerate() {
            let msg = [b'a' + i as u8; 5];
            w.write_all(&msg).unwrap();
            let mut back = [0u8; 5];
            r.read_exact(&mut back).unwrap();
            assert_eq!(back, msg);
        }
        server.join().unwrap();
    }

    #[test]
    fn the_name_cant_be_taken_twice() {
        let n = name("twice");
        let _first = PipeListener::bind(&n, 4).unwrap();
        let err = PipeListener::bind(&n, 4).err().expect("bound twice");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert!(Pipe::serve(&n).is_err());
    }

    #[test]
    fn closing_ends_a_blocked_read_on_both_sides() {
        let n = name("close");
        let mut listener = PipeListener::bind(&n, 2).unwrap();
        let client = std::thread::spawn({
            let n = n.clone();
            move || {
                let (mut r, _w) = Pipe::connect(&n, Duration::from_secs(5)).unwrap().split().unwrap();
                let mut buf = [0u8; 1];
                r.read(&mut buf).unwrap()
            }
        });
        let pipe = listener.accept().unwrap();
        let closer = pipe.closer();
        let (mut r, mut w) = pipe.split().unwrap();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 1];
            r.read(&mut buf).unwrap()
        });
        std::thread::sleep(Duration::from_millis(200)); // both reads are blocked now
        closer.close();
        assert_eq!(reader.join().unwrap(), 0, "server read sees end of stream");
        assert_eq!(client.join().unwrap(), 0, "client was disconnected");
        assert_eq!(w.write(b"x").unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }
}
