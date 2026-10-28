//! Drawing the Books pane into its area; dialogs are centred on the screen.
//! Mutates only the table scroll state and the picture caches.

use habit_books::stats::format_duration;
use habit_books::Status;
use chrono::{Local, TimeZone};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Padding, Clear, List, ListItem, Paragraph, Row, Table, Tabs, Wrap};
use ratatui::Frame;

use ratatui_image::Resize;

use super::cover::{self, Cover, CoverMode, PixelArt};
use super::form::{rating_label, Field, Form};
use super::reading::{LogForm, LOG_LABELS};
use super::{inbox, insights, tab_label, Books, Mode, Online, OnlineState, TABS};
use habit_books::series::{self, Row as SeriesRow};

pub const ACCENT: Color = Color::Cyan;

pub fn draw(f: &mut Frame, area: Rect, app: &mut Books) {
    app.graphic.new_frame();
    app.pixel.new_frame();
    app.request_cover();
    let filter = matches!(app.mode, Mode::Search) || !app.query.is_empty();
    let [tabs, hint, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(u16::from(app.hint.is_some())),
        Constraint::Min(0),
        Constraint::Length(u16::from(filter)),
    ])
    .areas(area);
    draw_tabs(f, app, tabs);
    if let Some(text) = &app.hint {
        f.render_widget(Line::from(format!(" {text} ")).style(Style::new().fg(Color::Black).bg(Color::Yellow)), hint);
    }
    if app.insights {
        insights::draw(f, app, body);
    } else {
        let [list, detail] = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(body);
        if app.grid {
            draw_grid(f, app, list);
        } else {
            draw_table(f, app, list);
        }
        draw_detail(f, app, detail);
    }
    if filter {
        draw_filter(f, app, footer);
    }

    match &app.mode {
        Mode::Form(form) => draw_form(f, form),
        Mode::StatusPicker(i) => draw_status_picker(f, *i),
        Mode::ConfirmDelete => {
            if let Some(book) = app.selected() {
                let area = popup(f.area(), 44, 3);
                f.render_widget(Clear, area);
                f.render_widget(
                    Paragraph::new(format!("Delete “{}”? (y/n)", book.title))
                        .block(Block::bordered().border_style(Color::Red)),
                    area,
                );
            }
        }
        Mode::Online(_) => draw_online(f, app),
        Mode::Volumes(_) => super::volumes::draw(f, app),
        Mode::SetPage(page) => draw_set_page(f, app, page),
        Mode::Log(form) => draw_log(f, form),
        Mode::Assign(a) => inbox::draw(f, app, a),
        Mode::Sessions { selected, confirm_delete } => draw_sessions(f, app, *selected, *confirm_delete),
        Mode::Help => draw_help(f),
        Mode::Normal | Mode::Search => {}
    }
}

fn draw_tabs(f: &mut Frame, app: &Books, area: Rect) {
    let titles = TABS.iter().zip(app.counts).map(|(tab, n)| format!("{}·{n}", tab_label(*tab))).chain(["Insights".to_owned()]);
    let selected = if app.insights { TABS.len() } else { app.tab };
    let live = match &app.live {
        Some(live) => {
            let book = app.live_target().unwrap_or("which book? m");
            let paused = if live.running { "" } else { " (paused)" };
            format!(" ◉ {} · {book}{paused} ", live.habit)
        }
        None => match app.pin.as_ref() {
            Some(pin) => format!(" ◉ next: {} ", app.title_of(&pin.book_id)),
            None => String::new(),
        },
    };
    let inbox = if app.pending.is_empty() { String::new() } else { format!(" inbox {} ", app.pending.len()) };
    let [tabs, inbox_area, live_area] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(inbox.chars().count() as u16 + 1),
        Constraint::Length(live.chars().count().min(44) as u16),
    ])
    .areas(area);
    f.render_widget(Span::styled(inbox, Style::new().fg(Color::Black).bg(Color::Magenta)), inbox_area);
    f.render_widget(
        Tabs::new(titles).select(selected).highlight_style(Style::new().fg(ACCENT).bold().underlined()),
        tabs,
    );
    f.render_widget(Span::styled(live, Style::new().fg(Color::Black).bg(Color::Yellow)), live_area);
}

/// The library's frame; `None` once it said that there's nothing to list.
fn library_block(f: &mut Frame, app: &Books, area: Rect) -> Option<Block<'static>> {
    let block = Block::bordered().title(format!(" {} ", tab_label(app.tab_status())));
    if app.books.is_empty() {
        let hint = if app.query.is_empty() { "No books here yet. n adds one, O finds one online." } else { "No matches." };
        f.render_widget(Paragraph::new(hint).dim().block(block), area);
        return None;
    }
    Some(block)
}

