//! Book search via the Google Books API (https://developers.google.com/books),
//! next to Open Library: it knows many more manga and recent German editions.
//! Works without a key, but that quota is shared and often used up: habitd
//! passes `[books] google_books_key` on with [`set_key`].

use serde::Deserialize;
use std::sync::Mutex;

static KEY: Mutex<Option<String>> = Mutex::new(None);

/// The API key searches use from now on.
pub fn set_key(key: Option<String>) {
    *KEY.lock().unwrap_or_else(|e| e.into_inner()) = key.filter(|k| !k.trim().is_empty());
}

use crate::model::{Book, Format};

/// What a hit's `olid` starts with, so it's told apart from Open Library ids.
pub const ID_PREFIX: &str = "gb:";

/// Publishers that only (or mostly) publish manga.
const MANGA_PUBLISHERS: [&str; 9] =
    ["tokyopop", "kazé", "kaze", "altraverse", "viz media", "yen press", "kodansha", "shueisha", "egmont manga"];

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Response {
    items: Vec<Volume>,
}

/// One search result.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Volume {
    pub id: String,
    #[serde(rename = "volumeInfo")]
    info: Info,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Info {
    title: String,
    subtitle: Option<String>,
    authors: Vec<String>,
    publisher: Option<String>,
    published_date: Option<String>,
    page_count: Option<u32>,
    categories: Vec<String>,
    industry_identifiers: Vec<Identifier>,
    image_links: Option<ImageLinks>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Identifier {
    #[serde(rename = "type")]
    kind: String,
    identifier: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ImageLinks {
    thumbnail: Option<String>,
    small_thumbnail: Option<String>,
}

#[cfg(feature = "store")]
pub fn search(query: &str, limit: usize) -> anyhow::Result<Vec<Volume>> {
    let limit = limit.min(40).to_string();
    let key = KEY.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut params = vec![("q", query), ("maxResults", limit.as_str()), ("printType", "books"), ("langRestrict", "de")];
    if let Some(key) = &key {
        params.push(("key", key));
    }
    let body = crate::http::get("https://www.googleapis.com/books/v1/volumes", &params).map_err(|e| match key {
        None => anyhow::anyhow!("{e} (without google_books_key under [books] the shared quota may be used up)"),
        Some(_) => e,
    })?;
    parse(&body)
}

pub fn parse(json: &[u8]) -> anyhow::Result<Vec<Volume>> {
    Ok(serde_json::from_slice::<Response>(json)?.items.into_iter().filter(|v| !v.info.title.trim().is_empty()).collect())
}

impl Volume {
    pub fn is_manga(&self) -> bool {
        let has = |s: &str| s.to_lowercase().contains("manga");
        let publisher = self.info.publisher.as_deref().unwrap_or_default().to_lowercase();
        self.info.categories.iter().any(|c| has(c))
            || has(&self.info.title)
            || has(&publisher)
            || MANGA_PUBLISHERS.iter().any(|p| publisher.contains(p))
    }

    pub fn isbn(&self) -> Option<String> {
        let of = |kind: &str| self.info.industry_identifiers.iter().find(|i| i.kind == kind).map(|i| i.identifier.clone());
        of("ISBN_13").or_else(|| of("ISBN_10"))
    }

    /// The cover over https, without the page curl Google draws on it.
    pub fn cover_url(&self) -> Option<String> {
        let links = self.info.image_links.as_ref()?;
        let url = links.thumbnail.as_ref().or(links.small_thumbnail.as_ref())?;
        Some(url.replacen("http://", "https://", 1).replace("&edge=curl", ""))
    }

    /// A new, unsaved book prefilled from this hit.
    pub fn to_book(&self) -> Book {
        let info = &self.info;
        let mut book = Book::new(info.title.split_whitespace().collect::<Vec<_>>().join(" "));
        book.subtitle = info.subtitle.clone().filter(|s| !s.trim().is_empty());
        book.author = Some(info.authors.join(", ")).filter(|a| !a.is_empty());
        book.olid = Some(format!("{ID_PREFIX}{}", self.id));
        book.pages = info.page_count.filter(|p| *p > 0);
        book.pub_year = info.published_date.as_deref().and_then(|d| d.get(..4)).and_then(|y| y.parse().ok());
        book.isbn = self.isbn();
        book.cover_url = self.cover_url();
        if self.is_manga() {
            book.format = Format::Manga;
        }
        book
    }
}

/// Open Library's hits first, then Google's that aren't among them (same ISBN,
/// or same title and author).
pub fn merge(open_library: Vec<Book>, google: Vec<Book>) -> Vec<Book> {
    fn key(b: &Book) -> String {
        format!("{}|{}", b.title.to_lowercase(), b.author.as_deref().unwrap_or_default().to_lowercase())
    }
    let mut seen_isbn: std::collections::HashSet<String> = open_library.iter().filter_map(|b| b.isbn.clone()).collect();
    let mut seen_key: std::collections::HashSet<String> = open_library.iter().map(key).collect();
    let mut all = open_library;
    for book in google {
        if book.isbn.as_ref().is_some_and(|i| seen_isbn.contains(i)) || seen_key.contains(&key(&book)) {
            continue;
        }
        seen_isbn.extend(book.isbn.clone());
        seen_key.insert(key(&book));
        all.push(book);
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"kind":"books#volumes","totalItems":3,"items":[
        {"id":"zyTCAlFPjgYC","volumeInfo":{"title":"One Piece  1","subtitle":"Romance Dawn","authors":["Eiichiro Oda"],
         "publisher":"Carlsen Manga","publishedDate":"2001-05","pageCount":208,"categories":["Comics & Graphic Novels"],
         "industryIdentifiers":[{"type":"ISBN_10","identifier":"3551745811"},{"type":"ISBN_13","identifier":"9783551745811"}],
         "imageLinks":{"smallThumbnail":"http://books.google.com/books/content?id=zyTCAlFPjgYC&printsec=frontcover&img=1&zoom=5&edge=curl&source=gbs_api",
         "thumbnail":"http://books.google.com/books/content?id=zyTCAlFPjgYC&printsec=frontcover&img=1&zoom=1&edge=curl&source=gbs_api"}}},
        {"id":"x","volumeInfo":{"title":"Der Process","authors":["Franz Kafka"],"publishedDate":"1925",
         "industryIdentifiers":[{"type":"OTHER","identifier":"UOM:39015"}]}},
        {"id":"empty","volumeInfo":{"title":" "}}
    ]}"#;

    #[test]
    fn parses_volumes_into_books() {
        let hits = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(hits.len(), 2, "volumes without a title are dropped");
        let op = hits[0].to_book();
        assert_eq!(op.title, "One Piece 1");
        assert_eq!(op.subtitle.as_deref(), Some("Romance Dawn"));
        assert_eq!(op.author.as_deref(), Some("Eiichiro Oda"));
        assert_eq!(op.olid.as_deref(), Some("gb:zyTCAlFPjgYC"));
        assert_eq!((op.pages, op.pub_year), (Some(208), Some(2001)));
        assert_eq!(op.isbn.as_deref(), Some("9783551745811"));
        assert_eq!(op.format, Format::Manga, "by its publisher");
        assert_eq!(
            op.cover_url.as_deref(),
            Some("https://books.google.com/books/content?id=zyTCAlFPjgYC&printsec=frontcover&img=1&zoom=1&source=gbs_api")
        );
        let kafka = hits[1].to_book();
        assert_eq!((kafka.isbn, kafka.cover_url, kafka.pub_year, kafka.format), (None, None, Some(1925), Format::Paperback));
        assert!(parse(br#"{"kind":"books#volumes","totalItems":0}"#).unwrap().is_empty());
    }

    #[test]
    fn merge_keeps_open_library_first_and_drops_duplicates() {
        let book = |title: &str, author: &str, isbn: Option<&str>| {
            let mut b = Book::new(title);
            b.author = Some(author.into());
            b.isbn = isbn.map(Into::into);
            b
        };
        let ol = vec![book("Dune", "Frank Herbert", Some("1")), book("Emma", "Jane Austen", None)];
        let gb = vec![
            book("Dune (German)", "Frank Herbert", Some("1")),
            book("emma", "jane austen", Some("2")),
            book("Berserk 1", "Kentaro Miura", Some("3")),
            book("Berserk 1", "Kentaro Miura", Some("4")),
        ];
        let titles: Vec<_> = merge(ol, gb).into_iter().map(|b| b.title).collect();
        assert_eq!(titles, ["Dune", "Emma", "Berserk 1"]);
    }
}
