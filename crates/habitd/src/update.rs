//! Looks for a newer release on GitHub a minute after start and then once a
//! day (`general.update_check`); the engine puts it in snapshots for the TUI
//! and `hf status`. The request runs off the event loop.

use crate::Event;
use habit_core::snapshot::UpdateView;
use habit_ipc::update::Release;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;

const FIRST_CHECK: Duration = Duration::from_secs(60);
const EVERY: Duration = Duration::from_secs(24 * 3600);
const RETRY: Duration = Duration::from_secs(3600);

pub struct Checker {
    next: Instant,
    busy: bool,
    failing: bool,
}

impl Default for Checker {
    fn default() -> Self {
        Checker { next: Instant::now() + FIRST_CHECK, busy: false, failing: false }
    }
}

impl Checker {
    /// Starts a check when one is due and checks are on.
    pub fn poll(&mut self, enabled: bool, tx: &Sender<Event>) {
        if !enabled || self.busy || Instant::now() < self.next {
            return;
        }
        self.busy = true;
        let tx = tx.clone();
        tokio::task::spawn_blocking(move || {
            let _ = tx.blocking_send(Event::UpdateChecked(habit_ipc::update::available()));
        });
    }

    /// Takes a check's result: what to show (`None` after a failure, which
    /// keeps what was known and tries again in an hour).
    pub fn finished(&mut self, result: Result<Option<Release>, String>) -> Option<Option<UpdateView>> {
        self.busy = false;
        match result {
            Ok(release) => {
                self.next = Instant::now() + EVERY;
                self.failing = false;
                if let Some(r) = &release {
                    eprintln!("habitd: habitfocus {} is available ({}); run `hf update`", r.version, r.url);
                }
                Some(release.map(|r| UpdateView { version: r.version, url: r.url }))
            }
            Err(e) => {
                self.next = Instant::now() + RETRY;
                if !self.failing {
                    eprintln!("habitd: update check failed: {e}");
                    self.failing = true;
                }
                None
            }
        }
    }
}