fn draw_table(f: &mut Frame, app: &mut Books, area: Rect) {
    let Some(block) = library_block(f, app, area) else { return };
    let rows: Vec<Row<'static>> = app.rows.iter().map(|row| {
        let (index, nested) = match row {
            SeriesRow::Book { index, nested } => (*index, *nested),
            SeriesRow::Series(s) => {
                let fold = if s.expanded { "▾ " } else { "▸ " };
                let author = app.series_current(s).and_then(|b| b.author.clone()).unwrap_or_default();
                let range = series::range_label(s.first, s.last);
                return Row::new([
                    Cell::from(Line::from(vec![
                        fold.fg(ACCENT),
                        Span::from(s.name.clone()).bold(),
                        format!("  {range}").dim(),
                    ])),
                    Cell::from(author).style(Style::new().fg(Color::Gray)),
                    Cell::from(format!("{}/{} ✓", s.finished, s.owned)).style(Color::Green),
                ]);
            }
        };
        let b = &app.books[index];
        let mark = if app.pin.as_ref().is_some_and(|p| p.book_id == b.id) {
            Span::from("◉ ").fg(Color::Yellow)
        } else {
            Span::raw("")
        };
        let indent = if nested { Span::from("  └ ").dim() } else { Span::raw("") };
        let title = match (nested, b.volume) {
            (true, Some(v)) => format!("Vol. {v}{}", b.subtitle.as_deref().map(|s| format!(" · {s}")).unwrap_or_default()),
            _ => b.title.clone(),
        };
        let status = if nested { format!("  {}", b.status.label()) } else { String::new() };
        Row::new([
            Cell::from(Line::from(vec![indent, mark, Span::raw(title), Span::styled(status, status_color(b.status).dim())])),
            Cell::from(if nested { String::new() } else { b.author.clone().unwrap_or_default() }).style(Style::new().fg(Color::Gray)),
            Cell::from(if b.score > 0 { rating_label(b.score, app.rating) } else { String::new() }).style(Color::Yellow),
        ])
    }).collect();
    let table = Table::new(rows, [Constraint::Fill(3), Constraint::Fill(2), Constraint::Length(6)])
        .header(Row::new(["Title", "Author", "Rating"]).style(Style::new().add_modifier(Modifier::DIM)))
        .row_highlight_style(Style::new().bg(Color::DarkGray).bold())
        .highlight_symbol("▌")
        .block(block);
    f.render_stateful_widget(table, area, &mut app.table);
}

/// Size of a grid tile in cells: a cover with the title under it.
const TILE: (u16, u16) = (16, 13);

/// The rows as tiles, left to right; scrolls by whole lines of tiles to keep
/// the selection on screen.
fn draw_grid(f: &mut Frame, app: &mut Books, area: Rect) {
    let Some(block) = library_block(f, app, area) else { return };
    let inner = block.inner(area);
    f.render_widget(block, area);
    let (tile_w, tile_h) = TILE;
    let cols = (inner.width / tile_w).max(1);
    let lines = usize::from((inner.height / tile_h).max(1));
    let selected = app.table.selected().unwrap_or(0);
    let line = selected / usize::from(cols);
    app.grid_top = app.grid_top.clamp(line.saturating_sub(lines - 1), line);

    let first = app.grid_top * usize::from(cols);
    let shown = first..app.rows.len().min(first + lines * usize::from(cols));
    if app.cover_mode != CoverMode::Off {
        let urls: Vec<String> = app.rows[shown.clone()].iter().filter_map(|row| app.row_cover(row)).collect();
        app.request_covers(urls);
    }
    let graphics = matches!(app.mode, Mode::Normal | Mode::Search) && !app.covered;
    let left = inner.x + inner.width.saturating_sub(tile_w * cols) / 2;
    for i in shown {
        let n = (i - first) as u16;
        let tile = Rect::new(left + n % cols * tile_w, inner.y + n / cols * tile_h, tile_w, tile_h).intersection(inner);
        draw_tile(f, app, i, tile, i == selected, graphics);
    }
}

