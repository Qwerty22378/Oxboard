//! Windows named pipes for the engine link.
//!
//! - **One instance, local only, owner only.** The pipe is created with
//!   `FILE_FLAG_FIRST_PIPE_INSTANCE` (no one can squat the name first and impersonate the
//!   host), one instance (a second client is refused), `PIPE_REJECT_REMOTE_CLIENTS`, and an
//!   explicit DACL granting access to SYSTEM and the creating user only (the default DACL lets
//!   *Everyone* read). The controller also picks an unguessable name per launch.
//! - **Overlapped I/O.** Synchronous I/O on one pipe handle is serialized by Windows, so a
//!   thread blocked in `ReadFile` would block every `WriteFile` too. Each half here does its
//!   own overlapped operation with its own event, so reading and writing never wait on each
//!   other.

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_PIPE_BUSY,
    ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_SHARE_MODE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::CreateEventW;
use windows::core::{HRESULT, HSTRING, PCWSTR};

/// SYSTEM and the owner (the user who created the pipe) get full access; nobody else any.
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;OW)";
const BUFFER: u32 = 64 * 1024;

struct Handle(HANDLE);

// A kernel handle; both halves use it concurrently with their own OVERLAPPED, which is the
// supported way to share a pipe between threads.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
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

impl Pipe {
    /// Create the pipe `name` (`\\.\pipe\...`) and wait for the one client.
    pub fn serve(name: &str) -> io::Result<Pipe> {
        unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(PIPE_SDDL),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
            .map_err(io_err)?;
            let sa = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.0,
                bInheritHandle: false.into(),
            };
            let h = CreateNamedPipeW(
                &HSTRING::from(name),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                BUFFER,
                BUFFER,
                0,
                Some(&sa),
            );
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            if h.is_invalid() {
                return Err(io::Error::last_os_error());
            }
            let pipe = Pipe {
                handle: Arc::new(Handle(h)),
            };
            let event = Event::new()?;
            let mut ov = OVERLAPPED {
                hEvent: event.0,
                ..Default::default()
            };
            match ConnectNamedPipe(h, Some(&mut ov)) {
                Ok(()) => {}
                Err(e) if is(&e, ERROR_PIPE_CONNECTED) => {}
                Err(e) if is(&e, ERROR_IO_PENDING) => {
                    let mut n = 0;
                    GetOverlappedResult(h, &ov, &mut n, true).map_err(io_err)?;
                }
                Err(e) => return Err(io_err(e)),
            }
            Ok(pipe)
        }
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
                        handle: Arc::new(Handle(h)),
                    });
                }
                Err(e) if (is(&e, ERROR_FILE_NOT_FOUND) || is(&e, ERROR_PIPE_BUSY)) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(io_err(e)),
            }
        }
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

/// The other side going away reads as end of stream.
fn closed(e: &windows::core::Error) -> bool {
    is(e, ERROR_BROKEN_PIPE) || is(e, ERROR_PIPE_NOT_CONNECTED) || is(e, ERROR_NO_DATA)
}

pub struct PipeReader {
    handle: Arc<Handle>,
    event: Event,
}

impl io::Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let h = self.handle.0;
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
        let h = self.handle.0;
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
