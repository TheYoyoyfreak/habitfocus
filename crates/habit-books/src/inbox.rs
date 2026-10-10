//! What happens when a reading-habit session ends: it goes to the pinned book,
//! else to the only book being read, else it waits here until it is assigned.

use rusqlite::{params, OptionalExtension};

use crate::db::{now_ms, Db};
use crate::model::{habit_session_ref, session_id_for_ref, Book, Pending, Pin, Session, Source, Status};

const PIN_KEY: &str = "pin";

/// Where a finished habit session went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recorded {
    /// Saved as a session of this book.
    Assigned { book_id: String, session: Session },
    /// Several books are being read: it waits for a choice.
    Pending(Pending),
    /// Known already (recorded, deleted or discarded before).
    Duplicate,
}

impl Db {
    /// Records a finished session of a reading habit on this device.
    pub fn record_habit_session(
        &self,
        habit: &str,
        started_ms: i64,
        finished_ms: i64,
        focused_ms: i64,
    ) -> anyhow::Result<Recorded> {
        let external_ref = habit_session_ref(habit, started_ms);
        if self.session_ref_exists(&external_ref)? {
            return Ok(Recorded::Duplicate);
        }
        let target = match self.pin()?.filter(|p| p.applies_to(finished_ms)) {
            Some(pin) => {
                self.set_pin(None)?;
                Some(pin.book_id)
            }
            None => match self.reading_books()?.as_slice() {
                [only] => Some(only.id.clone()),
                _ => None,
            },
        };
        match target {
            Some(book_id) => {
                let mut session = self.new_session(&book_id, started_ms, finished_ms)?;
                session.id = session_id_for_ref(&external_ref);
                session.duration_ms = focused_ms;
                session.source = Source::Habitfocus;
                session.external_ref = Some(external_ref);
                self.add_session(&session)?;
                Ok(Recorded::Assigned { book_id, session })
            }
            None => {
                let pending = Pending {
                    id: uuid::Uuid::now_v7().to_string(),
                    started_at: started_ms,
                    ended_at: finished_ms,
                    duration_ms: focused_ms,
                    source: Source::Habitfocus,
                    device: self.device_name.clone(),
                    external_ref,
                };
                self.add_pending(&pending)?;
                Ok(Recorded::Pending(pending))
            }
        }
    }

    /// Queues a session for assignment; false if its `external_ref` is known.
    pub fn add_pending(&self, p: &Pending) -> anyhow::Result<bool> {
        if self.session_ref_exists(&p.external_ref)? {
            return Ok(false);
        }
        let n = self.conn.execute(
            "INSERT INTO pending_sessions (id, started_at, ended_at, duration_ms, source, device, external_ref, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(external_ref) DO NOTHING",
            params![p.id, p.started_at, p.ended_at, p.duration_ms, p.source.as_str(), p.device, p.external_ref, now_ms()],
        )?;
        Ok(n > 0)
    }

