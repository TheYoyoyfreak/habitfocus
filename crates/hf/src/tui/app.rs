//! TUI state and key handling, kept free of terminal I/O for testing.

use super::editor::{self, Editor, Section};
use habit_core::config::Strictness;
use habit_core::duration::parse_duration;
use habit_core::snapshot::{AppUsageView, Breakdown, DayView, GroupView, HabitView, SessionView};
use habit_core::state::Event;
use habit_core::Snapshot;
use habit_ipc::{Request, Response};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::{Duration, Instant};

const FLASH_DURATION: Duration = Duration::from_secs(5);
/// Longest text a prompt takes (app names are the longest).
const MAX_INPUT: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Today,
    Blocks,
    Habits,
    Insights,
    Lock,
}

impl Pane {
    pub const ALL: [Pane; 5] = [Pane::Today, Pane::Blocks, Pane::Habits, Pane::Insights, Pane::Lock];

    pub fn title(self) -> &'static str {
        match self {
            Pane::Today => "Today",
            Pane::Blocks => "Blocks",
            Pane::Habits => "Habits",
            Pane::Insights => "Insights",
            Pane::Lock => "Lock",
        }
    }

    fn cycle(self, delta: isize) -> Pane {
        let index = Pane::ALL.iter().position(|&p| p == self).unwrap_or(0) as isize;
        Pane::ALL[(index + delta).rem_euclid(Pane::ALL.len() as isize) as usize]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Popup {
    Unlock { group: String, input: String },
    ConfirmAbort { emergency: bool },
    /// Opens on its own when a session reaches its goal.
    GoalReached,
    Settings { index: usize, editing: Option<String> },
    /// Duration to start or extend the commitment lock.
    Lock { input: String },
    /// Amount to log for a manual/counter habit; "=N" sets today's count.
    Count { habit: String, input: String },
    /// Delete a block or habit from the config.
    ConfirmDelete { section: Section, id: String },
    /// Display name for an app id or `site:` key; empty shows the id again.
    RenameApp { app: String, input: String },
    /// Category of an app id or `site:` key; empty takes it out.
    SetCategory { app: String, input: String },
}

#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    Quit,
    Send(Request),
    /// Refetch the log, stats and screen time (e.g. another day's hours).
    Refresh,
    /// Fetch the config entries and open the form for `id` (a new entry
    /// without one).
    OpenEditor { section: Section, id: Option<String> },
}

pub struct Setting {
    pub key: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
}

pub const SETTINGS: &[Setting] = &[
    Setting { key: "day_start", label: "New day starts at", hint: "HH:MM; saved progress resets then" },
    Setting { key: "idle_timeout", label: "Pause app habits when idle for", hint: "e.g. 90s, 5m (restart habitd)" },
    Setting { key: "emergency_penalty", label: "Emergency abort penalty", hint: "e.g. 30m, 1h" },
    Setting { key: "expiry_warning", label: "Warn before relocking", hint: "e.g. 1m" },
    Setting { key: "sound", label: "Sound when a habit is done", hint: "a sound name like complete, a file, or empty" },
    Setting { key: "notifications", label: "Notifications", hint: "enter toggles" },
    Setting { key: "terminal_programs", label: "Screen time per terminal program", hint: "nvim, claude… instead of the terminal; enter toggles" },
    Setting { key: "auto_categories", label: "Group terminals and browsers", hint: "categories Terminal and Browser; enter toggles" },
    Setting { key: "update_check", label: "Check for new versions", hint: "once a day on GitHub; enter toggles" },
];

/// Settings that are on or off: enter toggles them.
const SWITCHES: &[&str] = &["notifications", "terminal_programs", "auto_categories", "update_check"];

fn on_off(on: bool) -> String {
    if on { "on" } else { "off" }.to_string()
}

pub fn setting_value(snapshot: &Snapshot, key: &str) -> String {
    let s = &snapshot.settings;
    match key {
        "day_start" => s.day_start.clone(),
        "idle_timeout" => s.idle_timeout.clone(),
        "emergency_penalty" => s.emergency_penalty.clone(),
        "expiry_warning" => s.expiry_warning.clone(),
        "notifications" => on_off(s.notifications),
        "terminal_programs" => on_off(s.terminal_programs),
        "auto_categories" => on_off(s.auto_categories),
        "update_check" => on_off(s.update_check),
        "sound" => s.sound.clone(),
        _ => String::new(),
    }
}

pub struct Flash {
    pub text: String,
    pub error: bool,
    at: Instant,
}

