//! The long-term archive, `history.db` next to state.json: every event,
//! session and visit, kept without limit. state.json only holds what the
//! engine needs to run; this is for looking back.
//!
//! Only habitd opens the database. Clients ask through the socket.
//!
//! Records of other devices (pulled by sync) live in the same tables with
//! their `device` set; this device's own rows have `device` NULL. Every query
//! for the engine and the views reads only this device's rows.

use habit_core::archive::{Afk, AfkReason, Record, Visit};
use habit_core::state::{Event, EventKind, HistoryEntry, Outcome, State};
use rusqlite::{params, Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::Path;

/// Schema version, stored in `PRAGMA user_version`.
const VERSION: i64 = 4;

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

/// Sync (schema 4): who wrote a row, and this installation's identity.
///
/// `device` is NULL for this device's rows and the id of the other device for
/// pulled ones. `uid` identifies a pulled row across devices
/// (`<device>:<table>:<rowid there>`), so pulling it twice inserts it once.
/// This device's rows need no stored uid: theirs is derived from the rowid.
const SYNC_SCHEMA: &str = "
    ALTER TABLE events ADD COLUMN device TEXT;
    ALTER TABLE events ADD COLUMN uid TEXT;
    CREATE UNIQUE INDEX events_uid ON events (uid);
    ALTER TABLE sessions ADD COLUMN device TEXT;
    ALTER TABLE sessions ADD COLUMN uid TEXT;
    CREATE UNIQUE INDEX sessions_uid ON sessions (uid);
    ALTER TABLE visits ADD COLUMN device TEXT;
    ALTER TABLE visits ADD COLUMN uid TEXT;
    CREATE UNIQUE INDEX visits_uid ON visits (uid);
    ALTER TABLE afk ADD COLUMN device TEXT;
    ALTER TABLE afk ADD COLUMN uid TEXT;
    CREATE UNIQUE INDEX afk_uid ON afk (uid);

    -- One row: this installation. The id is its `host` on the sync server.
    CREATE TABLE device (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL
    );
    -- The other devices of the sync account.
    CREATE TABLE devices (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        last_seen_ms INTEGER
    );
    -- Sync bookkeeping: cursors per table and device.
    CREATE TABLE sync_state (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
";

/// This installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// A random UUID, created with the archive.
    pub id: String,
    /// The hostname at the time.
    pub name: String,
}

/// The record tables. Each is one log per device on the sync server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Events,
    Sessions,
    Visits,
    Afk,
}

impl Table {
    pub const ALL: [Table; 4] = [Table::Events, Table::Sessions, Table::Visits, Table::Afk];

    /// The SQL table, also the record tag on the sync server.
    pub fn name(self) -> &'static str {
        match self {
            Table::Events => "events",
            Table::Sessions => "sessions",
            Table::Visits => "visits",
            Table::Afk => "afk",
        }
    }
}

/// Whose rows a query reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Devices {
    /// This device (the rows with `device` NULL).
    This,
    /// Another device, pulled by sync.
    One(String),
    /// Every device.
    All,
}

impl Devices {
    /// The value queries compare `device` with: `device IS ?` matches NULL for
    /// this device, and `'*'` stands for every device.
    fn param(&self) -> Option<&str> {
        match self {
            Devices::This => None,
            Devices::One(id) => Some(id),
            Devices::All => Some("*"),
        }
    }
}

/// Another device of the sync account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDevice {
    pub id: String,
    pub name: String,
    pub last_seen_ms: Option<u64>,
    /// Rows pulled from it.
    pub rows: u64,
}

/// A row of this device, as sync sends it.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalRow {
    pub rowid: i64,
    /// Unique across devices: `<device>:<table>:<rowid>`.
    pub uid: String,
    pub record: Record,
}

