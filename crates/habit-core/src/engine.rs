use crate::config::{
    Config, Group, Habit, HabitKind, OnTarget, RequireMode, RewardMode, Strictness, UnlockMode, Weekday,
    TerminalFront, MINUTES_PER_WEEK, TERMINAL_PREFIX,
};
use crate::duration::{format_duration, format_duration_config, format_duration_long, format_time_of_day};
use crate::lock::{Lock, END_DELAY_MS};
use crate::snapshot::{
    AppUsageView, Breakdown, DayView, GroupView, HabitView, HourSlice, LockView, PauseReason, RequirementView,
    SessionView, SettingsView, Snapshot, Timeline, TimelineAfk, TimelineVisit, UpdateView,
};
use crate::archive::{self, Afk, AfkReason, Record, Visit};
use crate::stats;
use crate::state::{Event, EventKind, HistoryEntry, Outcome, SavedProgress, Session, State, Unlock};
use std::collections::{BTreeMap, BTreeSet};

const DAY_MS: i64 = 24 * 3_600_000;
/// A timer client that hasn't reported for this long counts as unfocused
/// (e.g. the TUI was killed while focused).
pub const TIMER_STALE_MS: u64 = 6_000;
/// A browser window gets this long for its extension to connect during a lock.
pub const BROWSER_GRACE_MS: u64 = 30_000;
/// An extension host that hasn't said hello for this long counts as gone.
pub const EXTENSION_STALE_MS: u64 = 60_000;
/// Longest step of screen-time accrual. After a suspend or a clock jump the
/// daemon sees a single tick with an arbitrarily large delta.
pub const MAX_USAGE_STEP_MS: u64 = 5_000;
/// Shorter visits (switching through windows) aren't archived.
pub const MIN_VISIT_MS: u64 = 1_000;
/// A jump at least this long between two observations is a suspend, archived
/// as asleep.
pub const MIN_ASLEEP_MS: u64 = 30_000;
/// Visits of the same app or site closer together than this are one session.
pub const SESSION_GAP_MS: u64 = 60_000;
const HOUR_MS: u64 = 3_600_000;

/// A screen-time key without its `site:` or `term:` prefix.
fn display_key(key: &str) -> &str {
    key.strip_prefix("site:").or_else(|| key.strip_prefix(TERMINAL_PREFIX)).unwrap_or(key)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: u64,
    pub app_id: String,
    pub title: String,
    pub pid: Option<u32>,
}

/// Observations from the compositor, idle detector and clock.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// Full window list (replaces everything known).
    WindowsReset(Vec<WindowInfo>),
    /// A window opened or its app id / title changed.
    WindowChanged(WindowInfo),
    WindowClosed(u64),
    FocusChanged(Option<u64>),
    Idle(bool),
    /// Terminal focus of a timer client (`hf tui`). `focused: None` removes
    /// the client. Clients re-report while focused as a heartbeat.
    TimerFocus {
        source: String,
        focused: Option<bool>,
    },
    /// The extension's native host is alive in the browser process `pid`.
    /// Sent on connect and periodically.
    BrowserHello {
        source: String,
        pid: u32,
    },
    /// Active tab of a browser window, reported by the extension. `tab: None`
    /// forgets the window; `window: None` forgets everything from `source`.
    BrowserTab {
        source: String,
        window: Option<u64>,
        tab: Option<BrowserTab>,
    },
    /// URLs of the tabs of `source` playing sound, focused or not. Replaces
    /// the previous report.
    BrowserMedia {
        source: String,
        urls: Vec<String>,
    },
    /// What's in front in terminal window `window`, found by the daemon: the
    /// program (e.g. "nvim"; `None` when it's only the shell) and the tmux
    /// session shown (`None` outside tmux).
    TerminalProgram {
        window: u64,
        program: Option<String>,
        tmux_session: Option<String>,
    },
    Tick,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserTab {
    pub title: String,
    pub url: String,
}

/// Actions the daemon must carry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    CloseWindow(u64),
    FocusWindow(u64),
    Notify { title: String, body: String },
    /// A timed habit reached its target: play `general.sound`.
    PlaySound(String),
    /// The commitment lock ended: load the config file again.
    ReloadConfig,
}

pub struct Engine {
    config: Config,
    state: State,
    windows: BTreeMap<u64, WindowInfo>,
    focused: Option<u64>,
    idle: bool,
    /// Most recently focused window allowed by the running habit.
    last_allowed: Option<u64>,
    /// Windows we already asked the compositor to close.
    closing: BTreeSet<u64>,
    /// Groups whose upcoming relock was already announced.
    warned: BTreeSet<String>,
    /// App id -> when a "blocked" event was last logged for it.
    blocked_logged: BTreeMap<String, u64>,
    /// Scheduled groups inside a window at the last tick (not persisted).
    schedule_blocking: BTreeSet<String>,
    /// `schedule_blocking` reflects the current config; false after startup
    /// and reloads so those don't announce every window as "just opened".
    schedule_seeded: bool,
    /// Scheduled groups whose next window was already announced.
    schedule_warned: BTreeSet<String>,
    /// Active tab per (extension instance, browser window).
    tabs: BTreeMap<(String, u64), BrowserTab>,
    /// Tabs playing sound per extension instance: their URLs.
    media: BTreeMap<String, Vec<String>>,
    /// Program in front per terminal window, e.g. "nvim".
    programs: BTreeMap<u64, String>,
    /// tmux session shown per terminal window.
    tmux_sessions: BTreeMap<u64, String>,
    /// Extension native hosts: source -> (browser pid, last hello).
    extension_hosts: BTreeMap<String, (u32, u64)>,
    /// Browser windows without a live extension: window -> first seen.
    unverified_browsers: BTreeMap<u64, u64>,
    /// Timer clients: source -> (focused, last report).
    timer_clients: BTreeMap<String, (bool, u64)>,
    /// Compositor window of the most recently focused timer client.
    timer_window: Option<u64>,
    /// When screen time was last accrued (not persisted: downtime never counts).
    usage_flushed_at: Option<u64>,
    /// Screen time accrued since the last tick, folded into `state.app_days`.
    app_pending: BTreeMap<String, u64>,
    /// `state.app_days` changed since the daemon last saved for it. Kept apart
    /// from `dirty` so focus changes don't rewrite state.json every time.
    usage_dirty: bool,
    /// The app or site focused right now, archived once focus moves on.
    visit: Option<Visit>,
    /// Idle since then; archived as AFK once input returns.
    afk_since: Option<u64>,
    /// Records waiting for the daemon to write them to the archive.
    archive: Vec<Record>,
    /// A newer release, as habitd's daily check found (not persisted).
    update: Option<UpdateView>,
    /// Local UTC offset in ms for a given UTC time.
    local_offset: Box<dyn Fn(u64) -> i64 + Send>,
    dirty: bool,
}

impl Engine {
    pub fn new(config: Config, mut state: State) -> Self {
        if let Some(session) = &state.session {
            if !config.habits.contains_key(&session.habit) {
                state.session = None;
            }
        }
        // Unlocks from before unlock modes were plain wall-clock deadlines.
        let migrated = !state.unlocks.is_empty();
        for (group, until) in std::mem::take(&mut state.unlocks) {
            let unlock = Unlock { mode: UnlockMode::Wallclock, started_at: 0, until, granted_ms: 0, usage_left_ms: 0 };
            state.open_unlocks.entry(group).or_insert(unlock);
        }
        Self {
            config,
            state,
            windows: BTreeMap::new(),
            focused: None,
            idle: false,
            last_allowed: None,
            closing: BTreeSet::new(),
            warned: BTreeSet::new(),
            blocked_logged: BTreeMap::new(),
            schedule_blocking: BTreeSet::new(),
            schedule_seeded: false,
            schedule_warned: BTreeSet::new(),
            tabs: BTreeMap::new(),
            media: BTreeMap::new(),
            programs: BTreeMap::new(),
            update: None,
            tmux_sessions: BTreeMap::new(),
            extension_hosts: BTreeMap::new(),
            unverified_browsers: BTreeMap::new(),
            timer_clients: BTreeMap::new(),
            timer_window: None,
            usage_flushed_at: None,
            app_pending: BTreeMap::new(),
            usage_dirty: false,
            visit: None,
            afk_since: None,
            archive: Vec::new(),
            local_offset: Box::new(|_| 0),
            dirty: migrated,
        }
    }

    /// Sets how local time is derived (default: UTC). Used for day boundaries.
    pub fn with_local_offset(mut self, offset: impl Fn(u64) -> i64 + Send + 'static) -> Self {
        self.local_offset = Box::new(offset);
        self
    }

    /// Logical day number: days since the epoch in local time, where a day
    /// begins at `general.day_start`.
    pub fn day_of(&self, now: u64) -> i64 {
        self.local_shifted(now, self.config.general.day_start).div_euclid(DAY_MS)
    }

    fn local_shifted(&self, now: u64, shift: u64) -> i64 {
        now as i64 + (self.local_offset)(now) - shift as i64
    }

    /// Calendar day in local time (unaffected by `day_start`).
    pub fn civil_day_of(&self, now: u64) -> i64 {
        self.local_shifted(now, 0).div_euclid(DAY_MS)
    }

    /// Milliseconds since local midnight.
    pub fn time_of_day(&self, now: u64) -> u64 {
        self.local_shifted(now, 0).rem_euclid(DAY_MS) as u64
    }

    /// Local weekday, Monday = 0 (1970-01-01 was a Thursday).
    pub fn weekday_of(&self, now: u64) -> u8 {
        (self.civil_day_of(now) + 3).rem_euclid(7) as u8
    }

    /// When a logical day begins, in unix ms. The offset is resolved twice so
    /// a DST change between the guess and the boundary settles.
    pub fn day_start_ms(&self, day: i64) -> u64 {
        let local_midnight = day * DAY_MS + self.config.general.day_start as i64;
        let mut utc = local_midnight - (self.local_offset)(local_midnight.max(0) as u64);
        utc = local_midnight - (self.local_offset)(utc.max(0) as u64);
        utc.max(0) as u64
    }

    pub fn next_day_start(&self, now: u64) -> u64 {
        self.day_start_ms(self.day_of(now) + 1)
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// Stores habitd's liveness record (saved with the state, not marked dirty).
    pub fn set_heartbeat(&mut self, heartbeat: crate::lock::Heartbeat) {
        self.state.heartbeat = Some(heartbeat);
    }

    /// Whether persistent state changed since the last call.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub fn handle(&mut self, input: Input, now: u64) -> Vec<Effect> {
        // Charge the time since the last input to what was focused *before*
        // this input changes it.
        self.accrue_usage(now);
        let mut fx = Vec::new();
        match input {
            Input::WindowsReset(list) => {
                self.windows = list.into_iter().map(|w| (w.id, w)).collect();
                self.closing.retain(|id| self.windows.contains_key(id));
                self.programs.retain(|id, _| self.windows.contains_key(id));
                self.tmux_sessions.retain(|id, _| self.windows.contains_key(id));
                if self.focused.is_some_and(|id| !self.windows.contains_key(&id)) {
                    self.focused = None;
                }
                self.enforce_all_windows(now, &mut fx);
            }
            Input::WindowChanged(window) => {
                let id = window.id;
                self.windows.insert(id, window);
                self.enforce_window(id, now, &mut fx);
            }
            Input::WindowClosed(id) => {
                self.windows.remove(&id);
                self.closing.remove(&id);
                self.programs.remove(&id);
                self.tmux_sessions.remove(&id);
                if self.focused == Some(id) {
                    self.focused = None;
                }
                if self.last_allowed == Some(id) {
                    self.last_allowed = None;
                }
            }
            Input::FocusChanged(id) => {
                self.focused = id;
                self.enforce_strict(&mut fx);
            }
            Input::Idle(idle) => self.idle = idle,
            Input::TimerFocus { source, focused } => match focused {
                Some(focused) => {
                    let was_focused = self.timer_clients.get(&source).is_some_and(|(f, _)| *f);
                    if focused && !was_focused {
                        // The terminal reports focus after the compositor
                        // moved it, so the focused window is the timer's.
                        self.timer_window = self.focused;
                    }
                    self.timer_clients.insert(source, (focused, now));
                }
                None => {
                    self.timer_clients.remove(&source);
                }
            },
            Input::BrowserTab { source, window, tab } => match (window, tab) {
                (Some(window), Some(tab)) => {
                    self.tabs.insert((source, window), tab);
                }
                (Some(window), None) => {
                    self.tabs.remove(&(source, window));
                }
                (None, _) => {
                    self.tabs.retain(|(s, _), _| *s != source);
                    self.media.remove(&source);
                    self.extension_hosts.remove(&source);
                }
            },
            Input::BrowserMedia { source, urls } => {
                if urls.is_empty() {
                    self.media.remove(&source);
                } else {
                    self.media.insert(source, urls);
                }
            }
            Input::BrowserHello { source, pid } => {
                self.extension_hosts.insert(source, (pid, now));
            }
            Input::TerminalProgram { window, program, tmux_session } => {
                let known = self.windows.contains_key(&window);
                for (map, value) in [(&mut self.programs, program), (&mut self.tmux_sessions, tmux_session)] {
                    match value.filter(|_| known) {
                        Some(value) => map.insert(window, value),
                        None => map.remove(&window),
                    };
                }
            }
            Input::Tick => self.expire(now, &mut fx),
        }
        self.update_session(now, &mut fx);
        self.complete_passive(now, &mut fx);
        self.sync_afk(now);
        fx
    }

    // ---- commands -------------------------------------------------------

