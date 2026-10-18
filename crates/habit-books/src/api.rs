//! The `books` request of habitd's protocol: what clients can ask of the
//! library, and (with `store`) how it is answered. Clients get the rows and
//! compute stats themselves (`crate::stats`).

use crate::model::{Book, Pending, Pin, Reading, Session, Status};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BooksOp {
    /// Everything at once (books, sessions, readings, inbox, pin), for the TUI.
    Library,
    /// Live books, most recently changed first; `query` matches title, author
    /// and tags.
    List {
        #[serde(default)]
        status: Option<Status>,
        #[serde(default)]
        query: String,
    },
    /// Open Library and Google Books search: unsaved books to pick from, and
    /// Manga-Passion's German manga editions.
    Search { query: String },
    /// Every volume of a Manga-Passion edition, as unsaved books.
    Volumes { edition: u64 },
    /// Adds or updates a book (a new id adds it).
    Save { book: Box<Book> },
    Delete { id: String },
    SetStatus { id: String, status: Status },
    /// When the book's latest pass was started and finished (a finish only
    /// counts for finished and abandoned books; books for later have none).
    SetDates {
        book: String,
        started_at: i64,
        #[serde(default)]
        finished_at: Option<i64>,
    },
    /// Records a session by hand, from the book's current page to `page_to`.
    Log {
        book: String,
        started_at: i64,
        ended_at: i64,
        #[serde(default)]
        page_to: Option<u32>,
    },
    DeleteSession { id: String },
    /// The page reached in the book's latest session.
    SetPage { book: String, page: u32 },
    /// Books a waiting session onto a book.
    Assign {
        pending: String,
        book: String,
        #[serde(default)]
        page_to: Option<u32>,
    },
    /// Drops a waiting session for good.
    Discard { pending: String },
    /// The next reading session that ends goes to `book`; `None` unpins.
    Pin { book: Option<String> },
    /// Downloads a cover into the cache (`habit_books::covers::path`).
    FetchCover { url: String },
    /// Takes over a standalone booktrack database (default: booktrack's own).
    ImportBooktrack {
        #[serde(default)]
        path: Option<String>,
    },
    /// Sends the whole library again with the next sync, rows from other
    /// devices too (after moving to a new sync server).
    Resync,
}

impl BooksOp {
    /// Changes the library (clients refetch afterwards).
    pub fn is_write(&self) -> bool {
        !matches!(self, BooksOp::Library
                | BooksOp::List { .. }
                | BooksOp::Search { .. }
                | BooksOp::Volumes { .. }
                | BooksOp::FetchCover { .. })
    }
}

/// What a `books` request answers; which fields are filled depends on the op.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BooksReply {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub books: Vec<Book>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<Session>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readings: Vec<Reading>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<Pending>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<Pin>,
    /// Best guess for a waiting session's book.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested: Option<String>,
    /// The book saved or changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub book: Option<Book>,
    /// The session recorded or changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Session>,
    /// Search results (Open Library first, then Google Books).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hits: Vec<Book>,
    /// Manga editions a search found (Manga-Passion).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<crate::mangapassion::Series>,
    /// What `import_booktrack` took over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported: Option<ImportCounts>,
    /// How many rows `resync` queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requeued: Option<usize>,
}

/// Rows newly taken over by `import_booktrack` (rows already here are skipped).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportCounts {
    pub books: usize,
    pub readings: usize,
    pub sessions: usize,
    pub pending: usize,
}

/// Results per source of a search.
#[cfg(feature = "store")]
const SEARCH_LIMIT: usize = 20;


