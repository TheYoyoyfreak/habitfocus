//! Book data types, shared by habitd (storage) and its clients (wire format).
//! The field set mirrors openreads so its backups can be imported later.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Reading,
    Finished,
    ForLater,
    Abandoned,
}

impl Status {
    pub const ALL: [Status; 4] = [Status::Reading, Status::Finished, Status::ForLater, Status::Abandoned];

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Reading => "reading",
            Status::Finished => "finished",
            Status::ForLater => "for_later",
            Status::Abandoned => "abandoned",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Reading => "Reading",
            Status::Finished => "Finished",
            Status::ForLater => "For later",
            Status::Abandoned => "Abandoned",
        }
    }

    pub fn parse(s: &str) -> Option<Status> {
        Status::ALL.into_iter().find(|v| v.as_str() == s || v.label().eq_ignore_ascii_case(s))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Paperback,
    Hardcover,
    Ebook,
    Audiobook,
    Manga,
}

impl Format {
    pub const ALL: [Format; 5] = [Format::Paperback, Format::Hardcover, Format::Ebook, Format::Audiobook, Format::Manga];

    pub fn as_str(self) -> &'static str {
        match self {
            Format::Paperback => "paperback",
            Format::Hardcover => "hardcover",
            Format::Ebook => "ebook",
            Format::Audiobook => "audiobook",
            Format::Manga => "manga",
        }
    }

    pub fn parse(s: &str) -> Option<Format> {
        Format::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

/// Timestamps are unix milliseconds (UTC). `id` is a UUIDv7 so records created
/// on different devices never collide when they sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Book {
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub author: Option<String>,
    pub isbn: Option<String>,
    pub olid: Option<String>,
    pub pages: Option<u32>,
    pub pub_year: Option<i32>,
    pub format: Format,
    pub status: Status,
    /// 0 = unrated, otherwise 1..=10: points, or half stars when shown as
    /// five stars.
    pub score: u8,
    pub tags: Vec<String>,
    pub notes: Option<String>,
    pub cover_url: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Book {
    pub fn new(title: impl Into<String>) -> Book {
        Book {
            id: uuid::Uuid::now_v7().to_string(),
            title: title.into(),
            subtitle: None,
            author: None,
            isbn: None,
            olid: None,
            pages: None,
            pub_year: None,
            format: Format::Paperback,
            status: Status::ForLater,
            score: 0,
            tags: Vec::new(),
            notes: None,
            cover_url: None,
            created_at: 0,
            updated_at: 0,
        }
    }
}

/// Where a reading session was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Logged by hand (`hf books log`, the Books pane).
    Manual,
    /// A session of a reading habit.
    Habitfocus,
    App,
}

impl Source {
    pub const ALL: [Source; 3] = [Source::Manual, Source::Habitfocus, Source::App];

    pub fn as_str(self) -> &'static str {
        match self {
            Source::Manual => "manual",
            Source::Habitfocus => "habitfocus",
            Source::App => "app",
        }
    }

    pub fn parse(s: &str) -> Option<Source> {
        Source::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

/// One sitting with a book.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub book_id: String,
    pub started_at: i64,
    pub ended_at: i64,
    /// Time actually spent reading; can be less than `ended_at - started_at` (pauses).
    pub duration_ms: i64,
    /// Page before and after the session, when known.
    pub page_from: Option<u32>,
    pub page_to: Option<u32>,
    pub source: Source,
    /// Name of the device it was read on.
    pub device: String,
    /// Identity in the system it came from (e.g. a habitfocus session), for de-duplication.
    pub external_ref: Option<String>,
}

impl Session {
    pub fn pages_read(&self) -> Option<u32> {
        match (self.page_from, self.page_to) {
            (Some(from), Some(to)) if to >= from => Some(to - from),
            _ => None,
        }
    }
}

/// One pass through a book: opened when it moves to "reading", closed when it
/// is finished or abandoned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reading {
    pub id: String,
    pub book_id: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// `finished` or `abandoned` once closed.
    pub outcome: Option<Status>,
}

/// A finished reading-habit session that has no book yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub id: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub duration_ms: i64,
    pub source: Source,
    pub device: String,
    pub external_ref: String,
}

/// "The next reading session that ends is for this book."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pin {
    pub book_id: String,
    pub set_at: i64,
}

/// A pin only applies to sessions ending within this time after it was set.
pub const PIN_TTL_MS: i64 = 24 * 3_600_000;

impl Pin {
    pub fn applies_to(&self, ended_at: i64) -> bool {
        ended_at >= self.set_at && ended_at - self.set_at <= PIN_TTL_MS
    }
}

/// The `external_ref` of a habit session: the same on every device that
/// records it, so a session never comes in twice.
pub fn habit_session_ref(habit: &str, started_ms: i64) -> String {
    format!("habitfocus:{habit}:{started_ms}")
}

/// Session id derived from its `external_ref`: two devices that record the same
/// habit session create the same row, which sync then merges.
pub fn session_id_for_ref(external_ref: &str) -> String {
    const NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x6a0c_51f3_8d2e_4b7a_9c41_2f5e_b8d0_7a13);
    uuid::Uuid::new_v5(&NAMESPACE, external_ref.as_bytes()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_window() {
        let pin = Pin { book_id: "b".into(), set_at: 1_000 };
        assert!(!pin.applies_to(999));
        assert!(pin.applies_to(5_000));
        assert!(!pin.applies_to(1_000 + PIN_TTL_MS + 1));
    }

    #[test]
    fn session_ids_follow_the_ref() {
        let r = habit_session_ref("read", 1_000);
        assert_eq!(r, "habitfocus:read:1000");
        // The mobile app derives the same id; don't change the namespace.
        assert_eq!(session_id_for_ref(&r), "71edf177-f9d1-5979-8392-bf26776f3a9a");
        assert_ne!(session_id_for_ref(&r), session_id_for_ref(&habit_session_ref("read", 1_001)));
    }
}
