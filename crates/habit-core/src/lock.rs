//! Commitment lock: while it is active, config changes that make things easier
//! are held back until it ends, and stopping habitd extends it.

use crate::config::{Config, Habit, HabitKind, RequireMode, Strictness, Weekday, MINUTES_PER_DAY, MINUTES_PER_WEEK};
use crate::duration::{format_duration, format_duration_config, format_time_of_day};
use serde::{Deserialize, Serialize};

/// Ending a lock early takes effect this long after it was requested.
pub const END_DELAY_MS: u64 = 24 * 3_600_000;
/// Downtime while locked shorter than this is ignored (restarts, upgrades).
pub const TAMPER_GRACE_MS: u64 = 60_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lock {
    /// Unix ms the commitment runs until (extended by tampering).
    pub until: u64,
    /// Config text in force for the lock. Reloads that only tighten it replace it.
    pub baseline: String,
    /// When the user asked to end the lock early; it ends `END_DELAY_MS` later.
    #[serde(default)]
    pub end_requested_at: Option<u64>,
    /// Easier changes found in the config file, applied when the lock ends.
    #[serde(default)]
    pub pending: Vec<String>,
    /// Total time the lock was extended for unexplained daemon downtime.
    #[serde(default)]
    pub extended_ms: u64,
}

impl Lock {
    pub fn ends_at(&self) -> u64 {
        match self.end_requested_at {
            Some(requested) => self.until.min(requested + END_DELAY_MS),
            None => self.until,
        }
    }
}

/// Written regularly by habitd so a restart can tell how long it was down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub wall_ms: u64,
    /// `/proc/sys/kernel/random/boot_id`: changes on reboot.
    pub boot_id: String,
    /// CLOCK_MONOTONIC in ms: does not advance while suspended.
    pub monotonic_ms: u64,
    /// Set when habitd stopped because the session or system shut down.
    #[serde(default)]
    pub clean_shutdown: bool,
}

/// Awake time habitd was not running that isn't explained by a reboot,
/// suspend, logout or shutdown.
pub fn unexplained_downtime(prev: &Heartbeat, boot_id: &str, monotonic_now: u64, grace_ms: u64) -> Option<u64> {
    if prev.clean_shutdown || prev.boot_id != boot_id {
        return None;
    }
    let down = monotonic_now.saturating_sub(prev.monotonic_ms);
    (down > grace_ms).then_some(down)
}

fn contains_ignore_case(list: &[String], item: &str) -> bool {
    list.iter().any(|x| x.eq_ignore_ascii_case(item))
}

/// Reward per millisecond of target, summed over rewarded groups. Only
/// habits measured in time have one (0 for a passive habit that only
/// tracks); logged habits aren't comparable.
fn reward_rate(habit: &Habit) -> Option<f64> {
    if !habit.kind.tracks_time() {
        None
    } else if !habit.has_reward() || habit.target == 0 {
        Some(0.0)
    } else {
        Some(habit.reward.duration as f64 * habit.reward.groups.len() as f64 / habit.target as f64)
    }
}