    /// Starts (or resumes today's saved progress of) a habit. A session of
    /// another habit is stopped first, keeping its progress.
    pub fn start(&mut self, habit_id: &str, now: u64) -> Result<(Vec<Effect>, String), String> {
        if !self.config.habits.contains_key(habit_id) {
            return Err(format!(
                "unknown habit {habit_id:?} (known: {})",
                self.config.habits.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if self.config.habits[habit_id].kind == HabitKind::Passive {
            return Err(format!(
                "{} counts by itself while its windows are focused; there's nothing to start",
                self.config.habit_name(habit_id)
            ));
        }
        if !self.config.habits[habit_id].kind.is_timed() {
            return Err(format!(
                "{} is logged with `hf done {habit_id}`, not started",
                self.config.habit_name(habit_id)
            ));
        }
        if self.state.session.as_ref().is_some_and(|s| s.habit == habit_id) {
            return Err(format!("{} is already running", self.config.habit_name(habit_id)));
        }
        let mut fx = Vec::new();
        let mut messages = Vec::new();
        if self.state.session.is_some() {
            let (stop_fx, message) = self.stop(now)?;
            fx.extend(stop_fx);
            messages.push(message);
        }
        let today = self.day_of(now);
        let resumed = match self.state.progress.remove(habit_id) {
            Some(saved) if saved.day == today => saved.ms,
            _ => 0,
        };
        self.state.session = Some(Session {
            habit: habit_id.to_string(),
            started_at: now,
            accumulated_ms: resumed,
            running_since: None,
            resumed_ms: resumed,
            extra_rounds: 0,
            awaiting: false,
        });
        self.last_allowed = None;
        self.dirty = true;
        let name = self.config.habit_name(habit_id);
        messages.push(if resumed > 0 {
            format!("Resumed {name} at {}", format_duration(resumed))
        } else {
            format!("Started {name}")
        });
        self.enforce_strict(&mut fx);
        self.update_session(now, &mut fx);
        let message = messages.join(". ");
        self.log_event(now, EventKind::SessionStarted, Some(habit_id), resumed, message.clone());
        Ok((fx, message))
    }

    /// Ends the session without reward, saving its progress for later today.
    /// Ends the session. Rewards of completed rounds stay banked; the
    /// unfinished part of the current round is saved for later today.
    pub fn stop(&mut self, now: u64) -> Result<(Vec<Effect>, String), String> {
        let mut fx = Vec::new();
        let Some(session) = self.settle(now, &mut fx)? else {
            return Ok((fx, "The session completed".into()));
        };
        let habit = self.config.habits[&session.habit].clone();
        let name = self.config.habit_name(&session.habit).to_string();
        let rounds = session.extra_rounds as u64;
        let remainder = session.accumulated_ms.saturating_sub(rounds * habit.target);
        if remainder > 0 {
            let day = self.day_of(now);
            self.state.progress.insert(session.habit.clone(), SavedProgress { ms: remainder, day });
        }
        let outcome = if rounds > 0 { Outcome::Completed } else { Outcome::Stopped };
        self.record_end(&session, now, outcome, false);
        let saved = format!("{} saved for today", format_duration(remainder));
        let message = match rounds {
            0 => format!("Stopped {name}, {saved}"),
            _ => format!(
                "Stopped {name} after {rounds} round(s), {} earned, {saved}",
                format_duration(rounds * habit.reward.duration)
            ),
        };
        self.log_event(now, EventKind::SessionStopped, Some(&session.habit), remainder, message.clone());
        Ok((fx, message))
    }

    /// Same as `stop`: rewards are banked when each round completes.
    pub fn finish(&mut self, now: u64) -> Result<(Vec<Effect>, String), String> {
        self.stop(now)
    }

    /// Dismisses the "round done" question and keeps going.
    pub fn continue_session(&mut self, now: u64) -> Result<(Vec<Effect>, String), String> {
        let mut fx = Vec::new();
        self.update_session(now, &mut fx);
        let Some(session) = self.state.session.as_mut() else {
            return Err("no session is running".into());
        };
        let habit = &self.config.habits[&session.habit];
        let name = self.config.habit_name(&session.habit).to_string();
        if !session.awaiting {
            let left = (habit.target * (session.extra_rounds as u64 + 1)).saturating_sub(session.accumulated_ms);
            return Err(format!("{name} is still in its round, {} left", format_duration(left)));
        }
        session.awaiting = false;
        self.dirty = true;
        Ok((
            fx,
            format!(
                "Round {} of {name}: {} more for another {}",
                session.extra_rounds + 1,
                format_duration((habit.target * (session.extra_rounds as u64 + 1)).saturating_sub(session.accumulated_ms)),
                format_duration(habit.reward.duration)
            ),
        ))
    }

    /// Ends the session without reward and discards its progress.
    pub fn abort(&mut self, emergency: bool, now: u64) -> Result<Vec<Effect>, String> {
        let mut fx = Vec::new();
        if let Some(session) = &self.state.session {
            let strict = self.config.habits[&session.habit].strictness == Strictness::Strict;
            if strict && !emergency {
                return Err(format!(
                    "{} is strict; stop it to keep the progress, or use an emergency abort (unlocks are refused for {} afterwards)",
                    self.config.habit_name(&session.habit),
                    format_duration(self.config.general.emergency_penalty)
                ));
            }
        }
        let Some(session) = self.settle(now, &mut fx)? else {
            return Ok(fx);
        };
        let strict = self.config.habits[&session.habit].strictness == Strictness::Strict;
        if emergency && strict {
            self.state.penalty_until = Some(now + self.config.general.emergency_penalty);
        }
        let outcome = if emergency { Outcome::EmergencyAborted } else { Outcome::Aborted };
        let name = self.config.habit_name(&session.habit).to_string();
        let discarded = session.accumulated_ms.saturating_sub(session.resumed_ms);
        self.record_end(&session, now, outcome, false);
        self.log_event(
            now,
            EventKind::SessionStopped,
            Some(&session.habit),
            discarded,
            format!("Aborted {name}, {} discarded", format_duration(discarded)),
        );
        if emergency && strict {
            let penalty = format_duration(self.config.general.emergency_penalty);
            self.log_event(now, EventKind::PenaltyStarted, None, self.config.general.emergency_penalty, format!("Unlocks refused for {penalty} after an emergency abort"));
        }
        Ok(fx)
    }

    /// Spends banked credit on a group. `duration: None` spends all of it.
    /// Returns the effects and the granted duration.
    pub fn unlock(
        &mut self,
        group: &str,
        duration: Option<u64>,
        now: u64,
    ) -> Result<(Vec<Effect>, u64), String> {
        self.accrue_usage(now);
        if !self.config.groups.contains_key(group) {
            return Err(format!("unknown group {group:?}"));
        }
        if !self.is_group_scheduled(group, now) {
            let name = self.config.group_name(group);
            let next = self.next_schedule_change(group, now);
            return Err(match next {
                Some(at) => format!("{name} isn't blocking right now; it blocks at {}", self.clock_label(at, now)),
                None => format!("{name} isn't blocking right now"),
            });
        }
        if !self.requirements_met(group, now) {
            return Err(self.requirement_message(group, now));
        }
        let penalty = self.penalty_remaining(now);
        if penalty > 0 {
            return Err(format!(
                "unlocks are refused for another {} after an emergency abort",
                format_duration(penalty)
            ));
        }
        let credit = self.state.credits.get(group).copied().unwrap_or(0);
        let name = self.config.group_name(group).to_string();
        let group_config = &self.config.groups[group];
        if credit == 0 {
            return Err(format!("no credit for {name}; complete a habit that rewards it first"));
        }
        let amount = if group_config.unlock_mode == UnlockMode::RestOfDay {
            let price = group_config.rest_of_day_price.unwrap_or(0);
            if duration.is_some() {
                return Err(format!(
                    "{name} unlocks for the rest of the day for {}; leave out the duration",
                    format_duration(price)
                ));
            }
            if self.is_group_unlocked(group, now) {
                return Err(format!("{name} is already unlocked for the rest of the day"));
            }
            if credit < price {
                return Err(format!(
                    "{name}'s day pass costs {}, you have {}",
                    format_duration(price),
                    format_duration(credit)
                ));
            }
            price
        } else {
            let amount = duration.unwrap_or(credit);
            if amount > credit {
                return Err(format!("only {} of credit available for {name}", format_duration(credit)));
            }
            amount
        };
        if amount == credit {
            self.state.credits.remove(group);
        } else {
            self.state.credits.insert(group.to_string(), credit - amount);
        }
        self.extend_unlock(group, amount, now);
        let text = match self.config.groups[group].unlock_mode {
            UnlockMode::Wallclock => format!("{name} unlocked for {}", format_duration(amount)),
            UnlockMode::Usage => format!("{name} unlocked for {} of use", format_duration(amount)),
            UnlockMode::RestOfDay => format!(
                "{name} unlocked until {} for {}",
                self.clock_label(self.next_day_start(now), now),
                format_duration(amount)
            ),
        };
        self.log_event(now, EventKind::Unlocked, Some(group), amount, text);
        Ok((Vec::new(), amount))
    }

    /// Ends an unlock early, refunding the unused time as credit.
    pub fn relock(&mut self, group: &str, now: u64) -> Result<Vec<Effect>, String> {
        self.accrue_usage(now);
        if !self.config.groups.contains_key(group) {
            return Err(format!("unknown group {group:?}"));
        }
        if !self.is_group_unlocked(group, now) {
            return Err(format!("{} is not unlocked", self.config.group_name(group)));
        }
        let unlock = self.state.open_unlocks.remove(group).expect("unlocked");
        let refund = match unlock.mode {
            UnlockMode::Wallclock => unlock.until.saturating_sub(now),
            UnlockMode::Usage => unlock.usage_left_ms,
            // A day pass is used the moment it's bought.
            UnlockMode::RestOfDay => 0,
        };
        if refund > 0 {
            *self.state.credits.entry(group.to_string()).or_default() += refund;
        }
        self.warned.remove(group);
        self.dirty = true;
        let name = self.config.group_name(group);
        let text = match unlock.mode {
            UnlockMode::RestOfDay => format!("{name} locked again (a day pass isn't refunded)"),
            _ => format!("{name} locked again, {} refunded", format_duration(refund)),
        };
        self.log_event(now, EventKind::Relocked, Some(group), refund, text);
        let mut fx = Vec::new();
        self.enforce_all_windows(now, &mut fx);
        Ok(fx)
    }

    // ---- commitment lock --------------------------------------------------

    pub fn is_locked(&self, now: u64) -> bool {
        self.state.lock.as_ref().is_some_and(|lock| now < lock.ends_at())
    }

    /// Config text in force for an active lock.
    pub fn lock_baseline(&self, now: u64) -> Option<&str> {
        self.state.lock.as_ref().filter(|_| self.is_locked(now)).map(|lock| lock.baseline.as_str())
    }

    /// Records what the config file would weaken (empty: nothing pending) and
    /// the accepted baseline.
    pub fn set_lock_resolution(&mut self, baseline: String, pending: Vec<String>) {
        if let Some(lock) = self.state.lock.as_mut() {
            if lock.baseline != baseline || lock.pending != pending {
                lock.baseline = baseline;
                lock.pending = pending;
                self.dirty = true;
            }
        }
    }

    /// Starts a commitment until `until`, or extends the current one.
    /// `config_text` is the config currently in force.
    pub fn lock(&mut self, until: u64, config_text: &str, now: u64) -> Result<String, String> {
        if until <= now {
            return Err("the lock must end in the future".into());
        }
        if self.is_locked(now) {
            let lock = self.state.lock.as_mut().expect("locked");
            if until <= lock.until {
                return Err(format!(
                    "a commitment can only be extended; it runs for another {}",
                    format_duration_long(lock.until - now)
                ));
            }
            lock.until = until;
            self.dirty = true;
            let message = format!("Commitment extended to {} from now", format_duration_long(until - now));
            self.log_event(now, EventKind::LockExtended, None, until - now, message.clone());
            return Ok(message);
        }
        self.state.lock = Some(Lock {
            until,
            baseline: config_text.to_string(),
            end_requested_at: None,
            pending: Vec::new(),
            extended_ms: 0,
        });
        self.unverified_browsers.clear();
        self.dirty = true;
        let message = format!("Committed for {}", format_duration_long(until - now));
        self.log_event(now, EventKind::LockStarted, None, until - now, message.clone());
        Ok(message)
    }

    /// Asks to end the lock early; it ends `END_DELAY_MS` later.
    pub fn request_lock_end(&mut self, now: u64) -> Result<String, String> {
        if !self.is_locked(now) {
            return Err("no commitment is active".into());
        }
        let lock = self.state.lock.as_mut().expect("locked");
        if lock.end_requested_at.is_some() {
            return Err(format!("the commitment already ends in {}", format_duration_long(lock.ends_at() - now)));
        }
        if lock.until.saturating_sub(now) <= END_DELAY_MS {
            return Err(format!("the commitment ends in {} anyway", format_duration_long(lock.until - now)));
        }
        lock.end_requested_at = Some(now);
        self.dirty = true;
        self.log_event(
            now,
            EventKind::LockEndRequested,
            None,
            END_DELAY_MS,
            format!("Early end requested; the commitment ends in {}", format_duration_long(END_DELAY_MS)),
        );
        Ok(format!(
            "The commitment ends in {}. Changed your mind? `hf lock cancel-end`",
            format_duration_long(END_DELAY_MS)
        ))
    }

    pub fn cancel_lock_end(&mut self, now: u64) -> Result<String, String> {
        let lock = self.state.lock.as_mut().filter(|l| now < l.ends_at() && l.end_requested_at.is_some());
        let Some(lock) = lock else {
            return Err("no early end was requested".into());
        };
        lock.end_requested_at = None;
        let remaining = lock.until - now;
        self.dirty = true;
        let message = format!("Early end cancelled; the commitment runs for another {}", format_duration_long(remaining));
        self.log_event(now, EventKind::LockExtended, None, remaining, message.clone());
        Ok(message)
    }

    /// Penalizes habitd downtime that wasn't a reboot, suspend or shutdown:
    /// a lock active before the downtime is extended by it.
    pub fn apply_downtime(&mut self, down_since: u64, downtime: u64) -> Vec<Effect> {
        let mut fx = Vec::new();
        let Some(lock) = self.state.lock.as_mut().filter(|l| l.ends_at() > down_since) else {
            return fx;
        };
        lock.until += downtime;
        if let Some(requested) = lock.end_requested_at.as_mut() {
            *requested += downtime;
        }
        lock.extended_ms += downtime;
        self.dirty = true;
        self.log_and_notify(
            &mut fx,
            down_since,
            EventKind::LockExtended,
            None,
            downtime,
            format!(
                "habitd was stopped for {} during your commitment, so it runs that much longer.",
                format_duration_long(downtime)
            ),
        );
        fx
    }

    pub fn reload(&mut self, config: Config, now: u64) -> Vec<Effect> {
        self.accrue_usage(now);
        self.config = config;
        self.schedule_seeded = false;
        self.log_event(now, EventKind::ConfigReloaded, None, 0, "Config reloaded".into());
        if let Some(session) = &self.state.session {
            if !self.config.habits.contains_key(&session.habit) {
                self.state.session = None;
                self.dirty = true;
            }
        }
        let mut fx = Vec::new();
        self.enforce_all_windows(now, &mut fx);
        self.enforce_strict(&mut fx);
        self.update_session(now, &mut fx);
        fx
    }

    // ---- queries --------------------------------------------------------

    /// A paid unlock is open for the group.
    pub fn is_group_unlocked(&self, group: &str, now: u64) -> bool {
        self.state
            .open_unlocks
            .get(group)
            .is_some_and(|u| now < u.until && (u.mode != UnlockMode::Usage || u.usage_left_ms > 0))
    }

    /// What's left of an open unlock: clock time, or budget of use.
    fn unlock_remaining(&self, group: &str, now: u64) -> u64 {
        match self.state.open_unlocks.get(group) {
            Some(u) if self.is_group_unlocked(group, now) => match u.mode {
                UnlockMode::Usage => u.usage_left_ms,
                _ => u.until.saturating_sub(now),
            },
            _ => 0,
        }
    }

    fn unlock_label(&self, group: &str, now: u64) -> Option<String> {
        let unlock = self.state.open_unlocks.get(group).filter(|_| self.is_group_unlocked(group, now))?;
        Some(match unlock.mode {
            UnlockMode::Wallclock => format!("{} left", format_duration(unlock.until.saturating_sub(now))),
            UnlockMode::Usage => format!("{} of use left", format_duration(unlock.usage_left_ms)),
            UnlockMode::RestOfDay => format!("until {}", self.clock_label(unlock.until, now)),
        })
    }

    fn minute_of_week(&self, now: u64) -> usize {
        self.weekday_of(now) as usize * 1440 + (self.time_of_day(now) / 60_000) as usize
    }

    /// The group's schedule says it blocks now (always true without one).
    pub fn is_group_scheduled(&self, group: &str, now: u64) -> bool {
        self.config.groups.get(group).is_some_and(|g| g.blocks_at(self.minute_of_week(now)))
    }

    /// The group blocks right now: inside its schedule and not unlocked. Every
    /// blocking decision and `GroupView.blocked` go through this.
    pub fn is_group_blocking(&self, group: &str, now: u64) -> bool {
        self.is_group_scheduled(group, now) && !self.is_group_unlocked(group, now)
    }

    /// Terminals are never blocked, whatever the config says: you'd have no
    /// way left to run `hf` and unlock.
    pub fn is_app_blocked(&self, app_id: &str, now: u64) -> bool {
        !self.config.is_protected_app(app_id)
            && self.config.groups.iter().any(|(id, g)| g.matches_app(app_id) && self.is_group_blocking(id, now))
    }

    /// Neither are habitfocus, shells or the desktop (`PROTECTED_PROCESSES`).
    pub fn is_process_blocked(&self, comm: &str, now: u64) -> bool {
        !Config::is_protected_process(comm)
            && self.config.groups.iter().any(|(id, g)| g.matches_process(comm) && self.is_group_blocking(id, now))
    }

    pub fn blocked_domains(&self, now: u64) -> BTreeSet<String> {
        self.config
            .groups
            .iter()
            .filter(|(id, _)| self.is_group_blocking(id, now))
            .flat_map(|(_, g)| g.domains.iter().cloned())
            .collect()
    }

    /// When the group's schedule next flips between blocking and off hours.
    pub fn next_schedule_change(&self, group: &str, now: u64) -> Option<u64> {
        let g = self.config.groups.get(group).filter(|g| g.has_schedule())?;
        let minute = self.minute_of_week(now);
        let current = g.blocks_at(minute);
        let minute_start = now - self.time_of_day(now) % 60_000;
        (1..=MINUTES_PER_WEEK)
            .find(|k| g.blocks_at(minute + k) != current)
            .map(|k| minute_start + k as u64 * 60_000)
    }

    /// "22:00" for later today, "tomorrow 02:00", or "tue 02:00" further out.
    fn clock_label(&self, at: u64, now: u64) -> String {
        let time = crate::duration::format_time_of_day(self.time_of_day(at));
        match self.civil_day_of(at) - self.civil_day_of(now) {
            0 => time,
            1 => format!("tomorrow {time}"),
            _ => format!("{} {time}", Weekday::NAMES[self.weekday_of(at) as usize]),
        }
    }

    fn schedule_label(&self, group: &str, now: u64) -> Option<String> {
        let g = self.config.groups.get(group).filter(|g| g.has_schedule())?;
        let blocking = g.blocks_at(self.minute_of_week(now));
        Some(match (blocking, self.next_schedule_change(group, now)) {
            (true, Some(at)) => format!("until {}", self.clock_label(at, now)),
            (false, Some(at)) => format!("off hours until {}", self.clock_label(at, now)),
            (true, None) => "all week".into(),
            (false, None) => "never blocking".into(),
        })
    }

    /// URL of the active tab shown in a compositor window. Browsers title their
    /// windows "<tab title> — <browser>", which links extension tabs to windows.
    pub fn window_url(&self, window: &WindowInfo) -> Option<&str> {
        self.tabs
            .values()
            .filter(|tab| !tab.title.is_empty() && window.title.starts_with(&tab.title))
            .max_by_key(|tab| tab.title.len())
            .map(|tab| tab.url.as_str())
    }

    pub fn has_process_rules(&self) -> bool {
        self.config.groups.values().any(|g| !g.processes.is_empty())
    }

    /// Builds daily stats from the session history once, for state files from
    /// before daily stats existed. Call after `with_local_offset`.
    pub fn backfill_day_stats(&mut self) {
        if !self.state.days.is_empty() || self.state.history.is_empty() {
            return;
        }
        let entries: Vec<(i64, String, u64, bool)> = self
            .state
            .history
            .iter()
            .map(|h| (self.day_of(h.finished_at), h.habit.clone(), h.focused_ms, h.outcome == Outcome::Completed))
            .collect();
        for (day, habit, focused_ms, completed) in entries {
            stats::record(&mut self.state.days, day, &habit, focused_ms, completed);
        }
        self.dirty = true;
    }

    /// Daily stats for the last `count` logical days, oldest first.
    pub fn day_stats(&self, count: u32, now: u64) -> Vec<DayView> {
        let today = self.day_of(now);
        (today - i64::from(count.max(1)) + 1..=today)
            .map(|day| DayView {
                day,
                date: stats::civil_date(day),
                habits: self.state.days.get(&day).cloned().unwrap_or_default(),
            })
            .collect()
    }

    /// What the focused window counts as for screen time: its app id,
    /// `site:<host>` for a browser window whose active tab is known, or
    /// `term:<program>` for a terminal running a program.
    fn usage_key(&self) -> Option<String> {
        let window = self.focused.and_then(|id| self.windows.get(&id))?;
        let program = self.focused_terminal().and_then(|w| self.programs.get(&w.id));
        if let Some(program) = program.filter(|_| self.config.general.terminal_programs) {
            return Some(format!("{TERMINAL_PREFIX}{program}"));
        }
        let site = self.window_url(window).and_then(crate::config::url_host);
        Some(site.map_or_else(|| window.app_id.clone(), |host| format!("site:{host}")))
    }

    /// The focused window, when it's a terminal with a known pid and what's
    /// in front matters: for screen time (`terminal_programs`) or for
    /// `program` and `tmux_session` allow rules. The daemon looks up what runs in it and reports it as
    /// `Input::TerminalProgram`.
    pub fn focused_terminal(&self) -> Option<&WindowInfo> {
        if !self.config.general.terminal_programs && !self.config.has_terminal_rules() {
            return None;
        }
        let window = self.focused.and_then(|id| self.windows.get(&id))?;
        (window.pid.is_some() && self.config.is_terminal(&window.app_id)).then_some(window)
    }

    /// What the daemon last reported in front in window `id`.
    fn terminal_front(&self, id: u64) -> TerminalFront<'_> {
        TerminalFront {
            program: self.programs.get(&id).map(String::as_str),
            tmux_session: self.tmux_sessions.get(&id).map(String::as_str),
        }
    }

    /// What habitd's release check found: a newer version, or `None`.
    pub fn set_available_update(&mut self, update: Option<UpdateView>) {
        self.update = update;
    }

    /// Every screen-time key recorded in the kept days, plus the current one.
    fn usage_keys(&self) -> BTreeSet<&str> {
        self.state
            .app_days
            .values()
            .flat_map(|apps| apps.keys())
            .chain(self.app_pending.keys())
            .chain(self.visit.iter().map(|v| &v.key))
            .map(String::as_str)
            .collect()
    }

    /// `[app_names]` plus the built-in names of the keys in use.
    fn app_names_view(&self) -> BTreeMap<String, String> {
        let mut names = self.config.app_names.clone();
        for key in self.usage_keys() {
            if let Some(name) = self.config.default_app_name(key) {
                names.entry(key.to_string()).or_insert_with(|| name.to_string());
            }
        }
        names
    }

    /// `[app_categories]` plus the automatic categories of the keys in use.
    fn app_categories_view(&self) -> BTreeMap<String, String> {
        let mut categories = self.config.app_categories.clone();
        for key in self.usage_keys() {
            if let Some(category) = self.config.default_app_category(key) {
                categories.entry(key.to_string()).or_insert_with(|| category.to_string());
            }
        }
        categories
    }

    /// The habit whose session is counting right now, if any.
    fn counting_habit(&self) -> Option<String> {
        self.state.session.as_ref().filter(|s| s.running_since.is_some()).map(|s| s.habit.clone())
    }

    /// Without input, but not away: a habit is counting that keeps counting
    /// without input (a timer, e.g. reading a paper book next to `hf tui`).
    fn idle_in_habit(&self) -> bool {
        self.running_habit().is_some_and(|h| h.kind == HabitKind::Timer) && self.counting_habit().is_some()
    }

    /// Away from the computer: idle, and no habit is counting through it.
    fn away(&self) -> bool {
        self.idle && !self.idle_in_habit()
    }

    /// Starts or archives the stretch away, after the input or session
    /// change that started or ended it.
    fn sync_afk(&mut self, now: u64) {
        if !self.away() {
            self.end_afk(now);
        } else if self.afk_since.is_none() {
            self.afk_since = Some(now);
        }
    }

    /// Accrues the time since the last call to the focused window, unless away.
    fn accrue_usage(&mut self, now: u64) {
        let Some(previous) = self.usage_flushed_at.replace(now) else {
            return;
        };
        // Time habitd didn't see is a suspend; while away, the stretch away
        // already covers it.
        if now.saturating_sub(previous) >= MIN_ASLEEP_MS && self.afk_since.is_none() {
            self.queue(Record::Afk(Afk { start: previous, end: now, reason: AfkReason::Asleep }));
        }
        let delta = now.saturating_sub(previous).min(MAX_USAGE_STEP_MS);
        if delta == 0 {
            return;
        }
        let key = if self.away() { None } else { self.usage_key() };
        // A visit continues while the same key stays focused without a gap
        // (away, a clamped jump) and counts towards the same habit; anything
        // else ends it.
        let habit = self.counting_habit();
        let jumped = now.saturating_sub(previous) > MAX_USAGE_STEP_MS;
        let continues = self
            .visit
            .as_ref()
            .is_some_and(|v| !jumped && Some(&v.key) == key.as_ref() && v.end == previous && v.habit == habit);
        if !continues {
            self.end_visit();
        }
        // A usage unlock also runs down while its video or music plays
        // unfocused or without input, so this doesn't depend on `key`.
        let burning: Vec<String> = self
            .state
            .open_unlocks
            .keys()
            .filter(|group| self.usage_burning(group, now))
            .cloned()
            .collect();
        for group in burning {
            if let Some(unlock) = self.state.open_unlocks.get_mut(&group) {
                unlock.usage_left_ms = unlock.usage_left_ms.saturating_sub(delta);
                self.usage_dirty = true;
            }
        }
        let Some(key) = key else { return };
        self.accrue_passive(delta, now);
        match &mut self.visit {
            Some(visit) => {
                visit.end = now;
                visit.active_ms += delta;
            }
            None => {
                self.visit = Some(Visit { key: key.clone(), start: now - delta, end: now, active_ms: delta, habit })
            }
        }
        *self.app_pending.entry(key).or_default() += delta;
    }

    /// Adds `delta` of focus to every passive habit the focused window counts
    /// for. Saved on the screen-time cadence; `complete_passive` pays goals.
    fn accrue_passive(&mut self, delta: u64, now: u64) {
        let Some(id) = self.focused else { return };
        let Some(window) = self.windows.get(&id) else { return };
        let (url, front) = (self.window_url(window), self.terminal_front(id));
        let counting: Vec<String> = self
            .config
            .habits
            .iter()
            .filter(|(_, h)| h.kind == HabitKind::Passive && h.allows(&window.app_id, &window.title, url, front))
            .map(|(id, _)| id.clone())
            .collect();
        let day = self.day_of(now);
        for habit in counting {
            stats::record(&mut self.state.days, day, &habit, delta, false);
            self.usage_dirty = true;
        }
    }

    /// Completes passive habits whose time today reached another full
    /// `target` (at most `rounds_per_day`), paying their reward if they have one.
    fn complete_passive(&mut self, now: u64, fx: &mut Vec<Effect>) {
        let day = self.day_of(now);
        let due: Vec<(String, Habit, u32)> = self
            .config
            .habits
            .iter()
            .filter(|(_, h)| h.kind == HabitKind::Passive && h.target > 0)
            .filter_map(|(id, h)| {
                let stats = self.state.days.get(&day)?.get(id)?;
                let rounds = u32::try_from(stats.focused_ms / h.target).unwrap_or(u32::MAX);
                let rounds = rounds.min(h.rounds_per_day().unwrap_or(u32::MAX));
                (rounds > stats.completions).then(|| (id.clone(), h.clone(), rounds - stats.completions))
            })
            .collect();
        for (id, habit, new_rounds) in due {
            let name = self.config.habit_name(&id).to_string();
            for _ in 0..new_rounds {
                stats::record(&mut self.state.days, day, &id, 0, true);
                self.dirty = true;
                let focused = self.state.days[&day][&id].focused_ms;
                let mut text = format!("{name}: {} today, goal reached.", format_duration(focused));
                if habit.has_reward() {
                    text += &format!(" {}.", self.grant_reward(&habit, now));
                }
                let streak = stats::habit_streak(&self.state.days, day, &id).current;
                if streak >= 2 {
                    text += &format!(" {streak}-day streak.");
                }
                self.play_sound(fx);
                self.log_and_notify(fx, now, EventKind::HabitCompleted, Some(&id), habit.reward.duration, text);
            }
        }
    }

    /// The focused window is an app or site of `group`.
    fn focused_in_group(&self, group: &Group) -> bool {
        let Some(window) = self.focused.and_then(|id| self.windows.get(&id)) else {
            return false;
        };
        group.matches_app(&window.app_id)
            || self
                .window_url(window)
                .and_then(crate::config::url_host)
                .is_some_and(|host| group.matches_domain(&host))
    }

    /// A tab on one of `group`'s sites plays sound, focused or not.
    fn playing_in_group(&self, group: &Group) -> bool {
        self.media
            .values()
            .flatten()
            .filter_map(|url| crate::config::url_host(url))
            .any(|host| group.matches_domain(&host))
    }

    /// A usage-based unlock of `group` is being used right now: it's open, the
    /// group is inside its schedule (off hours it's free anyway), and either
    /// one of its apps or sites is focused while the user isn't idle, or one
    /// of its sites plays a video or music (watched in the background, on
    /// another monitor or without touching the mouse).
    fn usage_burning(&self, group: &str, now: u64) -> bool {
        let Some(unlock) = self.state.open_unlocks.get(group) else { return false };
        let Some(g) = self.config.groups.get(group) else { return false };
        unlock.mode == UnlockMode::Usage
            && self.is_group_unlocked(group, now)
            && self.is_group_scheduled(group, now)
            && ((!self.idle && self.focused_in_group(g)) || self.playing_in_group(g))
    }

    /// Moves pending screen time into the persisted per-day map and prunes
    /// old days.
    fn fold_usage(&mut self, now: u64) {
        let today = self.day_of(now);
        for (app, ms) in std::mem::take(&mut self.app_pending) {
            stats::record_app(&mut self.state.app_days, today, &app, ms);
            self.usage_dirty = true;
        }
        let keep = i64::from(self.config.general.screen_time_days.max(1));
        if stats::prune_before(&mut self.state.app_days, today - keep + 1) {
            self.usage_dirty = true;
        }
    }

    /// Folds screen time into the state before a final save.
    pub fn flush_usage(&mut self, now: u64) {
        self.accrue_usage(now);
        self.fold_usage(now);
        self.end_visit();
        self.end_afk(now);
    }

    /// Whether screen time changed since the last call; the daemon saves for
    /// it on a slower cadence than for other state.
    pub fn take_usage_dirty(&mut self) -> bool {
        std::mem::take(&mut self.usage_dirty)
    }

    /// Screen time per app over the last `days` logical days, most used first.
    pub fn app_usage(&self, days: u32, now: u64) -> Vec<AppUsageView> {
        let today = self.day_of(now);
        let first = today - i64::from(days.max(1)) + 1;
        let period = (today - first + 1) as usize;
        let mut totals: BTreeMap<&str, (u64, u64, Vec<u64>)> = BTreeMap::new();
        for (&day, apps) in self.state.app_days.range(first..=today) {
            for (app, &ms) in apps {
                let entry = totals.entry(app).or_insert_with(|| (0, 0, vec![0; period]));
                entry.1 += ms;
                entry.2[(day - first) as usize] += ms;
                if day == today {
                    entry.0 += ms;
                }
            }
        }
        for (app, &ms) in &self.app_pending {
            let entry = totals.entry(app).or_insert_with(|| (0, 0, vec![0; period]));
            entry.0 += ms;
            entry.1 += ms;
            entry.2[period - 1] += ms;
        }
        let mut usage: Vec<AppUsageView> = totals
            .into_iter()
            .map(|(app, (today_ms, total_ms, days_ms))| AppUsageView {
                app: app.to_string(),
                today_ms,
                total_ms,
                sessions: 0,
                sessions_today: 0,
                avg_session_ms: 0,
                longest_session_ms: 0,
                hours: Vec::new(),
                hours_today: Vec::new(),
                days_ms,
                days_sessions: Vec::new(),
            })
            .collect();
        usage.sort_by(|a, b| b.total_ms.cmp(&a.total_ms).then_with(|| a.app.cmp(&b.app)));
        usage
    }

    /// Whether any screen time was recorded under `key` (an app id or
    /// `site:<host>`), e.g. to warn about a mistyped id.
    pub fn has_usage(&self, key: &str) -> bool {
        self.app_pending.contains_key(key) || self.state.app_days.values().any(|apps| apps.contains_key(key))
    }

    /// How the hours between `from` and `to` were spent, by habit, category
    /// or app. `visits` come from the archive, plus the one in progress.
    pub fn hour_slices(&self, visits: &[Visit], from: u64, to: u64) -> Vec<HourSlice> {
        let mut hours: BTreeMap<(u8, String), (String, u64, bool)> = BTreeMap::new();
        for visit in visits.iter().chain(self.visit.as_ref()).filter(|v| v.end > from && v.start < to) {
            let (label, is_habit) = match &visit.habit {
                Some(habit) => (self.config.habit_name(habit).to_string(), true),
                None => match self.config.app_category(&visit.key) {
                    Some(category) => (category.to_string(), false),
                    None => (display_key(self.config.app_name(&visit.key)).to_string(), false),
                },
            };
            let wall = visit.end - visit.start;
            let mut t = visit.start.max(from);
            while t < visit.end.min(to) {
                let into_hour = self.time_of_day(t) % HOUR_MS;
                let next = (t + HOUR_MS - into_hour).min(visit.end.min(to));
                let hour = (self.time_of_day(t) / HOUR_MS).min(23) as u8;
                let share = (next - t) * visit.active_ms / wall.max(1);
                let entry = hours
                    .entry((hour, format!("{label}\u{1}{}", visit.key)))
                    .or_insert((label.clone(), 0, is_habit));
                entry.1 += share;
                t = next;
            }
        }
        hours
            .into_iter()
            .filter(|(_, (_, ms, _))| *ms > 0)
            .map(|((hour, joined), (label, ms, habit))| HourSlice {
                hour,
                label,
                key: joined.split_once('\u{1}').map_or(String::new(), |(_, key)| key.to_string()),
                ms,
                habit,
            })
            .collect()
    }

    /// One logical day of hours, `offset` days back from today.
    pub fn day_breakdown(&self, visits: &[Visit], offset: u32, now: u64, more_before: bool) -> Breakdown {
        let day = self.day_of(now) - i64::from(offset);
        let (from, to) = (self.day_start_ms(day), self.day_start_ms(day + 1).min(now));
        Breakdown { slices: self.hour_slices(visits, from, to), date: stats::civil_date(day), offset, more_before }
    }

    /// The visits and time away of one logical day, `offset` days back from
    /// today, cut to the day and labelled. `visits` and `afk` come from the
    /// archive, plus the ones in progress.
    pub fn day_timeline(&self, visits: &[Visit], afk: &[Afk], offset: u32, now: u64, more_before: bool) -> Timeline {
        let day = self.day_of(now) - i64::from(offset);
        let (from, to) = (self.day_start_ms(day), self.day_start_ms(day + 1));
        let visits = visits
            .iter()
            .chain(self.visit.as_ref())
            .filter(|v| v.end > from && v.start < to)
            .map(|v| {
                let (start, end) = (v.start.max(from), v.end.min(to));
                let wall = (v.end - v.start).max(1);
                TimelineVisit {
                    key: v.key.clone(),
                    name: display_key(self.config.app_name(&v.key)).to_string(),
                    category: self.config.app_category(&v.key).map(str::to_string),
                    habit: v.habit.as_deref().map(|h| self.config.habit_name(h).to_string()),
                    start_ms: start,
                    end_ms: end,
                    active_ms: v.active_ms * (end - start) / wall,
                }
            })
            .collect();
        let idle_now = self.afk_since.map(|start| Afk { start, end: now, reason: AfkReason::Idle });
        let afk = afk
            .iter()
            .chain(idle_now.as_ref())
            .filter(|a| a.end > from && a.start < to)
            .map(|a| TimelineAfk { start_ms: a.start.max(from), end_ms: a.end.min(to), reason: a.reason })
            .collect();
        Timeline { date: stats::civil_date(day), offset, from_ms: from, to_ms: to, more_before, visits, afk }
    }

    /// The hours of the whole `app_usage(days)` period.
    pub fn period_breakdown(&self, visits: &[Visit], days: u32, now: u64) -> Breakdown {
        let from = self.usage_period_start(days, now);
        Breakdown {
            slices: self.hour_slices(visits, from, now),
            date: stats::civil_date(self.day_of(now) - i64::from(days.max(1)) + 1),
            offset: 0,
            more_before: false,
        }
    }

    /// First moment of the period `app_usage(days)` covers.
    pub fn usage_period_start(&self, days: u32, now: u64) -> u64 {
        self.day_start_ms(self.day_of(now) - i64::from(days.max(1)) + 1)
    }

    /// `app_usage` plus what only visits tell: sessions, their length and the
    /// time of day. `visits` come from the archive (oldest first) and cover
    /// the period; the visit in progress is added here.
    pub fn app_insights(&self, days: u32, visits: &[Visit], now: u64) -> Vec<AppUsageView> {
        #[derive(Default)]
        struct Acc {
            sessions: u32,
            sessions_today: u32,
            active_ms: u64,
            longest_ms: u64,
            session_ms: u64,
            last_end: Option<u64>,
            hours: [u64; 24],
            hours_today: [u64; 24],
            days_sessions: Vec<u32>,
        }
        let from = self.usage_period_start(days, now);
        let today = self.day_of(now);
        let first = today - i64::from(days.max(1)) + 1;
        let period = (today - first + 1) as usize;
        let mut keys: BTreeMap<&str, Acc> = BTreeMap::new();
        for visit in visits.iter().chain(self.visit.as_ref()).filter(|v| v.end > from && v.start < now) {
            let acc = keys.entry(&visit.key).or_default();
            if acc.last_end.is_some_and(|end| visit.start.saturating_sub(end) < SESSION_GAP_MS) {
                acc.session_ms += visit.active_ms;
            } else {
                acc.longest_ms = acc.longest_ms.max(acc.session_ms);
                acc.session_ms = visit.active_ms;
                acc.sessions += 1;
                let day = self.day_of(visit.start);
                if day == today {
                    acc.sessions_today += 1;
                }
                acc.days_sessions.resize(period, 0);
                // A session from before the period counts on its first day.
                acc.days_sessions[((day.max(first) - first) as usize).min(period - 1)] += 1;
            }
            acc.active_ms += visit.active_ms;
            acc.last_end = Some(visit.end);
            // Spread the visit over the local hours it covers.
            let wall = visit.end - visit.start;
            let mut t = visit.start.max(from);
            while t < visit.end.min(now) {
                let into_hour = self.time_of_day(t) % HOUR_MS;
                let next = (t + HOUR_MS - into_hour).min(visit.end);
                let hour = (self.time_of_day(t) / HOUR_MS) as usize;
                let share = (next - t) * visit.active_ms / wall.max(1);
                acc.hours[hour.min(23)] += share;
                if self.day_of(t) == today {
                    acc.hours_today[hour.min(23)] += share;
                }
                t = next;
            }
        }
        let mut usage = self.app_usage(days, now);
        for view in &mut usage {
            let Some(acc) = keys.get(view.app.as_str()) else { continue };
            view.sessions = acc.sessions;
            view.sessions_today = acc.sessions_today;
            view.avg_session_ms = acc.active_ms / u64::from(acc.sessions.max(1));
            view.longest_session_ms = acc.longest_ms.max(acc.session_ms);
            view.hours = acc.hours.to_vec();
            view.hours_today = acc.hours_today.to_vec();
            view.days_sessions = acc.days_sessions.clone();
        }
        usage
    }

    /// Records that the daemon started (the first line of the activity log
    /// after a restart).
    pub fn log_startup(&mut self, now: u64) {
        self.log_event(now, EventKind::DaemonStarted, None, 0, "habitd started".into());
    }

    /// Most recent events, newest first.
    pub fn events(&self, limit: usize) -> Vec<Event> {
        self.state.events.iter().rev().take(limit).cloned().collect()
    }

    /// Most recent history entries, newest first.
    pub fn history(&self, limit: usize) -> Vec<HistoryEntry> {
        self.state.history.iter().rev().take(limit).cloned().collect()
    }

    pub fn penalty_remaining(&self, now: u64) -> u64 {
        self.state.penalty_until.map_or(0, |until| until.saturating_sub(now))
    }

    /// Whether the snapshot changes with time alone (clients need ticks).
    pub fn is_time_sensitive(&self, now: u64) -> bool {
        self.state.session.is_some()
            || self.is_locked(now)
            || self
                .state
                .open_unlocks
                .iter()
                .any(|(group, u)| u.mode != UnlockMode::Usage || self.usage_burning(group, now))
            || self.penalty_remaining(now) > 0
    }

    pub fn snapshot(&self, now: u64) -> Snapshot {
        let session = self.state.session.as_ref().map(|s| {
            let habit = &self.config.habits[&s.habit];
            let rounds = s.extra_rounds as u64;
            let total = s.elapsed(now);
            // The clock restarts every round; ticks bank rounds within a second.
            let elapsed = total.saturating_sub(rounds * habit.target).min(habit.target);
            let running = s.running_since.is_some();
            SessionView {
                habit: s.habit.clone(),
                name: self.config.habit_name(&s.habit).to_string(),
                running,
                pause_reason: match (running, self.idle && habit.kind == HabitKind::Apps) {
                    (true, _) => None,
                    (false, true) => Some(PauseReason::Idle),
                    (false, false) => Some(PauseReason::Unfocused),
                },
                elapsed_ms: elapsed,
                target_ms: habit.target,
                remaining_ms: habit.target - elapsed,
                progress: elapsed as f64 / habit.target as f64,
                strictness: habit.strictness,
                kind: habit.kind,
                habit_target_ms: habit.target,
                awaiting_decision: s.awaiting,
                rounds_completed: s.extra_rounds,
                banked_ms: rounds * habit.reward.duration,
                total_elapsed_ms: total,
                resumed_ms: s.resumed_ms,
            }
        });
        let today = self.day_of(now);
        let today_stats = self.state.days.get(&today);
        let habits = self
            .config
            .habits
            .iter()
            .map(|(id, h)| HabitView {
                id: id.clone(),
                name: self.config.habit_name(id).to_string(),
                target_ms: h.target,
                strictness: h.strictness,
                reward_groups: h.reward.groups.clone(),
                reward_ms: h.reward.duration,
                reward_mode: h.reward.mode,
                kind: h.kind,
                on_target: h.on_target(),
                saved_ms: self
                    .state
                    .progress
                    .get(id)
                    .filter(|p| p.day == today)
                    .map_or(0, |p| p.ms),
                streak_days: stats::habit_streak(&self.state.days, today, id).current,
                best_streak_days: stats::habit_streak(&self.state.days, today, id).best,
                done_today: today_stats.and_then(|d| d.get(id)).is_some_and(|s| s.completions > 0),
                today_focused_ms: self.today_focused(id, now),
                goal: h.goal(),
                unit: if h.kind.tracks_time() { String::new() } else { h.unit_label().to_string() },
                count_today: today_stats.and_then(|d| d.get(id)).map_or(0, |s| s.count),
                rounds_today: today_stats.and_then(|d| d.get(id)).map_or(0, |s| s.completions),
                daily_limit: if h.kind.is_timed() { None } else { h.rounds_per_day() },
            })
            .collect();
        let groups = self
            .config
            .groups
            .iter()
            .map(|(id, g)| {
                let remaining = self.unlock_remaining(id, now);
                GroupView {
                    id: id.clone(),
                    name: self.config.group_name(id).to_string(),
                    blocked: self.is_group_blocking(id, now),
                    unlock_remaining_ms: remaining,
                    credit_ms: self.state.credits.get(id).copied().unwrap_or(0),
                    apps: g.apps.clone(),
                    domains: g.domains.clone(),
                    processes: g.processes.clone(),
                    scheduled: g.has_schedule(),
                    off_schedule: !self.is_group_scheduled(id, now),
                    schedule_label: self.schedule_label(id, now),
                    schedule_next_change_at_ms: self.next_schedule_change(id, now).unwrap_or(0),
                    schedule_summary: g.schedule_summary(),
                    unlock_mode: g.unlock_mode.name().to_string(),
                    unlock_label: self.unlock_label(id, now),
                    unlock_until_ms: self
                        .state
                        .open_unlocks
                        .get(id)
                        .filter(|_| self.is_group_unlocked(id, now))
                        .map_or(0, |u| u.until),
                    rest_of_day_price_ms: g.rest_of_day_price.unwrap_or(0),
                    requires: g
                        .requires
                        .iter()
                        .map(|h| RequirementView {
                            habit: h.clone(),
                            name: self.config.habit_name(h).to_string(),
                            done: self.completed_today(h, now),
                            progress: self.progress_today(h, now),
                        })
                        .collect(),
                    require_all: g.require == RequireMode::All,
                    requirements_met: self.requirements_met(id, now),
                }
            })
            .collect();
        Snapshot {
            now_ms: now,
            session,
            habits,
            groups,
            penalty_remaining_ms: self.penalty_remaining(now),
            idle: self.idle,
            blocked_domains: self.blocked_domains(now).into_iter().collect(),
            lock: self.state.lock.as_ref().filter(|l| now < l.ends_at()).map(|l| LockView {
                ends_at_ms: l.ends_at(),
                remaining_ms: l.ends_at() - now,
                until_ms: l.until,
                end_requested: l.end_requested_at.is_some(),
                pending: l.pending.clone(),
                extended_ms: l.extended_ms,
                browser_guard: !self.config.general.browsers.is_empty(),
            }),
            streak_days: stats::overall_streak(&self.state.days, today).current,
            events_seq: self.state.events_seq,
            credit_expires_at_ms: self.next_day_start(now),
            credit_expires_label: self.clock_label(self.next_day_start(now), now),
            app_names: self.app_names_view(),
            app_categories: self.app_categories_view(),
            update: self.update.clone().filter(|_| self.config.general.update_check),
            settings: SettingsView {
                sound: self.config.general.sound.clone(),
                day_start: format_time_of_day(self.config.general.day_start),
                idle_timeout: format_duration_config(self.config.general.idle_timeout),
                emergency_penalty: format_duration_config(self.config.general.emergency_penalty),
                expiry_warning: format_duration_config(self.config.general.expiry_warning),
                notifications: self.config.general.notifications,
                terminal_programs: self.config.general.terminal_programs,
                auto_categories: self.config.general.auto_categories,
                update_check: self.config.general.update_check,
            },
        }
    }

    // ---- internals ------------------------------------------------------

    /// Appends to the activity log. `text` is what clients display.
    fn log_event(&mut self, now: u64, kind: EventKind, subject: Option<&str>, amount: u64, text: String) {
        let event = Event { at: now, kind, subject: subject.map(str::to_string), amount, text };
        self.queue(Record::Event(event.clone()));
        self.state.push_event(event);
        self.dirty = true;
    }

    /// Queues a record for the archive, dropping the oldest ones when nothing
    /// drains the queue.
    fn queue(&mut self, record: Record) {
        self.archive.push(record);
        if self.archive.len() > archive::MAX_QUEUED {
            let excess = self.archive.len() - archive::MAX_QUEUED;
            self.archive.drain(..excess);
        }
    }

    /// Records for the archive since the last call, oldest first.
    pub fn take_archive(&mut self) -> Vec<Record> {
        std::mem::take(&mut self.archive)
    }

    /// Archives the current visit, if it's long enough to be one.
    fn end_visit(&mut self) {
        if let Some(visit) = self.visit.take() {
            if visit.active_ms >= MIN_VISIT_MS {
                self.queue(Record::Visit(visit));
            }
        }
    }

    /// Archives the idle stretch in progress, if any.
    fn end_afk(&mut self, now: u64) {
        if let Some(start) = self.afk_since.take() {
            if now >= start + MIN_VISIT_MS {
                self.queue(Record::Afk(Afk { start, end: now, reason: AfkReason::Idle }));
            }
        }
    }

    /// Notification title per event kind.
    fn event_title(kind: EventKind) -> &'static str {
        match kind {
            EventKind::HabitCompleted => "Habit complete",
            EventKind::RoundCompleted => "Round complete",
            EventKind::BlockEnforced => "Blocked",
            EventKind::UnlockEnded => "Unlock ended",
            EventKind::UsageExhausted => "Unlock used up",
            EventKind::CreditExpired => "Credit expired",
            EventKind::ScheduleOpened => "Block active",
            EventKind::ScheduleClosed => "Block off hours",
            EventKind::LockEnded => "Commitment ended",
            EventKind::LockExtended => "Commitment extended",
            _ => "habitfocus",
        }
    }

    /// Logs an event and shows it as a notification, titled by kind.
    fn log_and_notify(
        &mut self,
        fx: &mut Vec<Effect>,
        now: u64,
        kind: EventKind,
        subject: Option<&str>,
        amount: u64,
        text: String,
    ) {
        self.notify(fx, Self::event_title(kind), text.clone());
        self.log_event(now, kind, subject, amount, text);
    }

    fn notify(&self, fx: &mut Vec<Effect>, title: &str, body: String) {
        if self.config.general.notifications {
            fx.push(Effect::Notify {
                title: title.to_string(),
                body,
            });
        }
    }

    fn running_habit(&self) -> Option<Habit> {
        let session = self.state.session.as_ref()?;
        self.config.habits.get(&session.habit).cloned()
    }

    fn window_allowed(&self, habit: &Habit, id: u64) -> bool {
        match habit.kind {
            HabitKind::Timer => self.timer_window == Some(id),
            HabitKind::Manual | HabitKind::Counter | HabitKind::Passive | HabitKind::Other => false,
            HabitKind::Apps => self
                .windows
                .get(&id)
                .is_some_and(|w| habit.allows(&w.app_id, &w.title, self.window_url(w), self.terminal_front(id))),
        }
    }

    fn timer_focused(&self, now: u64) -> bool {
        self.timer_clients
            .values()
            .any(|&(focused, seen)| focused && now.saturating_sub(seen) <= TIMER_STALE_MS)
    }

    /// Latest time a timer session may count up to: a client that went silent
    /// while focused only counts until it became stale.
    fn timer_count_limit(&self, now: u64) -> u64 {
        let last_focused_report = self
            .timer_clients
            .values()
            .filter(|(focused, _)| *focused)
            .map(|&(_, seen)| seen)
            .max();
        match last_focused_report {
            Some(seen) => now.min(seen + TIMER_STALE_MS),
            None => now,
        }
    }

    /// Brings the session's time up to date. Returns the session taken out of
    /// the state, or `None` if it completed while settling.
    fn settle(&mut self, now: u64, fx: &mut Vec<Effect>) -> Result<Option<Session>, String> {
        if self.state.session.is_none() {
            return Err("no session is running".into());
        }
        self.update_session(now, fx);
        Ok(self.state.session.take().map(|mut session| {
            session.running_since = None;
            session
        }))
    }

    /// Records a finished session. `count_completion` adds a completion to
    /// today's stats (rounds of `ask` habits are counted when they complete).
    fn record_end(&mut self, session: &Session, now: u64, outcome: Outcome, count_completion: bool) {
        let focused_ms = session.accumulated_ms.saturating_sub(session.resumed_ms);
        let day = self.day_of(now);
        stats::record(&mut self.state.days, day, &session.habit, focused_ms, count_completion);
        let entry = HistoryEntry {
            habit: session.habit.clone(),
            started_at: session.started_at,
            finished_at: now,
            focused_ms,
            outcome,
        };
        self.queue(Record::Session(entry.clone()));
        self.state.record(entry);
        self.last_allowed = None;
        self.dirty = true;
    }

    fn enforce_all_windows(&mut self, now: u64, fx: &mut Vec<Effect>) {
        let ids: Vec<u64> = self.windows.keys().copied().collect();
        for id in ids {
            self.enforce_window(id, now, fx);
        }
    }

    fn enforce_window(&mut self, id: u64, now: u64, fx: &mut Vec<Effect>) {
        if self.closing.contains(&id) {
            return;
        }
        let Some(window) = self.windows.get(&id) else {
            return;
        };
        if !self.is_app_blocked(&window.app_id, now) {
            return;
        }
        let app = window.app_id.clone();
        self.closing.insert(id);
        fx.push(Effect::CloseWindow(id));
        // A blocked app that respawns windows would otherwise flood the log
        // and the notification tray.
        let recent = self.blocked_logged.get(&app).is_some_and(|&at| now.saturating_sub(at) < 60_000);
        if !recent {
            self.blocked_logged.insert(app.clone(), now);
            let text = format!("{} is locked. Finish a habit to unlock it.", self.config.app_name(&app));
            self.log_and_notify(fx, now, EventKind::BlockEnforced, Some(&app), 0, text);
        }
    }

    fn enforce_strict(&mut self, fx: &mut Vec<Effect>) {
        let Some(habit) = self.running_habit() else {
            return;
        };
        if habit.strictness != Strictness::Strict
            || (habit.kind == HabitKind::Apps && habit.allow.is_empty())
        {
            return;
        }
        if let Some(id) = self.focused {
            if self.window_allowed(&habit, id) {
                self.last_allowed = Some(id);
                return;
            }
            let exempt = self.windows.get(&id).is_some_and(|w| {
                self.config
                    .general
                    .strict_exempt_apps
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(&w.app_id))
            });
            if exempt {
                return;
            }
        }
        let target = self
            .last_allowed
            .filter(|&id| self.window_allowed(&habit, id))
            .or_else(|| {
                self.windows
                    .keys()
                    .copied()
                    .find(|&id| self.window_allowed(&habit, id))
            });
        if let Some(target) = target {
            fx.push(Effect::FocusWindow(target));
        }
    }

