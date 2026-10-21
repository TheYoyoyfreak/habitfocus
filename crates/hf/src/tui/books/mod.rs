//! The Books pane: the book library (`[books]`), as booktrack had it. State and
//! key handling live here, independent of drawing; habitd keeps the library and
//! is reached through a [`Backend`] (the socket, or a database in tests).

mod cover;
mod form;
mod inbox;
mod insights;
mod reading;
pub mod ui;
mod volumes;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use habit_books::api::{BooksOp, BooksReply};
use habit_books::mangapassion::{self, Series};
use habit_books::series::{self, Row};
use habit_books::{Book, Pending, Pin, Reading, Session, Status};
use habit_ipc::Request;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::widgets::TableState;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;

use cover::{Cover, CoverMode, Pictures};
use form::{Form, FormOutcome};
use inbox::Assign;
use reading::LogForm;
use volumes::{Outcome as VolumesOutcome, Volumes};

/// Where the library is kept.
pub trait Backend {
    fn call(&self, op: BooksOp) -> Result<BooksReply, String>;

    /// Where the view (`G`) is remembered between runs; `None` forgets it.
    fn view_path(&self) -> Option<PathBuf> {
        None
    }
}

/// habitd, over its socket.
pub struct Ipc;

impl Backend for Ipc {
    fn call(&self, op: BooksOp) -> Result<BooksReply, String> {
        let response = habit_ipc::request(&Request::Books { op })?;
        if !response.ok {
            return Err(response.error.unwrap_or_else(|| "request failed".into()));
        }
        Ok(response.books.unwrap_or_default())
    }

    fn view_path(&self) -> Option<PathBuf> {
        Some(habit_ipc::state_path().with_file_name("hf.json"))
    }
}

/// Whether the file at `path` asks for the grid; anything unreadable is the list.
fn load_grid(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|prefs| prefs.get("books_grid")?.as_bool())
        .unwrap_or(false)
}

/// Writes `grid` into the file at `path`, keeping whatever else it holds.
fn save_grid(path: &Path, grid: bool) -> anyhow::Result<()> {
    let mut prefs = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text).ok())
        .unwrap_or_default();
    prefs.insert("books_grid".into(), grid.into());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&prefs)? + "\n")?;
    Ok(())
}

/// Status filters, `f` steps through them: one per status, then everything.
pub const TABS: [Option<Status>; 5] =
    [Some(Status::Reading), Some(Status::Finished), Some(Status::ForLater), Some(Status::Abandoned), None];

pub fn tab_label(tab: Option<Status>) -> &'static str {
    tab.map_or("All", Status::label)
}

pub enum Mode {
    Normal,
    Search,
    Form(Box<Form>),
    /// Index into `Status::ALL` currently highlighted.
    StatusPicker(usize),
    ConfirmDelete,
    Online(Box<Online>),
    /// Volumes of a manga edition to pick from.
    Volumes(Box<Volumes>),
    /// Page reached in the selected book's latest session, as typed.
    SetPage(String),
    Log(Box<LogForm>),
    /// Choosing the book for the oldest inbox session.
    Assign(Box<Assign>),
    /// The selected book's sessions, newest first.
    Sessions { selected: usize, confirm_delete: bool },
    Help,
}

/// Online search popup (Open Library, Google Books, Manga-Passion).
pub struct Online {
    pub query: String,
    pub state: OnlineState,
    /// Ignores answers to searches that were superseded.
    seq: u64,
}

pub enum OnlineState {
    Typing,
    Loading,
    /// Manga editions (`series`) come first, then the books; `selected`
    /// counts through both. `owned`: ids of hits already in the library.
    Results { series: Vec<Series>, hits: Vec<Book>, selected: usize, owned: HashSet<String> },
    Error(String),
}

/// Results of background work (network, image decoding).
pub enum Done {
    Cover { url: String, result: Result<image::DynamicImage, String> },
    Search { seq: u64, result: Result<(Vec<Series>, Vec<Book>), String> },
    Volumes { seq: u64, result: Result<Vec<Book>, String> },
}

/// A reading-habit session in progress, from the snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct Live {
    pub habit: String,
    pub running: bool,
    pub elapsed_ms: u64,
}

