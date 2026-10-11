//! Picking volumes of a manga edition (Manga-Passion) to add: one opens the
//! add form, several are added at once with a status and finish date.

use std::collections::{BTreeSet, HashSet};

use habit_books::mangapassion::Series;
use habit_books::{Book, Status};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Margin};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use super::form::{format_date, parse_date};
use super::ui::{popup, ACCENT};
use super::Books;

pub struct Volumes {
    pub series: Series,
    pub state: State,
    /// Ignores answers to loads that were superseded.
    pub seq: u64,
    /// The volume to open the form for once loaded (`N`).
    pub want: Option<u32>,
    /// Volume numbers of this series already in the library.
    pub owned: HashSet<u32>,
}

pub enum State {
    Loading,
    Error(String),
    Ready(Picker),
}

pub struct Picker {
    pub books: Vec<Book>,
    pub cursor: usize,
    pub marked: BTreeSet<usize>,
    pub note: Option<String>,
    /// Adding the marked volumes: how.
    pub bulk: Option<Bulk>,
}

pub struct Bulk {
    pub status: Status,
    /// The day they were finished, as typed; empty is today.
    pub date: String,
    pub error: Option<String>,
}

pub enum Outcome {
    Continue,
    Close,
    /// One volume: on to the add form.
    Edit(Box<Book>),
    /// Several at once, finished (or abandoned) on `finished`.
    Add { books: Vec<Book>, status: Status, finished: Option<i64> },
}

impl Volumes {
    pub fn new(series: Series, owned: HashSet<u32>, want: Option<u32>) -> Volumes {
        Volumes { series, state: State::Loading, seq: 0, want, owned }
    }

    /// The loaded volumes arrived: straight to the form for the wanted one,
    /// else the list, starting at the first volume not in the library.
    pub fn loaded(&mut self, result: Result<Vec<Book>, String>) -> Option<Book> {
        let books = match result {
            Ok(books) if books.is_empty() => {
                self.state = State::Error("Manga-Passion lists no volumes for this edition yet.".into());
                return None;
            }
            Ok(books) => books,
            Err(e) => {
                self.state = State::Error(e);
                return None;
            }
        };
        let mut note = None;
        if let Some(want) = self.want.take() {
            if let Some(book) = books.iter().find(|b| b.volume == Some(want)) {
                return Some(book.clone());
            }
            note = Some(format!("Volume {want} isn't out yet."));
        }
        let cursor = books.iter().position(|b| !self.is_owned(b)).unwrap_or(books.len() - 1);
        self.state = State::Ready(Picker { books, cursor, marked: BTreeSet::new(), note, bulk: None });
        None
    }

    pub fn is_owned(&self, book: &Book) -> bool {
        book.volume.is_some_and(|v| self.owned.contains(&v))
    }

