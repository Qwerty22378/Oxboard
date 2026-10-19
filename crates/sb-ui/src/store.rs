//! What the UI knows: the controller's view (kept in sync from snapshots and diffs), the link's
//! state, commands still in flight, toasts and the latest meters. It owns no authoritative
//! state: everything here is a copy, or the UI's own bookkeeping.
//!
//! Pure: link events in, requests to send out. The views read it and ask it for commands.

use crate::link::LinkEvent;
use sb_shared::events::MeterLevels;
use sb_shared::ui::{Command, MeterTarget, Mirror, RequestId, StateView, Synced, UiMessage, UiRequest};
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// How long a toast stays up.
pub const TOAST_FOR: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq)]
pub enum Connection {
    Connecting,
    Connected { controller: String },
    Lost { reason: String },
}

#[derive(Debug, Clone)]
pub struct Pending {
    pub cmd: Command,
    /// The controller has it (`Accepted`); multi-step commands stay here until they finish.
    pub accepted: bool,
    pub since: Instant,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Toast {
    pub text: String,
    pub until: Instant,
}

#[derive(Debug)]
pub struct Store {
    pub mirror: Mirror,
    pub connection: Connection,
    pub pending: BTreeMap<RequestId, Pending>,
    pub toasts: VecDeque<Toast>,
    /// Latest levels by what they measure (peak hold and decay are the widgets' business).
    pub meters: BTreeMap<MeterTarget, MeterLevels>,
    next_id: RequestId,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            mirror: Mirror::default(),
            connection: Connection::Connecting,
            pending: BTreeMap::new(),
            toasts: VecDeque::new(),
            meters: BTreeMap::new(),
            next_id: 0,
        }
    }
}

impl Store {
    /// The view, once a snapshot has arrived.
    pub fn view(&self) -> Option<&StateView> {
        self.mirror.synced.then_some(&self.mirror.view)
    }

    pub fn connected(&self) -> bool {
        matches!(self.connection, Connection::Connected { .. })
    }

    fn toast(&mut self, text: impl Into<String>, now: Instant) {
        self.toasts.push_back(Toast {
            text: text.into(),
            until: now + TOAST_FOR,
        });
    }

    /// Take in what the link brought; returns what to send back (a fresh subscription after a
    /// gap in the diffs).
    pub fn apply(&mut self, ev: LinkEvent, now: Instant) -> Option<UiRequest> {
        match ev {
            LinkEvent::Connected { controller } => {
                self.connection = Connection::Connected { controller };
                None
            }
            LinkEvent::Lost(reason) => {
                // Whatever was in flight may or may not have happened; the next snapshot tells.
                if !self.pending.is_empty() {
                    let n = self.pending.len();
                    self.toast(
                        format!("Lost the controller with {n} change(s) in flight; check they took"),
                        now,
                    );
                    self.pending.clear();
                }
                self.mirror.synced = false;
                self.meters.clear();
                self.connection = Connection::Lost { reason };
                None
            }
            LinkEvent::Message(msg) => self.message(msg, now),
        }
    }

    fn message(&mut self, msg: UiMessage, now: Instant) -> Option<UiRequest> {
        match &msg {
            UiMessage::Snapshot { .. } | UiMessage::Diff { .. } => {
                return (self.mirror.apply(&msg) == Synced::Gap).then_some(UiRequest::Subscribe);
            }
            UiMessage::Accepted(id) => {
                if let Some(p) = self.pending.get_mut(id) {
                    p.accepted = true;
                }
            }
            UiMessage::Completed(id) => {
                self.pending.remove(id);
            }
            UiMessage::Failed(id, e) => {
                self.pending.remove(id);
                self.toast(e.message.clone(), now);
            }
            UiMessage::Meters(levels) => {
                self.meters.clear();
                self.meters.extend(levels.iter().cloned());
            }
            // Only during the handshake, which the link handles; the app shows and hides.
            UiMessage::Welcome { .. } | UiMessage::Refused { .. } | UiMessage::ToggleOverlay => {}
        }
        None
    }

    /// Register `cmd` and return the request that sends it; `None` (and a toast) when there's
    /// no controller to send it to.
    pub fn command(&mut self, cmd: Command, now: Instant) -> Option<UiRequest> {
        if !self.connected() {
            self.toast("Not connected to the soundboard service", now);
            return None;
        }
        self.next_id += 1;
        let id = self.next_id;
        self.pending.insert(
            id,
            Pending {
                cmd: cmd.clone(),
                accepted: false,
                since: now,
            },
        );
        Some(UiRequest::Command { id, cmd })
    }

    /// The send itself failed: forget the command and say so.
    pub fn send_failed(&mut self, req: &UiRequest, why: &str, now: Instant) {
        if let UiRequest::Command { id, .. } = req {
            self.pending.remove(id);
        }
        self.toast(format!("Not sent: {why}"), now);
    }

    /// Whether a command matching `pred` is still in flight (a row's spinner).
    pub fn is_pending(&self, pred: impl Fn(&Command) -> bool) -> bool {
        self.pending.values().any(|p| pred(&p.cmd))
    }

