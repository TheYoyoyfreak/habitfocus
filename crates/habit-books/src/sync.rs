//! The library's side of multi-device sync: what changed here since the last
//! push, and merging rows from other devices, last write wins.
//!
//! Every synced table is a log per device on the sync server (tag =
//! [`SyncTable::tag`]). Rows travel as JSON objects of their columns, `rev`
//! aside: a local edit gets a new `rev`, a row from elsewhere gets `rev = 0`,
//! so it never goes out again from here. Of two versions of a row the one with
//! the later `updated_at` wins, the larger `device_id` on a tie, so every
//! device ends up with the same rows whatever order they arrive in.

use rusqlite::types::Value;
use rusqlite::{params_from_iter, OptionalExtension};

use crate::db::Db;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncTable {
    Books,
    Readings,
    Sessions,
}

impl SyncTable {
    pub const ALL: [SyncTable; 3] = [SyncTable::Books, SyncTable::Readings, SyncTable::Sessions];

    /// Its tag on the sync server (also the table name).
    pub fn tag(self) -> &'static str {
        match self {
            SyncTable::Books => "books",
            SyncTable::Readings => "readings",
            SyncTable::Sessions => "reading_sessions",
        }
    }

    pub fn of_tag(tag: &str) -> Option<SyncTable> {
        SyncTable::ALL.into_iter().find(|t| t.tag() == tag)
    }

    fn columns(self) -> &'static [&'static str] {
        match self {
            SyncTable::Books => &[
                "id", "title", "subtitle", "author", "isbn", "olid", "pages", "pub_year", "format", "status", "rating",
                "tags", "notes", "cover_url", "created_at", "updated_at", "deleted_at", "device_id",
            ],
            SyncTable::Readings => &[
                "id", "book_id", "started_at", "finished_at", "outcome", "created_at", "updated_at", "deleted_at",
                "device_id",
            ],
            SyncTable::Sessions => &[
                "id", "book_id", "started_at", "ended_at", "duration_ms", "page_from", "page_to", "source", "device",
                "external_ref", "created_at", "updated_at", "deleted_at", "device_id",
            ],
        }
    }

    /// Columns a row can't do without.
    fn required(self) -> &'static [&'static str] {
        match self {
            SyncTable::Books => &["id", "title", "format", "status", "created_at", "updated_at", "device_id"],
            SyncTable::Readings => &["id", "book_id", "started_at", "created_at", "updated_at", "device_id"],
            SyncTable::Sessions => &[
                "id", "book_id", "started_at", "ended_at", "duration_ms", "source", "device", "created_at", "updated_at",
                "device_id",
            ],
        }
    }
}

fn to_json(value: Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Integer(i) => i.into(),
        Value::Real(f) => serde_json::Number::from_f64(f).map_or(serde_json::Value::Null, serde_json::Value::Number),
        Value::Text(s) => s.into(),
        Value::Blob(_) => serde_json::Value::Null,
    }
}

fn from_json(value: &serde_json::Value) -> Option<Value> {
    Some(match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Number(n) => Value::Integer(n.as_i64()?),
        serde_json::Value::String(s) => Value::Text(s.clone()),
        serde_json::Value::Bool(b) => Value::Integer(i64::from(*b)),
        _ => return None,
    })
}

