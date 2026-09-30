//! Records for the long-term archive (`history.db`, written by habitd). The
//! engine only queues them; state.json keeps what the engine itself needs.

use crate::state::{Event, HistoryEntry};
use serde::{Deserialize, Serialize};

/// Most records kept waiting when nothing drains the queue (no archive).
pub const MAX_QUEUED: usize = 10_000;

/// One stretch of focus on an app or site: from `start` to `end`, of which
/// `active_ms` wasn't idle. `key` is an app id or `site:<host>`, as in
/// screen time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Visit {
    pub key: String,
    pub start: u64,
    pub end: u64,
    pub active_ms: u64,
    /// The habit whose session was counting, so the time reads as "Reading"
    /// rather than as the app it happened in.
    #[serde(default)]
    pub habit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    Event(Event),
    Session(HistoryEntry),
    Visit(Visit),
}
