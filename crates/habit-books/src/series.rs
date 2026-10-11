//! Volumes of a series (manga, mostly): which series a book belongs to, and
//! the library list with each series folded into one row.

use std::collections::{HashMap, HashSet};

use crate::model::{Book, Format, Status};

/// Words that may stand between a series' name and its volume number.
const MARKERS: [&str; 7] = ["band", "bd.", "bd", "vol.", "vol", "volume", "nr."];

/// "Chainsaw Man 21", "Berserk, Band 3", "Vinland Saga Vol. 2" → the series
/// and the volume. Titles that are only a number, or don't end in one, aren't
/// volumes.
pub fn split_volume(title: &str) -> Option<(String, u32)> {
    let title = title.trim();
    let digits = title.len() - title.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 || digits > 4 {
        return None;
    }
    let (rest, number) = title.split_at(title.len() - digits);
    let number: u32 = number.parse().ok()?;
    // The number must stand on its own ("Fahrenheit451" is no volume).
    let mut rest = rest.strip_suffix('#').unwrap_or(rest);
    if !rest.ends_with(char::is_whitespace) && !rest.ends_with('.') {
        return None;
    }
    rest = rest.trim_end();
    let lower = rest.to_lowercase();
    if let Some(marker) = MARKERS.iter().find(|m| {
        lower.ends_with(*m) && lower[..lower.len() - m.len()].ends_with(|c: char| c.is_whitespace() || c == ',')
    }) {
        rest = &rest[..rest.len() - marker.len()];
    }
    let name = rest.trim_end().trim_end_matches([',', '-', '–', ':', '#']).trim_end();
    let is_marker = MARKERS.contains(&name.to_lowercase().as_str());
    (name.chars().any(char::is_alphabetic) && !is_marker).then(|| (name.to_string(), number))
}

/// The series `book` is a volume of: what it says, or for a manga without
/// one, what its title says.
pub fn series_of(book: &Book) -> Option<(String, Option<u32>)> {
    if let Some(name) = book.series.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let volume = book.volume.or_else(|| split_volume(&book.title).map(|(_, v)| v));
        return Some((name.to_string(), volume));
    }
    if book.format == Format::Manga {
        return split_volume(&book.title).map(|(name, v)| (name, Some(v)));
    }
    None
}

/// Series names compared without case or extra spaces.
pub fn key(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Fills in a manga's series and volume from its title when they're missing.
pub fn fill_in(book: &mut Book) {
    if book.series.as_deref().is_some_and(|s| !s.trim().is_empty()) {
        if book.volume.is_none() {
            book.volume = split_volume(&book.title).map(|(_, v)| v);
        }
        return;
    }
    if book.format == Format::Manga {
        if let Some((name, volume)) = split_volume(&book.title) {
            book.series = Some(name);
            book.volume = Some(volume);
        }
    }
}

/// One line of the library list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A book; `nested` under its series' row.
    Book { index: usize, nested: bool },
    /// Two or more volumes of a series, folded.
    Series(Summary),
}

/// What a folded series row shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub key: String,
    pub name: String,
    /// Indices into the list, by volume.
    pub books: Vec<usize>,
    /// Volume numbers in the list, lowest and highest.
    pub first: Option<u32>,
    pub last: Option<u32>,
    /// Of the whole series in the library (not just this list): volumes, and
    /// how many of them are finished.
    pub owned: usize,
    pub finished: usize,
    pub expanded: bool,
}

/// `books` (one tab of the library) with the volumes of each series folded
/// into one row where the first of them was; `expanded` series also list
/// their volumes under it. `library` is every book, for the counts.
pub fn rows(books: &[Book], library: &[Book], expanded: &HashSet<String>) -> Vec<Row> {
    let mut members: HashMap<String, Vec<(Option<u32>, usize)>> = HashMap::new();
    let mut names: HashMap<String, String> = HashMap::new();
    for (i, book) in books.iter().enumerate() {
        if let Some((name, volume)) = series_of(book) {
            let k = key(&name);
            names.entry(k.clone()).or_insert(name);
            members.entry(k).or_default().push((volume, i));
        }
    }
    let mut totals: HashMap<String, (usize, usize)> = HashMap::new();
    for book in library {
        if let Some((name, _)) = series_of(book) {
            let t = totals.entry(key(&name)).or_default();
            t.0 += 1;
            t.1 += usize::from(book.status == Status::Finished);
        }
    }
    let mut rows = Vec::new();
    let mut done = HashSet::new();
    for (i, book) in books.iter().enumerate() {
        let Some(k) = series_of(book).map(|(name, _)| key(&name)).filter(|k| members[k].len() > 1) else {
            rows.push(Row::Book { index: i, nested: false });
            continue;
        };
        if !done.insert(k.clone()) {
            continue;
        }
        let mut volumes = members[&k].clone();
        volumes.sort_by_key(|&(v, i)| (v.is_none(), v, i));
        let numbers: Vec<u32> = volumes.iter().filter_map(|&(v, _)| v).collect();
        let (owned, finished) = totals.get(&k).copied().unwrap_or_default();
        let open = expanded.contains(&k);
        rows.push(Row::Series(Summary {
            key: k.clone(),
            name: names[&k].clone(),
            books: volumes.iter().map(|&(_, i)| i).collect(),
            first: numbers.iter().min().copied(),
            last: numbers.iter().max().copied(),
            owned,
            finished,
            expanded: open,
        }));
        if open {
            rows.extend(volumes.iter().map(|&(_, index)| Row::Book { index, nested: true }));
        }
    }
    rows
}

