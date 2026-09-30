use crate::duration::{
    deserialize_duration, deserialize_opt_duration, deserialize_time_of_day, format_time_of_day, parse_time_of_day,
};
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub general: General,
    #[serde(default)]
    pub groups: BTreeMap<String, Group>,
    #[serde(default)]
    pub habits: BTreeMap<String, Habit>,
    /// Display names for app ids and `site:<host>` screen-time keys, e.g.
    /// `steam_app_275850 = "No Man's Sky"`. Only labels: nothing matches on them.
    #[serde(default)]
    pub app_names: BTreeMap<String, String>,
    /// Categories of screen-time keys, e.g. `steam_app_275850 = "Games"`, for
    /// grouping screen time. Labels too.
    #[serde(default)]
    pub app_categories: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct General {
    /// Inactivity after which a running session pauses.
    #[serde(deserialize_with = "deserialize_duration")]
    pub idle_timeout: u64,
    /// How long unlocks are refused after an emergency abort of a strict session.
    #[serde(deserialize_with = "deserialize_duration")]
    pub emergency_penalty: u64,
    /// Notify this long before an unlock ends.
    #[serde(deserialize_with = "deserialize_duration")]
    pub expiry_warning: u64,
    /// App ids that strict sessions never pull focus away from.
    pub strict_exempt_apps: Vec<String>,
    pub notifications: bool,
    /// When a new day starts (`HH:MM`, local time). Saved progress of stopped
    /// sessions is kept until then, so late sessions count towards the evening.
    #[serde(deserialize_with = "deserialize_time_of_day")]
    pub day_start: u64,
    /// Browser app ids. During a commitment lock their windows are closed when
    /// the habitfocus extension isn't running in them.
    pub browsers: Vec<String>,
    /// How many days of per-app screen time to keep.
    pub screen_time_days: u32,
    /// Terminal emulator app ids (compared ignoring case).
    pub terminals: Vec<String>,
    /// Screen time in a terminal goes to the program in front, like
    /// `term:nvim`, instead of to the terminal.
    pub terminal_programs: bool,
    /// Terminals and their programs are in the category "Terminal", browsers
    /// and sites in "Browser", unless `[app_categories]` says otherwise.
    pub auto_categories: bool,
    /// habitd looks for a newer release on GitHub once a day.
    pub update_check: bool,
    /// Sound when a timed habit reaches its target: a sound name like
    /// "complete", a path to a file, or "" for silence.
    pub sound: String,
}

/// Terminal emulators known out of the box.
pub const DEFAULT_TERMINALS: &[&str] = &[
    "kitty",
    "Alacritty",
    "foot",
    "footclient",
    "com.mitchellh.ghostty",
    "org.wezfurlong.wezterm",
    "org.gnome.Ptyxis",
    "org.gnome.Console",
    "org.kde.konsole",
    "xterm",
    // Omarchy's default terminal.
    "org.omarchy.terminal",
];

/// Prefix of the screen-time key of a program in a terminal: `term:nvim`.
pub const TERMINAL_PREFIX: &str = "term:";
/// Category of terminals and their programs with `auto_categories`.
pub const TERMINAL_CATEGORY: &str = "Terminal";
/// Category of browsers and sites with `auto_categories`.
pub const BROWSER_CATEGORY: &str = "Browser";

/// Display names of well-known terminal programs, used unless
/// `[app_names]` has one.
const PROGRAM_NAMES: &[(&str, &str)] = &[
    ("nvim", "Neovim"),
    ("vim", "Vim"),
    ("hx", "Helix"),
    ("emacs", "Emacs"),
    ("claude", "Claude Code"),
    ("claude-code", "Claude Code"),
    ("codex", "Codex"),
    ("gemini", "Gemini CLI"),
    ("opencode", "OpenCode"),
    ("crush", "Crush"),
    ("cursor-agent", "Cursor Agent"),
    ("copilot", "Copilot CLI"),
    ("aider", "Aider"),
    ("lazygit", "lazygit"),
    ("btop", "btop"),
    ("htop", "htop"),
    ("yazi", "Yazi"),
    ("ssh", "SSH"),
];

impl Default for General {
    fn default() -> Self {
        Self {
            idle_timeout: 60_000,
            emergency_penalty: 3_600_000,
            expiry_warning: 60_000,
            strict_exempt_apps: Vec::new(),
            notifications: true,
            day_start: 0,
            browsers: Vec::new(),
            screen_time_days: 60,
            sound: "complete".into(),
            terminals: DEFAULT_TERMINALS.iter().map(|t| t.to_string()).collect(),
            terminal_programs: true,
            auto_categories: true,
            update_check: true,
        }
    }
}

