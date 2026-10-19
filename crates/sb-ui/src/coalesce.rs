//! Fader drags make a value per mouse move; the controller needs far fewer. Commands that set
//! a continuous value are sent at most once per [`INTERVAL`] per target: the first at once,
//! then the latest when the interval is up (so the last value of a drag always goes out).
//! Everything else passes straight through, and drops any held value for the same target, so
//! a drag followed quickly by "off" doesn't come back on.

use sb_shared::ui::{AppKey, Command, Sink, Source};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// ~60 Hz.
pub const INTERVAL: Duration = Duration::from_millis(16);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Master,
    App(AppKey),
    Route(Source, Sink),
    Clip(u32),
}

/// The target `cmd` sets, and whether it's a continuous value (held back) or not.
fn key(cmd: &Command) -> Option<(Key, bool)> {
    Some(match cmd {
        Command::SetMasterGain(_) => (Key::Master, true),
        Command::SetMasterMuted(_) => (Key::Master, false),
        Command::SetAppGain { app, .. } => (Key::App(app.clone()), true),
        Command::SetAppMuted { app, .. } => (Key::App(app.clone()), false),
        Command::SetRoute { from, to, gain } => (Key::Route(from.clone(), to.clone()), gain.is_some()),
        Command::UpdateClip { id, .. } => (Key::Clip(id.0), true),
        Command::RemoveClip { id } => (Key::Clip(id.0), false),
        _ => return None,
    })
}

#[derive(Debug)]
struct Slot {
    last_sent: Instant,
    held: Option<Command>,
}

#[derive(Debug, Default)]
pub struct Coalescer {
    slots: BTreeMap<Key, Slot>,
}

impl Coalescer {
    /// What to send now for `cmd` (it, or nothing if it's held back).
    pub fn push(&mut self, cmd: Command, now: Instant) -> Option<Command> {
        let Some((k, continuous)) = key(&cmd) else {
            return Some(cmd);
        };
        if !continuous {
            // A mute or a switch-off: it wins over anything still held for the target.
            self.slots.remove(&k);
            return Some(cmd);
        }
        match self.slots.get_mut(&k) {
            Some(s) if now < s.last_sent + INTERVAL => {
                s.held = Some(cmd);
                None
            }
            _ => {
                self.slots.insert(
                    k,
                    Slot {
                        last_sent: now,
                        held: None,
                    },
                );
                Some(cmd)
            }
        }
    }

    /// Held values whose interval is up.
    pub fn due(&mut self, now: Instant) -> Vec<Command> {
        let mut out = Vec::new();
        for s in self.slots.values_mut() {
            if s.held.is_some() && now >= s.last_sent + INTERVAL {
                out.extend(s.held.take());
                s.last_sent = now;
            }
        }
        // Targets quiet for a while start fresh (their next change goes out at once).
        self.slots
            .retain(|_, s| s.held.is_some() || now < s.last_sent + INTERVAL * 4);
        out
    }

    /// When [`Coalescer::due`] next has something.
    pub fn next_due(&self) -> Option<Instant> {
        self.slots
            .values()
            .filter(|s| s.held.is_some())
            .map(|s| s.last_sent + INTERVAL)
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_shared::ui::DeviceKey;

    fn ms(t: Instant, n: u64) -> Instant {
        t + Duration::from_millis(n)
    }

    #[test]
    fn a_drag_sends_the_first_then_the_latest_per_interval() {
        let t = Instant::now();
        let mut c = Coalescer::default();
        assert_eq!(
            c.push(Command::SetMasterGain(0.1), t),
            Some(Command::SetMasterGain(0.1))
        );
        assert_eq!(c.push(Command::SetMasterGain(0.2), ms(t, 2)), None);
        assert_eq!(c.push(Command::SetMasterGain(0.3), ms(t, 5)), None);
        assert_eq!(c.next_due(), Some(ms(t, 16)));
        assert!(c.due(ms(t, 10)).is_empty());
        assert_eq!(c.due(ms(t, 16)), [Command::SetMasterGain(0.3)]);
        assert_eq!(c.next_due(), None);
        // Another target isn't held back by this one.
        let app = Command::SetAppGain {
            app: AppKey("a".into()),
            gain: 0.5,
        };
        assert_eq!(c.push(app.clone(), ms(t, 17)), Some(app));
    }

    #[test]
    fn a_switch_off_drops_what_was_held() {
        let t = Instant::now();
        let (from, to) = (Source::Clips, Sink::Device(DeviceKey("d".into())));
        let set = |g| Command::SetRoute {
            from: from.clone(),
            to: to.clone(),
            gain: g,
        };
        let mut c = Coalescer::default();
        assert!(c.push(set(Some(0.5)), t).is_some());
        assert!(c.push(set(Some(0.6)), ms(t, 3)).is_none());
        assert_eq!(c.push(set(None), ms(t, 5)), Some(set(None)));
        assert!(c.due(ms(t, 40)).is_empty(), "0.6 must not turn it back on");
    }

    #[test]
    fn other_commands_pass_straight_through() {
        let t = Instant::now();
        let mut c = Coalescer::default();
        for _ in 0..3 {
            assert_eq!(c.push(Command::RestartEngine, t), Some(Command::RestartEngine));
        }
    }
}
