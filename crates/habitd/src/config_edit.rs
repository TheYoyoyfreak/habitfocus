//! Editing whole blocks (`[groups.<id>]`) and habits (`[habits.<id>]`) in the
//! config file, for the TUI. Only keys whose value changed are rewritten, so
//! comments and formatting elsewhere survive, as in `settings.rs`.

use habit_core::Config;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table};

pub const SECTIONS: &[&str] = &["groups", "habits"];

/// "block" or "habit", for messages.
pub fn noun(section: &str) -> &'static str {
    if section == "groups" { "block" } else { "habit" }
}

/// Entries by id, each a JSON table.
pub type Entries = BTreeMap<String, Value>;

/// The blocks and habits of the config file as JSON, with values as written
/// (durations stay strings like "20m").
pub fn entries(text: &str) -> Result<(Entries, Entries), String> {
    let table: toml::Table = toml::from_str(text).map_err(|e| format!("cannot parse config: {e}"))?;
    let section = |name: &str| -> Entries {
        table
            .get(name)
            .and_then(|v| v.as_table())
            .map(|t| t.iter().map(|(id, v)| (id.clone(), serde_json::to_value(v).unwrap_or(Value::Null))).collect())
            .unwrap_or_default()
    };
    Ok((section("groups"), section("habits")))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn to_toml(value: &Value) -> Option<toml_edit::Value> {
    Some(match value {
        Value::Null => return None,
        Value::Bool(b) => (*b).into(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.into(),
            None => n.as_f64()?.into(),
        },
        Value::String(s) => s.as_str().into(),
        Value::Array(items) => {
            let mut array = Array::new();
            for item in items.iter().filter_map(to_toml) {
                array.push(item);
            }
            array.into()
        }
        Value::Object(map) => {
            let mut table = InlineTable::new();
            for (key, item) in map {
                if let Some(item) = to_toml(item) {
                    table.insert(key, item);
                }
            }
            table.into()
        }
    })
}

/// Removes `id` from the string array at `key` of a table, if present, and
/// the key itself once empty unless the config requires it (`keep_empty`).
fn remove_from_list(table: &mut dyn toml_edit::TableLike, key: &str, id: &str, keep_empty: bool) {
    if let Some(array) = table.get_mut(key).and_then(|item| item.as_array_mut()) {
        array.retain(|v| v.as_str() != Some(id));
        if array.is_empty() && !keep_empty {
            table.remove(key);
        }
    }
}

/// Applies an edit and returns the new config text, validated by parsing it.
/// `table: None` deletes the entry and drops references to it; `create`
/// refuses an id that already exists.
pub fn apply(
    text: &str,
    section: &str,
    id: &str,
    table: Option<&Map<String, Value>>,
    create: bool,
) -> Result<(String, Config), String> {
    if !SECTIONS.contains(&section) {
        return Err(format!("unknown section {section:?}"));
    }
    let noun = noun(section);
    if !valid_id(id) {
        return Err(format!("{noun} ids use lowercase letters, digits, - and _ (got {id:?})"));
    }
    let (old_groups, old_habits) = entries(text)?;
    let old = if section == "groups" { old_groups.get(id) } else { old_habits.get(id) };
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("cannot parse config: {e}"))?;
    let entries_table = doc
        .entry(section)
        .or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        })
        .as_table_like_mut()
        .ok_or(format!("[{section}] is not a table"))?;

    match table {
        None => {
            if old.is_none() {
                return Err(format!("there is no {noun} {id:?}"));
            }
            entries_table.remove(id);
            // Other entries may still point at it.
            let (other, key): (&str, &[&str]) =
                if section == "groups" { ("habits", &["reward", "groups"]) } else { ("groups", &["requires"]) };
            if let Some(others) = doc.get_mut(other).and_then(|i| i.as_table_like_mut()) {
                for (_, entry) in others.iter_mut() {
                    let Some(entry) = entry.as_table_like_mut() else { continue };
                    match key {
                        [list] => remove_from_list(entry, list, id, false),
                        // A reward needs its list of groups, even empty.
                        [sub, list] => {
                            if let Some(sub) = entry.get_mut(sub).and_then(|i| i.as_table_like_mut()) {
                                remove_from_list(sub, list, id, true);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        Some(new) => {
            if create && old.is_some() {
                return Err(format!("a {noun} called {id:?} already exists"));
            }
            if !create && old.is_none() {
                return Err(format!("there is no {noun} {id:?}"));
            }
            let entry = entries_table
                .entry(id)
                .or_insert_with(|| Item::Table(Table::new()))
                .as_table_like_mut()
                .ok_or(format!("[{section}.{id}] is not a table"))?;
            let old = old.and_then(Value::as_object);
            // New keys are appended; name and kind read best first.
            let mut keys: Vec<&String> = new.keys().collect();
            keys.sort_by_key(|k| match k.as_str() {
                "name" => 0,
                "kind" => 1,
                _ => 2,
            });
            for (key, value) in keys.into_iter().map(|k| (k, &new[k])) {
                if old.and_then(|o| o.get(key)) == Some(value) {
                    continue; // unchanged: keep its formatting and comments
                }
                match (to_toml(value), entry.get_mut(key)) {
                    // Replace the value in place: the key keeps the comments
                    // above it, the value its spacing and trailing comment.
                    (Some(mut v), Some(Item::Value(current))) => {
                        *v.decor_mut() = current.decor().clone();
                        *current = v;
                    }
                    (Some(v), _) => {
                        entry.insert(key, Item::Value(v));
                    }
                    (None, _) => {
                        entry.remove(key);
                    }
                }
            }
            if let Some(old) = old {
                for key in old.keys().filter(|k| !new.contains_key(*k)) {
                    entry.remove(key);
                }
            }
        }
    }
    let updated = doc.to_string();
    let config = Config::from_toml(&updated)?;
    config.check_lockout(Config::from_toml(text).ok().as_ref())?;
    Ok((updated, config))
}

/// Tables of per-app labels: display names and categories.
pub const LABEL_TABLES: &[&str] = &["app_names", "app_categories"];

/// Sets or removes the label of an app key in `table` (`app_names` or
/// `app_categories`); an empty label removes it.
pub fn set_app_label(text: &str, table: &str, app: &str, name: Option<&str>) -> Result<(String, Config), String> {
    if !LABEL_TABLES.contains(&table) {
        return Err(format!("unknown table {table:?}"));
    }
    let app = app.trim();
    if app.is_empty() {
        return Err("which app?".into());
    }
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("cannot parse config: {e}"))?;
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => {
            let names = doc
                .entry(table)
                .or_insert_with(|| Item::Table(Table::new()))
                .as_table_like_mut()
                .ok_or(format!("[{table}] is not a table"))?;
            match names.get_mut(app) {
                Some(Item::Value(current)) => {
                    let decor = current.decor().clone();
                    *current = name.into();
                    *current.decor_mut() = decor;
                }
                _ => {
                    names.insert(app, Item::Value(name.into()));
                }
            }
        }
        None => {
            if let Some(names) = doc.get_mut(table).and_then(|i| i.as_table_like_mut()) {
                names.remove(app);
                if names.is_empty() {
                    doc.remove(table);
                }
            }
        }
    }
    let updated = doc.to_string();
    let config = Config::from_toml(&updated)?;
    config.check_lockout(Config::from_toml(text).ok().as_ref())?;
    Ok((updated, config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TEXT: &str = r#"# my config
[general]
day_start = "04:00"

# the social block
[groups.social]
name = "Social"
apps = ["discord"] # chat
# sites to block
domains = ["reddit.com"]
schedule = { days = ["mon"], ranges = ["08:00-12:00"] }
requires = ["walk"]

[groups.games]
apps = ["steam"]

[habits.walk]
kind = "manual"
reward = { groups = ["social"], duration = "15m" }

[habits.read]
target = "20m"
allow = [{ app = "zathura" }]
reward = { groups = ["social", "games"], duration = "1h" }
"#;

    fn obj(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn entry(section: &str, id: &str) -> Map<String, Value> {
        let (groups, habits) = entries(TEXT).unwrap();
        let map = if section == "groups" { groups } else { habits };
        obj(map[id].clone())
    }

    #[test]
    fn reads_entries_as_written() {
        let social = entry("groups", "social");
        assert_eq!(social["name"], "Social");
        assert_eq!(social["schedule"]["ranges"][0], "08:00-12:00");
        assert_eq!(entry("habits", "read")["target"], "20m");
    }

    #[test]
    fn edits_only_changed_keys() {
        let mut social = entry("groups", "social");
        social.insert("domains".into(), json!(["reddit.com", "x.com"]));
        social.insert("unlock_mode".into(), json!("usage"));
        social.remove("requires");
        let (updated, config) = apply(TEXT, "groups", "social", Some(&social), false).unwrap();
        assert!(updated.contains("# the social block"));
        assert!(updated.contains(r#"apps = ["discord"] # chat"#), "{updated}");
        assert!(updated.contains(r#"schedule = { days = ["mon"], ranges = ["08:00-12:00"] }"#));
        assert!(updated.contains("# sites to block\ndomains = [\"reddit.com\", \"x.com\"]"), "{updated}");
        assert!(!updated.contains("requires"));
        assert_eq!(config.groups["social"].domains.len(), 2);
        assert!(config.groups["social"].requires.is_empty());
    }

    #[test]
    fn creates_entries() {
        let new = obj(json!({ "name": "Night", "domains": ["youtube.com"], "unlock_mode": "rest_of_day", "rest_of_day_price": "1h" }));
        let (updated, config) = apply(TEXT, "groups", "night", Some(&new), true).unwrap();
        assert!(updated.contains("[groups.night]\nname = \"Night\"\n"), "{updated}");
        assert_eq!(config.groups["night"].rest_of_day_price, Some(3_600_000));
        let habit = obj(json!({ "kind": "counter", "goal": 20, "unit": "cards", "reward": { "groups": ["night"], "duration": "30m" } }));
        let (_, config) = apply(&updated, "habits", "anki", Some(&habit), true).unwrap();
        assert_eq!(config.habits["anki"].goal(), 20);
        // A config without [habits] yet gets one without an empty header.
        let (updated, _) = apply("", "groups", "x", Some(&obj(json!({ "apps": ["steam"] }))), true).unwrap();
        assert_eq!(updated.trim(), "[groups.x]\napps = [\"steam\"]");
    }

    #[test]
    fn refuses_blocking_the_terminal() {
        let mut social = entry("groups", "social");
        social.insert("apps".into(), json!(["discord", "kitty"]));
        let err = apply(TEXT, "groups", "social", Some(&social), false).unwrap_err();
        assert!(err.contains("can't block the terminal \"kitty\""), "{err}");
        let new = obj(json!({ "processes": ["hf"] }));
        assert!(apply(TEXT, "groups", "x", Some(&new), true).unwrap_err().contains("\"hf\""));

        // A config that already has one stays editable, so it can be fixed.
        let old = TEXT.replace(r#"apps = ["discord"]"#, r#"apps = ["discord", "kitty"]"#);
        assert!(set_app_label(&old, "app_names", "steam", Some("Steam")).is_ok());
        let (_, config) = apply(&old, "groups", "social", Some(&entry("groups", "social")), false).unwrap();
        assert!(config.lockout_entries().is_empty());
    }

    #[test]
    fn deleting_drops_references() {
        let (updated, config) = apply(TEXT, "habits", "walk", None, false).unwrap();
        assert!(!config.habits.contains_key("walk"));
        assert!(config.groups["social"].requires.is_empty());
        assert!(!updated.contains("requires"), "an emptied requires goes away");
        assert!(updated.contains("# the social block"));

        let (_, config) = apply(TEXT, "groups", "social", None, false).unwrap();
        assert_eq!(config.habits["read"].reward.groups, ["games"]);
        assert!(config.habits["walk"].reward.groups.is_empty());
    }

    #[test]
    fn names_apps() {
        let (updated, config) = set_app_label(TEXT, "app_names", "steam_app_275850", Some("No Man's Sky")).unwrap();
        assert!(updated.contains("[app_names]\nsteam_app_275850 = \"No Man's Sky\""), "{updated}");
        assert_eq!(config.app_name("steam_app_275850"), "No Man's Sky");
        let (updated, config) = set_app_label(&updated, "app_names", "site:www.youtube.com", Some("YouTube")).unwrap();
        assert!(updated.contains(r#""site:www.youtube.com" = "YouTube""#), "{updated}");
        assert_eq!(config.app_name("site:www.youtube.com"), "YouTube");
        let (updated, _) = set_app_label(&updated, "app_names", "steam_app_275850", Some("NMS")).unwrap();
        assert!(updated.contains(r#"steam_app_275850 = "NMS""#));
        let (updated, _) = set_app_label(&updated, "app_names", "steam_app_275850", None).unwrap();
        let (updated, config) = set_app_label(&updated, "app_names", "site:www.youtube.com", Some("  ")).unwrap();
        assert!(!updated.contains("app_names"), "an empty table goes away: {updated}");
        assert!(config.app_names.is_empty());
        assert!(updated.contains("# the social block"));

        let (updated, config) = set_app_label(TEXT, "app_categories", "steam_app_275850", Some("Games")).unwrap();
        assert!(updated.contains("[app_categories]\nsteam_app_275850 = \"Games\""), "{updated}");
        assert_eq!(config.app_categories["steam_app_275850"], "Games");
        assert!(set_app_label(TEXT, "groups", "x", Some("y")).is_err());
    }

    #[test]
    fn refuses_bad_edits() {
        let bad_target = obj(json!({ "target": "soon", "reward": { "groups": [], "duration": "1m" } }));
        assert!(apply(TEXT, "habits", "read", Some(&bad_target), false).is_err());
        assert!(apply(TEXT, "groups", "Social Media", Some(&Map::new()), true).unwrap_err().contains("lowercase"));
        assert!(apply(TEXT, "groups", "social", Some(&Map::new()), true).unwrap_err().contains("already exists"));
        assert!(apply(TEXT, "groups", "nope", Some(&Map::new()), false).unwrap_err().contains("no block"));
        assert!(apply(TEXT, "groups", "nope", None, false).is_err());
        assert!(apply(TEXT, "general", "x", None, false).is_err());
        // A block whose habit reference is unknown fails validation.
        let requires_ghost = obj(json!({ "requires": ["ghost"] }));
        assert!(apply(TEXT, "groups", "games", Some(&requires_ghost), false).is_err());
    }
}