/// Changes in `new` that make habits easier than `baseline`, described for
/// the user. Empty means `new` is equally strict or stricter.
pub fn weakenings(baseline: &Config, new: &Config) -> Vec<String> {
    // `app_names`, `app_categories`, `category_rules`, `sound`, `terminals`,
    // `terminal_programs`, `auto_categories` and `update_check` don't change what is required or
    // blocked (only how screen time is labelled), so they never weaken anything.
    let mut out = Vec::new();
    let (old_g, new_g) = (&baseline.general, &new.general);
    if new_g.idle_timeout > old_g.idle_timeout {
        out.push(format!(
            "raises idle timeout {} → {}",
            format_duration_config(old_g.idle_timeout),
            format_duration_config(new_g.idle_timeout)
        ));
    }
    if new_g.emergency_penalty < old_g.emergency_penalty {
        out.push(format!(
            "lowers emergency penalty {} → {}",
            format_duration_config(old_g.emergency_penalty),
            format_duration_config(new_g.emergency_penalty)
        ));
    }
    if new_g.day_start != old_g.day_start {
        out.push(format!(
            "moves the day start {} → {}",
            format_time_of_day(old_g.day_start),
            format_time_of_day(new_g.day_start)
        ));
    }
    for app in &new_g.strict_exempt_apps {
        if !contains_ignore_case(&old_g.strict_exempt_apps, app) {
            out.push(format!("exempts {app} from strict sessions"));
        }
    }
    for browser in &old_g.browsers {
        if !contains_ignore_case(&new_g.browsers, browser) {
            out.push(format!("stops guarding the {browser} browser"));
        }
    }
    // `program` and `tmux_session` allow rules match in any terminal.
    if new.has_terminal_rules() {
        for terminal in &new_g.terminals {
            if !contains_ignore_case(&old_g.terminals, terminal) {
                out.push(format!("lets program and tmux session rules match in {terminal}"));
            }
        }
    }

    for (id, group) in &baseline.groups {
        let name = baseline.group_name(id);
        let Some(new_group) = new.groups.get(id) else {
            out.push(format!("removes group {name}"));
            continue;
        };
        for (what, old_items, new_items) in [
            ("app", &group.apps, &new_group.apps),
            ("process", &group.processes, &new_group.processes),
            ("domain", &group.domains, &new_group.domains),
        ] {
            for item in old_items {
                if !contains_ignore_case(new_items, item) {
                    out.push(format!("removes {what} {item} from {name}"));
                }
            }
        }
        if new_group.unlock_mode > group.unlock_mode {
            out.push(format!(
                "unlocks {name} more generously ({} → {})",
                group.unlock_mode.name(),
                new_group.unlock_mode.name()
            ));
        }
        for habit in &group.requires {
            if !new_group.requires.contains(habit) {
                out.push(format!("stops requiring {} for {name}", baseline.habit_name(habit)));
            }
        }
        if group.require == RequireMode::All && new_group.require == RequireMode::Any && group.requires.len() > 1 {
            out.push(format!("lets one required habit unlock {name} instead of all"));
        }
        if let (Some(old), Some(new)) = (group.rest_of_day_price, new_group.rest_of_day_price) {
            if new < old {
                out.push(format!(
                    "lowers the {name} day pass {} → {}",
                    format_duration(old),
                    format_duration(new)
                ));
            }
        }
        // Compare blocked minutes, not fields: adding a schedule to a group
        // that blocked around the clock shrinks it, widening one does not.
        if let Some(minute) = (0..MINUTES_PER_WEEK).find(|&m| group.blocks_at(m) && !new_group.blocks_at(m)) {
            out.push(format!(
                "shrinks when {name} blocks ({} {} no longer blocked)",
                Weekday::NAMES[minute / MINUTES_PER_DAY],
                format_time_of_day((minute % MINUTES_PER_DAY) as u64 * 60_000)
            ));
        }
    }

    let best_rate = baseline.habits.values().filter_map(reward_rate).fold(0.0, f64::max);
    for (id, habit) in &new.habits {
        let name = new.habit_name(id);
        let Some(old) = baseline.habits.get(id) else {
            // New timed habits are fine unless they pay better than any
            // existing one; logged habits are self-reported, so always easier.
            match reward_rate(habit) {
                Some(rate) if rate > best_rate * (1.0 + 1e-9) => {
                    out.push(format!("adds habit {name} with a better reward per minute than existing habits"))
                }
                Some(_) => {}
                None => out.push(format!("adds habit {name} that is logged by hand")),
            }
            continue;
        };
        if habit.target < old.target {
            out.push(format!(
                "lowers {name} target {} → {}",
                format_duration(old.target),
                format_duration(habit.target)
            ));
        }
        if habit.kind != old.kind {
            out.push(format!("switches {name} from {} to {} tracking", old.kind.name(), habit.kind.name()));
        }
        if habit.goal() < old.goal() {
            out.push(format!("lowers {name} goal {} → {}", old.goal(), habit.goal()));
        }
        let (old_limit, new_limit) = (old.rounds_per_day(), habit.rounds_per_day());
        if !habit.kind.is_timed() && new_limit.unwrap_or(u32::MAX) > old_limit.unwrap_or(u32::MAX) {
            let limit = new_limit.map_or("unlimited".to_string(), |l| l.to_string());
            out.push(format!("raises {name} daily limit {} → {limit}", old_limit.unwrap_or(0)));
        }
        if habit.unit != old.unit {
            out.push(format!("changes the unit of {name}"));
        }
        if old.strictness == Strictness::Strict && habit.strictness == Strictness::Soft {
            out.push(format!("makes {name} soft"));
        }
        if habit.reward.duration > old.reward.duration {
            out.push(format!(
                "raises {name} reward {} → {}",
                format_duration(old.reward.duration),
                format_duration(habit.reward.duration)
            ));
        }
        for group in &habit.reward.groups {
            if !old.reward.groups.contains(group) {
                out.push(format!("makes {name} also reward {}", new.group_name(group)));
            }
        }
        if matches!(habit.kind, HabitKind::Apps | HabitKind::Passive) {
            if habit.allow.is_empty() && !old.allow.is_empty() {
                out.push(format!("lets any activity count for {name}"));
            }
            for rule in &habit.allow {
                if !old.allow.is_empty() && !old.allow.iter().any(|r| rule.within(r)) {
                    out.push(format!("lets {} count for {name}", rule.describe()));
                }
            }
        }
    }
    out
}