/// Apps and sites that stay blocked until unlocked with earned credit.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Group {
    pub name: Option<String>,
    /// Wayland app ids whose windows get closed.
    pub apps: Vec<String>,
    /// Process names (`/proc/<pid>/comm`) that get terminated.
    pub processes: Vec<String>,
    /// Domains (and their subdomains) blocked by the browser extension.
    pub domains: Vec<String>,
    /// Local-time windows in which the group blocks. Empty: always.
    #[serde(deserialize_with = "one_or_many")]
    pub schedule: Vec<ScheduleRule>,
    /// How spent credit unlocks the group.
    pub unlock_mode: UnlockMode,
    /// Credit a `rest_of_day` unlock costs.
    #[serde(deserialize_with = "deserialize_opt_duration")]
    pub rest_of_day_price: Option<u64>,
    /// Habits that must be completed today before the group can be unlocked.
    pub requires: Vec<String>,
    /// Whether one (`any`, default) or every (`all`) required habit is needed.
    pub require: RequireMode,
    /// Compiled from `schedule` in `Config::prepare`.
    #[serde(skip)]
    mask: Option<WeekMask>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequireMode {
    #[default]
    Any,
    All,
}

/// How an unlock is used up. Ordered from strictest to most generous (the
/// commitment lock relies on the order).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnlockMode {
    /// The unlock runs on the clock, used or not.
    #[default]
    #[serde(alias = "wall")]
    Wallclock,
    /// The unlock is a budget of actual use: it only runs down while an app
    /// or site of the group is focused. Unused budget ends at `day_start`.
    Usage,
    /// Unlocked until the next `day_start`, for `rest_of_day_price`.
    RestOfDay,
}

impl UnlockMode {
    pub fn name(self) -> &'static str {
        match self {
            UnlockMode::Wallclock => "wallclock",
            UnlockMode::Usage => "usage",
            UnlockMode::RestOfDay => "rest_of_day",
        }
    }
}

pub const MINUTES_PER_DAY: usize = 1440;
pub const MINUTES_PER_WEEK: usize = 7 * MINUTES_PER_DAY;

/// Which minutes of the week (Monday 00:00 = 0) a schedule covers.
#[derive(Clone, PartialEq, Eq)]
pub struct WeekMask(Box<[bool]>);

impl std::fmt::Debug for WeekMask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let covered = self.0.iter().filter(|&&m| m).count();
        write!(f, "WeekMask({covered} of {MINUTES_PER_WEEK} minutes)")
    }
}

impl WeekMask {
    fn compile(rules: &[ScheduleRule]) -> WeekMask {
        let mut mask = vec![false; MINUTES_PER_WEEK].into_boxed_slice();
        for rule in rules {
            for day in &rule.days {
                for range in &rule.ranges {
                    let from = (range.from / 60_000) as usize;
                    let to = (range.to / 60_000) as usize;
                    // A range ending at or before its start runs past midnight;
                    // equal ends cover a whole day.
                    let len = if to > from { to - from } else { MINUTES_PER_DAY - from + to };
                    let start = *day as usize * MINUTES_PER_DAY + from;
                    for minute in start..start + len {
                        mask[minute % MINUTES_PER_WEEK] = true;
                    }
                }
            }
        }
        WeekMask(mask)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Weekday {
    pub const NAMES: [&'static str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
}

/// One or more weekdays with the time ranges that block on them.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleRule {
    pub days: Vec<Weekday>,
    pub ranges: Vec<TimeRange>,
}

/// `"HH:MM-HH:MM"` in local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    pub from: u64,
    pub to: u64,
}

impl<'de> Deserialize<'de> for TimeRange {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        let (from, to) = text
            .split_once('-')
            .ok_or_else(|| serde::de::Error::custom(format!("invalid range {text:?}: expected \"HH:MM-HH:MM\"")))?;
        let from = parse_time_of_day(from).map_err(serde::de::Error::custom)?;
        let to = parse_time_of_day(to).map_err(serde::de::Error::custom)?;
        Ok(TimeRange { from, to })
    }
}

impl std::fmt::Display for TimeRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", format_time_of_day(self.from), format_time_of_day(self.to))
    }
}

/// Accepts a single table as well as an array of tables.
fn one_or_many<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany<T> {
        One(T),
        Many(Vec<T>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(one) => vec![one],
        OneOrMany::Many(many) => many,
    })
}

impl Group {
    pub fn matches_app(&self, app_id: &str) -> bool {
        self.apps.iter().any(|a| a.eq_ignore_ascii_case(app_id))
    }

    pub fn matches_process(&self, comm: &str) -> bool {
        self.processes.iter().any(|p| p == comm)
    }

    /// Whether the group blocks at `minute_of_week` (Monday 00:00 = 0).
    /// Groups without a schedule always block.
    pub fn blocks_at(&self, minute_of_week: usize) -> bool {
        self.mask.as_ref().is_none_or(|mask| mask.0[minute_of_week % MINUTES_PER_WEEK])
    }

