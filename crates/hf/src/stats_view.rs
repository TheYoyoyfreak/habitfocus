//! Shared rendering of daily stats for `hf stats` and the TUI.

use habit_core::snapshot::DayView;
use habit_core::stats::DayStats;

pub const COMPLETED: char = '■';
pub const FOCUSED: char = '▪';
pub const NOTHING: char = '·';

/// One character per day, oldest first.
pub fn day_grid(days: &[DayView], habit: &str) -> String {
    days.iter()
        .map(|day| match day.habits.get(habit) {
            Some(stats) if stats.completions > 0 => COMPLETED,
            Some(stats) if stats.focused_ms > 0 || stats.count > 0 => FOCUSED,
            _ => NOTHING,
        })
        .collect()
}

/// Focused time for `habit` over the last `count` days of `days`.
pub fn focused_in_last(days: &[DayView], habit: &str, count: usize) -> u64 {
    days.iter()
        .rev()
        .take(count)
        .filter_map(|day| day.habits.get(habit))
        .map(|stats| stats.focused_ms)
        .sum()
}

/// Weekday of a day number (days since 1970-01-01, a Thursday), Monday = 0.
pub fn weekday(day: i64) -> usize {
    (day + 3).rem_euclid(7) as usize
}

/// Heatmap cell for one day of a habit: how far it got against its target
/// (timed habits) or goal (logged habits).
pub fn intensity(stats: Option<&DayStats>, timed: bool, target_ms: u64, goal: u64) -> char {
    let Some(s) = stats else { return NOTHING };
    let ratio = if timed { s.focused_ms as f64 / target_ms.max(1) as f64 } else { s.count as f64 / goal.max(1) as f64 };
    match (s.completions, ratio) {
        (c, r) if c >= 2 || r >= 2.0 => '█',
        (1, _) => '▓',
        (_, r) if r >= 1.0 => '▓',
        (_, r) if r >= 0.5 => '▒',
        (_, r) if r > 0.0 => '░',
        _ => NOTHING,
    }
}

const EIGHTHS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// A bar chart of use per hour of the day: `height` rows (top first), two
/// cells per hour, the busiest hour full height. Any use shows at least a
/// sliver.
pub fn hour_chart(hours: &[u64], height: usize) -> Vec<String> {
    let max = hours.iter().copied().max().unwrap_or(0);
    let levels = (height * 8) as u64;
    let filled: Vec<usize> =
        (0..24).map(|h| if max == 0 { 0 } else { (hours.get(h).copied().unwrap_or(0) * levels).div_ceil(max) as usize }).collect();
    (0..height)
        .map(|row| {
            let below = (height - 1 - row) * 8;
            filled.iter().map(|&f| EIGHTHS[f.saturating_sub(below).min(8)].to_string().repeat(2)).collect()
        })
        .collect()
}

/// Hour labels under `hour_chart`: every third hour.
pub fn hour_axis() -> String {
    let mut axis = String::new();
    for hour in (0..24).step_by(3) {
        axis += &format!("{hour:<6}");
    }
    axis
}

/// The busiest hour, as "21:00–22:00".
pub fn peak_hour(hours: &[u64]) -> Option<String> {
    let (hour, &ms) = hours.iter().enumerate().max_by_key(|(_, &ms)| ms)?;
    (ms > 0).then(|| format!("{hour:02}:00–{:02}:00", (hour + 1) % 24))
}

/// Per-hour use summed over several apps.
pub fn sum_hours<'a>(hours: impl IntoIterator<Item = &'a Vec<u64>>) -> Vec<u64> {
    let mut sum = vec![0; 24];
    for app in hours {
        for (total, ms) in sum.iter_mut().zip(app) {
            *total += ms;
        }
    }
    sum
}

/// Two per-day series added up; the shorter one counts as zero where it ends.
pub fn sum_series<T: Copy + Default + std::ops::Add<Output = T>>(a: &[T], b: &[T]) -> Vec<T> {
    (0..a.len().max(b.len()))
        .map(|i| a.get(i).copied().unwrap_or_default() + b.get(i).copied().unwrap_or_default())
        .collect()
}

