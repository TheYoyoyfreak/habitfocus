//! Add/edit form for a book. Text fields are edited at the end of the line;
//! choice fields (format, status, rating) cycle with ←/→ or space.

use habit_books::{Book, Format, Status};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Title,
    Subtitle,
    Author,
    Pages,
    Year,
    Isbn,
    Format,
    Status,
    Rating,
    Tags,
    Cover,
    Notes,
}

impl Field {
    pub const ALL: [Field; 12] = [
        Field::Title,
        Field::Subtitle,
        Field::Author,
        Field::Pages,
        Field::Year,
        Field::Isbn,
        Field::Format,
        Field::Status,
        Field::Rating,
        Field::Tags,
        Field::Cover,
        Field::Notes,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Field::Title => "Title",
            Field::Subtitle => "Subtitle",
            Field::Author => "Author",
            Field::Pages => "Pages",
            Field::Year => "Year",
            Field::Isbn => "ISBN",
            Field::Format => "Format",
            Field::Status => "Status",
            Field::Rating => "Rating",
            Field::Tags => "Tags",
            Field::Cover => "Cover",
            Field::Notes => "Notes",
        }
    }

    pub fn is_choice(self) -> bool {
        matches!(self, Field::Format | Field::Status | Field::Rating)
    }
}

pub enum FormOutcome {
    Continue,
    Cancel,
    Save(Box<Book>),
}

pub struct Form {
    pub is_new: bool,
    book: Book,
    /// One buffer per entry in `Field::ALL`; unused for choice fields.
    text: [String; 12],
    pub focus: usize,
    pub error: Option<String>,
}

impl Form {
    pub fn new(book: Book, is_new: bool) -> Form {
        let opt = |s: &Option<String>| s.clone().unwrap_or_default();
        let num = |n: Option<String>| n.unwrap_or_default();
        let text = Field::ALL.map(|f| match f {
            Field::Title => book.title.clone(),
            Field::Subtitle => opt(&book.subtitle),
            Field::Author => opt(&book.author),
            Field::Pages => num(book.pages.map(|p| p.to_string())),
            Field::Year => num(book.pub_year.map(|y| y.to_string())),
            Field::Isbn => opt(&book.isbn),
            Field::Tags => book.tags.join(", "),
            Field::Cover => opt(&book.cover_url),
            Field::Notes => opt(&book.notes),
            Field::Format | Field::Status | Field::Rating => String::new(),
        });
        Form { is_new, book, text, focus: 0, error: None }
    }

    pub fn field(&self) -> Field {
        Field::ALL[self.focus]
    }

    /// Display value of a field.
    pub fn value(&self, i: usize) -> String {
        match Field::ALL[i] {
            Field::Format => self.book.format.as_str().to_owned(),
            Field::Status => self.book.status.label().to_owned(),
            Field::Rating => stars(self.book.rating),
            _ => self.text[i].clone(),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> FormOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return FormOutcome::Cancel,
            KeyCode::Enter => return self.submit(),
            KeyCode::Char('s') if ctrl => return self.submit(),
            KeyCode::Tab | KeyCode::Down => self.focus = (self.focus + 1) % Field::ALL.len(),
            KeyCode::BackTab | KeyCode::Up => self.focus = (self.focus + Field::ALL.len() - 1) % Field::ALL.len(),
            KeyCode::Left if self.field().is_choice() => self.cycle(-1),
            KeyCode::Right | KeyCode::Char(' ') if self.field().is_choice() => self.cycle(1),
            KeyCode::Char('u') if ctrl => self.text[self.focus].clear(),
            KeyCode::Backspace => {
                self.text[self.focus].pop();
            }
            KeyCode::Char(c) if !ctrl && !self.field().is_choice() => self.text[self.focus].push(c),
            _ => {}
        }
        FormOutcome::Continue
    }