pub struct Books {
    backend: Box<dyn Backend>,
    /// Off in tests so nothing touches the network.
    pub network: bool,
    tx: Sender<Done>,
    rx: Receiver<Done>,
    seq: u64,
    /// By cover URL, so books sharing a cover share the download.
    pub covers: HashMap<String, Cover>,
    pub cover_mode: CoverMode,
    /// Set when the terminal has a real graphics protocol.
    pub picker: Option<Picker>,
    pub truecolor: bool,
    /// Something of habitfocus' own (a popup) lies over the pane, so no
    /// graphics: they would cover it.
    pub covered: bool,
    /// Encoded graphics on screen.
    pub graphic: Pictures<Protocol>,
    /// Pixel-art renderings on screen.
    pub pixel: Pictures<image::RgbaImage>,
    /// Index into `TABS`.
    pub tab: usize,
    /// The Insights view instead of the library.
    pub insights: bool,
    /// The library as a grid of covers instead of the table, remembered
    /// through `Backend::view_path`.
    pub grid: bool,
    /// First grid row on screen.
    pub grid_top: usize,
    pub query: String,
    /// Books on the current tab.
    pub books: Vec<Book>,
    /// What the table lists: `books`, with series folded.
    pub rows: Vec<Row>,
    /// Series (by `series::key`) whose volumes are listed under them.
    pub expanded: HashSet<String>,
    /// Matches per tab for the current query, same order as `TABS`.
    pub counts: [usize; 5],
    pub table: TableState,
    pub mode: Mode,
    /// All live sessions, oldest first.
    pub sessions: Vec<Session>,
    pub readings: Vec<Reading>,
    /// Book chosen in advance for the next reading session.
    pub pin: Option<Pin>,
    /// Sessions waiting for a book, oldest first.
    pub pending: Vec<Pending>,
    /// Best guess for a waiting session's book.
    pub suggested: Option<String>,
    /// Books with status "reading", and every live book.
    pub reading: Vec<Book>,
    pub all_books: Vec<Book>,
    /// Every live book's title by id (the table only holds the current tab).
    pub titles: HashMap<String, String>,
    /// A reading-habit session in progress.
    pub live: Option<Live>,
    /// Set while no habit feeds the library (habitd's `BooksView::hint`).
    pub hint: Option<String>,
    /// How ratings are shown (`[books] rating`).
    pub rating: habit_core::config::RatingScale,
    /// For the footer: text and whether it's an error. hf shows it.
    pub message: Option<(String, bool)>,
}

impl Books {
    pub fn new(backend: Box<dyn Backend>, picker: Option<&Picker>) -> Books {
        let (tx, rx) = mpsc::channel();
        let mut books = Books {
            backend,
            network: true,
            tx,
            rx,
            seq: 0,
            covers: HashMap::new(),
            cover_mode: CoverMode::Auto,
            // Halfblocks means "no graphics protocol", which our own pixel art handles.
            picker: picker.filter(|p| p.protocol_type() != ProtocolType::Halfblocks).cloned(),
            truecolor: cover::truecolor(),
            covered: false,
            graphic: Pictures::default(),
            pixel: Pictures::default(),
            tab: 0,
            insights: false,
            grid: false,
            grid_top: 0,
            query: String::new(),
            books: Vec::new(),
            rows: Vec::new(),
            expanded: HashSet::new(),
            counts: [0; 5],
            table: TableState::default(),
            mode: Mode::Normal,
            sessions: Vec::new(),
            readings: Vec::new(),
            pin: None,
            pending: Vec::new(),
            suggested: None,
            reading: Vec::new(),
            all_books: Vec::new(),
            titles: HashMap::new(),
            live: None,
            hint: None,
            rating: Default::default(),
            message: None,
        };
        books.grid = books.backend.view_path().is_some_and(|path| load_grid(&path));
        books.reload_or_report(None);
        // Start on "Reading" if there is something to read, else on "All".
        if books.books.is_empty() {
            books.tab = TABS.len() - 1;
            books.reload_or_report(None);
        }
        books
    }

    fn call(&self, op: BooksOp) -> anyhow::Result<BooksReply> {
        self.backend.call(op).map_err(anyhow::Error::msg)
    }