    fn update_session(&mut self, now: u64, fx: &mut Vec<Effect>) {
        let Some(habit) = self.running_habit() else {
            return;
        };
        let (focused_ok, idle_pauses) = match habit.kind {
            HabitKind::Timer => (self.timer_focused(now), false),
            // Logged habits never have a session.
            HabitKind::Manual | HabitKind::Counter | HabitKind::Passive | HabitKind::Other => (false, false),
            HabitKind::Apps => (
                habit.allow.is_empty() || self.focused.is_some_and(|id| self.window_allowed(&habit, id)),
                true,
            ),
        };
        if focused_ok && habit.kind == HabitKind::Apps && habit.strictness == Strictness::Strict {
            self.last_allowed = self.focused;
        }
        let should_run = focused_ok && !(idle_pauses && self.idle);

        let count_until = match habit.kind {
            HabitKind::Timer => self.timer_count_limit(now),
            _ => now,
        };
        let session = self.state.session.as_mut().expect("running_habit checked");
        if let Some(since) = session.running_since {
            session.accumulated_ms += count_until.saturating_sub(since);
        }
        match habit.on_target() {
            OnTarget::Finish if session.accumulated_ms >= habit.target => {
                self.complete(&habit, now, fx);
                return;
            }
            OnTarget::Finish => {}
            OnTarget::Ask => {
                let mut completed_rounds = Vec::new();
                while session.accumulated_ms >= habit.target * (session.extra_rounds as u64 + 1) {
                    session.extra_rounds += 1;
                    session.awaiting = true;
                    completed_rounds.push(session.extra_rounds);
                }
                for round in completed_rounds {
                    self.bank_round(&habit, round, now, fx);
                }
            }
        }
        let session = self.state.session.as_mut().expect("still running");
        if should_run != session.running_since.is_some() {
            self.dirty = true;
        }
        session.running_since = should_run.then_some(now);
    }