impl Db {
    /// Rows of `table` changed here after `after_rev`, oldest change first, as
    /// `(rev, row)`.
    pub fn local_changes(
        &self,
        table: SyncTable,
        after_rev: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<(i64, serde_json::Value)>> {
        let columns = table.columns();
        let mut stmt = self.conn.prepare(&format!(
            "SELECT rev, {} FROM {} WHERE rev > ?1 ORDER BY rev LIMIT ?2",
            columns.join(", "),
            table.tag()
        ))?;
        let rows = stmt.query_map(rusqlite::params![after_rev, limit as i64], |r| {
            let mut row = serde_json::Map::new();
            for (i, column) in columns.iter().enumerate() {
                row.insert((*column).to_owned(), to_json(r.get::<_, Value>(i + 1)?));
            }
            Ok((r.get::<_, i64>(0)?, serde_json::Value::Object(row)))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Merges rows of `table` from another device; returns how many changed
    /// something here. Rows that don't parse (from a newer habitfocus) are
    /// skipped.
    pub fn merge_remote(&self, table: SyncTable, rows: &[serde_json::Value]) -> anyhow::Result<usize> {
        let columns = table.columns();
        let assignments: Vec<String> =
            columns.iter().filter(|c| **c != "id").map(|c| format!("{c} = excluded.{c}")).collect();
        let sql = format!(
            "INSERT INTO {table} ({cols}, rev) VALUES ({marks}, 0)
             ON CONFLICT(id) DO UPDATE SET {assignments}, rev = 0
             WHERE excluded.updated_at > {table}.updated_at
                OR (excluded.updated_at = {table}.updated_at AND excluded.device_id > {table}.device_id)",
            table = table.tag(),
            cols = columns.join(", "),
            marks = vec!["?"; columns.len()].join(", "),
            assignments = assignments.join(", "),
        );
        let tx = self.conn.unchecked_transaction()?;
        let mut changed = 0;
        for row in rows {
            let Some(object) = row.as_object() else { continue };
            if table.required().iter().any(|c| object.get(*c).is_none_or(serde_json::Value::is_null)) {
                continue;
            }
            let Some(values) =
                columns.iter().map(|c| from_json(object.get(*c).unwrap_or(&serde_json::Value::Null))).collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            if table == SyncTable::Sessions {
                if let Some(external_ref) = object.get("external_ref").and_then(serde_json::Value::as_str) {
                    // The same habit session under another id (recorded before
                    // ids came from the ref): the one here stays.
                    let id = object["id"].as_str().unwrap_or_default();
                    let other: Option<String> = tx
                        .query_row(
                            "SELECT id FROM reading_sessions WHERE external_ref = ?1 AND id != ?2",
                            [external_ref, id],
                            |r| r.get(0),
                        )
                        .optional()?;
                    if other.is_some() {
                        continue;
                    }
                    // Another device recorded it with a book: it doesn't wait here anymore.
                    tx.execute("DELETE FROM pending_sessions WHERE external_ref = ?1", [external_ref])?;
                }
            }
            changed += tx.execute(&sql, params_from_iter(values))?;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// A sync cursor (kept in settings).
    pub fn sync_value(&self, key: &str) -> anyhow::Result<Option<String>> {
        self.setting(&format!("sync:{key}"))
    }

    pub fn set_sync_value(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.set_setting(&format!("sync:{key}"), Some(value))
    }

    /// Forgets all sync cursors (signing in again starts over).
    pub fn clear_sync_state(&self) -> anyhow::Result<()> {
        self.conn.execute("DELETE FROM settings WHERE key LIKE 'sync:%'", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Book, Status};

    /// Two devices of one account.
    fn devices() -> (Db, Db) {
        let open = |id: &str, name: &str| {
            let db = Db::open_in_memory().unwrap();
            Db { device_id: id.into(), device_name: name.into(), ..db }
        };
        (open("dev-a", "laptop"), open("dev-b", "phone"))
    }

    /// Sends everything `from` changed to `to`, like a sync through the server would.
    fn ship(from: &Db, to: &Db) -> usize {
        SyncTable::ALL
            .into_iter()
            .map(|table| {
                let rows: Vec<_> = from.local_changes(table, 0, 1000).unwrap().into_iter().map(|(_, r)| r).collect();
                to.merge_remote(table, &rows).unwrap()
            })
            .sum()
    }

    #[test]
    fn rows_travel_and_merged_rows_stay_put() {
        let (a, b) = devices();
        let mut dune = Book::new("Dune");
        dune.status = Status::Reading;
        a.save_book(&mut dune).unwrap();
        let mut s = a.new_session(&dune.id, 0, 60_000).unwrap();
        s.page_to = Some(30);
        a.add_session(&s).unwrap();

        assert_eq!(ship(&a, &b), 3, "book, reading and session");
        assert_eq!(b.get_book(&dune.id).unwrap().unwrap().title, "Dune");
        assert_eq!(b.list_sessions(None).unwrap()[0].device, "laptop");
        assert_eq!(b.list_readings().unwrap().len(), 1);
        // What arrived isn't sent on from b.
        assert!(SyncTable::ALL.iter().all(|t| b.local_changes(*t, 0, 10).unwrap().is_empty()));
        assert_eq!(ship(&a, &b), 0, "again changes nothing");
    }

    #[test]
    fn the_later_edit_wins_on_both_sides() {
        let (a, b) = devices();
        let mut dune = Book::new("Dune");
        a.save_book(&mut dune).unwrap();
        ship(&a, &b);

        let mut on_b = b.get_book(&dune.id).unwrap().unwrap();
        on_b.rating = 5;
        b.save_book(&mut on_b).unwrap();
        let mut on_a = a.get_book(&dune.id).unwrap().unwrap();
        on_a.updated_at = on_b.updated_at; // save_book bumps it past b's edit
        on_a.notes = Some("later".into());
        a.save_book(&mut on_a).unwrap();

        ship(&a, &b);
        ship(&b, &a);
        let (x, y) = (a.get_book(&dune.id).unwrap().unwrap(), b.get_book(&dune.id).unwrap().unwrap());
        assert_eq!(x, y);
        assert_eq!((x.notes.as_deref(), x.rating), (Some("later"), 0), "a's edit is the later one");
    }

    #[test]
    fn ties_go_to_the_larger_device_id_and_deletes_travel() {
        let (a, b) = devices();
        let mut dune = Book::new("Dune");
        a.save_book(&mut dune).unwrap();
        ship(&a, &b);
        // Same timestamp on both: dev-b wins everywhere.
        for (db, title) in [(&a, "A"), (&b, "B")] {
            db.conn
                .execute("UPDATE books SET title = ?1, updated_at = 99999999999999, device_id = ?2, rev = 50", [
                    title,
                    db.device_id(),
                ])
                .unwrap();
        }
        ship(&a, &b);
        ship(&b, &a);
        assert_eq!(a.get_book(&dune.id).unwrap().unwrap().title, "B");
        assert_eq!(b.get_book(&dune.id).unwrap().unwrap().title, "B");

        let (c, d) = devices();
        let mut emma = Book::new("Emma");
        c.save_book(&mut emma).unwrap();
        ship(&c, &d);
        c.delete_book(&emma.id).unwrap();
        ship(&c, &d);
        assert!(d.get_book(&emma.id).unwrap().is_none());
    }

    #[test]
    fn a_session_recorded_elsewhere_settles_the_inbox() {
        let (a, b) = devices();
        for db in [&a, &b] {
            for title in ["Dune", "Emma"] {
                let mut book = Book::new(title);
                book.id = format!("id-{title}");
                book.status = Status::Reading;
                db.save_book(&mut book).unwrap();
            }
        }
        // Both see the habit session; a doesn't know the book, b does.
        a.record_habit_session("read", 1_000, 61_000, 60_000).unwrap();
        b.set_pin(Some("id-Emma")).unwrap();
        let now = crate::db::now_ms();
        b.record_habit_session("read", 1_000, now, 60_000).unwrap();
        assert_eq!(a.list_pending().unwrap().len(), 1);
        ship(&b, &a);
        assert!(a.list_pending().unwrap().is_empty());
        assert_eq!(a.list_sessions(Some("id-Emma")).unwrap().len(), 1);
    }

    #[test]
    fn rows_from_newer_versions_are_skipped() {
        let (_, b) = devices();
        let rows = [serde_json::json!({"id": "x"}), serde_json::json!("nonsense"), serde_json::json!({"id": "y", "title": ["?"]})];
        assert_eq!(b.merge_remote(SyncTable::Books, &rows).unwrap(), 0);
    }

    #[test]
    fn cursors_are_kept_and_cleared() {
        let (a, _) = devices();
        a.set_sync_value("push:books", "3:17").unwrap();
        assert_eq!(a.sync_value("push:books").unwrap().as_deref(), Some("3:17"));
        a.clear_sync_state().unwrap();
        assert_eq!(a.sync_value("push:books").unwrap(), None);
    }
}