/// How to show a screen-time key: its name from `[app_names]`, else the host
/// of a `site:` key or the program of a `term:` key, else the app id.
pub fn app_display(names: &std::collections::BTreeMap<String, String>, key: &str) -> String {
    match names.get(key) {
        Some(name) => name.clone(),
        None => strip_key(key).to_string(),
    }
}

/// A screen-time key without its `site:` or `term:` prefix.
pub fn strip_key(key: &str) -> &str {
    key.strip_prefix("site:").or_else(|| key.strip_prefix(habit_core::config::TERMINAL_PREFIX)).unwrap_or(key)
}

/// Apps and sites without a category, when grouping by category.
pub const UNCATEGORIZED: &str = "Uncategorized";

/// One line of screen time: an app or site, or a whole category.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageRow {
    /// The app or site key; `None` for a category.
    pub key: Option<String>,
    pub label: String,
    pub category: Option<String>,
    pub today_ms: u64,
    pub total_ms: u64,
    pub sessions: u32,
    pub sessions_today: u32,
    pub avg_session_ms: u64,
    pub longest_session_ms: u64,
    pub hours: Vec<u64>,
    /// The same for today alone.
    pub hours_today: Vec<u64>,
    /// Use per day over the period, oldest first.
    pub days_ms: Vec<u64>,
    /// Sessions started per day over the period, oldest first.
    pub days_sessions: Vec<u32>,
}

impl UsageRow {
    fn add(&mut self, other: &UsageRow) {
        let active = self.avg_session_ms * u64::from(self.sessions) + other.avg_session_ms * u64::from(other.sessions);
        self.today_ms += other.today_ms;
        self.total_ms += other.total_ms;
        self.sessions += other.sessions;
        self.sessions_today += other.sessions_today;
        self.avg_session_ms = active / u64::from(self.sessions.max(1));
        self.longest_session_ms = self.longest_session_ms.max(other.longest_session_ms);
        self.hours = sum_hours([&self.hours, &other.hours]);
        self.hours_today = sum_hours([&self.hours_today, &other.hours_today]);
        self.days_ms = sum_series(&self.days_ms, &other.days_ms);
        self.days_sessions = sum_series(&self.days_sessions, &other.days_sessions);
    }

    /// The category this row's time counts under in the week chart: its own
    /// label for a category row.
    pub fn group(&self) -> &str {
        match &self.key {
            None => &self.label,
            Some(_) => self.category.as_deref().unwrap_or(UNCATEGORIZED),
        }
    }

    /// Whether `filter` (lowercase) appears in the key, name or category.
    fn matches(&self, filter: &str) -> bool {
        [self.key.as_deref(), Some(&self.label), self.category.as_deref()]
            .into_iter()
            .flatten()
            .any(|text| text.to_lowercase().contains(filter))
    }
}

/// Screen time as rows: apps and sites matching `filter` (by id, name or
/// category; empty matches all), or their categories summed with
/// `by_category`. Most used first.
pub fn usage_rows(
    apps: &[habit_core::snapshot::AppUsageView],
    names: &std::collections::BTreeMap<String, String>,
    categories: &std::collections::BTreeMap<String, String>,
    filter: &str,
    by_category: bool,
) -> Vec<UsageRow> {
    let filter = filter.trim().to_lowercase();
    let rows = apps
        .iter()
        .map(|a| UsageRow {
            key: Some(a.app.clone()),
            label: app_display(names, &a.app),
            category: categories.get(&a.app).cloned(),
            today_ms: a.today_ms,
            total_ms: a.total_ms,
            sessions: a.sessions,
            sessions_today: a.sessions_today,
            avg_session_ms: a.avg_session_ms,
            longest_session_ms: a.longest_session_ms,
            hours: a.hours.clone(),
            hours_today: a.hours_today.clone(),
            days_ms: a.days_ms.clone(),
            days_sessions: a.days_sessions.clone(),
        })
        .filter(|row| row.matches(&filter));
    if !by_category {
        return rows.collect();
    }
    let mut groups: std::collections::BTreeMap<String, UsageRow> = std::collections::BTreeMap::new();
    for row in rows {
        let name = row.category.clone().unwrap_or_else(|| UNCATEGORIZED.to_string());
        let group = groups.entry(name.clone()).or_insert_with(|| UsageRow { label: name, ..UsageRow::default() });
        group.add(&row);
    }
    let mut out: Vec<UsageRow> = groups.into_values().collect();
    out.sort_by(|a, b| b.total_ms.cmp(&a.total_ms).then_with(|| a.label.cmp(&b.label)));
    out
}

