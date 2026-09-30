//! Daily totals per habit and the streaks derived from them. Kept apart from
//! the capped session history so long streaks stay accurate.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DayStats {
    pub focused_ms: u64,
    pub completions: u32,
    /// Logged count of a manual/counter habit.
    #[serde(default)]
    pub count: u64,
}

/// Logical day (see `Engine::day_of`) -> habit -> totals.
pub type Days = BTreeMap<i64, BTreeMap<String, DayStats>>;

/// Logical day -> app id, or `site:<host>` for a browser tab -> focused ms.
pub type AppDays = BTreeMap<i64, BTreeMap<String, u64>>;

pub fn record_app(days: &mut AppDays, day: i64, app: &str, ms: u64) {
    *days.entry(day).or_default().entry(app.to_string()).or_default() += ms;
}

/// Drops days before `first_kept`. Returns whether anything was removed.
pub fn prune_before(days: &mut AppDays, first_kept: i64) -> bool {
    let before = days.len();
    *days = days.split_off(&first_kept);
    days.len() != before
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Streak {
    /// Consecutive days up to today, or up to yesterday while today isn't done
    /// yet (the streak is still alive until the day ends).
    pub current: u32,
    pub best: u32,
}

pub fn record(days: &mut Days, day: i64, habit: &str, focused_ms: u64, completed: bool) {
    let stats = days.entry(day).or_default().entry(habit.to_string()).or_default();
    stats.focused_ms += focused_ms;
    if completed {
        stats.completions += 1;
    }
}

fn streak_of(done: &BTreeSet<i64>, today: i64) -> Streak {
    let mut best = 0;
    let mut run = 0;
    let mut previous = None;
    for &day in done {
        run = if previous == Some(day - 1) { run + 1 } else { 1 };
        best = best.max(run);
        previous = Some(day);
    }
    let mut current = 0;
    let mut day = if done.contains(&today) { today } else { today - 1 };
    while done.contains(&day) {
        current += 1;
        day -= 1;
    }
    Streak { current, best }
}

/// Days on which `habit` was completed at least once.
pub fn habit_streak(days: &Days, today: i64, habit: &str) -> Streak {
    let done = days
        .iter()
        .filter(|(_, habits)| habits.get(habit).is_some_and(|s| s.completions > 0))
        .map(|(day, _)| *day)
        .collect();
    streak_of(&done, today)
}

/// Days on which any habit was completed.
pub fn overall_streak(days: &Days, today: i64) -> Streak {
    let done = days
        .iter()
        .filter(|(_, habits)| habits.values().any(|s| s.completions > 0))
        .map(|(day, _)| *day)
        .collect();
    streak_of(&done, today)
}

/// `YYYY-MM-DD` for a logical day number (days since 1970-01-01).
pub fn civil_date(day: i64) -> String {
    // Howard Hinnant's civil_from_days.
    let z = day + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completed_on(days_done: &[i64]) -> Days {
        let mut days = Days::new();
        for &day in days_done {
            record(&mut days, day, "read", 60_000, true);
        }
        days
    }

    #[test]
    fn current_streak_survives_until_the_day_ends() {
        let days = completed_on(&[1, 2, 3, 5, 6]);
        assert_eq!(habit_streak(&days, 6, "read"), Streak { current: 2, best: 3 });
        // Day 7 not done yet: the streak from day 6 is still alive.
        assert_eq!(habit_streak(&days, 7, "read").current, 2);
        // Day 8: day 7 was missed.
        assert_eq!(habit_streak(&days, 8, "read").current, 0);
        assert_eq!(habit_streak(&days, 8, "read").best, 3);
    }

    #[test]
    fn partial_focus_does_not_count_and_overall_uses_any_habit() {
        let mut days = completed_on(&[10]);
        record(&mut days, 11, "read", 5 * 60_000, false);
        record(&mut days, 11, "walk", 30 * 60_000, true);
        assert_eq!(habit_streak(&days, 11, "read").current, 1);
        assert_eq!(overall_streak(&days, 11), Streak { current: 2, best: 2 });
        assert_eq!(days[&11]["read"], DayStats { focused_ms: 5 * 60_000, completions: 0, count: 0 });
        assert_eq!(habit_streak(&days, 11, "unknown"), Streak::default());
    }

    #[test]
    fn app_days_record_and_prune() {
        let mut days = AppDays::new();
        record_app(&mut days, 1, "zen", 1000);
        record_app(&mut days, 1, "zen", 500);
        record_app(&mut days, 5, "kitty", 10);
        assert_eq!(days[&1]["zen"], 1500);
        assert!(prune_before(&mut days, 3));
        assert_eq!(days.keys().copied().collect::<Vec<_>>(), vec![5]);
        assert!(!prune_before(&mut days, 3));
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(19_723), "2024-01-01");
        assert_eq!(civil_date(20_713), "2026-09-17");
        assert_eq!(civil_date(-1), "1969-12-31");
    }
}