/// The config to run while locked.
pub struct Resolved {
    pub config: Config,
    /// The new baseline: the file if it was accepted, else the old baseline.
    pub baseline: String,
    /// Easier changes held back until the lock ends.
    pub pending: Vec<String>,
}

/// Runs the config file if it only tightens the baseline, otherwise keeps the
/// baseline and reports what the file would weaken. An unparsable file is an
/// error; the caller keeps what it runs.
pub fn resolve_locked(baseline_text: &str, file_text: &str) -> Result<Resolved, String> {
    let file = Config::from_toml(file_text)?;
    let baseline = Config::from_toml(baseline_text).map_err(|e| format!("locked config is unreadable: {e}"))?;
    let pending = weakenings(&baseline, &file);
    Ok(if pending.is_empty() {
        Resolved { config: file, baseline: file_text.to_string(), pending }
    } else {
        Resolved { config: baseline, baseline: baseline_text.to_string(), pending }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
        [general]
        idle_timeout = "90s"
        emergency_penalty = "1h"
        day_start = "04:00"
        browsers = ["zen"]

        [groups.social]
        apps = ["discord"]
        domains = ["reddit.com", "youtube.com"]

        [habits.reading]
        target = "20m"
        strictness = "strict"
        allow = [{ app = "zathura" }]
        reward = { groups = ["social"], duration = "1h" }
    "#;

    fn weak(new: &str) -> Vec<String> {
        weakenings(&Config::from_toml(BASE).unwrap(), &Config::from_toml(new).unwrap())
    }

    #[test]
    fn identical_and_stricter_configs_pass() {
        assert!(weak(BASE).is_empty());
        let stricter = BASE
            .replace(r#"domains = ["reddit.com", "youtube.com"]"#, r#"domains = ["reddit.com", "youtube.com", "x.com"]"#)
            .replace(r#"target = "20m""#, r#"target = "30m""#)
            .replace(r#"idle_timeout = "90s""#, r#"idle_timeout = "60s""#)
            .replace(r#"duration = "1h""#, r#"duration = "45m""#)
            // Narrowing a rule to one program lets less count.
            .replace(
                r#"allow = [{ app = "zathura" }]"#,
                r#"allow = [{ app = "zathura", program = "nvim" }, { app = "zathura", tmux_session = "thesis" }]"#,
            );
        assert!(weak(&stricter).is_empty(), "{:?}", weak(&stricter));
    }

    #[test]
    fn detects_each_kind_of_weakening() {
        let cases = [
            (r#"domains = ["reddit.com", "youtube.com"]"#, r#"domains = ["reddit.com"]"#, "removes domain youtube.com"),
            (r#"target = "20m""#, r#"target = "10m""#, "lowers reading target"),
            (r#"strictness = "strict""#, r#"strictness = "soft""#, "makes reading soft"),
            (r#"duration = "1h""#, r#"duration = "2h""#, "raises reading reward"),
            (r#"idle_timeout = "90s""#, r#"idle_timeout = "10m""#, "raises idle timeout"),
            (r#"emergency_penalty = "1h""#, r#"emergency_penalty = "1m""#, "lowers emergency penalty"),
            (r#"day_start = "04:00""#, r#"day_start = "05:00""#, "moves the day start"),
            (r#"browsers = ["zen"]"#, r#"browsers = []"#, "stops guarding the zen browser"),
            (r#"allow = [{ app = "zathura" }]"#, r#"allow = [{ app = "zathura" }, { app = "kitty" }]"#, "lets app kitty count"),
            (r#"allow = [{ app = "zathura" }]"#, r#"allow = []"#, "lets any activity count"),
            (r#"allow = [{ app = "zathura" }]"#, r#"allow = [{ app = "zathura" }, { program = "nvim" }]"#, "lets program nvim count"),
            (
                r#"allow = [{ app = "zathura" }]"#,
                r#"allow = [{ app = "zathura" }, { tmux_session = "x" }]"#,
                "lets tmux session x count",
            ),
            (r#"apps = ["discord"]"#, r#"apps = []"#, "removes app discord"),
        ];
        for (from, to, expected) in cases {
            let changed = BASE.replace(from, to);
            assert_ne!(changed, BASE, "replacement {from:?} didn't apply");
            let found = weak(&changed);
            assert!(found.iter().any(|w| w.contains(expected)), "{expected:?} not in {found:?}");
        }
    }

    #[test]
    fn new_terminals_weaken_only_program_rules() {
        let more_terminals = |base: &str| base.replace("browsers = [\"zen\"]", "browsers = [\"zen\"]\nterminals = [\"kitty\", \"zathura\"]");
        assert!(weak(&more_terminals(BASE)).is_empty());
        let base = BASE.replace(r#"allow = [{ app = "zathura" }]"#, r#"allow = [{ program = "nvim" }]"#);
        let found = weakenings(&Config::from_toml(&base).unwrap(), &Config::from_toml(&more_terminals(&base)).unwrap());
        assert_eq!(found, ["lets program and tmux session rules match in zathura"]);
    }

    #[test]
    fn logged_habits_and_requirements_only_tighten_one_way() {
        let base = format!(
            "{BASE}\n[habits.anki]\nkind = \"counter\"\ngoal = 20\nunit = \"cards\"\ndaily_limit = 2\nreward = {{ groups = [\"social\"], duration = \"30m\" }}\n"
        )
        .replace("domains = [\"reddit.com\", \"youtube.com\"]", "domains = [\"reddit.com\", \"youtube.com\"]\nrequires = [\"reading\", \"anki\"]\nrequire = \"all\"");
        let check = |from: &str, to: &str, expected: &str| {
            let changed = base.replace(from, to);
            assert_ne!(changed, base, "replacement {from:?} didn't apply");
            let found = weakenings(&Config::from_toml(&base).unwrap(), &Config::from_toml(&changed).unwrap());
            assert!(found.iter().any(|w| w.contains(expected)), "{expected:?} not in {found:?}");
        };
        check("goal = 20", "goal = 10", "lowers anki goal 20 → 10");
        check("daily_limit = 2", "daily_limit = 3", "raises anki daily limit 2 → 3");
        check("daily_limit = 2\n", "", "raises anki daily limit 2 → unlimited");
        check("unit = \"cards\"", "unit = \"notes\"", "changes the unit of anki");
        check("requires = [\"reading\", \"anki\"]", "requires = [\"reading\"]", "stops requiring anki for social");
        check("require = \"all\"", "require = \"any\"", "lets one required habit unlock social");
        check(
            "[habits.anki]",
            "[habits.journal]\nkind = \"manual\"\nreward = { groups = [\"social\"], duration = \"1m\" }\n\n[habits.anki]",
            "adds habit journal that is logged by hand",
        );

        let stricter = base.replace("goal = 20", "goal = 30").replace("daily_limit = 2", "daily_limit = 1");
        let found = weakenings(&Config::from_toml(&base).unwrap(), &Config::from_toml(&stricter).unwrap());
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn schedules_are_compared_by_blocked_minutes() {
        let with = |schedule: &str| BASE.replace("domains = [\"reddit.com\", \"youtube.com\"]", &format!("domains = [\"reddit.com\", \"youtube.com\"]\n{schedule}"));
        let evenings = with("schedule = { days = [\"mon\"], ranges = [\"18:00-23:00\"] }");
        let found = weak(&evenings);
        assert!(found.iter().any(|w| w.contains("shrinks when social blocks (mon 00:00 no longer blocked)")), "{found:?}");

        let base_evenings = weakenings(
            &Config::from_toml(&evenings).unwrap(),
            &Config::from_toml(&with("schedule = { days = [\"mon\", \"tue\"], ranges = [\"17:00-23:30\"] }")).unwrap(),
        );
        assert!(base_evenings.is_empty(), "widening is stricter: {base_evenings:?}");

        let narrower = weakenings(
            &Config::from_toml(&evenings).unwrap(),
            &Config::from_toml(&with("schedule = { days = [\"mon\"], ranges = [\"19:00-23:00\"] }")).unwrap(),
        );
        assert!(narrower.iter().any(|w| w.contains("mon 18:00 no longer blocked")), "{narrower:?}");

        let removed = weakenings(&Config::from_toml(&evenings).unwrap(), &Config::from_toml(BASE).unwrap());
        assert!(removed.is_empty(), "removing a schedule blocks around the clock: {removed:?}");
    }

    #[test]
    fn unlock_modes_are_ranked() {
        let social = "domains = [\"reddit.com\", \"youtube.com\"]";
        let mode = |extra: &str| BASE.replace(social, &format!("{social}\n{extra}"));
        let usage = mode("unlock_mode = \"usage\"");
        assert!(weak(&usage).iter().any(|w| w.contains("unlocks social more generously (wallclock → usage)")));
        let day = |price: &str| mode(&format!("unlock_mode = \"rest_of_day\"\nrest_of_day_price = \"{price}\""));
        let between = |a: &str, b: &str| weakenings(&Config::from_toml(a).unwrap(), &Config::from_toml(b).unwrap());
        assert!(between(&usage, BASE).is_empty(), "back to wall clock is stricter");
        assert!(between(&day("1h"), &usage).is_empty());
        assert!(between(&day("1h"), &day("20m")).iter().any(|w| w.contains("lowers the social day pass 1:00:00 → 20:00")));
        assert!(between(&day("20m"), &day("1h")).is_empty());
    }

    #[test]
    fn retention_settings_are_not_weakenings() {
        let changed = BASE.replace("browsers = [\"zen\"]", "browsers = [\"zen\"]\nscreen_time_days = 7");
        assert_ne!(changed, BASE);
        assert!(weak(&changed).is_empty());
    }

    #[test]
    fn new_habits_are_allowed_unless_they_pay_better() {
        let fair = format!("{BASE}\n[habits.walk]\ntarget = \"40m\"\nreward = {{ groups = [\"social\"], duration = \"2h\" }}\n");
        assert!(weak(&fair).is_empty(), "{:?}", weak(&fair));
        let cheap = format!("{BASE}\n[habits.cheat]\ntarget = \"1m\"\nreward = {{ groups = [\"social\"], duration = \"1h\" }}\n");
        assert!(weak(&cheap)[0].contains("adds habit cheat"));
        // Passive habits: tracking-only ones are harmless, paying ones compare
        // their rate like timed habits.
        let tracked = format!("{BASE}\n[habits.code]\nkind = \"passive\"\nallow = [{{ app = \"zed\" }}]\n");
        assert!(weak(&tracked).is_empty(), "{:?}", weak(&tracked));
        let paying = format!(
            "{BASE}\n[habits.code]\nkind = \"passive\"\ntarget = \"10m\"\nallow = [{{ app = \"zed\" }}]\nreward = {{ groups = [\"social\"], duration = \"1h\" }}\n"
        );
        assert!(weak(&paying)[0].contains("adds habit code"), "{:?}", weak(&paying));
        let widened = tracked.replace(r#"allow = [{ app = "zed" }]"#, r#"allow = [{ app = "zed" }, { app = "kitty" }]"#);
        let found = weakenings(&Config::from_toml(&tracked).unwrap(), &Config::from_toml(&widened).unwrap());
        assert_eq!(found, ["lets app kitty count for code"]);
        let removed_group = BASE.replace("[groups.social]", "[groups.other]").replace(r#"groups = ["social"]"#, r#"groups = ["other"]"#);
        assert!(weak(&removed_group).iter().any(|w| w.contains("removes group social")));
    }

    #[test]
    fn resolve_keeps_baseline_when_weakened() {
        let stricter = BASE.replace(r#"target = "20m""#, r#"target = "25m""#);
        let resolved = resolve_locked(BASE, &stricter).unwrap();
        assert!(resolved.pending.is_empty());
        assert_eq!(resolved.config.habits["reading"].target, 25 * 60_000);
        assert_eq!(resolved.baseline, stricter);

        let weaker = BASE.replace(r#"target = "20m""#, r#"target = "5m""#);
        let resolved = resolve_locked(BASE, &weaker).unwrap();
        assert_eq!(resolved.pending.len(), 1);
        assert_eq!(resolved.config.habits["reading"].target, 20 * 60_000);
        assert_eq!(resolved.baseline, BASE);

        assert!(resolve_locked(BASE, "not toml [").is_err());
    }

    #[test]
    fn downtime_ignores_reboots_suspend_and_clean_shutdowns() {
        let prev = Heartbeat { wall_ms: 0, boot_id: "a".into(), monotonic_ms: 1_000, clean_shutdown: false };
        assert_eq!(unexplained_downtime(&prev, "a", 1_000 + 30_000, TAMPER_GRACE_MS), None);
        assert_eq!(unexplained_downtime(&prev, "a", 1_000 + 600_000, TAMPER_GRACE_MS), Some(600_000));
        assert_eq!(unexplained_downtime(&prev, "b", 1_000 + 600_000, TAMPER_GRACE_MS), None);
        let clean = Heartbeat { clean_shutdown: true, ..prev };
        assert_eq!(unexplained_downtime(&clean, "a", 1_000 + 600_000, TAMPER_GRACE_MS), None);
    }

    #[test]
    fn end_request_shortens_to_the_cooldown() {
        let mut lock = Lock { until: 10 * END_DELAY_MS, baseline: String::new(), end_requested_at: None, pending: vec![], extended_ms: 0 };
        assert_eq!(lock.ends_at(), 10 * END_DELAY_MS);
        lock.end_requested_at = Some(END_DELAY_MS);
        assert_eq!(lock.ends_at(), 2 * END_DELAY_MS);
        lock.end_requested_at = Some(10 * END_DELAY_MS);
        assert_eq!(lock.ends_at(), 10 * END_DELAY_MS);
    }
}
