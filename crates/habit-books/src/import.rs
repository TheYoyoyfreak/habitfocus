//! One-time move from the standalone booktrack app (`booktrack.db`).

use std::path::{Path, PathBuf};

use anyhow::Context;

pub use crate::api::ImportCounts;
use crate::db::Db;

/// booktrack's default database: `$XDG_DATA_HOME/booktrack/booktrack.db`.
pub fn booktrack_path() -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into())).join(".local/share")
    });
    data.join("booktrack/booktrack.db")
}

const BOOK_COLUMNS: &str = "id, title, subtitle, author, isbn, olid, pages, pub_year, format, status, rating, tags, \
                            notes, cover_url, created_at, updated_at, deleted_at, device_id";
const READING_COLUMNS: &str = "id, book_id, started_at, finished_at, outcome, created_at, updated_at, deleted_at, device_id";
const SESSION_COLUMNS: &str = "id, book_id, started_at, ended_at, duration_ms, page_from, page_to, source, device, \
                               external_ref, created_at, updated_at, deleted_at, device_id";
const PENDING_COLUMNS: &str =
    "id, started_at, ended_at, duration_ms, source, device, external_ref, created_at, discarded_at";

impl Db {
    /// Copies books, readings, sessions and the inbox from a booktrack database,
    /// keeping ids and timestamps (so running it again changes nothing). The
    /// source is opened read-only. Imported rows are synced like local edits.
    pub fn import_booktrack(&self, path: &Path) -> anyhow::Result<ImportCounts> {
        anyhow::ensure!(path.is_file(), "no booktrack database at {}", path.display());
        let uri = format!("file:{}?mode=ro", path.display());
        self.conn.execute("ATTACH DATABASE ?1 AS bt", [&uri]).with_context(|| format!("opening {}", path.display()))?;
        let result = self.copy_booktrack();
        self.conn.execute_batch("DETACH DATABASE bt")?;
        result
    }

    fn copy_booktrack(&self) -> anyhow::Result<ImportCounts> {
        let has_pending: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM bt.sqlite_master WHERE type = 'table' AND name = 'pending_sessions')",
            [],
            |r| r.get(0),
        )?;
        let rev = self.next_rev()?;
        let tx = self.conn.unchecked_transaction()?;
        let copy = |table: &str, columns: &str| -> anyhow::Result<usize> {
            Ok(tx.execute(
                &format!("INSERT OR IGNORE INTO {table} ({columns}, rev) SELECT {columns}, ?1 FROM bt.{table}"),
                [rev],
            )?)
        };
        let mut counts = ImportCounts {
            books: copy("books", BOOK_COLUMNS)?,
            readings: copy("readings", READING_COLUMNS)?,
            sessions: copy("reading_sessions", SESSION_COLUMNS)?,
            pending: 0,
        };
        if has_pending {
            counts.pending = tx.execute(
                &format!(
                    "INSERT OR IGNORE INTO pending_sessions ({PENDING_COLUMNS}) SELECT {PENDING_COLUMNS}
                     FROM bt.pending_sessions p
                     WHERE NOT EXISTS (SELECT 1 FROM reading_sessions s WHERE s.external_ref = p.external_ref)"
                ),
                [],
            )?;
        }
        tx.commit()?;
        Ok(counts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Status;
    use rusqlite::Connection;

    /// A booktrack database as booktrack 0.1 left it (its four migrations).
    fn booktrack_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE books (id TEXT PRIMARY KEY, title TEXT NOT NULL, subtitle TEXT, author TEXT, isbn TEXT,
                 olid TEXT, pages INTEGER, pub_year INTEGER, format TEXT NOT NULL, status TEXT NOT NULL,
                 rating INTEGER NOT NULL DEFAULT 0, tags TEXT NOT NULL DEFAULT '', notes TEXT, cover_url TEXT,
                 created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, deleted_at INTEGER, device_id TEXT NOT NULL);
             CREATE TABLE readings (id TEXT PRIMARY KEY, book_id TEXT NOT NULL, started_at INTEGER NOT NULL,
                 finished_at INTEGER, outcome TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
                 deleted_at INTEGER, device_id TEXT NOT NULL);
             CREATE TABLE reading_sessions (id TEXT PRIMARY KEY, book_id TEXT NOT NULL, started_at INTEGER NOT NULL,
                 ended_at INTEGER NOT NULL, duration_ms INTEGER NOT NULL, page_from INTEGER, page_to INTEGER,
                 source TEXT NOT NULL, device TEXT NOT NULL, external_ref TEXT UNIQUE, created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL, deleted_at INTEGER, device_id TEXT NOT NULL);
             CREATE TABLE pending_sessions (id TEXT PRIMARY KEY, started_at INTEGER NOT NULL, ended_at INTEGER NOT NULL,
                 duration_ms INTEGER NOT NULL, source TEXT NOT NULL, device TEXT NOT NULL,
                 external_ref TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL, discarded_at INTEGER);
             INSERT INTO books VALUES ('b1', 'Dune', NULL, 'Frank Herbert', NULL, NULL, 612, 1965, 'paperback',
                 'reading', 0, 'scifi', NULL, NULL, 1, 2, NULL, 'old-dev');
             INSERT INTO books VALUES ('b2', 'Gone', NULL, NULL, NULL, NULL, NULL, NULL, 'paperback',
                 'for_later', 0, '', NULL, NULL, 1, 3, 3, 'old-dev');
             INSERT INTO readings VALUES ('r1', 'b1', 1, NULL, NULL, 1, 1, NULL, 'old-dev');
             INSERT INTO reading_sessions VALUES ('s1', 'b1', 10, 70, 60, 0, 40, 'habitfocus', 'desk',
                 'habitfocus:read:10', 70, 70, NULL, 'old-dev');
             INSERT INTO pending_sessions VALUES ('p1', 100, 200, 100, 'habitfocus', 'desk', 'habitfocus:read:100',
                 200, NULL);
             PRAGMA user_version = 4;",
        )
        .unwrap();
    }

    #[test]
    fn imports_everything_once() {
        let dir = std::env::temp_dir().join(format!("habit-books-import-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("booktrack.db");
        booktrack_db(&path);

        let db = Db::open_in_memory().unwrap();
        let counts = db.import_booktrack(&path).unwrap();
        assert_eq!(counts, ImportCounts { books: 2, readings: 1, sessions: 1, pending: 1 });
        assert_eq!(db.import_booktrack(&path).unwrap(), ImportCounts::default());

        let books = db.list_books(None, "").unwrap();
        assert_eq!(books.len(), 1, "the deleted book stays a tombstone");
        assert_eq!((books[0].id.as_str(), books[0].status, books[0].updated_at), ("b1", Status::Reading, 2));
        assert_eq!(db.list_sessions(None).unwrap()[0].page_to, Some(40));
        assert_eq!(db.list_pending().unwrap()[0].external_ref, "habitfocus:read:100");
        // The habit session booktrack had already imported doesn't come in twice.
        assert_eq!(db.record_habit_session("read", 10, 70, 60).unwrap(), crate::Recorded::Duplicate);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_database_is_an_error() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.import_booktrack(Path::new("/definitely/not/booktrack.db")).is_err());
    }
}
