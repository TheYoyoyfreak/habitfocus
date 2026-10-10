//! Reading sessions and readings (passes through a book).

use rusqlite::{params, OptionalExtension, Row};

use crate::db::{now_ms, Db};
use crate::model::{Reading, Session, Source, Status};

const SESSION_COLUMNS: &str =
    "id, book_id, started_at, ended_at, duration_ms, page_from, page_to, source, device, external_ref";

impl Db {
    /// Opens or closes a [`Reading`] when a book changes status.
    pub(crate) fn track_status_change(&self, book_id: &str, status: Status) -> anyhow::Result<()> {
        let now = now_ms();
        let open: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM readings WHERE book_id = ?1 AND finished_at IS NULL AND deleted_at IS NULL
                 ORDER BY started_at DESC LIMIT 1",
                [book_id],
                |r| r.get(0),
            )
            .optional()?;
        match (status, open) {
            (Status::Reading, None) => self.insert_reading(book_id, now, None)?,
            (Status::Finished | Status::Abandoned, Some(id)) => {
                let rev = self.next_rev()?;
                self.conn.execute(
                    "UPDATE readings SET finished_at = ?1, outcome = ?2, updated_at = MAX(?1, updated_at + 1),
                        device_id = ?3, rev = ?4 WHERE id = ?5",
                    params![now, status.as_str(), self.device_id, rev, id],
                )?;
            }
            // Marked finished without ever being "reading": still counts as read.
            (Status::Finished, None) => self.insert_reading(book_id, now, Some(Status::Finished))?,
            _ => {}
        }
        Ok(())
    }

    fn insert_reading(&self, book_id: &str, at: i64, outcome: Option<Status>) -> anyhow::Result<()> {
        let finished_at = outcome.map(|_| at);
        let rev = self.next_rev()?;
        self.conn.execute(
            "INSERT INTO readings (id, book_id, started_at, finished_at, outcome, created_at, updated_at, device_id, rev)
             VALUES (?1, ?2, ?3, ?4, ?5, ?3, ?3, ?6, ?7)",
            params![
                uuid::Uuid::now_v7().to_string(),
                book_id,
                at,
                finished_at,
                outcome.map(Status::as_str),
                self.device_id,
                rev
            ],
        )?;
        Ok(())
    }

    /// Sets when the book's latest pass was started and finished, opening one
    /// if it has none (e.g. a book read before it was added). The finish only
    /// counts for a finished or abandoned book; one being read stays open.
    pub fn set_dates(&self, book_id: &str, started_at: i64, finished_at: Option<i64>) -> anyhow::Result<Reading> {
        let Some(book) = self.get_book(book_id)? else { anyhow::bail!("no book {book_id}") };
        let outcome = match book.status {
            Status::ForLater => anyhow::bail!("a book for later hasn't been read: change its status first"),
            Status::Reading => None,
            Status::Abandoned => finished_at.map(|_| Status::Abandoned),
            Status::Finished => finished_at.map(|_| Status::Finished),
        };
        let finished_at = finished_at.filter(|_| outcome.is_some());
        if let Some(finished) = finished_at {
            anyhow::ensure!(finished >= started_at, "a book can't be finished before it was started");
        }
        let latest: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM readings WHERE book_id = ?1 AND deleted_at IS NULL ORDER BY started_at DESC LIMIT 1",
                [book_id],
                |r| r.get(0),
            )
            .optional()?;
        let id = latest.unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        let now = now_ms();
        let rev = self.next_rev()?;
        self.conn.execute(
            "INSERT INTO readings (id, book_id, started_at, finished_at, outcome, created_at, updated_at, device_id, rev)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET started_at = excluded.started_at, finished_at = excluded.finished_at,
                outcome = excluded.outcome, updated_at = MAX(excluded.updated_at, readings.updated_at + 1),
                device_id = excluded.device_id, rev = excluded.rev",
            params![id, book_id, started_at, finished_at, outcome.map(Status::as_str), now, self.device_id, rev],
        )?;
        Ok(Reading { id, book_id: book_id.to_owned(), started_at, finished_at, outcome })
    }

    pub fn list_readings(&self) -> anyhow::Result<Vec<Reading>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.id, r.book_id, r.started_at, r.finished_at, r.outcome FROM readings r
             JOIN books b ON b.id = r.book_id AND b.deleted_at IS NULL
             WHERE r.deleted_at IS NULL ORDER BY r.started_at",
        )?;
        let rows = stmt.query_map([], |r| {
            let outcome: Option<String> = r.get(4)?;
            Ok(Reading {
                id: r.get(0)?,
                book_id: r.get(1)?,
                started_at: r.get(2)?,
                finished_at: r.get(3)?,
                outcome: outcome.as_deref().and_then(Status::parse),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// An unsaved manual session on this device, starting from the book's current page.
    pub fn new_session(&self, book_id: &str, started_at: i64, ended_at: i64) -> anyhow::Result<Session> {
        Ok(Session {
            id: uuid::Uuid::now_v7().to_string(),
            book_id: book_id.to_owned(),
            started_at,
            ended_at,
            duration_ms: (ended_at - started_at).max(0),
            page_from: Some(self.current_page(book_id)?.unwrap_or(0)),
            page_to: None,
            source: Source::Manual,
            device: self.device_name.clone(),
            external_ref: None,
        })
    }

    /// Saves a new session; false if its `external_ref` is known (even deleted,
    /// waiting or discarded) or its id exists.
    pub fn add_session(&self, s: &Session) -> anyhow::Result<bool> {
        if let Some(r) = &s.external_ref {
            if self.session_ref_exists(r)? {
                return Ok(false);
            }
        }
        let now = now_ms();
        let rev = self.next_rev()?;
        let n = self.conn.execute(
            &format!(
                "INSERT INTO reading_sessions ({SESSION_COLUMNS}, created_at, updated_at, device_id, rev)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?12, ?13)
                 ON CONFLICT DO NOTHING"
            ),
            params![
                s.id,
                s.book_id,
                s.started_at,
                s.ended_at,
                s.duration_ms,
                s.page_from,
                s.page_to,
                s.source.as_str(),
                s.device,
                s.external_ref,
                now,
                self.device_id,
                rev
            ],
        )?;
        Ok(n > 0)
    }

    /// Whether a session with this `external_ref` was ever saved, deleted,
    /// queued for assignment or discarded.
    pub fn session_ref_exists(&self, external_ref: &str) -> anyhow::Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM reading_sessions WHERE external_ref = ?1
                 UNION ALL SELECT 1 FROM pending_sessions WHERE external_ref = ?1",
                [external_ref],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Sets the page reached on the book's latest session (e.g. one from a
    /// reading habit, which knows nothing about pages). Returns that session.
    pub fn set_last_page(&self, book_id: &str, page: u32) -> anyhow::Result<Option<Session>> {
        let Some(mut last) = self.list_sessions(Some(book_id))?.into_iter().max_by_key(|s| s.ended_at) else {
            return Ok(None);
        };
        let rev = self.next_rev()?;
        self.conn.execute(
            "UPDATE reading_sessions SET page_to = ?1, updated_at = MAX(?2, updated_at + 1), device_id = ?3, rev = ?4
             WHERE id = ?5",
            params![page, now_ms(), self.device_id, rev, last.id],
        )?;
        last.page_to = Some(page);
        Ok(Some(last))
    }

    pub fn delete_session(&self, id: &str) -> anyhow::Result<bool> {
        let rev = self.next_rev()?;
        let n = self.conn.execute(
            "UPDATE reading_sessions SET deleted_at = ?1, updated_at = MAX(?1, updated_at + 1), device_id = ?2, rev = ?3
             WHERE id = ?4 AND deleted_at IS NULL",
            params![now_ms(), self.device_id, rev, id],
        )?;
        Ok(n > 0)
    }

    /// Live sessions of live books (optionally one book), oldest first.
    pub fn list_sessions(&self, book_id: Option<&str>) -> anyhow::Result<Vec<Session>> {
        let cols = SESSION_COLUMNS.split(", ").map(|c| format!("s.{c}")).collect::<Vec<_>>().join(", ");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {cols} FROM reading_sessions s
             JOIN books b ON b.id = s.book_id AND b.deleted_at IS NULL
             WHERE s.deleted_at IS NULL AND (?1 IS NULL OR s.book_id = ?1)
             ORDER BY s.started_at"
        ))?;
        let rows = stmt.query_map([book_id], session_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Page reached in the most recent session that recorded one.
    pub fn current_page(&self, book_id: &str) -> anyhow::Result<Option<u32>> {
        Ok(self
            .conn
            .query_row(
                "SELECT page_to FROM reading_sessions
                 WHERE book_id = ?1 AND page_to IS NOT NULL AND deleted_at IS NULL
                 ORDER BY ended_at DESC LIMIT 1",
                [book_id],
                |r| r.get(0),
            )
            .optional()?)
    }
}

fn session_from_row(r: &Row) -> rusqlite::Result<Session> {
    let source: String = r.get(7)?;
    Ok(Session {
        id: r.get(0)?,
        book_id: r.get(1)?,
        started_at: r.get(2)?,
        ended_at: r.get(3)?,
        duration_ms: r.get(4)?,
        page_from: r.get(5)?,
        page_to: r.get(6)?,
        source: Source::parse(&source).unwrap_or(Source::Manual),
        device: r.get(8)?,
        external_ref: r.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Book;

    fn db_with_book(status: Status) -> (Db, String) {
        let db = Db::open_in_memory().unwrap();
        let mut b = Book::new("Dune");
        b.status = status;
        b.pages = Some(600);
        db.save_book(&mut b).unwrap();
        (db, b.id)
    }

    #[test]
    fn sessions_continue_from_the_page_reached() {
        let (db, id) = db_with_book(Status::Reading);
        let mut s = db.new_session(&id, 0, 60_000).unwrap();
        s.page_to = Some(40);
        db.add_session(&s).unwrap();
        assert_eq!((s.page_from, s.pages_read()), (Some(0), Some(40)));
        assert_eq!(db.current_page(&id).unwrap(), Some(40));
        let mut s = db.new_session(&id, 120_000, 180_000).unwrap();
        s.page_to = Some(90);
        db.add_session(&s).unwrap();
        assert_eq!(s.pages_read(), Some(50));
        assert_eq!(db.list_sessions(Some(&id)).unwrap().len(), 2);
    }

    #[test]
    fn external_ref_deduplicates_even_after_delete() {
        let (db, id) = db_with_book(Status::Reading);
        let mut s = db.new_session(&id, 1_000, 61_000).unwrap();
        s.external_ref = Some("habitfocus:read:1000".into());
        assert!(db.add_session(&s).unwrap());
        s.id = uuid::Uuid::now_v7().to_string();
        assert!(!db.add_session(&s).unwrap());
        let first = &db.list_sessions(None).unwrap()[0];
        assert!(db.delete_session(&first.id).unwrap());
        assert!(!db.add_session(&s).unwrap());
        assert!(db.list_sessions(None).unwrap().is_empty());
    }

    #[test]
    fn readings_follow_status_changes() {
        let (db, id) = db_with_book(Status::ForLater);
        assert!(db.list_readings().unwrap().is_empty());
        db.set_status(&id, Status::Reading).unwrap();
        db.set_status(&id, Status::Reading).unwrap();
        let r = db.list_readings().unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].finished_at, None);
        db.set_status(&id, Status::Finished).unwrap();
        let r = db.list_readings().unwrap();
        assert_eq!((r.len(), r[0].outcome), (1, Some(Status::Finished)));
        assert!(r[0].finished_at.is_some());
        // A re-read opens a second pass.
        db.set_status(&id, Status::Reading).unwrap();
        assert_eq!(db.list_readings().unwrap().len(), 2);
    }

    #[test]
    fn dates_go_on_the_latest_pass_or_open_one() {
        // Read before it was added: the pass is created.
        let (db, id) = db_with_book(Status::ForLater);
        assert!(db.set_dates(&id, 1_000, Some(5_000)).is_err(), "not read yet");
        db.set_status(&id, Status::Finished).unwrap();
        db.conn.execute("DELETE FROM readings", []).unwrap();
        let r = db.set_dates(&id, 1_000, Some(5_000)).unwrap();
        assert_eq!((r.started_at, r.finished_at, r.outcome), (1_000, Some(5_000), Some(Status::Finished)));
        assert_eq!(db.list_readings().unwrap(), std::slice::from_ref(&r));
        // Corrected later: the same pass, sent again.
        let rev: i64 = db.conn.query_row("SELECT rev FROM readings", [], |r| r.get(0)).unwrap();
        db.set_dates(&id, 2_000, Some(6_000)).unwrap();
        let again = db.list_readings().unwrap();
        assert_eq!((again.len(), again[0].id.as_str(), again[0].started_at), (1, r.id.as_str(), 2_000));
        assert!(db.conn.query_row("SELECT rev FROM readings", [], |r| r.get::<_, i64>(0)).unwrap() > rev);
        assert!(db.set_dates(&id, 7_000, Some(6_000)).is_err());
        assert!(db.set_dates("nope", 0, None).is_err());

        // A book being read keeps its pass open; an abandoned one ends abandoned.
        let (db, id) = db_with_book(Status::Reading);
        assert_eq!(db.set_dates(&id, 1_000, Some(5_000)).unwrap().finished_at, None);
        db.set_status(&id, Status::Abandoned).unwrap();
        assert_eq!(db.set_dates(&id, 1_000, Some(5_000)).unwrap().outcome, Some(Status::Abandoned));
        assert_eq!(db.list_readings().unwrap().len(), 1);
    }

    #[test]
    fn set_last_page_fills_in_the_latest_session() {
        let (db, id) = db_with_book(Status::Reading);
        assert!(db.set_last_page(&id, 10).unwrap().is_none());
        let mut first = db.new_session(&id, 0, 1000).unwrap();
        first.page_to = Some(20);
        db.add_session(&first).unwrap();
        // e.g. a reading habit session: starts at page 20, end page unknown.
        db.add_session(&db.new_session(&id, 2000, 3000).unwrap()).unwrap();
        let s = db.set_last_page(&id, 45).unwrap().unwrap();
        assert_eq!((s.page_from, s.page_to, s.pages_read()), (Some(20), Some(45), Some(25)));
        assert_eq!(db.current_page(&id).unwrap(), Some(45));
    }

    #[test]
    fn sessions_of_deleted_books_are_hidden() {
        let (db, id) = db_with_book(Status::Reading);
        db.add_session(&db.new_session(&id, 0, 1000).unwrap()).unwrap();
        db.delete_book(&id).unwrap();
        assert!(db.list_sessions(None).unwrap().is_empty());
    }
}
