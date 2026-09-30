//! Popups drawn over any pane.

use super::app::{setting_value, App, Popup, SETTINGS};
use habit_core::config::{HabitKind, Strictness};
use habit_core::duration::{format_duration, format_duration_long};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

pub fn draw(frame: &mut Frame, app: &App) {
    match &app.popup {
        Some(Popup::Unlock { group, input }) => draw_unlock(frame, app, group, input),
        Some(Popup::ConfirmAbort { emergency }) => draw_abort(frame, app, *emergency),
        Some(Popup::GoalReached) => draw_goal(frame, app),
        Some(Popup::Settings { index, editing }) => draw_settings(frame, app, *index, editing.as_deref()),
        Some(Popup::Lock { input }) => draw_lock(frame, app, input),
        Some(Popup::Count { habit, input }) => draw_count(frame, app, habit, input),
        Some(Popup::ConfirmDelete { section, id }) => draw_delete(frame, app, *section, id),
        Some(Popup::RenameApp { app: key, input }) => draw_rename(frame, key, input),
        Some(Popup::SetCategory { app: key, input }) => draw_category(frame, app, key, input),
        None => {}
    }
}

fn popup_area(frame: &Frame, width: u16, height: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width)]).flex(Flex::Center).areas(frame.area());
    let [area] = Layout::vertical([Constraint::Length(height)]).flex(Flex::Center).areas(area);
    area
}

fn render(frame: &mut Frame, width: u16, title: String, color: Color, lines: Vec<Line>) {
    let area = popup_area(frame, width, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(Block::bordered().title(title).border_style(color)), area);
}

fn input_line<'a>(label: &'a str, input: &'a str) -> Line<'a> {
    Line::from(vec![format!(" {label}: ").into(), input.bold(), "█".fg(Color::Cyan)])
}

fn draw_unlock(frame: &mut Frame, app: &App, group: &str, input: &str) {
    let view = app.snapshot.as_ref().and_then(|s| s.groups.iter().find(|g| g.id == group));
    let name = view.map_or(group, |g| g.name.as_str());
    let credit = view.map_or(0, |g| g.credit_ms);
    let expires = app.snapshot.as_ref().map_or("", |s| s.credit_expires_label.as_str());
    let hint = match view.map(|g| g.unlock_mode.as_str()) {
        Some("rest_of_day") => format!(
            " Day pass: enter unlocks until the day ends for {}.",
            format_duration(view.map_or(0, |g| g.rest_of_day_price_ms))
        ),
        Some("usage") => " Minutes of use, e.g. 15m. Empty spends all credit.".to_string(),
        _ => " e.g. 15m, 1h. Empty spends all credit.".to_string(),
    };
    let lines = vec![
        Line::from(vec![
            " Credit: ".fg(Color::DarkGray),
            format_duration(credit).fg(Color::Cyan),
            format!("  expires {expires}").fg(Color::DarkGray),
        ]),
        Line::from(""),
        input_line("Duration", input),
        Line::from(hint).fg(Color::DarkGray),
        Line::from(" enter confirm · esc cancel").fg(Color::DarkGray),
    ];
    render(frame, 58, format!(" Unlock {name} "), Color::Cyan, lines);
}

fn draw_count(frame: &mut Frame, app: &App, habit: &str, input: &str) {
    let view = app.snapshot.as_ref().and_then(|s| s.habits.iter().find(|h| h.id == habit));
    let name = view.map_or(habit, |h| h.name.as_str());
    let today = view.map_or(String::new(), |h| format!("{} {}", h.count_today, h.unit));
    let lines = vec![
        Line::from(vec![" Today: ".fg(Color::DarkGray), today.fg(Color::Cyan)]),
        Line::from(""),
        input_line("Amount", input),
        Line::from(" 5 adds, -2 corrects, =40 sets today's count").fg(Color::DarkGray),
        Line::from(" enter log · esc cancel").fg(Color::DarkGray),
    ];
    render(frame, 52, format!(" Log {name} "), Color::Cyan, lines);
}

fn draw_rename(frame: &mut Frame, key: &str, input: &str) {
    let lines = vec![
        Line::from(vec![" App id: ".fg(Color::DarkGray), key.to_string().into()]),
        Line::from(""),
        input_line("Show as", input),
        Line::from(" Saved in config.toml; empty shows the id again").fg(Color::DarkGray),
        Line::from(" enter save · esc cancel").fg(Color::DarkGray),
    ];
    render(frame, 60, " Rename ".into(), Color::Cyan, lines);
}

fn draw_category(frame: &mut Frame, app: &App, key: &str, input: &str) {
    let name = app.snapshot.as_ref().map_or(key.to_string(), |s| crate::stats_view::app_display(&s.app_names, key));
    let existing = app.categories();
    let id = if name == key { String::new() } else { format!("  {key}") };
    let mut lines = vec![
        Line::from(vec![" App: ".fg(Color::DarkGray), name.into(), id.fg(Color::DarkGray)]),
        Line::from(""),
        input_line("Category", input),
    ];
    if !existing.is_empty() {
        lines.push(Line::from(format!(" In use: {}", existing.join(", "))).fg(Color::DarkGray));
    }
    lines.push(Line::from(" Saved in config.toml; empty takes it out").fg(Color::DarkGray));
    lines.push(Line::from(" enter save · esc cancel").fg(Color::DarkGray));
    render(frame, 64, " Category ".into(), Color::Cyan, lines);
}

