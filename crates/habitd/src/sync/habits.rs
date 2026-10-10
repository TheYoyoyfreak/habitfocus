//! Publishes this device's habits for clients that have no config of their own
//! (the habitfocus_mobile app): names, kinds, targets, which habits are
//! reading, and when the day starts. Only habits: blocks, rules and rewards
//! stay on the device.
//!
//! A record with tag `habits` goes out whenever the summary changed (its hash
//! is kept as `habits:hash` in `sync_state`); readers take the newest record of
//! all devices. The rows are `[[0, summary]]`, sealed like every record.

use super::api::Api;
use super::crypto::Key;
use super::run::{seal, Batch, SyncError};
use crate::archive::{Archive, Device};
use habit_core::Config;
use sha2::{Digest, Sha256};

pub const TAG: &str = "habits";

/// What clients learn about the habits of `config`.
pub fn summary(config: &Config) -> serde_json::Value {
    let habits: Vec<serde_json::Value> = config
        .habits
        .iter()
        .map(|(id, habit)| {
            serde_json::json!({
                "id": id,
                "name": config.habit_name(id),
                "kind": habit.kind.name(),
                "target_ms": habit.target,
                "goal": habit.goal(),
                "unit": habit.unit,
                "daily_limit": habit.daily_limit,
                "strictness": habit.strictness,
            })
        })
        .collect();
    serde_json::json!({
        "habits": habits,
        "books": {
            "enabled": config.books.enabled,
            "habits": config.books.habits,
            "rating": config.books.rating,
            "google_books_key": config.books.google_books_key,
        },
        "day_start_ms": config.general.day_start,
    })
}

/// Sends the summary of `config` if it changed since the last one sent;
/// returns whether it did.
pub fn publish(api: &dyn Api, archive: &mut Archive, key: &Key, config: &Config) -> Result<bool, SyncError> {
    let summary = summary(config);
    let hash: String = Sha256::digest(summary.to_string().as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    if archive.sync_value("habits:hash")?.as_deref() == Some(hash.as_str()) {
        return Ok(false);
    }
    let device: Device = archive.device().clone();
    let next = api.status()?.tail(&device.id, TAG).map_or(0, |tail| tail + 1);
    let batch = Batch { rows: vec![(0, summary)] };
    api.upload(&[seal(key, &device, TAG, next, &batch)])?;
    archive.set_sync_value("habits:hash", &hash)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::run::open;
    use crate::sync::run::tests::FakeServer;
    use habit_core::state::State;

    const CONFIG: &str = r#"
        [general]
        day_start = "04:00"
        [groups.social]
        [habits.read]
        name = "Reading"
        kind = "timer"
        target = "20m"
        reward = { groups = ["social"], duration = "1h" }
        [habits.pushups]
        kind = "counter"
        goal = 50
        unit = "reps"
        reward = { groups = ["social"], duration = "10m" }
        [books]
        enabled = true
        habits = ["read"]
    "#;

    #[test]
    fn publishes_only_when_the_habits_change() {
        let server = FakeServer::default();
        let key = Key::generate();
        let mut archive = Archive::in_memory(&State::default());
        let config = Config::from_toml(CONFIG).unwrap();
        assert!(publish(&server, &mut archive, &key, &config).unwrap());
        assert!(!publish(&server, &mut archive, &key, &config).unwrap(), "unchanged");

        let changed = Config::from_toml(&CONFIG.replace("20m", "30m")).unwrap();
        assert!(publish(&server, &mut archive, &key, &changed).unwrap());
        let logs = server.logs.borrow();
        let log = &logs[&(archive.device().id.clone(), TAG.to_string())];
        assert_eq!(log.keys().copied().collect::<Vec<_>>(), [0, 1]);

        let newest = open(&key, &log[&1]).unwrap();
        let summary = &newest.rows[0].1;
        assert_eq!(summary["habits"][1]["id"], "read");
        assert_eq!(summary["habits"][1]["name"], "Reading");
        assert_eq!(summary["habits"][1]["kind"], "timer");
        assert_eq!(summary["habits"][1]["target_ms"], 30 * 60_000);
        assert_eq!(summary["habits"][0]["unit"], "reps");
        assert_eq!(summary["habits"][0]["goal"], 50);
        assert_eq!(summary["books"]["habits"][0], "read");
        assert_eq!(summary["day_start_ms"], 4 * 3_600_000);
    }
}
