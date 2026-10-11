//! Add/edit form for a book. Text fields are edited at the end of the line;
//! choice fields (format, status, rating) cycle with ←/→ or space.

use chrono::{Local, NaiveDate, NaiveTime, TimeZone};
use habit_books::{Book, Format, Status};
use habit_core::config::RatingScale;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Title,
    Subtitle,
    Author,
    Series,
    Volume,
    Pages,
    Year,
    Isbn,
    Format,
    Status,
    Started,
    Finished,
    Rating,
    Tags,
    Cover,
    Notes,
}

impl Field {
    pub const ALL: [Field; 16] = [
        Field::Title,
        Field::Subtitle,
        Field::Author,
        Field::Series,
        Field::Volume,
        Field::Pages,
        Field::Year,
        Field::Isbn,
        Field::Format,
        Field::Status,
        Field::Started,
        Field::Finished,
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
            Field::Series => "Series",
            Field::Volume => "Volume",
            Field::Pages => "Pages",
            Field::Year => "Year",
            Field::Isbn => "ISBN",
            Field::Format => "Format",
            Field::Status => "Status",
            Field::Started => "Started",
            Field::Finished => "Finished",
            Field::Rating => "Rating",
            Field::Tags => "Tags",
            Field::Cover => "Cover",
            Field::Notes => "Notes",
        }
    }

    /// Shown dimmed while the field is empty.
    pub fn placeholder(self) -> &'static str {
        match self {
            Field::Started | Field::Finished => "YYYY-MM-DD",
            Field::Series => "e.g. Chainsaw Man (manga: from the title)",
            Field::Cover => "web address, or an image file to use (it syncs)",
            _ => "",
        }
    }

    pub fn is_choice(self) -> bool {
        matches!(self, Field::Format | Field::Status | Field::Rating)
    }
}

/// The Cover field while it holds an image of the book's own.
const OWN_IMAGE: &str = "(own image)";

/// When a book was started and (maybe) finished, as `BooksOp::SetDates` takes it.
pub type Dates = (i64, Option<i64>);

pub enum FormOutcome {
    Continue,
    Cancel,
    /// The book, and its reading dates when they were changed.
    Save(Box<Book>, Option<Dates>),
}