fn draw_delete(frame: &mut Frame, app: &App, section: super::editor::Section, id: &str) {
    let snapshot = app.snapshot.as_ref();
    let name = match section {
        super::editor::Section::Groups => snapshot.and_then(|s| s.groups.iter().find(|g| g.id == id)).map(|g| g.name.clone()),
        super::editor::Section::Habits => snapshot.and_then(|s| s.habits.iter().find(|h| h.id == id)).map(|h| h.name.clone()),
    }
    .unwrap_or_else(|| id.to_string());
    let others = match section {
        super::editor::Section::Groups => " Habits that reward it stop doing so.",
        super::editor::Section::Habits => " Blocks that wait for it stop waiting.",
    };
    let lines = vec![
        Line::from(format!(" Delete the {} {name} from config.toml?", section.noun())),
        Line::from(others).fg(Color::DarkGray),
        Line::from(""),
        Line::from(vec![" y".bold(), " delete · any other key cancels".fg(Color::DarkGray)]),
    ];
    render(frame, 56, " Delete ".into(), Color::Red, lines);
}

fn draw_abort(frame: &mut Frame, app: &App, emergency: bool) {
    let session = app.session();
    let name = session.map_or("the session", |s| s.name.as_str());
    let elapsed = session.map_or(0, |s| s.elapsed_ms);
    let consequence = if emergency && session.is_some_and(|s| s.strictness == Strictness::Strict) {
        " Unlocks will be refused for a while."
    } else {
        " Use s instead to stop and keep it."
    };
    let lines = vec![
        Line::from(format!(" Discard {} of {name}?", format_duration(elapsed))),
        Line::from(consequence).fg(Color::DarkGray),
        Line::from(""),
        Line::from(vec![" y".bold(), " discard · any other key cancels".fg(Color::DarkGray)]),
    ];
    let title = if emergency { " Emergency abort " } else { " Abort " };
    render(frame, 50, title.into(), Color::Red, lines);
}

fn draw_goal(frame: &mut Frame, app: &App) {
    let Some(s) = app.session() else { return };
    let per_round = s.banked_ms / u64::from(s.rounds_completed.max(1));
    let lines = vec![
        Line::from(vec![
            format!(" {} ", s.name).bold(),
            format!("round {} done, {} banked", s.rounds_completed, format_duration(per_round)).fg(Color::Green),
        ]),
        Line::from(format!(" {} earned in this session", format_duration(s.banked_ms))).fg(Color::DarkGray),
        Line::from(""),
        Line::from(vec![
            " c".bold(),
            format!(" keep going: another {} for {}", format_duration(s.target_ms), format_duration(per_round)).into(),
        ]),
        Line::from(vec![" s".bold(), " stop and keep the unfinished part".into()]),
        Line::from(" Time keeps counting until you decide · esc later").fg(Color::DarkGray),
    ];
    render(frame, 58, " Round complete ".into(), Color::Cyan, lines);
}

fn draw_settings(frame: &mut Frame, app: &App, index: usize, editing: Option<&str>) {
    let mut lines = vec![Line::from("")];
    for (i, setting) in SETTINGS.iter().enumerate() {
        let selected = i == index;
        let value = match (selected, editing) {
            (true, Some(input)) => Line::from(vec![input.bold(), "█".fg(Color::Cyan)]),
            _ => Line::from(app.snapshot.as_ref().map(|s| setting_value(s, setting.key)).unwrap_or_default().bold()),
        };
        let marker = if selected { "▌" } else { " " };
        let mut spans = vec![marker.fg(Color::Cyan), format!(" {:<32}", setting.label).into()];
        spans.extend(value.spans);
        let line = Line::from(spans);
        lines.push(if selected && editing.is_none() { line.add_modifier(Modifier::REVERSED) } else { line });
        if selected {
            lines.push(Line::from(format!("   {}", setting.hint)).fg(Color::DarkGray));
        }
    }
    lines.push(Line::from(""));
    if app.session().is_some_and(|s| s.kind == HabitKind::Timer) {
        lines.push(Line::from(" The timer is paused while settings are open.").fg(Color::Yellow));
    }
    let help = if editing.is_some() { " enter save · esc cancel" } else { " enter edit · j/k move · esc close" };
    lines.push(Line::from(help).fg(Color::DarkGray));
    render(frame, 56, " Settings ".into(), Color::Cyan, lines);
}

fn draw_lock(frame: &mut Frame, app: &App, input: &str) {
    let lock = app.snapshot.as_ref().and_then(|s| s.lock.as_ref());
    let mut lines = vec![Line::from("")];
    let title = match lock {
        Some(lock) => {
            lines.push(Line::from(format!(" Committed for {}", format_duration_long(lock.remaining_ms))).fg(Color::DarkGray));
            lines.push(input_line("Extend by", input));
            " Extend commitment "
        }
        None => {
            lines.push(input_line("Commit for", input));
            lines.push(Line::from(" Changes that make habits easier wait until it ends.").fg(Color::DarkGray));
            " Commitment lock "
        }
    };
    lines.push(Line::from(" e.g. 7d, 12h, 2d12h · it can't be shortened").fg(Color::DarkGray));
    lines.push(Line::from(""));
    lines.push(Line::from(" enter commit · esc cancel").fg(Color::DarkGray));
    render(frame, 60, title.into(), Color::Magenta, lines);
}