    pub fn set_message(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), false));
    }

    /// The selected book; `None` on a folded series' row.
    pub fn selected(&self) -> Option<&Book> {
        match self.selected_row()? {
            Row::Book { index, .. } => self.books.get(*index),
            Row::Series(_) => None,
        }
    }

    pub fn selected_row(&self) -> Option<&Row> {
        self.table.selected().and_then(|i| self.rows.get(i))
    }

    /// A series row's volume to show: the first not finished, else the last.
    pub fn series_current(&self, summary: &series::Summary) -> Option<&Book> {
        let volumes: Vec<&Book> = summary.books.iter().filter_map(|i| self.books.get(*i)).collect();
        volumes.iter().find(|b| b.status != Status::Finished).or(volumes.last()).copied()
    }

    /// Status filter of the current tab (`None` for "All").
    pub fn tab_status(&self) -> Option<Status> {
        TABS.get(self.tab).copied().flatten()
    }

    /// Whether a dialog of the pane is open (it takes every key).
    pub fn is_modal(&self) -> bool {
        !matches!(self.mode, Mode::Normal)
    }

    /// The cover that should be on screen: the highlighted search hit, else the selected book.
    pub fn cover_url(&self) -> Option<String> {
        if self.cover_mode == CoverMode::Off {
            return None;
        }
        if let Mode::Online(online) = &self.mode {
            return match &online.state {
                OnlineState::Results { series, hits, selected, .. } => match series.get(*selected) {
                    Some(s) => s.cover.clone(),
                    None => hits.get(*selected - series.len()).and_then(|b| b.cover_url.clone()),
                },
                _ => None,
            };
        }
        if let Mode::Volumes(v) = &self.mode {
            return v.current().and_then(|b| b.cover_url.clone());
        }
        if self.insights {
            return None;
        }
        self.row_cover(self.selected_row()?)
    }

    /// A row's cover: the book's, or for a series its current volume's.
    pub fn row_cover(&self, row: &Row) -> Option<String> {
        match row {
            Row::Series(s) => self.series_current(s)?.cover_url.clone(),
            Row::Book { index, .. } => self.books.get(*index)?.cover_url.clone(),
        }
    }

    /// Takes the wanted cover from the disk cache, or starts downloading it.
    pub fn request_cover(&mut self) {
        if let Some(url) = self.cover_url() {
            self.load_cover(url, usize::MAX);
        }
    }

    /// The same for the grid's tiles on screen, downloading a few at a time; the
    /// next frame asks for the rest.
    pub fn request_covers(&mut self, urls: impl IntoIterator<Item = String>) {
        for url in urls {
            self.load_cover(url, 4);
        }
    }

    /// Downloads only while fewer than `downloads` are under way.
    fn load_cover(&mut self, url: String, downloads: usize) {
        if self.covers.contains_key(&url) || !self.network {
            return;
        }
        if let Some(cover) = cover::cached(&url) {
            self.covers.insert(url, cover);
            return;
        }
        if self.covers.values().filter(|c| matches!(c, Cover::Loading)).count() >= downloads {
            return;
        }
        self.covers.insert(url.clone(), Cover::Loading);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = cover::download(&url);
            let _ = tx.send(Done::Cover { url, result });
        });
    }

    /// Applies finished background work; true if anything arrived.
    pub fn poll_jobs(&mut self) -> bool {
        let mut any = false;
        while let Ok(done) = self.rx.try_recv() {
            self.finish(done);
            any = true;
        }
        any
    }

    pub fn finish(&mut self, done: Done) {
        match done {
            Done::Cover { url, result } => {
                self.covers.insert(url, cover::ready(result));
            }
            Done::Search { seq, result } => {
                let owned = self.all_books.iter().filter_map(|b| b.olid.clone()).collect();
                let Mode::Online(online) = &mut self.mode else { return };
                if online.seq != seq {
                    return;
                }
                online.state = match result {
                    Ok((series, hits)) if series.is_empty() && hits.is_empty() => {
                        OnlineState::Error("Nothing found.".into())
                    }
                    Ok((series, hits)) => OnlineState::Results { series, hits, selected: 0, owned },
                    Err(e) => OnlineState::Error(e),
                };
            }
            Done::Volumes { seq, result } => {
                let Mode::Volumes(v) = &mut self.mode else { return };
                if v.seq != seq {
                    return;
                }
                if let Some(mut book) = v.loaded(result) {
                    book.status = Status::Reading;
                    self.mode = Mode::Form(Box::new(self.form(book, true)));
                }
            }
        }
    }

    fn start_search(&mut self, online: &mut Online) {
        self.seq += 1;
        online.seq = self.seq;
        online.state = OnlineState::Loading;
        if !self.network {
            return;
        }
        let (tx, query, seq) = (self.tx.clone(), online.query.trim().to_owned(), self.seq);
        std::thread::spawn(move || {
            let result = Ipc.call(BooksOp::Search { query }).map(|reply| (reply.series, reply.hits));
            let _ = tx.send(Done::Search { seq, result });
        });
    }

    fn online_key(&mut self, mut online: Box<Online>, key: KeyEvent) {
        match &mut online.state {
            OnlineState::Typing | OnlineState::Error(_) => match key.code {
                KeyCode::Esc => return,
                KeyCode::Enter if !online.query.trim().is_empty() => self.start_search(&mut online),
                KeyCode::Backspace => {
                    online.query.pop();
                    online.state = OnlineState::Typing;
                }
                KeyCode::Char(c) => {
                    online.query.push(c);
                    online.state = OnlineState::Typing;
                }
                _ => {}
            },
            OnlineState::Loading => {
                if key.code == KeyCode::Esc {
                    return;
                }
            }
            OnlineState::Results { series, hits, selected, .. } => match key.code {
                KeyCode::Esc => return,
                KeyCode::Char('j') | KeyCode::Down => *selected = (*selected + 1).min(series.len() + hits.len() - 1),
                KeyCode::Char('k') | KeyCode::Up => *selected = selected.saturating_sub(1),
                KeyCode::Char('/') | KeyCode::Char('i') => online.state = OnlineState::Typing,
                KeyCode::Enter if *selected < series.len() => {
                    let chosen = series[*selected].clone();
                    self.open_volumes(chosen, None);
                    return;
                }
                KeyCode::Enter => {
                    let mut book = hits[*selected - series.len()].clone();
                    book.status = self.tab_status().unwrap_or(Status::ForLater);
                    self.mode = Mode::Form(Box::new(self.form(book, true)));
                    return;
                }
                _ => {}
            },
        }
        self.mode = Mode::Online(online);
    }

    /// Volume numbers of the series `name` in the library.
    fn owned_volumes(&self, name: &str) -> HashSet<u32> {
        let k = series::key(name);
        self.all_books
            .iter()
            .filter_map(series::series_of)
            .filter(|(n, _)| series::key(n) == k)
            .filter_map(|(_, v)| v)
            .collect()
    }

    /// Shows the volumes of `edition` to pick from; with `want`, opens the
    /// form for that volume straight away.
    fn open_volumes(&mut self, edition: Series, want: Option<u32>) {
        let name = edition.title.trim_end_matches("(eBook)").trim().to_string();
        let mut v = Volumes::new(edition, self.owned_volumes(&name), want);
        self.seq += 1;
        v.seq = self.seq;
        if self.network {
            let (tx, seq, id) = (self.tx.clone(), self.seq, v.series.id);
            std::thread::spawn(move || {
                let result = Ipc.call(BooksOp::Volumes { edition: id }).map(|reply| reply.hits);
                let _ = tx.send(Done::Volumes { seq, result });
            });
        }
        self.mode = Mode::Volumes(Box::new(v));
    }

    /// `N`: the volume after the last one of the selected series in the library.
    fn next_volume(&mut self) {
        let name = match self.selected_row() {
            Some(Row::Series(s)) => Some(s.name.clone()),
            Some(Row::Book { .. }) => self.selected().and_then(series::series_of).map(|(name, _)| name),
            None => None,
        };
        let Some(name) = name else {
            self.set_message("Not part of a series: e sets one, O finds manga volumes");
            return;
        };
        let k = series::key(&name);
        let edition = self
            .all_books
            .iter()
            .filter(|b| series::series_of(b).is_some_and(|(n, _)| series::key(&n) == k))
            .find_map(|b| b.olid.as_deref().and_then(mangapassion::edition_of));
        let next = series::last_volume(&self.all_books, &name).unwrap_or(0) + 1;
        match edition {
            Some(id) => self.open_volumes(Series { id, title: name, ..Default::default() }, Some(next)),
            // Not from Manga-Passion: look the series up, the edition is picked there.
            None => {
                let mut online = Box::new(Online { query: name, state: OnlineState::Typing, seq: 0 });
                self.start_search(&mut online);
                self.mode = Mode::Online(online);
            }
        }
    }

    fn volumes_key(&mut self, mut v: Box<Volumes>, key: KeyEvent) -> anyhow::Result<()> {
        match v.handle_key(key) {
            VolumesOutcome::Continue => self.mode = Mode::Volumes(v),
            VolumesOutcome::Close => {}
            VolumesOutcome::Edit(mut book) => {
                book.status = self.tab_status().unwrap_or(Status::ForLater);
                self.mode = Mode::Form(Box::new(self.form(*book, true)));
            }
            VolumesOutcome::Add { books, status, finished } => {
                let n = books.len();
                let mut first = None;
                for mut book in books {
                    book.status = status;
                    let saved = self.call(BooksOp::Save { book: Box::new(book) })?.book;
                    let Some(saved) = saved else { continue };
                    if let (Some(day), Status::Finished | Status::Abandoned) = (finished, status) {
                        self.call(BooksOp::SetDates { book: saved.id.clone(), started_at: day, finished_at: Some(day) })?;
                    }
                    first.get_or_insert(saved);
                }
                let Some(first) = first else { return Ok(()) };
                let name = first.series.clone().unwrap_or_default();
                self.set_message(format!("Added {n} volume{} of {name}", if n == 1 { "" } else { "s" }));
                self.expanded.insert(series::key(&name));
                if self.tab_status().is_some_and(|s| s != status) {
                    self.tab = TABS.iter().position(|t| *t == Some(status)).unwrap_or(0);
                }
                self.insights = false;
                self.reload(Some(first.id))?;
            }
        }
        Ok(())
    }

    /// The add/edit form for `book`, with its reading dates and the rating scale.
    fn form(&self, book: Book, is_new: bool) -> Form {
        let (started, finished) = self.dates_of(&book);
        Form::new(book, is_new).with_scale(self.rating).with_dates(started, finished)
    }

    /// When `book` was started and finished: its latest pass, or else what
    /// its sessions say.
    pub fn dates_of(&self, book: &Book) -> (Option<i64>, Option<i64>) {
        if let Some(r) = self.readings.iter().filter(|r| r.book_id == book.id).max_by_key(|r| r.started_at) {
            return (Some(r.started_at), r.finished_at);
        }
        let st = self.book_stats(&book.id, book.pages);
        let done = matches!(book.status, Status::Finished | Status::Abandoned);
        (st.first_read, st.last_read.filter(|_| done))
    }

    /// Re-reads the library, showing what went wrong instead of failing.
    pub fn reload_or_report(&mut self, keep: Option<String>) {
        if let Err(e) = self.reload(keep) {
            self.message = Some((format!("{e:#}"), true));
        }
    }

    /// Re-reads the library, keeping `keep` (or the current book) selected when still visible.
    pub fn reload(&mut self, keep: Option<String>) -> anyhow::Result<()> {
        let keep = keep.or_else(|| match self.selected_row()? {
            Row::Series(s) => s.books.first().and_then(|i| self.books.get(*i)).map(|b| b.id.clone()),
            Row::Book { .. } => self.selected().map(|b| b.id.clone()),
        });
        let library = self.call(BooksOp::Library)?;
        self.sessions = library.sessions;
        self.readings = library.readings;
        self.pin = library.pin;
        self.pending = library.pending;
        self.suggested = library.suggested;
        self.all_books = library.books;
        self.reading = self.all_books.iter().filter(|b| b.status == Status::Reading).cloned().collect();
        self.titles = self.all_books.iter().map(|b| (b.id.clone(), b.title.clone())).collect();
        let all: Vec<Book> = self.all_books.iter().filter(|b| matches_query(b, &self.query)).cloned().collect();
        for (count, tab) in self.counts.iter_mut().zip(TABS) {
            *count = all.iter().filter(|b| tab.is_none_or(|s| b.status == s)).count();
        }
        let tab = self.tab_status();
        self.books = all.into_iter().filter(|b| tab.is_none_or(|s| b.status == s)).collect();
        self.rows = series::rows(&self.books, &self.all_books, &self.expanded);
        let pos = keep.and_then(|id| self.books.iter().position(|b| b.id == id)).and_then(|book| {
            self.rows.iter().position(|row| match row {
                Row::Book { index, .. } => *index == book,
                Row::Series(s) => !s.expanded && s.books.contains(&book),
            })
        });
        let fallback = self.table.selected().unwrap_or(0).min(self.rows.len().saturating_sub(1));
        self.table.select(if self.rows.is_empty() { None } else { Some(pos.unwrap_or(fallback)) });
        Ok(())
    }

    /// Picks up changes made elsewhere (the CLI, a finished session) and asks
    /// for the book when new sessions land in the inbox. True when it did.
    pub fn refresh(&mut self) -> bool {
        if !matches!(self.mode, Mode::Normal | Mode::Search) {
            return false;
        }
        let before = self.pending.len();
        self.reload_or_report(None);
        if self.pending.len() > before {
            self.open_assign();
            return true;
        }
        false
    }

    /// Handles `key` if it is one of the pane's; false lets habitfocus have it.
    pub fn handle_key(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => return self.normal_key(key),
            Mode::Search => self.search_key(key)?,
            Mode::Form(mut form) => match form.handle_key(key) {
                FormOutcome::Continue => self.mode = Mode::Form(form),
                FormOutcome::Cancel => {}
                FormOutcome::Save(book, dates) => {
                    let book = self
                        .call(BooksOp::Save { book })?
                        .book
                        .ok_or_else(|| anyhow::anyhow!("habitd didn't save the book"))?;
                    if let Some((started_at, finished_at)) = dates {
                        self.call(BooksOp::SetDates { book: book.id.clone(), started_at, finished_at })?;
                    }
                    self.set_message(format!("Saved “{}”", book.title));
                    if let Some((name, _)) = series::series_of(&book) {
                        self.expanded.insert(series::key(&name));
                    }
                    // Follow the book to its tab so it stays in view.
                    if self.tab_status().is_some_and(|s| s != book.status) {
                        self.tab = TABS.iter().position(|t| *t == Some(book.status)).unwrap_or(0);
                    }
                    self.insights = false;
                    self.reload(Some(book.id))?;
                }
            },
            Mode::StatusPicker(i) => match key.code {
                KeyCode::Char('j') | KeyCode::Down => self.mode = Mode::StatusPicker((i + 1) % Status::ALL.len()),
                KeyCode::Char('k') | KeyCode::Up => {
                    self.mode = Mode::StatusPicker((i + Status::ALL.len() - 1) % Status::ALL.len())
                }
                KeyCode::Enter => {
                    if let Some(id) = self.selected().map(|b| b.id.clone()) {
                        let status = Status::ALL[i];
                        self.call(BooksOp::SetStatus { id, status })?;
                        self.set_message(format!("Moved to {}", status.label()));
                        self.reload(None)?;
                    }
                }
                _ => {}
            },
            Mode::Online(online) => self.online_key(online, key),
            Mode::Volumes(v) => self.volumes_key(v, key)?,
            Mode::SetPage(page) => self.set_page_key(page, key)?,
            Mode::Log(form) => self.log_key(form, key)?,
            Mode::Assign(a) => self.assign_key(a, key)?,
            Mode::Sessions { selected, confirm_delete } => self.sessions_key(selected, confirm_delete, key)?,
            Mode::Help => {}
            Mode::ConfirmDelete => {
                if key.code == KeyCode::Char('y') {
                    if let Some(book) = self.selected().cloned() {
                        self.call(BooksOp::Delete { id: book.id })?;
                        self.set_message(format!("Deleted “{}”", book.title));
                        self.reload(None)?;
                    }
                }
            }
        }
        Ok(true)
    }

    fn normal_key(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        // Insights has no book list: only switching views applies there.
        if self.insights && !matches!(key.code, KeyCode::Char('f' | 'F' | 'i' | '?' | 'I')) {
            return Ok(false);
        }
        let len = self.rows.len();
        match key.code {
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('i') => self.insights = !self.insights,
            KeyCode::Char('f') => self.switch_tab((self.tab + 1) % TABS.len())?,
            KeyCode::Char('F') => self.switch_tab((self.tab + TABS.len() - 1) % TABS.len())?,
            KeyCode::Char('g') => self.open_log(),
            KeyCode::Char('v') if self.selected().is_some() => {
                self.mode = Mode::Sessions { selected: 0, confirm_delete: false }
            }
            KeyCode::Char('m') => self.toggle_pin()?,
            KeyCode::Char('I') => {
                if self.pending.is_empty() {
                    self.set_message("Inbox is empty");
                } else {
                    self.open_assign();
                }
            }
            KeyCode::Char('p') if self.selected().is_some() => self.mode = Mode::SetPage(String::new()),
            KeyCode::Esc if !self.query.is_empty() => {
                self.query.clear();
                self.reload(None)?;
            }
            KeyCode::Char('j') | KeyCode::Down if len > 0 => {
                self.table.select(Some(self.table.selected().map_or(0, |i| (i + 1).min(len - 1))))
            }
            KeyCode::Char('k') | KeyCode::Up if len > 0 => {
                self.table.select(Some(self.table.selected().map_or(0, |i| i.saturating_sub(1))))
            }
            KeyCode::Home if len > 0 => self.table.select(Some(0)),
            KeyCode::End if len > 0 => self.table.select(Some(len - 1)),
            KeyCode::Char('/') => self.mode = Mode::Search,
            KeyCode::Char('O') => {
                self.mode = Mode::Online(Box::new(Online { query: String::new(), state: OnlineState::Typing, seq: 0 }))
            }
            KeyCode::Char('G') => {
                self.grid = !self.grid;
                self.set_message(if self.grid { "Grid view" } else { "List view" });
                if let Some(path) = self.backend.view_path() {
                    if let Err(e) = save_grid(&path, self.grid) {
                        self.message = Some((format!("Couldn't remember the view: {e:#}"), true));
                    }
                }
            }
            KeyCode::Char('C') => {
                self.cover_mode = self.cover_mode.next();
                self.set_message(format!("Covers: {}", self.cover_mode.label()));
            }
            KeyCode::Char('n') => {
                let mut book = Book::new("");
                book.status = self.tab_status().unwrap_or(Status::ForLater);
                self.mode = Mode::Form(Box::new(self.form(book, true)));
            }
            KeyCode::Enter | KeyCode::Char(' ') if matches!(self.selected_row(), Some(Row::Series(_))) => {
                self.toggle_series()?
            }
            KeyCode::Char('N') => self.next_volume(),
            KeyCode::Char('e') | KeyCode::Enter => {
                if let Some(book) = self.selected().cloned() {
                    self.mode = Mode::Form(Box::new(self.form(book, false)));
                }
            }
            KeyCode::Char('S') => {
                if let Some(book) = self.selected() {
                    let i = Status::ALL.iter().position(|s| *s == book.status).unwrap_or(0);
                    self.mode = Mode::StatusPicker(i);
                }
            }
            KeyCode::Char('d') if self.selected().is_some() => self.mode = Mode::ConfirmDelete,
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Folds or unfolds the selected series, staying on its row.
    fn toggle_series(&mut self) -> anyhow::Result<()> {
        let Some(Row::Series(s)) = self.selected_row() else { return Ok(()) };
        let (k, open) = (s.key.clone(), s.expanded);
        if open {
            self.expanded.remove(&k);
        } else {
            self.expanded.insert(k.clone());
        }
        self.reload(None)?;
        if let Some(pos) = self.rows.iter().position(|r| matches!(r, Row::Series(s) if s.key == k)) {
            self.table.select(Some(pos));
        }
        Ok(())
    }

    fn search_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        match key.code {
            KeyCode::Enter => return Ok(()),
            KeyCode::Esc => self.query.clear(),
            KeyCode::Backspace => {
                self.query.pop();
            }
            KeyCode::Char(c) => self.query.push(c),
            _ => {}
        }
        if key.code != KeyCode::Esc {
            self.mode = Mode::Search;
        }
        self.reload(None)
    }

    fn switch_tab(&mut self, tab: usize) -> anyhow::Result<()> {
        self.tab = tab;
        self.insights = false;
        self.table.select(Some(0));
        self.reload(None)
    }
}

