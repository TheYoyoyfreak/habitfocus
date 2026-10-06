use crate::config::{HabitKind, OnTarget, RewardMode, Strictness};
use serde::{Deserialize, Serialize};

/// Read-only view of the daemon, streamed to clients (CLI, TUI, shell plugins).
///
/// Fields added after the first release carry `#[serde(default)]`, so clients
/// keep working against a daemon that hasn't been restarted since an upgrade.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub now_ms: u64,
    pub session: Option<SessionView>,
    pub habits: Vec<HabitView>,
    pub groups: Vec<GroupView>,
    pub penalty_remaining_ms: u64,
    pub idle: bool,
    /// Domains of all currently blocked groups.
    #[serde(default)]
    pub blocked_domains: Vec<String>,
    #[serde(default)]
    pub settings: SettingsView,
    #[serde(default)]
    pub lock: Option<LockView>,
    /// Consecutive days with at least one completed habit.
    #[serde(default)]
    pub streak_days: u32,
    /// Total events ever recorded; clients refetch the log when it changes.
    #[serde(default)]
    pub events_seq: u64,
    /// Unspent credit expires at the next day start (unix ms).
    #[serde(default)]
    pub credit_expires_at_ms: u64,
    /// e.g. "04:00" or "sat 04:00".
    #[serde(default)]
    pub credit_expires_label: String,
    /// Display names for app ids and `site:<host>` keys (`[app_names]`).
    #[serde(default)]
    pub app_names: std::collections::BTreeMap<String, String>,
    /// Categories of app ids and `site:<host>` keys (`[app_categories]`).
    #[serde(default)]
    pub app_categories: std::collections::BTreeMap<String, String>,
    /// A newer release is out (`general.update_check`).
    #[serde(default)]
    pub update: Option<UpdateView>,
    /// Multi-device sync, while this device is signed in to a sync server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncView>,
}

/// The sync account and how syncing goes, from habitd's sync thread.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncView {
    pub server: String,
    pub username: String,
    /// This device's name.
    pub device: String,
    /// The last check reached the server; `None` before the first one.
    pub reachable: Option<bool>,
    /// A sync is running.
    pub syncing: bool,
    /// When the last sync succeeded (unix ms).
    pub last_sync_ms: Option<u64>,
    /// Why the last sync failed, when it did.
    pub error: Option<String>,
    /// The server ended this device's session: it syncs no more until signed in again.
    pub signed_out: bool,
    /// The other devices of the account, by name.
    pub devices: Vec<SyncDeviceView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncDeviceView {
    pub id: String,
    pub name: String,
    pub last_seen_ms: Option<u64>,
    /// Archive rows pulled from it.
    pub rows: u64,
}

/// Screen time across devices counted once: time spent at any screen (the
/// union of all visits), next to the summed time of the Insights rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WallClock {
    pub today_ms: u64,
    /// Over the requested period, today included.
    pub total_ms: u64,
}

/// A release newer than the running habitd.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateView {
    /// "0.2.0"
    pub version: String,
    /// The release page.
    pub url: String,
}

/// Screen time of one app (or `site:<host>`), from the `app_stats` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppUsageView {
    pub app: String,
    pub today_ms: u64,
    /// Over the requested number of days, today included.
    pub total_ms: u64,
    /// Separate sessions over the period (visits less than
    /// `SESSION_GAP_MS` apart count as one). From habitd's archive, so zero
    /// without one and only since it exists.
    #[serde(default)]
    pub sessions: u32,
    #[serde(default)]
    pub sessions_today: u32,
    #[serde(default)]
    pub avg_session_ms: u64,
    #[serde(default)]
    pub longest_session_ms: u64,
    /// Use per local hour of the day over the period (24 entries, or none
    /// without an archive).
    #[serde(default)]
    pub hours: Vec<u64>,
    /// The same for today alone.
    #[serde(default)]
    pub hours_today: Vec<u64>,
    /// Use per logical day over the period, oldest first (one entry per day).
    #[serde(default)]
    pub days_ms: Vec<u64>,
    /// Sessions started per day over the period, oldest first (none without
    /// an archive).
    #[serde(default)]
    pub days_sessions: Vec<u32>,
}

