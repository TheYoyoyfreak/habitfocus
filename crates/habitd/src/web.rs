//! Serves the insights page (`habit_ipc::web`) with `general.web`, so it's on
//! 127.0.0.1 whenever habitd runs, without `hf web`. The server runs on a
//! thread of its own and asks habitd over the socket like any client.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Wait before binding again after the port was taken (e.g. by `hf web`).
const RETRY: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Server {
    running: Option<(u16, Arc<AtomicBool>)>,
    /// The port that couldn't be bound and when to try again.
    failed: Option<(u16, Instant)>,
}

impl Server {
    /// Starts, stops or moves the server to match the config.
    pub fn poll(&mut self, enabled: bool, port: u16) {
        if self.running.as_ref().is_some_and(|(p, _)| enabled && *p == port) {
            return;
        }
        self.stop();
        if !enabled {
            self.failed = None;
            return;
        }
        if self.failed.is_some_and(|(p, retry_at)| p == port && Instant::now() < retry_at) {
            return;
        }
        match habit_ipc::web::bind(port) {
            Ok(listener) => {
                eprintln!("habitd: insights on {}", habit_ipc::web::url(port));
                let stop = Arc::new(AtomicBool::new(false));
                let flag = stop.clone();
                std::thread::spawn(move || habit_ipc::web::serve(listener, port, &flag));
                self.running = Some((port, stop));
                self.failed = None;
            }
            Err(e) => {
                if self.failed.is_none_or(|(p, _)| p != port) {
                    eprintln!("habitd: can't serve the insights on 127.0.0.1:{port}: {e}; trying again every 30 s");
                }
                self.failed = Some((port, Instant::now() + RETRY));
            }
        }
    }

    fn stop(&mut self) {
        if let Some((port, stop)) = self.running.take() {
            stop.store(true, Ordering::Relaxed);
            habit_ipc::web::wake(port);
            eprintln!("habitd: stopped serving the insights");
        }
    }
}