fn draw_tile(f: &mut Frame, app: &mut Books, i: usize, area: Rect, selected: bool, graphics: bool) {
    let block = if selected {
        Block::bordered().border_type(BorderType::Rounded).border_style(ACCENT)
    } else {
        Block::new().padding(Padding::uniform(1))
    };
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [cover_area, _, caption] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1), Constraint::Length(2)]).areas(inner);

    let row = app.rows[i].clone();
    let url = app.row_cover(&row).filter(|_| app.cover_mode != CoverMode::Off);
    match url.as_ref().map(|url| (url, app.covers.get(url))) {
        Some((url, Some(Cover::Ready(_)))) => draw_cover(f, app, url, cover_area, graphics),
        state => {
            // A blank cover: two cells per three rows keep a book's proportions.
            let width = (cover_area.height * 4 / 3).min(cover_area.width);
            let [blank] = Layout::horizontal([Constraint::Length(width)]).flex(Flex::Center).areas(cover_area);
            let mark = if matches!(state, Some((_, None | Some(Cover::Loading)))) { "…" } else { "" };
            let text = Paragraph::new(mark).centered().dim();
            f.render_widget(text.block(Block::bordered().border_type(BorderType::Rounded).dim()), blank);
        }
    }

    let title = if selected { Style::new().bold() } else { Style::new() };
    let lines = match &row {
        SeriesRow::Series(s) => {
            let fold = if s.expanded { "▾ " } else { "▸ " };
            vec![
                Line::from(vec![fold.fg(ACCENT), Span::styled(s.name.clone(), title)]),
                Line::from(format!("{}/{} ✓", s.finished, s.owned)).green(),
            ]
        }
        SeriesRow::Book { index, nested } => {
            let b = &app.books[*index];
            let mark = if app.pin.as_ref().is_some_and(|p| p.book_id == b.id) { "◉ " } else { "" };
            let name = match (nested, b.volume) {
                (true, Some(v)) => format!("Vol. {v}"),
                _ => b.title.clone(),
            };
            vec![Line::from(vec![mark.fg(Color::Yellow), Span::styled(name, title)])]
        }
    };
    // A title may take both lines; a series keeps its count on the second.
    let caption_text = Paragraph::new(lines).centered();
    if matches!(row, SeriesRow::Series(_)) {
        f.render_widget(caption_text, caption);
    } else {
        f.render_widget(caption_text.wrap(Wrap { trim: true }), caption);
    }
}

