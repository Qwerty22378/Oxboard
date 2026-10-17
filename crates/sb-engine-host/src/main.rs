//! `sb-engine-host --pipe \\.\pipe\NAME [--fake-driver]`
//!
//! Serves one controller on the named pipe `NAME`, then exits: when the controller sends
//! `Shutdown`, disconnects, or crashes (the pipe breaks). Exit code 0 for those, 1 if the
//! handshake was refused, 2 if the mix thread died, 3 on bad arguments or pipe errors.
//!
//! `--fake-driver` serves virtual endpoints 1 (an app playing 660 Hz) and 2 (a virtual mic)
//! through the user-mode `FakeDriver`, until the real driver link exists (D6).

#[cfg(not(windows))]
fn main() {
    eprintln!("sb-engine-host serves a Windows named pipe; on other platforms use the library");
    std::process::exit(3);
}

#[cfg(windows)]
fn main() {
    std::process::exit(win::run());
}

#[cfg(windows)]
mod win {
    use sb_engine::backend::driver::{FakeDriver, FakeEndpoint};
    use sb_engine::backend::wasapi::{ComGuard, WasapiEndpoints};
    use sb_engine::engine::{start_wasapi, start_wasapi_with_driver};
    use sb_engine::graph::compile::Tables;
    use sb_engine::rt::mix_thread::MixThread;
    use sb_engine::rt::stats::MixSnapshot;
    use sb_engine_host::host::{Backend, Exit, HostConfig, MixHandle, Started, serve};
    use sb_engine_host::pipe::Pipe;
    use sb_ring::ioctl::Direction;

    struct Wasapi {
        fake_driver: bool,
    }

    struct Mix(MixThread);

    impl MixHandle for Mix {
        fn stats(&self) -> MixSnapshot {
            self.0.stats.snapshot()
        }
        fn is_finished(&self) -> bool {
            self.0.is_finished()
        }
    }

    impl Backend for Wasapi {
        type E = WasapiEndpoints;
        fn start(
            &mut self,
            block: usize,
            tables: Tables,
            master: Option<&str>,
        ) -> Result<Started<WasapiEndpoints>, String> {
            let started = if self.fake_driver {
                let driver = FakeDriver::new(vec![
                    FakeEndpoint {
                        id: 1,
                        direction: Direction::FromApps,
                        period_frames: 480,
                        ppm: 0.0,
                        tone_hz: 660.0,
                    },
                    FakeEndpoint {
                        id: 2,
                        direction: Direction::ToApps,
                        period_frames: 480,
                        ppm: 0.0,
                        tone_hz: 0.0,
                    },
                ]);
                start_wasapi_with_driver(block, tables, master, Box::new(driver))
            } else {
                start_wasapi(block, tables, master)
            };
            let (engine, mix) = started.map_err(|e| e.message())?;
            Ok((engine, Box::new(Mix(mix))))
        }
    }

    pub fn run() -> i32 {
        let mut args = std::env::args().skip(1);
        let (mut pipe, mut fake_driver) = (None, false);
        while let Some(a) = args.next() {
            match a.as_str() {
                "--pipe" => pipe = args.next(),
                "--fake-driver" => fake_driver = true,
                other => {
                    eprintln!("unknown argument {other}");
                    return 3;
                }
            }
        }
        let Some(name) = pipe else {
            eprintln!("usage: sb-engine-host --pipe \\\\.\\pipe\\NAME [--fake-driver]");
            return 3;
        };
        // The engine's control thread (this one) opens devices: it needs COM.
        let _com = match ComGuard::new() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("COM: {e}");
                return 3;
            }
        };
        let (reader, writer) = match Pipe::serve(&name).and_then(Pipe::split) {
            Ok(rw) => rw,
            Err(e) => {
                eprintln!("pipe {name}: {e}");
                return 3;
            }
        };
        match serve(Wasapi { fake_driver }, reader, writer, HostConfig::default()) {
            Exit::Shutdown | Exit::Disconnected => 0,
            Exit::Refused(reason) => {
                eprintln!("refused: {reason}");
                1
            }
            Exit::MixFailed => {
                eprintln!("the mix thread died");
                2
            }
        }
    }
}
