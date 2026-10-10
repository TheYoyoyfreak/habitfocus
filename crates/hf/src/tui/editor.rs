//! The full-screen form that edits one block or habit. It works on the entry's
//! table as written in config.toml (JSON from `config_entries`) and saves it
//! back with `edit_config`; habitd validates and applies the lock rules. Pure
//! state and key handling, like `app.rs`; drawing lives in `form.rs`.

use habit_core::config::Weekday;
use habit_core::duration::{format_time_of_day, parse_duration, parse_time_of_day};
use habit_ipc::Request;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Groups,
    Habits,
}

impl Section {
    pub fn key(self) -> &'static str {
        match self {
            Section::Groups => "groups",
            Section::Habits => "habits",
        }
    }

    pub fn noun(self) -> &'static str {
        match self {
            Section::Groups => "block",
            Section::Habits => "habit",
        }
    }
}

/// Where the options of a multi-select come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Options {
    Habits,
    Groups,
}

/// Where the picker of a list field gets suggestions from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suggest {
    Apps,
    Sites,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    /// A duration like "20m"; `optional` ones can be cleared.
    Duration { optional: bool },
    /// A whole number ≥ 1; `optional` ones can be cleared.
    Number { optional: bool },
    /// One of `(value, label)`; the first is the default when the key is absent.
    Choice(&'static [(&'static str, &'static str)]),
    List(Suggest),
    /// Allow rules: tables like `{ app = "zed" }`, added from recent use or
    /// typed (`rule_from_text`).
    Rules,
    /// Schedule rules: tables like `{ days = ["mon"], ranges = ["08:00-12:00"] }`,
    /// typed as text (`schedule_from_text`).
    Schedule,
    Multi(Options),
    /// Shown, not editable here.
    ReadOnly,
}

pub struct Field {
    /// Dotted path into the table, e.g. `reward.duration`.
    pub key: &'static str,
    pub label: &'static str,
    pub kind: Kind,
}

const UNLOCK_MODES: &[(&str, &str)] =
    &[("wallclock", "clock time"), ("usage", "minutes of use"), ("rest_of_day", "day pass")];
const REQUIRE: &[(&str, &str)] = &[("any", "one of them"), ("all", "all of them")];

const BLOCK_FIELDS: &[Field] = &[
    Field { key: "name", label: "Name", kind: Kind::Text },
    Field { key: "apps", label: "Apps", kind: Kind::List(Suggest::Apps) },
    Field { key: "domains", label: "Sites", kind: Kind::List(Suggest::Sites) },
    Field { key: "unlock_mode", label: "Unlock mode", kind: Kind::Choice(UNLOCK_MODES) },
    Field { key: "rest_of_day_price", label: "Day pass price", kind: Kind::Duration { optional: false } },
    Field { key: "requires", label: "Do first", kind: Kind::Multi(Options::Habits) },
    Field { key: "require", label: "Needs", kind: Kind::Choice(REQUIRE) },
    Field { key: "schedule", label: "Schedule", kind: Kind::Schedule },
    Field { key: "processes", label: "Processes", kind: Kind::ReadOnly },
];

const HABIT_KINDS: &[(&str, &str)] = &[
    ("apps", "apps: time in allowed windows"),
    ("timer", "timer: while hf tui is focused"),
    ("manual", "manual: done or not"),
    ("counter", "counter: a number toward a goal"),
    ("passive", "passive: counts by itself all day"),
];
const STRICTNESS: &[(&str, &str)] = &[("soft", "soft: pauses when you leave"), ("strict", "strict: pulls focus back")];
const REWARD_MODES: &[(&str, &str)] = &[("bank", "bank as credit"), ("immediate", "unlock right away")];

const HABIT_FIELDS: &[Field] = &[
    Field { key: "name", label: "Name", kind: Kind::Text },
    Field { key: "kind", label: "Kind", kind: Kind::Choice(HABIT_KINDS) },
    Field { key: "target", label: "Target", kind: Kind::Duration { optional: false } },
    Field { key: "goal", label: "Goal", kind: Kind::Number { optional: false } },
    Field { key: "unit", label: "Unit", kind: Kind::Text },
    Field { key: "daily_limit", label: "Max rounds/day", kind: Kind::Number { optional: true } },
    Field { key: "strictness", label: "Strictness", kind: Kind::Choice(STRICTNESS) },
    Field { key: "reward.groups", label: "Rewards", kind: Kind::Multi(Options::Groups) },
    Field { key: "reward.duration", label: "Reward", kind: Kind::Duration { optional: false } },
    Field { key: "reward.mode", label: "Reward mode", kind: Kind::Choice(REWARD_MODES) },
    Field { key: "allow", label: "Counts in", kind: Kind::Rules },
    Field { key: "on_target", label: "At the target", kind: Kind::ReadOnly },
];

/// Keys that only apply to some habit kinds.
fn applies(key: &str, kind: &str) -> bool {
    let timed = matches!(kind, "apps" | "timer");
    match key {
        "target" => timed || kind == "passive",
        "strictness" | "on_target" => timed,
        "allow" => matches!(kind, "apps" | "passive"),
        "goal" | "unit" => kind == "counter",
        "daily_limit" => !timed,
        _ => true,
    }
}

/// One line of the form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// The id of a new entry (editable until it's created).
    Id,
    Field(usize),
    Item { field: usize, index: usize },
    Add { field: usize },
    Option { field: usize, id: String },
}

/// Names and suggestions the form needs from outside: ids and names of
/// habits and groups, and recently used apps and sites with their use per day.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub habits: Vec<(String, String)>,
    pub groups: Vec<(String, String)>,
    /// (app id or `site:<host>`, average ms per day).
    pub recent: Vec<(String, u64)>,
    /// Display names of app ids (`[app_names]`).
    pub app_names: std::collections::BTreeMap<String, String>,
    /// tmux sessions that exist right now, suggested for allow rules.
    pub tmux_sessions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    pub field: usize,
    pub filter: String,
    pub index: usize,
}