fn draw_detail(f: &mut Frame, app: &mut Books, area: Rect) {
    let block = Block::bordered().title(" Details ");
    let mut inner = block.inner(area);
    f.render_widget(block, area);
    if let Some(SeriesRow::Series(s)) = app.selected_row().cloned() {
        return draw_series_detail(f, app, &s, inner);
    }
    let Some(b) = app.selected().cloned() else { return };
    if let (Some(url), false) = (&b.cover_url, app.cover_mode == CoverMode::Off) {
        let rows = (inner.height / 2).min(16);
        let cover_area = Rect { height: rows, ..inner };
        // Graphics sit on top of the cell grid, so they would cover popups.
        let graphics = matches!(app.mode, Mode::Normal | Mode::Search) && !app.covered;
        draw_cover(f, app, url, cover_area, graphics);
        inner.y += rows + 1;
        inner.height = inner.height.saturating_sub(rows + 1);
    }
    let b = &b;
    let field = |label: &str, value: String| {
        Line::from(vec![Span::styled(format!("{label:<9}"), Style::new().dim()), Span::raw(value)])
    };
    let mut lines = vec![Line::from(b.title.as_str().bold().fg(ACCENT))];
    if let Some(s) = &b.subtitle {
        lines.push(Line::from(s.as_str().italic()));
    }
    if let Some(a) = &b.author {
        lines.push(Line::from(format!("by {a}")));
    }
    if let Some((name, volume)) = series::series_of(b) {
        let volume = volume.map(|v| format!(", volume {v}")).unwrap_or_default();
        lines.push(Line::from(vec![format!("{name}{volume}").fg(Color::Magenta), "  N next volume".dim()]));
    }
    lines.push(Line::default());
    lines.push(field("Status", b.status.label().to_owned()).style(status_color(b.status)));
    lines.push(field("Rating", rating_label(b.score, app.rating)));
    match app.dates_of(b) {
        (Some(started), Some(finished)) => lines.push(field("Read", format!("{} – {}", date(started), date(finished)))),
        (Some(started), None) if b.status != Status::ForLater => lines.push(field("Started", date(started))),
        _ => {}
    }
    let st = app.book_stats(&b.id, b.pages);
    // Audiobooks show only when they were started and finished.
    let paged = b.reads_with_pages();
    if let (true, Some(cur), Some(total)) = (paged, st.current_page, b.pages.filter(|p| *p > 0)) {
        let ratio = (f64::from(cur) / f64::from(total)).min(1.0);
        let filled = (ratio * 16.0).round() as usize;
        lines.push(Line::from(vec![
            Span::styled(format!("{:<9}", "Progress"), Style::new().dim()),
            Span::styled("━".repeat(filled), Style::new().fg(ACCENT)),
            Span::styled("━".repeat(16 - filled), Style::new().fg(Color::DarkGray)),
            Span::raw(format!(" {cur}/{total} · {:.0}%", ratio * 100.0)),
        ]));
    } else if let Some(cur) = st.current_page.filter(|_| paged) {
        lines.push(field("Page", cur.to_string()));
    }
    if st.sessions > 0 {
        let plural = if st.sessions == 1 { "" } else { "s" };
        lines.push(field("Time", format!("{} in {} session{plural}", format_duration(st.total_ms), st.sessions)));
    }
    if let Some(pace) = st.pace_pph.filter(|_| paged) {
        let left = st.remaining_ms.filter(|ms| *ms > 0).map(|ms| format!(" · ~{} left", format_duration(ms)));
        lines.push(field("Pace", format!("{pace:.0} pages/h{}", left.unwrap_or_default())));
    }
    if let Some(last) = st.last_read {
        lines.push(field("Last", date(last)));
    }
    if app.pin.as_ref().is_some_and(|p| p.book_id == b.id) {
        lines.push(Line::from("◉ pinned: the next reading session goes here".fg(Color::Yellow)));
    }
    lines.push(field("Format", b.format.as_str().to_owned()));
    if let Some(p) = b.pages.filter(|_| paged) {
        lines.push(field("Pages", p.to_string()));
    }
    if let Some(y) = b.pub_year {
        lines.push(field("Year", y.to_string()));
    }
    if let Some(i) = &b.isbn {
        lines.push(field("ISBN", i.clone()));
    }
    if !b.tags.is_empty() {
        lines.push(field("Tags", b.tags.join(", ")));
    }
    lines.push(field("Added", date(b.created_at)));
    if let Some(n) = &b.notes {
        lines.push(Line::default());
        lines.push(Line::from(n.as_str()));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// A folded series: its volumes in the library and where reading stands.
fn draw_series_detail(f: &mut Frame, app: &mut Books, s: &series::Summary, mut inner: Rect) {
    let current = app.series_current(s).cloned();
    if let (Some(url), false) = (current.as_ref().and_then(|b| b.cover_url.clone()), app.cover_mode == CoverMode::Off) {
        let rows = (inner.height / 2).min(16);
        let graphics = matches!(app.mode, Mode::Normal | Mode::Search) && !app.covered;
        draw_cover(f, app, &url, Rect { height: rows, ..inner }, graphics);
        inner.y += rows + 1;
        inner.height = inner.height.saturating_sub(rows + 1);
    }
    let field = |label: &str, value: String| {
        Line::from(vec![Span::styled(format!("{label:<9}"), Style::new().dim()), Span::raw(value)])
    };
    let mut lines = vec![Line::from(s.name.as_str().bold().fg(ACCENT))];
    if let Some(a) = current.as_ref().and_then(|b| b.author.clone()) {
        lines.push(Line::from(format!("by {a}")));
    }
    lines.push(Line::default());
    let here = series::range_label(s.first, s.last);
    lines.push(field("Here", format!("{} volumes{}", s.books.len(), if here.is_empty() { here } else { format!(", {here}") })));
    lines.push(field("Library", format!("{} volumes, {} finished", s.owned, s.finished)));
    if let Some(b) = current.filter(|b| b.status != Status::Finished) {
        lines.push(field("Now", b.title.clone()).style(status_color(b.status)));
    }
    let pages: u32 = s.books.iter().filter_map(|i| app.books.get(*i)).filter(|b| b.status == Status::Finished).filter_map(|b| b.pages).sum();
    if pages > 0 {
        lines.push(field("Read", format!("{pages} pages")));
    }
    lines.push(Line::default());
    let fold = if s.expanded { "enter fold" } else { "enter unfold" };
    lines.push(Line::from(format!("{fold} · N next volume").dim()));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// Draws the cover at `url` centred in `area`: as a real picture when allowed and
/// the terminal can, else as pixel art.
pub fn draw_cover(f: &mut Frame, app: &mut Books, url: &str, area: Rect, graphics: bool) {
    if area.width < 4 || area.height < 3 {
        return;
    }
    let placeholder = |f: &mut Frame, text: &str| {
        let [mid] = Layout::vertical([Constraint::Length(1)]).flex(Flex::Center).areas(area);
        f.render_widget(Paragraph::new(text.to_owned()).dim().centered(), mid);
    };
    let img = match app.covers.get(url) {
        None | Some(Cover::Loading) => return placeholder(f, "loading cover…"),
        Some(Cover::Failed(e)) => return placeholder(f, &format!("no cover: {e}")),
        Some(Cover::Ready(img)) => img,
    };
    let key = (url.to_owned(), area.width, area.height);
    if let (Some(picker), true, CoverMode::Auto) = (&app.picker, graphics, app.cover_mode) {
        let size = ratatui::layout::Size::new(area.width, area.height);
        let made = app.graphic.get_or_make(key.clone(), || picker.new_protocol(img.clone(), size, Resize::Fit(None)).ok());
        if let Some(protocol) = made {
            let size = protocol.size();
            let x = area.x + area.width.saturating_sub(size.width) / 2;
            let target = Rect { x, width: size.width.min(area.width), height: size.height.min(area.height), ..area };
            f.render_widget(ratatui_image::Image::new(protocol), target);
            return;
        }
    }
    if let Some(image) = app.pixel.get_or_make(key, || Some(cover::pixelate(img, area.width, area.height))) {
        f.render_widget(PixelArt { image, truecolor: app.truecolor }, area);
    }
}

fn draw_online(f: &mut Frame, app: &mut Books) {
    let screen = f.area();
    let area = popup(screen, screen.width.saturating_sub(4).min(100), screen.height.saturating_sub(2).min(26));
    f.render_widget(Clear, area);
    let block = Block::bordered().title(" Search Open Library, Google Books and Manga-Passion ").border_style(ACCENT);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Mode::Online(online) = &app.mode else { return };
    let Online { query, state, .. } = online.as_ref();

    let [input, _, body, hint] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
            .areas(inner);
    let typing = matches!(state, OnlineState::Typing | OnlineState::Error(_));
    let cursor = if typing { "█" } else { "" };
    f.render_widget(Line::from(vec!["› ".fg(ACCENT), query.as_str().into(), cursor.into()]), input);

    let hint_text = match state {
        OnlineState::Typing | OnlineState::Error(_) => "enter search · esc close",
        OnlineState::Loading => "esc close",
        OnlineState::Results { .. } => "j/k move · enter add (a manga series: pick volumes) · / new search · esc close",
    };
    f.render_widget(Line::from(hint_text.dim()), hint);

    match state {
        OnlineState::Typing => {}
        OnlineState::Loading => f.render_widget(Paragraph::new("Searching…".dim()), body),
        OnlineState::Error(e) => {
            f.render_widget(Paragraph::new(e.as_str().fg(Color::Red)).wrap(Wrap { trim: true }), body)
        }
        OnlineState::Results { series, hits, selected, owned } => {
            let [list, preview] = Layout::horizontal([Constraint::Min(0), Constraint::Length(24)]).areas(body);
            let editions = series.iter().map(|s| {
                let mut first = vec![Span::from("▸ ").fg(ACCENT), Span::from(s.title.as_str()).bold(), " manga series".fg(Color::Magenta)];
                if s.complete {
                    first.push(" complete".dim());
                }
                let volumes = (s.volumes > 0).then(|| format!("{} volumes", s.volumes));
                let facts = [s.publisher.clone(), volumes, s.author.clone(), Some("Manga-Passion".into())];
                let second = format!("  {}", facts.into_iter().flatten().collect::<Vec<_>>().join(" · "));
                ListItem::new(vec![Line::from(first), Line::from(second.dim())])
            });
            let books = hits
                .iter()
                .map(|h| {
                    let mut first = vec![Span::from(h.title.as_str()).bold()];
                    if h.format == habit_books::Format::Manga {
                        first.push(" manga".fg(Color::Magenta));
                    }
                    if h.olid.as_ref().is_some_and(|id| owned.contains(id)) {
                        first.push(" ✓ in library".fg(Color::Green));
                    }
                    let year = h.pub_year.map(|y| format!("({y})"));
                    let google = h
                        .olid
                        .as_deref()
                        .is_some_and(|id| id.starts_with(habit_books::googlebooks::ID_PREFIX))
                        .then(|| "· Google Books".to_string());
                    let second =
                        format!("  {}", [h.author.clone(), year, google].into_iter().flatten().collect::<Vec<_>>().join(" "));
                    ListItem::new(vec![Line::from(first), Line::from(second.dim())])
                });
            let items: Vec<ListItem> = editions
                .chain(books)
                .enumerate()
                .map(|(i, item)| if i == *selected { item.style(Style::new().bg(Color::DarkGray)) } else { item })
                .collect();
            let mut state = ratatui::widgets::ListState::default().with_selected(Some(*selected));
            f.render_stateful_widget(List::new(items), list, &mut state);
            let cover = match series.get(*selected) {
                Some(s) => s.cover.clone(),
                None => hits.get(*selected - series.len()).and_then(|h| h.cover_url.clone()),
            };
            if let Some(url) = cover {
                draw_cover(f, app, &url, preview.inner(ratatui::layout::Margin::new(1, 0)), false);
            }
        }
    }
}

fn draw_filter(f: &mut Frame, app: &Books, area: Rect) {
    let line = if matches!(app.mode, Mode::Search) {
        Line::from(vec!["/".fg(ACCENT), app.query.as_str().into(), "█".into()])
    } else {
        Line::from(vec![format!("filter: {}", app.query).fg(ACCENT), "  esc clears".dim()])
    };
    f.render_widget(Paragraph::new(line), area);
}

/// Key hints for the footer.
pub fn hints(app: &Books) -> Vec<(&'static str, &'static str)> {
    match &app.mode {
        Mode::Search => vec![("enter", "keep filter"), ("esc", "clear")],
        Mode::Normal if app.insights => vec![("i", "library"), ("f", "status"), ("?", "keys")],
        Mode::Normal => vec![
            ("n", "add"),
            ("O", "online"),
            ("e", "edit"),
            ("N", "next vol."),
            ("S", "status"),
            ("m", "pin"),
            ("p", "page"),
            ("g", "log"),
            ("v", "sessions"),
            ("f", "filter"),
            ("i", "insights"),
            ("?", "keys"),
        ],
        _ => Vec::new(),
    }
}

fn draw_form(f: &mut Frame, form: &Form) {
    let area = popup(f.area(), 60, Field::ALL.len() as u16 + 4);
    f.render_widget(Clear, area);
    let title = if form.is_new { " Add book " } else { " Edit book " };
    let block = Block::bordered().title(title).border_style(ACCENT);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Field::ALL
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let focused = i == form.focus;
            let label = Span::styled(
                format!("{:<10}", field.label()),
                if focused { Style::new().fg(ACCENT).bold() } else { Style::new().dim() },
            );
            let value = form.value(i);
            let placeholder = Span::from(if value.is_empty() { field.placeholder() } else { "" }).dim();
            let value = match (focused, field.is_choice()) {
                (true, true) => format!("‹ {value} ›"),
                (true, false) => format!("{value}█"),
                _ => value,
            };
            Line::from(vec![label, Span::raw(value), placeholder])
        })
        .collect();
    lines.push(Line::default());
    lines.push(match &form.error {
        Some(e) => Line::from(e.as_str().fg(Color::Red)),
        None => Line::from("tab/↑↓ move · ←→ change · enter save · esc cancel".dim()),
    });
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_status_picker(f: &mut Frame, selected: usize) {
    let area = popup(f.area(), 24, Status::ALL.len() as u16 + 2);
    f.render_widget(Clear, area);
    let items = Status::ALL.iter().enumerate().map(|(i, s)| {
        let item = ListItem::new(format!(" {}", s.label())).style(status_color(*s));
        if i == selected { item.add_modifier(Modifier::REVERSED) } else { item }
    });
    f.render_widget(List::new(items).block(Block::bordered().title(" Move to ").border_style(ACCENT)), area);
}

fn draw_set_page(f: &mut Frame, app: &Books, page: &str) {
    let Some(book) = app.selected() else { return };
    let last = app.selected_sessions().first().map(|s| {
        let pages = s.page_to.map(|p| format!(", ended on page {p}")).unwrap_or_default();
        format!("Last session: {} · {} ({}){pages}", datetime(s.started_at), format_duration(s.duration_ms), s.source.as_str())
    });
    let area = popup(f.area(), 64, 7);
    f.render_widget(Clear, area);
    let lines = vec![
        Line::from(last.unwrap_or_else(|| "No sessions yet.".into())).dim(),
        Line::default(),
        Line::from(vec![Span::styled("Page reached: ", Style::new().dim()), page.into(), "█".into()]),
        Line::default(),
        Line::from("enter save · esc cancel".dim()),
    ];
    let title = format!(" Page · {} ", book.title);
    f.render_widget(Paragraph::new(lines).block(Block::bordered().title(title).border_style(ACCENT)), area);
}

fn draw_log(f: &mut Frame, form: &LogForm) {
    let area = popup(f.area(), 52, 9);
    f.render_widget(Clear, area);
    let mut lines = vec![Line::from(form.title.as_str().bold()), Line::default()];
    for (i, label) in LOG_LABELS.iter().enumerate() {
        let focused = i == form.focus;
        let style = if focused { Style::new().fg(ACCENT).bold() } else { Style::new().dim() };
        let cursor = if focused { "█" } else { "" };
        lines.push(Line::from(vec![Span::styled(format!("{label:<10}"), style), form.fields[i].as_str().into(), cursor.into()]));
    }
    lines.push(Line::default());
    lines.push(match &form.error {
        Some(e) => Line::from(e.as_str().fg(Color::Red)),
        None => Line::from("tab move · enter save · esc cancel".dim()),
    });
    f.render_widget(Paragraph::new(lines).block(Block::bordered().title(" Log session ").border_style(ACCENT)), area);
}

fn draw_sessions(f: &mut Frame, app: &Books, selected: usize, confirm: bool) {
    let screen = f.area();
    let area = popup(screen, screen.width.saturating_sub(4).min(76), screen.height.saturating_sub(4).min(20));
    f.render_widget(Clear, area);
    let title = app.selected().map_or(String::new(), |b| format!(" Sessions · {} ", b.title));
    let block = Block::bordered().title(title).border_style(ACCENT);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [list, hint] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    let sessions = app.selected_sessions();
    if sessions.is_empty() {
        f.render_widget(Paragraph::new("No sessions yet. Read with a reading habit, or g logs one.".dim()), list);
    } else {
        let items: Vec<ListItem> = sessions
            .iter()
            .map(|s| {
                let pages = match (s.page_from, s.page_to) {
                    (Some(a), Some(b)) => format!("p.{a}–{b}"),
                    (None, Some(b)) => format!("→ p.{b}"),
                    _ => String::new(),
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{}  ", datetime(s.started_at))),
                    Span::styled(format!("{:>7}  ", format_duration(s.duration_ms)), Style::new().bold()),
                    Span::raw(format!("{pages:<12}")),
                    Span::styled(format!("{} · {}", s.source.as_str(), s.device), Style::new().dim()),
                ]))
            })
            .collect();
        let mut state = ratatui::widgets::ListState::default().with_selected(Some(selected));
        f.render_stateful_widget(List::new(items).highlight_style(Style::new().bg(Color::DarkGray)), list, &mut state);
    }
    let hint_line = if confirm {
        Line::from("Delete this session? (y/n)".fg(Color::Red))
    } else {
        Line::from("j/k move · d delete · esc close".dim())
    };
    f.render_widget(hint_line, hint);
}

