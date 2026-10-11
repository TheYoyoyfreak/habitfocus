//! `hf books`: the book library. habitd keeps it; this asks and prints.

use anyhow::{anyhow, bail, ensure};
use chrono::{Local, NaiveDateTime, TimeZone};
use clap::Subcommand;
use habit_books::stats::{self, format_duration};
use habit_books::{Book, Status};
use habit_ipc::books::{BooksOp, BooksReply};
use habit_ipc::Request;

#[derive(Subcommand)]
pub enum BooksCommand {
    /// List books
    List {
        #[arg(short, long, value_parser = parse_status)]
        status: Option<Status>,
        /// Filter by title, author or tag
        #[arg(short, long, default_value = "")]
        query: String,
        #[arg(long)]
        json: bool,
    },
    /// Add a book
    Add {
        title: String,
        #[arg(short, long)]
        author: Option<String>,
        #[arg(short, long)]
        pages: Option<u32>,
        /// reading, finished, for_later or abandoned
        #[arg(short, long, default_value = "for_later", value_parser = parse_status)]
        status: Status,
    },
    /// Search Open Library, Google Books and Manga-Passion; add a result with --add
    Search {
        query: String,
        /// Add the result with this number to the library
        #[arg(long)]
        add: Option<usize>,
        #[arg(short, long, default_value = "for_later", value_parser = parse_status)]
        status: Status,
    },
    /// Volumes of a German manga edition (its id from `search`); add some with --add
    Volumes {
        edition: u64,
        /// Volumes to add: "3", "1-5" or "1-3,7"
        #[arg(long)]
        add: Option<String>,
        #[arg(short, long, default_value = "for_later", value_parser = parse_status)]
        status: Status,
    },
    /// Change a book's status (reading, finished, for_later, abandoned)
    Status {
        /// Book title, author or id
        book: String,
        #[arg(value_parser = parse_status)]
        status: Status,
    },
    /// Record a session after the fact
    Log {
        /// Book title, author or id
        book: String,
        #[arg(short, long)]
        minutes: u32,
        /// Page reached
        #[arg(short, long)]
        page: Option<u32>,
        /// Start time, "YYYY-MM-DD HH:MM" (default: `minutes` ago)
        #[arg(long)]
        at: Option<String>,
    },
    /// Set the page reached in the latest session (e.g. after a reading habit)
    Page {
        page: u32,
        /// Book (default: the book of the latest session)
        #[arg(short, long)]
        book: Option<String>,
    },
    /// List reading sessions
    Sessions {
        book: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Reading statistics
    Stats,
    /// The next reading session that ends goes to this book (no book: show the pin)
    Pin {
        book: Option<String>,
        /// Unpin
        #[arg(long, conflicts_with = "book")]
        clear: bool,
    },
    /// Sessions waiting for a book (several books were being read)
    Inbox {
        #[command(subcommand)]
        action: Option<InboxAction>,
    },
    /// Take over the books of the standalone booktrack app (run once)
    ImportBooktrack {
        /// Default: ~/.local/share/booktrack/booktrack.db
        path: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum InboxAction {
    /// Record session number `n` (from `hf books inbox`) for a book
    Assign {
        n: usize,
        book: String,
        /// Page reached
        #[arg(short, long)]
        page: Option<u32>,
    },
    /// Drop session number `n` for good
    Discard { n: usize },
}

fn parse_status(s: &str) -> Result<Status, String> {
    Status::parse(s).ok_or_else(|| format!("unknown status `{s}` (reading, finished, for_later, abandoned)"))
}

fn call(op: BooksOp) -> anyhow::Result<BooksReply> {
    let response = habit_ipc::request(&Request::Books { op }).map_err(anyhow::Error::msg)?;
    if !response.ok {
        bail!("{}", response.error.unwrap_or_else(|| "request failed".into()));
    }
    Ok(response.books.unwrap_or_default())
}

/// Finds one book by id, or by a title/author/tag query that matches exactly
/// one book (or exactly one title).
fn resolve_book<'a>(books: &'a [Book], query: &str) -> anyhow::Result<&'a Book> {
    if let Some(book) = books.iter().find(|b| b.id == query) {
        return Ok(book);
    }
    let needle = query.trim().to_lowercase();
    let mut hits: Vec<&Book> = books
        .iter()
        .filter(|b| {
            b.title.to_lowercase().contains(&needle)
                || b.author.as_deref().unwrap_or("").to_lowercase().contains(&needle)
                || b.tags.iter().any(|t| t.to_lowercase().contains(&needle))
        })
        .collect();
    if hits.len() > 1 {
        let exact: Vec<&Book> = hits.iter().copied().filter(|b| b.title.eq_ignore_ascii_case(query.trim())).collect();
        if exact.len() == 1 {
            hits = exact;
        }
    }
    match hits.len() {
        0 => bail!("no book matches “{query}”"),
        1 => Ok(hits[0]),
        _ => {
            let titles: Vec<_> = hits.iter().take(8).map(|b| format!("  {}", b.title)).collect();
            bail!("“{query}” matches several books:\n{}", titles.join("\n"))
        }
    }
}

fn title<'a>(books: &'a [Book], id: &'a str) -> &'a str {
    books.iter().find(|b| b.id == id).map_or(id, |b| b.title.as_str())
}

fn local_time(ms: i64) -> String {
    Local.timestamp_millis_opt(ms).single().map_or_else(String::new, |t| t.format("%Y-%m-%d %H:%M").to_string())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn run(command: BooksCommand) -> anyhow::Result<()> {
    // On stderr, so `--json` output stays clean.
    if let Some(hint) = habit_ipc::request(&Request::Status).ok().and_then(|r| r.snapshot?.books?.hint) {
        eprintln!("note: {hint}");
    }
    match command {
        BooksCommand::List { status, query, json } => {
            let books = call(BooksOp::List { status, query })?.books;
            if json {
                println!("{}", serde_json::to_string_pretty(&books)?);
            } else {
                for b in books {
                    println!("{:<10} {}  {}", b.status.label(), b.title, b.author.as_deref().unwrap_or(""));
                }
            }
        }
        BooksCommand::Add { title, author, pages, status } => {
            let mut book = Book::new(title);
            book.author = author;
            book.pages = pages;
            book.status = status;
            let book = call(BooksOp::Save { book: Box::new(book) })?.book.ok_or_else(|| anyhow!("habitd didn't save the book"))?;
            println!("added {} ({})", book.title, book.id);
        }
        BooksCommand::Search { query, add, status } => {
            let reply = call(BooksOp::Search { query })?;
            let hits = reply.hits;
            if let Some(n) = add {
                let mut book = hits.get(n.wrapping_sub(1)).cloned().ok_or_else(|| anyhow!("no result #{n}"))?;
                book.status = status;
                let book = call(BooksOp::Save { book: Box::new(book) })?.book.ok_or_else(|| anyhow!("habitd didn't save the book"))?;
                println!("added {} ({})", book.title, book.id);
                return Ok(());
            }
            if hits.is_empty() {
                println!("nothing found");
            }
            for (i, h) in hits.iter().enumerate() {
                let year = h.pub_year.map(|y| format!(" ({y})")).unwrap_or_default();
                let manga = if h.format == habit_books::Format::Manga { " [manga]" } else { "" };
                let google = if is_google(h) { " [Google Books]" } else { "" };
                println!("{:>2}. {} — {}{year}{manga}{google}", i + 1, h.title, h.author.as_deref().unwrap_or(""));
            }
            if !reply.series.is_empty() {
                println!("\nManga series (hf books volumes <id> lists their volumes):");
            }
            for s in &reply.series {
                let volumes = (s.volumes > 0).then(|| format!("{} volumes", s.volumes));
                let facts: Vec<String> = [s.publisher.clone(), volumes, s.author.clone()].into_iter().flatten().collect();
                println!("  [{}] {} — {}", s.id, s.title, facts.join(", "));
            }
        }
        BooksCommand::Volumes { edition, add, status } => {
            let volumes = call(BooksOp::Volumes { edition })?.hits;
            let Some(add) = add else {
                for v in &volumes {
                    let pages = v.pages.map(|p| format!(", {p} pages")).unwrap_or_default();
                    let year = v.pub_year.map(|y| format!(" ({y})")).unwrap_or_default();
                    println!("{:>4}. {}{year}{pages}", v.volume.unwrap_or(0), v.title);
                }
                return Ok(());
            };
            let wanted = parse_volumes(&add)?;
            let mut added = 0;
            for mut book in volumes.into_iter().filter(|v| v.volume.is_some_and(|n| wanted.contains(&n))) {
                book.status = status;
                let book = call(BooksOp::Save { book: Box::new(book) })?.book.ok_or_else(|| anyhow!("habitd didn't save the book"))?;
                println!("added {} ({})", book.title, book.id);
                added += 1;
            }
            if added == 0 {
                bail!("no such volumes in edition {edition}");
            }
        }
        BooksCommand::Status { book, status } => {
            let books = call(BooksOp::List { status: None, query: String::new() })?.books;
            let book = resolve_book(&books, &book)?;
            call(BooksOp::SetStatus { id: book.id.clone(), status })?;
            println!("“{}”: {}", book.title, status.label());
        }
        BooksCommand::Log { book, minutes, page, at } => {
            let books = call(BooksOp::List { status: None, query: String::new() })?.books;
            let book = resolve_book(&books, &book)?;
            let duration = i64::from(minutes) * 60_000;
            let start = match at {
                Some(at) => {
                    let naive = NaiveDateTime::parse_from_str(&at, "%Y-%m-%d %H:%M")
                        .map_err(|_| anyhow!("--at must look like 2026-10-07 20:30"))?;
                    Local.from_local_datetime(&naive).earliest().ok_or_else(|| anyhow!("no such local time"))?.timestamp_millis()
                }
                None => now_ms() - duration,
            };
            call(BooksOp::Log { book: book.id.clone(), started_at: start, ended_at: start + duration, page_to: page })?;
            println!("logged {} of “{}”", format_duration(duration), book.title);
        }
        BooksCommand::Page { page, book } => {
            let library = call(BooksOp::Library)?;
            let book = match book {
                Some(q) => resolve_book(&library.books, &q)?,
                None => {
                    let last = library
                        .sessions
                        .iter()
                        .max_by_key(|s| s.ended_at)
                        .ok_or_else(|| anyhow!("no sessions yet; pass --book"))?;
                    resolve_book(&library.books, &last.book_id)?
                }
            };
            let s = call(BooksOp::SetPage { book: book.id.clone(), page })?.session;
            let read = s.and_then(|s| s.pages_read()).map(|p| format!(", {p} pages in that session")).unwrap_or_default();
            println!("“{}”: page {page}{read}", book.title);
        }
        BooksCommand::Sessions { book, json } => {
            let library = call(BooksOp::Library)?;
            let book_id = book.map(|q| resolve_book(&library.books, &q).map(|b| b.id.clone())).transpose()?;
            let sessions: Vec<_> =
                library.sessions.iter().filter(|s| book_id.as_ref().is_none_or(|id| &s.book_id == id)).collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
                return Ok(());
            }
            for s in sessions {
                let pages = match (s.page_from, s.page_to) {
                    (Some(f), Some(t)) => format!("p.{f}–{t}"),
                    (None, Some(t)) => format!("→ p.{t}"),
                    _ => String::new(),
                };
                println!(
                    "{}  {:>7}  {:<11} {:<10} {:<12} {}",
                    local_time(s.started_at),
                    format_duration(s.duration_ms),
                    pages,
                    s.source.as_str(),
                    s.device,
                    title(&library.books, &s.book_id)
                );
            }
        }
        BooksCommand::Stats => {
            let library = call(BooksOp::Library)?;
            let ins = stats::insights(&library.sessions, &library.books, &library.readings, now_ms(), &Local);
            println!("today      {}", format_duration(ins.today_ms));
            println!("this week  {}", format_duration(ins.week_ms));
            println!("this month {}", format_duration(ins.month_ms));
            println!("this year  {}  ({} books finished)", format_duration(ins.year_ms), ins.finished_this_year);
            println!("all time   {}  in {} sessions, {} pages", format_duration(ins.total_ms), ins.sessions, ins.pages);
            let days = |n: u32| if n == 1 { "1 day".to_owned() } else { format!("{n} days") };
            println!("streak     {} (longest {})", days(ins.current_streak), days(ins.longest_streak));
            for (id, ms) in ins.by_book.iter().take(5) {
                println!("  {:>8}  {}", format_duration(*ms), title(&library.books, id));
            }
        }
        BooksCommand::Pin { book, clear } => {
            let library = call(BooksOp::Library)?;
            if clear {
                call(BooksOp::Pin { book: None })?;
                println!("unpinned");
            } else if let Some(q) = book {
                let book = resolve_book(&library.books, &q)?;
                call(BooksOp::Pin { book: Some(book.id.clone()) })?;
                println!("the next reading session goes to “{}”", book.title);
            } else {
                match &library.pin {
                    Some(pin) => println!("pinned: {}", title(&library.books, &pin.book_id)),
                    None => println!("nothing pinned"),
                }
            }
        }
        BooksCommand::Inbox { action } => {
            let library = call(BooksOp::Library)?;
            let pending = &library.pending;
            let nth = |n: usize| pending.get(n.wrapping_sub(1)).ok_or_else(|| anyhow!("no session #{n} (see hf books inbox)"));
            match action {
                None => {
                    if pending.is_empty() {
                        println!("inbox is empty");
                    }
                    for (i, p) in pending.iter().enumerate() {
                        println!(
                            "{:>2}. {}  {:>7}  {} · {}",
                            i + 1,
                            local_time(p.started_at),
                            format_duration(p.duration_ms),
                            p.source.as_str(),
                            p.device
                        );
                    }
                }
                Some(InboxAction::Assign { n, book, page }) => {
                    let p = nth(n)?;
                    let book = resolve_book(&library.books, &book)?;
                    call(BooksOp::Assign { pending: p.id.clone(), book: book.id.clone(), page_to: page })?;
                    println!("recorded {} for “{}”", format_duration(p.duration_ms), book.title);
                }
                Some(InboxAction::Discard { n }) => {
                    let p = nth(n)?;
                    call(BooksOp::Discard { pending: p.id.clone() })?;
                    println!("discarded {} from {}", format_duration(p.duration_ms), local_time(p.started_at));
                }
            }
        }
        BooksCommand::ImportBooktrack { path } => {
            let reply = call(BooksOp::ImportBooktrack { path })?;
            let n = reply.imported.unwrap_or_default();
            println!(
                "took over {} books, {} readings, {} sessions and {} waiting sessions ({} books in the library)",
                n.books,
                n.readings,
                n.sessions,
                n.pending,
                reply.books.len()
            );
        }
    }
    Ok(())
}

/// Whether a search hit came from Google Books rather than Open Library.
/// "1-3,7" → 1, 2, 3, 7.
fn parse_volumes(s: &str) -> anyhow::Result<Vec<u32>> {
    let mut all = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let bad = || anyhow!("volumes look like 3, 1-5 or 1-3,7");
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (u32, u32) = (a.trim().parse().map_err(|_| bad())?, b.trim().parse().map_err(|_| bad())?);
                ensure!(a <= b, "{part}: the first volume comes first");
                all.extend(a..=b);
            }
            None => all.push(part.parse().map_err(|_| bad())?),
        }
    }
    Ok(all)
}