pub struct Archive {
    conn: Connection,
        device: Device,
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
        let mut conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // The sync thread has a connection of its own.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let device = migrate(&mut conn, state)?;
        Ok(Archive { conn, device })
    }

    #[cfg(test)]
    pub(crate) fn in_memory(state: &State) -> Archive {
        let mut conn = Connection::open_in_memory().unwrap();
        let device = migrate(&mut conn, state).unwrap();
        Archive { conn, device }
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
            .prepare("SELECT at_ms, kind, subject, amount, text FROM events WHERE device IS NULL
             ORDER BY at_ms DESC, id DESC LIMIT ?1")?;
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

    #[cfg(test)]
    pub fn visits(&self, from: u64, to: u64) -> anyhow::Result<Vec<Visit>> {
        self.visits_of(&Devices::This, from, to)
    }

    /// Visits of `devices` that overlap `from..to`, oldest first.
    pub fn visits_of(&self, devices: &Devices, from: u64, to: u64) -> anyhow::Result<Vec<Visit>> {
        let mut stmt = self.conn.prepare(
            "SELECT key, start_ms, end_ms, active_ms, habit FROM visits
             WHERE (device IS ?3 OR ?3 = '*') AND end_ms > ?1 AND start_ms < ?2 ORDER BY start_ms, id",
        )?;
        let rows = stmt.query_map(params![from as i64, to as i64, devices.param()], |row| {
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

/// What sync needs: this device's rows by cursor, other devices' rows by uid.
impl Archive {
    /// This installation.
    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Stores records another device wrote, keyed by their uid; ones already
    /// here are skipped. Returns how many were new.
    pub fn insert_remote(&mut self, device: &str, rows: &[(String, Record)]) -> anyhow::Result<usize> {
        let tx = self.conn.transaction()?;
        let mut added = 0;
        for (uid, record) in rows {
            added += insert_row(&tx, record, Some((device, uid)))?;
        }
        tx.commit()?;
        Ok(added)
    }

    /// This device's rows of `table` after rowid `after`, oldest first.
    pub fn local_rows(&self, table: Table, after: i64, limit: usize) -> anyhow::Result<Vec<LocalRow>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let row = |rowid: i64, record: Record| LocalRow {
            rowid,
            uid: format!("{}:{}:{rowid}", self.device.id, table.name()),
            record,
        };
        let mut rows = Vec::new();
        match table {
            Table::Events => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, at_ms, kind, subject, amount, text FROM events
                     WHERE device IS NULL AND id > ?1 ORDER BY id LIMIT ?2",
                )?;
                let mut query = stmt.query(params![after, limit])?;
                while let Some(r) = query.next()? {
                    // Kinds this habitd doesn't know can't come from here.
                    let Some(kind) = from_name::<EventKind>(&r.get::<_, String>(2)?) else { continue };
                    let event =
                        Event { at: r.get::<_, i64>(1)? as u64, kind, subject: r.get(3)?, amount: r.get::<_, i64>(4)? as u64, text: r.get(5)? };
                    rows.push(row(r.get(0)?, Record::Event(event)));
                }
            }
            Table::Sessions => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, habit, started_ms, finished_ms, focused_ms, outcome FROM sessions
                     WHERE device IS NULL AND id > ?1 ORDER BY id LIMIT ?2",
                )?;
                let mut query = stmt.query(params![after, limit])?;
                while let Some(r) = query.next()? {
                    let Some(outcome) = from_name::<Outcome>(&r.get::<_, String>(5)?) else { continue };
                    let session = HistoryEntry {
                        habit: r.get(1)?,
                        started_at: r.get::<_, i64>(2)? as u64,
                        finished_at: r.get::<_, i64>(3)? as u64,
                        focused_ms: r.get::<_, i64>(4)? as u64,
                        outcome,
                    };
                    rows.push(row(r.get(0)?, Record::Session(session)));
                }
            }
            Table::Visits => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, key, start_ms, end_ms, active_ms, habit FROM visits
                     WHERE device IS NULL AND id > ?1 ORDER BY id LIMIT ?2",
                )?;
                let mut query = stmt.query(params![after, limit])?;
                while let Some(r) = query.next()? {
                    let visit = Visit {
                        key: r.get(1)?,
                        start: r.get::<_, i64>(2)? as u64,
                        end: r.get::<_, i64>(3)? as u64,
                        active_ms: r.get::<_, i64>(4)? as u64,
                        habit: r.get(5)?,
                    };
                    rows.push(row(r.get(0)?, Record::Visit(visit)));
                }
            }
            Table::Afk => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, start_ms, end_ms, reason FROM afk WHERE device IS NULL AND id > ?1 ORDER BY id LIMIT ?2",
                )?;
                let mut query = stmt.query(params![after, limit])?;
                while let Some(r) = query.next()? {
                    let Some(reason) = from_name::<AfkReason>(&r.get::<_, String>(3)?) else { continue };
                    let afk = Afk { start: r.get::<_, i64>(1)? as u64, end: r.get::<_, i64>(2)? as u64, reason };
                    rows.push(row(r.get(0)?, Record::Afk(afk)));
                }
            }
        }
        Ok(rows)
    }

    /// A value of the sync bookkeeping, e.g. a cursor.
    pub fn sync_value(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM sync_state WHERE key = ?1", params![key], |row| row.get(0))
            .optional()?)
    }

    pub fn set_sync_value(&mut self, key: &str, value: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO sync_state (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Forgets cursors and devices, for signing out. Pulled rows stay.
    pub fn clear_sync_state(&mut self) -> anyhow::Result<()> {
        self.conn.execute_batch("DELETE FROM sync_state; DELETE FROM devices;")?;
        Ok(())
    }

    /// The other devices of the sync account with how many rows came from
    /// each, by name.
    pub fn remote_devices(&self) -> anyhow::Result<Vec<RemoteDevice>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, last_seen_ms,
                (SELECT COUNT(*) FROM events WHERE device = devices.id)
                + (SELECT COUNT(*) FROM sessions WHERE device = devices.id)
                + (SELECT COUNT(*) FROM visits WHERE device = devices.id)
                + (SELECT COUNT(*) FROM afk WHERE device = devices.id)
             FROM devices ORDER BY name, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(RemoteDevice {
                id: r.get(0)?,
                name: r.get(1)?,
                last_seen_ms: r.get::<_, Option<i64>>(2)?.map(|ms| ms as u64),
                rows: r.get::<_, i64>(3)? as u64,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Remembers another device of the sync account.
    pub fn set_remote_device(&mut self, id: &str, name: &str, last_seen_ms: Option<u64>) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO devices (id, name, last_seen_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT (id) DO UPDATE SET name = excluded.name, last_seen_ms = excluded.last_seen_ms",
            params![id, name, last_seen_ms.map(|ms| ms as i64)],
        )?;
        Ok(())
    }
}

impl Archive {
    #[cfg(test)]
    pub fn afk(&self, from: u64, to: u64) -> anyhow::Result<Vec<Afk>> {
        self.afk_of(&Devices::This, from, to)
    }

    /// Time away of `devices` overlapping `from`..`to`, oldest first. Reasons
    /// this habitd doesn't know are skipped.
    pub fn afk_of(&self, devices: &Devices, from: u64, to: u64) -> anyhow::Result<Vec<Afk>> {
        let mut stmt = self.conn.prepare(
            "SELECT start_ms, end_ms, reason FROM afk
             WHERE (device IS ?3 OR ?3 = '*') AND end_ms > ?1 AND start_ms < ?2 ORDER BY start_ms, id",
        )?;
        let rows = stmt.query_map(params![from as i64, to as i64, devices.param()], |row| {
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

    #[cfg(test)]
    pub fn first_visit(&self) -> anyhow::Result<Option<u64>> {
        self.first_visit_of(&Devices::This)
    }

    /// When the first visit of `devices` was recorded, if any.
    pub fn first_visit_of(&self, devices: &Devices) -> anyhow::Result<Option<u64>> {
        let first: Option<i64> = self.conn.query_row(
            "SELECT MIN(start_ms) FROM visits WHERE device IS ?1 OR ?1 = '*'",
            params![devices.param()],
            |row| row.get(0),
        )?;
        Ok(first.map(|ms| ms as u64))
    }
}

#[cfg(test)]
impl Archive {
    /// Keys of the visits pulled from `device`, oldest first.
    pub(crate) fn remote_visit_keys(&self, device: &str) -> Vec<String> {
        let mut stmt = self.conn.prepare("SELECT key FROM visits WHERE device = ?1 ORDER BY start_ms, id").unwrap();
        stmt.query_map(params![device], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
    }
}

/// Brings the schema up to date and returns this installation, creating
/// its identity the first time.
fn migrate(conn: &mut Connection, state: &State) -> anyhow::Result<Device> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > VERSION {
        anyhow::bail!("history.db is from a newer habitd (schema {version}, this one knows {VERSION})");
    }
    let tx = conn.transaction()?;
    if version == 0 {
        tx.execute_batch(SCHEMA)?;
    }
    if version == 1 {
        // Visits learned which habit they counted towards.
        tx.execute_batch("ALTER TABLE visits ADD COLUMN habit TEXT")?;
    }
    if version < 3 {
        tx.execute_batch(AFK_SCHEMA)?;
    }
    if version < 4 {
        tx.execute_batch(SYNC_SCHEMA)?;
    }
    if version == 0 {
        // A new archive starts with the history still in state.json.
        let imported: Vec<Record> = state
            .events
            .iter()
            .cloned()
            .map(Record::Event)
            .chain(state.history.iter().cloned().map(Record::Session))
            .collect();
        insert(&tx, &imported)?;
    }
    if version < VERSION {
        tx.pragma_update(None, "user_version", VERSION)?;
    }
    let existing = tx.query_row("SELECT id, name FROM device", [], |r| Ok(Device { id: r.get(0)?, name: r.get(1)? })).optional()?;
    let device = match existing {
        Some(device) => device,
        None => {
            let device = Device { id: uuid::Uuid::new_v4().to_string(), name: hostname() };
            tx.execute("INSERT INTO device (id, name) VALUES (?1, ?2)", params![device.id, device.name])?;
            device
        }
    };
    tx.commit()?;
    Ok(device)
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_string())
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "habitfocus".to_string())
}

/// Writes this device's records.
fn insert(conn: &Connection, records: &[Record]) -> rusqlite::Result<()> {
    for record in records {
        insert_row(conn, record, None)?;
    }
    Ok(())
}

/// Writes one record: this device's (`origin` None) or another device's,
/// skipped when its uid is already here. Returns the rows written.
fn insert_row(conn: &Connection, record: &Record, origin: Option<(&str, &str)>) -> rusqlite::Result<usize> {
    let (device, uid) = origin.unzip();
    let verb = if origin.is_some() { "INSERT OR IGNORE" } else { "INSERT" };
    match record {
        Record::Event(e) => conn
            .prepare_cached(&format!(
                "{verb} INTO events (at_ms, kind, subject, amount, text, device, uid) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
            ))?
            .execute(params![e.at as i64, name(&e.kind), e.subject, e.amount as i64, e.text, device, uid]),
        Record::Session(s) => conn
            .prepare_cached(&format!(
                "{verb} INTO sessions (habit, started_ms, finished_ms, focused_ms, outcome, device, uid)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
            ))?
            .execute(params![
                s.habit,
                s.started_at as i64,
                s.finished_at as i64,
                s.focused_ms as i64,
                name::<Outcome>(&s.outcome),
                device,
                uid
            ]),
        Record::Visit(v) => conn
            .prepare_cached(&format!(
                "{verb} INTO visits (key, start_ms, end_ms, active_ms, habit, device, uid) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
            ))?
            .execute(params![v.key, v.start as i64, v.end as i64, v.active_ms as i64, v.habit, device, uid]),
        Record::Afk(a) => conn
            .prepare_cached(&format!(
                "{verb} INTO afk (start_ms, end_ms, reason, device, uid) VALUES (?1, ?2, ?3, ?4, ?5)"
            ))?
            .execute(params![a.start as i64, a.end as i64, name(&a.reason), device, uid]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use habit_core::state::HistoryEntry;

    /// A database as an older habitd left it, at schema `version` (1 to 3).
    fn old_database(version: i64) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        if version == 1 {
            conn.execute_batch("ALTER TABLE visits DROP COLUMN habit").unwrap();
        }
        if version >= 3 {
            conn.execute_batch(AFK_SCHEMA).unwrap();
        }
        conn.pragma_update(None, "user_version", version).unwrap();
        conn
    }

    fn migrated(mut conn: Connection) -> Archive {
        let device = migrate(&mut conn, &State::default()).unwrap();
        Archive { conn, device }
    }

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
        migrate(&mut archive.conn, &state).unwrap();
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
        let conn = old_database(1);
        conn.execute("INSERT INTO visits (key, start_ms, end_ms, active_ms) VALUES ('kitty', 0, 100, 100)", []).unwrap();
        let mut old = migrated(conn);
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
        let mut old = migrated(old_database(2));
        old.write(&[afk(0, 10, AfkReason::Idle)]).unwrap();
        assert_eq!(old.afk(0, 10).unwrap().len(), 1);
        let version: i64 = old.conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, VERSION);
    }

    fn one_of_each(at: u64) -> Vec<Record> {
        vec![
            Record::Event(event(at, "earned")),
            Record::Session(HistoryEntry {
                habit: "read".into(),
                started_at: at,
                finished_at: at + 10,
                focused_ms: 10,
                outcome: Outcome::Completed,
            }),
            Record::Visit(Visit { key: "site:example.org".into(), start: at, end: at + 5, active_ms: 5, habit: None }),
            Record::Afk(Afk { start: at + 5, end: at + 9, reason: AfkReason::Asleep }),
        ]
    }

    #[test]
    fn schema_3_gains_sync_and_keeps_its_rows() {
        let conn = old_database(3);
        conn.execute("INSERT INTO visits (key, start_ms, end_ms, active_ms) VALUES ('kitty', 0, 100, 100)", []).unwrap();
        conn.execute("INSERT INTO afk (start_ms, end_ms, reason) VALUES (100, 200, 'idle')", []).unwrap();
        let archive = migrated(conn);
        assert_eq!(archive.visits(0, 1000).unwrap()[0].key, "kitty");
        assert_eq!(archive.afk(0, 1000).unwrap().len(), 1);
        let version: i64 = archive.conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, VERSION);
        // Rows from before sync are this device's and go out with the first push.
        let rows = archive.local_rows(Table::Visits, 0, 10).unwrap();
        assert_eq!(rows[0].uid, format!("{}:visits:1", archive.device().id));
    }

    #[test]
    fn the_device_is_created_once_and_kept() {
        let dir = std::env::temp_dir().join(format!("habitfocus-archive-{}", uuid::Uuid::new_v4()));
        let path = dir.join("history.db");
        let first = Archive::open(&path, &State::default()).unwrap().device().clone();
        let again = Archive::open(&path, &State::default()).unwrap().device().clone();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(first, again);
        assert!(uuid::Uuid::parse_str(&first.id).is_ok(), "{first:?}");
        assert!(!first.name.is_empty());
        assert_ne!(Archive::in_memory(&State::default()).device().id, first.id, "every archive is its own device");
    }

    #[test]
    fn local_rows_come_by_cursor_with_their_uid() {
        let mut archive = Archive::in_memory(&State::default());
        archive.write(&one_of_each(0)).unwrap();
        archive.write(&one_of_each(100)).unwrap();
        let id = archive.device().id.clone();

        // `one_of_each` is in the order of `Table::ALL`.
        for (table, written) in Table::ALL.into_iter().zip(one_of_each(100)) {
            let rows = archive.local_rows(table, 0, 10).unwrap();
            assert_eq!(rows.iter().map(|r| r.rowid).collect::<Vec<_>>(), [1, 2], "{table:?}");
            assert_eq!(rows[1].uid, format!("{id}:{}:2", table.name()));
            assert_eq!(rows[1].record, written, "read back as written");
        }
        let after = archive.local_rows(Table::Visits, 1, 10).unwrap();
        assert_eq!(after.iter().map(|r| r.rowid).collect::<Vec<_>>(), [2]);
        assert_eq!(archive.local_rows(Table::Visits, 0, 1).unwrap().len(), 1, "limited");
        assert!(archive.local_rows(Table::Visits, 2, 10).unwrap().is_empty());
    }

    #[test]
    fn remote_rows_are_stored_once_and_kept_out_of_this_devices_views() {
        let mut archive = Archive::in_memory(&State::default());
        archive.write(&[Record::Visit(Visit { key: "kitty".into(), start: 500, end: 600, active_ms: 100, habit: None })]).unwrap();
        let laptop = "0199b4c1-0000-7000-8000-000000000001";
        let rows: Vec<(String, Record)> =
            one_of_each(0).into_iter().enumerate().map(|(i, r)| (format!("{laptop}:x:{i}"), r)).collect();
        assert_eq!(archive.insert_remote(laptop, &rows).unwrap(), 4);
        assert_eq!(archive.insert_remote(laptop, &rows).unwrap(), 0, "pulling again changes nothing");

        assert_eq!(archive.visits(0, 1000).unwrap().iter().map(|v| v.key.as_str()).collect::<Vec<_>>(), ["kitty"]);
        assert!(archive.events(10).unwrap().is_empty());
        assert!(archive.afk(0, 1000).unwrap().is_empty());
        assert_eq!(archive.first_visit().unwrap(), Some(500));
        for table in Table::ALL {
            let rows = archive.local_rows(table, 0, 10).unwrap();
            assert!(rows.iter().all(|r| r.uid.starts_with(&archive.device().id)), "{table:?}: {rows:?}");
        }
        let stored: i64 = archive
            .conn
            .query_row("SELECT COUNT(*) FROM visits WHERE device = ?1", params![laptop], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, 1);
    }

    #[test]
    fn queries_read_one_device_or_all() {
        let mut archive = Archive::in_memory(&State::default());
        archive.write(&[Record::Visit(Visit { key: "kitty".into(), start: 50, end: 60, active_ms: 10, habit: None })]).unwrap();
        archive.write(&[Record::Afk(Afk { start: 70, end: 80, reason: AfkReason::Idle })]).unwrap();
        let laptop = "0199b4c1-0000-7000-8000-000000000001";
        let rows: Vec<(String, Record)> =
            one_of_each(0).into_iter().enumerate().map(|(i, r)| (format!("{laptop}:x:{i}"), r)).collect();
        archive.insert_remote(laptop, &rows).unwrap();

        let keys = |devices: &Devices| -> Vec<String> {
            archive.visits_of(devices, 0, 1000).unwrap().into_iter().map(|v| v.key).collect()
        };
        assert_eq!(keys(&Devices::This), ["kitty"]);
        assert_eq!(keys(&Devices::One(laptop.into())), ["site:example.org"]);
        assert_eq!(keys(&Devices::All), ["site:example.org", "kitty"], "oldest first");
        assert!(keys(&Devices::One("unknown".into())).is_empty());
        assert_eq!(archive.afk_of(&Devices::One(laptop.into()), 0, 1000).unwrap().len(), 1);
        assert_eq!(archive.afk_of(&Devices::All, 0, 1000).unwrap().len(), 2);
        assert_eq!(archive.first_visit_of(&Devices::This).unwrap(), Some(50));
        assert_eq!(archive.first_visit_of(&Devices::All).unwrap(), Some(0));
    }

    #[test]
    fn sync_state_and_devices_are_kept() {
        let mut archive = Archive::in_memory(&State::default());
        assert_eq!(archive.sync_value("push:visits").unwrap(), None);
        archive.set_sync_value("push:visits", "41").unwrap();
        archive.set_sync_value("push:visits", "42").unwrap();
        assert_eq!(archive.sync_value("push:visits").unwrap().as_deref(), Some("42"));

        archive.set_remote_device("d1", "laptop", None).unwrap();
        archive.set_remote_device("d1", "old laptop", Some(1234)).unwrap();
        archive.insert_remote("d1", &[("d1:visits:1".into(), one_of_each(0).remove(2))]).unwrap();
        let devices = archive.remote_devices().unwrap();
        assert_eq!(
            devices,
            [RemoteDevice { id: "d1".into(), name: "old laptop".into(), last_seen_ms: Some(1234), rows: 1 }]
        );

        archive.clear_sync_state().unwrap();
        assert_eq!(archive.sync_value("push:visits").unwrap(), None);
        assert!(archive.remote_devices().unwrap().is_empty());
        assert_eq!(archive.conn.query_row("SELECT COUNT(*) FROM visits", [], |r| r.get::<_, i64>(0)).unwrap(), 1, "pulled rows stay");
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