pub struct Form {
    pub is_new: bool,
    book: Book,
    /// One buffer per entry in `Field::ALL`; unused for choice fields.
    text: [String; 16],
    /// Dates as the form opened with them, to tell whether they changed.
    dates_before: (String, String),
    pub scale: RatingScale,
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
            Field::Series => opt(&book.series),
            Field::Volume => num(book.volume.map(|v| v.to_string())),
            Field::Pages => num(book.pages.map(|p| p.to_string())),
            Field::Year => num(book.pub_year.map(|y| y.to_string())),
            Field::Isbn => opt(&book.isbn),
            Field::Tags => book.tags.join(", "),
            // An own image is a long data: URL; it stays unless this is changed.
            Field::Cover if book.cover_url.as_deref().is_some_and(|u| u.starts_with("data:")) => OWN_IMAGE.into(),
            Field::Cover => opt(&book.cover_url),
            Field::Notes => opt(&book.notes),
            Field::Format | Field::Status | Field::Rating | Field::Started | Field::Finished => String::new(),
        });
        Form { is_new, book, text, dates_before: Default::default(), scale: RatingScale::Stars, focus: 0, error: None }
    }

    /// How ratings are shown and entered.
    pub fn with_scale(mut self, scale: RatingScale) -> Form {
        self.scale = scale;
        self
    }

    /// When the book was started and finished, as known so far.
    pub fn with_dates(mut self, started: Option<i64>, finished: Option<i64>) -> Form {
        let started = started.map(format_date).unwrap_or_default();
        let finished = finished.map(format_date).unwrap_or_default();
        self.text[Field::Started as usize] = started.clone();
        self.text[Field::Finished as usize] = finished.clone();
        self.dates_before = (started, finished);
        self
    }

    pub fn field(&self) -> Field {
        Field::ALL[self.focus]
    }

    /// Display value of a field.
    pub fn value(&self, i: usize) -> String {
        match Field::ALL[i] {
            Field::Format => self.book.format.as_str().to_owned(),
            Field::Status => self.book.status.label().to_owned(),
            Field::Rating => rating_label(self.book.score, self.scale),
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
            Field::Rating => {
                self.book.score = match self.scale {
                    RatingScale::Points => (i32::from(self.book.score) + step).rem_euclid(11) as u8,
                    // Whole stars; a half star (from points) rounds up first.
                    RatingScale::Stars => (i32::from(self.book.score.div_ceil(2)) + step).rem_euclid(6) as u8 * 2,
                }
            }
            _ => {}
        }
    }

    fn submit(&mut self) -> FormOutcome {
        match self.build().and_then(|book| Ok((self.dates(&book)?, book))) {
            Ok((dates, book)) => FormOutcome::Save(Box::new(book), dates),
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
        book.series = opt(Field::Series);
        book.volume = opt(Field::Volume).map(|s| s.parse().map_err(|_| "Volume must be a whole number")).transpose()?;
        book.pages = opt(Field::Pages).map(|s| s.parse().map_err(|_| "Pages must be a whole number")).transpose()?;
        book.pub_year = opt(Field::Year).map(|s| s.parse().map_err(|_| "Year must be a number")).transpose()?;
        book.isbn = opt(Field::Isbn);
        book.tags = get(Field::Tags)
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .collect();
        if get(Field::Cover) != OWN_IMAGE {
            book.cover_url = opt(Field::Cover);
        }
        book.notes = opt(Field::Notes);
        Ok(book)
    }

    /// The reading dates to set, if they were changed and the book has been read.
    fn dates(&self, book: &Book) -> Result<Option<Dates>, String> {
        let started = self.text[Field::Started as usize].trim();
        let finished = self.text[Field::Finished as usize].trim();
        if (started, finished) == (self.dates_before.0.trim(), self.dates_before.1.trim()) || book.status == Status::ForLater {
            return Ok(None);
        }
        let started = parse_date(started, "Started")?;
        let finished = parse_date(finished, "Finished")?;
        Ok(match (started, finished) {
            (None, None) => None,
            // Only the finish is known: say it was read that day.
            (started, finished) => {
                let started = started.or(finished).expect("one is set");
                if finished.is_some_and(|f| f < started) {
                    return Err("Finished can't be before Started".into());
                }
                Some((started, finished))
            }
        })
    }
}

/// A date as the form shows it.
pub fn format_date(ms: i64) -> String {
    Local.timestamp_millis_opt(ms).single().map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default()
}

/// Noon of a typed `YYYY-MM-DD`, so the day is the same in every time zone nearby.
pub fn parse_date(s: &str, label: &str) -> Result<Option<i64>, String> {
    if s.is_empty() {
        return Ok(None);
    }
    let day = NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| format!("{label} must look like 2026-10-07"))?;
    let noon = day.and_time(NaiveTime::from_hms_opt(12, 0, 0).expect("valid"));
    Local.from_local_datetime(&noon).earliest().map(|d| Some(d.timestamp_millis())).ok_or(format!("{label}: no such day"))
}