pub struct App {
    pub snapshot: Option<Snapshot>,
    pub connected: bool,
    /// Daily totals for streaks and the heatmap, oldest first.
    pub stats: Vec<DayView>,
    /// Activity log, newest first.
    pub events: Vec<Event>,
    /// Screen time over the last week, most used first.
    pub apps: Vec<AppUsageView>,
    /// Hours of the chosen day, by habit, category or app.
    pub breakdown_day: Breakdown,
    /// The same over the whole period.
    pub breakdown_period: Breakdown,
    /// Days back the hourly chart shows (0 = today).
    pub chart_day: u32,
    /// The hourly chart shows the whole period instead of one day.
    pub chart_period: bool,
    pub pane: Pane,
    pub habit_index: usize,
    pub group_index: usize,
    /// Row of the Insights table: 0 is the total, then `insight_rows()`.
    pub app_index: usize,
    /// Insights filter (btop-style): matches app ids, names and categories.
    pub insights_filter: String,
    /// The filter is being typed.
    pub filtering: bool,
    /// Insights shows categories instead of apps.
    pub by_category: bool,
    pub popup: Option<Popup>,
    /// The block or habit form, when open.
    pub editor: Option<Editor>,
    /// Why the timer can't track focus in this terminal, if it can't.
    pub timer_warning: Option<String>,
    /// How this terminal shows pictures (the sundial); `None` draws text only.
    pub picker: Option<ratatui_image::picker::Picker>,
    /// The last sundial picture and what it was drawn from, since drawing
    /// happens every frame but the dial rarely changes.
    pub dial: std::cell::RefCell<Option<(u64, ratatui_image::protocol::Protocol)>>,
    /// (habit, goal) whose goal-reached prompt was dismissed.
    dismissed_goal: Option<(String, u64)>,
    /// `events_seq` of the last snapshot, to refetch the log when it moves.
    events_seq: Option<u64>,
    flash: Option<Flash>,
}

impl App {
    pub fn new() -> Self {
        Self {
            snapshot: None,
            connected: false,
            stats: Vec::new(),
            events: Vec::new(),
            apps: Vec::new(),
            breakdown_day: Breakdown::default(),
            breakdown_period: Breakdown::default(),
            chart_day: 0,
            chart_period: false,
            pane: Pane::Today,
            habit_index: 0,
            group_index: 0,
            app_index: 0,
            insights_filter: String::new(),
            filtering: false,
            by_category: false,
            popup: None,
            editor: None,
            timer_warning: None,
            picker: None,
            dial: std::cell::RefCell::new(None),
            dismissed_goal: None,
            events_seq: None,
            flash: None,
        }
    }

    /// Stores a new snapshot. Returns true when something was logged since the
    /// previous one (or it's the first), so the caller should refetch the log,
    /// stats and screen time.
    pub fn set_snapshot(&mut self, snapshot: Snapshot) -> bool {
        let changed = self.events_seq.replace(snapshot.events_seq) != Some(snapshot.events_seq);
        self.habit_index = self.habit_index.min(snapshot.habits.len().saturating_sub(1));
        self.group_index = self.group_index.min(snapshot.groups.len().saturating_sub(1));

        let awaiting = snapshot
            .session
            .as_ref()
            .filter(|s| s.awaiting_decision)
            .map(|s| (s.habit.clone(), s.target_ms));
        match (&awaiting, &self.popup) {
            (Some(goal), None) if self.dismissed_goal.as_ref() != Some(goal) => {
                self.popup = Some(Popup::GoalReached);
            }
            (None, Some(Popup::GoalReached)) => self.popup = None,
            _ => {}
        }

        self.snapshot = Some(snapshot);
        self.connected = true;
        changed
    }

    pub fn flash(&self) -> Option<&Flash> {
        self.flash.as_ref().filter(|f| f.at.elapsed() < FLASH_DURATION)
    }

    pub fn set_flash(&mut self, text: impl Into<String>, error: bool) {
        self.flash = Some(Flash { text: text.into(), error, at: Instant::now() });
    }

    pub fn apply_response(&mut self, result: Result<Response, String>) {
        // A saving form closes on success and shows the error otherwise.
        if let Some(form) = self.editor.as_mut().filter(|e| e.saving) {
            form.saving = false;
            match &result {
                Ok(response) if response.ok => self.editor = None,
                Ok(response) => {
                    form.error = Some(response.error.clone().unwrap_or_else(|| "request failed".into()));
                    return;
                }
                Err(e) => {
                    form.error = Some(e.lines().next().unwrap_or_default().to_string());
                    return;
                }
            }
        }
        match result {
            Ok(response) if response.ok => {
                if let Some(message) = response.message {
                    self.set_flash(message, false);
                }
                if let Some(snapshot) = response.snapshot {
                    self.set_snapshot(snapshot);
                }
            }
            Ok(response) => self.set_flash(response.error.unwrap_or_else(|| "request failed".into()), true),
            Err(e) => self.set_flash(e.lines().next().unwrap_or_default().to_string(), true),
        }
    }

    /// Whether the current view is a detour that shouldn't count as timer
    /// time, even while the terminal is focused.
    pub fn pauses_timer(&self) -> bool {
        matches!(self.popup, Some(Popup::Settings { .. })) || self.editor.is_some()
    }

