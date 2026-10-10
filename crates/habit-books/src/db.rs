//! SQLite storage (`books.db`, next to habitfocus' `history.db`). Every synced
//! table carries `updated_at`, `device_id` and `deleted_at` (soft delete) for
//! last-write-wins sync, and `rev`: a local change counter, so sync can push
//! what changed here since last time. Rows that arrive by sync keep `rev = 0`
//! and are never sent back.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::model::{Book, Format, Status};

/// Schema migrations, applied in order. `PRAGMA user_version` stores how many ran.
/// Never edit a released entry; append a new one.
const MIGRATIONS: &[&str] = &["CREATE TABLE settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    CREATE TABLE books (
        id         TEXT PRIMARY KEY,
        title      TEXT NOT NULL,
        subtitle   TEXT,
        author     TEXT,
        isbn       TEXT,
        olid       TEXT,
        pages      INTEGER,
        pub_year   INTEGER,
        format     TEXT NOT NULL,
        status     TEXT NOT NULL,
        rating     INTEGER NOT NULL DEFAULT 0,
        tags       TEXT NOT NULL DEFAULT '',
        notes      TEXT,
        cover_url  TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        deleted_at INTEGER,
        device_id  TEXT NOT NULL,
        rev        INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX books_rev ON books(rev);
    CREATE TABLE readings (
        id          TEXT PRIMARY KEY,
        book_id     TEXT NOT NULL,
        started_at  INTEGER NOT NULL,
        finished_at INTEGER,
        outcome     TEXT,
        created_at  INTEGER NOT NULL,
        updated_at  INTEGER NOT NULL,
        deleted_at  INTEGER,
        device_id   TEXT NOT NULL,
        rev         INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX readings_book ON readings(book_id);
    CREATE INDEX readings_rev ON readings(rev);
    CREATE TABLE reading_sessions (
        id           TEXT PRIMARY KEY,
        book_id      TEXT NOT NULL,
        started_at   INTEGER NOT NULL,
        ended_at     INTEGER NOT NULL,
        duration_ms  INTEGER NOT NULL,
        page_from    INTEGER,
        page_to      INTEGER,
        source       TEXT NOT NULL,
        device       TEXT NOT NULL,
        -- Deleted rows keep their ref, so a session doesn't come back.
        external_ref TEXT UNIQUE,
        created_at   INTEGER NOT NULL,
        updated_at   INTEGER NOT NULL,
        deleted_at   INTEGER,
        device_id    TEXT NOT NULL,
        rev          INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX sessions_book ON reading_sessions(book_id, started_at);
    CREATE INDEX sessions_started ON reading_sessions(started_at);
    CREATE INDEX sessions_rev ON reading_sessions(rev);
    -- Local to this device: sessions waiting to be assigned to a book.
    CREATE TABLE pending_sessions (
        id           TEXT PRIMARY KEY,
        started_at   INTEGER NOT NULL,
        ended_at     INTEGER NOT NULL,
        duration_ms  INTEGER NOT NULL,
        source       TEXT NOT NULL,
        device       TEXT NOT NULL,
        external_ref TEXT NOT NULL UNIQUE,
        created_at   INTEGER NOT NULL,
        discarded_at INTEGER
    );",
    // Ratings out of 10 (`rating` stays, out of 5, for older habitfocus). Rows
    // without a score (older devices) fall back to twice their rating.
    "ALTER TABLE books ADD COLUMN score INTEGER;"];

const BOOK_COLUMNS: &str = "id, title, subtitle, author, isbn, olid, pages, pub_year, format, status, \
                            rating, tags, notes, cover_url, created_at, updated_at, score";

/// [`BOOK_COLUMNS`] as read back.
const BOOK_SELECT: &str = "id, title, subtitle, author, isbn, olid, pages, pub_year, format, status, \
                           COALESCE(score, rating * 2), tags, notes, cover_url, created_at, updated_at";

pub struct Db {
    pub(crate) conn: Connection,
    pub(crate) device_id: String,
    pub(crate) device_name: String,
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

impl Db {
    /// Opens (creating and migrating) the database at `path`. `device_id` and
    /// `device_name` are habitfocus' own (its `history.db` device), so books
    /// and habit sessions agree on which device did what.
    pub fn open(path: &Path, device_id: &str, device_name: &str) -> anyhow::Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // The sync thread opens its own connection.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Db::init(conn, device_id, device_name)
    }

    pub fn open_in_memory() -> anyhow::Result<Db> {
        Db::init(Connection::open_in_memory()?, "test-device", "test")
    }

    fn init(conn: Connection, device_id: &str, device_name: &str) -> anyhow::Result<Db> {
        migrate(&conn)?;
        Ok(Db { conn, device_id: device_id.to_owned(), device_name: device_name.to_owned() })
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Name of this device in session records.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Next value of the local change counter (see the module docs).
    pub(crate) fn next_rev(&self) -> anyhow::Result<i64> {
        let rev: i64 = self.setting("rev")?.and_then(|r| r.parse().ok()).unwrap_or(0) + 1;
        self.set_setting("rev", Some(&rev.to_string()))?;
        Ok(rev)
    }

    /// Inserts or updates `book`, stamping `updated_at` (and `created_at` on first save).
    pub fn save_book(&self, book: &mut Book) -> anyhow::Result<()> {
        let now = now_ms();
        if book.created_at == 0 {
            book.created_at = now;
        }
        // Keep updated_at strictly increasing so last-write-wins sync can order edits.
        book.updated_at = now.max(book.updated_at + 1);
        let before = self.get_book(&book.id)?.map(|b| b.status);
        let rev = self.next_rev()?;
        self.conn.execute(
            &format!(
                "INSERT INTO books ({BOOK_COLUMNS}, deleted_at, device_id, rev)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?19, NULL, ?17, ?18)
                 ON CONFLICT(id) DO UPDATE SET
                    title = excluded.title, subtitle = excluded.subtitle, author = excluded.author,
                    isbn = excluded.isbn, olid = excluded.olid, pages = excluded.pages,
                    pub_year = excluded.pub_year, format = excluded.format, status = excluded.status,
                    rating = excluded.rating, score = excluded.score, tags = excluded.tags, notes = excluded.notes,
                    cover_url = excluded.cover_url, updated_at = excluded.updated_at,
                    deleted_at = NULL, device_id = excluded.device_id, rev = excluded.rev"
            ),
            params![
                book.id,
                book.title,
                book.subtitle,
                book.author,
                book.isbn,
                book.olid,
                book.pages,
                book.pub_year,
                book.format.as_str(),
                book.status.as_str(),
                book.score.div_ceil(2),
                book.tags.join(","),
                book.notes,
                book.cover_url,
                book.created_at,
                book.updated_at,
                self.device_id,
                rev,
                book.score,
            ],
        )?;
        if before != Some(book.status) {
            self.track_status_change(&book.id, book.status)?;
        }
        Ok(())
    }

    pub fn setting(&self, key: &str) -> anyhow::Result<Option<String>> {
        get_setting(&self.conn, key)
    }

    pub fn set_setting(&self, key: &str, value: Option<&str>) -> anyhow::Result<()> {
        match value {
            Some(v) => set_setting(&self.conn, key, v),
            None => {
                self.conn.execute("DELETE FROM settings WHERE key = ?1", [key])?;
                Ok(())
            }
        }
    }

    pub fn set_status(&self, id: &str, status: Status) -> anyhow::Result<Option<Book>> {
        let Some(mut book) = self.get_book(id)? else { return Ok(None) };
        book.status = status;
        self.save_book(&mut book)?;
        Ok(Some(book))
    }

    /// Soft delete: the row stays as a tombstone for sync.
    pub fn delete_book(&self, id: &str) -> anyhow::Result<bool> {
        let rev = self.next_rev()?;
        let n = self.conn.execute(
            "UPDATE books SET deleted_at = ?1, updated_at = MAX(?1, updated_at + 1), device_id = ?2, rev = ?3
             WHERE id = ?4 AND deleted_at IS NULL",
            params![now_ms(), self.device_id, rev, id],
        )?;
        Ok(n > 0)
    }

    pub fn get_book(&self, id: &str) -> anyhow::Result<Option<Book>> {
        let sql = format!("SELECT {BOOK_SELECT} FROM books WHERE id = ?1 AND deleted_at IS NULL");
        Ok(self.conn.query_row(&sql, [id], book_from_row).optional()?)
    }

    /// Live books, most recently changed first. `query` matches title, author and
    /// tags case-insensitively; an empty query matches everything.
    pub fn list_books(&self, status: Option<Status>, query: &str) -> anyhow::Result<Vec<Book>> {
        let pattern = format!("%{}%", query.trim().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let sql = format!(
            "SELECT {BOOK_SELECT} FROM books
             WHERE deleted_at IS NULL
               AND (?1 IS NULL OR status = ?1)
               AND (title LIKE ?2 ESCAPE '\\' OR IFNULL(author, '') LIKE ?2 ESCAPE '\\'
                    OR tags LIKE ?2 ESCAPE '\\')
             ORDER BY updated_at DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![status.map(Status::as_str), pattern], book_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

fn migrate(conn: &Connection) -> anyhow::Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let version = version as usize;
    anyhow::ensure!(
        version <= MIGRATIONS.len(),
        "books database schema v{version} is newer than this habitd (v{}); please update",
        MIGRATIONS.len()
    );
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql).with_context(|| format!("migration {}", i + 1))?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

pub(crate) fn get_setting(conn: &Connection, key: &str) -> anyhow::Result<Option<String>> {
    Ok(conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional()?)
}

fn set_setting(conn: &Connection, key: &str, value: &str) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn book_from_row(r: &Row) -> rusqlite::Result<Book> {
    let format: String = r.get(8)?;
    let status: String = r.get(9)?;
    let tags: String = r.get(11)?;
    Ok(Book {
        id: r.get(0)?,
        title: r.get(1)?,
        subtitle: r.get(2)?,
        author: r.get(3)?,
        isbn: r.get(4)?,
        olid: r.get(5)?,
        pages: r.get(6)?,
        pub_year: r.get(7)?,
        format: Format::parse(&format).unwrap_or(Format::Paperback),
        status: Status::parse(&status).unwrap_or(Status::ForLater),
        score: r.get(10)?,
        tags: tags.split(',').filter(|t| !t.is_empty()).map(str::to_owned).collect(),
        notes: r.get(12)?,
        cover_url: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(title: &str, author: &str, status: Status) -> Book {
        let mut b = Book::new(title);
        b.author = Some(author.into());
        b.status = status;
        b
    }

    #[test]
    fn save_and_reload_roundtrips_all_fields() {
        let db = Db::open_in_memory().unwrap();
        let mut b = book("Dune", "Frank Herbert", Status::Reading);
        b.subtitle = Some("Book One".into());
        b.pages = Some(612);
        b.pub_year = Some(1965);
        b.format = Format::Ebook;
        b.score = 7;
        b.tags = vec!["scifi".into(), "classic".into()];
        b.notes = Some("spice".into());
        db.save_book(&mut b).unwrap();
        assert!(b.created_at > 0);
        assert_eq!(db.get_book(&b.id).unwrap(), Some(b));
    }

    #[test]
    fn update_bumps_updated_at_and_keeps_created_at() {
        let db = Db::open_in_memory().unwrap();
        let mut b = book("Dune", "Frank Herbert", Status::ForLater);
        db.save_book(&mut b).unwrap();
        let (created, updated) = (b.created_at, b.updated_at);
        let b2 = db.set_status(&b.id, Status::Finished).unwrap().unwrap();
        assert_eq!(b2.created_at, created);
        assert!(b2.updated_at > updated);
        assert_eq!(db.get_book(&b.id).unwrap().unwrap().status, Status::Finished);
    }

    #[test]
    fn list_filters_by_status_and_query() {
        let db = Db::open_in_memory().unwrap();
        for (t, a, s) in [
            ("Dune", "Frank Herbert", Status::Reading),
            ("Emma", "Jane Austen", Status::Finished),
            ("100%_done", "Nobody", Status::Finished),
        ] {
            db.save_book(&mut book(t, a, s)).unwrap();
        }
        assert_eq!(db.list_books(None, "").unwrap().len(), 3);
        assert_eq!(db.list_books(Some(Status::Finished), "").unwrap().len(), 2);
        let hits = db.list_books(None, "herb").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Dune");
        // LIKE wildcards in the query are matched literally.
        assert_eq!(db.list_books(None, "%_").unwrap().len(), 1);
    }

    #[test]
    fn delete_is_soft_and_resave_revives() {
        let db = Db::open_in_memory().unwrap();
        let mut b = book("Dune", "Frank Herbert", Status::Reading);
        db.save_book(&mut b).unwrap();
        assert!(db.delete_book(&b.id).unwrap());
        assert!(!db.delete_book(&b.id).unwrap());
        assert!(db.get_book(&b.id).unwrap().is_none());
        assert!(db.list_books(None, "").unwrap().is_empty());
        let tombstones: i64 =
            db.conn.query_row("SELECT COUNT(*) FROM books WHERE deleted_at IS NOT NULL", [], |r| r.get(0)).unwrap();
        assert_eq!(tombstones, 1);
    }

    #[test]
    fn local_writes_advance_rev() {
        let db = Db::open_in_memory().unwrap();
        let mut b = book("Dune", "Frank Herbert", Status::ForLater);
        db.save_book(&mut b).unwrap();
        let rev = |db: &Db| -> i64 { db.conn.query_row("SELECT rev FROM books", [], |r| r.get(0)).unwrap() };
        let first = rev(&db);
        assert!(first > 0);
        db.delete_book(&b.id).unwrap();
        assert!(rev(&db) > first);
    }

    #[test]
    fn reopening_keeps_data() {
        let dir = std::env::temp_dir().join(format!("habit-books-test-{}", uuid::Uuid::now_v7()));
        let path = dir.join("books.db");
        {
            let db = Db::open(&path, "dev-1", "desk").unwrap();
            db.save_book(&mut Book::new("Dune")).unwrap();
        }
        let db = Db::open(&path, "dev-1", "desk").unwrap();
        assert_eq!(db.device_id(), "dev-1");
        assert_eq!(db.list_books(None, "").unwrap().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