/// "Vol. 3–7", "Vol. 4", or nothing.
pub fn range_label(first: Option<u32>, last: Option<u32>) -> String {
    match (first, last) {
        (Some(a), Some(b)) if a != b => format!("Vol. {a}–{b}"),
        (Some(a), _) => format!("Vol. {a}"),
        _ => String::new(),
    }
}

/// The highest volume of the series `name` in the library.
pub fn last_volume(library: &[Book], name: &str) -> Option<u32> {
    let k = key(name);
    library.iter().filter_map(series_of).filter(|(n, _)| key(n) == k).filter_map(|(_, v)| v).max()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manga(title: &str, status: Status) -> Book {
        let mut b = Book::new(title);
        b.format = Format::Manga;
        b.status = status;
        b
    }

    #[test]
    fn volumes_are_read_off_titles() {
        let split = |t: &str| split_volume(t);
        assert_eq!(split("Chainsaw Man 21"), Some(("Chainsaw Man".into(), 21)));
        assert_eq!(split("Chainsaw Man 01"), Some(("Chainsaw Man".into(), 1)));
        assert_eq!(split("Berserk, Band 3"), Some(("Berserk".into(), 3)));
        assert_eq!(split("Vinland Saga Vol. 2"), Some(("Vinland Saga".into(), 2)));
        assert_eq!(split("One Piece - Bd. 105"), Some(("One Piece".into(), 105)));
        assert_eq!(split("Naruto #7"), Some(("Naruto".into(), 7)));
        assert_eq!(split("ONE PIECE  1"), Some(("ONE PIECE".into(), 1)));
        // Not volumes.
        assert_eq!(split("1984"), None);
        assert_eq!(split("Fahrenheit451"), None);
        assert_eq!(split("Dune"), None);
        assert_eq!(split("Band 3"), None);
    }

    #[test]
    fn manga_fall_into_series_and_other_books_only_when_told() {
        assert_eq!(series_of(&manga("Berserk 2", Status::Reading)), Some(("Berserk".into(), Some(2))));
        let novel = Book::new("Fahrenheit 451");
        assert_eq!(series_of(&novel), None, "a number in a novel's title is no volume");
        let mut dune = Book::new("Dune Messiah");
        dune.series = Some("Dune".into());
        dune.volume = Some(2);
        assert_eq!(series_of(&dune), Some(("Dune".into(), Some(2))));

        let mut b = manga("Chainsaw Man 4", Status::ForLater);
        fill_in(&mut b);
        assert_eq!((b.series.as_deref(), b.volume), (Some("Chainsaw Man"), Some(4)));
        let mut kept = manga("Chainsaw Man 4", Status::ForLater);
        kept.series = Some("CSM".into());
        fill_in(&mut kept);
        assert_eq!((kept.series.as_deref(), kept.volume), (Some("CSM"), Some(4)));
    }

    #[test]
    fn series_fold_into_one_row_where_their_first_volume_was() {
        let books = vec![
            manga("Berserk 2", Status::Finished),
            Book::new("Dune"),
            manga("Berserk 1", Status::Finished),
            manga("Naruto 1", Status::Finished),
            manga("berserk  3", Status::Reading),
        ];
        let folded = rows(&books, &books, &HashSet::new());
        assert_eq!(folded.len(), 3, "{folded:?}");
        let Row::Series(s) = &folded[0] else { panic!("{folded:?}") };
        assert_eq!((s.name.as_str(), s.books.clone(), s.first, s.last), ("Berserk", vec![2, 0, 4], Some(1), Some(3)));
        assert_eq!((s.owned, s.finished, s.expanded), (3, 2, false));
        assert_eq!(folded[1], Row::Book { index: 1, nested: false });
        assert_eq!(folded[2], Row::Book { index: 3, nested: false }, "a single volume stays a book");

        let open = rows(&books, &books, &HashSet::from(["berserk".to_string()]));
        assert_eq!(open.len(), 6);
        assert_eq!(open[1..4], [2, 0, 4].map(|index| Row::Book { index, nested: true }));
        assert_eq!(last_volume(&books, "BERSERK"), Some(3));
        assert_eq!(range_label(Some(1), Some(3)), "Vol. 1–3");
    }
}