pub enum Outcome {
    None,
    Close,
    Send(Request),
}

pub struct Editor {
    pub section: Section,
    pub id: String,
    pub creating: bool,
    /// The id follows the name until the user types one.
    id_edited: bool,
    pub table: Map<String, Value>,
    /// The table as opened, to bring back fields when the habit kind changes back.
    original: Map<String, Value>,
    pub context: Context,
    pub cursor: usize,
    /// Text being typed into the selected row.
    pub input: Option<String>,
    pub picker: Option<Picker>,
    pub dirty: bool,
    pub confirm_discard: bool,
    /// Waiting for habitd to answer a save.
    pub saving: bool,
    pub error: Option<String>,
}

/// Typed text as an allow rule. Words combine into one rule: a plain word
/// is an app id, `site:youtube.com` the site and its subdomains, `term:nvim`
/// (or `program:`) a terminal program, `tmux:thesis` a tmux session, and
/// `title:` / `url:` a regex.
pub fn rule_from_text(text: &str) -> Result<Map<String, Value>, String> {
    let mut rule = Map::new();
    for word in text.split_whitespace() {
        let (key, value) = match word.split_once(':') {
            Some(("site", host)) => ("url", site_pattern(host)),
            Some(("term" | "program", program)) => ("program", program.to_string()),
            Some(("tmux", session)) => ("tmux_session", session.to_string()),
            Some(("title", regex)) => ("title", regex.to_string()),
            Some(("url", regex)) => ("url", regex.to_string()),
            Some(("app", app)) => ("app", app.to_string()),
            _ => ("app", word.to_string()),
        };
        if value.is_empty() {
            return Err(format!("{word:?} needs something after the colon"));
        }
        if rule.insert(key.to_string(), value.into()).is_some() {
            return Err(format!("one rule can only have one {key}; add another rule for an alternative"));
        }
    }
    if rule.is_empty() {
        return Err("type an app id, site:…, term:…, tmux:…".into());
    }
    Ok(rule)
}

const FULL_DAY_NAMES: [&str; 7] = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];

fn weekday(word: &str) -> Result<usize, String> {
    Weekday::NAMES
        .iter()
        .position(|d| *d == word)
        .or_else(|| FULL_DAY_NAMES.iter().position(|d| *d == word))
        .ok_or_else(|| format!("{word:?} isn't a day; use mon, tue, … sun"))
}

/// Typed text as a schedule rule: days and times in any order. Days are
/// names (`mon`), spans (`mon-fri`) or `weekdays`, `weekend`, `daily`;
/// times are `HH:MM-HH:MM`, running into the next day when they end before
/// they start. Commas are optional: "mon-fri 23:00-07:00".
pub fn schedule_from_text(text: &str) -> Result<Map<String, Value>, String> {
    let mut days = Vec::new();
    let mut ranges = Vec::new();
    for word in text.to_lowercase().split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty()) {
        if word.contains(':') {
            let (from, to) =
                word.split_once('-').ok_or_else(|| format!("{word:?} isn't a time range; write it as 23:00-07:00"))?;
            let (from, to) = (parse_time_of_day(from)?, parse_time_of_day(to)?);
            ranges.push(format!("{}-{}", format_time_of_day(from), format_time_of_day(to)));
            continue;
        }
        let span: Vec<usize> = match word {
            "daily" | "everyday" => (0..7).collect(),
            "weekdays" => (0..5).collect(),
            "weekend" | "weekends" => vec![5, 6],
            _ => match word.split_once('-') {
                // Spans may wrap around the week: fri-mon.
                Some((a, b)) => {
                    let (a, b) = (weekday(a)?, weekday(b)?);
                    (0..=(b + 7 - a) % 7).map(|i| (a + i) % 7).collect()
                }
                None => vec![weekday(word)?],
            },
        };
        days.extend(span);
    }
    days.sort_unstable();
    days.dedup();
    ranges.dedup();
    if days.is_empty() || ranges.is_empty() {
        return Err("a rule needs days and times, e.g. \"mon-fri 23:00-07:00\"".into());
    }
    let mut rule = Map::new();
    rule.insert("days".into(), days.iter().map(|&d| Weekday::NAMES[d]).collect::<Vec<_>>().into());
    rule.insert("ranges".into(), ranges.into());
    Ok(rule)
}

/// A schedule rule as text `schedule_from_text` reads back: "mon-fri 23:00-07:00".
pub fn schedule_text(rule: &Value) -> String {
    let mut days: Vec<usize> = strings(rule.get("days")).iter().filter_map(|d| weekday(d).ok()).collect();
    days.sort_unstable();
    days.dedup();
    let mut words = Vec::new();
    if days.len() == 7 {
        words.push("daily".to_string());
    }
    let mut i = 0;
    while i < days.len() && days.len() < 7 {
        // Runs of three or more consecutive days read as a span.
        let mut end = i;
        while end + 1 < days.len() && days[end + 1] == days[end] + 1 {
            end += 1;
        }
        if end - i >= 2 {
            words.push(format!("{}-{}", Weekday::NAMES[days[i]], Weekday::NAMES[days[end]]));
        } else {
            words.extend(days[i..=end].iter().map(|&d| Weekday::NAMES[d].to_string()));
        }
        i = end + 1;
    }
    words.extend(strings(rule.get("ranges")));
    words.join(" ")
}

/// Whether a schedule rule has a range that runs past midnight.
pub fn schedule_overnight(rule: &Value) -> bool {
    strings(rule.get("ranges")).iter().any(|r| {
        let mut ends = r.split('-').map(parse_time_of_day);
        matches!((ends.next(), ends.next()), (Some(Ok(from)), Some(Ok(to))) if to < from)
    })
}

/// The `url` regex of `site:<host>`: the host and its subdomains.
fn site_pattern(host: &str) -> String {
    let host = host.trim_end_matches('/').strip_prefix("www.").unwrap_or(host.trim_end_matches('/'));
    format!("^https?://([^/]*\\.)?{}(/|$)", host.replace('.', "\\."))
}

