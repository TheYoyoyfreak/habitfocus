//! German manga editions via Manga-Passion (https://www.manga-passion.de):
//! every edition a German publisher put out, with each of its volumes. Its API
//! is the one the site itself uses; it needs no key.

use serde::{Deserialize, Serialize};

use crate::model::{Book, Format};

pub const API: &str = "https://api.manga-passion.de";

/// What a volume's `olid` starts with: `mp:<edition>:<volume>`.
pub const ID_PREFIX: &str = "mp:";

/// Editions a search asks for; the API sorts newest first, so the one wanted
/// is often not among the first few.
pub const SEARCH_LIMIT: usize = 50;

/// Editions a search answers with, best first.
const SHOWN: usize = 10;

/// Volumes per page, the most the API gives.
pub const PAGE: usize = 100;

/// A German edition of a manga: the series to pick volumes from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Series {
    pub id: u64,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Volumes out so far.
    pub volumes: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover: Option<String>,
    /// An e-book edition.
    #[serde(default)]
    pub digital: bool,
    /// All its volumes are out.
    #[serde(default)]
    pub complete: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Edition {
    id: u64,
    title: String,
    num_volumes: Option<u32>,
    status: Option<u8>,
    publishers: Vec<Named>,
    sources: Vec<Source>,
    cover: Option<String>,
    print: Option<bool>,
    digital: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Named {
    name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Source {
    contributors: Vec<Contributor>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Contributor {
    contributor: Named,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Volume {
    id: u64,
    number: Option<u32>,
    /// Set on special editions ("Collectors Edition").
    title: Option<String>,
    special_type: Option<u8>,
    pages: Option<u32>,
    year: Option<i32>,
    cover: Option<String>,
    edition: Edition,
}

impl Edition {
    /// The series' name, without the "(eBook)" of digital editions.
    fn name(&self) -> String {
        let title = self.title.trim();
        title.strip_suffix("(eBook)").unwrap_or(title).trim().to_string()
    }

    fn author(&self) -> Option<String> {
        let mut names: Vec<&str> = Vec::new();
        for c in self.sources.iter().flat_map(|s| &s.contributors) {
            let name = c.contributor.name.trim();
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
        }
        Some(names.join(", ")).filter(|a| !a.is_empty())
    }

    fn to_series(&self) -> Series {
        Series {
            id: self.id,
            title: self.title.trim().to_string(),
            publisher: self.publishers.first().map(|p| p.name.clone()),
            author: self.author(),
            volumes: self.num_volumes.unwrap_or(0),
            cover: self.cover.clone(),
            digital: self.digital == Some(true) && self.print != Some(true),
            complete: self.status == Some(2),
        }
    }
}

/// The edition `olid` points into, for volumes from here.
pub fn edition_of(olid: &str) -> Option<u64> {
    olid.strip_prefix(ID_PREFIX)?.split(':').next()?.parse().ok()
}

/// What to search for: a trailing volume number ("chainsaw man 3") isn't
/// part of any edition's title.
pub fn query_of(query: &str) -> String {
    crate::series::split_volume(query).map_or_else(|| query.trim().to_string(), |(name, _)| name)
}

/// Editions whose title contains `query`, best match first: the exact title,
/// print before e-book, more volumes first.
pub fn parse_search(json: &[u8], query: &str) -> anyhow::Result<Vec<Series>> {
    let editions: Vec<Edition> = serde_json::from_slice(json)?;
    let wanted = crate::series::key(&query_of(query));
    let mut series: Vec<Series> = editions.iter().filter(|e| !e.title.trim().is_empty()).map(Edition::to_series).collect();
    series.sort_by_key(|s| {
        let name = crate::series::key(s.title.trim_end_matches("(eBook)"));
        (name != wanted, !name.starts_with(&wanted), s.digital, std::cmp::Reverse(s.volumes))
    });
    series.truncate(SHOWN);
    Ok(series)
}

/// One page of an edition's volumes, as books: one per number, the regular
/// volume over special editions of it.
pub fn parse_volumes(json: &[u8]) -> anyhow::Result<Vec<Book>> {
    let volumes: Vec<Volume> = serde_json::from_slice(json)?;
    Ok(volumes_to_books(volumes))
}

fn volumes_to_books(mut volumes: Vec<Volume>) -> Vec<Book> {
    volumes.retain(|v| v.number.is_some());
    volumes.sort_by_key(|v| (v.number, v.title.as_deref().is_some_and(|t| !t.trim().is_empty()), v.special_type.is_some(), v.id));
    volumes.dedup_by_key(|v| v.number);
    volumes.iter().map(Volume::to_book).collect()
}

impl Volume {
    fn to_book(&self) -> Book {
        let name = self.edition.name();
        let number = self.number.unwrap_or(0);
        let mut book = Book::new(format!("{name} {number}"));
        book.subtitle = self.title.clone().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        book.author = self.edition.author();
        book.olid = Some(format!("{ID_PREFIX}{}:{}", self.edition.id, self.id));
        book.pages = self.pages.filter(|p| *p > 0);
        book.pub_year = self.year;
        book.cover_url = self.cover.clone().or_else(|| self.edition.cover.clone());
        book.format = Format::Manga;
        book.series = Some(name);
        book.volume = Some(number);
        book
    }
}

#[cfg(feature = "store")]
pub fn search(query: &str) -> anyhow::Result<Vec<Series>> {
    let title = query_of(query);
    if title.is_empty() {
        return Ok(Vec::new());
    }
    let limit = SEARCH_LIMIT.to_string();
    let body = crate::http::get(&format!("{API}/editions"), &[("title", &title), ("itemsPerPage", &limit)])?;
    parse_search(&body, query)
}

/// Every volume of edition `id`, by number.
#[cfg(feature = "store")]
pub fn volumes(id: u64) -> anyhow::Result<Vec<Book>> {
    let url = format!("{API}/editions/{id}/volumes");
    let per_page = PAGE.to_string();
    let mut all: Vec<Volume> = Vec::new();
    for page in 1..=20 {
        let page = page.to_string();
        let body = crate::http::get(&url, &[("itemsPerPage", &per_page), ("page", &page)])?;
        let batch: Vec<Volume> = serde_json::from_slice(&body)?;
        let last = batch.len() < PAGE;
        all.extend(batch);
        if last {
            break;
        }
    }
    Ok(volumes_to_books(all))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EDITION: &str = r#"{"id":269,"title":"Chainsaw Man","status":1,"numVolumes":22,
        "publishers":[{"id":14,"name":"Egmont Manga"}],
        "sources":[{"id":264,"contributors":[{"id":514,"contributor":{"id":324,"name":"Tatsuki Fujimoto"},"role":0},
            {"id":515,"contributor":{"id":324,"name":"Tatsuki Fujimoto"},"role":1}]}],
        "cover":"https://media.manga-passion.de/e.jpg","print":true,"digital":false}"#;

    fn editions() -> String {
        let ebook = EDITION.replace("\"id\":269", "\"id\":2748").replace("Chainsaw Man\"", "Chainsaw Man (eBook)\"")
            .replace("\"print\":true,\"digital\":false", "\"print\":false,\"digital\":true");
        let buddy = r#"{"id":1950,"title":"Chainsaw Man: Buddy Stories","status":2,"numVolumes":1,"print":true}"#;
        let other = r#"{"id":7,"title":"Negative Happy Chainsaw Edge","numVolumes":2,"print":true}"#;
        format!("[{other},{ebook},{buddy},{EDITION}]")
    }

    #[test]
    fn the_edition_named_like_the_query_comes_first_print_before_ebook() {
        let series = parse_search(editions().as_bytes(), "chainsaw man 3").unwrap();
        let ids: Vec<u64> = series.iter().map(|s| s.id).collect();
        assert_eq!(ids, [269, 2748, 1950, 7]);
        let csm = &series[0];
        assert_eq!(csm.publisher.as_deref(), Some("Egmont Manga"));
        assert_eq!(csm.author.as_deref(), Some("Tatsuki Fujimoto"), "drawn and written by the same");
        assert_eq!((csm.volumes, csm.digital, csm.complete), (22, false, false));
        assert!(series[1].digital);
        assert!(series[2].complete);
    }

    #[test]
    fn volumes_become_books_one_per_number() {
        let json = format!(
            r#"[{{"id":20,"number":2,"pages":192,"year":2020,"cover":"https://m/2.jpg","edition":{EDITION}}},
                {{"id":10,"number":1,"pages":null,"year":2020,"cover":null,"edition":{EDITION}}},
                {{"id":99,"number":2,"title":"Collectors Edition","specialType":0,"edition":{EDITION}}},
                {{"id":5,"number":null,"edition":{EDITION}}}]"#
        );
        let books = parse_volumes(json.as_bytes()).unwrap();
        let titles: Vec<&str> = books.iter().map(|b| b.title.as_str()).collect();
        assert_eq!(titles, ["Chainsaw Man 1", "Chainsaw Man 2"]);
        let two = &books[1];
        assert_eq!(two.olid.as_deref(), Some("mp:269:20"), "the regular volume, not the special edition");
        assert_eq!((two.series.as_deref(), two.volume, two.pages), (Some("Chainsaw Man"), Some(2), Some(192)));
        assert_eq!((two.format, two.pub_year), (Format::Manga, Some(2020)));
        assert_eq!(books[0].cover_url.as_deref(), Some("https://media.manga-passion.de/e.jpg"), "edition cover");
        assert_eq!(edition_of("mp:269:20"), Some(269));
        assert_eq!(edition_of("OL1W"), None);
    }

    #[test]
    fn ebook_editions_name_the_series_without_the_suffix() {
        let ebook = EDITION.replace("Chainsaw Man\"", "Chainsaw Man (eBook)\"");
        let json = format!(r#"[{{"id":1,"number":4,"edition":{ebook}}}]"#);
        let books = parse_volumes(json.as_bytes()).unwrap();
        assert_eq!((books[0].title.as_str(), books[0].series.as_deref()), ("Chainsaw Man 4", Some("Chainsaw Man")));
    }

    /// Against the real API: `cargo test -p habit-books -- --ignored live`.
    #[cfg(feature = "store")]
    #[test]
    #[ignore = "network"]
    fn live() {
        let found = search("Chainsaw Man 3").unwrap();
        let csm = &found[0];
        assert_eq!((csm.title.as_str(), csm.publisher.as_deref()), ("Chainsaw Man", Some("Egmont Manga")), "{found:#?}");
        let csm_volumes = volumes(csm.id).unwrap();
        assert!(csm_volumes.len() >= 20);
        assert_eq!(csm_volumes[0].title, "Chainsaw Man 1");
        let one_piece = search("one piece").unwrap();
        assert_eq!(one_piece[0].title, "One Piece");
        let all = volumes(one_piece[0].id).unwrap();
        assert!(all.len() > 100, "more than one page: {}", all.len());
        assert!(all.windows(2).all(|w| w[0].volume < w[1].volume));
        eprintln!("{} {:?}", all.len(), all.last().map(|b| (&b.title, b.pages, &b.cover_url)));
    }
}
