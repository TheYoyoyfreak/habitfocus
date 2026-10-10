//! Reading sessions in the Books pane: logging a past session, the page
//! reached, and the session list.

use chrono::{Local, NaiveDateTime, TimeZone};
use habit_books::api::BooksOp;
use habit_books::stats::{self, format_duration, BookStats};
use habit_books::Session;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{Books, Mode};

/// "Log a session" form: how long, which page, when it ended.
pub struct LogForm {
    pub book_id: String,
    pub title: String,
    /// Minutes, page reached, end time.
    pub fields: [String; 3],
    pub focus: usize,
    pub error: Option<String>,
}

pub const LOG_LABELS: [&str; 3] = ["Minutes", "Page", "Ended at"];

impl LogForm {
    /// The `log` request for what was typed.
    fn op(&self) -> Result<BooksOp, String> {
        let minutes: u32 = self.fields[0].trim().parse().ok().filter(|m| *m > 0).ok_or("Minutes must be a positive number")?;
        let page_to = parse_page(&self.fields[1])?;
        let naive = NaiveDateTime::parse_from_str(self.fields[2].trim(), "%Y-%m-%d %H:%M")
            .map_err(|_| "Ended at must look like 2026-10-07 20:30")?;
        let ended_at = Local.from_local_datetime(&naive).earliest().ok_or("No such local time")?.timestamp_millis();
        let started_at = ended_at - i64::from(minutes) * 60_000;
        Ok(BooksOp::Log { book: self.book_id.clone(), started_at, ended_at, page_to })
    }
}

fn parse_page(s: &str) -> Result<Option<u32>, &'static str> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    s.parse().map(Some).map_err(|_| "Page must be a whole number")
}

