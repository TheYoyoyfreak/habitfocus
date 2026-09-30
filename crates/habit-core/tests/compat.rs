//! Clients must accept snapshots from older daemons, and a daemon must accept
//! state files written by older versions.

use habit_core::config::HabitKind;
use habit_core::{Config, Engine, Snapshot, State};
use serde_json::Value;

#[test]
fn parses_snapshot_without_newer_fields() {
    let old = r#"{"now_ms":1,"session":null,"habits":[],"groups":[],"penalty_remaining_ms":0,"idle":false}"#;
    let snapshot: habit_core::Snapshot = serde_json::from_str(old).unwrap();
    assert!(snapshot.blocked_domains.is_empty());
}

const CONFIG: &str = r#"
    [groups.social]
    apps = ["discord"]
    requires = ["walk"]

    [habits.walk]
    kind = "manual"
    reward = { groups = ["social"], duration = "10m" }

    [habits.read]
    target = "20m"
    reward = { groups = ["social"], duration = "30m" }
"#;

/// Removes fields added since schedules, unlock modes and logged habits,
/// as a daemon from before them would send.
fn strip(value: &mut Value, keys: &[&str]) {
    let object = value.as_object_mut().unwrap();
    for key in keys {
        object.remove(*key);
    }
}

#[test]
fn parses_groups_and_habits_from_before_blocks_and_logged_habits() {
    let engine = Engine::new(Config::from_toml(CONFIG).unwrap(), State::default());
    let mut value = serde_json::to_value(engine.snapshot(0)).unwrap();
    strip(&mut value, &["events_seq", "credit_expires_at_ms", "credit_expires_label"]);
    for group in value["groups"].as_array_mut().unwrap() {
        strip(group, &[
            "processes", "scheduled", "off_schedule", "schedule_label", "schedule_next_change_at_ms",
            "schedule_summary", "unlock_mode", "unlock_label", "unlock_until_ms", "rest_of_day_price_ms",
            "requires", "require_all", "requirements_met",
        ]);
    }
    for habit in value["habits"].as_array_mut().unwrap() {
        strip(habit, &["goal", "unit", "count_today", "rounds_today", "daily_limit"]);
    }
    let snapshot: Snapshot = serde_json::from_value(value).unwrap();
    let group = &snapshot.groups[0];
    // Without the field, a group counts as not waiting for anything.
    assert!(group.requirements_met && group.requires.is_empty() && !group.scheduled);
    assert_eq!(snapshot.habits[0].goal, 0);
}

#[test]
fn unknown_habit_kinds_from_newer_daemons_parse() {
    let engine = Engine::new(Config::from_toml(CONFIG).unwrap(), State::default());
    let mut value = serde_json::to_value(engine.snapshot(0)).unwrap();
    value["habits"][0]["kind"] = "anki_sync".into();
    let snapshot: Snapshot = serde_json::from_value(value).unwrap();
    assert_eq!(snapshot.habits[0].kind, HabitKind::Other);
}

#[test]
fn loads_state_written_before_unlock_modes() {
    // Shape of state.json on main before this branch: wall-clock unlocks as
    // group -> until, and day stats without counts.
    let old = r#"{
        "session": null,
        "credits": {"social": 600000},
        "unlocks": {"social": 3600000},
        "progress": {},
        "lock": null,
        "heartbeat": null,
        "penalty_until": null,
        "history": [],
        "days": {"0": {"read": {"focused_ms": 1200000, "completions": 1}}}
    }"#;
    let state: State = serde_json::from_str(old).unwrap();
    assert_eq!(state.days[&0]["read"].count, 0);
    let engine = Engine::new(Config::from_toml(CONFIG).unwrap(), state);
    let group = engine.snapshot(60_000).groups.into_iter().next().unwrap();
    assert!(!group.blocked, "the old unlock keeps running");
    assert_eq!(group.unlock_mode, "wallclock");
    assert_eq!(group.credit_ms, 600_000);
    // Migrated unlocks are written in the new shape only.
    let saved = serde_json::to_value(engine.state()).unwrap();
    assert!(saved["open_unlocks"]["social"].is_object());
    assert!(saved.get("unlocks").is_none(), "{saved}");
}
