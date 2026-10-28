//! Assigning inbox sessions (reading-habit sessions while several books are
//! being read) to a book, and pinning the book for the next session.

use habit_books::api::BooksOp;
use habit_books::stats::format_duration;
use habit_books::Book;
use chrono::{Local, TimeZone};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState};
use ratatui::Frame;

use super::ui::{popup, ACCENT};
use super::{Books, Mode};

/// The dialog for the oldest inbox session.
pub struct Assign {
    /// Index into [`Books::assign_books`].
    pub selected: usize,
    /// Page reached, as typed.
    pub page: String,
    pub error: Option<String>,
}

impl Books {
    /// Books to choose from: the ones being read, or every book if none is (never audiobooks).
    pub fn assign_books(&self) -> Vec<&Book> {
        let reading: Vec<&Book> = self.reading.iter().collect();
        if !reading.is_empty() {
            return reading;
        }
        self.all_books.iter().filter(|b| b.reads_with_pages()).collect()
    }

    /// Opens the dialog for the oldest waiting session, preselecting the likely book.
    pub fn open_assign(&mut self) {
        if self.pending.is_empty() {
            self.mode = Mode::Normal;
            return;
        }
        let suggested = self.suggested.clone();
        let selected = suggested.and_then(|id| self.assign_books().iter().position(|b| b.id == id)).unwrap_or(0);
        self.mode = Mode::Assign(Box::new(Assign { selected, page: String::new(), error: None }));
    }

    pub(super) fn assign_key(&mut self, mut a: Box<Assign>, key: KeyEvent) -> anyhow::Result<()> {
        let n = self.assign_books().len();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Ok(()),
            KeyCode::Char('x') if ctrl => {
                if let Some(p) = self.pending.first().cloned() {
                    self.call(BooksOp::Discard { pending: p.id })?;
                    self.set_message(format!("Discarded {}", format_duration(p.duration_ms)));
                }
                self.reload(None)?;
                self.open_assign();
                return Ok(());
            }
            KeyCode::Down | KeyCode::Char('j') if n > 0 => a.selected = (a.selected + 1) % n,
            KeyCode::Up | KeyCode::Char('k') if n > 0 => a.selected = (a.selected + n - 1) % n,
            KeyCode::Backspace => {
                a.page.pop();
            }
            KeyCode::Char(c) if c.is_ascii_digit() && !ctrl => a.page.push(c),
            KeyCode::Enter => {
                let (Some(p), Some(book)) = (self.pending.first().cloned(), self.assign_books().get(a.selected).map(|b| (*b).clone())) else {
                    return Ok(());
                };
                let page_to = if a.page.is_empty() { None } else { a.page.parse().ok() };
                self.call(BooksOp::Assign { pending: p.id, book: book.id, page_to })?;
                self.set_message(format!("Recorded {} for “{}”", format_duration(p.duration_ms), book.title));
                self.reload(None)?;
                self.open_assign();
                return Ok(());
            }
            _ => {}
        }
        self.mode = Mode::Assign(a);
        Ok(())
    }

    /// `m`: the reading session in progress (or the next one) is for the
    /// selected book; pressing it again on the same book unpins.
    pub(super) fn toggle_pin(&mut self) -> anyhow::Result<()> {
        let Some(book) = self.selected().cloned() else { return Ok(()) };
        if self.pin.as_ref().is_some_and(|p| p.book_id == book.id) {
            self.call(BooksOp::Pin { book: None })?;
            self.set_message("Unpinned: the book will be asked for at the end");
        } else {
            self.call(BooksOp::Pin { book: Some(book.id.clone()) })?;
            let when = if self.live.is_some() { "This" } else { "The next" };
            self.set_message(format!("{when} reading session counts for “{}”", book.title));
        }
        self.reload(None)
    }

    /// Title of the book the next reading session will go to, if decided.
    pub fn live_target(&self) -> Option<&str> {
        match (&self.pin, self.reading.as_slice()) {
            (Some(p), _) => Some(self.title_of(&p.book_id)),
            (None, [only]) => Some(only.title.as_str()),
            _ => None,
        }
    }
}

