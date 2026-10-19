//! Ties the store, the link and the views together as the shell's [`Content`].

use crate::coalesce::Coalescer;
use crate::link::{Link, LinkEvent};
use crate::log;
use crate::shell::{Content, Waker};
use crate::store::Store;
use crate::views;
use std::sync::mpsc::{self, Receiver};
use std::time::Instant;

pub struct App {
    store: Store,
    link: Link,
    events: Receiver<LinkEvent>,
    /// Fader drags, thinned to ~60 Hz.
    coalescer: Coalescer,
    /// Which view shows (the UI's own preference).
    mode: views::Mode,
    waker: Waker,
}

impl App {
    /// Connect to the controller at `address`; link events wake the shell through `waker`.
    pub fn new(address: String, waker: Waker) -> Self {
        let (tx, events) = mpsc::channel();
        let link = Link::spawn(address, "overlay".into(), tx, move || waker.wake());
        // Hidden until shown: no wakeups for meters meanwhile.
        link.set_visible(false);
        Self {
            store: Store::default(),
            link,
            events,
            coalescer: Coalescer::default(),
            mode: views::Mode::default(),
            waker,
        }
    }
}

impl Content for App {
    fn size(&self) -> (f32, f32) {
        views::SIZE
    }

    fn update(&mut self) -> bool {
        let mut changed = false;
        for ev in self.events.try_iter() {
            changed = true;
            if ev == LinkEvent::Message(sb_shared::ui::UiMessage::ToggleOverlay) {
                self.waker.toggle();
                continue;
            }
            if let LinkEvent::Connected { controller } | LinkEvent::Lost(controller) = &ev {
                log::line(format!("link: {controller}"));
            }
            if let Some(req) = self.store.apply(ev, Instant::now())
                && let Err(e) = self.link.send(&req)
            {
                log::line(format!("resubscribe: {e}"));
            }
        }
        changed
    }

    fn ui(&mut self, ui: &mut egui::Ui) -> bool {
        let now = Instant::now();
        self.store.expire_toasts(now);
        let mut out = views::panel(ui, &self.store, &mut self.mode);
        if out.pick_clips {
            // Modal: this frame waits for the dialog (the overlay stays up meanwhile).
            for path in crate::shell::dialog::pick_audio_files(self.waker.window()) {
                out.commands.push(sb_shared::ui::Command::AddClip {
                    path: path.to_string_lossy().into_owned(),
                    name: String::new(),
                });
            }
        }
        let mut send: Vec<_> = out
            .commands
            .into_iter()
            .filter_map(|c| self.coalescer.push(c, now))
            .collect();
        send.extend(self.coalescer.due(now));
        for cmd in send {
            if let Some(req) = self.store.command(cmd, now)
                && let Err(e) = self.link.send(&req)
            {
                self.store.send_failed(&req, &e, now);
            }
        }
        // A drag's last value goes out when its interval is up, even if the mouse stopped.
        if let Some(at) = self.coalescer.next_due() {
            ui.ctx().request_repaint_after(at.saturating_duration_since(now));
        }
        out.close
    }

    fn visibility(&mut self, shown: bool) {
        self.link.set_visible(shown);
        if shown {
            // Whatever waited while hidden.
            self.update();
        }
    }
}