/// Shared text editing for the small inputs here.
fn edit(buf: &mut String, key: KeyEvent) {
    match key.code {
        KeyCode::Backspace => {
            buf.pop();
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => buf.clear(),
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => buf.push(c),
        _ => {}
    }
}

impl Books {
    /// Stats for one book from the loaded sessions.
    pub fn book_stats(&self, book_id: &str, pages: Option<u32>) -> BookStats {
        let sessions: Vec<Session> = self.sessions.iter().filter(|s| s.book_id == book_id).cloned().collect();
        stats::book_stats(pages, &sessions)
    }

    pub fn title_of(&self, book_id: &str) -> &str {
        self.titles.get(book_id).map_or("?", String::as_str)
    }

    pub(super) fn set_page_key(&mut self, mut page: String, key: KeyEvent) -> anyhow::Result<()> {
        match key.code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                let Some(book) = self.selected().cloned() else { return Ok(()) };
                match parse_page(&page) {
                    Ok(Some(p)) => {
                        let message = match self.call(BooksOp::SetPage { book: book.id, page: p }) {
                            Ok(reply) => match reply.session.and_then(|s| s.pages_read()) {
                                Some(n) => format!("Page {p}: {n} pages in the last session"),
                                None => format!("Page {p}"),
                            },
                            Err(_) => "No sessions yet: read with a reading habit, or g logs one".into(),
                        };
                        self.set_message(message);
                        self.reload(None)?;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        self.message = Some((e.into(), true));
                        self.mode = Mode::SetPage(page);
                    }
                }
            }
            _ => {
                edit(&mut page, key);
                self.mode = Mode::SetPage(page);
            }
        }
        Ok(())
    }

    pub(super) fn open_log(&mut self) {
        let Some(book) = self.selected() else { return };
        let now = Local::now().format("%Y-%m-%d %H:%M").to_string();
        self.mode = Mode::Log(Box::new(LogForm {
            book_id: book.id.clone(),
            title: book.title.clone(),
            fields: [String::new(), String::new(), now],
            focus: 0,
            error: None,
        }));
    }

    pub(super) fn log_key(&mut self, mut form: Box<LogForm>, key: KeyEvent) -> anyhow::Result<()> {
        match key.code {
            KeyCode::Esc => return Ok(()),
            KeyCode::Tab | KeyCode::Down => form.focus = (form.focus + 1) % 3,
            KeyCode::BackTab | KeyCode::Up => form.focus = (form.focus + 2) % 3,
            KeyCode::Enter => match form.op() {
                Ok(op) => {
                    let duration = match &op {
                        BooksOp::Log { started_at, ended_at, .. } => ended_at - started_at,
                        _ => 0,
                    };
                    match self.call(op) {
                        Ok(_) => {
                            self.set_message(format!("Logged {} of “{}”", format_duration(duration), form.title));
                            return self.reload(None);
                        }
                        Err(e) => form.error = Some(format!("{e:#}")),
                    }
                }
                Err(e) => form.error = Some(e),
            },
            _ => edit(&mut form.fields[form.focus], key),
        }
        self.mode = Mode::Log(form);
        Ok(())
    }

    /// The selected book's sessions, newest first.
    pub fn selected_sessions(&self) -> Vec<&Session> {
        let Some(book) = self.selected() else { return Vec::new() };
        self.sessions.iter().rev().filter(|s| s.book_id == book.id).collect()
    }

    pub(super) fn sessions_key(&mut self, mut selected: usize, confirm: bool, key: KeyEvent) -> anyhow::Result<()> {
        let len = self.selected_sessions().len();
        if confirm {
            if key.code == KeyCode::Char('y') {
                if let Some(id) = self.selected_sessions().get(selected).map(|s| s.id.clone()) {
                    self.call(BooksOp::DeleteSession { id })?;
                    self.set_message("Session deleted");
                    self.reload(None)?;
                }
            }
            self.mode = Mode::Sessions { selected: selected.min(len.saturating_sub(2)), confirm_delete: false };
            return Ok(());
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('v') | KeyCode::Char('q') => return Ok(()),
            KeyCode::Char('j') | KeyCode::Down => selected = (selected + 1).min(len.saturating_sub(1)),
            KeyCode::Char('k') | KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Char('d') if len > 0 => {
                self.mode = Mode::Sessions { selected, confirm_delete: true };
                return Ok(());
            }
            _ => {}
        }
        self.mode = Mode::Sessions { selected, confirm_delete: false };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{books_from, key, press};
    use super::*;
    use habit_books::{Book, Db, Status};

    fn books_with_book() -> Books {
        let db = Db::open_in_memory().unwrap();
        let mut b = Book::new("Dune");
        b.pages = Some(600);
        b.status = Status::Reading;
        db.save_book(&mut b).unwrap();
        books_from(db)
    }

    #[test]
    fn log_session_and_delete_it() {
        let mut books = books_with_book();
        press(&mut books, "g45\t120\n");
        assert!(matches!(books.mode, Mode::Normal), "{:?}", books.message);
        assert_eq!(books.sessions.len(), 1);
        assert_eq!(books.sessions[0].duration_ms, 45 * 60_000);
        let st = books.book_stats(&books.selected().unwrap().id.clone(), Some(600));
        assert_eq!((st.current_page, st.pages_read), (Some(120), 120));

        press(&mut books, "vdn");
        assert_eq!(books.sessions.len(), 1);
        press(&mut books, "dy");
        assert!(books.sessions.is_empty());
    }

    #[test]
    fn log_rejects_bad_minutes() {
        let mut books = books_with_book();
        press(&mut books, "g0\n");
        let Mode::Log(form) = &books.mode else { panic!() };
        assert_eq!(form.error.as_deref(), Some("Minutes must be a positive number"));
    }

    #[test]
    fn p_sets_page_of_latest_session() {
        let mut books = books_with_book();
        press(&mut books, "g30\n");
        press(&mut books, "p77\n");
        assert_eq!(books.sessions[0].page_to, Some(77));
        assert_eq!(books.message.as_ref().map(|m| m.0.as_str()), Some("Page 77: 77 pages in the last session"));
        books.handle_key(key(KeyCode::Char('p'))).unwrap();
        press(&mut books, "x\n");
        assert!(matches!(books.mode, Mode::SetPage(_)), "a bad page keeps the prompt open");
    }
}