    fn cycle(&mut self, step: i32) {
        fn next<T: Copy + PartialEq>(all: &[T], cur: T, step: i32) -> T {
            let i = all.iter().position(|v| *v == cur).unwrap_or(0) as i32;
            all[(i + step).rem_euclid(all.len() as i32) as usize]
        }
        match self.field() {
            Field::Format => self.book.format = next(&Format::ALL, self.book.format, step),
            Field::Status => self.book.status = next(&Status::ALL, self.book.status, step),
            Field::Rating => self.book.rating = (self.book.rating as i32 + step).rem_euclid(6) as u8,
            _ => {}
        }
    }

    fn submit(&mut self) -> FormOutcome {
        match self.build() {
            Ok(book) => FormOutcome::Save(Box::new(book)),
            Err(e) => {
                self.error = Some(e);
                FormOutcome::Continue
            }
        }
    }

    fn build(&self) -> Result<Book, String> {
        let get = |f: Field| self.text[f as usize].trim();
        let opt = |f: Field| Some(get(f)).filter(|s| !s.is_empty()).map(str::to_owned);
        let mut book = self.book.clone();
        book.title = get(Field::Title).to_owned();
        if book.title.is_empty() {
            return Err("Title is required".into());
        }
        book.subtitle = opt(Field::Subtitle);
        book.author = opt(Field::Author);
        book.pages = opt(Field::Pages).map(|s| s.parse().map_err(|_| "Pages must be a whole number")).transpose()?;
        book.pub_year = opt(Field::Year).map(|s| s.parse().map_err(|_| "Year must be a number")).transpose()?;
        book.isbn = opt(Field::Isbn);
        book.tags = get(Field::Tags)
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .collect();
        book.cover_url = opt(Field::Cover);
        book.notes = opt(Field::Notes);
        Ok(book)
    }
}

pub fn stars(rating: u8) -> String {
    if rating == 0 {
        "–".into()
    } else {
        "★".repeat(rating as usize) + &"☆".repeat(5 - rating as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(form: &mut Form, s: &str) {
        for c in s.chars() {
            form.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn focus(form: &mut Form, field: Field) {
        while form.field() != field {
            form.handle_key(key(KeyCode::Tab));
        }
    }

    #[test]
    fn builds_book_from_typed_fields() {
        let mut form = Form::new(Book::new(""), true);
        type_str(&mut form, "Dune");
        focus(&mut form, Field::Pages);
        type_str(&mut form, "612");
        focus(&mut form, Field::Status);
        form.handle_key(key(KeyCode::Left)); // ForLater -> Finished
        focus(&mut form, Field::Rating);
        form.handle_key(key(KeyCode::Left)); // 0 -> 5
        focus(&mut form, Field::Tags);
        type_str(&mut form, "scifi, , classic");
        let FormOutcome::Save(book) = form.handle_key(key(KeyCode::Enter)) else { panic!("not saved") };
        assert_eq!(book.title, "Dune");
        assert_eq!(book.pages, Some(612));
        assert_eq!(book.status, Status::Finished);
        assert_eq!(book.rating, 5);
        assert_eq!(book.tags, ["scifi", "classic"]);
        assert_eq!(book.author, None);
    }

    #[test]
    fn rejects_missing_title_and_bad_numbers() {
        let mut form = Form::new(Book::new(""), true);
        assert!(matches!(form.handle_key(key(KeyCode::Enter)), FormOutcome::Continue));
        assert_eq!(form.error.as_deref(), Some("Title is required"));
        type_str(&mut form, "Dune");
        focus(&mut form, Field::Pages);
        type_str(&mut form, "lots");
        assert!(matches!(form.handle_key(key(KeyCode::Enter)), FormOutcome::Continue));
        assert_eq!(form.error.as_deref(), Some("Pages must be a whole number"));
    }

    #[test]
    fn typing_on_choice_fields_is_ignored() {
        let mut form = Form::new(Book::new("Dune"), false);
        focus(&mut form, Field::Format);
        type_str(&mut form, "xyz");
        assert_eq!(form.value(Field::Format as usize), "paperback");
    }
}