    /// Opens the form on `entries` (the `config_entries` answer).
    pub fn open_editor(&mut self, section: Section, id: Option<String>, entries: &habit_ipc::ConfigEntries) {
        let Some(snapshot) = &self.snapshot else { return };
        let tables = match section {
            Section::Groups => &entries.groups,
            Section::Habits => &entries.habits,
        };
        let table = match &id {
            Some(id) => match tables.get(id).and_then(|t| t.as_object()) {
                Some(table) => table.clone(),
                None => {
                    self.set_flash(format!("{id} isn't in the config file"), true);
                    return;
                }
            },
            None => serde_json::Map::new(),
        };
        let context = editor::Context {
            habits: snapshot.habits.iter().map(|h| (h.id.clone(), h.name.clone())).collect(),
            groups: snapshot.groups.iter().map(|g| (g.id.clone(), g.name.clone())).collect(),
            recent: self.apps.iter().map(|a| (a.app.clone(), a.total_ms / 7)).collect(),
            schedule: id
                .as_ref()
                .and_then(|id| snapshot.groups.iter().find(|g| &g.id == id))
                .map(|g| g.schedule_summary.clone())
                .unwrap_or_default(),
            app_names: snapshot.app_names.clone(),
            tmux_sessions: Vec::new(),
        };
        self.editor = Some(Editor::new(section, id, table, context));
    }

    pub fn session(&self) -> Option<&SessionView> {
        self.snapshot.as_ref()?.session.as_ref()
    }

    pub fn selected_habit(&self) -> Option<&HabitView> {
        self.snapshot.as_ref()?.habits.get(self.habit_index)
    }

    pub fn selected_group(&self) -> Option<&GroupView> {
        self.snapshot.as_ref()?.groups.get(self.group_index)
    }

    /// The rows of the Insights table below its total: apps and sites (or
    /// categories) matching the filter.
    pub fn insight_rows(&self) -> Vec<crate::stats_view::UsageRow> {
        let Some(snapshot) = &self.snapshot else { return Vec::new() };
        crate::stats_view::usage_rows(
            &self.apps,
            &snapshot.app_names,
            &snapshot.app_categories,
            &self.insights_filter,
            self.by_category,
        )
    }

    /// Category names in use, for the category prompt.
    pub fn categories(&self) -> Vec<String> {
        let mut names: Vec<String> =
            self.snapshot.iter().flat_map(|s| s.app_categories.values().cloned()).collect();
        names.sort();
        names.dedup();
        names
    }

    fn move_selection(&mut self, delta: isize) {
        let insight_rows = if self.pane == Pane::Insights { self.insight_rows().len() } else { 0 };
        let Some(snapshot) = &self.snapshot else { return };
        let (index, len) = match self.pane {
            Pane::Today | Pane::Habits => (&mut self.habit_index, snapshot.habits.len()),
            Pane::Blocks => (&mut self.group_index, snapshot.groups.len()),
            Pane::Insights => (&mut self.app_index, insight_rows + 1),
            Pane::Lock => return,
        };
        if len > 0 {
            *index = (*index as isize + delta).clamp(0, len as isize - 1) as usize;
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        if let Some(form) = &mut self.editor {
            return match form.handle_key(key) {
                editor::Outcome::None => Action::None,
                editor::Outcome::Close => {
                    self.editor = None;
                    Action::None
                }
                editor::Outcome::Send(request) => Action::Send(request),
            };
        }
        match self.popup.take() {
            Some(popup) => self.handle_popup_key(popup, key),
            None => self.handle_main_key(key),
        }
    }

    fn handle_popup_key(&mut self, popup: Popup, key: KeyEvent) -> Action {
        match popup {
            Popup::Unlock { group, input } => self.handle_input_key(
                input,
                key,
                |input| Popup::Unlock { group: group.clone(), input },
                |_, text| {
                    let duration_ms = if text.is_empty() { None } else { Some(parse_duration(text)?) };
                    Ok(Request::Unlock { group: group.clone(), duration_ms })
                },
            ),
            Popup::Lock { input } => self.handle_input_key(
                input,
                key,
                |input| Popup::Lock { input },
                |app, text| {
                    let snapshot = app.snapshot.as_ref();
                    let now = snapshot.map_or(0, |s| s.now_ms);
                    let from = snapshot.and_then(|s| s.lock.as_ref()).map_or(now, |l| l.until_ms);
                    match parse_duration(text)? {
                        0 => Err("enter a duration like 7d or 12h".into()),
                        ms => Ok(Request::Lock { until_ms: from + ms }),
                    }
                },
            ),
            Popup::Count { habit, input } => self.handle_input_key(
                input,
                key,
                |input| Popup::Count { habit: habit.clone(), input },
                |_, text| {
                    let (set, number) = match text.strip_prefix('=') {
                        Some(rest) => (true, rest.trim()),
                        None => (false, text),
                    };
                    match number.parse::<i64>() {
                        Ok(amount) if set && amount < 0 => Err("a count can't be negative".into()),
                        Ok(amount) => Ok(Request::Done { habit: habit.clone(), amount, set }),
                        Err(_) => Err("enter a number like 5, -2 or =40".into()),
                    }
                },
            ),
            Popup::SetCategory { app, input } => self.handle_input_key(
                input,
                key,
                |input| Popup::SetCategory { app: app.clone(), input },
                |_, text| {
                    Ok(Request::SetAppCategory { app: app.clone(), category: (!text.is_empty()).then(|| text.to_string()) })
                },
            ),
            Popup::RenameApp { app, input } => self.handle_input_key(
                input,
                key,
                |input| Popup::RenameApp { app: app.clone(), input },
                |_, text| Ok(Request::SetAppName { app: app.clone(), name: (!text.is_empty()).then(|| text.to_string()) }),
            ),
            Popup::ConfirmDelete { section, id } => match key.code {
                KeyCode::Char('y') => Action::Send(Request::EditConfig {
                    section: section.key().into(),
                    id,
                    table: None,
                    create: false,
                }),
                _ => Action::None,
            },
            Popup::ConfirmAbort { emergency } => match key.code {
                KeyCode::Char('y') => Action::Send(Request::Abort { emergency }),
                _ => Action::None,
            },
            Popup::GoalReached => match key.code {
                KeyCode::Char('c') | KeyCode::Enter => Action::Send(Request::Continue),
                KeyCode::Char('s') => Action::Send(Request::Stop),
                KeyCode::Esc => {
                    self.dismissed_goal = self.session().map(|s| (s.habit.clone(), s.target_ms));
                    Action::None
                }
                _ => {
                    self.popup = Some(Popup::GoalReached);
                    Action::None
                }
            },
            Popup::Settings { index, editing } => self.handle_settings_key(index, editing, key),
        }
    }

    /// A one-line prompt: esc closes it, enter sends what `submit` makes of
    /// the trimmed input, or keeps it open and flashes the error.
    fn handle_input_key(
        &mut self,
        mut input: String,
        key: KeyEvent,
        reopen: impl Fn(String) -> Popup,
        submit: impl Fn(&App, &str) -> Result<Request, String>,
    ) -> Action {
        match key.code {
            KeyCode::Esc => return Action::None,
            KeyCode::Enter => match submit(self, input.trim()) {
                Ok(request) => return Action::Send(request),
                Err(e) => self.set_flash(e, true),
            },
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) if !c.is_control() && input.len() < MAX_INPUT => input.push(c),
            _ => {}
        }
        self.popup = Some(reopen(input));
        Action::None
    }