/// Answers `op` from `db`. Blocking: searches and cover downloads go out to
/// the network.
#[cfg(feature = "store")]
pub fn handle(db: &crate::Db, op: BooksOp) -> anyhow::Result<BooksReply> {
    use crate::import::booktrack_path;
    use crate::{covers, googlebooks, mangapassion, openlibrary, series};
    use std::path::PathBuf;

    let mut reply = BooksReply::default();
    match op {
        BooksOp::Library => {
            reply.books = db.list_books(None, "")?;
            reply.sessions = db.list_sessions(None)?;
            reply.readings = db.list_readings()?;
            reply.pending = db.list_pending()?;
            reply.pin = db.pin()?;
            reply.suggested = db.suggested_book()?;
        }
        BooksOp::List { status, query } => reply.books = db.list_books(status, &query)?,
        BooksOp::Search { query } => {
            // All at once; any one alone is enough.
            let (ol, gb, mp) = std::thread::scope(|s| {
                let gb = s.spawn(|| googlebooks::search(&query, SEARCH_LIMIT));
                let mp = s.spawn(|| mangapassion::search(&query));
                let ol = openlibrary::search(&query, SEARCH_LIMIT);
                let crashed = |name: &str| anyhow::anyhow!("{name} search crashed");
                (
                    ol,
                    gb.join().unwrap_or_else(|_| Err(crashed("Google Books"))),
                    mp.join().unwrap_or_else(|_| Err(crashed("Manga-Passion"))),
                )
            });
            reply.series = mp.as_ref().map(Vec::clone).unwrap_or_default();
            reply.hits = match (ol, gb) {
                (Err(e), Err(_)) if mp.is_err() => return Err(e),
                (ol, gb) => googlebooks::merge(
                    ol.map(|h| h.iter().map(openlibrary::Hit::to_book).collect()).unwrap_or_default(),
                    gb.map(|h| h.iter().map(googlebooks::Volume::to_book).collect()).unwrap_or_default(),
                ),
            };
        }
        BooksOp::Volumes { edition } => reply.hits = mangapassion::volumes(edition)?,
        BooksOp::Save { book } => {
            let mut book = *book;
            anyhow::ensure!(!book.title.trim().is_empty(), "a book needs a title");
            series::fill_in(&mut book);
            // A cover from a local file goes in the book, so other devices see it.
            if let Some(url) = book.cover_url.clone().filter(|u| covers::is_local(u)) {
                book.cover_url = Some(covers::embed(&covers::path(&url))?);
            }
            db.save_book(&mut book)?;
            reply.book = Some(book);
        }
        BooksOp::Delete { id } => anyhow::ensure!(db.delete_book(&id)?, "no such book"),
        BooksOp::SetStatus { id, status } => {
            reply.book = Some(db.set_status(&id, status)?.ok_or_else(|| anyhow::anyhow!("no such book"))?);
        }
        BooksOp::Log { book, started_at, ended_at, page_to } => {
            anyhow::ensure!(db.get_book(&book)?.is_some(), "no such book");
            anyhow::ensure!(ended_at >= started_at, "a session can't end before it starts");
            let mut session = db.new_session(&book, started_at, ended_at)?;
            session.page_to = page_to;
            db.add_session(&session)?;
            reply.session = Some(session);
        }
        BooksOp::SetDates { book, started_at, finished_at } => {
            reply.readings = vec![db.set_dates(&book, started_at, finished_at)?];
        }
        BooksOp::DeleteSession { id } => anyhow::ensure!(db.delete_session(&id)?, "no such session"),
        BooksOp::SetPage { book, page } => {
            reply.session =
                Some(db.set_last_page(&book, page)?.ok_or_else(|| anyhow::anyhow!("that book has no sessions yet"))?);
        }
        BooksOp::Assign { pending, book, page_to } => reply.session = Some(db.assign_pending(&pending, &book, page_to)?),
        BooksOp::Discard { pending } => anyhow::ensure!(db.discard_pending(&pending)?, "no such session"),
        BooksOp::Pin { book } => {
            if let Some(id) = &book {
                anyhow::ensure!(db.get_book(id)?.is_some(), "no such book");
            }
            db.set_pin(book.as_deref())?;
            reply.pin = db.pin()?;
        }
        BooksOp::FetchCover { url } => {
            covers::fetch(&url)?;
        }
        BooksOp::ImportBooktrack { path } => {
            let path = path.map_or_else(booktrack_path, PathBuf::from);
            let counts = db.import_booktrack(&path)?;
            reply.imported = Some(counts);
            reply.books = db.list_books(None, "")?;
        }
        BooksOp::Resync => reply.requeued = Some(db.requeue_all()?),
    }
    Ok(reply)
}

#[cfg(all(test, feature = "store"))]
mod tests {
    use super::*;
    use crate::Db;

    #[test]
    fn saves_books_and_logs_sessions() {
        let db = Db::open_in_memory().unwrap();
        let mut dune = Book::new("Dune");
        dune.status = Status::Reading;
        let saved = handle(&db, BooksOp::Save { book: Box::new(dune.clone()) }).unwrap().book.unwrap();
        assert!(saved.created_at > 0);
        let s = handle(&db, BooksOp::Log { book: dune.id.clone(), started_at: 0, ended_at: 60_000, page_to: Some(12) })
            .unwrap()
            .session
            .unwrap();
        assert_eq!((s.device.as_str(), s.pages_read()), ("test", Some(12)));
        let all = handle(&db, BooksOp::Library).unwrap();
        assert_eq!((all.books.len(), all.sessions.len(), all.readings.len()), (1, 1, 1));
        assert!(handle(&db, BooksOp::Save { book: Box::new(Book::new("  ")) }).is_err());
        assert!(handle(&db, BooksOp::Pin { book: Some("nope".into()) }).is_err());
        assert!(handle(&db, BooksOp::Log { book: dune.id.clone(), started_at: 10, ended_at: 0, page_to: None }).is_err());
        let set = handle(&db, BooksOp::SetDates { book: dune.id, started_at: 5, finished_at: None }).unwrap();
        assert_eq!(set.readings.len(), 1);
        assert_eq!(handle(&db, BooksOp::Library).unwrap().readings[0].started_at, 5);
    }
}