/// A score out of 10 as stars (halves included) or points; "–" when unrated.
pub fn rating_label(score: u8, scale: RatingScale) -> String {
    let score = score.min(10);
    match (score, scale) {
        (0, _) => "–".into(),
        (_, RatingScale::Points) => format!("{score}/10"),
        (_, RatingScale::Stars) => {
            let full = usize::from(score / 2);
            let half = score % 2 == 1;
            "★".repeat(full) + if half { "½" } else { "" } + &"☆".repeat(5 - full - usize::from(half))
        }
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
        let FormOutcome::Save(book, dates) = form.handle_key(key(KeyCode::Enter)) else { panic!("not saved") };
        assert_eq!(dates, None);
        assert_eq!(book.title, "Dune");
        assert_eq!(book.pages, Some(612));
        assert_eq!(book.status, Status::Finished);
        assert_eq!(book.score, 10);
        assert_eq!(book.tags, ["scifi", "classic"]);
        assert_eq!(book.author, None);
        assert_eq!((book.series, book.volume), (None, None));
    }

    #[test]
    fn series_and_volume_are_fields() {
        let mut book = Book::new("Berserk 3");
        book.series = Some("Berserk".into());
        book.volume = Some(3);
        let mut form = Form::new(book, false);
        assert_eq!(form.value(Field::Series as usize), "Berserk");
        focus(&mut form, Field::Volume);
        form.handle_key(key(KeyCode::Backspace));
        type_str(&mut form, "4");
        let FormOutcome::Save(saved, _) = form.handle_key(key(KeyCode::Enter)) else { panic!() };
        assert_eq!((saved.series.as_deref(), saved.volume), (Some("Berserk"), Some(4)));
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
    fn ratings_follow_the_scale() {
        assert_eq!(rating_label(0, RatingScale::Stars), "–");
        assert_eq!(rating_label(7, RatingScale::Stars), "★★★½☆");
        assert_eq!(rating_label(10, RatingScale::Stars), "★★★★★");
        assert_eq!(rating_label(7, RatingScale::Points), "7/10");

        let mut book = Book::new("Dune");
        book.score = 7;
        let mut form = Form::new(book.clone(), false).with_scale(RatingScale::Points);
        focus(&mut form, Field::Rating);
        form.handle_key(key(KeyCode::Right));
        assert_eq!(form.value(Field::Rating as usize), "8/10");
        let mut form = Form::new(book, false);
        focus(&mut form, Field::Rating);
        form.handle_key(key(KeyCode::Right)); // 3.5 stars -> 4 -> 5
        assert_eq!(form.value(Field::Rating as usize), "★★★★★");
        form.handle_key(key(KeyCode::Right));
        assert_eq!(form.value(Field::Rating as usize), "–");
    }

    #[test]
    fn changed_dates_are_saved_with_the_book() {
        let mut book = Book::new("Dune");
        book.status = Status::Finished;
        let started = parse_date("2026-03-01", "x").unwrap().unwrap();
        let mut form = Form::new(book.clone(), false).with_dates(Some(started), None);
        assert_eq!(form.value(Field::Started as usize), "2026-03-01");
        // Untouched: nothing to set.
        let FormOutcome::Save(_, dates) = form.handle_key(key(KeyCode::Enter)) else { panic!() };
        assert_eq!(dates, None);

        focus(&mut form, Field::Finished);
        type_str(&mut form, "2026-03-20");
        let FormOutcome::Save(_, dates) = form.handle_key(key(KeyCode::Enter)) else { panic!() };
        let finished = parse_date("2026-03-20", "x").unwrap();
        assert_eq!(dates, Some((started, finished)));

        // Only a finish: started that day too. A finish before the start is refused.
        let mut form = Form::new(book.clone(), true);
        focus(&mut form, Field::Finished);
        type_str(&mut form, "2026-03-20");
        let FormOutcome::Save(_, dates) = form.handle_key(key(KeyCode::Enter)) else { panic!() };
        assert_eq!(dates, Some((finished.unwrap(), finished)));
        focus(&mut form, Field::Started);
        type_str(&mut form, "2026-04-01");
        assert!(matches!(form.handle_key(key(KeyCode::Enter)), FormOutcome::Continue));
        assert_eq!(form.error.as_deref(), Some("Finished can't be before Started"));
        focus(&mut form, Field::Started);
        form.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_str(&mut form, "soon");
        assert!(matches!(form.handle_key(key(KeyCode::Enter)), FormOutcome::Continue));
        assert_eq!(form.error.as_deref(), Some("Started must look like 2026-10-07"));
    }

    #[test]
    fn own_cover_images_stay_unless_replaced() {
        let mut book = Book::new("Dune");
        book.cover_url = Some("data:image/jpeg;base64,AAAA".into());
        let mut form = Form::new(book.clone(), false);
        assert_eq!(form.value(Field::Cover as usize), OWN_IMAGE);
        let FormOutcome::Save(saved, _) = form.handle_key(key(KeyCode::Enter)) else { panic!() };
        assert_eq!(saved.cover_url, book.cover_url);
        focus(&mut form, Field::Cover);
        form.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_str(&mut form, "~/covers/dune.jpg");
        let FormOutcome::Save(saved, _) = form.handle_key(key(KeyCode::Enter)) else { panic!() };
        assert_eq!(saved.cover_url.as_deref(), Some("~/covers/dune.jpg"));
    }

    #[test]
    fn typing_on_choice_fields_is_ignored() {
        let mut form = Form::new(Book::new("Dune"), false);
        focus(&mut form, Field::Format);
        type_str(&mut form, "xyz");
        assert_eq!(form.value(Field::Format as usize), "paperback");
    }
}