/// The host of a `url` regex made by `site_pattern`, to show it as a site.
pub fn site_of_pattern(pattern: &str) -> Option<String> {
    let host = pattern.strip_prefix("^https?://([^/]*\\.)?")?.strip_suffix("(/|$)")?;
    Some(host.replace("\\.", "."))
}

/// An id from a name: "Social Media" → "social-media".
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

fn get<'a>(table: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    let mut parts = path.split('.');
    let mut value = table.get(parts.next()?)?;
    for part in parts {
        value = value.get(part)?;
    }
    Some(value)
}

fn set(table: &mut Map<String, Value>, path: &str, value: Option<Value>) {
    match path.split_once('.') {
        None => match value {
            Some(v) => {
                table.insert(path.to_string(), v);
            }
            None => {
                table.remove(path);
            }
        },
        Some((head, rest)) => {
            let child = table.entry(head).or_insert_with(|| Value::Object(Map::new()));
            if !child.is_object() {
                *child = Value::Object(Map::new());
            }
            set(child.as_object_mut().expect("object"), rest, value);
        }
    }
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// The text shown for a scalar value.
pub fn scalar(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

impl Editor {
    pub fn new(section: Section, id: Option<String>, mut table: Map<String, Value>, context: Context) -> Editor {
        if id.is_none() && section == Section::Habits && table.is_empty() {
            // What a habit needs to be valid; the user adjusts it.
            table = serde_json::json!({ "target": "20m", "reward": { "groups": [], "duration": "30m" } })
                .as_object()
                .cloned()
                .unwrap_or_default();
        }
        Editor {
            original: table.clone(),
            section,
            creating: id.is_none(),
            id: id.unwrap_or_default(),
            id_edited: false,
            table,
            context,
            cursor: 0,
            input: None,
            picker: None,
            dirty: false,
            confirm_discard: false,
            saving: false,
            error: None,
        }
    }

    pub fn fields(&self) -> &'static [Field] {
        match self.section {
            Section::Groups => BLOCK_FIELDS,
            Section::Habits => HABIT_FIELDS,
        }
    }

    pub fn value(&self, key: &str) -> Option<&Value> {
        get(&self.table, key)
    }

    pub fn list(&self, key: &str) -> Vec<String> {
        strings(self.value(key))
    }

    /// The items of a list field. A schedule may be written as a single table.
    pub fn items(&self, key: &str) -> Vec<Value> {
        match self.value(key) {
            Some(Value::Array(items)) => items.clone(),
            Some(table @ Value::Object(_)) => vec![table.clone()],
            _ => Vec::new(),
        }
    }

    /// Whether a field applies to the entry as it is now (e.g. the day pass
    /// price only for day-pass blocks).
    fn visible(&self, field: &Field) -> bool {
        if self.section == Section::Habits {
            return match field.key {
                "on_target" => self.value("on_target").is_some() && applies("on_target", self.habit_kind()),
                key => applies(key, self.habit_kind()),
            };
        }
        match field.key {
            "rest_of_day_price" => self.value("unlock_mode").and_then(Value::as_str) == Some("rest_of_day"),
            "require" => self.list("requires").len() > 1,
            "processes" => !self.list("processes").is_empty(),
            _ => true,
        }
    }

    pub fn habit_kind(&self) -> &str {
        self.value("kind").and_then(Value::as_str).unwrap_or("apps")
    }

    /// Switches a habit's kind: drops what doesn't apply to it, brings back
    /// what it had when opened, and fills in what the kind needs.
    fn set_habit_kind(&mut self, kind: &str) {
        self.change("kind", Some(kind.into()));
        for key in ["target", "strictness", "on_target", "allow", "goal", "unit", "daily_limit"] {
            if !applies(key, kind) {
                self.table.remove(key);
            } else if let (None, Some(original)) = (self.table.get(key), self.original.get(key)) {
                self.table.insert(key.into(), original.clone());
            }
        }
        if applies("target", kind) && !self.table.contains_key("target") {
            self.table.insert("target".into(), "20m".into());
        }
        if kind == "counter" && !self.table.contains_key("goal") {
            self.table.insert("goal".into(), 10.into());
        }
    }

    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        if self.creating {
            rows.push(Row::Id);
        }
        for (i, field) in self.fields().iter().enumerate() {
            if !self.visible(field) {
                continue;
            }
            match field.kind {
                Kind::List(_) | Kind::Rules | Kind::Schedule => {
                    let items = self.items(field.key).len();
                    rows.extend((0..items).map(|index| Row::Item { field: i, index }));
                    rows.push(Row::Add { field: i });
                }
                Kind::Multi(options) => {
                    let options = match options {
                        Options::Habits => &self.context.habits,
                        Options::Groups => &self.context.groups,
                    };
                    rows.extend(options.iter().map(|(id, _)| Row::Option { field: i, id: id.clone() }));
                }
                _ => rows.push(Row::Field(i)),
            }
        }
        rows
    }

    pub fn row(&self) -> Option<Row> {
        self.rows().get(self.cursor).cloned()
    }

    /// The field a row belongs to.
    pub fn field_of(&self, row: &Row) -> Option<&'static Field> {
        let fields = self.fields();
        match row {
            Row::Id => None,
            Row::Field(i) | Row::Item { field: i, .. } | Row::Add { field: i } | Row::Option { field: i, .. } => {
                fields.get(*i)
            }
        }
    }

    fn change(&mut self, key: &str, value: Option<Value>) {
        set(&mut self.table, key, value);
        self.dirty = true;
        self.error = None;
        let rows = self.rows().len();
        self.cursor = self.cursor.min(rows.saturating_sub(1));
    }

    /// An app id with its display name, if it has one: "No Man's Sky (steam_app_275850)".
    pub fn app_with_name(&self, id: &str) -> String {
        match self.context.app_names.get(id) {
            Some(name) => format!("{name} ({id})"),
            None => id.to_string(),
        }
    }

    /// Picker suggestions for allow rules, as text `rule_from_text` reads:
    /// recent apps, sites and terminal programs, then tmux sessions, minus
    /// rules the habit has.
    fn rule_suggestions(&self, field: &Field, filter: &str) -> Vec<(String, u64)> {
        let present = self.value(field.key).and_then(Value::as_array).cloned().unwrap_or_default();
        let recent = self.context.recent.iter().map(|(key, per_day)| {
            let text = match key.strip_prefix("site:") {
                Some(host) => format!("site:{}", host.strip_prefix("www.").unwrap_or(host)),
                None => key.clone(),
            };
            (text, *per_day)
        });
        let sessions = self.context.tmux_sessions.iter().map(|s| (format!("tmux:{s}"), 0));
        let mut seen = std::collections::BTreeSet::new();
        recent
            .chain(sessions)
            .filter(|(text, _)| !text.contains(char::is_whitespace) && seen.insert(text.clone()))
            .filter(|(text, _)| rule_from_text(text).is_ok_and(|rule| !present.contains(&Value::Object(rule))))
            .filter(|(text, _)| {
                text.to_lowercase().contains(filter)
                    || self.context.app_names.get(text).is_some_and(|n| n.to_lowercase().contains(filter))
            })
            .collect()
    }

    /// Picker suggestions for a list field: recent apps or sites not in the
    /// list yet, matching the filter (by id or display name).
    pub fn suggestions(&self, picker: &Picker) -> Vec<(String, u64)> {
        let Some(field) = self.fields().get(picker.field) else { return Vec::new() };
        if field.kind == Kind::Rules {
            return self.rule_suggestions(field, &picker.filter.to_lowercase());
        }
        let Kind::List(suggest) = field.kind else { return Vec::new() };
        let present = self.list(field.key);
        let filter = picker.filter.to_lowercase();
        self.context
            .recent
            .iter()
            .filter_map(|(key, per_day)| {
                let value = match (suggest, key.strip_prefix("site:")) {
                    (Suggest::Sites, Some(host)) => host.strip_prefix("www.").unwrap_or(host).to_string(),
                    // Programs in terminals aren't windows: rules can't match them.
                    // Terminals can't be blocked: hf runs in them.
                    (Suggest::Apps, None)
                        if !key.starts_with(habit_core::config::TERMINAL_PREFIX)
                            && !habit_core::config::DEFAULT_TERMINALS.iter().any(|t| t.eq_ignore_ascii_case(key)) =>
                    {
                        key.clone()
                    }
                    _ => return None,
                };
                Some((value, *per_day))
            })
            .filter(|(value, _)| {
                !present.contains(value)
                    && (value.to_lowercase().contains(&filter)
                        || self.context.app_names.get(value).is_some_and(|n| n.to_lowercase().contains(&filter)))
            })
            .collect()
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        if self.confirm_discard {
            self.confirm_discard = false;
            return if key.code == KeyCode::Char('y') { Outcome::Close } else { Outcome::None };
        }
        if self.picker.is_some() {
            self.handle_picker_key(key);
            return Outcome::None;
        }
        if self.input.is_some() {
            self.handle_input_key(key);
            return Outcome::None;
        }
        if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return self.save();
        }
        let rows = self.rows();
        let row = rows.get(self.cursor).cloned();
        let field = row.as_ref().and_then(|r| self.field_of(r));
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if self.dirty {
                    self.confirm_discard = true;
                    return Outcome::None;
                }
                return Outcome::Close;
            }
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.cursor = (self.cursor + 1).min(rows.len().saturating_sub(1)),
            KeyCode::Char('a') if matches!(field.map(|f| f.kind), Some(Kind::Schedule)) => {
                // Typing on the add row appends a rule.
                let add = row.as_ref().and_then(|r| match r {
                    Row::Item { field, .. } | Row::Add { field } => Some(Row::Add { field: *field }),
                    _ => None,
                });
                if let Some(at) = add.and_then(|add| rows.iter().position(|r| *r == add)) {
                    self.cursor = at;
                    self.input = Some(String::new());
                }
            }
            KeyCode::Char('a') if matches!(field.map(|f| f.kind), Some(Kind::List(_) | Kind::Rules)) => self.open_picker(),
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                if let (Some(Row::Item { index, .. }), Some(field)) = (&row, field) {
                    let mut items = self.items(field.key);
                    if *index < items.len() {
                        items.remove(*index);
                    }
                    self.change(field.key, (!items.is_empty()).then(|| items.into()));
                }
            }
            KeyCode::Enter | KeyCode::Char(' ' | 'h' | 'l') | KeyCode::Left | KeyCode::Right => {
                let back = matches!(key.code, KeyCode::Left | KeyCode::Char('h'));
                match (&row, field.map(|f| f.kind)) {
                    (Some(Row::Id), _) => self.input = Some(self.id.clone()),
                    (Some(Row::Add { .. }), Some(Kind::Schedule)) if !back => self.input = Some(String::new()),
                    (Some(Row::Item { field, index }), Some(Kind::Schedule)) if !back => {
                        let items = self.items(self.fields()[*field].key);
                        self.input = Some(items.get(*index).map(schedule_text).unwrap_or_default());
                    }
                    (Some(Row::Add { .. }), _) => self.open_picker(),
                    (Some(Row::Option { id, .. }), Some(_)) => {
                        let key = field.expect("field").key;
                        let mut chosen = self.list(key);
                        match chosen.iter().position(|c| c == id) {
                            Some(i) => {
                                chosen.remove(i);
                            }
                            None => chosen.push(id.clone()),
                        }
                        // A reward needs its (possibly empty) list of groups.
                        let keep = key == "reward.groups";
                        self.change(key, (keep || !chosen.is_empty()).then(|| chosen.into()));
                    }
                    (Some(Row::Field(_)), Some(Kind::Choice(choices))) => {
                        let key = field.expect("field").key;
                        let current = self.value(key).and_then(Value::as_str).unwrap_or(choices[0].0);
                        let at = choices.iter().position(|(v, _)| *v == current).unwrap_or(0);
                        let next = if back { at + choices.len() - 1 } else { at + 1 } % choices.len();
                        if key == "kind" {
                            self.set_habit_kind(choices[next].0);
                        } else {
                            self.change(key, Some(choices[next].0.into()));
                        }
                    }
                    (Some(Row::Field(_)), Some(Kind::Text | Kind::Duration { .. } | Kind::Number { .. }))
                        if matches!(key.code, KeyCode::Enter) =>
                    {
                        self.input = Some(scalar(self.value(field.expect("field").key)));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Outcome::None
    }

    fn open_picker(&mut self) {
        let field = self.row().and_then(|r| match r {
            Row::Item { field, .. } | Row::Add { field } => Some(field),
            _ => None,
        });
        if let Some(field) = field {
            self.picker = Some(Picker { field, filter: String::new(), index: 0 });
        }
    }

    fn handle_picker_key(&mut self, key: KeyEvent) {
        let Some(mut picker) = self.picker.take() else { return };
        let options = self.suggestions(&picker);
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Up => picker.index = picker.index.saturating_sub(1),
            KeyCode::Down => picker.index = (picker.index + 1).min(options.len().saturating_sub(1)),
            KeyCode::Backspace => {
                picker.filter.pop();
                picker.index = 0;
                self.error = None;
            }
            KeyCode::Enter => {
                let typed = picker.filter.trim().to_string();
                let chosen = options.get(picker.index).map(|(v, _)| v.clone()).or((!typed.is_empty()).then_some(typed));
                let (Some(value), Some(field)) = (chosen, self.fields().get(picker.field)) else { return };
                let item = if field.kind == Kind::Rules {
                    match rule_from_text(&value) {
                        Ok(rule) => Value::Object(rule),
                        Err(e) => {
                            // Keep the picker open to fix the text.
                            self.error = Some(e);
                            self.picker = Some(picker);
                            return;
                        }
                    }
                } else {
                    Value::from(value)
                };
                let mut items = self.value(field.key).and_then(Value::as_array).cloned().unwrap_or_default();
                items.push(item);
                self.change(field.key, Some(items.into()));
                return;
            }
            KeyCode::Char(c) if !c.is_control() => {
                picker.filter.push(c);
                picker.index = 0;
                self.error = None;
            }
            _ => {}
        }
        self.picker = Some(picker);
    }

    fn handle_input_key(&mut self, key: KeyEvent) {
        let Some(mut input) = self.input.take() else { return };
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) if !c.is_control() && input.chars().count() < 60 => input.push(c),
            KeyCode::Enter => {
                if let Err(e) = self.commit_input(input.trim()) {
                    self.error = Some(e);
                    self.input = Some(input);
                }
                return;
            }
            _ => {}
        }
        self.input = Some(input);
    }

    /// Stores typed text into the selected row, checking its format.
    fn commit_input(&mut self, text: &str) -> Result<(), String> {
        let row = self.row();
        if row == Some(Row::Id) {
            let id = slug(text);
            if id.is_empty() {
                return Err("the id needs a letter or digit".into());
            }
            self.id = id;
            self.id_edited = true;
            self.dirty = true;
            return Ok(());
        }
        let Some(field) = row.as_ref().and_then(|r| self.field_of(r)) else { return Ok(()) };
        if field.kind == Kind::Schedule {
            let mut items = self.items(field.key);
            let rule = if text.is_empty() { None } else { Some(Value::Object(schedule_from_text(text)?)) };
            match (row, rule) {
                (Some(Row::Item { index, .. }), Some(rule)) if index < items.len() => items[index] = rule,
                // Clearing a rule's text removes it.
                (Some(Row::Item { index, .. }), None) if index < items.len() => {
                    items.remove(index);
                }
                (Some(Row::Add { .. }), Some(rule)) => items.push(rule),
                _ => return Ok(()),
            }
            self.change(field.key, (!items.is_empty()).then(|| items.into()));
            return Ok(());
        }
        let value = match field.kind {
            Kind::Text if text.is_empty() => None,
            Kind::Text => Some(Value::from(text)),
            Kind::Duration { optional } | Kind::Number { optional } if text.is_empty() => {
                // A passive habit without a daily goal is only tracked.
                let optional = optional || (field.key == "target" && self.habit_kind() == "passive");
                if !optional {
                    return Err(format!("{} can't be empty", field.label));
                }
                None
            }
            Kind::Duration { .. } => {
                parse_duration(text)?;
                Some(Value::from(text))
            }
            Kind::Number { .. } => match text.parse::<u64>() {
                Ok(n) if n >= 1 => Some(Value::from(n)),
                _ => return Err(format!("{} must be a whole number of at least 1", field.label)),
            },
            _ => return Ok(()),
        };
        if field.key == "name" && self.creating && !self.id_edited {
            self.id = slug(text);
        }
        self.change(field.key, value);
        Ok(())
    }

    fn save(&mut self) -> Outcome {
        if self.id.is_empty() {
            self.error = Some(format!("give the {} a name first", self.section.noun()));
            return Outcome::None;
        }
        self.saving = true;
        self.error = None;
        Outcome::Send(Request::EditConfig {
            section: self.section.key().into(),
            id: self.id.clone(),
            table: Some(Value::Object(self.table.clone())),
            create: self.creating,
        })
    }
}