    /// Completes a `finish` habit: ends the session and grants the reward.
    fn complete(&mut self, habit: &Habit, now: u64, fx: &mut Vec<Effect>) {
        let mut session = self.state.session.take().expect("session exists");
        session.running_since = None;
        self.record_end(&session, now, Outcome::Completed, true);
        let name = self.config.habit_name(&session.habit).to_string();
        let mut message = format!("{name} done. {}.", self.grant_reward(habit, now));
        let streak = stats::habit_streak(&self.state.days, self.day_of(now), &session.habit).current;
        if streak >= 2 {
            message += &format!(" {streak}-day streak.");
        }
        let amount = habit.reward.duration;
        self.play_sound(fx);
        self.log_and_notify(fx, now, EventKind::HabitCompleted, Some(&session.habit), amount, message);
    }

    /// Sounds the end of a timed habit's target, unless it's switched off.
    fn play_sound(&self, fx: &mut Vec<Effect>) {
        let sound = self.config.general.sound.trim();
        if !sound.is_empty() {
            fx.push(Effect::PlaySound(sound.to_string()));
        }
    }

    /// Banks the reward of a completed round of an `ask` habit; the session
    /// continues.
    fn bank_round(&mut self, habit: &Habit, round: u32, now: u64, fx: &mut Vec<Effect>) {
        let habit_id = self.state.session.as_ref().expect("session exists").habit.clone();
        let day = self.day_of(now);
        stats::record(&mut self.state.days, day, &habit_id, 0, true);
        let name = self.config.habit_name(&habit_id).to_string();
        let mut message = format!("{name}: round {round} done. {}.", self.grant_reward(habit, now));
        if round == 1 {
            let streak = stats::habit_streak(&self.state.days, self.day_of(now), &habit_id).current;
            if streak >= 2 {
                message += &format!(" {streak}-day streak.");
            }
        }
        message += " Keep going for another round, or stop.";
        let amount = habit.reward.duration;
        self.play_sound(fx);
        self.log_and_notify(fx, now, EventKind::RoundCompleted, Some(&habit_id), amount, message);
        self.dirty = true;
    }