    fn handle_settings_key(&mut self, mut index: usize, editing: Option<String>, key: KeyEvent) -> Action {
        let setting = &SETTINGS[index];
        if let Some(mut input) = editing {
            let mut action = Action::None;
            let mut editing = true;
            match key.code {
                KeyCode::Esc => editing = false,
                KeyCode::Enter => {
                    action = Action::Send(Request::SetSetting {
                        key: setting.key.to_string(),
                        value: input.trim().to_string(),
                    });
                    editing = false;
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) if !c.is_control() && input.len() < MAX_INPUT => input.push(c),
                _ => {}
            }
            self.popup = Some(Popup::Settings { index, editing: editing.then_some(input) });
            return action;
        }
        let mut action = Action::None;
        let mut editing = None;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('o') => return Action::None,
            KeyCode::Up | KeyCode::Char('k') => index = index.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => index = (index + 1).min(SETTINGS.len() - 1),
            KeyCode::Enter => {
                let current = self.snapshot.as_ref().map(|s| setting_value(s, setting.key)).unwrap_or_default();
                if SWITCHES.contains(&setting.key) {
                    let toggled = if current == "on" { "off" } else { "on" };
                    action = Action::Send(Request::SetSetting {
                        key: setting.key.to_string(),
                        value: toggled.to_string(),
                    });
                } else {
                    editing = Some(current);
                }
            }
            _ => {}
        }
        self.popup = Some(Popup::Settings { index, editing });
        action
    }

    /// Typing the Insights filter: it updates as you type, enter keeps it,
    /// esc clears it.
    fn handle_filter_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Enter => self.filtering = false,
            KeyCode::Esc => {
                self.filtering = false;
                self.insights_filter.clear();
            }
            KeyCode::Backspace => {
                self.insights_filter.pop();
            }
            KeyCode::Char(c) if !c.is_control() && self.insights_filter.chars().count() < MAX_INPUT => {
                self.insights_filter.push(c)
            }
            _ => return Action::None,
        }
        self.app_index = 0;
        Action::None
    }

    fn handle_main_key(&mut self, key: KeyEvent) -> Action {
        if self.pane == Pane::Insights {
            if self.filtering {
                return self.handle_filter_key(key);
            }
            // Esc clears a filter before it quits.
            if key.code == KeyCode::Esc && !self.insights_filter.is_empty() {
                self.insights_filter.clear();
                self.app_index = 0;
                return Action::None;
            }
        }
        let session = self.session();
        let strict = session.is_some_and(|s| s.strictness == Strictness::Strict);
        let has_session = session.is_some();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
            KeyCode::Char(c @ '1'..='5') => self.pane = Pane::ALL[c as usize - '1' as usize],
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.pane = self.pane.cycle(1),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => self.pane = self.pane.cycle(-1),
            KeyCode::Char('L') => self.pane = Pane::Lock,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char('s') if has_session => return Action::Send(Request::Stop),
            KeyCode::Char('c') if has_session => return Action::Send(Request::Continue),
            KeyCode::Char('a') if has_session => {
                if strict {
                    self.set_flash("Strict session: s stops and keeps progress, A discards it with a penalty", true);
                } else {
                    self.popup = Some(Popup::ConfirmAbort { emergency: false });
                }
            }
            KeyCode::Char('A') if has_session => self.popup = Some(Popup::ConfirmAbort { emergency: true }),
            KeyCode::Char('o') => self.popup = Some(Popup::Settings { index: 0, editing: None }),
            KeyCode::Char('R') => return Action::Send(Request::Reload),
            _ => {
                return match self.pane {
                    Pane::Today | Pane::Habits => self.handle_habit_key(key),
                    Pane::Blocks => self.handle_block_key(key),
                    Pane::Lock => self.handle_lock_key(key),
                    Pane::Insights => self.handle_insights_key(key),
                }
            }
        }
        Action::None
    }

    fn handle_habit_key(&mut self, key: KeyEvent) -> Action {
        if self.pane == Pane::Habits {
            let selected = self.selected_habit().map(|h| h.id.clone());
            match (key.code, selected) {
                (KeyCode::Char('n'), _) => return Action::OpenEditor { section: Section::Habits, id: None },
                (KeyCode::Char('e'), Some(id)) => return Action::OpenEditor { section: Section::Habits, id: Some(id) },
                (KeyCode::Char('D'), Some(id)) => {
                    self.popup = Some(Popup::ConfirmDelete { section: Section::Habits, id });
                    return Action::None;
                }
                _ => {}
            }
        }
        let Some(h) = self.selected_habit() else { return Action::None };
        let (id, name, logged) = (h.id.clone(), h.name.clone(), !h.kind.is_timed());
        if h.kind == habit_core::config::HabitKind::Passive {
            if matches!(key.code, KeyCode::Enter | KeyCode::Char('+' | '-' | '=')) {
                self.set_flash(format!("{name} counts by itself while its windows are focused"), false);
            }
            return Action::None;
        }
        let done = |amount| Action::Send(Request::Done { habit: id.clone(), amount, set: false });
        match key.code {
            KeyCode::Enter if logged => done(1),
            KeyCode::Enter => Action::Send(Request::Start { habit: id.clone() }),
            KeyCode::Char('+') if logged => done(1),
            KeyCode::Char('-') if logged => done(-1),
            KeyCode::Char('=') if logged => {
                self.popup = Some(Popup::Count { habit: id.clone(), input: String::new() });
                Action::None
            }
            KeyCode::Char('+' | '-' | '=') => {
                self.set_flash(format!("{name} is tracked by time; enter starts it"), true);
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_insights_key(&mut self, key: KeyEvent) -> Action {
        // Row 0 is the total; category rows have no key.
        let selected = self.app_index.checked_sub(1).and_then(|i| self.insight_rows().into_iter().nth(i)).and_then(|r| r.key);
        let label = |table: fn(&Snapshot) -> &std::collections::BTreeMap<String, String>, app: &str| {
            self.snapshot.as_ref().and_then(|s| table(s).get(app)).cloned().unwrap_or_default()
        };
        match (key.code, selected) {
            (KeyCode::Char('f' | '/'), _) => self.filtering = true,
            (KeyCode::Char('g'), _) => {
                self.by_category = !self.by_category;
                self.app_index = 0;
            }
            (KeyCode::Char('d'), _) => {
                self.chart_period = !self.chart_period;
                self.chart_day = 0;
                return Action::Refresh;
            }
            // Step through the days of the hourly chart.
            (KeyCode::Char('['), _) => {
                if !self.breakdown_day.more_before && !self.chart_period {
                    self.set_flash("Nothing recorded before that day", true);
                    return Action::None;
                }
                self.chart_period = false;
                self.chart_day += 1;
                return Action::Refresh;
            }
            (KeyCode::Char(']'), _) if self.chart_day > 0 || self.chart_period => {
                self.chart_period = false;
                self.chart_day = self.chart_day.saturating_sub(1);
                return Action::Refresh;
            }
            (KeyCode::Char('r'), Some(app)) => {
                let input = label(|s| &s.app_names, &app);
                self.popup = Some(Popup::RenameApp { app, input });
            }
            (KeyCode::Char('c'), Some(app)) => {
                let input = label(|s| &s.app_categories, &app);
                self.popup = Some(Popup::SetCategory { app, input });
            }
            _ => {}
        }
        Action::None
    }

    fn handle_block_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('n') {
            return Action::OpenEditor { section: Section::Groups, id: None };
        }
        let Some(group) = self.selected_group().map(|g| g.id.clone()) else { return Action::None };
        match key.code {
            KeyCode::Char('e') => Action::OpenEditor { section: Section::Groups, id: Some(group) },
            KeyCode::Char('D') => {
                self.popup = Some(Popup::ConfirmDelete { section: Section::Groups, id: group });
                Action::None
            }
            KeyCode::Enter => Action::Send(Request::Unlock { group, duration_ms: None }),
            KeyCode::Char('u') => {
                self.popup = Some(Popup::Unlock { group, input: String::new() });
                Action::None
            }
            KeyCode::Char('r') => Action::Send(Request::Relock { group }),
            _ => Action::None,
        }
    }

    fn handle_lock_key(&mut self, key: KeyEvent) -> Action {
        let lock = self.snapshot.as_ref().and_then(|s| s.lock.as_ref());
        match key.code {
            KeyCode::Enter | KeyCode::Char('x') => {
                self.popup = Some(Popup::Lock { input: String::new() });
                Action::None
            }
            KeyCode::Char('e') => match lock {
                Some(l) if l.end_requested => Action::Send(Request::LockCancelEnd),
                Some(_) => Action::Send(Request::LockEnd),
                None => Action::None,
            },
            _ => Action::None,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use habit_core::{Config, Engine, Input, State};

    pub const CONFIG: &str = r#"
        [general]
        day_start = "04:00"

        [groups.games]
        name = "Games"
        apps = ["steam"]

        [groups.social]
        name = "Social"
        domains = ["reddit.com"]

        [habits.book]
        name = "Book"
        kind = "timer"
        target = "20m"
        reward = { groups = ["social"], duration = "1h" }

        [habits.reading]
        name = "Reading"
        target = "20m"
        strictness = "strict"
        allow = [{ app = "zathura" }]
        reward = { groups = ["social"], duration = "1h" }

        [habits.walk]
        name = "Walk"
        target = "30m"
        reward = { groups = ["games"], duration = "30m", mode = "immediate" }
    "#;

    pub fn engine() -> Engine {
        Engine::new(Config::from_toml(CONFIG).unwrap(), State::default())
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn app() -> App {
        let mut app = App::new();
        app.set_snapshot(engine().snapshot(0));
        app
    }

    /// A book timer session that just completed its first 20 minute round.
    pub fn engine_at_goal() -> Engine {
        let mut e = engine();
        e.start("book", 0).unwrap();
        let mut t = 0;
        while t <= 20 * 60_000 {
            e.handle(Input::TimerFocus { source: "tui".into(), focused: Some(true) }, t);
            t += 2000;
        }
        e
    }

    #[test]
    fn navigates_and_starts_selected_habit() {
        let mut app = app();
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.habit_index, 2);
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::Send(Request::Start { habit: "walk".into() }));
    }

    #[test]
    fn logged_habits_are_counted_instead_of_started() {
        let config = format!(
            "{CONFIG}\n[habits.yoga]\nkind = \"counter\"\ngoal = 3\nreward = {{ groups = [\"games\"], duration = \"10m\" }}\n"
        );
        let mut app = App::new();
        app.set_snapshot(Engine::new(Config::from_toml(&config).unwrap(), State::default()).snapshot(0));
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Down));
        }
        let done = |amount| Action::Send(Request::Done { habit: "yoga".into(), amount, set: false });
        assert_eq!(app.handle_key(key(KeyCode::Enter)), done(1));
        assert_eq!(app.handle_key(key(KeyCode::Char('+'))), done(1));
        assert_eq!(app.handle_key(key(KeyCode::Char('-'))), done(-1));
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.handle_key(key(KeyCode::Char('+'))), Action::None);
    }

    #[test]
    fn enter_in_groups_pane_unlocks_all_credit() {
        let mut app = app();
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::Send(Request::Unlock { group: "social".into(), duration_ms: None })
        );
    }

    #[test]
    fn unlock_popup_parses_duration() {
        let mut app = app();
        app.handle_key(key(KeyCode::Char('2')));
        app.handle_key(key(KeyCode::Char('u')));
        for c in "30x".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
        assert!(app.popup.is_some());
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Char('m')));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::Send(Request::Unlock { group: "games".into(), duration_ms: Some(30 * 60_000) })
        );
        assert!(app.popup.is_none());
    }

    #[test]
    fn stop_keeps_progress_and_abort_asks_first() {
        let mut e = engine();
        e.start("walk", 0).unwrap();
        let mut app = App::new();
        app.set_snapshot(e.snapshot(0));

        assert_eq!(app.handle_key(key(KeyCode::Char('s'))), Action::Send(Request::Stop));
        assert_eq!(app.handle_key(key(KeyCode::Char('a'))), Action::None);
        assert_eq!(app.popup, Some(Popup::ConfirmAbort { emergency: false }));
        assert_eq!(app.handle_key(key(KeyCode::Char('y'))), Action::Send(Request::Abort { emergency: false }));
    }

    #[test]
    fn strict_abort_needs_emergency_confirmation() {
        let mut e = engine();
        e.start("reading", 0).unwrap();
        let mut app = App::new();
        app.set_snapshot(e.snapshot(0));

        assert_eq!(app.handle_key(key(KeyCode::Char('a'))), Action::None);
        assert!(app.flash().is_some_and(|f| f.error));
        app.handle_key(key(KeyCode::Char('A')));
        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), Action::None);
        assert!(app.popup.is_none());
        app.handle_key(key(KeyCode::Char('A')));
        assert_eq!(app.handle_key(key(KeyCode::Char('y'))), Action::Send(Request::Abort { emergency: true }));
    }

    #[test]
    fn round_prompt_offers_continue_or_stop_and_can_be_dismissed() {
        let e = engine_at_goal();
        let mut app = App::new();
        app.set_snapshot(e.snapshot(20 * 60_000));
        assert_eq!(app.popup, Some(Popup::GoalReached));
        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), Action::None);
        assert_eq!(app.popup, Some(Popup::GoalReached));
        assert_eq!(app.handle_key(key(KeyCode::Char('s'))), Action::Send(Request::Stop));

        // Dismissed prompts stay closed on later snapshots of the same round.
        let mut app = App::new();
        app.set_snapshot(e.snapshot(20 * 60_000));
        app.handle_key(key(KeyCode::Esc));
        app.set_snapshot(e.snapshot(20 * 60_000 + 1000));
        assert!(app.popup.is_none());
        assert_eq!(app.handle_key(key(KeyCode::Char('c'))), Action::Send(Request::Continue));
    }

    #[test]
    fn settings_edit_and_toggle() {
        let mut app = app();
        app.handle_key(key(KeyCode::Char('o')));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.popup, Some(Popup::Settings { index: 0, editing: Some("04:00".into()) }));
        for _ in 0..5 {
            app.handle_key(key(KeyCode::Backspace));
        }
        for c in "03:30".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::Send(Request::SetSetting { key: "day_start".into(), value: "03:30".into() })
        );
        assert_eq!(app.popup, Some(Popup::Settings { index: 0, editing: None }));

        for _ in 0..10 {
            app.handle_key(key(KeyCode::Down));
        }
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::Send(Request::SetSetting { key: "update_check".into(), value: "off".into() })
        );
        app.handle_key(key(KeyCode::Up));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::Send(Request::SetSetting { key: "auto_categories".into(), value: "off".into() })
        );
        app.handle_key(key(KeyCode::Up));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::Send(Request::SetSetting { key: "terminal_programs".into(), value: "off".into() })
        );
        app.handle_key(key(KeyCode::Up));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::Send(Request::SetSetting { key: "notifications".into(), value: "off".into() })
        );
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::None);
        assert!(app.popup.is_none());
    }

    #[test]
    fn only_settings_pause_the_timer() {
        let mut app = app();
        assert!(!app.pauses_timer());
        app.handle_key(key(KeyCode::Char('o')));
        assert!(app.pauses_timer());
        app.handle_key(key(KeyCode::Enter)); // editing a value still pauses
        assert!(app.pauses_timer());
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.pauses_timer());
        app.handle_key(key(KeyCode::Char('2')));
        app.handle_key(key(KeyCode::Char('u')));
        assert!(!app.pauses_timer());
    }

    #[test]
    fn lock_pane_starts_extends_and_ends() {
        let mut app = app();
        app.handle_key(key(KeyCode::Char('L')));
        assert_eq!(app.pane, Pane::Lock);
        assert_eq!(app.handle_key(key(KeyCode::Char('e'))), Action::None);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.popup, Some(Popup::Lock { input: String::new() }));
        for c in "7d".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::Send(Request::Lock { until_ms: 7 * 86_400_000 })
        );

        let mut e = engine();
        e.lock(7 * 86_400_000, "", 0).unwrap();
        let mut app = App::new();
        app.set_snapshot(e.snapshot(0));
        app.handle_key(key(KeyCode::Char('5')));
        assert_eq!(app.handle_key(key(KeyCode::Char('e'))), Action::Send(Request::LockEnd));
        e.request_lock_end(0).unwrap();
        app.set_snapshot(e.snapshot(0));
        assert_eq!(app.handle_key(key(KeyCode::Char('e'))), Action::Send(Request::LockCancelEnd));
        // Extending adds to the current end.
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Char('1')));
        app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::Send(Request::Lock { until_ms: 8 * 86_400_000 })
        );
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('0')));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
        assert!(app.flash().is_some_and(|f| f.error));
    }

    #[test]
    fn panes_switch_by_number_tab_and_arrows() {
        let mut app = app();
        assert_eq!(app.pane, Pane::Today);
        app.handle_key(key(KeyCode::Char('3')));
        assert_eq!(app.pane, Pane::Habits);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.pane, Pane::Insights);
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.pane, Pane::Today);
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.pane, Pane::Lock);
        app.handle_key(key(KeyCode::Char('h')));
        assert_eq!(app.pane, Pane::Insights);

        // Each list keeps its own selection.
        app.handle_key(key(KeyCode::Char('2')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char('1')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        assert_eq!((app.group_index, app.habit_index), (1, 2));
        app.handle_key(key(KeyCode::Char('4')));
        app.handle_key(key(KeyCode::Down));
        assert_eq!((app.group_index, app.habit_index), (1, 2));
    }

    #[test]
    fn count_popup_adds_or_sets() {
        let config = format!(
            "{CONFIG}\n[habits.yoga]\nkind = \"counter\"\ngoal = 3\nreward = {{ groups = [\"games\"], duration = \"10m\" }}\n"
        );
        let mut app = App::new();
        app.set_snapshot(Engine::new(Config::from_toml(&config).unwrap(), State::default()).snapshot(0));
        app.habit_index = 3;
        let mut submit = |text: &str| {
            app.handle_key(key(KeyCode::Char('=')));
            for c in text.chars() {
                app.handle_key(key(KeyCode::Char(c)));
            }
            app.handle_key(key(KeyCode::Enter))
        };
        assert_eq!(submit("12"), Action::Send(Request::Done { habit: "yoga".into(), amount: 12, set: false }));
        assert_eq!(submit("-2"), Action::Send(Request::Done { habit: "yoga".into(), amount: -2, set: false }));
        assert_eq!(submit("=40"), Action::Send(Request::Done { habit: "yoga".into(), amount: 40, set: true }));
        assert_eq!(submit("x"), Action::None);
        assert!(matches!(app.popup, Some(Popup::Count { .. })));
    }

    #[test]
    fn new_events_ask_for_a_refresh() {
        let mut e = engine();
        let mut app = App::new();
        assert!(app.set_snapshot(e.snapshot(0)), "first snapshot");
        assert!(!app.set_snapshot(e.snapshot(0)));
        e.start("walk", 0).unwrap();
        assert!(app.set_snapshot(e.snapshot(0)));
        assert!(!app.set_snapshot(e.snapshot(1000)));
    }

    #[test]
    fn escape_closes_popup_before_quitting() {
        let mut app = app();
        app.handle_key(key(KeyCode::Char('2')));
        app.handle_key(key(KeyCode::Char('u')));
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::None);
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::Quit);
    }

    fn entries() -> habit_ipc::ConfigEntries {
        let (mut groups, mut habits) = (std::collections::BTreeMap::new(), std::collections::BTreeMap::new());
        groups.insert("games".to_string(), serde_json::json!({ "name": "Games", "apps": ["steam"] }));
        habits.insert(
            "walk".to_string(),
            serde_json::json!({ "name": "Walk", "target": "30m", "reward": { "groups": ["games"], "duration": "30m", "mode": "immediate" } }),
        );
        habit_ipc::ConfigEntries { groups, habits }
    }

    #[test]
    fn blocks_and_habits_open_the_form_and_delete_after_asking() {
        let mut app = app();
        app.handle_key(key(KeyCode::Char('2')));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('e'))),
            Action::OpenEditor { section: Section::Groups, id: Some("games".into()) }
        );
        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), Action::OpenEditor { section: Section::Groups, id: None });
        app.handle_key(key(KeyCode::Char('D')));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            Action::Send(Request::EditConfig { section: "groups".into(), id: "games".into(), table: None, create: false })
        );

        app.handle_key(key(KeyCode::Char('3')));
        app.habit_index = 2;
        assert_eq!(
            app.handle_key(key(KeyCode::Char('e'))),
            Action::OpenEditor { section: Section::Habits, id: Some("walk".into()) }
        );
        app.handle_key(key(KeyCode::Char('D')));
        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), Action::None);
        assert!(app.popup.is_none());
        // Today doesn't edit.
        app.handle_key(key(KeyCode::Char('1')));
        assert_eq!(app.handle_key(key(KeyCode::Char('e'))), Action::None);
    }

    #[test]
    fn the_form_saves_and_closes_or_shows_the_error() {
        let mut app = app();
        app.open_editor(Section::Groups, Some("games".into()), &entries());
        assert!(app.editor.is_some() && app.pauses_timer());
        let save = || KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(matches!(app.handle_key(save()), Action::Send(Request::EditConfig { create: false, .. })));
        app.apply_response(Ok(Response::err("Not during your commitment (1d left): removes app steam from Games")));
        assert!(app.editor.as_ref().unwrap().error.as_deref().unwrap().starts_with("Not during"));
        app.handle_key(save());
        app.apply_response(Ok(Response { ok: true, message: Some("Saved block Games".into()), ..Default::default() }));
        assert!(app.editor.is_none());
        assert_eq!(app.flash().map(|f| f.text.as_str()), Some("Saved block Games"));

        app.open_editor(Section::Habits, Some("nope".into()), &entries());
        assert!(app.editor.is_none() && app.flash().is_some_and(|f| f.error));
        app.open_editor(Section::Habits, None, &entries());
        assert!(app.editor.as_ref().is_some_and(|e| e.creating));
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::None);
        assert!(app.editor.is_none(), "an untouched form closes right away");
    }

    #[test]
    fn insights_rename_apps() {
        let mut app = app();
        app.apps = vec![habit_core::snapshot::AppUsageView {
            app: "steam_app_275850".into(),
            today_ms: 60_000,
            total_ms: 60_000,
            sessions: 1,
            sessions_today: 1,
            avg_session_ms: 60_000,
            longest_session_ms: 60_000,
            hours: vec![0; 24],
            hours_today: vec![0; 24],
            days_ms: Vec::new(),
            days_sessions: Vec::new(),
        }];
        app.handle_key(key(KeyCode::Char('4')));
        assert_eq!(app.handle_key(key(KeyCode::Char('r'))), Action::None);
        assert!(app.popup.is_none(), "the all-apps row can't be renamed");
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char('r')));
        for c in "No Man's Sky".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::Send(Request::SetAppName { app: "steam_app_275850".into(), name: Some("No Man's Sky".into()) })
        );
        app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::Send(Request::SetAppName { app: "steam_app_275850".into(), name: None })
        );

        app.handle_key(key(KeyCode::Char('c')));
        for c in "Games".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::Send(Request::SetAppCategory { app: "steam_app_275850".into(), category: Some("Games".into()) })
        );
        // Category rows have no app to rename or categorize.
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char('c')));
        app.handle_key(key(KeyCode::Char('r')));
        assert!(app.popup.is_none());
        // While filtering, keys go to the filter, not to panes or quit.
        app.handle_key(key(KeyCode::Char('f')));
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Action::None);
        app.handle_key(key(KeyCode::Char('1')));
        assert_eq!((app.pane, app.insights_filter.as_str()), (Pane::Insights, "q1"));
    }
}