fn draw_help(f: &mut Frame) {
    const KEYS: &[(&str, &str)] = &[
        ("f / F", "next / previous status"),
        ("i", "insights"),
        ("j/k home/end", "move"),
        ("/", "filter library"),
        ("n", "add a book"),
        ("O", "add from Open Library, Google Books, Manga-Passion"),
        ("e enter", "edit book (on a series: fold / unfold)"),
        ("N", "add the next volume of the series"),
        ("S", "change status"),
        ("d", "delete book"),
        ("m", "pin: the next reading session is for this book"),
        ("p", "set page reached (last session)"),
        ("g", "log a past session"),
        ("v", "sessions of this book"),
        ("I", "inbox: choose the book for reading sessions"),
        ("G", "grid of covers / list"),
        ("C", "cover display: auto / pixel / off"),
    ];
    let area = popup(f.area(), 64, KEYS.len() as u16 + 4);
    f.render_widget(Clear, area);
    let mut lines: Vec<Line> = KEYS
        .iter()
        .map(|(k, what)| Line::from(vec![Span::styled(format!("{k:<13}"), Style::new().fg(ACCENT).bold()), Span::raw(*what)]))
        .collect();
    lines.push(Line::default());
    lines.push(Line::from("any key closes".dim()));
    f.render_widget(Paragraph::new(lines).block(Block::bordered().title(" Keys ").border_style(ACCENT)), area);
}