    pub fn has_schedule(&self) -> bool {
        !self.schedule.is_empty()
    }

    /// Human summary of the schedule, e.g. `["mon tue 07:00-22:00"]`.
    pub fn schedule_summary(&self) -> Vec<String> {
        self.schedule
            .iter()
            .map(|rule| {
                let days = rule.days.iter().map(|d| Weekday::NAMES[*d as usize]).collect::<Vec<_>>().join(" ");
                let ranges = rule.ranges.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
                format!("{days} {ranges}")
            })
            .collect()
    }

    /// Same rule as the browser extension: the domain itself or a subdomain.
    pub fn matches_domain(&self, host: &str) -> bool {
        self.domains
            .iter()
            .any(|d| host == d || host.strip_suffix(d.as_str()).is_some_and(|rest| rest.ends_with('.')))
    }
}

/// Lowercase host of an http(s) URL, without userinfo, port or trailing dot.
pub fn url_host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next()?;
    if host_port.starts_with('[') {
        return None; // IPv6 literal, never a configured domain
    }
    let host = host_port.split(':').next()?.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Habit {
    pub name: Option<String>,
    #[serde(default)]
    pub kind: HabitKind,
    /// Focused time per round (`apps` and `timer` habits), or the daily goal
    /// of a `passive` habit (0: only tracked).
    #[serde(default, deserialize_with = "deserialize_duration")]
    pub target: u64,
    /// Count per round (`manual`: default 1, `counter`: required).
    #[serde(default)]
    goal: Option<u64>,
    /// What a `counter` counts, e.g. "cards".
    #[serde(default)]
    pub unit: Option<String>,
    /// Most rewarded rounds per day (`manual`, `counter` and `passive`).
    #[serde(default)]
    pub daily_limit: Option<u32>,
    /// Windows that count as working on the habit. Empty: any activity counts.
    #[serde(default)]
    pub allow: Vec<WindowRule>,
    #[serde(default)]
    pub strictness: Strictness,
    /// What happens when the target is reached. Default: `ask` for timers,
    /// `finish` otherwise.
    #[serde(default)]
    on_target: Option<OnTarget>,
    /// Required, except for `passive` habits that are only tracked.
    #[serde(default)]
    pub reward: Reward,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HabitKind {
    /// Counts while an allowed application window is focused.
    #[default]
    Apps,
    /// Counts while the `hf tui` window is focused, for offline activities
    /// like reading a paper book. Ignores keyboard/mouse inactivity.
    Timer,
    /// Checked off by hand (`hf done journal`).
    Manual,
    /// Counted by hand (`hf done anki 20`), rewarded per `goal`.
    Counter,
    /// Counts by itself all day while an allowed window is focused, without
    /// a session; `target` is an optional daily goal.
    Passive,
    /// A kind this build doesn't know (snapshots from a newer daemon).
    /// Rejected in configs.
    #[serde(other)]
    Other,
}

impl HabitKind {
    /// Tracked by a timed session, as opposed to logged with `hf done` or
    /// counted passively.
    pub fn is_timed(self) -> bool {
        matches!(self, HabitKind::Apps | HabitKind::Timer)
    }

    /// Measured in time (sessions or passive), as opposed to counts.
    pub fn tracks_time(self) -> bool {
        matches!(self, HabitKind::Apps | HabitKind::Timer | HabitKind::Passive)
    }

    pub fn name(self) -> &'static str {
        match self {
            HabitKind::Apps => "apps",
            HabitKind::Timer => "timer",
            HabitKind::Manual => "manual",
            HabitKind::Counter => "counter",
            HabitKind::Passive => "passive",
            HabitKind::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnTarget {
    /// Complete and grant the reward automatically.
    Finish,
    /// Bank the reward, then keep counting towards another round until the
    /// user stops; every further full target banks the reward again.
    Ask,
}

impl Habit {
    pub fn on_target(&self) -> OnTarget {
        self.on_target.unwrap_or(match self.kind {
            HabitKind::Timer => OnTarget::Ask,
            _ => OnTarget::Finish,
        })
    }

    /// Count per round for `manual`/`counter` habits (0 for timed habits).
    pub fn goal(&self) -> u64 {
        match self.kind {
            HabitKind::Manual => self.goal.unwrap_or(1),
            HabitKind::Counter => self.goal.unwrap_or(0),
            _ => 0,
        }
    }

    /// Rewarded rounds per day of a logged or passive habit: `daily_limit`,
    /// else once for a manual or passive habit and unlimited for a counter.
    pub fn rounds_per_day(&self) -> Option<u32> {
        self.daily_limit.or(matches!(self.kind, HabitKind::Manual | HabitKind::Passive).then_some(1))
    }

    /// Pays something when completed (tracking-only passive habits don't).
    pub fn has_reward(&self) -> bool {
        self.reward.given && !self.reward.groups.is_empty() && self.reward.duration > 0
    }

    /// Unit label for counts, e.g. "cards" or "times".
    pub fn unit_label(&self) -> &str {
        self.unit.as_deref().unwrap_or("times")
    }

    /// `url` is the active browser tab of the window, `front` what's in
    /// front when it's a terminal.
    pub fn allows(&self, app_id: &str, title: &str, url: Option<&str>, front: TerminalFront) -> bool {
        self.allow.is_empty() || self.allow.iter().any(|r| r.matches(app_id, title, url, front))
    }
}

/// What's in front in a terminal window, for `program` and `tmux_session`
/// rules. Empty for other windows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerminalFront<'a> {
    pub program: Option<&'a str>,
    pub tmux_session: Option<&'a str>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowRule {
    pub app: Option<String>,
    /// Regex matched against the window title.
    pub title: Option<String>,
    /// Regex matched against the active tab URL (needs the browser extension).
    pub url: Option<String>,
    /// The program in front in a terminal window, as screen time names it
    /// after `term:` (e.g. "nvim"), followed through tmux. Ignores case.
    pub program: Option<String>,
    /// The name of the tmux session shown in a terminal window.
    pub tmux_session: Option<String>,
    #[serde(skip)]
    title_re: Option<Regex>,
    #[serde(skip)]
    url_re: Option<Regex>,
}

impl WindowRule {
    /// Every condition of `broader` is one of this rule's too, so this rule
    /// matches nothing `broader` doesn't (used to spot rules that let more
    /// count).
    pub fn within(&self, broader: &WindowRule) -> bool {
        fn same(narrow: &Option<String>, broad: &Option<String>, ignore_case: bool) -> bool {
            match (narrow, broad) {
                (_, None) => true,
                (Some(n), Some(b)) => if ignore_case { n.eq_ignore_ascii_case(b) } else { n == b },
                (None, Some(_)) => false,
            }
        }
        same(&self.app, &broader.app, true)
            && same(&self.title, &broader.title, false)
            && same(&self.url, &broader.url, false)
            && same(&self.program, &broader.program, true)
            && same(&self.tmux_session, &broader.tmux_session, false)
    }

    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(app) = &self.app {
            parts.push(format!("app {app}"));
        }
        if let Some(title) = &self.title {
            parts.push(format!("title /{title}/"));
        }
        if let Some(url) = &self.url {
            parts.push(format!("url /{url}/"));
        }
        if let Some(program) = &self.program {
            parts.push(format!("program {program}"));
        }
        if let Some(session) = &self.tmux_session {
            parts.push(format!("tmux session {session}"));
        }
        parts.join(", ")
    }

    pub fn matches(&self, app_id: &str, title: &str, url: Option<&str>, front: TerminalFront) -> bool {
        self.app.as_ref().is_none_or(|a| a.eq_ignore_ascii_case(app_id))
            && self.program.as_ref().is_none_or(|p| front.program.is_some_and(|q| p.eq_ignore_ascii_case(q)))
            && self.tmux_session.as_ref().is_none_or(|s| front.tmux_session == Some(s.as_str()))
            && self.title_re.as_ref().is_none_or(|r| r.is_match(title))
            && self
                .url_re
                .as_ref()
                .is_none_or(|r| url.is_some_and(|u| r.is_match(u)))
    }
}

/// Plain hostname check: lowercase labels of `a-z0-9-`, at least one dot.
pub fn is_valid_domain(domain: &str) -> bool {
    domain.len() <= 253
        && domain.contains('.')
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strictness {
    /// Leaving the allowed window only pauses the timer.
    #[default]
    Soft,
    /// Focus is pulled back to the allowed window until the habit is done.
    Strict,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reward {
    pub groups: Vec<String>,
    #[serde(deserialize_with = "deserialize_duration")]
    pub duration: u64,
    #[serde(default)]
    pub mode: RewardMode,
    /// The habit has a `reward` table (false for the default of a passive
    /// habit that is only tracked).
    #[serde(skip_deserializing, default = "yes")]
    pub given: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RewardMode {
    /// Credit is banked and spent later with `hf unlock`.
    #[default]
    Bank,
    /// The groups unlock right away.
    Immediate,
}

impl Config {
    pub fn from_toml(text: &str) -> Result<Config, String> {
        let mut config: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        config.prepare()?;
        Ok(config)
    }

    fn prepare(&mut self) -> Result<(), String> {
        for (table, labels) in [("app_names", &self.app_names), ("app_categories", &self.app_categories)] {
            if let Some((key, _)) = labels.iter().find(|(_, label)| label.trim().is_empty()) {
                return Err(format!("{table}: the entry for {key:?} is empty"));
            }
        }
        for (id, group) in &mut self.groups {
            if let Some(bad) = group.domains.iter().find(|d| !is_valid_domain(d)) {
                return Err(format!(
                    "group {id:?}: invalid domain {bad:?} (use a lowercase hostname like \"reddit.com\")"
                ));
            }
            for rule in &group.schedule {
                if rule.days.is_empty() || rule.ranges.is_empty() {
                    return Err(format!("group {id:?}: schedule entries need `days` and `ranges`"));
                }
            }
            group.mask = group.has_schedule().then(|| WeekMask::compile(&group.schedule));
            match (group.unlock_mode, group.rest_of_day_price) {
                (UnlockMode::RestOfDay, None | Some(0)) => {
                    return Err(format!("group {id:?}: unlock_mode = \"rest_of_day\" needs a rest_of_day_price, e.g. \"1h\""));
                }
                (UnlockMode::Wallclock | UnlockMode::Usage, Some(_)) => {
                    return Err(format!("group {id:?}: rest_of_day_price only applies to unlock_mode = \"rest_of_day\""));
                }
                _ => {}
            }
        }
        let habit_ids: Vec<String> = self.habits.keys().cloned().collect();
        for (id, group) in &self.groups {
            if let Some(unknown) = group.requires.iter().find(|h| !habit_ids.contains(h)) {
                return Err(format!("group {id:?}: requires unknown habit {unknown:?}"));
            }
        }
        for (id, habit) in &mut self.habits {
            match habit.kind {
                HabitKind::Apps | HabitKind::Timer => {
                    if habit.target == 0 {
                        return Err(format!("habit {id:?}: target must be greater than zero"));
                    }
                    if habit.goal.is_some() || habit.unit.is_some() || habit.daily_limit.is_some() {
                        return Err(format!(
                            "habit {id:?}: goal, unit and daily_limit are for manual and counter habits"
                        ));
                    }
                }
                HabitKind::Manual | HabitKind::Counter => {
                    if habit.target != 0 || !habit.allow.is_empty() {
                        return Err(format!(
                            "habit {id:?}: {} habits are logged with `hf done`; use `goal` instead of target/allow",
                            habit.kind.name()
                        ));
                    }
                    if habit.kind == HabitKind::Counter && habit.goal.unwrap_or(0) == 0 {
                        return Err(format!("habit {id:?}: counter habits need a goal, e.g. goal = 20"));
                    }
                    if habit.goal == Some(0) || habit.daily_limit == Some(0) {
                        return Err(format!("habit {id:?}: goal and daily_limit must be at least 1"));
                    }
                }
                HabitKind::Passive => {
                    if habit.allow.is_empty() {
                        return Err(format!(
                            "habit {id:?}: passive habits need allow rules saying what counts, e.g. allow = [{{ app = \"zed\" }}]"
                        ));
                    }
                    if habit.goal.is_some() || habit.unit.is_some() {
                        return Err(format!("habit {id:?}: passive habits have a target (a daily goal), not a goal or unit"));
                    }
                    if habit.strictness == Strictness::Strict || habit.on_target.is_some() {
                        return Err(format!("habit {id:?}: passive habits have no session: strictness and on_target don't apply"));
                    }
                    if habit.daily_limit == Some(0) {
                        return Err(format!("habit {id:?}: daily_limit must be at least 1"));
                    }
                    if habit.target == 0 && !habit.reward.groups.is_empty() {
                        return Err(format!("habit {id:?}: a passive habit needs a target to earn its reward"));
                    }
                }
                HabitKind::Other => return Err(format!("habit {id:?}: unknown kind")),
            }
            if habit.kind != HabitKind::Passive && !habit.reward.given {
                return Err(format!(
                    "habit {id:?}: needs a reward, e.g. reward = {{ groups = [\"social\"], duration = \"30m\" }}"
                ));
            }
            if habit.kind == HabitKind::Timer && !habit.allow.is_empty() {
                return Err(format!(
                    "habit {id:?}: timer habits count while `hf tui` is focused and take no allow rules"
                ));
            }
            for group in &habit.reward.groups {
                if !self.groups.contains_key(group) {
                    return Err(format!("habit {id:?}: reward group {group:?} is not defined"));
                }
            }
            for rule in &mut habit.allow {
                if rule.app.is_none()
                    && rule.title.is_none()
                    && rule.url.is_none()
                    && rule.program.is_none()
                    && rule.tmux_session.is_none()
                {
                    return Err(format!(
                        "habit {id:?}: allow rules need `app`, `title`, `url`, `program` and/or `tmux_session`"
                    ));
                }
                if rule.tmux_session.as_deref().is_some_and(str::is_empty) {
                    return Err(format!("habit {id:?}: allow rule tmux_session is empty"));
                }
                if let Some(program) = &mut rule.program {
                    // `term:nvim`, as `hf apps --json` lists it, means nvim.
                    if let Some(name) = program.strip_prefix(TERMINAL_PREFIX) {
                        *program = name.to_string();
                    }
                    if program.is_empty() || program.contains(char::is_whitespace) {
                        return Err(format!(
                            "habit {id:?}: allow rule program must be a program name like \"nvim\""
                        ));
                    }
                }
                if let Some(pattern) = &rule.title {
                    let re = Regex::new(pattern)
                        .map_err(|e| format!("habit {id:?}: invalid title regex: {e}"))?;
                    rule.title_re = Some(re);
                }
                if let Some(pattern) = &rule.url {
                    let re = Regex::new(pattern)
                        .map_err(|e| format!("habit {id:?}: invalid url regex: {e}"))?;
                    rule.url_re = Some(re);
                }
            }
        }
        Ok(())
    }

    /// The display name of an app id or screen-time key from `[app_names]`,
    /// else a known terminal program's name, else the key itself.
    pub fn app_name<'a>(&'a self, key: &'a str) -> &'a str {
        match self.app_names.get(key) {
            Some(name) => name,
            None => self.default_app_name(key).unwrap_or(key),
        }
    }

    /// The built-in display name of a screen-time key, if it has one.
    pub fn default_app_name(&self, key: &str) -> Option<&'static str> {
        let program = key.strip_prefix(TERMINAL_PREFIX)?;
        PROGRAM_NAMES.iter().find(|(p, _)| *p == program).map(|(_, name)| *name)
    }

    /// The category of a screen-time key: from `[app_categories]`, else the
    /// automatic one.
    pub fn app_category(&self, key: &str) -> Option<&str> {
        match self.app_categories.get(key) {
            Some(category) => Some(category),
            None => self.default_app_category(key),
        }
    }

    /// "Terminal" for terminals and their programs, "Browser" for browsers
    /// and sites, with `auto_categories`.
    pub fn default_app_category(&self, key: &str) -> Option<&'static str> {
        if !self.general.auto_categories {
            None
        } else if key.starts_with(TERMINAL_PREFIX) || self.is_terminal(key) {
            Some(TERMINAL_CATEGORY)
        } else if key.starts_with("site:") || self.is_browser(key) {
            Some(BROWSER_CATEGORY)
        } else {
            None
        }
    }

    /// Some habit counts only while a certain program or tmux session is in
    /// front in a terminal, so the daemon must look it up.
    pub fn has_terminal_rules(&self) -> bool {
        self.habits.values().any(|h| h.allow.iter().any(|r| r.program.is_some() || r.tmux_session.is_some()))
    }

    pub fn is_terminal(&self, app_id: &str) -> bool {
        self.general.terminals.iter().any(|t| t.eq_ignore_ascii_case(app_id))
    }

    pub fn is_browser(&self, app_id: &str) -> bool {
        self.general.browsers.iter().any(|b| b.eq_ignore_ascii_case(app_id))
    }

    pub fn habit_name<'a>(&'a self, id: &'a str) -> &'a str {
        self.habits.get(id).and_then(|h| h.name.as_deref()).unwrap_or(id)
    }

    pub fn group_name<'a>(&'a self, id: &'a str) -> &'a str {
        self.groups.get(id).and_then(|g| g.name.as_deref()).unwrap_or(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let config = Config::from_toml(crate::EXAMPLE_CONFIG).unwrap();
        assert!(!config.habits.is_empty());
    }

    #[test]
    fn timer_habits_default_to_ask_and_reject_allow_rules() {
        let config = Config::from_toml(
            r#"
            [general]
            day_start = "04:30"
            [groups.social]
            [habits.book]
            kind = "timer"
            target = "20m"
            reward = { groups = ["social"], duration = "1h" }
            [habits.code]
            target = "20m"
            reward = { groups = ["social"], duration = "1h" }
            "#,
        )
        .unwrap();
        assert_eq!(config.general.day_start, 16_200_000);
        assert_eq!(config.habits["book"].on_target(), OnTarget::Ask);
        assert_eq!(config.habits["code"].on_target(), OnTarget::Finish);

        let err = Config::from_toml(
            r#"
            [groups.social]
            [habits.book]
            kind = "timer"
            target = "20m"
            allow = [{ app = "kitty" }]
            reward = { groups = ["social"], duration = "1h" }
            "#,
        )
        .unwrap_err();
        assert!(err.contains("timer habits"));
    }

    const MON: usize = 0;
    const TUE: usize = MINUTES_PER_DAY;
    const SAT: usize = 5 * MINUTES_PER_DAY;

    fn at(day: usize, hh: usize, mm: usize) -> usize {
        day + hh * 60 + mm
    }

    #[test]
    fn schedules_compile_to_week_masks() {
        let config = Config::from_toml(
            r#"
            [groups.work]
            schedule = [
              { days = ["mon", "tue", "wed", "thu", "fri"], ranges = ["07:00-12:30", "13:30-22:00"] },
              { days = ["sat"], ranges = ["09:00-10:00"] },
            ]
            [groups.night]
            schedule = { days = ["mon", "sun"], ranges = ["22:00-02:00"] }
            [groups.always]
            apps = ["steam"]
            [groups.allday]
            schedule = { days = ["sat"], ranges = ["00:00-00:00"] }
            "#,
        )
        .unwrap();
        let work = &config.groups["work"];
        assert!(work.blocks_at(at(MON, 8, 0)));
        assert!(!work.blocks_at(at(MON, 12, 45)), "lunch gap");
        assert!(work.blocks_at(at(MON, 13, 30)));
        assert!(!work.blocks_at(at(MON, 22, 0)), "end is exclusive");
        assert!(work.blocks_at(at(SAT, 9, 30)));
        assert!(!work.blocks_at(at(SAT, 12, 0)));

        let night = &config.groups["night"];
        assert!(night.blocks_at(at(MON, 23, 30)));
        assert!(night.blocks_at(at(TUE, 1, 30)), "wraps into tuesday");
        assert!(!night.blocks_at(at(TUE, 3, 0)));
        assert!(night.blocks_at(at(MON, 1, 0)), "sunday night wraps into monday (week wraps)");

        assert!(config.groups["always"].blocks_at(at(SAT, 3, 0)));
        assert!(config.groups["allday"].blocks_at(at(SAT, 23, 59)));
        assert!(!config.groups["allday"].blocks_at(at(TUE, 12, 0)));
        assert_eq!(work.schedule_summary()[0], "mon tue wed thu fri 07:00-12:30, 13:30-22:00");

        assert!(Config::from_toml("[groups.x]\nschedule = { days = [], ranges = [\"08:00-09:00\"] }").is_err());
        assert!(Config::from_toml("[groups.x]\nschedule = { days = [\"mon\"], ranges = [\"8-9\"] }").is_err());
        assert!(Config::from_toml("[groups.x]\nschedule = { days = [\"monday\"], ranges = [\"08:00-09:00\"] }").is_err());
    }

    #[test]
    fn unlock_mode_settings_are_validated() {
        let config = Config::from_toml("[groups.a]\nunlock_mode = \"wall\"\n[groups.b]\nunlock_mode = \"usage\"").unwrap();
        assert_eq!(config.groups["a"].unlock_mode, UnlockMode::Wallclock);
        assert_eq!(config.groups["b"].unlock_mode, UnlockMode::Usage);
        let err = Config::from_toml("[groups.a]\nunlock_mode = \"rest_of_day\"").unwrap_err();
        assert!(err.contains("needs a rest_of_day_price"), "{err}");
        let err = Config::from_toml("[groups.a]\nrest_of_day_price = \"1h\"").unwrap_err();
        assert!(err.contains("only applies"), "{err}");
        let ok = Config::from_toml("[groups.a]\nunlock_mode = \"rest_of_day\"\nrest_of_day_price = \"45m\"").unwrap();
        assert_eq!(ok.groups["a"].rest_of_day_price, Some(45 * 60_000));
    }

    #[test]
    fn manual_counter_habits_and_requirements_are_validated() {
        let config = Config::from_toml(
            r#"
            [groups.feeds]
            requires = ["journal", "anki"]
            require = "all"
            [habits.journal]
            kind = "manual"
            reward = { groups = ["feeds"], duration = "15m" }
            [habits.anki]
            kind = "counter"
            goal = 20
            unit = "cards"
            daily_limit = 2
            reward = { groups = ["feeds"], duration = "30m" }
            "#,
        )
        .unwrap();
        assert_eq!(config.habits["journal"].goal(), 1);
        assert_eq!(config.habits["journal"].unit_label(), "times");
        assert_eq!(config.habits["anki"].goal(), 20);
        assert_eq!(config.groups["feeds"].require, RequireMode::All);

        let bad = [
            ("[habits.a]\nkind = \"counter\"\nreward = { groups = [], duration = 1 }", "need a goal"),
            ("[habits.a]\nkind = \"manual\"\ntarget = \"10m\"\nreward = { groups = [], duration = 1 }", "logged with `hf done`"),
            ("[habits.a]\ntarget = \"10m\"\ngoal = 3\nreward = { groups = [], duration = 1 }", "are for manual and counter"),
            ("[habits.a]\nkind = \"juggling\"\nreward = { groups = [], duration = 1 }", "unknown kind"),
            ("[groups.g]\nrequires = [\"nope\"]", "requires unknown habit"),
        ];
        for (text, expected) in bad {
            let err = Config::from_toml(text).unwrap_err();
            assert!(err.contains(expected), "{text}: {err}");
        }
    }

    #[test]
    fn url_hosts_and_domain_matching() {
        assert_eq!(url_host("https://www.YouTube.com/watch?v=1").as_deref(), Some("www.youtube.com"));
        assert_eq!(url_host("http://user:pw@reddit.com:8080/r").as_deref(), Some("reddit.com"));
        assert_eq!(url_host("https://x.com./home").as_deref(), Some("x.com"));
        assert_eq!(url_host("about:blank"), None);
        assert_eq!(url_host("moz-extension://abc/blocked.html"), None);

        let group = Group { domains: vec!["youtube.com".into()], ..Group::default() };
        assert!(group.matches_domain("youtube.com"));
        assert!(group.matches_domain("m.youtube.com"));
        assert!(!group.matches_domain("notyoutube.com"));
    }

    #[test]
    fn rejects_unknown_reward_group() {
        let err = Config::from_toml(
            r#"
            [habits.read]
            target = "20m"
            reward = { groups = ["nope"], duration = "1h" }
            "#,
        )
        .unwrap_err();
        assert!(err.contains("nope"));
    }

    #[test]
    fn title_rule_matches() {
        let config = Config::from_toml(
            r#"
            [groups.social]
            [habits.wiki]
            target = 10
            allow = [{ app = "zen", title = "(?i)wikipedia" }]
            reward = { groups = ["social"], duration = 30 }
            "#,
        )
        .unwrap();
        let habit = &config.habits["wiki"];
        assert!(habit.allows("zen", "Rust - Wikipedia — Zen Browser", None, TerminalFront::default()));
        assert!(!habit.allows("zen", "YouTube — Zen Browser", None, TerminalFront::default()));
        assert!(!habit.allows("kitty", "wikipedia", None, TerminalFront::default()));
    }

    #[test]
    fn url_rule_needs_a_matching_url() {
        let config = Config::from_toml(
            r#"
            [groups.social]
            [habits.wiki]
            target = 10
            allow = [{ app = "zen", url = "^https://[a-z]+\\.wikipedia\\.org/" }]
            reward = { groups = ["social"], duration = 30 }
            "#,
        )
        .unwrap();
        let habit = &config.habits["wiki"];
        assert!(habit.allows("zen", "", Some("https://en.wikipedia.org/wiki/Rust"), TerminalFront::default()));
        assert!(!habit.allows("zen", "", Some("https://youtube.com/"), TerminalFront::default()));
        assert!(!habit.allows("zen", "", None, TerminalFront::default()));
    }

    #[test]
    fn passive_habits_are_validated() {
        let config = |habit: &str| {
            Config::from_toml(&format!("[groups.social]\n[habits.code]\nkind = \"passive\"\n{habit}\n"))
        };
        let only_tracked = config(r#"allow = [{ app = "zed" }]"#).unwrap();
        assert!(!only_tracked.habits["code"].has_reward());
        assert_eq!(only_tracked.habits["code"].rounds_per_day(), Some(1));
        assert!(config("").unwrap_err().contains("need allow rules"));
        assert!(config("allow = [{ app = \"zed\" }]\ngoal = 3").is_err());
        assert!(config("allow = [{ app = \"zed\" }]\nstrictness = \"strict\"").is_err());
        assert!(config("allow = [{ app = \"zed\" }]\nreward = { groups = [\"social\"], duration = \"5m\" }")
            .unwrap_err()
            .contains("needs a target"));
        // Other kinds still need a reward.
        let apps = Config::from_toml("[habits.x]\ntarget = \"5m\"\n").unwrap_err();
        assert!(apps.contains("needs a reward"), "{apps}");
    }

    #[test]
    fn terminal_rules_are_validated_and_normalized() {
        let habit = |allow: &str| {
            Config::from_toml(&format!(
                "[groups.social]\n[habits.code]\ntarget = 10\nallow = [{allow}]\nreward = {{ groups = [\"social\"], duration = 30 }}\n"
            ))
            .map(|c| c.habits["code"].allow[0].clone())
        };
        assert_eq!(habit(r#"{ program = "term:nvim" }"#).unwrap().program.as_deref(), Some("nvim"));
        assert!(habit(r#"{ program = "" }"#).is_err());
        assert!(habit(r#"{ tmux_session = "" }"#).is_err());
        let rule = habit(r#"{ tmux_session = "my thesis" }"#).unwrap();
        assert!(rule.matches("kitty", "", None, TerminalFront { program: None, tmux_session: Some("my thesis") }));
        assert!(!rule.matches("kitty", "", None, TerminalFront::default()));
        assert_eq!(rule.describe(), "tmux session my thesis");
    }

    #[test]
    fn validates_domains() {
        assert!(is_valid_domain("reddit.com"));
        assert!(is_valid_domain("old.reddit.co.uk"));
        for bad in ["reddit", "Reddit.com", "reddit.com\n0.0.0.0 x", "*.reddit.com", "-a.com", "a..com"] {
            assert!(!is_valid_domain(bad), "{bad:?}");
        }
        let err = Config::from_toml("[groups.s]\ndomains = [\"https://reddit.com\"]").unwrap_err();
        assert!(err.contains("invalid domain"));
    }
}