    /// Credits or unlocks the habit's reward groups; returns a description.
    fn grant_reward(&mut self, habit: &Habit, now: u64) -> String {
        let amount = habit.reward.duration;
        let groups = habit
            .reward
            .groups
            .iter()
            .map(|g| self.config.group_name(g).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let reward = format_duration(amount);
        self.dirty = true;
        match habit.reward.mode {
            RewardMode::Bank => {
                for group in &habit.reward.groups {
                    *self.state.credits.entry(group.clone()).or_default() += amount;
                }
                format!("Earned {reward} for {groups}")
            }
            RewardMode::Immediate => {
                // A group still waiting for its required habits gets the
                // reward as credit, so it can't be unlocked around the gate.
                let (open, gated): (Vec<&String>, Vec<&String>) =
                    habit.reward.groups.iter().partition(|g| self.requirements_met(g, now));
                for group in &open {
                    self.extend_unlock(group, amount, now);
                }
                for group in &gated {
                    *self.state.credits.entry((*group).clone()).or_default() += amount;
                }
                let names = |list: &[&String]| {
                    list.iter().map(|g| self.config.group_name(g).to_string()).collect::<Vec<_>>().join(", ")
                };
                match (open.is_empty(), gated.is_empty()) {
                    (_, true) => format!("{groups} unlocked for {reward}"),
                    (true, false) => format!("Earned {reward} for {groups} (its other required habits aren't done yet)"),
                    (false, false) => format!(
                        "{} unlocked for {reward}; earned {reward} for {} (waiting for its required habits)",
                        names(&open),
                        names(&gated)
                    ),
                }
            }
        }
    }

    /// Logs progress on a manual or counter habit and banks one reward per
    /// full `goal` (at most `daily_limit` a day). `set` replaces today's count;
    /// otherwise `amount` is added (negative amounts correct mistakes but never
    /// take back banked rewards).
    pub fn log_done(&mut self, habit_id: &str, amount: i64, set: bool, now: u64) -> Result<(Vec<Effect>, String), String> {
        let Some(habit) = self.config.habits.get(habit_id).cloned() else {
            return Err(format!(
                "unknown habit {habit_id:?} (known: {})",
                self.config.habits.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        };
        let name = self.config.habit_name(habit_id).to_string();
        if habit.kind.is_timed() {
            return Err(format!("{name} is tracked by time; start it with `hf start {habit_id}`"));
        }
        if habit.kind == HabitKind::Passive {
            return Err(format!("{name} counts by itself while its windows are focused"));
        }
        let day = self.day_of(now);
        let goal = habit.goal();
        let limit = habit.rounds_per_day().unwrap_or(u32::MAX);
        if habit.kind == HabitKind::Manual && amount > 0 && !set && self.completed_today(habit_id, now) {
            return Ok((Vec::new(), format!("{name} is already done today")));
        }
        let (count, new_rounds, rounds) = {
            let stats = self.state.days.entry(day).or_default().entry(habit_id.to_string()).or_default();
            stats.count = if set { amount.max(0) as u64 } else { stats.count.saturating_add_signed(amount) };
            let due = u32::try_from(stats.count / goal).unwrap_or(u32::MAX).min(limit);
            let new_rounds = due.saturating_sub(stats.completions);
            stats.completions += new_rounds;
            (stats.count, new_rounds, stats.completions)
        };
        self.dirty = true;
        let unit = habit.unit_label().to_string();
        let logged = if habit.kind == HabitKind::Manual && goal == 1 {
            format!("{name} {}", if count > 0 { "logged" } else { "unlogged" })
        } else {
            format!("{name}: {count} {unit} today")
        };
        self.log_event(now, EventKind::CountLogged, Some(habit_id), count, logged);

        let mut fx = Vec::new();
        let mut message = None;
        for _ in 0..new_rounds {
            let mut text = format!("{name} done. {}.", self.grant_reward(&habit, now));
            let streak = stats::habit_streak(&self.state.days, day, habit_id).current;
            if streak >= 2 {
                text += &format!(" {streak}-day streak.");
            }
            self.log_and_notify(&mut fx, now, EventKind::HabitCompleted, Some(habit_id), habit.reward.duration, text.clone());
            message = Some(text);
        }
        if new_rounds > 1 {
            message = message.map(|m| format!("{m} ({new_rounds} rounds)"));
        }
        let message = message.unwrap_or_else(|| {
            if rounds >= limit {
                format!("{name}: {count} {unit} today (daily limit of {limit} reached)")
            } else {
                format!("{name}: {count} / {} {unit} today", goal * (u64::from(rounds) + 1))
            }
        });
        Ok((fx, message))
    }

    fn completed_today(&self, habit: &str, now: u64) -> bool {
        self.state
            .days
            .get(&self.day_of(now))
            .and_then(|d| d.get(habit))
            .is_some_and(|s| s.completions > 0)
    }

    /// Focused time today, including a running session.
    fn today_focused(&self, habit: &str, now: u64) -> u64 {
        let logged = self.state.days.get(&self.day_of(now)).and_then(|d| d.get(habit)).map_or(0, |s| s.focused_ms);
        let running = self
            .state
            .session
            .as_ref()
            .filter(|s| s.habit == habit)
            .map_or(0, |s| s.elapsed(now).saturating_sub(s.resumed_ms));
        logged + running
    }

    /// Progress towards completing `habit` today, 0–1.
    fn progress_today(&self, habit_id: &str, now: u64) -> f64 {
        if self.completed_today(habit_id, now) {
            return 1.0;
        }
        let Some(habit) = self.config.habits.get(habit_id) else { return 0.0 };
        let (done, needed) = if habit.kind.tracks_time() {
            (self.today_focused(habit_id, now), habit.target)
        } else {
            let count = self.state.days.get(&self.day_of(now)).and_then(|d| d.get(habit_id)).map_or(0, |s| s.count);
            (count, habit.goal())
        };
        if needed == 0 { 0.0 } else { (done as f64 / needed as f64).min(1.0) }
    }

    /// The group's required habits are done today (always true without any).
    pub fn requirements_met(&self, group: &str, now: u64) -> bool {
        let Some(g) = self.config.groups.get(group) else { return true };
        if g.requires.is_empty() {
            return true;
        }
        match g.require {
            RequireMode::Any => g.requires.iter().any(|h| self.completed_today(h, now)),
            RequireMode::All => g.requires.iter().all(|h| self.completed_today(h, now)),
        }
    }

    fn requirement_message(&self, group: &str, now: u64) -> String {
        let g = &self.config.groups[group];
        let name = self.config.group_name(group);
        let habit_names: Vec<&str> = g.requires.iter().map(|h| self.config.habit_name(h)).collect();
        match g.require {
            RequireMode::Any => format!("{name} needs one of {} done today", habit_names.join(", ")),
            RequireMode::All => {
                let done = g.requires.iter().filter(|h| self.completed_today(h, now)).count();
                let missing: Vec<&str> = g
                    .requires
                    .iter()
                    .filter(|h| !self.completed_today(h, now))
                    .map(|h| self.config.habit_name(h))
                    .collect();
                format!(
                    "{name} needs {} done today ({done} of {} done; missing {})",
                    habit_names.join(" and "),
                    g.requires.len(),
                    missing.join(", ")
                )
            }
        }
    }

    /// Unspent credit expires when a new day starts. State without a credit
    /// day (new, or from before expiry existed) adopts today without expiring.
    fn expire_credit(&mut self, now: u64, fx: &mut Vec<Effect>) {
        let today = self.day_of(now);
        match self.state.credit_day {
            Some(day) if day == today => {}
            Some(_) => {
                let total: u64 = self.state.credits.values().sum();
                if total > 0 {
                    self.state.credits.clear();
                    let text = format!("{} of unspent credit expired with the new day.", format_duration(total));
                    self.log_and_notify(fx, now, EventKind::CreditExpired, None, total, text);
                }
                self.state.credit_day = Some(today);
                self.dirty = true;
            }
            None => {
                self.state.credit_day = Some(today);
                self.dirty = true;
            }
        }
    }

    /// Announces schedule windows opening and closing, closes windows that
    /// became blocked, and warns `expiry_warning` before a window opens.
    fn check_schedules(&mut self, now: u64, fx: &mut Vec<Effect>) {
        let scheduled: Vec<String> =
            self.config.groups.iter().filter(|(_, g)| g.has_schedule()).map(|(id, _)| id.clone()).collect();
        let blocking: BTreeSet<String> =
            scheduled.iter().filter(|id| self.is_group_scheduled(id, now)).cloned().collect();
        if !self.schedule_seeded {
            self.schedule_blocking = blocking;
            self.schedule_seeded = true;
            return;
        }
        let opened: Vec<String> = blocking.difference(&self.schedule_blocking).cloned().collect();
        let closed: Vec<String> = self.schedule_blocking.difference(&blocking).cloned().collect();
        for group in &opened {
            self.schedule_warned.remove(group);
            let name = self.config.group_name(group).to_string();
            let until = self.next_schedule_change(group, now).map(|at| self.clock_label(at, now));
            let text = match (self.is_group_unlocked(group, now), until) {
                (true, _) => format!("{name}'s blocking hours started; your unlock keeps it open for now."),
                (false, Some(until)) => format!("{name} is blocked until {until}."),
                (false, None) => format!("{name} is blocked."),
            };
            self.log_and_notify(fx, now, EventKind::ScheduleOpened, Some(group), 0, text);
        }
        for group in &closed {
            let name = self.config.group_name(group).to_string();
            let text = match self.next_schedule_change(group, now) {
                Some(at) => format!("{name} is off hours until {}.", self.clock_label(at, now)),
                None => format!("{name} is off hours."),
            };
            self.log_and_notify(fx, now, EventKind::ScheduleClosed, Some(group), 0, text);
        }
        if !opened.is_empty() {
            self.enforce_all_windows(now, fx);
        }
        let warning = self.config.general.expiry_warning;
        for group in scheduled.iter().filter(|g| !blocking.contains(*g)) {
            let Some(at) = self.next_schedule_change(group, now) else { continue };
            if at.saturating_sub(now) <= warning && self.schedule_warned.insert(group.clone()) {
                let text = format!(
                    "{} blocks in {}.",
                    self.config.group_name(group),
                    format_duration(at.saturating_sub(now))
                );
                self.notify(fx, "Block starting", text);
            }
        }
        self.schedule_blocking = blocking;
    }

    /// During a lock, closes browser windows whose process has no live
    /// extension host (the extension was disabled or removed).
    fn guard_browsers(&mut self, now: u64, fx: &mut Vec<Effect>) {
        if !self.is_locked(now) || self.config.general.browsers.is_empty() {
            self.unverified_browsers.clear();
            return;
        }
        let live_pids: BTreeSet<u32> = self
            .extension_hosts
            .values()
            .filter(|(_, seen)| now.saturating_sub(*seen) <= EXTENSION_STALE_MS)
            .map(|(pid, _)| *pid)
            .collect();
        let mut to_close = Vec::new();
        let mut unverified = BTreeMap::new();
        for window in self.windows.values() {
            let is_browser = self.config.is_browser(&window.app_id);
            if !is_browser || window.pid.is_some_and(|pid| live_pids.contains(&pid)) {
                continue;
            }
            let since = self.unverified_browsers.get(&window.id).copied().unwrap_or(now);
            if now.saturating_sub(since) >= BROWSER_GRACE_MS {
                to_close.push((window.id, window.app_id.clone()));
            }
            unverified.insert(window.id, since);
        }
        self.unverified_browsers = unverified;
        for (id, app) in to_close {
            if self.closing.insert(id) {
                fx.push(Effect::CloseWindow(id));
                self.notify(
                    fx,
                    "Browser closed",
                    format!("{app} runs without the habitfocus extension, which your commitment doesn't allow."),
                );
            }
        }
    }

    /// Opens or extends an unlock in the group's mode. `amount` is clock time
    /// (`wallclock`), budget of use (`usage`) or the price paid (`rest_of_day`).
    fn extend_unlock(&mut self, group: &str, amount: u64, now: u64) {
        let mode = self.config.groups.get(group).map_or(UnlockMode::Wallclock, |g| g.unlock_mode);
        let live = self
            .state
            .open_unlocks
            .get(group)
            .filter(|u| u.mode == mode && self.is_group_unlocked(group, now))
            .cloned();
        let started_at = live.as_ref().map_or(now, |u| u.started_at);
        let granted_ms = live.as_ref().map_or(0, |u| u.granted_ms) + amount;
        let unlock = match mode {
            UnlockMode::Wallclock => Unlock {
                mode,
                started_at,
                until: live.as_ref().map_or(now, |u| u.until) + amount,
                granted_ms,
                usage_left_ms: 0,
            },
            UnlockMode::Usage => Unlock {
                mode,
                started_at,
                until: self.next_day_start(now),
                granted_ms,
                usage_left_ms: live.as_ref().map_or(0, |u| u.usage_left_ms) + amount,
            },
            UnlockMode::RestOfDay => Unlock {
                mode,
                started_at,
                until: self.next_day_start(now),
                granted_ms,
                usage_left_ms: 0,
            },
        };
        self.state.open_unlocks.insert(group.to_string(), unlock);
        self.warned.remove(group);
        self.dirty = true;
    }

    fn expire(&mut self, now: u64, fx: &mut Vec<Effect>) {
        self.fold_usage(now);
        self.check_schedules(now, fx);
        self.expire_credit(now, fx);
        if self.state.lock.as_ref().is_some_and(|l| now >= l.ends_at()) {
            let lock = self.state.lock.take().expect("checked");
            self.dirty = true;
            self.unverified_browsers.clear();
            let pending = if lock.pending.is_empty() {
                String::new()
            } else {
                format!(" {} held-back change(s) now apply.", lock.pending.len())
            };
            let text = format!("Your commitment is over.{pending}");
            self.log_and_notify(fx, now, EventKind::LockEnded, None, 0, text);
            fx.push(Effect::ReloadConfig);
        }
        self.guard_browsers(now, fx);

        let today = self.day_of(now);
        let before = self.state.progress.len();
        self.state.progress.retain(|_, saved| saved.day == today);
        if self.state.progress.len() != before {
            self.dirty = true;
        }

        if self.state.penalty_until.is_some_and(|until| until <= now) {
            self.state.penalty_until = None;
            self.dirty = true;
            self.log_event(now, EventKind::PenaltyEnded, None, 0, "Unlocks are allowed again".into());
        }

        let expired: Vec<String> = self
            .state
            .open_unlocks
            .keys()
            .filter(|group| !self.is_group_unlocked(group, now))
            .cloned()
            .collect();
        for group in &expired {
            let unlock = self.state.open_unlocks.remove(group).expect("listed above");
            self.warned.remove(group);
            self.dirty = true;
            let name = self.config.group_name(group).to_string();
            if unlock.mode == UnlockMode::Usage && unlock.usage_left_ms == 0 {
                let text = format!("{name} used up its unlock and is locked again.");
                self.log_and_notify(fx, now, EventKind::UsageExhausted, Some(group), unlock.granted_ms, text);
            } else {
                let text = format!("{name} is locked again.");
                self.log_and_notify(fx, now, EventKind::UnlockEnded, Some(group), 0, text);
            }
        }
        if !expired.is_empty() {
            self.enforce_all_windows(now, fx);
        }

        let warning = self.config.general.expiry_warning;
        let upcoming: Vec<(String, u64, UnlockMode)> = self
            .state
            .open_unlocks
            .iter()
            .map(|(g, u)| (g.clone(), self.unlock_remaining(g, now), u.mode))
            .filter(|(g, remaining, _)| *remaining <= warning && !self.warned.contains(g))
            .collect();
        for (group, remaining, mode) in upcoming {
            let name = self.config.group_name(&group);
            let text = match mode {
                UnlockMode::Usage => format!("{name} re-blocks after {} more use.", format_duration(remaining)),
                _ => format!("{name} locks again in {}.", format_duration(remaining)),
            };
            self.notify(fx, "Unlock ending", text);
            self.warned.insert(group);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
        [general]
        idle_timeout = "60s"
        emergency_penalty = "1h"
        expiry_warning = "60s"
        strict_exempt_apps = ["exempt"]

        [groups.social]
        apps = ["discord"]
        processes = ["Discord"]
        domains = ["reddit.com"]

        [groups.games]
        apps = ["steam"]

        [habits.reading]
        target = "10m"
        allow = [{ app = "zathura" }]
        reward = { groups = ["social"], duration = "30m" }

        [habits.deep]
        target = "10m"
        allow = [{ app = "zathura" }]
        strictness = "strict"
        reward = { groups = ["games"], duration = "1h", mode = "immediate" }

        [habits.anything]
        target = "1m"
        reward = { groups = ["games"], duration = "5m" }
    "#;

    const MIN: u64 = 60_000;

    fn win(id: u64, app: &str) -> WindowInfo {
        WindowInfo {
            id,
            app_id: app.into(),
            title: String::new(),
            pid: None,
        }
    }

    fn engine() -> Engine {
        let mut e = Engine::new(Config::from_toml(CONFIG).unwrap(), State::default());
        e.handle(
            Input::WindowsReset(vec![win(1, "zathura"), win(2, "kitty")]),
            0,
        );
        e
    }

    #[test]
    fn blocked_app_windows_are_closed() {
        let mut e = engine();
        let fx = e.handle(Input::WindowChanged(win(3, "discord")), 0);
        assert!(fx.contains(&Effect::CloseWindow(3)));
        // Title changes of the same window don't re-issue the close.
        let fx = e.handle(Input::WindowChanged(win(3, "discord")), 10);
        assert!(!fx.contains(&Effect::CloseWindow(3)));
        assert!(e.is_process_blocked("Discord", 0));
        assert!(e.blocked_domains(0).contains("reddit.com"));
    }

    #[test]
    fn terminals_and_habitfocus_are_never_blocked() {
        let config = "[groups.all]\napps = [\"kitty\", \"discord\"]\nprocesses = [\"hf\", \"habitd\", \"bash\", \"steam\"]";
        let mut e = Engine::new(Config::from_toml(config).unwrap(), State::default());
        let fx = e.handle(Input::WindowChanged(win(3, "kitty")), 0);
        assert!(!fx.contains(&Effect::CloseWindow(3)), "{fx:?}");
        let fx = e.handle(Input::WindowChanged(win(4, "discord")), 0);
        assert!(fx.contains(&Effect::CloseWindow(4)), "{fx:?}");
        for comm in ["hf", "habitd", "bash"] {
            assert!(!e.is_process_blocked(comm, 0), "{comm}");
        }
        assert!(e.is_process_blocked("steam", 0));
    }

    #[test]
    fn timer_counts_only_while_focused_and_active() {
        let mut e = engine();
        e.start("reading", 0).unwrap();
        e.handle(Input::FocusChanged(Some(1)), 0);
        e.handle(Input::Tick, 2 * MIN);
        assert_eq!(e.snapshot(2 * MIN).session.unwrap().elapsed_ms, 2 * MIN);

        // Tabbing out pauses.
        e.handle(Input::FocusChanged(Some(2)), 2 * MIN);
        e.handle(Input::Tick, 5 * MIN);
        let s = e.snapshot(5 * MIN).session.unwrap();
        assert_eq!(s.elapsed_ms, 2 * MIN);
        assert_eq!(s.pause_reason, Some(PauseReason::Unfocused));

        // Back, but idle.
        e.handle(Input::FocusChanged(Some(1)), 5 * MIN);
        e.handle(Input::Idle(true), 6 * MIN);
        e.handle(Input::Tick, 9 * MIN);
        let s = e.snapshot(9 * MIN).session.unwrap();
        assert_eq!(s.elapsed_ms, 3 * MIN);
        assert_eq!(s.pause_reason, Some(PauseReason::Idle));

        e.handle(Input::Idle(false), 9 * MIN);
        let fx = e.handle(Input::Tick, 16 * MIN);
        assert!(e.state().session.is_none());
        assert!(matches!(fx.as_slice(), [Effect::PlaySound(_), Effect::Notify { .. }]), "{fx:?}");
        assert_eq!(e.state().credits["social"], 30 * MIN);
    }

    #[test]
    fn strict_session_pulls_focus_back() {
        let mut e = engine();
        e.handle(Input::FocusChanged(Some(1)), 0);
        e.start("deep", 0).unwrap();
        let fx = e.handle(Input::FocusChanged(Some(2)), MIN);
        assert_eq!(fx, vec![Effect::FocusWindow(1)]);

        // Exempt apps may take focus.
        e.handle(Input::WindowChanged(win(5, "exempt")), MIN);
        let fx = e.handle(Input::FocusChanged(Some(5)), MIN);
        assert!(fx.is_empty());

        // Starting strict while elsewhere focuses the allowed window at once.
        let mut e = engine();
        e.handle(Input::FocusChanged(Some(2)), 0);
        assert_eq!(e.start("deep", 0).unwrap().0, vec![Effect::FocusWindow(1)]);
    }

    #[test]
    fn strict_abort_requires_emergency_and_applies_penalty() {
        let mut e = engine();
        e.start("deep", 0).unwrap();
        assert!(e.abort(false, MIN).is_err());
        e.abort(true, MIN).unwrap();
        assert!(e.state().session.is_none());
        assert_eq!(e.penalty_remaining(MIN), 60 * MIN);

        e.state.credits.insert("social".into(), 10 * MIN);
        assert!(e.unlock("social", None, 2 * MIN).is_err());
        e.handle(Input::Tick, 62 * MIN);
        assert!(e.unlock("social", None, 62 * MIN).is_ok());
    }

    #[test]
    fn soft_abort_is_free() {
        let mut e = engine();
        e.start("reading", 0).unwrap();
        e.abort(false, MIN).unwrap();
        assert_eq!(e.penalty_remaining(MIN), 0);
    }

    #[test]
    fn unlock_spends_credit_and_expires() {
        let mut e = engine();
        e.state.credits.insert("social".into(), 30 * MIN);
        assert!(e.unlock("social", Some(40 * MIN), 0).is_err());
        let (_, granted) = e.unlock("social", Some(10 * MIN), 0).unwrap();
        assert_eq!(granted, 10 * MIN);
        assert_eq!(e.state().credits["social"], 20 * MIN);
        assert!(!e.is_app_blocked("discord", 0));
        assert!(e.handle(Input::WindowChanged(win(3, "discord")), 0).is_empty());

        let fx = e.handle(Input::Tick, 9 * MIN + 30_000);
        assert!(matches!(&fx[..], [Effect::Notify { title, .. }] if title == "Unlock ending"));

        let fx = e.handle(Input::Tick, 10 * MIN);
        assert!(fx.contains(&Effect::CloseWindow(3)));
        assert!(e.is_app_blocked("discord", 10 * MIN));
    }

    #[test]
    fn relock_refunds_unused_time() {
        let mut e = engine();
        e.state.credits.insert("social".into(), 30 * MIN);
        e.unlock("social", None, 0).unwrap();
        e.relock("social", 10 * MIN).unwrap();
        assert_eq!(e.state().credits["social"], 20 * MIN);
        assert!(e.is_app_blocked("discord", 10 * MIN));
    }

    #[test]
    fn immediate_reward_unlocks() {
        let mut e = engine();
        e.handle(Input::FocusChanged(Some(1)), 0);
        e.start("deep", 0).unwrap();
        e.handle(Input::Tick, 10 * MIN);
        assert!(e.is_group_unlocked("games", 10 * MIN));
        assert!(e.is_group_unlocked("games", 69 * MIN));
        assert!(!e.is_group_unlocked("games", 70 * MIN));
    }

    #[test]
    fn habit_without_allow_rules_counts_any_activity() {
        let mut e = engine();
        e.start("anything", 0).unwrap();
        e.handle(Input::FocusChanged(None), 0);
        e.handle(Input::Tick, MIN);
        assert!(e.state().session.is_none());
        assert_eq!(e.state().credits["games"], 5 * MIN);
    }

    #[test]
    fn browser_tab_url_decides_focus() {
        let config = format!(
            "{CONFIG}\n[habits.wiki]\ntarget = \"10m\"\nallow = [{{ app = \"zen\", url = \"wikipedia\\\\.org\" }}]\nreward = {{ groups = [\"social\"], duration = \"5m\" }}\n"
        );
        let mut e = Engine::new(Config::from_toml(&config).unwrap(), State::default());
        let mut zen = win(7, "zen");
        zen.title = "Rust (programming language) - Wikipedia — Zen Browser".into();
        e.handle(Input::WindowsReset(vec![zen.clone()]), 0);
        e.handle(Input::FocusChanged(Some(7)), 0);
        e.start("wiki", 0).unwrap();
        assert!(!e.snapshot(0).session.unwrap().running);

        let tab = |title: &str, url: &str| Input::BrowserTab {
            source: "host".into(),
            window: Some(1),
            tab: Some(BrowserTab { title: title.into(), url: url.into() }),
        };
        e.handle(tab("Rust (programming language) - Wikipedia", "https://en.wikipedia.org/wiki/Rust"), 0);
        assert!(e.snapshot(0).session.unwrap().running);

        // Switching to a YouTube tab: title and URL change.
        zen.title = "Some video - YouTube — Zen Browser".into();
        e.handle(Input::WindowChanged(zen), MIN);
        e.handle(tab("Some video - YouTube", "https://youtube.com/watch"), MIN);
        let s = e.snapshot(2 * MIN).session.unwrap();
        assert!(!s.running);
        assert_eq!(s.elapsed_ms, MIN);

        // The extension going away forgets its tabs.
        e.handle(Input::BrowserTab { source: "host".into(), window: None, tab: None }, MIN);
        assert!(e.tabs.is_empty());
    }

    const TIMER_CONFIG: &str = r#"
        [general]
        day_start = "04:00"
        emergency_penalty = "1h"

        [groups.social]
        apps = ["discord"]

        [habits.book]
        name = "Book"
        kind = "timer"
        target = "20m"
        reward = { groups = ["social"], duration = "1h" }

        [habits.strictbook]
        kind = "timer"
        target = "20m"
        strictness = "strict"
        reward = { groups = ["social"], duration = "1h" }

        [habits.code]
        target = "30m"
        allow = [{ app = "zed" }]
        reward = { groups = ["social"], duration = "30m" }
    "#;

    fn timer_engine() -> Engine {
        let mut e = Engine::new(Config::from_toml(TIMER_CONFIG).unwrap(), State::default());
        e.handle(Input::WindowsReset(vec![win(1, "kitty"), win(2, "zed"), win(3, "zen")]), 0);
        e
    }

    fn timer_focus(e: &mut Engine, focused: bool, now: u64) -> Vec<Effect> {
        e.handle(Input::TimerFocus { source: "tui".into(), focused: Some(focused) }, now)
    }

    /// Focused heartbeats every 2s from `from` to `to` (inclusive), like `hf tui`.
    fn heartbeat(e: &mut Engine, from: u64, to: u64) -> Vec<Effect> {
        let mut fx = Vec::new();
        let mut t = from;
        while t <= to {
            fx.extend(timer_focus(e, true, t));
            t += 2000;
        }
        fx
    }

    #[test]
    fn stop_saves_progress_and_start_resumes_it() {
        let mut e = timer_engine();
        e.handle(Input::FocusChanged(Some(2)), 0);
        e.start("code", 0).unwrap();
        e.handle(Input::Tick, 5 * MIN);
        let (_, message) = e.stop(5 * MIN).unwrap();
        assert!(message.contains("5:00 saved"), "{message}");
        assert!(e.state().session.is_none());
        assert_eq!(e.snapshot(5 * MIN).habits.iter().find(|h| h.id == "code").unwrap().saved_ms, 5 * MIN);

        let (_, message) = e.start("code", 60 * MIN).unwrap();
        assert!(message.contains("Resumed"), "{message}");
        e.handle(Input::Tick, 70 * MIN);
        assert_eq!(e.snapshot(70 * MIN).session.unwrap().elapsed_ms, 15 * MIN);

        // History counts each sitting separately.
        e.stop(70 * MIN).unwrap();
        let sittings: Vec<u64> = e.history(10).iter().map(|h| h.focused_ms).collect();
        assert_eq!(sittings, vec![10 * MIN, 5 * MIN]);
        assert_eq!(e.history(1)[0].outcome, Outcome::Stopped);
    }

    #[test]
    fn starting_another_habit_stops_the_current_one() {
        let mut e = timer_engine();
        e.handle(Input::FocusChanged(Some(2)), 0);
        e.start("code", 0).unwrap();
        e.handle(Input::Tick, 3 * MIN);
        let (_, message) = e.start("book", 3 * MIN).unwrap();
        assert!(message.contains("Stopped") && message.contains("Started Book"), "{message}");
        assert_eq!(e.state().progress["code"].ms, 3 * MIN);
        assert!(e.start("book", 4 * MIN).is_err());
    }

    #[test]
    fn saved_progress_resets_when_the_day_starts() {
        const HOUR: u64 = 60 * MIN;
        // UTC+2, day starts 04:00 local = 02:00 UTC.
        let mut e = timer_engine().with_local_offset(|_| 2 * HOUR as i64);
        e.handle(Input::FocusChanged(Some(2)), 0);
        // 23:40 local (21:40 UTC) until 00:20 local: still the same day.
        let evening = 21 * HOUR + 40 * MIN;
        e.start("code", evening).unwrap();
        e.handle(Input::Tick, evening + 25 * MIN);
        e.stop(evening + 25 * MIN).unwrap();
        assert_eq!(e.day_of(evening), e.day_of(evening + 25 * MIN));
        // 01:40 local: still the same day.
        e.handle(Input::Tick, evening + 2 * HOUR);
        assert_eq!(e.state().progress["code"].ms, 25 * MIN);

        // 04:00 local the next morning: a new day, progress is gone.
        let morning = 26 * HOUR;
        assert_ne!(e.day_of(morning), e.day_of(evening));
        e.handle(Input::Tick, morning);
        assert!(e.state().progress.is_empty());
        let (_, message) = e.start("code", morning).unwrap();
        assert!(message.starts_with("Started"), "{message}");
    }

    #[test]
    fn timer_counts_only_while_the_timer_is_focused_and_ignores_idle() {
        let mut e = timer_engine();
        e.handle(Input::FocusChanged(Some(1)), 0);
        e.start("book", 0).unwrap();
        timer_focus(&mut e, true, 0);
        e.handle(Input::Idle(true), 1000); // reading paper: no keyboard input
        heartbeat(&mut e, 2000, 5 * MIN);
        let s = e.snapshot(5 * MIN).session.unwrap();
        assert!(s.running);
        assert_eq!(s.elapsed_ms, 5 * MIN);

        // Looking at another window pauses.
        e.handle(Input::FocusChanged(Some(3)), 5 * MIN);
        timer_focus(&mut e, false, 5 * MIN);
        e.handle(Input::Tick, 9 * MIN);
        let s = e.snapshot(9 * MIN).session.unwrap();
        assert!(!s.running);
        assert_eq!(s.pause_reason, Some(PauseReason::Unfocused));
        assert_eq!(s.elapsed_ms, 5 * MIN);

        // A client that stops reporting (killed TUI) goes stale and only
        // counts until then.
        timer_focus(&mut e, true, 9 * MIN);
        e.handle(Input::Tick, 12 * MIN);
        let s = e.snapshot(12 * MIN).session.unwrap();
        assert!(!s.running);
        assert_eq!(s.elapsed_ms, 5 * MIN + TIMER_STALE_MS);
    }

    #[test]
    fn strict_timer_pulls_focus_back_to_the_timer_window() {
        let mut e = timer_engine();
        e.handle(Input::FocusChanged(Some(1)), 0);
        e.start("strictbook", 0).unwrap();
        timer_focus(&mut e, true, 0);
        let fx = e.handle(Input::FocusChanged(Some(3)), MIN);
        assert_eq!(fx, vec![Effect::FocusWindow(1)]);
    }

    #[test]
    fn timer_rounds_bank_the_reward_at_each_full_target() {
        let mut e = timer_engine();
        e.handle(Input::FocusChanged(Some(1)), 0);
        e.start("book", 0).unwrap();
        heartbeat(&mut e, 0, 10 * MIN);
        assert!(e.continue_session(10 * MIN).is_err());
        assert!(e.state().credits.is_empty());

        // 20 minutes: round 1 banks 1h right away and the clock restarts.
        let fx = heartbeat(&mut e, 10 * MIN + 2000, 20 * MIN);
        assert!(
            matches!(&fx[..], [Effect::PlaySound(_), Effect::Notify { title, .. }] if title == "Round complete"),
            "{fx:?}"
        );
        assert_eq!(e.state().credits["social"], 60 * MIN);
        let s = e.snapshot(20 * MIN).session.unwrap();
        assert!(s.awaiting_decision);
        assert_eq!((s.rounds_completed, s.elapsed_ms, s.target_ms, s.banked_ms), (1, 0, 20 * MIN, 60 * MIN));

        // Reading on without deciding: no extra credit until the next full target.
        assert!(heartbeat(&mut e, 20 * MIN + 2000, 30 * MIN).is_empty());
        assert_eq!(e.state().credits["social"], 60 * MIN);
        let s = e.snapshot(30 * MIN).session.unwrap();
        assert_eq!((s.elapsed_ms, s.total_elapsed_ms), (10 * MIN, 30 * MIN));
        e.continue_session(30 * MIN).unwrap();
        assert!(!e.snapshot(30 * MIN).session.unwrap().awaiting_decision);

        // 40 minutes: round 2.
        heartbeat(&mut e, 30 * MIN + 2000, 40 * MIN);
        assert_eq!(e.state().credits["social"], 120 * MIN);
        assert_eq!(e.state().days[&e.day_of(40 * MIN)]["book"].completions, 2);

        // Stopping 5 minutes into round 3 keeps both rewards and saves the 5 minutes.
        heartbeat(&mut e, 40 * MIN + 2000, 45 * MIN);
        let (_, message) = e.stop(45 * MIN).unwrap();
        assert!(message.contains("2 round(s), 2:00:00 earned, 5:00 saved"), "{message}");
        assert_eq!(e.state().progress["book"].ms, 5 * MIN);
        assert_eq!(e.history(1)[0].outcome, Outcome::Completed);
        assert_eq!(e.state().credits["social"], 120 * MIN);

        // Resuming continues the unfinished round.
        e.start("book", 50 * MIN).unwrap();
        heartbeat(&mut e, 50 * MIN, 65 * MIN);
        assert_eq!(e.state().credits["social"], 180 * MIN);
    }

    #[test]
    fn finishing_a_timed_habit_plays_a_sound() {
        let mut e = engine();
        e.handle(Input::FocusChanged(Some(1)), 0);
        e.start("reading", 0).unwrap();
        let fx = e.handle(Input::Tick, 10 * MIN);
        assert!(fx.contains(&Effect::PlaySound("complete".into())), "{fx:?}");

        // Rounds of a timer habit sound too, and logged habits don't.
        let mut e = timer_engine();
        e.start("book", 0).unwrap();
        let fx = heartbeat(&mut e, 0, 20 * MIN);
        assert!(fx.contains(&Effect::PlaySound("complete".into())), "{fx:?}");

        // An empty setting is silent.
        let quiet = CONFIG.replace("[general]", "[general]\nsound = \"\"");
        let mut e = Engine::new(Config::from_toml(&quiet).unwrap(), State::default());
        e.handle(Input::WindowsReset(vec![win(1, "zathura")]), 0);
        e.handle(Input::FocusChanged(Some(1)), 0);
        e.start("reading", 0).unwrap();
        let fx = e.handle(Input::Tick, 10 * MIN);
        assert!(!fx.iter().any(|e| matches!(e, Effect::PlaySound(_))), "{fx:?}");
    }

    #[test]
    fn apps_habits_complete_once_with_the_exact_reward() {
        let mut e = timer_engine();
        e.handle(Input::FocusChanged(Some(2)), 0);
        e.start("code", 0).unwrap();
        e.handle(Input::Tick, 31 * MIN);
        assert!(e.state().session.is_none());
        assert_eq!(e.state().credits["social"], 30 * MIN);
    }

    #[test]
    fn stopping_strict_sessions_is_free_but_abort_needs_emergency() {
        let mut e = timer_engine();
        e.start("strictbook", 0).unwrap();
        heartbeat(&mut e, 0, 2 * MIN);
        assert!(e.abort(false, MIN).is_err());
        e.stop(2 * MIN).unwrap();
        assert_eq!(e.penalty_remaining(2 * MIN), 0);
        assert_eq!(e.state().progress["strictbook"].ms, 2 * MIN);
    }

    fn locked_engine() -> Engine {
        let config = format!("[general]\nbrowsers = [\"zen\"]\n{CONFIG}").replace("[general]\n        idle_timeout", "        idle_timeout");
        let mut e = Engine::new(Config::from_toml(&config).unwrap(), State::default());
        let mut zen = win(9, "zen");
        zen.pid = Some(4242);
        e.handle(Input::WindowsReset(vec![win(1, "zathura"), zen]), 0);
        e.lock(7 * DAY, &config, 0).unwrap();
        e
    }

    const DAY: u64 = 24 * 60 * MIN;

    #[test]
    fn lock_can_only_be_extended_and_ends_after_cooldown() {
        let mut e = locked_engine();
        assert!(e.is_locked(DAY));
        assert!(e.lock(2 * DAY, "", MIN).is_err());
        assert!(e.lock(8 * DAY, "", MIN).is_ok());

        assert!(e.cancel_lock_end(MIN).is_err());
        e.request_lock_end(DAY).unwrap();
        assert!(e.request_lock_end(DAY).is_err());
        assert_eq!(e.snapshot(DAY).lock.unwrap().ends_at_ms, 2 * DAY);
        e.cancel_lock_end(DAY + MIN).unwrap();
        e.request_lock_end(2 * DAY).unwrap();

        let fx = e.handle(Input::Tick, 3 * DAY);
        assert!(fx.contains(&Effect::ReloadConfig));
        assert!(!e.is_locked(3 * DAY));
        assert!(e.snapshot(3 * DAY).lock.is_none());
    }

    #[test]
    fn downtime_extends_an_active_lock_only() {
        let mut e = locked_engine();
        let fx = e.apply_downtime(DAY, 30 * MIN);
        assert!(matches!(&fx[..], [Effect::Notify { title, .. }] if title == "Commitment extended"));
        let view = e.snapshot(DAY).lock.unwrap();
        assert_eq!(view.until_ms, 7 * DAY + 30 * MIN);
        assert_eq!(view.extended_ms, 30 * MIN);

        // Downtime after the lock already ended isn't penalized.
        assert!(e.apply_downtime(9 * DAY, 30 * MIN).is_empty());
        let mut unlocked = engine();
        assert!(unlocked.apply_downtime(0, 30 * MIN).is_empty());
    }

    #[test]
    fn browsers_without_extension_are_closed_during_a_lock() {
        let mut e = locked_engine();
        // The extension in zen (pid 4242) says hello: nothing happens.
        e.handle(Input::BrowserHello { source: "host".into(), pid: 4242 }, 0);
        assert!(e.handle(Input::Tick, BROWSER_GRACE_MS + 1000).is_empty());

        // The extension goes away: closed after the grace period.
        e.handle(Input::BrowserTab { source: "host".into(), window: None, tab: None }, 40_000);
        assert!(e.handle(Input::Tick, 41_000).is_empty());
        let fx = e.handle(Input::Tick, 41_000 + BROWSER_GRACE_MS);
        assert!(fx.contains(&Effect::CloseWindow(9)), "{fx:?}");

        // Without a lock browsers are left alone.
        let mut e = engine();
        e.handle(Input::WindowChanged(WindowInfo { pid: Some(1), ..win(9, "zen") }), 0);
        assert!(e.handle(Input::Tick, 10 * MIN).is_empty());
    }

    const HOUR: u64 = 60 * MIN;

    #[test]
    fn local_time_helpers_follow_the_offset_and_day_start() {
        // day_start 04:00 in the test config, local time UTC+2.
        let e = timer_engine().with_local_offset(|_| 2 * HOUR as i64);
        // 1970-01-01 03:00 UTC = 05:00 local, a Thursday, logical day 0.
        let thursday_morning = 3 * HOUR;
        assert_eq!(e.civil_day_of(thursday_morning), 0);
        assert_eq!(e.time_of_day(thursday_morning), 5 * HOUR);
        assert_eq!(e.weekday_of(thursday_morning), 3); // Monday = 0
        assert_eq!(e.day_of(thursday_morning), 0);

        // 01:00 local belongs to the previous logical day but the same civil day.
        let after_midnight = 23 * HOUR;
        assert_eq!(e.civil_day_of(after_midnight), 1);
        assert_eq!(e.time_of_day(after_midnight), HOUR);
        assert_eq!(e.weekday_of(after_midnight), 4);
        assert_eq!(e.day_of(after_midnight), 0);

        // Day boundaries: 04:00 local = 02:00 UTC.
        assert_eq!(e.day_start_ms(0), 2 * HOUR);
        assert_eq!(e.day_start_ms(1), 26 * HOUR);
        assert_eq!(e.next_day_start(thursday_morning), 26 * HOUR);
        assert_eq!(e.day_of(e.day_start_ms(5)), 5);

        // A DST-style offset change resolves to the later offset.
        let dst = timer_engine().with_local_offset(|t| if t < 100 * HOUR { HOUR as i64 } else { 2 * HOUR as i64 });
        assert_eq!(dst.day_of(dst.day_start_ms(7)), 7);
    }

    #[test]
    fn events_are_logged_capped_and_counted() {
        let mut e = timer_engine();
        e.log_startup(0);
        e.handle(Input::FocusChanged(Some(2)), 0);
        e.start("code", MIN).unwrap();
        e.stop(3 * MIN).unwrap();
        let kinds: Vec<_> = e.events(10).iter().map(|ev| ev.kind).collect();
        assert_eq!(
            kinds,
            vec![EventKind::SessionStopped, EventKind::SessionStarted, EventKind::DaemonStarted]
        );
        assert_eq!(Engine::event_title(EventKind::RoundCompleted), "Round complete");
        let stopped = &e.events(1)[0];
        assert!(stopped.text.contains("Stopped"), "{stopped:?}");
        assert_eq!(stopped.subject.as_deref(), Some("code"));
        assert_eq!(e.snapshot(3 * MIN).events_seq, 3);

        for i in 0..600 {
            e.log_startup(10 * MIN + i);
        }
        assert_eq!(e.state().events.len(), 500);
        assert_eq!(e.state().events_seq, 603);
    }

    /// Ticks once a second from `from` to `to`, like the daemon.
    fn tick_through(e: &mut Engine, from: u64, to: u64) {
        let mut t = from;
        while t <= to {
            e.handle(Input::Tick, t);
            t += 1000;
        }
    }

    #[test]
    fn screen_time_follows_focus_and_skips_idle() {
        let mut e = timer_engine();
        e.handle(Input::FocusChanged(Some(2)), 0); // zed
        tick_through(&mut e, 1000, 10 * MIN);
        e.handle(Input::FocusChanged(Some(1)), 10 * MIN); // kitty
        tick_through(&mut e, 10 * MIN + 1000, 12 * MIN);
        e.handle(Input::Idle(true), 12 * MIN);
        tick_through(&mut e, 12 * MIN + 1000, 20 * MIN);

        let usage = e.app_usage(1, 20 * MIN);
        let get = |app: &str| usage.iter().find(|u| u.app == app).map_or(0, |u| u.today_ms);
        assert_eq!(get("zed"), 10 * MIN);
        assert_eq!(get("kitty"), 2 * MIN);
        assert_eq!(usage[0].app, "zed");
    }

    #[test]
    fn screen_time_clamps_jumps_and_attributes_sites() {
        let mut e = timer_engine();
        e.handle(Input::FocusChanged(Some(3)), 0); // zen
        // A suspend: one tick three hours later only adds one clamped step.
        e.handle(Input::Tick, 3 * HOUR);
        assert_eq!(e.app_usage(1, 3 * HOUR)[0].today_ms, MAX_USAGE_STEP_MS);

        // With a reported tab, browser time is attributed to the site.
        let mut zen = win(3, "zen");
        zen.title = "Some video - YouTube — Zen Browser".into();
        e.handle(Input::WindowChanged(zen), 3 * HOUR);
        e.handle(
            Input::BrowserTab {
                source: "host".into(),
                window: Some(1),
                tab: Some(BrowserTab { title: "Some video - YouTube".into(), url: "https://www.youtube.com/watch".into() }),
            },
            3 * HOUR,
        );
        tick_through(&mut e, 3 * HOUR + 1000, 3 * HOUR + MIN);
        let usage = e.app_usage(1, 3 * HOUR + MIN);
        assert!(usage.iter().any(|u| u.app == "site:www.youtube.com" && u.today_ms == MIN), "{usage:?}");
    }

    fn program(e: &mut Engine, window: u64, program: Option<&str>, now: u64) {
        e.handle(Input::TerminalProgram { window, program: program.map(str::to_string), tmux_session: None }, now);
    }

    #[test]
    fn screen_time_in_terminals_goes_to_the_program() {
        let mut e = timer_engine();
        e.handle(Input::WindowChanged(WindowInfo { pid: Some(900), ..win(1, "kitty") }), 0);
        assert_eq!(e.focused_terminal(), None, "not focused");
        e.handle(Input::FocusChanged(Some(2)), 0);
        assert_eq!(e.focused_terminal(), None, "zed is no terminal");
        e.handle(Input::FocusChanged(Some(1)), 0);
        assert_eq!(e.focused_terminal().map(|w| w.id), Some(1));

        program(&mut e, 1, Some("nvim"), 0);
        tick_through(&mut e, 1000, 2 * MIN);
        program(&mut e, 1, Some("claude"), 2 * MIN);
        tick_through(&mut e, 2 * MIN + 1000, 5 * MIN);
        program(&mut e, 1, None, 5 * MIN); // back at the shell
        tick_through(&mut e, 5 * MIN + 1000, 6 * MIN);

        let usage = e.app_usage(1, 6 * MIN);
        let get = |app: &str| usage.iter().find(|u| u.app == app).map_or(0, |u| u.today_ms);
        assert_eq!((get("term:nvim"), get("term:claude"), get("kitty")), (2 * MIN, 3 * MIN, MIN));

        // Names and categories for clients.
        let snapshot = e.snapshot(6 * MIN);
        assert_eq!(snapshot.app_names.get("term:nvim").map(String::as_str), Some("Neovim"));
        assert_eq!(snapshot.app_names.get("term:claude").map(String::as_str), Some("Claude Code"));
        for key in ["kitty", "term:nvim", "term:claude"] {
            assert_eq!(snapshot.app_categories.get(key).map(String::as_str), Some("Terminal"), "{key}");
        }

        // A closed window forgets its program.
        program(&mut e, 1, Some("nvim"), 6 * MIN);
        e.handle(Input::WindowClosed(1), 6 * MIN);
        e.handle(Input::WindowChanged(WindowInfo { pid: Some(900), ..win(1, "kitty") }), 6 * MIN);
        e.handle(Input::FocusChanged(Some(1)), 6 * MIN);
        tick_through(&mut e, 6 * MIN + 1000, 7 * MIN);
        let usage = e.app_usage(1, 7 * MIN);
        assert_eq!(usage.iter().find(|u| u.app == "kitty").map(|u| u.today_ms), Some(2 * MIN));
    }

    #[test]
    fn allow_rules_can_match_the_program_in_a_terminal() {
        let config = format!(
            "{CONFIG}\n[habits.code]\ntarget = \"10m\"\nallow = [{{ app = \"kitty\", program = \"term:NVIM\" }}]\n\
             reward = {{ groups = [\"social\"], duration = \"5m\" }}\n"
        )
        .replace("[general]", "[general]\nterminal_programs = false");
        let config = Config::from_toml(&config).unwrap();
        assert!(config.has_terminal_rules());
        let mut e = Engine::new(config, State::default());
        e.handle(Input::WindowsReset(vec![WindowInfo { pid: Some(900), ..win(2, "kitty") }]), 0);
        e.handle(Input::FocusChanged(Some(2)), 0);
        // Looked up for the rule even with per-program screen time off.
        assert_eq!(e.focused_terminal().map(|w| w.id), Some(2));
        e.start("code", 0).unwrap();
        assert!(!e.snapshot(0).session.unwrap().running, "program not known yet");

        program(&mut e, 2, Some("nvim"), 0);
        tick_through(&mut e, 1000, MIN);
        let session = e.snapshot(MIN).session.unwrap();
        assert!(session.running && session.elapsed_ms == MIN, "{session:?}");

        // Another tmux pane in the same window pauses it.
        program(&mut e, 2, Some("claude"), MIN);
        tick_through(&mut e, MIN + 1000, 2 * MIN);
        let session = e.snapshot(2 * MIN).session.unwrap();
        assert!(!session.running && session.elapsed_ms == MIN, "{session:?}");
        // Screen time still goes to the terminal: terminal_programs is off.
        assert!(e.app_usage(1, 2 * MIN).iter().all(|u| !u.app.starts_with("term:")));
    }

    #[test]
    fn allow_rules_can_match_a_tmux_session() {
        let config = format!(
            "{CONFIG}\n[habits.thesis]\ntarget = \"10m\"\n\
             allow = [{{ tmux_session = \"thesis\" }}, {{ tmux_session = \"lab\", program = \"nvim\" }}]\n\
             reward = {{ groups = [\"social\"], duration = \"5m\" }}\n"
        );
        let mut e = Engine::new(Config::from_toml(&config).unwrap(), State::default());
        e.handle(Input::WindowsReset(vec![WindowInfo { pid: Some(900), ..win(2, "kitty") }]), 0);
        e.handle(Input::FocusChanged(Some(2)), 0);
        e.start("thesis", 0).unwrap();
        let front = |e: &mut Engine, program: Option<&str>, session: Option<&str>, now: u64| {
            e.handle(
                Input::TerminalProgram {
                    window: 2,
                    program: program.map(str::to_string),
                    tmux_session: session.map(str::to_string),
                },
                now,
            );
        };
        let running = |e: &Engine, now: u64| e.snapshot(now).session.unwrap().running;

        front(&mut e, None, Some("thesis"), 0); // any program, even the shell
        assert!(running(&e, 0));
        front(&mut e, Some("nvim"), Some("Thesis"), 0);
        assert!(!running(&e, 0), "session names are exact");
        front(&mut e, Some("nvim"), None, 0);
        assert!(!running(&e, 0), "outside tmux");
        front(&mut e, Some("claude"), Some("lab"), 0);
        assert!(!running(&e, 0), "lab counts only with nvim");
        front(&mut e, Some("nvim"), Some("lab"), 0);
        assert!(running(&e, 0));
        e.handle(Input::WindowClosed(2), 0);
        assert!(e.tmux_sessions.is_empty() && e.programs.is_empty());
    }

    const PASSIVE_CONFIG: &str = r#"
        [general]
        sound = "complete"

        [groups.social]
        apps = ["discord"]

        [habits.code]
        name = "Coding"
        kind = "passive"
        target = "2m"
        allow = [{ app = "zed" }, { program = "nvim" }]
        reward = { groups = ["social"], duration = "10m" }

        [habits.agents]
        kind = "passive"
        allow = [{ program = "claude" }]
    "#;

    #[test]
    fn passive_habits_count_by_themselves_and_pay_their_goal_once() {
        let mut e = Engine::new(Config::from_toml(PASSIVE_CONFIG).unwrap(), State::default());
        e.handle(Input::WindowsReset(vec![win(1, "zed"), WindowInfo { pid: Some(900), ..win(2, "kitty") }]), 0);
        assert!(e.start("code", 0).unwrap_err().contains("nothing to start"));
        assert!(e.log_done("code", 1, false, 0).is_err());

        e.handle(Input::FocusChanged(Some(1)), 0);
        tick_through(&mut e, 1000, MIN);
        e.handle(Input::Idle(true), MIN);
        tick_through(&mut e, MIN + 1000, 5 * MIN); // idle doesn't count
        e.handle(Input::Idle(false), 5 * MIN);
        let habit = |e: &Engine, id: &str, now| e.snapshot(now).habits.into_iter().find(|h| h.id == id).unwrap();
        assert_eq!(habit(&e, "code", 5 * MIN).today_focused_ms, MIN);
        assert!(e.state().session.is_none(), "no session involved");

        // nvim in the terminal counts for coding, claude for agents.
        e.handle(Input::FocusChanged(Some(2)), 5 * MIN);
        program(&mut e, 2, Some("claude"), 5 * MIN);
        tick_through(&mut e, 5 * MIN + 1000, 6 * MIN);
        program(&mut e, 2, Some("nvim"), 6 * MIN);
        let mut effects = Vec::new();
        let mut t = 6 * MIN + 1000;
        while t <= 8 * MIN {
            effects.extend(e.handle(Input::Tick, t));
            t += 1000;
        }
        let code = habit(&e, "code", 8 * MIN);
        assert_eq!((code.today_focused_ms, code.done_today, code.rounds_today), (3 * MIN, true, 1));
        assert_eq!(habit(&e, "agents", 8 * MIN).today_focused_ms, MIN);
        assert!(!habit(&e, "agents", 8 * MIN).done_today, "no target: only tracked");
        assert_eq!(e.state().credits["social"], 10 * MIN, "paid once");
        assert!(effects.iter().any(|f| matches!(f, Effect::PlaySound(_))));
        assert!(
            effects.iter().any(|f| matches!(f, Effect::Notify { body, .. } if body.contains("Coding: 2:00 today, goal reached"))),
            "{effects:?}"
        );
        // Passive time is saved with screen time, not on every tick.
        assert!(e.take_usage_dirty());
    }

    #[test]
    fn a_newer_release_shows_in_snapshots_unless_checks_are_off() {
        let update = UpdateView { version: "0.2.0".into(), url: "https://example.org".into() };
        let mut e = timer_engine();
        assert_eq!(e.snapshot(0).update, None);
        e.set_available_update(Some(update.clone()));
        assert_eq!(e.snapshot(0).update, Some(update.clone()));

        let config = TIMER_CONFIG.replace("emergency_penalty = \"1h\"", "emergency_penalty = \"1h\"\nupdate_check = false");
        e.reload(Config::from_toml(&config).unwrap(), 0);
        let snapshot = e.snapshot(0);
        assert!(snapshot.update.is_none() && !snapshot.settings.update_check);
    }

    #[test]
    fn terminal_programs_and_categories_can_be_turned_off() {
        let config = TIMER_CONFIG.replace(
            "emergency_penalty = \"1h\"",
            "emergency_penalty = \"1h\"\nterminal_programs = false\nauto_categories = false",
        );
        assert_ne!(config, TIMER_CONFIG);
        let mut e = Engine::new(Config::from_toml(&config).unwrap(), State::default());
        e.handle(Input::WindowsReset(vec![WindowInfo { pid: Some(900), ..win(1, "kitty") }]), 0);
        e.handle(Input::FocusChanged(Some(1)), 0);
        assert_eq!(e.focused_terminal(), None);
        program(&mut e, 1, Some("nvim"), 0);
        tick_through(&mut e, 1000, MIN);
        assert_eq!(e.app_usage(1, MIN)[0].app, "kitty");
        assert!(e.snapshot(MIN).app_categories.is_empty());
        let settings = e.snapshot(MIN).settings;
        assert!(!settings.terminal_programs && !settings.auto_categories);
    }

    #[test]
    fn browsers_and_sites_are_in_the_browser_category() {
        let config = TIMER_CONFIG.replace(
            "emergency_penalty = \"1h\"",
            "emergency_penalty = \"1h\"\nbrowsers = [\"zen\"]",
        ) + "\n[app_categories]\n\"site:www.youtube.com\" = \"Videos\"\n";
        let config = Config::from_toml(&config).unwrap();
        assert_eq!(config.app_category("zen"), Some("Browser"));
        assert_eq!(config.app_category("site:github.com"), Some("Browser"));
        assert_eq!(config.app_category("site:www.youtube.com"), Some("Videos"), "[app_categories] wins");
        assert_eq!(config.app_category("Alacritty"), Some("Terminal"));
        assert_eq!(config.app_category("zed"), None);
        assert_eq!(config.app_name("term:htop"), "htop");
        assert_eq!(config.app_name("term:unknown"), "term:unknown");
    }

    #[test]
    fn screen_time_is_saved_lazily_and_pruned() {
        let mut e = timer_engine();
        e.handle(Input::Tick, 0); // the first tick records the credit day
        e.take_dirty();
        e.handle(Input::FocusChanged(Some(2)), 0);
        tick_through(&mut e, 1000, 5000);
        assert!(!e.take_dirty(), "focus and screen time alone must not force a save");
        assert!(e.take_usage_dirty());

        // Only `screen_time_days` days are kept.
        let config = TIMER_CONFIG.replace("emergency_penalty = \"1h\"", "emergency_penalty = \"1h\"\nscreen_time_days = 2");
        assert_ne!(config, TIMER_CONFIG);
        let mut e = Engine::new(Config::from_toml(&config).unwrap(), State::default());
        e.handle(Input::WindowsReset(vec![win(2, "zed")]), 0);
        e.handle(Input::FocusChanged(Some(2)), 0);
        for day in 0..4 {
            tick_through(&mut e, day * DAY + 5 * HOUR, day * DAY + 5 * HOUR + 2000);
        }
        assert_eq!(e.state().app_days.len(), 2);
    }

    const SCHEDULE_CONFIG: &str = r#"
        [groups.night]
        name = "Night"
        apps = ["steam"]
        domains = ["youtube.com"]
        schedule = { days = ["mon"], ranges = ["22:00-02:00"] }

        [groups.always]
        apps = ["discord"]

        [habits.read]
        target = "10m"
        reward = { groups = ["night"], duration = "30m" }
    "#;

    /// Unix ms of a local (UTC+2) time on 1970-01-05, a Monday; `day` adds days.
    fn local_monday(day: u64, hh: u64, mm: u64) -> u64 {
        4 * DAY + day * DAY + hh * HOUR + mm * MIN - 2 * HOUR
    }

    fn schedule_engine() -> Engine {
        let mut e = Engine::new(Config::from_toml(SCHEDULE_CONFIG).unwrap(), State::default())
            .with_local_offset(|_| 2 * HOUR as i64);
        e.handle(Input::WindowsReset(vec![win(5, "steam")]), local_monday(0, 21, 0));
        e
    }

    #[test]
    fn scheduled_groups_block_only_inside_their_window() {
        let mut e = schedule_engine();
        let evening = local_monday(0, 21, 0);
        assert!(!e.is_app_blocked("steam", evening));
        assert!(e.is_app_blocked("discord", evening), "groups without a schedule always block");
        assert!(!e.blocked_domains(evening).contains("youtube.com"));
        let night = e.snapshot(evening).groups.into_iter().find(|g| g.id == "night").unwrap();
        assert!(!night.blocked && night.off_schedule && night.scheduled);
        assert_eq!(night.schedule_label.as_deref(), Some("off hours until 22:00"));
        assert_eq!(night.schedule_next_change_at_ms, local_monday(0, 22, 0));
        assert_eq!(night.schedule_summary, vec!["mon 22:00-02:00"]);

        // Seeding tick, then the warning a minute before, then the window opens.
        assert!(e.handle(Input::Tick, evening).is_empty());
        let fx = e.handle(Input::Tick, local_monday(0, 21, 59));
        assert!(matches!(&fx[..], [Effect::Notify { title, .. }] if title == "Block starting"), "{fx:?}");
        let fx = e.handle(Input::Tick, local_monday(0, 22, 0));
        assert!(fx.contains(&Effect::CloseWindow(5)), "{fx:?}");
        let kinds: Vec<_> = e.events(2).iter().map(|ev| ev.kind).collect();
        assert_eq!(kinds, vec![EventKind::BlockEnforced, EventKind::ScheduleOpened]);
        assert!(e.blocked_domains(local_monday(0, 22, 0)).contains("youtube.com"));

        // Past midnight into tuesday, until 02:00.
        let late = local_monday(1, 1, 30);
        assert!(e.is_app_blocked("steam", late));
        let night = e.snapshot(late).groups.into_iter().find(|g| g.id == "night").unwrap();
        assert_eq!(night.schedule_label.as_deref(), Some("until 02:00"));
        e.handle(Input::Tick, local_monday(1, 2, 0));
        assert_eq!(e.events(1)[0].kind, EventKind::ScheduleClosed);
        assert!(!e.is_app_blocked("steam", local_monday(1, 2, 0)));
        let label = e.snapshot(local_monday(1, 2, 0)).groups[1].schedule_label.clone();
        assert_eq!(label.as_deref(), Some("off hours until mon 22:00"));
    }

    #[test]
    fn unlocking_is_refused_off_schedule() {
        let mut e = schedule_engine();
        e.state.credits.insert("night".into(), 30 * MIN);
        let err = e.unlock("night", None, local_monday(0, 21, 0)).unwrap_err();
        assert!(err.contains("isn't blocking right now; it blocks at 22:00"), "{err}");
        assert_eq!(e.state().credits["night"], 30 * MIN, "no credit spent");
        assert!(e.unlock("night", Some(10 * MIN), local_monday(0, 23, 0)).is_ok());
        assert!(!e.is_app_blocked("steam", local_monday(0, 23, 5)));
    }

    #[test]
    fn reload_does_not_announce_every_window() {
        let mut e = schedule_engine();
        e.handle(Input::Tick, local_monday(0, 23, 0));
        let config = Config::from_toml(SCHEDULE_CONFIG).unwrap();
        e.reload(config, local_monday(0, 23, 0));
        let fx = e.handle(Input::Tick, local_monday(0, 23, 0) + 1000);
        assert!(!fx.iter().any(|f| matches!(f, Effect::Notify { title, .. } if title == "Block active")), "{fx:?}");
    }

    const MODES_CONFIG: &str = r#"
        [general]
        day_start = "04:00"
        expiry_warning = "60s"
        notifications = true

        [groups.social]
        name = "Social"
        apps = ["discord"]
        domains = ["youtube.com"]
        unlock_mode = "usage"

        [groups.shop]
        name = "Shop"
        apps = ["steam"]
        unlock_mode = "rest_of_day"
        rest_of_day_price = "1h"

        [groups.clock]
        apps = ["slack"]

        [habits.read]
        target = "10m"
        reward = { groups = ["social"], duration = "30m" }
    "#;

    fn modes_engine() -> Engine {
        let mut e = Engine::new(Config::from_toml(MODES_CONFIG).unwrap(), State::default());
        e.handle(Input::WindowsReset(vec![win(2, "zed")]), 10 * HOUR);
        e
    }

    /// Ticks once a second and collects all effects.
    fn tick_collect(e: &mut Engine, from: u64, to: u64) -> Vec<Effect> {
        let mut fx = Vec::new();
        let mut t = from;
        while t <= to {
            fx.extend(e.handle(Input::Tick, t));
            t += 1000;
        }
        fx
    }

    #[test]
    fn usage_unlocks_only_run_down_while_used() {
        let mut e = modes_engine();
        let t0 = 10 * HOUR;
        e.state.credits.insert("social".into(), 10 * MIN);
        e.unlock("social", Some(5 * MIN), t0).unwrap();
        // Discord opens once it's unlocked.
        assert!(e.handle(Input::WindowChanged(win(1, "discord")), t0).is_empty());
        let social = |e: &Engine, now| e.snapshot(now).groups.into_iter().find(|g| g.id == "social").unwrap();
        assert_eq!(social(&e, t0).unlock_label.as_deref(), Some("5:00 of use left"));
        assert_eq!(social(&e, t0).unlock_until_ms, 28 * HOUR, "unused budget ends at the next day start");

        // Working in another app doesn't use the unlock.
        e.handle(Input::FocusChanged(Some(2)), t0);
        tick_collect(&mut e, t0 + 1000, t0 + 10 * MIN);
        assert_eq!(e.state().open_unlocks["social"].usage_left_ms, 5 * MIN);

        // Two minutes in discord use two minutes.
        let t1 = t0 + 10 * MIN;
        e.handle(Input::FocusChanged(Some(1)), t1);
        tick_collect(&mut e, t1 + 1000, t1 + 2 * MIN);
        assert_eq!(e.state().open_unlocks["social"].usage_left_ms, 3 * MIN);

        // Idle time doesn't count.
        let t2 = t1 + 2 * MIN;
        e.handle(Input::Idle(true), t2);
        tick_collect(&mut e, t2 + 1000, t2 + 5 * MIN);
        assert_eq!(e.state().open_unlocks["social"].usage_left_ms, 3 * MIN);

        // Using up the rest closes discord.
        let t3 = t2 + 5 * MIN;
        e.handle(Input::Idle(false), t3);
        let fx = tick_collect(&mut e, t3 + 1000, t3 + 3 * MIN + 2000);
        assert!(fx.contains(&Effect::CloseWindow(1)), "{fx:?}");
        assert!(e.events(5).iter().any(|ev| ev.kind == EventKind::UsageExhausted));
        assert!(e.is_app_blocked("discord", t3 + 4 * MIN));
    }

    #[test]
    fn relock_refunds_per_mode() {
        let mut e = modes_engine();
        let t0 = 10 * HOUR;
        e.state.credits.insert("social".into(), 10 * MIN);
        e.unlock("social", Some(4 * MIN), t0).unwrap();
        e.relock("social", t0 + HOUR).unwrap();
        assert_eq!(e.state().credits["social"], 10 * MIN, "unused usage budget is refunded in full");

        e.state.credits.insert("shop".into(), 2 * HOUR);
        e.unlock("shop", None, t0).unwrap();
        e.relock("shop", t0 + MIN).unwrap();
        assert_eq!(e.state().credits["shop"], HOUR, "a day pass isn't refunded");
        assert!(e.events(1)[0].text.contains("isn't refunded"));
    }

    #[test]
    fn usage_unlocks_run_down_while_their_site_plays_unfocused_or_idle() {
        let mut e = modes_engine();
        let t0 = 10 * HOUR;
        e.state.credits.insert("social".into(), 10 * MIN);
        e.unlock("social", Some(5 * MIN), t0).unwrap();
        let media = |urls: &[&str]| Input::BrowserMedia {
            source: "host".into(),
            urls: urls.iter().map(|u| u.to_string()).collect(),
        };

        // A video in the background while working in another app, then
        // without any input.
        e.handle(Input::FocusChanged(Some(2)), t0);
        e.handle(media(&["https://www.youtube.com/watch?v=1"]), t0);
        tick_collect(&mut e, t0 + 1000, t0 + MIN);
        assert_eq!(e.state().open_unlocks["social"].usage_left_ms, 4 * MIN);
        let t1 = t0 + MIN;
        e.handle(Input::Idle(true), t1);
        tick_collect(&mut e, t1 + 1000, t1 + MIN);
        assert_eq!(e.state().open_unlocks["social"].usage_left_ms, 3 * MIN);

        // Sound from other sites doesn't count, nor does a paused video.
        let t2 = t1 + MIN;
        e.handle(media(&["https://open.spotify.com/"]), t2);
        tick_collect(&mut e, t2 + 1000, t2 + MIN);
        e.handle(media(&[]), t2 + MIN);
        tick_collect(&mut e, t2 + MIN + 1000, t2 + 2 * MIN);
        assert_eq!(e.state().open_unlocks["social"].usage_left_ms, 3 * MIN);

        // The browser closing forgets its tabs.
        let t3 = t2 + 2 * MIN;
        e.handle(media(&["https://youtube.com/watch?v=2"]), t3);
        e.handle(Input::BrowserTab { source: "host".into(), window: None, tab: None }, t3);
        tick_collect(&mut e, t3 + 1000, t3 + MIN);
        assert_eq!(e.state().open_unlocks["social"].usage_left_ms, 3 * MIN);
    }

    #[test]
    fn rest_of_day_unlocks_cost_the_day_pass_price() {
        let mut e = modes_engine();
        let t0 = 10 * HOUR;
        e.state.credits.insert("shop".into(), 30 * MIN);
        let err = e.unlock("shop", None, t0).unwrap_err();
        assert!(err.contains("day pass costs 1:00:00, you have 30:00"), "{err}");

        e.state.credits.insert("shop".into(), 90 * MIN);
        assert!(e.unlock("shop", Some(10 * MIN), t0).unwrap_err().contains("leave out the duration"));
        let (_, spent) = e.unlock("shop", None, t0).unwrap();
        assert_eq!(spent, HOUR);
        assert_eq!(e.state().credits["shop"], 30 * MIN);
        assert!(e.unlock("shop", None, t0 + MIN).unwrap_err().contains("already unlocked"));
        let shop = e.snapshot(t0).groups.into_iter().find(|g| g.id == "shop").unwrap();
        assert_eq!(shop.unlock_label.as_deref(), Some("until tomorrow 04:00"), "{shop:?}");
        assert_eq!(shop.rest_of_day_price_ms, HOUR);

        // The next day start locks it again.
        tick_collect(&mut e, 28 * HOUR - 1000, 28 * HOUR);
        assert!(e.is_app_blocked("steam", 28 * HOUR));
    }

    #[test]
    fn wallclock_unlocks_keep_working_as_before() {
        let mut e = modes_engine();
        e.state.credits.insert("clock".into(), 10 * MIN);
        e.unlock("clock", Some(5 * MIN), 10 * HOUR).unwrap();
        let clock = e.snapshot(10 * HOUR + MIN).groups.into_iter().find(|g| g.id == "clock").unwrap();
        assert_eq!(clock.unlock_label.as_deref(), Some("4:00 left"));
        assert!(!e.is_group_unlocked("clock", 10 * HOUR + 5 * MIN));
    }

    #[test]
    fn legacy_unlocks_migrate_and_usage_survives_restarts() {
        let old = r#"{"unlocks": {"clock": 99999999999}, "credits": {"clock": 60000}}"#;
        let e = Engine::new(Config::from_toml(MODES_CONFIG).unwrap(), State::from_json(old).unwrap());
        let unlock = &e.state().open_unlocks["clock"];
        assert_eq!((unlock.mode, unlock.until), (UnlockMode::Wallclock, 99_999_999_999));
        let json = e.state().to_json();
        assert!(!json.contains("\"unlocks\""), "legacy field isn't written back: {json}");

        // A usage budget is untouched by a restart and the downtime.
        let mut e = modes_engine();
        e.state.credits.insert("social".into(), 10 * MIN);
        e.unlock("social", Some(5 * MIN), 10 * HOUR).unwrap();
        let saved = State::from_json(&e.state().to_json()).unwrap();
        let mut e = Engine::new(Config::from_toml(MODES_CONFIG).unwrap(), saved);
        e.handle(Input::WindowsReset(vec![win(1, "discord")]), 13 * HOUR);
        e.handle(Input::FocusChanged(Some(1)), 13 * HOUR);
        e.handle(Input::Tick, 13 * HOUR);
        assert_eq!(e.state().open_unlocks["social"].usage_left_ms, 5 * MIN);
    }

    #[test]
    fn unspent_credit_expires_when_the_day_starts() {
        let mut e = modes_engine();
        e.handle(Input::Tick, 10 * HOUR); // records the credit day
        e.state.credits.insert("social".into(), HOUR);
        assert!(tick_collect(&mut e, 28 * HOUR - 2000, 28 * HOUR - 1000).is_empty());
        assert_eq!(e.state().credits["social"], HOUR);
        let fx = e.handle(Input::Tick, 28 * HOUR);
        assert!(matches!(&fx[..], [Effect::Notify { title, body }] if title == "Credit expired" && body.contains("1:00:00")), "{fx:?}");
        assert!(e.state().credits.is_empty());
        assert_eq!(e.events(1)[0].kind, EventKind::CreditExpired);
        assert_eq!(e.snapshot(28 * HOUR).credit_expires_at_ms, 52 * HOUR);

        // State from before expiry existed keeps its credit on the first tick.
        let old = r#"{"credits": {"social": 3600000}}"#;
        let mut e = Engine::new(Config::from_toml(MODES_CONFIG).unwrap(), State::from_json(old).unwrap());
        e.handle(Input::Tick, 100 * HOUR);
        assert_eq!(e.state().credits["social"], HOUR);
    }

    #[test]
    fn completions_build_streaks_across_days() {
        let mut e = engine();
        e.handle(Input::FocusChanged(Some(1)), 0);
        for day in 0..3 {
            let start = day * DAY + 60 * MIN;
            e.start("reading", start).unwrap();
            e.handle(Input::FocusChanged(Some(1)), start);
            let fx = e.handle(Input::Tick, start + 10 * MIN);
            if day == 2 {
                assert!(
                    fx.iter().any(|f| matches!(f, Effect::Notify { body, .. } if body.contains("3-day streak"))),
                    "{fx:?}"
                );
            }
        }
        let habit = |snapshot: &Snapshot| snapshot.habits.iter().find(|h| h.id == "reading").cloned().unwrap();
        let today = habit(&e.snapshot(2 * DAY + 2 * 60 * MIN));
        assert_eq!((today.streak_days, today.best_streak_days, today.done_today), (3, 3, true));
        assert_eq!(today.today_focused_ms, 10 * MIN);
        assert_eq!(e.snapshot(2 * DAY + 2 * 60 * MIN).streak_days, 3);

        // Next day, not done yet: still alive. The day after: broken.
        assert_eq!(habit(&e.snapshot(3 * DAY + MIN)).streak_days, 3);
        assert!(!habit(&e.snapshot(3 * DAY + MIN)).done_today);
        assert_eq!(habit(&e.snapshot(4 * DAY + MIN)).streak_days, 0);
        assert_eq!(habit(&e.snapshot(4 * DAY + MIN)).best_streak_days, 3);

        let days = e.day_stats(5, 4 * DAY);
        assert_eq!(days.len(), 5);
        assert_eq!(days[0].date, "1970-01-01");
        assert_eq!(days[2].habits["reading"].completions, 1);
        assert!(days[4].habits.is_empty());
    }

    #[test]
    fn stats_are_backfilled_from_history() {
        let mut state = State::default();
        state.history.push(HistoryEntry {
            habit: "reading".into(),
            started_at: 0,
            finished_at: DAY + MIN,
            focused_ms: 10 * MIN,
            outcome: Outcome::Completed,
        });
        let mut e = Engine::new(Config::from_toml(CONFIG).unwrap(), state);
        e.backfill_day_stats();
        assert_eq!(e.state().days[&1]["reading"].completions, 1);
        // Only once: existing stats aren't duplicated.
        e.backfill_day_stats();
        assert_eq!(e.state().days[&1]["reading"].completions, 1);
    }

    #[test]
    fn restored_session_does_not_count_downtime() {
        let mut e = engine();
        e.start("reading", 0).unwrap();
        e.handle(Input::FocusChanged(Some(1)), 0);
        e.handle(Input::Tick, 3 * MIN);
        let saved = State::from_json(&e.state().to_json()).unwrap();

        let mut e = Engine::new(Config::from_toml(CONFIG).unwrap(), saved);
        e.handle(Input::WindowsReset(vec![win(1, "zathura")]), 100 * MIN);
        e.handle(Input::FocusChanged(Some(1)), 100 * MIN);
        assert_eq!(e.snapshot(100 * MIN).session.unwrap().elapsed_ms, 3 * MIN);
    }

    const LOGGED_CONFIG: &str = r#"
        [general]
        day_start = "04:00"
        notifications = true

        [groups.social]
        name = "Social"
        apps = ["discord"]
        requires = ["walk", "anki"]
        require = "all"

        [groups.games]
        apps = ["steam"]
        requires = ["walk", "anki"]

        [habits.walk]
        name = "Walk"
        kind = "manual"
        reward = { groups = ["games"], duration = "20m", mode = "immediate" }

        [habits.anki]
        name = "Anki"
        kind = "counter"
        goal = 20
        unit = "cards"
        daily_limit = 2
        reward = { groups = ["social"], duration = "30m" }
    "#;

    fn logged_engine() -> Engine {
        let mut e = Engine::new(Config::from_toml(LOGGED_CONFIG).unwrap(), State::default());
        e.handle(Input::WindowsReset(vec![]), 10 * HOUR);
        e
    }

    #[test]
    fn counters_bank_one_reward_per_goal_up_to_the_daily_limit() {
        let mut e = logged_engine();
        let t = 10 * HOUR;
        let (fx, msg) = e.log_done("anki", 15, false, t).unwrap();
        assert!(fx.is_empty());
        assert_eq!(msg, "Anki: 15 / 20 cards today");
        assert_eq!(e.state().credits.get("social").copied().unwrap_or(0), 0);

        let (fx, msg) = e.log_done("anki", 30, false, t).unwrap();
        assert_eq!(fx.len(), 2, "two rounds done: {fx:?}");
        assert!(msg.starts_with("Anki done. Earned 30:00 for Social") && msg.ends_with("(2 rounds)"), "{msg}");
        assert_eq!(e.state().credits["social"], 60 * MIN);

        // The daily limit caps further rounds, and corrections never take credit back.
        let (fx, msg) = e.log_done("anki", 40, false, t).unwrap();
        assert!(fx.is_empty());
        assert!(msg.contains("daily limit of 2 reached"), "{msg}");
        e.log_done("anki", -80, false, t).unwrap();
        assert_eq!(e.state().credits["social"], 60 * MIN);
        let (_, msg) = e.log_done("anki", 3, true, t).unwrap();
        assert_eq!(msg, "Anki: 3 cards today (daily limit of 2 reached)");
        e.log_done("anki", -10, false, t).unwrap();
        let view = e.snapshot(t).habits.into_iter().find(|h| h.id == "anki").unwrap();
        assert_eq!((view.count_today, view.rounds_today, view.goal), (0, 2, 20));
        assert_eq!(view.unit, "cards");

        // A new day starts from zero.
        let tomorrow = 34 * HOUR;
        let (_, msg) = e.log_done("anki", 1, false, tomorrow).unwrap();
        assert_eq!(msg, "Anki: 1 / 20 cards today");
    }

    #[test]
    fn manual_habits_complete_once_and_build_streaks() {
        let mut e = logged_engine();
        assert!(e.start("walk", 10 * HOUR).unwrap_err().contains("hf done walk"));
        assert!(e.log_done("nope", 1, false, 10 * HOUR).is_err());
        e.log_done("walk", 1, false, 10 * HOUR).unwrap();
        let (_, msg) = e.log_done("walk", 1, false, 34 * HOUR).unwrap();
        assert!(msg.contains("2-day streak"), "{msg}");
        let (fx, msg) = e.log_done("walk", 1, false, 34 * HOUR).unwrap();
        assert!(fx.is_empty(), "manual habits pay once a day by default");
        assert_eq!(msg, "Walk is already done today");
    }

    #[test]
    fn unlocking_waits_for_required_habits() {
        let mut e = logged_engine();
        let t = 10 * HOUR;
        e.state.credits.insert("social".into(), 60 * MIN);
        e.state.credits.insert("games".into(), 60 * MIN);
        let err = e.unlock("social", Some(10 * MIN), t).unwrap_err();
        assert_eq!(err, "Social needs Walk and Anki done today (0 of 2 done; missing Walk, Anki)");
        assert!(e.unlock("games", Some(10 * MIN), t).unwrap_err().contains("needs one of Walk, Anki"));

        let group = |e: &Engine, id: &str| e.snapshot(t).groups.into_iter().find(|g| g.id == id).unwrap();
        e.log_done("anki", 10, false, t).unwrap();
        let social = group(&e, "social");
        assert!(social.require_all && !social.requirements_met);
        assert_eq!(social.requires[1].progress, 0.5);

        // Walk alone meets games' "any" requirement, so its immediate reward opens it.
        let (_, msg) = e.log_done("walk", 1, false, t).unwrap();
        assert!(msg.contains("games unlocked for 20:00"), "{msg}");
        assert!(e.unlock("social", Some(10 * MIN), t).unwrap_err().contains("1 of 2 done; missing Anki"));
        e.log_done("anki", 10, false, t).unwrap();
        assert!(group(&e, "social").requirements_met);
        e.unlock("social", Some(10 * MIN), t).unwrap();
    }

    #[test]
    fn immediate_rewards_are_banked_while_gated() {
        let config = LOGGED_CONFIG.replace(
            r#"reward = { groups = ["social"], duration = "30m" }"#,
            r#"reward = { groups = ["social", "games"], duration = "30m", mode = "immediate" }"#,
        );
        let mut e = Engine::new(Config::from_toml(&config).unwrap(), State::default());
        let t = 10 * HOUR;
        // Anki's round meets the games requirement (any) but not social's (all).
        let (_, msg) = e.log_done("anki", 20, false, t).unwrap();
        assert!(msg.contains("games unlocked for 30:00; earned 30:00 for Social"), "{msg}");
        assert!(e.is_group_unlocked("games", t));
        assert!(!e.is_group_unlocked("social", t));
        assert_eq!(e.state().credits["social"], 30 * MIN);
    }

    fn archived_visits(e: &mut Engine) -> Vec<(String, u64, u64, u64)> {
        e.take_archive()
            .into_iter()
            .filter_map(|r| match r {
                Record::Visit(v) => Some((v.key, v.start, v.end, v.active_ms)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn visits_are_archived_per_focus_stretch() {
        let mut e = timer_engine();
        e.take_archive();
        e.handle(Input::FocusChanged(Some(2)), 0); // zed
        tick_through(&mut e, 1000, 10 * MIN);
        e.handle(Input::FocusChanged(Some(1)), 10 * MIN); // kitty
        tick_through(&mut e, 10 * MIN + 1000, 12 * MIN);
        e.handle(Input::Idle(true), 12 * MIN);
        tick_through(&mut e, 12 * MIN + 1000, 13 * MIN);
        e.handle(Input::Idle(false), 13 * MIN);
        tick_through(&mut e, 13 * MIN + 1000, 14 * MIN);
        // Passing through zed for half a second isn't a visit.
        e.handle(Input::FocusChanged(Some(2)), 14 * MIN);
        e.handle(Input::FocusChanged(Some(1)), 14 * MIN + 500);
        tick_through(&mut e, 14 * MIN + 1500, 15 * MIN);
        e.flush_usage(15 * MIN);

        let visits = archived_visits(&mut e);
        assert_eq!(
            visits,
            [
                ("zed".into(), 0, 10 * MIN, 10 * MIN),
                ("kitty".into(), 10 * MIN, 12 * MIN, 2 * MIN),
                ("kitty".into(), 13 * MIN, 14 * MIN, MIN),
                ("kitty".into(), 14 * MIN + 500, 15 * MIN, MIN - 500),
            ]
        );
    }

    #[test]
    fn app_insights_count_sessions_and_hours() {
        const DAY: u64 = 24 * HOUR;
        let mut e = engine().with_local_offset(|_| 2 * HOUR as i64);
        let now = DAY + 12 * HOUR; // local 14:00 on day 1
        let key = "site:x.com";
        for day in [0, 1] {
            stats::record_app(&mut e.state.app_days, day, key, 30 * MIN);
        }
        let visit = |start: u64, minutes: u64| Visit {
            key: key.into(),
            start,
            end: start + minutes * MIN,
            active_ms: minutes * MIN,
            habit: None,
        };
        let visits = [
            visit(8 * HOUR, 5),                           // yesterday, local 10:00
            visit(DAY + 8 * HOUR, 30),                    // local 10:00
            visit(DAY + 8 * HOUR + 30 * MIN + 30_000, 10), // 30 s later: the same session
            visit(DAY + 9 * HOUR + 50 * MIN, 20),         // local 11:50 to 12:10
        ];
        let usage = e.app_insights(2, &visits, now);
        let x = usage.iter().find(|u| u.app == key).unwrap();
        assert_eq!((x.sessions, x.sessions_today), (3, 2));
        assert_eq!(x.longest_session_ms, 40 * MIN);
        assert_eq!(x.avg_session_ms, 65 * MIN / 3);
        assert_eq!(x.hours[10], 45 * MIN);
        assert_eq!((x.hours[11], x.hours[12]), (10 * MIN, 10 * MIN));
        assert_eq!(x.hours.iter().sum::<u64>(), 65 * MIN);
        // Today alone leaves out yesterday's five minutes.
        assert_eq!(x.hours_today[10], 40 * MIN);
        assert_eq!(x.hours_today.iter().sum::<u64>(), 60 * MIN);
        // Per day, oldest first.
        assert_eq!(x.days_ms, [30 * MIN, 30 * MIN]);
        assert_eq!(x.days_sessions, [1, 2]);

        // One day only covers today.
        let x = e.app_insights(1, &visits, now).into_iter().find(|u| u.app == key).unwrap();
        assert_eq!((x.sessions, x.hours[10]), (2, 40 * MIN));
    }

    #[test]
    fn visits_and_hours_count_towards_the_running_habit() {
        let mut e = engine();
        e.take_archive();
        e.handle(Input::FocusChanged(Some(1)), 0); // zathura, allowed for reading
        e.start("reading", 0).unwrap();
        tick_through(&mut e, 1000, 3 * MIN);
        e.stop(3 * MIN).unwrap();
        tick_through(&mut e, 3 * MIN + 1000, 5 * MIN);
        e.flush_usage(5 * MIN);

        let visits: Vec<Visit> = e
            .take_archive()
            .into_iter()
            .filter_map(|r| match r {
                Record::Visit(v) => Some(v),
                _ => None,
            })
            .collect();
        // The same window, split where the session ended.
        assert_eq!(
            visits.iter().map(|v| (v.key.as_str(), v.habit.as_deref(), v.active_ms)).collect::<Vec<_>>(),
            [("zathura", Some("reading"), 3 * MIN), ("zathura", None, 2 * MIN)]
        );

        let breakdown = e.day_breakdown(&visits, 0, 5 * MIN, false);
        let slices: Vec<(u8, &str, &str, u64, bool)> =
            breakdown.slices.iter().map(|s| (s.hour, s.label.as_str(), s.key.as_str(), s.ms, s.habit)).collect();
        // Both slices keep the app they happened in, so one app can be picked
        // out even when its time reads as a habit.
        assert_eq!(
            slices,
            [(0, "reading", "zathura", 3 * MIN, true), (0, "zathura", "zathura", 2 * MIN, false)]
        );
        assert_eq!((breakdown.offset, breakdown.more_before), (0, false));
        assert_eq!(breakdown.slices, e.period_breakdown(&visits, 1, 5 * MIN).slices, "one day covers the same");
    }

    #[test]
    fn breakdowns_label_by_category_and_step_through_days() {
        const DAY: u64 = 24 * HOUR;
        let config = CONFIG.to_string() + "\n[app_categories]\nzathura = \"Reading\"\n";
        let e = Engine::new(Config::from_toml(&config).unwrap(), State::default())
            .with_local_offset(|_| 2 * HOUR as i64);
        let now = DAY + 12 * HOUR;
        let visit = |key: &str, start: u64, minutes: u64, habit: Option<&str>| Visit {
            key: key.into(),
            start,
            end: start + minutes * MIN,
            active_ms: minutes * MIN,
            habit: habit.map(str::to_string),
        };
        let visits = [
            visit("zathura", DAY + 9 * HOUR + 50 * MIN, 20, None), // local 11:50-12:10
            visit("zed", DAY + 8 * HOUR, 30, Some("deep")),        // local 10:00
            visit("zed", 8 * HOUR, 30, None),                      // yesterday
        ];
        let labelled = |b: &Breakdown| -> Vec<(u8, String, u64)> {
            b.slices.iter().map(|s| (s.hour, s.label.clone(), s.ms)).collect()
        };
        let today = e.day_breakdown(&visits, 0, now, true);
        assert_eq!(
            labelled(&today),
            [(10, "deep".into(), 30 * MIN), (11, "Reading".into(), 10 * MIN), (12, "Reading".into(), 10 * MIN)]
        );
        assert_eq!((today.date.as_str(), today.more_before), ("1970-01-02", true));

        // A step back shows yesterday alone.
        let yesterday = e.day_breakdown(&visits, 1, now, false);
        assert_eq!(labelled(&yesterday), [(10, "zed".into(), 30 * MIN)]);
        assert_eq!((yesterday.date.as_str(), yesterday.offset), ("1970-01-01", 1));
        assert!(e.day_breakdown(&visits, 2, now, false).slices.is_empty(), "nothing that far back");

        // The period covers both days.
        let period = e.period_breakdown(&visits, 2, now);
        assert_eq!(period.slices.iter().filter(|s| s.label == "zed").count(), 1);
        assert_eq!(period.slices.iter().map(|s| s.ms).sum::<u64>(), 80 * MIN);
    }

    #[test]
    fn app_insights_include_the_visit_in_progress() {
        let mut e = timer_engine();
        e.handle(Input::FocusChanged(Some(1)), 0);
        tick_through(&mut e, 1000, 3 * MIN);
        let kitty = e.app_insights(1, &[], 3 * MIN).into_iter().find(|u| u.app == "kitty").unwrap();
        assert_eq!((kitty.sessions_today, kitty.avg_session_ms), (1, 3 * MIN));
        let timeline = e.day_timeline(&[], &[], 0, 3 * MIN, false);
        let in_progress = timeline.visits.iter().find(|v| v.key == "kitty").unwrap();
        assert_eq!((in_progress.start_ms, in_progress.end_ms), (0, 3 * MIN));
    }

    #[test]
    fn timelines_label_visits_and_cut_them_to_the_day() {
        const DAY: u64 = 24 * HOUR;
        let config = CONFIG.to_string() + "\n[app_categories]\nzathura = \"Reading\"\n";
        let e = Engine::new(Config::from_toml(&config).unwrap(), State::default());
        let now = DAY + 12 * HOUR;
        let visit = |key: &str, start: u64, end: u64, habit: Option<&str>| Visit {
            key: key.into(),
            start,
            end,
            active_ms: end - start,
            habit: habit.map(str::to_string),
        };
        let visits = [
            visit("zed", DAY - 30 * MIN, DAY + 30 * MIN, None), // across midnight
            visit("zathura", DAY + HOUR, DAY + 2 * HOUR, Some("reading")),
        ];
        let today = e.day_timeline(&visits, &[], 0, now, true);
        assert_eq!((today.from_ms, today.to_ms, today.date.as_str()), (DAY, 2 * DAY, "1970-01-02"));
        let zed = &today.visits[0];
        assert_eq!((zed.start_ms, zed.end_ms, zed.active_ms), (DAY, DAY + 30 * MIN, 30 * MIN));
        let zathura = &today.visits[1];
        assert_eq!((zathura.category.as_deref(), zathura.habit.as_deref()), (Some("Reading"), Some("reading")));

        let yesterday = e.day_timeline(&visits, &[], 1, now, false);
        assert_eq!(yesterday.visits.len(), 1);
        assert_eq!(yesterday.visits[0].end_ms, DAY);
    }

    fn afk_records(e: &mut Engine) -> Vec<Afk> {
        e.take_archive()
            .into_iter()
            .filter_map(|r| match r {
                Record::Afk(a) => Some(a),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_timer_counting_through_idle_is_not_afk() {
        let mut e = timer_engine();
        e.handle(Input::FocusChanged(Some(1)), 0);
        e.start("book", 0).unwrap();
        heartbeat(&mut e, 0, MIN);
        e.handle(Input::Idle(true), MIN); // reading: no input
        heartbeat(&mut e, MIN + 2000, 10 * MIN);
        let timeline = e.day_timeline(&[], &[], 0, 10 * MIN, false);
        assert!(timeline.afk.is_empty(), "{:?}", timeline.afk);
        let reading = &timeline.visits[0];
        assert_eq!((reading.start_ms, reading.end_ms, reading.habit.as_deref()), (0, 10 * MIN, Some("Book")));

        // Looking away from the timer while still idle is time away again.
        e.handle(Input::FocusChanged(Some(2)), 10 * MIN);
        timer_focus(&mut e, false, 10 * MIN);
        tick_through(&mut e, 10 * MIN + 1000, 12 * MIN);
        let afk = e.day_timeline(&[], &[], 0, 12 * MIN, false).afk;
        assert_eq!(afk, [TimelineAfk { start_ms: 10 * MIN, end_ms: 12 * MIN, reason: AfkReason::Idle }]);
    }

    #[test]
    fn idle_stretches_are_archived_as_afk() {
        let mut e = timer_engine();
        e.handle(Input::FocusChanged(Some(1)), 0);
        tick_through(&mut e, 1000, MIN);
        e.handle(Input::Idle(true), MIN);
        tick_through(&mut e, MIN + 1000, 4 * MIN);
        assert!(afk_records(&mut e).is_empty(), "still away");
        // The timeline shows the stretch in progress.
        let afk = e.day_timeline(&[], &[], 0, 4 * MIN, false).afk;
        assert_eq!(afk, [TimelineAfk { start_ms: MIN, end_ms: 4 * MIN, reason: AfkReason::Idle }]);

        e.handle(Input::Idle(false), 5 * MIN);
        assert_eq!(afk_records(&mut e), [Afk { start: MIN, end: 5 * MIN, reason: AfkReason::Idle }]);
        assert!(e.day_timeline(&[], &[], 0, 5 * MIN, false).afk.is_empty(), "archived, no longer in progress");

        // Shutting down while away closes the stretch.
        tick_through(&mut e, 5 * MIN + 1000, 6 * MIN);
        e.handle(Input::Idle(true), 6 * MIN);
        e.flush_usage(8 * MIN);
        assert_eq!(afk_records(&mut e), [Afk { start: 6 * MIN, end: 8 * MIN, reason: AfkReason::Idle }]);
    }

    #[test]
    fn a_suspend_is_asleep_and_ends_the_visit() {
        let mut e = timer_engine();
        e.handle(Input::FocusChanged(Some(1)), 0);
        tick_through(&mut e, 1000, MIN);
        e.take_archive();
        // No ticks for an hour: the machine slept.
        e.handle(Input::Tick, HOUR + MIN);
        let records = e.take_archive();
        assert!(records.contains(&Record::Afk(Afk { start: MIN, end: HOUR + MIN, reason: AfkReason::Asleep })));
        assert!(
            records.iter().any(|r| matches!(r, Record::Visit(v) if v.key == "kitty" && v.end == MIN)),
            "the visit before ends where the sleep began: {records:?}"
        );
        // A short stall is neither.
        tick_through(&mut e, HOUR + MIN + 1000, HOUR + 2 * MIN);
        e.handle(Input::Tick, HOUR + 2 * MIN + 10_000);
        assert!(afk_records(&mut e).is_empty());
    }
}
