//! Editing `[general]` settings in the config file, preserving its comments
//! and formatting.

use habit_core::Config;
use std::path::Path;
use toml_edit::{value, DocumentMut};

/// Settings that can be changed at runtime, e.g. from the TUI.
pub const EDITABLE: &[&str] = &[
    "sound",
    "day_start",
    "idle_timeout",
    "emergency_penalty",
    "expiry_warning",
    "notifications",
    "terminal_programs",
    "auto_categories",
    "update_check",
];

/// Settings that are true or false.
const SWITCHES: &[&str] = &["notifications", "terminal_programs", "auto_categories", "update_check"];

/// Returns the updated config text, validated by parsing it.
pub fn apply(text: &str, key: &str, raw: &str) -> Result<(String, Config), String> {
    if !EDITABLE.contains(&key) {
        return Err(format!("{key:?} can't be changed here (editable: {})", EDITABLE.join(", ")));
    }
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("cannot parse config: {e}"))?;
    let raw = raw.trim();
    let general = doc
        .entry("general")
        .or_insert_with(toml_edit::table)
        .as_table_like_mut()
        .ok_or("[general] is not a table")?;
    if SWITCHES.contains(&key) {
        let enabled = match raw {
            "true" | "on" | "yes" => true,
            "false" | "off" | "no" => false,
            _ => return Err(format!("{key} must be true or false")),
        };
        general.insert(key, value(enabled));
    } else {
        general.insert(key, value(raw));
    }
    let updated = doc.to_string();
    let config = Config::from_toml(&updated)?;
    config.check_lockout(Config::from_toml(text).ok().as_ref())?;
    Ok((updated, config))
}

/// Replaces the config file atomically.
pub fn write_file(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = r#"# my config
[general]
# keep this comment
idle_timeout = "90s"

[groups.social]
domains = ["reddit.com"]
"#;

    #[test]
    fn updates_and_adds_keys_preserving_comments() {
        let (updated, config) = apply(TEXT, "day_start", "04:30").unwrap();
        assert!(updated.contains("# keep this comment"));
        assert!(updated.contains("day_start = \"04:30\""));
        assert_eq!(config.general.day_start, 16_200_000);

        let (updated, config) = apply(&updated, "idle_timeout", "5m").unwrap();
        assert!(updated.contains("idle_timeout = \"5m\""));
        assert_eq!(config.general.idle_timeout, 300_000);

        let (updated, config) = apply(&updated, "notifications", "off").unwrap();
        assert!(updated.contains("notifications = false"));
        assert!(!config.general.notifications);

        let (updated, config) = apply(&updated, "terminal_programs", "off").unwrap();
        assert!(updated.contains("terminal_programs = false"));
        assert!(!config.general.terminal_programs);
        let (updated, config) = apply(&updated, "auto_categories", "false").unwrap();
        assert!(updated.contains("auto_categories = false"));
        assert!(!config.general.auto_categories);
    }

    #[test]
    fn rejects_invalid_values_and_keys() {
        assert!(apply(TEXT, "day_start", "25:00").is_err());
        assert!(apply(TEXT, "idle_timeout", "soon").is_err());
        assert!(apply(TEXT, "notifications", "maybe").is_err());
        assert!(apply(TEXT, "terminal_programs", "sometimes").is_err());
        assert!(apply(TEXT, "groups", "x").is_err());
    }

    #[test]
    fn creates_general_table_when_missing() {
        let (updated, _) = apply("[groups.social]\n", "day_start", "03:00").unwrap();
        assert!(updated.contains("[general]"));
    }
}