/// TOML for a value as habitd writes new values: inline arrays and tables.
pub fn toml_inline(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => format!("{s:?}"),
        Value::Array(items) => format!("[{}]", items.iter().map(toml_inline).collect::<Vec<_>>().join(", ")),
        Value::Object(map) => {
            let items: Vec<String> = map.iter().map(|(k, v)| format!("{k} = {}", toml_inline(v))).collect();
            format!("{{ {} }}", items.join(", "))
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;
    use serde_json::json;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(editor: &mut Editor, text: &str) {
        for c in text.chars() {
            editor.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn context() -> Context {
        Context {
            habits: vec![("walk".into(), "Walk".into()), ("read".into(), "Reading".into())],
            groups: vec![("social".into(), "Social".into())],
            recent: vec![
                ("discord".into(), 30 * 60_000),
                ("site:www.youtube.com".into(), 90 * 60_000),
                ("kitty".into(), 1),
                ("steam_app_275850".into(), 60_000),
            ],
            app_names: [("steam_app_275850".to_string(), "No Man's Sky".to_string())].into(),
            tmux_sessions: vec!["thesis".into()],
        }
    }

    fn block() -> Editor {
        let table = json!({ "name": "Social", "apps": ["discord"], "schedule": { "days": ["mon"], "ranges": ["08:00-12:00"] } });
        Editor::new(Section::Groups, Some("social".into()), table.as_object().unwrap().clone(), context())
    }

    fn go_to(editor: &mut Editor, row: Row) {
        editor.cursor = editor.rows().iter().position(|r| *r == row).expect("row exists");
    }

    fn saved(editor: &mut Editor) -> Value {
        match editor.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)) {
            Outcome::Send(Request::EditConfig { table: Some(table), .. }) => table,
            _ => panic!("no save"),
        }
    }

    #[test]
    fn rows_follow_the_table() {
        let e = block();
        let rows = e.rows();
        assert_eq!(rows[0], Row::Field(0)); // name, no id row when editing
        assert!(rows.contains(&Row::Item { field: 1, index: 0 }));
        assert!(rows.contains(&Row::Add { field: 2 }));
        assert!(rows.contains(&Row::Option { field: 5, id: "read".into() }));
        assert!(!rows.contains(&Row::Field(4)), "price only for day passes");
        assert!(!rows.contains(&Row::Field(6)), "any/all only with two or more");
    }

    #[test]
    fn edits_choices_lists_and_requirements() {
        let mut e = block();
        go_to(&mut e, Row::Field(3));
        e.handle_key(key(KeyCode::Char('l')));
        e.handle_key(key(KeyCode::Char('l')));
        assert_eq!(e.value("unlock_mode"), Some(&json!("rest_of_day")));
        assert!(e.rows().contains(&Row::Field(4)));
        go_to(&mut e, Row::Field(4));
        e.handle_key(key(KeyCode::Enter));
        typed(&mut e, "soon");
        e.handle_key(key(KeyCode::Enter));
        assert!(e.error.is_some() && e.input.is_some(), "bad durations stay in the input");
        e.handle_key(key(KeyCode::Esc));
        e.handle_key(key(KeyCode::Enter));
        typed(&mut e, "1h");
        e.handle_key(key(KeyCode::Enter));
        assert_eq!(e.value("rest_of_day_price"), Some(&json!("1h")));

        // Sites from screen time, without www.
        go_to(&mut e, Row::Add { field: 2 });
        e.handle_key(key(KeyCode::Enter));
        let picker = e.picker.clone().unwrap();
        assert_eq!(e.suggestions(&picker), [("youtube.com".to_string(), 90 * 60_000)]);
        e.handle_key(key(KeyCode::Enter));
        // Or typed.
        e.handle_key(key(KeyCode::Char('a')));
        typed(&mut e, "x.com");
        e.handle_key(key(KeyCode::Enter));
        assert_eq!(e.list("domains"), ["youtube.com", "x.com"]);
        // Apps offer only apps not in the list yet.
        go_to(&mut e, Row::Add { field: 1 });
        e.handle_key(key(KeyCode::Char('a')));
        let picker = e.picker.clone().unwrap();
        assert_eq!(e.suggestions(&picker).iter().map(|s| s.0.as_str()).collect::<Vec<_>>(), ["steam_app_275850"], "not the terminal");
        // Found by its display name too, and added by id.
        typed(&mut e, "man's");
        e.handle_key(key(KeyCode::Enter));
        assert_eq!(e.list("apps"), ["discord", "steam_app_275850"]);
        assert_eq!(e.app_with_name("steam_app_275850"), "No Man's Sky (steam_app_275850)");
        go_to(&mut e, Row::Item { field: 1, index: 1 });
        e.handle_key(key(KeyCode::Char('d')));
        go_to(&mut e, Row::Item { field: 1, index: 0 });
        e.handle_key(key(KeyCode::Char('d')));
        assert!(e.value("apps").is_none(), "an emptied list is dropped");

        go_to(&mut e, Row::Option { field: 5, id: "walk".into() });
        e.handle_key(key(KeyCode::Char(' ')));
        go_to(&mut e, Row::Option { field: 5, id: "read".into() });
        e.handle_key(key(KeyCode::Char(' ')));
        go_to(&mut e, Row::Field(6));
        e.handle_key(key(KeyCode::Right));

        let table = saved(&mut e);
        assert_eq!(table["requires"], json!(["walk", "read"]));
        assert_eq!(table["require"], "all");
        assert_eq!(table["schedule"]["ranges"][0], "08:00-12:00", "untouched fields are kept as written");
        assert!(e.saving);
    }

    #[test]
    fn schedules_read_as_text() {
        let rule = |text: &str| Value::Object(schedule_from_text(text).unwrap());
        assert_eq!(rule("mon-fri 23:00-07:00"), json!({ "days": ["mon", "tue", "wed", "thu", "fri"], "ranges": ["23:00-07:00"] }));
        assert_eq!(rule("Sat, sunday 9:00-12:00, 14:00-18:00")["ranges"], json!(["09:00-12:00", "14:00-18:00"]));
        assert_eq!(rule("fri-mon 22:00-02:00")["days"], json!(["mon", "fri", "sat", "sun"]), "spans wrap the week");
        assert_eq!(rule("weekend weekdays 08:00-09:00")["days"].as_array().unwrap().len(), 7);
        for bad in ["mon-fri", "23:00-07:00", "mo 08:00-09:00", "mon 8-9", "mon 25:00-07:00"] {
            assert!(schedule_from_text(bad).is_err(), "{bad}");
        }
        for (text, shown) in [
            ("sun-thu 23:00-07:00", "mon-thu sun 23:00-07:00"),
            ("mon tue 08:00-12:00", "mon tue 08:00-12:00"),
            ("daily 00:00-00:00", "daily 00:00-00:00"),
            ("mon wed-fri 08:00-12:00", "mon wed-fri 08:00-12:00"),
        ] {
            assert_eq!(schedule_text(&rule(text)), shown);
            assert_eq!(rule(shown), rule(text), "{shown} reads back");
        }
        assert!(schedule_overnight(&rule("mon 23:00-07:00")) && !schedule_overnight(&rule("mon 07:00-23:00")));
    }

    #[test]
    fn edits_schedules() {
        let mut e = block();
        // A single table in the file shows as one rule.
        assert!(e.rows().contains(&Row::Item { field: 7, index: 0 }));
        go_to(&mut e, Row::Item { field: 7, index: 0 });
        e.handle_key(key(KeyCode::Enter));
        assert_eq!(e.input.as_deref(), Some("mon 08:00-12:00"));
        e.handle_key(key(KeyCode::Esc));
        // `a` on a rule types a new one.
        e.handle_key(key(KeyCode::Char('a')));
        assert_eq!(e.row(), Some(Row::Add { field: 7 }));
        typed(&mut e, "weekdays 23-7");
        e.handle_key(key(KeyCode::Enter));
        assert!(e.error.is_some() && e.input.is_some(), "bad times stay in the input");
        e.input = Some(String::new());
        typed(&mut e, "weekdays 23:00-7:00");
        e.handle_key(key(KeyCode::Enter));
        assert!(e.input.is_none(), "{:?}", e.error);
        // Editing a rule replaces it.
        go_to(&mut e, Row::Item { field: 7, index: 0 });
        e.handle_key(key(KeyCode::Enter));
        e.input = Some(String::new());
        typed(&mut e, "sat 10:00-11:00");
        e.handle_key(key(KeyCode::Enter));
        let table = saved(&mut e);
        assert_eq!(
            table["schedule"],
            json!([
                { "days": ["sat"], "ranges": ["10:00-11:00"] },
                { "days": ["mon", "tue", "wed", "thu", "fri"], "ranges": ["23:00-07:00"] },
            ])
        );
        // Deleting every rule drops the schedule: the block holds around the clock.
        e.saving = false;
        go_to(&mut e, Row::Item { field: 7, index: 1 });
        e.handle_key(key(KeyCode::Char('d')));
        go_to(&mut e, Row::Item { field: 7, index: 0 });
        e.handle_key(key(KeyCode::Char('d')));
        assert!(e.value("schedule").is_none());
    }

    #[test]
    fn new_entries_take_their_id_from_the_name() {
        let mut e = Editor::new(Section::Groups, None, Map::new(), context());
        assert_eq!(e.rows()[0], Row::Id);
        assert!(matches!(e.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)), Outcome::None));
        assert!(e.error.is_some());
        go_to(&mut e, Row::Field(0));
        e.handle_key(key(KeyCode::Enter));
        typed(&mut e, "Late Night!");
        e.handle_key(key(KeyCode::Enter));
        assert_eq!(e.id, "late-night");
        match e.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)) {
            Outcome::Send(Request::EditConfig { id, create, .. }) => assert_eq!((id.as_str(), create), ("late-night", true)),
            _ => panic!("no save"),
        }
    }

    #[test]
    fn discarding_changes_asks_first() {
        let mut e = block();
        assert!(matches!(e.handle_key(key(KeyCode::Esc)), Outcome::Close));
        go_to(&mut e, Row::Field(3));
        e.handle_key(key(KeyCode::Char(' ')));
        assert!(matches!(e.handle_key(key(KeyCode::Esc)), Outcome::None));
        assert!(e.confirm_discard);
        assert!(matches!(e.handle_key(key(KeyCode::Char('n'))), Outcome::None));
        e.handle_key(key(KeyCode::Esc));
        assert!(matches!(e.handle_key(key(KeyCode::Char('y'))), Outcome::Close));
    }

    fn habit() -> Editor {
        let table = json!({
            "name": "Reading", "target": "20m", "strictness": "strict", "allow": [{ "app": "zathura" }],
            "reward": { "groups": ["social"], "duration": "1h" }
        });
        Editor::new(Section::Habits, Some("read".into()), table.as_object().unwrap().clone(), context())
    }

    #[test]
    fn typed_text_becomes_allow_rules() {
        let rule = |text: &str| rule_from_text(text).map(Value::Object);
        assert_eq!(rule("zed"), Ok(json!({ "app": "zed" })));
        assert_eq!(rule("kitty tmux:thesis term:nvim"), Ok(json!({ "app": "kitty", "tmux_session": "thesis", "program": "nvim" })));
        assert_eq!(rule("program:claude"), Ok(json!({ "program": "claude" })));
        assert!(rule("tmux:").unwrap_err().contains("after the colon"));
        assert!(rule("zed code").unwrap_err().contains("only have one app"));
        assert!(rule("  ").is_err());

        // A site rule matches the site and its subdomains, as habitd reads it.
        let url = rule_from_text("site:www.github.com").unwrap()["url"].as_str().unwrap().to_string();
        assert_eq!(site_of_pattern(&url).as_deref(), Some("github.com"));
        let config = habit_core::Config::from_toml(&format!(
            "[habits.h]\nkind = \"passive\"\nallow = [{{ url = {} }}]\n",
            toml_inline(&Value::from(url))
        ))
        .unwrap();
        let allows = |u: &str| config.habits["h"].allows("zen", "", Some(u), Default::default());
        assert!(allows("https://github.com/") && allows("https://gist.github.com/x") && allows("https://github.com"));
        assert!(!allows("https://notgithub.com/") && !allows("https://github.com.evil.io/"));
    }

    #[test]
    fn allow_rules_are_added_and_removed_in_the_form() {
        let mut e = Editor::new(Section::Habits, None, Map::new(), context());
        go_to(&mut e, Row::Field(1));
        for _ in 0..4 {
            e.handle_key(key(KeyCode::Right)); // apps → … → passive
        }
        assert_eq!(e.habit_kind(), "passive");
        let add = Row::Add { field: 10 };
        go_to(&mut e, add.clone());

        // Suggestions: recent apps, sites and tmux sessions, as rules.
        e.handle_key(key(KeyCode::Enter));
        let picker = e.picker.clone().unwrap();
        let texts: Vec<String> = e.suggestions(&picker).into_iter().map(|(t, _)| t).collect();
        assert!(texts.contains(&"site:youtube.com".to_string()) && texts.contains(&"tmux:thesis".to_string()), "{texts:?}");
        typed(&mut e, "thes");
        e.handle_key(key(KeyCode::Enter));
        assert_eq!(e.value("allow"), Some(&json!([{ "tmux_session": "thesis" }])));

        // Typed words combine; mistakes keep the picker open.
        go_to(&mut e, add.clone());
        e.handle_key(key(KeyCode::Char('a')));
        typed(&mut e, "kitty term:");
        e.handle_key(key(KeyCode::Enter));
        assert!(e.picker.is_some() && e.error.is_some());
        typed(&mut e, "nvim");
        assert!(e.error.is_none(), "typing clears the error");
        e.handle_key(key(KeyCode::Enter));
        assert_eq!(e.value("allow"), Some(&json!([{ "tmux_session": "thesis" }, { "app": "kitty", "program": "nvim" }])));

        // The one that's there isn't suggested again; d removes one.
        go_to(&mut e, add);
        e.handle_key(key(KeyCode::Enter));
        let picker = e.picker.clone().unwrap();
        assert!(!e.suggestions(&picker).iter().any(|(t, _)| t == "tmux:thesis"));
        e.handle_key(key(KeyCode::Esc));
        go_to(&mut e, Row::Item { field: 10, index: 0 });
        e.handle_key(key(KeyCode::Char('d')));
        assert_eq!(e.value("allow"), Some(&json!([{ "app": "kitty", "program": "nvim" }])));

        // A passive habit may drop its target to be only tracked.
        go_to(&mut e, Row::Field(2));
        e.handle_key(key(KeyCode::Enter));
        for _ in 0..3 {
            e.handle_key(key(KeyCode::Backspace));
        }
        e.handle_key(key(KeyCode::Enter));
        assert!(e.error.is_none() && e.value("target").is_none());
    }

    fn visible_keys(e: &Editor) -> Vec<&'static str> {
        e.rows().iter().filter_map(|r| e.field_of(r)).map(|f| f.key).collect()
    }

    #[test]
    fn habit_fields_follow_the_kind() {
        let mut e = habit();
        let keys = visible_keys(&e);
        assert!(keys.contains(&"target") && keys.contains(&"allow") && !keys.contains(&"goal"), "{keys:?}");
        go_to(&mut e, Row::Field(1));
        e.handle_key(key(KeyCode::Right)); // timer
        assert!(e.value("allow").is_none() && e.value("target").is_some());
        e.handle_key(key(KeyCode::Right)); // manual
        assert!(e.value("target").is_none() && e.value("strictness").is_none());
        assert!(visible_keys(&e).contains(&"daily_limit"));
        e.handle_key(key(KeyCode::Right)); // counter
        assert_eq!(e.value("goal"), Some(&json!(10)));
        assert!(visible_keys(&e).contains(&"unit"));
        e.handle_key(key(KeyCode::Right)); // passive: a daily goal and allow rules, no session fields
        let keys = visible_keys(&e);
        assert!(keys.contains(&"target") && keys.contains(&"allow") && keys.contains(&"daily_limit"), "{keys:?}");
        assert!(!keys.contains(&"strictness") && e.value("goal").is_none());
        e.handle_key(key(KeyCode::Right)); // back to apps: what it had returns
        assert_eq!(e.value("allow"), Some(&json!([{ "app": "zathura" }])));
        assert_eq!(e.value("strictness"), Some(&json!("strict")));
        assert!(e.value("goal").is_none());
    }

    #[test]
    fn habit_rewards_and_numbers() {
        let mut e = habit();
        go_to(&mut e, Row::Option { field: 7, id: "social".into() });
        e.handle_key(key(KeyCode::Enter));
        assert_eq!(e.value("reward.groups"), Some(&json!([])), "the list stays, empty");
        go_to(&mut e, Row::Field(8));
        e.handle_key(key(KeyCode::Enter));
        for _ in 0..2 {
            e.handle_key(key(KeyCode::Backspace));
        }
        typed(&mut e, "45m");
        e.handle_key(key(KeyCode::Enter));
        go_to(&mut e, Row::Field(1));
        e.handle_key(key(KeyCode::Left)); // passive
        e.handle_key(key(KeyCode::Left)); // counter
        go_to(&mut e, Row::Field(3));
        e.handle_key(key(KeyCode::Enter));
        e.handle_key(key(KeyCode::Backspace));
        e.handle_key(key(KeyCode::Backspace));
        typed(&mut e, "0");
        e.handle_key(key(KeyCode::Enter));
        assert!(e.error.as_deref().unwrap_or_default().contains("at least 1"));
        e.handle_key(key(KeyCode::Backspace));
        typed(&mut e, "50");
        e.handle_key(key(KeyCode::Enter));
        let table = saved(&mut e);
        assert_eq!(table["reward"], json!({ "groups": [], "duration": "45m" }));
        assert_eq!(table["goal"], 50);
        assert!(table.get("target").is_none() && table.get("allow").is_none());
    }

    #[test]
    fn new_habits_start_valid() {
        let e = Editor::new(Section::Habits, None, Map::new(), context());
        assert_eq!(e.value("target"), Some(&json!("20m")));
        assert_eq!(e.value("reward.duration"), Some(&json!("30m")));
    }

    #[test]
    fn helpers() {
        assert_eq!(slug("  Social Media "), "social-media");
        assert_eq!(slug("a__b"), "a__b");
        assert_eq!(toml_inline(&json!({ "groups": ["a"], "duration": "1h" })), r#"{ duration = "1h", groups = ["a"] }"#);
    }
}