fn datetime(ms: i64) -> String {
    Local.timestamp_millis_opt(ms).single().map_or_else(String::new, |d| d.format("%Y-%m-%d %H:%M").to_string())
}

fn status_color(s: Status) -> Style {
    Style::new().fg(match s {
        Status::Reading => Color::Cyan,
        Status::Finished => Color::Green,
        Status::ForLater => Color::Blue,
        Status::Abandoned => Color::Red,
    })
}

fn date(ms: i64) -> String {
    Local.timestamp_millis_opt(ms).single().map_or_else(String::new, |d| d.format("%Y-%m-%d").to_string())
}

pub fn popup(area: Rect, width: u16, height: u16) -> Rect {
    let [v] = Layout::vertical([Constraint::Length(height)]).flex(Flex::Center).areas(area);
    let [h] = Layout::horizontal([Constraint::Length(width)]).flex(Flex::Center).areas(v);
    h
}

#[cfg(test)]
mod tests {
    use super::super::tests::{books_from, key};
    use super::*;
    use habit_books::{Book, Db};
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;
    use ratatui::Terminal;

    fn render(app: &mut Books) -> String {
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| draw(f, f.area(), app)).unwrap();
        term.backend().buffer().content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn renders_library_detail_and_form() {
        let db = Db::open_in_memory().unwrap();
        let mut b = Book::new("Dune");
        b.author = Some("Frank Herbert".into());
        b.status = Status::Reading;
        b.score = 8;
        db.save_book(&mut b).unwrap();
        let mut app = books_from(db);

        let screen = render(&mut app);
        assert!(screen.contains("Reading·1"));
        assert!(screen.contains("Insights"));
        assert!(screen.contains("Frank Herbert"));
        assert!(screen.contains("★★★★☆"));

        app.handle_key(key(KeyCode::Char('e'))).unwrap();
        assert!(render(&mut app).contains("Edit book"));
    }

