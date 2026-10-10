//! The book library (`[books]`) on a thread of its own, with its own
//! connection to `books.db`: Open Library searches and cover downloads take
//! seconds and must not hold up blocking. The event loop hands it the `books`
//! requests and the finished sessions of reading habits; how the library looks
//! reaches snapshots as `Event::BooksChanged`.
//!
//! `books.db` is only created once the library is used.

use habit_books::{Db, Recorded};
use habit_core::snapshot::BooksView;
use habit_ipc::books::BooksOp;
use habit_ipc::Response;
use std::path::PathBuf;
use std::sync::mpsc;
use tokio::sync::oneshot;

pub enum Job {
    Request(BooksOp, oneshot::Sender<Response>),
    /// A reading habit's session ended on this device.
    HabitSession { habit: String, started_ms: i64, finished_ms: i64, focused_ms: i64 },
    /// Something else (sync) changed `books.db`.
    Changed,
}

/// This device as habitfocus knows it (`history.db`), so books and habit
/// sessions agree on it.
pub struct Device {
    pub id: String,
    pub name: String,
}

struct Library {
    path: PathBuf,
    device: Device,
    db: Option<Db>,
    seq: u64,
    events: tokio::sync::mpsc::Sender<crate::Event>,
}

/// Starts the books thread.
pub fn spawn(path: PathBuf, device: Device, events: tokio::sync::mpsc::Sender<crate::Event>) -> mpsc::Sender<Job> {
    let (tx, rx) = mpsc::channel::<Job>();
    std::thread::spawn(move || {
        let mut library = Library { path, device, db: None, seq: 0, events };
        // Only an existing library is opened up front (for the inbox count).
        if library.path.is_file() {
            library.publish();
        }
        for job in rx {
            match job {
                Job::Request(op, reply) => {
                    let write = op.is_write();
                    let response = match library.db().and_then(|db| habit_books::api::handle(db, op)) {
                        Ok(reply) => Response { ok: true, books: Some(reply), ..Default::default() },
                        Err(e) => Response::err(format!("{e:#}")),
                    };
                    let _ = reply.send(response);
                    if write {
                        library.publish();
                    }
                }
                Job::Changed => library.publish(),
                Job::HabitSession { habit, started_ms, finished_ms, focused_ms } => {
                    match library.db().and_then(|db| db.record_habit_session(&habit, started_ms, finished_ms, focused_ms)) {
                        Ok(Recorded::Duplicate) => {}
                        Ok(_) => library.publish(),
                        Err(e) => eprintln!("habitd: couldn't record the reading session: {e:#}"),
                    }
                }
            }
        }
    });
    tx
}

impl Library {
    fn db(&mut self) -> anyhow::Result<&Db> {
        if self.db.is_none() {
            self.db = Some(Db::open(&self.path, &self.device.id, &self.device.name)?);
        }
        Ok(self.db.as_ref().expect("just opened"))
    }

    fn publish(&mut self) {
        self.seq += 1;
        let view = match self.view() {
            Ok(view) => view,
            Err(e) => {
                eprintln!("habitd: can't read {}: {e:#}", self.path.display());
                BooksView { seq: self.seq, ..Default::default() }
            }
        };
        let _ = self.events.blocking_send(crate::Event::BooksChanged(view));
    }

    fn view(&mut self) -> anyhow::Result<BooksView> {
        let seq = self.seq;
        let db = self.db()?;
        let pin = match db.pin()? {
            Some(pin) => db.get_book(&pin.book_id)?.map(|b| b.title),
            None => None,
        };
        Ok(BooksView { seq, pending: db.list_pending()?.len() as u32, pin, ..Default::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use habit_books::{Book, Status};

    fn library() -> (Library, tokio::sync::mpsc::Receiver<crate::Event>) {
        let dir = std::env::temp_dir().join(format!("habitd-books-{}", uuid::Uuid::now_v7()));
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let device = Device { id: "dev".into(), name: "desk".into() };
        (Library { path: dir.join("books.db"), device, db: None, seq: 0, events: tx }, rx)
    }

    #[test]
    fn publish_reports_inbox_and_pin() {
        let (mut lib, mut rx) = library();
        let mut a = Book::new("Dune");
        a.status = Status::Reading;
        let mut b = Book::new("Emma");
        b.status = Status::Reading;
        let db = lib.db().unwrap();
        db.save_book(&mut a).unwrap();
        db.save_book(&mut b).unwrap();
        db.record_habit_session("read", 0, 1_000, 1_000).unwrap();
        db.set_pin(Some(&a.id)).unwrap();
        lib.publish();
        let Some(crate::Event::BooksChanged(view)) = rx.try_recv().ok() else { panic!("no view") };
        assert_eq!(view, BooksView { seq: 1, pending: 1, pin: Some("Dune".into()), ..Default::default() });
        std::fs::remove_dir_all(lib.path.parent().unwrap()).unwrap();
    }
}