/// How an hour of the day was spent: one slice per habit, category or app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HourSlice {
    /// Local hour, 0–23.
    pub hour: u8,
    /// Habit name, else category, else the app's display name.
    pub label: String,
    /// The screen-time key the time happened in (app id or `site:<host>`),
    /// so clients can pick out one app even when the label is its habit.
    #[serde(default)]
    pub key: String,
    pub ms: u64,
    /// The time counted towards a habit.
    pub habit: bool,
}

/// Hourly breakdown of screen time (`app_stats` request).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Breakdown {
    pub slices: Vec<HourSlice>,
    /// `YYYY-MM-DD` of the day, or of the first day of a period.
    pub date: String,
    /// Days back from today (0 = today); 0 for a period.
    pub offset: u32,
    /// Something was recorded before this day, so stepping back makes sense.
    pub more_before: bool,
}

/// Every visit of one logical day, for a timeline (`timeline` request).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timeline {
    /// `YYYY-MM-DD` of the day.
    pub date: String,
    /// Days back from today (0 = today).
    pub offset: u32,
    /// Start and end of the logical day (unix ms), the whole day even today.
    pub from_ms: u64,
    pub to_ms: u64,
    /// Something was recorded before this day, so stepping back makes sense.
    pub more_before: bool,
    /// Oldest first, cut to the day.
    pub visits: Vec<TimelineVisit>,
    /// Time away (idle or asleep), oldest first, cut to the day. Recorded
    /// since the version that introduced it.
    #[serde(default)]
    pub afk: Vec<TimelineAfk>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineAfk {
    pub start_ms: u64,
    pub end_ms: u64,
    pub reason: crate::archive::AfkReason,
}

/// One stretch of focus on an app or site, with its labels resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineVisit {
    /// App id or `site:<host>`.
    pub key: String,
    /// The app's display name.
    pub name: String,
    pub category: Option<String>,
    /// Name of the habit whose session was counting.
    pub habit: Option<String>,
    pub start_ms: u64,
    pub end_ms: u64,
    /// Not idle, within `start_ms`..`end_ms`.
    pub active_ms: u64,
}

/// One logical day of stats (`stats` request).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayView {
    pub day: i64,
    /// `YYYY-MM-DD` of the day's start.
    pub date: String,
    pub habits: std::collections::BTreeMap<String, crate::stats::DayStats>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LockView {
    /// When the commitment ends: its end date, or the end of a requested
    /// early-end cooldown.
    pub ends_at_ms: u64,
    pub remaining_ms: u64,
    pub until_ms: u64,
    pub end_requested: bool,
    /// Easier config changes waiting for the lock to end.
    pub pending: Vec<String>,
    /// Time added for stopping habitd while locked.
    pub extended_ms: u64,
    /// Browser windows without the extension get closed.
    pub browser_guard: bool,
}