/// Like the library's own search: title, author or a tag contains the query,
/// ignoring case; an empty query matches everything.
fn matches_query(book: &Book, query: &str) -> bool {
    let needle = query.trim().to_lowercase();
    needle.is_empty()
        || book.title.to_lowercase().contains(&needle)
        || book.author.as_deref().unwrap_or("").to_lowercase().contains(&needle)
        || book.tags.iter().any(|t| t.to_lowercase().contains(&needle))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;

    /// The library in an in-memory database, answered like habitd would.
    pub struct Local(pub habit_books::Db);

    impl Backend for Local {
        fn call(&self, op: BooksOp) -> Result<BooksReply, String> {
            habit_books::api::handle(&self.0, op).map_err(|e| format!("{e:#}"))
        }
    }

    pub fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub fn press(books: &mut Books, keys: &str) {
        for c in keys.chars() {
            let code = match c {
                '\n' => KeyCode::Enter,
                '\x1b' => KeyCode::Esc,
                '\t' => KeyCode::Tab,
                c => KeyCode::Char(c),
            };
            books.handle_key(key(code)).unwrap();
        }
    }

    pub fn books_from(db: habit_books::Db) -> Books {
        let mut books = Books::new(Box::new(Local(db)), None);
        books.network = false;
        books
    }

    pub fn books_with(list: &[(&str, Status)]) -> Books {
        let db = habit_books::Db::open_in_memory().unwrap();
        for (title, status) in list {
            let mut b = Book::new(*title);
            b.status = *status;
            db.save_book(&mut b).unwrap();
        }
        books_from(db)
    }

    fn titles(books: &Books) -> Vec<&str> {
        books.books.iter().map(|b| b.title.as_str()).collect()
    }

    #[test]
    fn starts_on_reading_tab_or_all_when_empty() {
        assert_eq!(books_with(&[("Dune", Status::Reading)]).tab, 0);
        assert_eq!(books_with(&[("Emma", Status::Finished)]).tab, 4);
    }

    #[test]
    fn grid_is_remembered_next_to_other_settings() {
        let dir = std::env::temp_dir().join(format!("hf-view-{}", std::process::id()));
        let path = dir.join("hf.json");
        assert!(!load_grid(&path), "no file is the list");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, r#"{ "other": 1 }"#).unwrap();
        save_grid(&path, true).unwrap();
        assert!(load_grid(&path));
        let prefs: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(prefs["other"], 1, "the rest stays");
        save_grid(&path, false).unwrap();
        assert!(!load_grid(&path));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn add_book_via_form_lands_on_its_tab() {
        let mut books = books_with(&[]);
        press(&mut books, "nDune\n");
        assert!(matches!(books.mode, Mode::Normal));
        // Added from the "All" tab, so it defaults to "For later" and stays on "All".
        assert_eq!(titles(&books), ["Dune"]);
        assert_eq!(books.selected().unwrap().status, Status::ForLater);
        press(&mut books, "f");
        assert!(books.books.is_empty(), "now on Reading");
    }

    #[test]
    fn status_picker_moves_book() {
        let mut books = books_with(&[("Dune", Status::Reading)]);
        // Reading -> Finished is one step down in the picker.
        press(&mut books, "Sj\n");
        assert!(books.books.is_empty());
        assert_eq!(books.counts[1], 1);
    }

    #[test]
    fn search_filters_and_escape_clears() {
        let mut books = books_with(&[("Dune", Status::Finished), ("Emma", Status::Finished)]);
        press(&mut books, "/em");
        assert_eq!(titles(&books), ["Emma"]);
        press(&mut books, "\n");
        assert!(matches!(books.mode, Mode::Normal));
        assert_eq!(books.query, "em");
        press(&mut books, "\x1b");
        assert_eq!(books.books.len(), 2);
    }

    #[test]
    fn delete_needs_confirmation() {
        let mut books = books_with(&[("Dune", Status::Reading)]);
        press(&mut books, "dn");
        assert_eq!(books.books.len(), 1);
        press(&mut books, "dy");
        assert!(books.books.is_empty());
    }

    #[test]
    fn habitfocus_keys_pass_through() {
        let mut books = books_with(&[("Dune", Status::Reading)]);
        for c in ['q', 's', 'a', 'o', 'R', 'Y', 'L', 'h', 'l', '2'] {
            assert!(!books.handle_key(key(KeyCode::Char(c))).unwrap(), "{c} is habitfocus'");
        }
        assert!(!books.handle_key(key(KeyCode::Esc)).unwrap(), "esc without a filter quits");
        assert!(books.handle_key(key(KeyCode::Char('j'))).unwrap());
        // In Insights only the view keys are the pane's.
        press(&mut books, "i");
        assert!(books.insights);
        assert!(!books.handle_key(key(KeyCode::Char('j'))).unwrap());
        press(&mut books, "i");
        assert!(!books.insights);
    }

    #[test]
    fn online_search_prefills_add_form() {
        let mut books = books_with(&[("Dune", Status::Reading)]);
        press(&mut books, "Oone piece\n");
        let seq = match &books.mode {
            Mode::Online(o) => {
                assert!(matches!(o.state, OnlineState::Loading));
                o.seq
            }
            _ => panic!("not searching"),
        };
        let hit = |title: &str, olid: &str| {
            let mut b = Book::new(title);
            b.olid = Some(olid.into());
            b.format = habit_books::Format::Manga;
            b
        };
        // A stale answer is ignored, the current one shown.
        books.finish(Done::Search { seq: seq + 7, result: Ok((vec![], vec![])) });
        books.finish(Done::Search { seq, result: Ok((vec![], vec![hit("ONE PIECE 1", "OL1W"), hit("ONE PIECE 2", "OL2W")])) });
        press(&mut books, "j\n");
        let Mode::Form(_) = &books.mode else { panic!("no form") };
        press(&mut books, "\n");
        let added = books.selected().unwrap();
        assert_eq!(added.title, "ONE PIECE 2");
        assert_eq!(added.olid.as_deref(), Some("OL2W"));
        assert_eq!(added.format, habit_books::Format::Manga);
        // Added from the "Reading" tab.
        assert_eq!(added.status, Status::Reading);
    }

    #[test]
    fn search_errors_are_shown_and_editable() {
        let mut books = books_with(&[]);
        press(&mut books, "Ox\n");
        books.finish(Done::Search { seq: 1, result: Err("timeout".into()) });
        let Mode::Online(o) = &books.mode else { panic!() };
        assert!(matches!(&o.state, OnlineState::Error(e) if e == "timeout"));
        press(&mut books, "y");
        let Mode::Online(o) = &books.mode else { panic!() };
        assert_eq!(o.query, "xy");
        press(&mut books, "\x1b");
        assert!(matches!(books.mode, Mode::Normal));
    }

    fn manga(db: &habit_books::Db, title: &str, status: Status, olid: Option<&str>) {
        let mut b = Book::new(title);
        b.format = habit_books::Format::Manga;
        b.status = status;
        b.olid = olid.map(str::to_owned);
        habit_books::series::fill_in(&mut b);
        db.save_book(&mut b).unwrap();
    }

    fn series_lib() -> Books {
        let db = habit_books::Db::open_in_memory().unwrap();
        manga(&db, "Berserk 1", Status::Finished, Some("mp:5:1"));
        manga(&db, "Berserk 2", Status::Finished, Some("mp:5:2"));
        let mut dune = Book::new("Dune");
        dune.status = Status::Finished;
        db.save_book(&mut dune).unwrap();
        let mut books = books_from(db);
        books.tab = 1;
        books.reload(None).unwrap();
        books
    }

    #[test]
    fn volumes_fold_into_their_series_and_unfold_with_enter() {
        let mut books = series_lib();
        assert_eq!(books.books.len(), 3);
        assert_eq!(books.rows.len(), 2, "Berserk folded, Dune");
        let first = books.rows.iter().position(|r| matches!(r, Row::Series(_))).unwrap();
        books.table.select(Some(first));
        assert!(books.selected().is_none(), "a series row is no book");
        press(&mut books, "\n");
        assert_eq!(books.rows.len(), 4);
        assert!(matches!(books.selected_row(), Some(Row::Series(s)) if s.expanded));
        press(&mut books, "j");
        assert_eq!(books.selected().unwrap().title, "Berserk 1");
        press(&mut books, "k ");
        assert_eq!(books.rows.len(), 2, "space folds too");
    }

    #[test]
    fn next_volume_loads_the_edition_and_opens_the_form() {
        let mut books = series_lib();
        let first = books.rows.iter().position(|r| matches!(r, Row::Series(_))).unwrap();
        books.table.select(Some(first));
        press(&mut books, "N");
        let Mode::Volumes(v) = &books.mode else { panic!("no volumes") };
        assert_eq!((v.series.id, v.want), (5, Some(3)));
        let seq = v.seq;
        let volume = |n: u32| {
            let mut b = Book::new(format!("Berserk {n}"));
            b.series = Some("Berserk".into());
            b.volume = Some(n);
            b.format = habit_books::Format::Manga;
            b
        };
        books.finish(Done::Volumes { seq, result: Ok((1..=4).map(volume).collect()) });
        let Mode::Form(_) = &books.mode else { panic!("no form") };
        press(&mut books, "\n");
        assert_eq!(books.tab, 0, "a next volume is being read");
        assert_eq!(books.selected().unwrap().title, "Berserk 3");
    }

    #[test]
    fn marked_volumes_are_added_at_once() {
        let mut books = series_lib();
        books.open_volumes(Series { id: 5, title: "Berserk".into(), ..Default::default() }, None);
        let Mode::Volumes(v) = &books.mode else { panic!() };
        let seq = v.seq;
        let volume = |n: u32| {
            let mut b = Book::new(format!("Berserk {n}"));
            b.series = Some("Berserk".into());
            b.volume = Some(n);
            b
        };
        books.finish(Done::Volumes { seq, result: Ok((1..=5).map(volume).collect()) });
        // Starts on volume 3; marks 3 and 4, finished on a given day.
        press(&mut books, "ja\n2026-02-01\n");
        assert!(matches!(books.mode, Mode::Normal));
        assert_eq!(books.message.as_ref().unwrap().0, "Added 2 volumes of Berserk");
        assert_eq!(books.counts[1], 5, "finished: four volumes of Berserk and Dune");
        let day = form::parse_date("2026-02-01", "x").unwrap();
        let four = books.all_books.iter().find(|b| b.title == "Berserk 4").unwrap().id.clone();
        let reading = books.readings.iter().find(|r| r.book_id == four).unwrap();
        assert_eq!((Some(reading.started_at), reading.finished_at), (day, day));
        assert!(matches!(books.selected_row(), Some(Row::Book { nested: true, .. })), "unfolded to show them");
    }
}
