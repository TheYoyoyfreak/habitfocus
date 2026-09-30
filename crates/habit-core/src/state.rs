use crate::config::UnlockMode;
use crate::lock::{Heartbeat, Lock};
use crate::stats::{AppDays, Days};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const MAX_HISTORY: usize = 1000;
const MAX_EVENTS: usize = 500;

/// Everything that survives a daemon restart. Times are unix milliseconds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub session: Option<Session>,
    /// Banked unlock time per group.
    pub credits: BTreeMap<String, u64>,
    /// Legacy wall-clock unlocks (group -> until). Moved into `open_unlocks`
    /// on load; never written again.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub unlocks: BTreeMap<String, u64>,
    /// Active unlocks per group.
    pub open_unlocks: BTreeMap<String, Unlock>,
    /// Logical day `credits` were earned on. `None` (new or upgraded state)
    /// adopts the current day, so an upgrade never expires credit.
    pub credit_day: Option<i64>,
    /// Focused time of stopped sessions, resumed by the next start of the
    /// habit on the same day.
    pub progress: BTreeMap<String, SavedProgress>,
    pub lock: Option<Lock>,
    pub heartbeat: Option<Heartbeat>,
    /// Unlocks are refused until this time.
    pub penalty_until: Option<u64>,
    /// Recent sessions, capped at `MAX_HISTORY`.
    pub history: Vec<HistoryEntry>,
    /// Daily totals per habit (uncapped; feeds streaks).
    pub days: Days,
    /// Focused time per app per day (pruned to `general.screen_time_days`).
    pub app_days: AppDays,
    /// Recent things that happened, newest last, capped at `MAX_EVENTS`.
    pub events: Vec<Event>,
    /// Counts every event ever pushed, so clients can tell "nothing new"
    /// without diffing the list. Never decreases when old events are pruned.
    pub events_seq: u64,
}

/// One line of the activity log. `text` is rendered by the engine so every
/// client (Rust, Luau, JS) shows the same wording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub at: u64,
    pub kind: EventKind,
    /// Habit or group id the event is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Milliseconds, or a count for `count_logged`.
    #[serde(default)]
    pub amount: u64,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    // The notification title of each kind lives in `Engine::event_title`.
    DaemonStarted,
    ConfigReloaded,
    SessionStarted,
    SessionStopped,
    HabitCompleted,
    RoundCompleted,
    CountLogged,
    CreditEarned,
    CreditSpent,
    CreditExpired,
    Unlocked,
    UnlockEnded,
    Relocked,
    UsageExhausted,
    BlockEnforced,
    ScheduleOpened,
    ScheduleClosed,
    PenaltyStarted,
    PenaltyEnded,
    LockStarted,
    LockExtended,
    LockEndRequested,
    LockEnded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unlock {
    /// Copied from the group when granted, so a reload can't change it.
    pub mode: UnlockMode,
    pub started_at: u64,
    /// When the unlock ends at the latest (for `usage` and `rest_of_day`:
    /// the next day start).
    pub until: u64,
    /// Credit spent on (or granted to) this unlock.
    #[serde(default)]
    pub granted_ms: u64,
    /// Remaining budget of actual use (`usage` mode only).
    #[serde(default)]
    pub usage_left_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub habit: String,
    pub started_at: u64,
    /// Focused time counted so far, excluding the current running stretch.
    pub accumulated_ms: u64,
    /// Start of the current running stretch; `None` while paused.
    /// Not persisted: time while the daemon was down never counts.
    #[serde(skip)]
    pub running_since: Option<u64>,
    /// Progress carried over from earlier today when the session started.
    #[serde(default)]
    pub resumed_ms: u64,
    /// Full targets completed (and rewarded) in this session. Only habits
    /// with `on_target = "ask"` go past the first.
    #[serde(default)]
    pub extra_rounds: u32,
    /// A round was just completed; waiting for the user to continue or stop.
    #[serde(default)]
    pub awaiting: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedProgress {
    pub ms: u64,
    /// Logical day (see `Engine::day_of`) the progress belongs to.
    pub day: i64,
}

impl Session {
    pub fn elapsed(&self, now: u64) -> u64 {
        self.accumulated_ms + self.running_since.map_or(0, |since| now.saturating_sub(since))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub habit: String,
    pub started_at: u64,
    pub finished_at: u64,
    pub focused_ms: u64,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    /// Stopped early; the progress was saved for later today.
    Stopped,
    Aborted,
    EmergencyAborted,
}

impl State {
    pub fn from_json(text: &str) -> Result<State, String> {
        serde_json::from_str(text).map_err(|e| e.to_string())
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("state serializes")
    }

    pub(crate) fn push_event(&mut self, event: Event) {
        self.events.push(event);
        self.events_seq += 1;
        if self.events.len() > MAX_EVENTS {
            let excess = self.events.len() - MAX_EVENTS;
            self.events.drain(..excess);
        }
    }

    pub(crate) fn record(&mut self, entry: HistoryEntry) {
        self.history.push(entry);
        if self.history.len() > MAX_HISTORY {
            let excess = self.history.len() - MAX_HISTORY;
            self.history.drain(..excess);
        }
    }
}