/// Editable `[general]` settings, spelled as in the config file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SettingsView {
    #[serde(default)]
    pub sound: String,
    pub day_start: String,
    pub idle_timeout: String,
    pub emergency_penalty: String,
    pub expiry_warning: String,
    pub notifications: bool,
    /// Screen time in terminals goes to the program in front.
    #[serde(default)]
    pub terminal_programs: bool,
    /// Terminals and browsers are put in categories automatically.
    #[serde(default)]
    pub auto_categories: bool,
    #[serde(default)]
    pub update_check: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionView {
    pub habit: String,
    pub name: String,
    pub running: bool,
    pub pause_reason: Option<PauseReason>,
    /// Focused time in the current round (restarts after each full target).
    pub elapsed_ms: u64,
    /// The habit's target, i.e. the length of a round.
    pub target_ms: u64,
    pub remaining_ms: u64,
    pub progress: f64,
    pub strictness: Strictness,
    #[serde(default)]
    pub kind: HabitKind,
    /// The habit's configured target.
    #[serde(default)]
    pub habit_target_ms: u64,
    /// A round was completed and rewarded; waiting for continue or stop.
    #[serde(default)]
    pub awaiting_decision: bool,
    /// Full rounds completed in this session.
    #[serde(default)]
    pub rounds_completed: u32,
    /// Reward banked by this session's rounds.
    #[serde(default)]
    pub banked_ms: u64,
    /// Focused time of the whole session, across rounds.
    #[serde(default)]
    pub total_elapsed_ms: u64,
    #[serde(default)]
    pub resumed_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    Unfocused,
    Idle,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HabitView {
    pub id: String,
    pub name: String,
    pub target_ms: u64,
    pub strictness: Strictness,
    pub reward_groups: Vec<String>,
    pub reward_ms: u64,
    pub reward_mode: RewardMode,
    #[serde(default)]
    pub kind: HabitKind,
    #[serde(default = "default_on_target")]
    pub on_target: OnTarget,
    /// Progress saved from stopped sessions today.
    #[serde(default)]
    pub saved_ms: u64,
    /// Consecutive days completed, still alive while today isn't done.
    #[serde(default)]
    pub streak_days: u32,
    #[serde(default)]
    pub best_streak_days: u32,
    #[serde(default)]
    pub done_today: bool,
    /// Focused today, including a running session.
    #[serde(default)]
    pub today_focused_ms: u64,
    /// Count per round (manual/counter habits).
    #[serde(default)]
    pub goal: u64,
    #[serde(default)]
    pub unit: String,
    /// Logged today (manual/counter habits).
    #[serde(default)]
    pub count_today: u64,
    /// Rewarded rounds today.
    #[serde(default)]
    pub rounds_today: u32,
    #[serde(default)]
    pub daily_limit: Option<u32>,
}

fn default_on_target() -> OnTarget {
    OnTarget::Finish
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupView {
    pub id: String,
    pub name: String,
    pub blocked: bool,
    pub unlock_remaining_ms: u64,
    pub credit_ms: u64,
    pub apps: Vec<String>,
    pub domains: Vec<String>,
    #[serde(default)]
    pub processes: Vec<String>,
    /// The group has a schedule (blocks only inside its windows).
    #[serde(default)]
    pub scheduled: bool,
    /// Outside all schedule windows right now, so nothing is blocked.
    #[serde(default)]
    pub off_schedule: bool,
    /// e.g. "off hours until 22:00", or "until tue 02:00" while blocking.
    #[serde(default)]
    pub schedule_label: Option<String>,
    /// When the schedule next flips (unix ms, 0 = never).
    #[serde(default)]
    pub schedule_next_change_at_ms: u64,
    /// e.g. ["mon tue wed thu fri 07:00-22:00"].
    #[serde(default)]
    pub schedule_summary: Vec<String>,
    /// "wallclock", "usage" or "rest_of_day".
    #[serde(default)]
    pub unlock_mode: String,
    /// While unlocked, e.g. "12:00 left", "10:00 of use left", "until 04:00".
    #[serde(default)]
    pub unlock_label: Option<String>,
    /// When an open unlock ends at the latest (unix ms, 0 = not unlocked).
    #[serde(default)]
    pub unlock_until_ms: u64,
    /// Price of a `rest_of_day` unlock.
    #[serde(default)]
    pub rest_of_day_price_ms: u64,
    /// Habits to complete today before the group can be unlocked.
    #[serde(default)]
    pub requires: Vec<RequirementView>,
    /// Every required habit is needed (otherwise one is enough).
    #[serde(default)]
    pub require_all: bool,
    #[serde(default = "default_true")]
    pub requirements_met: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequirementView {
    pub habit: String,
    pub name: String,
    pub done: bool,
    /// Today's progress towards completing it, 0–1.
    pub progress: f64,
}