fn is_google(book: &habit_books::Book) -> bool {
    book.olid.as_deref().is_some_and(|id| id.starts_with(habit_books::googlebooks::ID_PREFIX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(title: &str, author: &str) -> Book {
        let mut b = Book::new(title);
        b.author = Some(author.into());
        b
    }

    #[test]
    fn volume_ranges() {
        assert_eq!(parse_volumes("1-3, 7").unwrap(), [1, 2, 3, 7]);
        assert_eq!(parse_volumes("4").unwrap(), [4]);
        assert!(parse_volumes("5-2").is_err());
        assert!(parse_volumes("one").is_err());
    }

    #[test]
    fn resolves_by_id_query_or_exact_title() {
        let books = [book("Dune", "Frank Herbert"), book("Dune Messiah", "Frank Herbert"), book("Emma", "Jane Austen")];
        assert_eq!(resolve_book(&books, &books[2].id).unwrap().title, "Emma");
        assert_eq!(resolve_book(&books, "austen").unwrap().title, "Emma");
        assert_eq!(resolve_book(&books, "dune").unwrap().title, "Dune", "an exact title wins");
        assert!(resolve_book(&books, "herbert").unwrap_err().to_string().contains("several"));
        assert!(resolve_book(&books, "nope").is_err());
    }
}