pub fn draw(f: &mut Frame, app: &Books, a: &Assign) {
    let Some(p) = app.pending.first() else { return };
    let books = app.assign_books();
    let height = (books.len() as u16 + 9).min(f.area().height.saturating_sub(2));
    let area = popup(f.area(), 64, height);
    f.render_widget(Clear, area);
    let more = if app.pending.len() > 1 { format!(" (1 of {})", app.pending.len()) } else { String::new() };
    let block = Block::bordered().title(format!(" Which book?{more} ")).border_style(ACCENT);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [head, _, list, _, page, hint] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    let time = |ms: i64| Local.timestamp_millis_opt(ms).single().map_or(String::new(), |t| t.format("%a %d.%m %H:%M").to_string());
    let end = Local.timestamp_millis_opt(p.ended_at).single().map_or(String::new(), |t| t.format("%H:%M").to_string());
    f.render_widget(
        Line::from(vec![
            Span::styled(format!("Read {}", format_duration(p.duration_ms)), Style::new().bold()),
            Span::raw(format!(", {}–{end}", time(p.started_at))),
            Span::styled(format!(" · {} · {}", p.source.as_str(), p.device), Style::new().dim()),
        ]),
        head,
    );

    let items: Vec<ListItem> = books
        .iter()
        .map(|b| {
            let progress = app
                .sessions
                .iter()
                .rev()
                .find(|s| s.book_id == b.id && s.page_to.is_some())
                .and_then(|s| s.page_to)
                .map(|p| format!("  p.{p}{}", b.pages.map(|t| format!("/{t}")).unwrap_or_default()))
                .unwrap_or_default();
            ListItem::new(Line::from(vec![Span::raw(b.title.as_str()), Span::styled(progress, Style::new().dim())]))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(a.selected));
    f.render_stateful_widget(
        List::new(items).highlight_style(Style::new().bg(Color::DarkGray).bold()).highlight_symbol("▌"),
        list,
        &mut state,
    );
    f.render_widget(
        Line::from(vec![Span::styled("Page reached (optional): ", Style::new().dim()), a.page.as_str().into(), "█".into()]),
        page,
    );
    let hint_line = match &a.error {
        Some(e) => Line::from(e.as_str().fg(Color::Red)),
        None => Line::from("j/k book · digits page · enter save · esc later · ctrl+x didn't read".dim()),
    };
    f.render_widget(hint_line, hint);
}

#[cfg(test)]
mod tests {
    use super::super::tests::{books_from, press};
    use super::*;
    use habit_books::{Db, Status};

    /// Two books being read and two sessions in the inbox.
    fn books() -> (Books, String, String) {
        let db = Db::open_in_memory().unwrap();
        let mut ids = Vec::new();
        for title in ["Dune", "Emma"] {
            let mut b = Book::new(title);
            b.status = Status::Reading;
            db.save_book(&mut b).unwrap();
            ids.push(b.id);
        }
        for (i, minutes) in [(1, 25), (2, 40)] {
            db.record_habit_session("book", i * 3_600_000, i * 3_600_000 + minutes * 60_000, minutes * 60_000).unwrap();
        }
        db.set_pin(Some(&ids[1])).unwrap();
        (books_from(db), ids[0].clone(), ids[1].clone())
    }

    #[test]
    fn assigns_inbox_one_by_one_with_suggestion_and_page() {
        let (mut books, dune, emma) = books();
        press(&mut books, "I");
        let Mode::Assign(a) = &books.mode else { panic!("no dialog") };
        // The pinned book is suggested.
        assert_eq!(books.assign_books()[a.selected].id, emma);
        press(&mut books, "k42\n");
        let s = books.sessions.iter().find(|s| s.book_id == dune).unwrap();
        assert_eq!((s.duration_ms, s.page_to), (25 * 60_000, Some(42)));
        // The next one follows; ctrl+x discards it and the dialog closes.
        assert!(matches!(books.mode, Mode::Assign(_)));
        books.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)).unwrap();
        assert!(books.pending.is_empty());
        assert!(matches!(books.mode, Mode::Normal));
        assert_eq!(books.sessions.len(), 1);
    }

    #[test]
    fn esc_keeps_sessions_for_later() {
        let (mut books, ..) = books();
        press(&mut books, "I\x1b");
        assert!(matches!(books.mode, Mode::Normal));
        assert_eq!(books.pending.len(), 2);
    }

    #[test]
    fn m_pins_and_unpins() {
        let (mut books, _, emma) = books();
        let at = books.books.iter().position(|b| b.id == emma).unwrap();
        books.table.select(Some(at));
        assert_eq!(books.live_target(), Some("Emma"));
        press(&mut books, "m");
        assert!(books.pin.is_none(), "m on the pinned book unpins");
        assert_eq!(books.live_target(), None, "two books being read: undecided");
        press(&mut books, "m");
        assert_eq!(books.pin.as_ref().map(|p| p.book_id.as_str()), Some(emma.as_str()));
    }

    #[test]
    fn dialog_renders() {
        let (mut books, ..) = books();
        press(&mut books, "I");
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        term.draw(|f| super::super::ui::draw(f, f.area(), &mut books)).unwrap();
        let screen: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        for text in ["Which book? (1 of 2)", "Read 25m", "habitfocus · test", "Dune", "Emma", "inbox 2", "Page reached"] {
            assert!(screen.contains(text), "missing {text}");
        }
    }
}