    #[test]
    fn shows_the_hint_while_no_habit_feeds_the_library() {
        let mut app = books_from(Db::open_in_memory().unwrap());
        assert!(!render(&mut app).contains("No habit counts"));
        app.hint = Some("No habit counts as reading".into());
        let screen = render(&mut app);
        assert!(screen.contains("No habit counts as reading"));
        assert!(screen.contains("Reading·0"), "the tabs stay");
    }

    #[test]
    fn cover_shows_as_pixel_art_without_graphics_protocol() {
        let db = Db::open_in_memory().unwrap();
        let mut b = Book::new("Dune");
        b.status = Status::Reading;
        b.cover_url = Some("https://example.org/dune.jpg".into());
        db.save_book(&mut b).unwrap();
        let mut app = books_from(db);

        app.covers.insert("https://example.org/dune.jpg".into(), Cover::Loading);
        assert!(render(&mut app).contains("loading cover"));
        let red = image::RgbaImage::from_pixel(20, 30, image::Rgba([255, 0, 0, 255]));
        app.finish(super::super::Done::Cover {
            url: "https://example.org/dune.jpg".into(),
            result: Ok(image::DynamicImage::ImageRgba8(red)),
        });
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| draw(f, f.area(), &mut app)).unwrap();
        let buf = term.backend().buffer();
        let pixels = buf.content().iter().filter(|c| c.symbol() == "▀" && c.fg == Color::Rgb(255, 0, 0)).count()
            + buf.content().iter().filter(|c| c.symbol() == "▀" && c.fg == Color::Indexed(196)).count();
        assert!(pixels > 20, "cover drawn as pixel art");
    }

    #[test]
    fn grid_shows_tiles_and_scrolls_to_the_selection() {
        use super::super::tests::{books_with, press};
        let list = [("Dune", Status::Reading), ("Emma", Status::Reading), ("Ivanhoe", Status::Reading), ("Ulysses", Status::Reading)];
        let mut app = books_with(&list);
        press(&mut app, "G");
        assert!(app.grid);
        let order: Vec<String> = app.rows.iter().map(|r| match r {
            SeriesRow::Book { index, .. } => app.books[*index].title.clone(),
            SeriesRow::Series(_) => unreachable!(),
        }).collect();
        // 100 columns leave room for three tiles in a line, 24 rows for one line.
        let screen = render(&mut app);
        assert!(order[..3].iter().all(|t| screen.contains(t.as_str())));
        assert!(!screen.contains(order[3].as_str()), "the fourth tile is on the next line");
        assert!(!screen.contains("Author"), "no table header");

        // j steps through the tiles one by one, the grid follows.
        press(&mut app, "jjj");
        assert_eq!(app.selected().unwrap().title, order[3]);
        let screen = render(&mut app);
        assert!(screen.contains(order[3].as_str()) && !screen.contains(order[2].as_str()));
        assert_eq!(app.grid_top, 1);
        press(&mut app, "k");
        render(&mut app);
        assert_eq!(app.grid_top, 0);

        press(&mut app, "G");
        assert!(render(&mut app).contains("Author"), "back to the table");
    }

    #[test]
    fn grid_draws_every_cover_on_screen_and_forgets_the_rest() {
        use super::super::tests::{books_from, press};
        let db = Db::open_in_memory().unwrap();
        for title in ["Dune", "Emma"] {
            let mut b = Book::new(title);
            b.status = Status::Reading;
            b.cover_url = Some(format!("https://example.org/{title}.jpg"));
            db.save_book(&mut b).unwrap();
        }
        let mut app = books_from(db);
        let red = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(20, 30, image::Rgba([255, 0, 0, 255])));
        for title in ["Dune", "Emma"] {
            app.finish(super::super::Done::Cover { url: format!("https://example.org/{title}.jpg"), result: Ok(red.clone()) });
        }
        press(&mut app, "G");
        render(&mut app);
        // Both tiles, and the selected book once more in the details.
        assert_eq!(app.pixel.len(), 3);
        press(&mut app, "G");
        render(&mut app);
        render(&mut app);
        assert_eq!(app.pixel.len(), 1, "only the details' cover is left");
    }

    fn app_with_sessions() -> Books {
        let db = Db::open_in_memory().unwrap();
        let mut b = Book::new("Dune");
        b.status = Status::Reading;
        b.pages = Some(600);
        db.save_book(&mut b).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        for (ago_h, minutes, page) in [(50, 60, 30), (26, 30, 45), (1, 60, 75)] {
            let start = now - ago_h * 3_600_000;
            let mut s = db.new_session(&b.id, start, start + minutes * 60_000).unwrap();
            s.page_to = Some(page);
            db.add_session(&s).unwrap();
        }
        db.set_pin(Some(&b.id)).unwrap();
        books_from(db)
    }

    #[test]
    fn detail_shows_progress_pace_and_pin() {
        let mut app = app_with_sessions();
        let screen = render(&mut app);
        assert!(screen.contains("75/600"), "{screen}");
        assert!(screen.contains("2h 30m in 3 sessions"));
        assert!(screen.contains("30 pages/h"));
        assert!(screen.contains("◉ Dune"), "pin marker");
        assert!(screen.contains("next: Dune"));
        app.live = Some(super::super::Live { habit: "read".into(), running: false, elapsed_ms: 0 });
        assert!(render(&mut app).contains("◉ read · Dune (paused)"));
    }

    #[test]
    fn insights_and_popups_render() {
        let mut app = app_with_sessions();
        let press = |app: &mut Books, c| app.handle_key(key(KeyCode::Char(c))).unwrap();
        press(&mut app, 'i');
        let screen = render(&mut app);
        for text in ["Overview", "Recent days", "By weekday", "Time of day", "Most read", "manual", "Sessions 3"] {
            assert!(screen.contains(text), "missing {text}:\n{screen}");
        }
        // Library keys do nothing here.
        assert!(!press(&mut app, 'n'));
        assert!(matches!(app.mode, Mode::Normal));
        press(&mut app, 'i');

        press(&mut app, 'v');
        assert!(render(&mut app).contains("p.45–75"));
        app.handle_key(key(KeyCode::Esc)).unwrap();

        press(&mut app, 'g');
        assert!(render(&mut app).contains("Log session"));
        app.handle_key(key(KeyCode::Esc)).unwrap();
        press(&mut app, '?');
        assert!(render(&mut app).contains("pin: the next reading session"));
    }
}
