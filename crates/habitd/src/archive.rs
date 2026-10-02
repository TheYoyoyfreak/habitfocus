//! The long-term archive, `history.db` next to state.json: every event,
//! session and visit, kept without limit. state.json only holds what the
//! engine needs to run; this is for looking back.
//!
//! Only habitd opens the database. Clients ask through the socket.

use habit_core::archive::{Afk, AfkReason, Record, Visit};
use habit_core::state::{Event, EventKind, Outcome, State};
use rusqlite::{params, Connection};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::Path;

/// Schema version, stored in `PRAGMA user_version`.
const VERSION: i64 = 3;

const SCHEMA: &str = "
    CREATE TABLE events (
        id INTEGER PRIMARY KEY,
        at_ms INTEGER NOT NULL,
        kind TEXT NOT NULL,
        subject TEXT,
        amount INTEGER NOT NULL,
        text TEXT NOT NULL
    );
    CREATE INDEX events_at ON events (at_ms);
    CREATE TABLE sessions (
        id INTEGER PRIMARY KEY,
        habit TEXT NOT NULL,
        started_ms INTEGER NOT NULL,
        finished_ms INTEGER NOT NULL,
        focused_ms INTEGER NOT NULL,
        outcome TEXT NOT NULL
    );
    CREATE INDEX sessions_finished ON sessions (finished_ms);
    CREATE TABLE visits (
        id INTEGER PRIMARY KEY,
        key TEXT NOT NULL,
        start_ms INTEGER NOT NULL,
        end_ms INTEGER NOT NULL,
        active_ms INTEGER NOT NULL,
        habit TEXT
    );
    CREATE INDEX visits_start ON visits (start_ms);
";

/// Time away from the computer (schema 3).
const AFK_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS afk (
        id INTEGER PRIMARY KEY,
        start_ms INTEGER NOT NULL,
        end_ms INTEGER NOT NULL,
        reason TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS afk_start ON afk (start_ms);
";

pub struct Archive {
    conn: Connection,
}

/// A serde unit enum as its bare name, e.g. `credit_expired`.
fn name<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

fn from_name<T: DeserializeOwned>(name: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(name.to_string())).ok()
}