    /// The volume under the cursor.
    pub fn current(&self) -> Option<&Book> {
        match &self.state {
            State::Ready(p) => p.books.get(p.cursor),
            _ => None,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        let owned = self.owned.clone();
        let State::Ready(picker) = &mut self.state else {
            return if key.code == KeyCode::Esc { Outcome::Close } else { Outcome::Continue };
        };
        if let Some(bulk) = &mut picker.bulk {
            match key.code {
                KeyCode::Esc => picker.bulk = None,
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    let all = [Status::Finished, Status::Reading, Status::ForLater, Status::Abandoned];
                    let i = all.iter().position(|s| *s == bulk.status).unwrap_or(0);
                    let step = if key.code == KeyCode::Left { all.len() - 1 } else { 1 };
                    bulk.status = all[(i + step) % all.len()];
                }
                KeyCode::Backspace => {
                    bulk.date.pop();
                }
                KeyCode::Char(c) if c.is_ascii_digit() || c == '-' => bulk.date.push(c),
                KeyCode::Enter => {
                    let finished = match parse_date(bulk.date.trim(), "Finished") {
                        Ok(day) => day,
                        Err(e) => {
                            bulk.error = Some(e);
                            return Outcome::Continue;
                        }
                    };
                    let books = picker.marked.iter().filter_map(|i| picker.books.get(*i).cloned()).collect();
                    return Outcome::Add { books, status: bulk.status, finished };
                }
                _ => {}
            }
            return Outcome::Continue;
        }
        let last = picker.books.len() - 1;
        let is_owned = |b: &Book| b.volume.is_some_and(|v| owned.contains(&v));
        match key.code {
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Char('j') | KeyCode::Down => picker.cursor = (picker.cursor + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => picker.cursor = picker.cursor.saturating_sub(1),
            KeyCode::PageDown => picker.cursor = (picker.cursor + 10).min(last),
            KeyCode::PageUp => picker.cursor = picker.cursor.saturating_sub(10),
            KeyCode::Home => picker.cursor = 0,
            KeyCode::End => picker.cursor = last,
            KeyCode::Char(' ') => {
                if !is_owned(&picker.books[picker.cursor]) && !picker.marked.remove(&picker.cursor) {
                    picker.marked.insert(picker.cursor);
                }
                picker.cursor = (picker.cursor + 1).min(last);
            }
            // Everything up to here that isn't in the library yet.
            KeyCode::Char('a') => {
                let upto = (0..=picker.cursor).filter(|&i| !is_owned(&picker.books[i]));
                picker.marked.extend(upto);
            }
            KeyCode::Char('c') => picker.marked.clear(),
            KeyCode::Enter if picker.marked.is_empty() => {
                return Outcome::Edit(Box::new(picker.books[picker.cursor].clone()));
            }
            KeyCode::Enter => {
                picker.bulk = Some(Bulk { status: Status::Finished, date: String::new(), error: None });
            }
            _ => {}
        }
        Outcome::Continue
    }
}

pub fn draw(f: &mut Frame, app: &mut Books) {
    let screen = f.area();
    let area = popup(screen, screen.width.saturating_sub(4).min(90), screen.height.saturating_sub(2).min(26));
    f.render_widget(Clear, area);
    let super::Mode::Volumes(v) = &app.mode else { return };
    let s = &v.series;
    let title = format!(" {} ", s.title);
    let block = Block::bordered().title(title).border_style(ACCENT);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [about, _, body, hint] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0), Constraint::Length(2)])
            .areas(inner);
    let mut facts: Vec<String> = [s.publisher.clone(), s.author.clone()].into_iter().flatten().collect();
    if s.volumes > 0 {
        facts.push(format!("{} volumes{}", s.volumes, if s.complete { ", complete" } else { "" }));
    }
    f.render_widget(Line::from(facts.join(" · ")).dim(), about);

    let picker = match &v.state {
        State::Loading => return f.render_widget(Paragraph::new("Loading volumes…".dim()), body),
        State::Error(e) => {
            f.render_widget(Paragraph::new(e.as_str().fg(Color::Red)).wrap(Wrap { trim: true }), body);
            return f.render_widget(Line::from("esc back".dim()), hint);
        }
        State::Ready(p) => p,
    };
    let [list, preview] = Layout::horizontal([Constraint::Min(0), Constraint::Length(24)]).areas(body);
    let items: Vec<ListItem> = picker
        .books
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let mark = if v.is_owned(b) {
                "✓ ".fg(Color::Green)
            } else if picker.marked.contains(&i) {
                "■ ".fg(ACCENT)
            } else {
                "□ ".dim()
            };
            let mut line = vec![mark, Span::raw(b.title.clone())];
            if let Some(sub) = &b.subtitle {
                line.push(format!(" · {sub}").dim());
            }
            let facts: Vec<String> =
                [b.pages.map(|p| format!("{p} p.")), b.pub_year.map(|y| y.to_string())].into_iter().flatten().collect();
            if !facts.is_empty() {
                line.push(format!("  {}", facts.join(" · ")).dim());
            }
            if v.is_owned(b) {
                line.push(" in library".fg(Color::Green));
            }
            ListItem::new(Line::from(line))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(picker.cursor));
    f.render_stateful_widget(List::new(items).highlight_style(Style::new().bg(Color::DarkGray)), list, &mut state);
    let cover = picker.books.get(picker.cursor).and_then(|b| b.cover_url.clone());

    let lines = match &picker.bulk {
        Some(bulk) => {
            let n = picker.marked.len();
            let day = if bulk.date.is_empty() { format!("today ({})", format_date(chrono::Utc::now().timestamp_millis())) } else { String::new() };
            let finish = matches!(bulk.status, Status::Finished | Status::Abandoned);
            let mut first = vec![
                format!("Add {n} volume{} as ", if n == 1 { "" } else { "s" }).into(),
                format!("‹ {} ›", bulk.status.label()).fg(ACCENT).bold(),
            ];
            if finish {
                first.extend([
                    "  finished on ".into(),
                    bulk.date.clone().into(),
                    "█".into(),
                    day.dim(),
                ]);
            }
            let second = match &bulk.error {
                Some(e) => Line::from(e.as_str().fg(Color::Red)),
                None => Line::from("←→ status · type a date (YYYY-MM-DD) · enter add · esc back".dim()),
            };
            vec![Line::from(first), second]
        }
        None => {
            let marked = match picker.marked.len() {
                0 => String::new(),
                n => format!("{n} marked · "),
            };
            let note = picker.note.clone().map(|n| Line::from(n.fg(Color::Yellow)));
            vec![
                note.unwrap_or_default(),
                Line::from(format!("{marked}space mark · a mark up to here · c clear · enter add · esc back").dim()),
            ]
        }
    };
    f.render_widget(Paragraph::new(lines), hint);
    if let Some(url) = cover {
        super::ui::draw_cover(f, app, &url, preview.inner(Margin::new(1, 0)), false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub fn volume(n: u32) -> Book {
        let mut b = Book::new(format!("Berserk {n}"));
        b.series = Some("Berserk".into());
        b.volume = Some(n);
        b.olid = Some(format!("mp:1:{n}"));
        b
    }

    fn picker(owned: &[u32], want: Option<u32>) -> (Volumes, Option<Book>) {
        let mut v = Volumes::new(Series { id: 1, title: "Berserk".into(), ..Default::default() }, owned.iter().copied().collect(), want);
        let got = v.loaded(Ok((1..=5).map(volume).collect()));
        (v, got)
    }

    #[test]
    fn starts_after_what_is_owned_and_marks_up_to_the_cursor() {
        let (mut v, _) = picker(&[1, 2], None);
        assert_eq!(v.current().unwrap().volume, Some(3));
        v.handle_key(key(KeyCode::Char('j')));
        v.handle_key(key(KeyCode::Char('a')));
        let State::Ready(p) = &v.state else { panic!() };
        assert_eq!(p.marked.iter().copied().collect::<Vec<_>>(), [2, 3], "volumes 3 and 4, not the owned ones");
        v.handle_key(key(KeyCode::Enter));
        v.handle_key(key(KeyCode::Right)); // Finished -> Reading
        v.handle_key(key(KeyCode::Left));
        for c in "2026-01-05".chars() {
            v.handle_key(key(KeyCode::Char(c)));
        }
        let Outcome::Add { books, status, finished } = v.handle_key(key(KeyCode::Enter)) else { panic!() };
        assert_eq!(books.iter().map(|b| b.volume.unwrap()).collect::<Vec<_>>(), [3, 4]);
        assert_eq!(status, Status::Finished);
        assert_eq!(finished, parse_date("2026-01-05", "x").unwrap());
    }

    #[test]
    fn space_toggles_and_enter_alone_edits_one() {
        let (mut v, _) = picker(&[], None);
        v.handle_key(key(KeyCode::Char(' ')));
        v.handle_key(key(KeyCode::Char('k')));
        v.handle_key(key(KeyCode::Char(' ')));
        let State::Ready(p) = &v.state else { panic!() };
        assert!(p.marked.is_empty());
        let Outcome::Edit(book) = v.handle_key(key(KeyCode::Enter)) else { panic!() };
        assert_eq!(book.volume, Some(2), "space moved on to volume 2");
    }

    #[test]
    fn the_wanted_volume_goes_straight_to_the_form() {
        let (_, got) = picker(&[1, 2], Some(3));
        assert_eq!(got.unwrap().title, "Berserk 3");
        let (v, got) = picker(&[1, 2, 3, 4, 5], Some(6));
        assert!(got.is_none());
        let State::Ready(p) = &v.state else { panic!() };
        assert_eq!(p.note.as_deref(), Some("Volume 6 isn't out yet."));
    }
}