/// All rows summed, for the "All" line.
pub fn usage_total(rows: &[UsageRow], label: &str) -> UsageRow {
    let mut total = UsageRow { label: label.to_string(), ..UsageRow::default() };
    for row in rows {
        total.add(row);
    }
    total
}

/// Each day's use split by category, oldest first: `(category, ms)` with the
/// biggest first. Rows may be apps or whole categories.
pub fn week_stacks(rows: &[UsageRow]) -> Vec<Vec<(String, u64)>> {
    let days = rows.iter().map(|r| r.days_ms.len()).max().unwrap_or(0);
    (0..days)
        .map(|day| {
            let mut parts: Vec<(String, u64)> = Vec::new();
            for row in rows {
                let ms = row.days_ms.get(day).copied().unwrap_or(0);
                if ms == 0 {
                    continue;
                }
                match parts.iter_mut().find(|(group, _)| group == row.group()) {
                    Some(part) => part.1 += ms,
                    None => parts.push((row.group().to_string(), ms)),
                }
            }
            parts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            parts
        })
        .collect()
}

pub fn streak_label(days: u32) -> String {
    match days {
        0 => "-".into(),
        1 => "1 day".into(),
        n => format!("{n} days"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use habit_core::stats::DayStats;

    fn day(n: i64, habits: &[(&str, u64, u32)]) -> DayView {
        DayView {
            day: n,
            date: String::new(),
            habits: habits
                .iter()
                .map(|(h, focused_ms, completions)| {
                    (h.to_string(), DayStats { focused_ms: *focused_ms, completions: *completions, count: 0 })
                })
                .collect(),
        }
    }

    #[test]
    fn grid_and_totals() {
        let days = vec![day(1, &[("read", 60_000, 1)]), day(2, &[]), day(3, &[("read", 30_000, 0)])];
        assert_eq!(day_grid(&days, "read"), "■·▪");
        assert_eq!(focused_in_last(&days, "read", 2), 30_000);
        assert_eq!(focused_in_last(&days, "read", 7), 90_000);
        assert_eq!(streak_label(0), "-");
        assert_eq!(streak_label(4), "4 days");
    }

    #[test]
    fn hour_charts() {
        let mut hours = vec![0; 24];
        hours[0] = 80;
        hours[12] = 40;
        hours[23] = 1;
        let rows = hour_chart(&hours, 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].chars().count(), 48);
        assert!(rows[0].starts_with("██") && rows[1].starts_with("██"));
        // Half height: an empty top row and a full bottom row.
        assert_eq!(&rows[0].chars().skip(24).take(2).collect::<String>(), "  ");
        assert_eq!(&rows[1].chars().skip(24).take(2).collect::<String>(), "██");
        assert!(rows[1].ends_with("▁▁"), "small values still show");
        assert!(hour_axis().starts_with("0     3     6"));
        assert_eq!(peak_hour(&hours).as_deref(), Some("00:00–01:00"));
        assert_eq!(peak_hour(&[0; 24]), None);
        assert_eq!(sum_hours([&vec![1; 24], &vec![2; 24]])[5], 3);
        let names = [("steam_app_1".to_string(), "Game".to_string())].into();
        assert_eq!(app_display(&names, "steam_app_1"), "Game");
        assert_eq!(app_display(&names, "site:x.com"), "x.com");
        assert_eq!(app_display(&names, "term:htop"), "htop");
        assert_eq!(app_display(&names, "kitty"), "kitty");
    }

    fn usage(app: &str, total_ms: u64, sessions: u32, avg: u64) -> habit_core::snapshot::AppUsageView {
        habit_core::snapshot::AppUsageView {
            app: app.into(),
            today_ms: total_ms,
            total_ms,
            sessions,
            sessions_today: sessions,
            avg_session_ms: avg,
            longest_session_ms: avg,
            hours: vec![1; 24],
            hours_today: vec![1; 24],
            days_ms: vec![total_ms / 2, total_ms - total_ms / 2],
            days_sessions: vec![0, sessions],
        }
    }

    #[test]
    fn usage_rows_filter_and_group() {
        let apps = [usage("steam_app_1", 600, 2, 300), usage("steam_app_2", 300, 1, 300), usage("kitty", 1000, 3, 300)];
        let names = [("steam_app_1".to_string(), "No Man's Sky".to_string())].into();
        let categories = [("steam_app_1".to_string(), "Games".to_string()), ("steam_app_2".to_string(), "Games".to_string())].into();

        let rows = usage_rows(&apps, &names, &categories, "", false);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].label, "No Man's Sky");
        // By name, id or category, case-insensitive.
        assert_eq!(usage_rows(&apps, &names, &categories, "SKY", false).len(), 1);
        assert_eq!(usage_rows(&apps, &names, &categories, "games", false).len(), 2);
        assert_eq!(usage_rows(&apps, &names, &categories, "kit", false)[0].key.as_deref(), Some("kitty"));

        let groups = usage_rows(&apps, &names, &categories, "", true);
        assert_eq!(groups.iter().map(|g| g.label.as_str()).collect::<Vec<_>>(), [UNCATEGORIZED, "Games"]);
        let games = &groups[1];
        assert_eq!((games.total_ms, games.sessions, games.avg_session_ms, games.key.clone()), (900, 3, 300, None));
        assert_eq!(games.hours[0], 2);
        assert_eq!((games.days_ms.clone(), games.days_sessions.clone()), (vec![450, 450], vec![0, 3]));
        let total = usage_total(&groups, "All");
        assert_eq!((total.total_ms, total.sessions), (1900, 6));
        assert_eq!(total.days_ms, [950, 950]);
    }

    #[test]
    fn week_stacks_by_category() {
        let apps = [usage("steam_app_1", 600, 2, 300), usage("steam_app_2", 300, 1, 300), usage("kitty", 1000, 3, 300)];
        let categories = [("steam_app_1".to_string(), "Games".to_string()), ("steam_app_2".to_string(), "Games".to_string())].into();
        let names = Default::default();
        let by_app = week_stacks(&usage_rows(&apps, &names, &categories, "", false));
        let by_category = week_stacks(&usage_rows(&apps, &names, &categories, "", true));
        assert_eq!(by_app, by_category);
        assert_eq!(by_app[1], [(UNCATEGORIZED.to_string(), 500), ("Games".to_string(), 450)]);
        assert_eq!(sum_series(&[1, 2], &[3]), [4, 2]);
    }

    #[test]
    fn weekdays_and_heat() {
        assert_eq!(weekday(0), 3); // thu
        assert_eq!(weekday(4), 0); // mon 1970-01-05
        assert_eq!(weekday(-1), 2);
        let stats = |focused_ms, completions, count| DayStats { focused_ms, completions, count };
        assert_eq!(intensity(None, true, 10, 1), NOTHING);
        assert_eq!(intensity(Some(&stats(3, 0, 0)), true, 10, 0), '░');
        assert_eq!(intensity(Some(&stats(6, 0, 0)), true, 10, 0), '▒');
        assert_eq!(intensity(Some(&stats(10, 1, 0)), true, 10, 0), '▓');
        assert_eq!(intensity(Some(&stats(0, 2, 40)), false, 0, 20), '█');
        assert_eq!(intensity(Some(&stats(0, 0, 0)), false, 0, 20), NOTHING);
    }
}
