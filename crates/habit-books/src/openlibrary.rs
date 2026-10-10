//! Book search via the Open Library search API (https://openlibrary.org/dev/docs/api/search).

use serde::Deserialize;

use crate::http;
use crate::model::{Book, Format};

const FIELDS: &str = "key,title,subtitle,author_name,first_publish_year,isbn,number_of_pages_median,cover_i,subject,\
     editions,editions.key,editions.title,editions.subtitle,editions.isbn,editions.cover_i";

/// Preferred edition language (ISO 639-1). Open Library ranks works with editions in this
/// language higher and returns the best-matching edition alongside each work.
const LANG: &str = "de";

/// One search result (an Open Library "work").
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Hit {
    /// e.g. `/works/OL8215661W`
    pub key: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub author_name: Vec<String>,
    pub first_publish_year: Option<i32>,
    pub isbn: Vec<String>,
    pub number_of_pages_median: Option<u32>,
    pub cover_i: Option<i64>,
    pub subject: Vec<String>,
    editions: Editions,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Editions {
    docs: Vec<Edition>,
}

/// The edition Open Library picked for [`LANG`] (falls back to another language if none exists).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Edition {
    title: Option<String>,
    subtitle: Option<String>,
    isbn: Vec<String>,
    cover_i: Option<i64>,
}

#[derive(Deserialize)]
struct Response {
    docs: Vec<Hit>,
}

pub fn search(query: &str, limit: usize) -> anyhow::Result<Vec<Hit>> {
    let limit = limit.to_string();
    let body = http::get(
        "https://openlibrary.org/search.json",
        &[("q", query), ("fields", FIELDS), ("lang", LANG), ("limit", &limit)],
    )?;
    parse(&body)
}

pub fn parse(json: &[u8]) -> anyhow::Result<Vec<Hit>> {
    let mut hits = serde_json::from_slice::<Response>(json)?.docs;
    hits.iter_mut().for_each(Hit::apply_edition);
    Ok(hits)
}

impl Hit {
    /// Shows the preferred edition's title, ISBN and cover instead of the work's (usually
    /// English) ones. Author, page count and first publish year stay work-level.
    fn apply_edition(&mut self) {
        let Some(ed) = self.editions.docs.pop() else { return };
        if let Some(title) = ed.title.map(|t| t.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|t| !t.is_empty()) {
            self.title = title;
            self.subtitle = ed.subtitle;
        }
        if !ed.isbn.is_empty() {
            self.isbn = ed.isbn;
        }
        if ed.cover_i.is_some_and(|id| id > 0) {
            self.cover_i = ed.cover_i;
        }
    }

    /// Open Library id without the `/works/` prefix.
    pub fn olid(&self) -> &str {
        self.key.rsplit('/').next().unwrap_or(&self.key)
    }

    pub fn authors(&self) -> Option<String> {
        Some(self.author_name.join(", ")).filter(|a| !a.is_empty())
    }

    pub fn is_manga(&self) -> bool {
        self.subject.iter().any(|s| s.to_lowercase().contains("manga"))
    }

    pub fn cover_url(&self) -> Option<String> {
        self.cover_i.filter(|id| *id > 0).map(|id| format!("https://covers.openlibrary.org/b/id/{id}-L.jpg"))
    }

    /// A new, unsaved book prefilled from this hit.
    pub fn to_book(&self) -> Book {
        let mut book = Book::new(self.title.clone());
        book.subtitle = self.subtitle.clone();
        book.author = self.authors();
        book.olid = Some(self.olid().to_owned());
        book.pages = self.number_of_pages_median;
        book.pub_year = self.first_publish_year;
        // Prefer ISBN-13; the list mixes both forms across editions.
        book.isbn = self.isbn.iter().find(|i| i.len() == 13).or(self.isbn.first()).cloned();
        book.cover_url = self.cover_url();
        if self.is_manga() {
            book.format = Format::Manga;
        }
        book
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"numFound":2,"docs":[
        {"author_name":["Eiichiro Oda"],"cover_i":1020563,"first_publish_year":1997,
         "isbn":["1591163641","9781569319017"],"key":"/works/OL8215661W",
         "number_of_pages_median":208,"title":"ONE PIECE 1","subject":["form:manga","Pirates"]},
        {"key":"/works/OL1W","title":"Bare"},
        {"key":"/works/OL82563W","title":"Harry Potter and the Philosopher's Stone","cover_i":1,
         "isbn":["9780747532699"],"editions":{"docs":[{"key":"/books/OL47682314M",
         "title":"Harry Potter und der Stein der Weisen","cover_i":15157474,"isbn":["9783551313119","3551313113"]}]}}
    ]}"#;

    #[test]
    fn parses_hits_into_books() {
        let hits = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(hits.len(), 3);
        let book = hits[0].to_book();
        assert_eq!(book.title, "ONE PIECE 1");
        assert_eq!(book.author.as_deref(), Some("Eiichiro Oda"));
        assert_eq!(book.olid.as_deref(), Some("OL8215661W"));
        assert_eq!(book.isbn.as_deref(), Some("9781569319017"));
        assert_eq!(book.pages, Some(208));
        assert_eq!(book.format, Format::Manga);
        assert_eq!(book.cover_url.as_deref(), Some("https://covers.openlibrary.org/b/id/1020563-L.jpg"));

        // Missing fields are fine.
        let bare = hits[1].to_book();
        assert_eq!((bare.author, bare.cover_url, bare.format), (None, None, Format::Paperback));

        // The preferred-language edition wins over the work's own data.
        let de = hits[2].to_book();
        assert_eq!(de.title, "Harry Potter und der Stein der Weisen");
        assert_eq!(de.isbn.as_deref(), Some("9783551313119"));
        assert_eq!(de.cover_url.as_deref(), Some("https://covers.openlibrary.org/b/id/15157474-L.jpg"));
    }
}