    /// Drop toasts whose time is up; returns when the next one expires.
    pub fn expire_toasts(&mut self, now: Instant) -> Option<Instant> {
        self.toasts.retain(|t| t.until > now);
        self.toasts.iter().map(|t| t.until).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_shared::ui::{DiffOp, ErrorInfo, ErrorKind, Level};

    fn connected() -> Store {
        let mut s = Store::default();
        s.apply(
            LinkEvent::Connected {
                controller: "sb-controller test".into(),
            },
            Instant::now(),
        );
        s
    }

    fn snapshot(seq: u64, gain: f32) -> LinkEvent {
        let mut view = StateView::default();
        view.master.gain = gain;
        LinkEvent::Message(UiMessage::Snapshot {
            seq,
            view: Box::new(view),
        })
    }

    fn master(seq: u64, gain: f32) -> LinkEvent {
        LinkEvent::Message(UiMessage::Diff {
            seq,
            ops: vec![DiffOp::Master(Level { gain, muted: false })],
        })
    }

    #[test]
    fn a_snapshot_then_diffs_and_a_gap_asks_again() {
        let now = Instant::now();
        let mut s = connected();
        assert!(s.view().is_none(), "nothing to show before the snapshot");
        assert_eq!(s.apply(snapshot(3, 0.5), now), None);
        assert_eq!(s.apply(master(4, 0.6), now), None);
        assert_eq!(s.view().unwrap().master.gain, 0.6);
        assert_eq!(
            s.apply(master(6, 0.7), now),
            Some(UiRequest::Subscribe),
            "5 went missing"
        );
        assert!(s.view().is_none());
        assert_eq!(s.apply(master(7, 0.8), now), None, "ignored until the snapshot");
        s.apply(snapshot(7, 0.8), now);
        assert_eq!(s.view().unwrap().master.gain, 0.8);
    }

    #[test]
    fn commands_are_tracked_until_they_finish_and_failures_toast() {
        let now = Instant::now();
        let mut s = connected();
        let req = s.command(Command::SetMasterMuted(true), now).unwrap();
        let UiRequest::Command { id, .. } = req else { panic!() };
        assert!(s.is_pending(|c| matches!(c, Command::SetMasterMuted(_))));
        s.apply(LinkEvent::Message(UiMessage::Accepted(id)), now);
        assert!(s.pending[&id].accepted);
        s.apply(LinkEvent::Message(UiMessage::Completed(id)), now);
        assert!(s.pending.is_empty());

        let UiRequest::Command { id, .. } = s.command(Command::SetMasterGain(9.0), now).unwrap() else {
            panic!()
        };
        let e = ErrorInfo::new(ErrorKind::Invalid, "gain 9 is outside 0 to 2");
        s.apply(LinkEvent::Message(UiMessage::Failed(id, e)), now);
        assert!(s.pending.is_empty());
        assert_eq!(s.toasts[0].text, "gain 9 is outside 0 to 2");
        assert_eq!(s.expire_toasts(now), Some(now + TOAST_FOR));
        assert_eq!(s.expire_toasts(now + TOAST_FOR), None);
        assert!(s.toasts.is_empty());
    }

    #[test]
    fn losing_the_controller_drops_what_was_in_flight_and_the_view() {
        let now = Instant::now();
        let mut s = connected();
        s.apply(snapshot(1, 1.0), now);
        s.command(Command::SetMasterMuted(true), now).unwrap();
        s.apply(LinkEvent::Lost("the controller closed the link".into()), now);
        assert!(s.pending.is_empty() && s.view().is_none());
        assert_eq!(s.toasts.len(), 1);
        assert_eq!(
            s.connection,
            Connection::Lost {
                reason: "the controller closed the link".into()
            }
        );
        // Nothing can be sent now; the user is told.
        assert_eq!(s.command(Command::SetMasterMuted(false), now), None);
        assert_eq!(s.toasts.len(), 2);
        // Back: a fresh snapshot starts over.
        s.apply(
            LinkEvent::Connected {
                controller: "again".into(),
            },
            now,
        );
        s.apply(snapshot(1, 0.3), now);
        assert_eq!(s.view().unwrap().master.gain, 0.3);
    }

    #[test]
    fn meters_are_the_latest_frame() {
        let now = Instant::now();
        let mut s = connected();
        let lv = |p| MeterLevels {
            peak: [p, p],
            rms: [p, p],
        };
        s.apply(
            LinkEvent::Message(UiMessage::Meters(vec![
                (MeterTarget::Clips, lv(0.5)),
                (MeterTarget::System, lv(0.2)),
            ])),
            now,
        );
        s.apply(
            LinkEvent::Message(UiMessage::Meters(vec![(MeterTarget::Clips, lv(0.1))])),
            now,
        );
        assert_eq!(s.meters.len(), 1);
        assert_eq!(s.meters[&MeterTarget::Clips].peak, [0.1, 0.1]);
    }
}