    /// Waiting sessions, oldest first.
    pub fn list_pending(&self) -> anyhow::Result<Vec<Pending>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, started_at, ended_at, duration_ms, source, device, external_ref FROM pending_sessions
             WHERE discarded_at IS NULL ORDER BY started_at",
        )?;
        let rows = stmt.query_map([], |r| {
            let source: String = r.get(4)?;
            Ok(Pending {
                id: r.get(0)?,
                started_at: r.get(1)?,
                ended_at: r.get(2)?,
                duration_ms: r.get(3)?,
                source: Source::parse(&source).unwrap_or(Source::Habitfocus),
                device: r.get(5)?,
                external_ref: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Turns a waiting session into a reading session of `book_id`.
    pub fn assign_pending(&self, id: &str, book_id: &str, page_to: Option<u32>) -> anyhow::Result<Session> {
        let p = self.list_pending()?.into_iter().find(|p| p.id == id).ok_or_else(|| anyhow::anyhow!("no such session"))?;
        anyhow::ensure!(self.get_book(book_id)?.is_some(), "no such book");
        let mut s = self.new_session(book_id, p.started_at, p.ended_at)?;
        s.id = session_id_for_ref(&p.external_ref);
        s.duration_ms = p.duration_ms;
        s.source = p.source;
        s.device = p.device;
        s.page_to = page_to;
        // The ref moves to the real session, which then guards against re-imports.
        let tx = self.conn.unchecked_transaction()?;
        self.conn.execute("DELETE FROM pending_sessions WHERE id = ?1", [id])?;
        s.external_ref = Some(p.external_ref);
        self.add_session(&s)?;
        tx.commit()?;
        Ok(s)
    }

    /// Drops the waiting session with this ref, e.g. because another device
    /// recorded it with a book.
    pub fn resolve_pending_ref(&self, external_ref: &str) -> anyhow::Result<bool> {
        Ok(self.conn.execute("DELETE FROM pending_sessions WHERE external_ref = ?1", [external_ref])? > 0)
    }

    /// Drops a waiting session ("I didn't actually read"); it is not imported again.
    pub fn discard_pending(&self, id: &str) -> anyhow::Result<bool> {
        let n = self.conn.execute(
            "UPDATE pending_sessions SET discarded_at = ?1 WHERE id = ?2 AND discarded_at IS NULL",
            params![now_ms(), id],
        )?;
        Ok(n > 0)
    }

    pub fn pin(&self) -> anyhow::Result<Option<Pin>> {
        let pin: Option<Pin> = self.setting(PIN_KEY)?.and_then(|j| serde_json::from_str(&j).ok());
        Ok(pin.filter(|p| self.get_book(&p.book_id).ok().flatten().is_some()))
    }

    /// Pins `book_id` for the session in progress (or the next one), or unpins.
    pub fn set_pin(&self, book_id: Option<&str>) -> anyhow::Result<()> {
        match book_id {
            Some(id) => {
                let pin = Pin { book_id: id.to_owned(), set_at: now_ms() };
                self.set_setting(PIN_KEY, Some(&serde_json::to_string(&pin)?))
            }
            None => self.set_setting(PIN_KEY, None),
        }
    }

    pub fn reading_books(&self) -> anyhow::Result<Vec<Book>> {
        self.list_books(Some(Status::Reading), "")
    }

    /// Best guess for a session without a book: the pinned book, else the
    /// reading book read most recently.
    pub fn suggested_book(&self) -> anyhow::Result<Option<String>> {
        if let Some(pin) = self.pin()? {
            return Ok(Some(pin.book_id));
        }
        let reading = self.reading_books()?;
        let last: Option<String> = self
            .conn
            .query_row(
                "SELECT s.book_id FROM reading_sessions s JOIN books b ON b.id = s.book_id
                 WHERE s.deleted_at IS NULL AND b.deleted_at IS NULL AND b.status = 'reading'
                 ORDER BY s.ended_at DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(last.or_else(|| reading.first().map(|b| b.id.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Session;

    fn pending(r: &str) -> Pending {
        Pending {
            id: uuid::Uuid::now_v7().to_string(),
            started_at: 1_000,
            ended_at: 61_000,
            duration_ms: 60_000,
            source: Source::Habitfocus,
            device: "desk".into(),
            external_ref: r.into(),
        }
    }

    fn book(db: &Db, title: &str, status: Status) -> String {
        let mut b = Book::new(title);
        b.status = status;
        db.save_book(&mut b).unwrap();
        b.id
    }

    #[test]
    fn assign_moves_session_and_keeps_ref() {
        let db = Db::open_in_memory().unwrap();
        let dune = book(&db, "Dune", Status::Reading);
        let p = pending("hf:1");
        assert!(db.add_pending(&p).unwrap());
        assert!(!db.add_pending(&pending("hf:1")).unwrap());
        let s = db.assign_pending(&p.id, &dune, Some(30)).unwrap();
        assert_eq!(s.id, session_id_for_ref("hf:1"));
        assert_eq!((s.duration_ms, s.source, s.page_to), (60_000, Source::Habitfocus, Some(30)));
        assert!(db.list_pending().unwrap().is_empty());
        assert_eq!(db.list_sessions(Some(&dune)).unwrap().len(), 1);
        // Already a real session now.
        assert!(!db.add_pending(&pending("hf:1")).unwrap());
    }

    #[test]
    fn discarded_sessions_stay_away() {
        let db = Db::open_in_memory().unwrap();
        let p = pending("hf:2");
        db.add_pending(&p).unwrap();
        assert!(db.discard_pending(&p.id).unwrap());
        assert!(db.list_pending().unwrap().is_empty());
        assert!(!db.add_pending(&pending("hf:2")).unwrap());
    }

    #[test]
    fn suggestion_prefers_pin_then_last_read() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.suggested_book().unwrap(), None);
        let dune = book(&db, "Dune", Status::Reading);
        let emma = book(&db, "Emma", Status::Reading);
        db.add_session(&db.new_session(&emma, 0, 1000).unwrap()).unwrap();
        assert_eq!(db.suggested_book().unwrap(), Some(emma.clone()));
        db.set_pin(Some(&dune)).unwrap();
        assert_eq!(db.suggested_book().unwrap(), Some(dune));
    }

    fn session_of(r: &Recorded) -> &Session {
        match r {
            Recorded::Assigned { session, .. } => session,
            other => panic!("not assigned: {other:?}"),
        }
    }

    #[test]
    fn habit_session_goes_to_the_only_reading_book() {
        let db = Db::open_in_memory().unwrap();
        let dune = book(&db, "Dune", Status::Reading);
        book(&db, "Emma", Status::ForLater);
        let now = now_ms();
        let r = db.record_habit_session("read", now - 60_000, now, 50_000).unwrap();
        let s = session_of(&r);
        assert_eq!((s.book_id.as_str(), s.duration_ms, s.source), (dune.as_str(), 50_000, Source::Habitfocus));
        assert_eq!(s.id, session_id_for_ref(&habit_session_ref("read", now - 60_000)));
        assert_eq!(s.device, "test");
        assert_eq!(db.record_habit_session("read", now - 60_000, now, 50_000).unwrap(), Recorded::Duplicate);
    }

    #[test]
    fn pin_wins_once_then_several_books_wait() {
        let db = Db::open_in_memory().unwrap();
        book(&db, "Dune", Status::Reading);
        let emma = book(&db, "Emma", Status::Reading);
        db.set_pin(Some(&emma)).unwrap();
        let now = now_ms();
        let r = db.record_habit_session("read", now - 1_000, now + 1, 1_000).unwrap();
        assert_eq!(session_of(&r).book_id, emma);
        assert!(db.pin().unwrap().is_none(), "a pin is used once");
        let r = db.record_habit_session("read", now + 10, now + 20, 10).unwrap();
        let Recorded::Pending(p) = r else { panic!("{r:?}") };
        assert_eq!(db.list_pending().unwrap(), vec![p.clone()]);
        // Another device recorded it with a book: the waiting entry goes away.
        assert!(db.resolve_pending_ref(&p.external_ref).unwrap());
        assert!(db.list_pending().unwrap().is_empty());
    }
}