impl Archive {
    /// Opens (or creates) the archive. A new archive starts with the log and
    /// session history still in `state`, so nothing from before is lost.
    pub fn open(path: &Path, state: &State) -> anyhow::Result<Archive> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let mut archive = Archive { conn };
        archive.migrate(state)?;
        Ok(archive)
    }

    #[cfg(test)]
    fn in_memory(state: &State) -> Archive {
        let mut archive = Archive { conn: Connection::open_in_memory().unwrap() };
        archive.migrate(state).unwrap();
        archive
    }

    fn migrate(&mut self, state: &State) -> anyhow::Result<()> {
        let version: i64 = self.conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > VERSION {
            anyhow::bail!("history.db is from a newer habitd (schema {version}, this one knows {VERSION})");
        }
        if version == 1 || version == 2 {
            let tx = self.conn.transaction()?;
            if version == 1 {
                // Visits learned which habit they counted towards.
                tx.execute_batch("ALTER TABLE visits ADD COLUMN habit TEXT")?;
            }
            tx.execute_batch(AFK_SCHEMA)?;
            tx.pragma_update(None, "user_version", VERSION)?;
            tx.commit()?;
        }
        if version == 0 {
            let tx = self.conn.transaction()?;
            tx.execute_batch(SCHEMA)?;
            tx.execute_batch(AFK_SCHEMA)?;
            let imported: Vec<Record> = state
                .events
                .iter()
                .cloned()
                .map(Record::Event)
                .chain(state.history.iter().cloned().map(Record::Session))
                .collect();
            insert(&tx, &imported)?;
            tx.pragma_update(None, "user_version", VERSION)?;
            tx.commit()?;
        }
        Ok(())
    }

    /// Writes a batch of records in one transaction.
    pub fn write(&mut self, records: &[Record]) -> anyhow::Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        insert(&tx, records)?;
        tx.commit()?;
        Ok(())
    }

    /// The most recent events, newest first. Events of kinds this habitd
    /// doesn't know (written by a newer one) are skipped.
    pub fn events(&self, limit: usize) -> anyhow::Result<Vec<Event>> {
        let mut stmt = self
            .conn
            .prepare("SELECT at_ms, kind, subject, amount, text FROM events ORDER BY at_ms DESC, id DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get(2)?, row.get::<_, i64>(3)?, row.get(4)?))
        })?;
        let mut events = Vec::new();
        for row in rows {
            let (at, kind, subject, amount, text) = row?;
            if let Some(kind) = from_name::<EventKind>(&kind) {
                events.push(Event { at: at as u64, kind, subject, amount: amount as u64, text });
            }
        }
        Ok(events)
    }

    /// Visits that overlap `from..to`, oldest first.
    pub fn visits(&self, from: u64, to: u64) -> anyhow::Result<Vec<Visit>> {
        let mut stmt = self.conn.prepare(
            "SELECT key, start_ms, end_ms, active_ms, habit FROM visits
             WHERE end_ms > ?1 AND start_ms < ?2 ORDER BY start_ms, id",
        )?;
        let rows = stmt.query_map(params![from as i64, to as i64], |row| {
            Ok(Visit {
                key: row.get(0)?,
                start: row.get::<_, i64>(1)? as u64,
                end: row.get::<_, i64>(2)? as u64,
                active_ms: row.get::<_, i64>(3)? as u64,
                habit: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

impl Archive {
    /// Time away overlapping `from`..`to`, oldest first. Reasons this habitd
    /// doesn't know are skipped.
    pub fn afk(&self, from: u64, to: u64) -> anyhow::Result<Vec<Afk>> {
        let mut stmt = self.conn.prepare(
            "SELECT start_ms, end_ms, reason FROM afk WHERE end_ms > ?1 AND start_ms < ?2 ORDER BY start_ms, id",
        )?;
        let rows = stmt.query_map(params![from as i64, to as i64], |row| {
            Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64, row.get::<_, String>(2)?))
        })?;
        let mut afk = Vec::new();
        for row in rows {
            let (start, end, reason) = row?;
            if let Some(reason) = from_name::<AfkReason>(&reason) {
                afk.push(Afk { start, end, reason });
            }
        }
        Ok(afk)
    }

    /// When the first visit was recorded, if any.
    pub fn first_visit(&self) -> anyhow::Result<Option<u64>> {
        let first: Option<i64> =
            self.conn.query_row("SELECT MIN(start_ms) FROM visits", [], |row| row.get(0))?;
        Ok(first.map(|ms| ms as u64))
    }
}

fn insert(conn: &Connection, records: &[Record]) -> rusqlite::Result<()> {
    let mut event = conn.prepare_cached(
        "INSERT INTO events (at_ms, kind, subject, amount, text) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    let mut session = conn.prepare_cached(
        "INSERT INTO sessions (habit, started_ms, finished_ms, focused_ms, outcome) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    let mut visit = conn.prepare_cached(
        "INSERT INTO visits (key, start_ms, end_ms, active_ms, habit) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    let mut afk = conn.prepare_cached("INSERT INTO afk (start_ms, end_ms, reason) VALUES (?1, ?2, ?3)")?;
    for record in records {
        match record {
            Record::Event(e) => {
                event.execute(params![e.at as i64, name(&e.kind), e.subject, e.amount as i64, e.text])?;
            }
            Record::Session(s) => {
                session.execute(params![
                    s.habit,
                    s.started_at as i64,
                    s.finished_at as i64,
                    s.focused_ms as i64,
                    name::<Outcome>(&s.outcome)
                ])?;
            }
            Record::Visit(v) => {
                visit.execute(params![v.key, v.start as i64, v.end as i64, v.active_ms as i64, v.habit])?;
            }
            Record::Afk(a) => {
                afk.execute(params![a.start as i64, a.end as i64, name(&a.reason)])?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use habit_core::state::HistoryEntry;

    fn event(at: u64, text: &str) -> Event {
        Event { at, kind: EventKind::CreditEarned, subject: Some("social".into()), amount: 60_000, text: text.into() }
    }

    #[test]
    fn imports_state_once_and_appends() {
        let state = State {
            events: vec![event(1, "old one"), event(2, "old two")],
            history: vec![HistoryEntry {
                habit: "read".into(),
                started_at: 0,
                finished_at: 10,
                focused_ms: 10,
                outcome: Outcome::Completed,
            }],
            ..State::default()
        };
        let mut archive = Archive::in_memory(&state);
        // A second migrate (the next start) doesn't import again.
        archive.migrate(&state).unwrap();
        archive.write(&[Record::Event(event(3, "new"))]).unwrap();

        let texts: Vec<String> = archive.events(10).unwrap().into_iter().map(|e| e.text).collect();
        assert_eq!(texts, ["new", "old two", "old one"]);
        assert_eq!(archive.events(1).unwrap()[0].kind, EventKind::CreditEarned);
        let sessions: i64 = archive.conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0)).unwrap();
        assert_eq!(sessions, 1);
    }

    #[test]
    fn visits_are_queried_by_overlap() {
        let mut archive = Archive::in_memory(&State::default());
        let visit = |key: &str, start, end| {
            Record::Visit(Visit { key: key.into(), start, end, active_ms: end - start, habit: None })
        };
        archive.write(&[visit("kitty", 0, 100), visit("zen", 100, 200), visit("kitty", 300, 400)]).unwrap();
        let keys = |from, to| archive.visits(from, to).unwrap().into_iter().map(|v| v.key).collect::<Vec<_>>();
        assert_eq!(keys(50, 150), ["kitty", "zen"]);
        assert_eq!(keys(200, 300), Vec::<String>::new());
        assert_eq!(archive.first_visit().unwrap(), Some(0));
        assert_eq!(Archive::in_memory(&State::default()).first_visit().unwrap(), None);

        // A database from before visits knew about habits keeps its rows.
        let old = Archive::in_memory(&State::default());
        old.conn.execute_batch("DROP TABLE visits; CREATE TABLE visits (id INTEGER PRIMARY KEY, key TEXT NOT NULL, start_ms INTEGER NOT NULL, end_ms INTEGER NOT NULL, active_ms INTEGER NOT NULL); INSERT INTO visits (key, start_ms, end_ms, active_ms) VALUES ('kitty', 0, 100, 100)").unwrap();
        old.conn.pragma_update(None, "user_version", 1).unwrap();
        let mut old = Archive { conn: old.conn };
        old.migrate(&State::default()).unwrap();
        let visits = old.visits(0, 1000).unwrap();
        assert_eq!((visits[0].key.as_str(), visits[0].habit.clone()), ("kitty", None));
        old.write(&[Record::Visit(Visit { key: "zed".into(), start: 0, end: 5, active_ms: 5, habit: Some("read".into()) })]).unwrap();
        assert_eq!(old.visits(0, 1000).unwrap()[1].habit.as_deref(), Some("read"));
    }

    #[test]
    fn afk_is_queried_by_overlap_and_added_to_old_databases() {
        let mut archive = Archive::in_memory(&State::default());
        let afk = |start, end, reason| Record::Afk(Afk { start, end, reason });
        archive.write(&[afk(0, 100, AfkReason::Idle), afk(200, 300, AfkReason::Asleep)]).unwrap();
        assert_eq!(archive.afk(50, 250).unwrap().len(), 2);
        assert_eq!(archive.afk(100, 200).unwrap(), []);
        assert_eq!(archive.afk(250, 400).unwrap()[0].reason, AfkReason::Asleep);

        // Schema 2 gains the table.
        let old = Archive::in_memory(&State::default());
        old.conn.execute_batch("DROP TABLE afk").unwrap();
        old.conn.pragma_update(None, "user_version", 2).unwrap();
        let mut old = Archive { conn: old.conn };
        old.migrate(&State::default()).unwrap();
        old.write(&[afk(0, 10, AfkReason::Idle)]).unwrap();
        assert_eq!(old.afk(0, 10).unwrap().len(), 1);
        let version: i64 = old.conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, VERSION);
    }

    #[test]
    fn unknown_event_kinds_are_skipped() {
        let archive = Archive::in_memory(&State::default());
        archive
            .conn
            .execute("INSERT INTO events (at_ms, kind, amount, text) VALUES (1, 'from_the_future', 0, 'x')", [])
            .unwrap();
        assert!(archive.events(10).unwrap().is_empty());
    }
}
