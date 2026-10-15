//! The master clock: what tells the mix thread a block is due.
//!
//! - **Device:** the master output's WASAPI event. The mix thread writes that device's buffer
//!   directly (no ring, no extra latency), rendering as many whole blocks as fit.
//! - **Timer:** a high-resolution waitable timer at the block rate, for when no output is
//!   the master, and as the fallback when the master device fails. Deadlines are computed
//!   from the start time, so timer jitter never accumulates into drift.

/// What the mix loop needs from a clock. Implementations must be real-time safe.
pub trait MasterClock {
    /// Block until work is due (or a short timeout passes). Returns how many blocks to
    /// render now; 0 means nothing yet, so the caller can check its stop flag.
    fn wait(&mut self) -> usize;
    /// Take one rendered block of the master bus (planar, one block long).
    fn deliver(&mut self, left: &[f32], right: &[f32]);
    /// A newly adopted plan plays on a different master device (`None`: timer). Called on
    /// the mix thread at the swap; opening a device isn't real-time safe, but it's rare and
    /// the output changes at that moment anyway.
    fn set_master(&mut self, device: Option<&str>);
}

/// How many whole blocks are due after `elapsed_ns` since start, at [`crate::SAMPLE_RATE`].
pub fn blocks_due(elapsed_ns: u128, block: usize) -> u64 {
    (elapsed_ns * crate::SAMPLE_RATE as u128 / 1_000_000_000 / block as u128) as u64
}

/// Timer catch-up policy: given blocks due in total and blocks rendered so far, how many to
/// render now and how many to skip. A thread that stalled renders at most `max_burst` blocks
/// at once and skips the rest, rather than racing through a long backlog.
pub fn catch_up(due: u64, rendered: u64, max_burst: u64) -> (u64, u64) {
    let behind = due.saturating_sub(rendered);
    (behind.min(max_burst), behind.saturating_sub(max_burst))
}

#[cfg(windows)]
pub use win::{Clock, MasterInfo};

#[cfg(windows)]
mod win {
    use super::{MasterClock, blocks_due, catch_up};
    use crate::backend::wasapi::{Client, RenderBuffer, StreamInfo};
    use crate::rt::stats::MixStats;
    use std::sync::Arc;
    use std::sync::atomic::Ordering::Relaxed;
    use std::time::Instant;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Threading::{
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SetWaitableTimer, TIMER_ALL_ACCESS,
        WaitForSingleObject,
    };
    use windows::core::Result;

    /// At most this many blocks rendered back to back after a stall.
    const MAX_BURST: u64 = 4;

    /// What the mix thread's clock started as.
    #[derive(Debug, Clone, Copy)]
    pub struct MasterInfo {
        /// The master device's stream, or `None` when running on the timer.
        pub device: Option<StreamInfo>,
    }

    /// Device clock with a ready-made timer to fall back to. Lives on the mix thread.
    pub struct Clock {
        device: Option<Device>,
        timer: Timer,
        block: usize,
        stats: Arc<MixStats>,
    }

    struct Device {
        client: Client,
        render: RenderBuffer,
    }

    impl Device {
        fn open(id: &str) -> Result<Self> {
            let client = Client::open(id)?;
            let render = client.start_render()?;
            Ok(Device { client, render })
        }
    }

    impl Clock {
        /// Open `master` (a render endpoint ID) as the clock, or run on the timer if `None`.
        /// Must be called on the mix thread, with COM initialized.
        pub fn new(master: Option<&str>, block: usize, stats: Arc<MixStats>) -> Result<Self> {
            let device = master.map(Device::open).transpose()?;
            Ok(Self {
                device,
                timer: Timer::new(block)?,
                block,
                stats,
            })
        }

        pub fn info(&self) -> MasterInfo {
            MasterInfo {
                device: self.device.as_ref().map(|d| d.client.info),
            }
        }

        fn fall_back(&mut self) {
            self.device = None;
            self.timer.restart();
            self.stats.master_fallback.store(true, Relaxed);
        }
    }

    impl MasterClock for Clock {
        fn wait(&mut self) -> usize {
            let Some(d) = &self.device else {
                let (render, skip) = self.timer.wait();
                if skip > 0 {
                    self.stats.late_blocks.fetch_add(skip, Relaxed);
                }
                return render as usize;
            };
            if !d.client.wait() {
                return 0;
            }
            match d.client.free_frames() {
                Ok(free) => free / self.block,
                Err(_) => {
                    self.fall_back();
                    0
                }
            }
        }

        fn set_master(&mut self, device: Option<&str>) {
            self.device = None; // stop the old device first
            match device.map(Device::open) {
                None => self.timer.restart(),
                Some(Ok(d)) => {
                    self.device = Some(d);
                    self.stats.master_fallback.store(false, Relaxed);
                }
                Some(Err(_)) => self.fall_back(),
            }
        }

        fn deliver(&mut self, left: &[f32], right: &[f32]) {
            let Some(d) = &self.device else { return };
            let written = d.render.write(self.block, |out| {
                for ((frame, l), r) in out.iter_mut().zip(left).zip(right) {
                    *frame = [*l, *r];
                }
            });
            if written.is_err() {
                self.fall_back();
            }
        }
    }

    /// High-resolution waitable timer ticking at the block rate.
    struct Timer {
        handle: HANDLE,
        block: usize,
        start: Instant,
        rendered: u64,
    }

    impl Timer {
        fn new(block: usize) -> Result<Self> {
            let handle = unsafe {
                CreateWaitableTimerExW(None, None, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)?
            };
            Ok(Self {
                handle,
                block,
                start: Instant::now(),
                rendered: 0,
            })
        }

        fn restart(&mut self) {
            self.start = Instant::now();
            self.rendered = 0;
        }

        /// Sleep until the next block is due; returns (blocks to render, blocks skipped).
        fn wait(&mut self) -> (u64, u64) {
            let next_ns = (self.rendered + 1) as u128 * self.block as u128 * 1_000_000_000 / crate::SAMPLE_RATE as u128;
            let remaining_ns = next_ns.saturating_sub(self.start.elapsed().as_nanos());
            if remaining_ns > 0 {
                // Negative due time = relative, in 100 ns units.
                let due = -((remaining_ns / 100).max(1) as i64);
                unsafe {
                    if SetWaitableTimer(self.handle, &due, 0, None, None, false).is_ok() {
                        WaitForSingleObject(self.handle, 200);
                    }
                }
            }
            let due = blocks_due(self.start.elapsed().as_nanos(), self.block);
            let (render, skip) = catch_up(due, self.rendered, MAX_BURST);
            self.rendered += render + skip;
            (render, skip)
        }
    }

    impl Drop for Timer {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.handle) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_due_counts_whole_blocks() {
        // 256 frames at 48 kHz = 5.333 ms.
        assert_eq!(blocks_due(5_333_000, 256), 0);
        assert_eq!(blocks_due(5_334_000, 256), 1);
        assert_eq!(blocks_due(1_000_000_000, 256), 187); // 48000 / 256 = 187.5
        // No accumulated rounding error over a long run: 1 hour.
        assert_eq!(blocks_due(3_600 * 1_000_000_000, 256), 3_600 * 48_000 / 256);
    }

    #[test]
    fn catch_up_caps_bursts() {
        assert_eq!(catch_up(10, 10, 4), (0, 0));
        assert_eq!(catch_up(11, 10, 4), (1, 0));
        assert_eq!(catch_up(14, 10, 4), (4, 0));
        assert_eq!(catch_up(20, 10, 4), (4, 6));
    }
}
