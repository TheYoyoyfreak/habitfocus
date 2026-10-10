//! The Insights view (`i`): totals, streaks and when/where/what you read.

use habit_books::stats::{self, format_duration, Insights};
use chrono::{Datelike, Local};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Bar, BarChart, Block, Paragraph};
use ratatui::Frame;

use super::ui::ACCENT;
use super::Books;

const WEEKDAYS: [&str; 7] = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];

pub fn draw(f: &mut Frame, app: &Books, area: Rect) {
    let ins = stats::insights(&app.sessions, &app.readings, chrono::Utc::now().timestamp_millis(), &Local);
    if ins.sessions == 0 {
        let text = "No reading sessions yet.\n\nSessions of your reading habits land here (pin the book with m),\nor press g on a book to log one you already read.";
        f.render_widget(Paragraph::new(text).dim().block(Block::bordered().title(" Insights ")), area);
        return;
    }
    let [overview, middle, bottom] =
        Layout::vertical([Constraint::Length(4), Constraint::Fill(1), Constraint::Fill(1)]).areas(area);
    draw_overview(f, &ins, overview);

    let [daily, weekday] = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(middle);
    draw_daily(f, &ins, daily);
    draw_weekdays(f, &ins, weekday);

    let [hours, books, sources] =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(35), Constraint::Percentage(25)])
            .areas(bottom);
    draw_hours(f, &ins, hours);
    draw_books(f, app, &ins, books);
    draw_sources(f, &ins, sources);
}

fn stat(label: &str, value: String) -> [Span<'static>; 3] {
    [Span::styled(format!("{label} "), Style::new().dim()), Span::styled(value, Style::new().bold().fg(ACCENT)), "   ".into()]
}

fn draw_overview(f: &mut Frame, ins: &Insights, area: Rect) {
    let year = Local::now().year();
    let first: Vec<Span> = [
        stat("Today", format_duration(ins.today_ms)),
        stat("Week", format_duration(ins.week_ms)),
        stat("Month", format_duration(ins.month_ms)),
        stat("Year", format_duration(ins.year_ms)),
        stat("All time", format_duration(ins.total_ms)),
    ]
    .into_iter()
    .flatten()
    .collect();
    let days = |n: u32| if n == 1 { "1 day".to_owned() } else { format!("{n} days") };
    let second: Vec<Span> = [
        stat("Streak", format!("{} (best {})", days(ins.current_streak), days(ins.longest_streak))),
        stat(&format!("Finished in {year}"), ins.finished_this_year.to_string()),
        stat("Pages", ins.pages.to_string()),
        stat("Sessions", ins.sessions.to_string()),
    ]
    .into_iter()
    .flatten()
    .collect();
    f.render_widget(
        Paragraph::new(vec![Line::from(first), Line::from(second)]).block(Block::bordered().title(" Overview ")),
        area,
    );
}

/// (bar width, gap) so `n` bars fit in `width`: 1-column gaps when there is room.
fn fit_bars(width: u16, n: u16) -> (u16, u16) {
    if width + 1 >= 2 * n {
        ((width + 1) / n - 1, 1)
    } else {
        ((width / n).max(1), 0)
    }
}

/// An axis line with `labels` placed under bar indices; labels that would
/// overlap the previous one are dropped.
fn axis(bar: (u16, u16), labels: impl IntoIterator<Item = (usize, String)>) -> Line<'static> {
    let (w, gap) = (usize::from(bar.0), usize::from(bar.1));
    let mut out = String::new();
    for (i, label) in labels {
        let col = i * (w + gap);
        if col < out.chars().count() + usize::from(!out.is_empty()) {
            continue;
        }
        out.push_str(&" ".repeat(col - out.chars().count()));
        out.push_str(&label);
    }
    Line::from(out).dim()
}

/// Splits off the last row of a block's inner area for an axis.
fn chart_areas(block: &Block, area: Rect) -> (Rect, Rect) {
    let inner = block.inner(area);
    let [bars, axis] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    (bars, axis)
}

fn minutes(ms: i64) -> u64 {
    (ms.max(0) / 60_000) as u64
}

fn draw_daily(f: &mut Frame, ins: &Insights, area: Rect) {
    let block = Block::bordered().title(" Recent days ");
    let (bars_area, axis_area) = chart_areas(&block, area);
    f.render_widget(block, area);
    // Fewer days rather than bars without gaps when the panel is narrow.
    let days = &ins.daily[ins.daily.len().saturating_sub(usize::from(bars_area.width).div_ceil(2))..];
    let fit = fit_bars(bars_area.width, days.len().max(1) as u16);
    let bars: Vec<Bar> = days.iter().map(|(_, ms)| Bar::new(minutes(*ms)).text_value(String::new())).collect();
    f.render_widget(BarChart::vertical(bars).bar_width(fit.0).bar_gap(fit.1).bar_style(ACCENT), bars_area);
    // Day of month every week, counted back from today.
    let last = days.len().saturating_sub(1);
    let labels = days.iter().enumerate().filter(|(i, _)| (last - i).is_multiple_of(7)).map(|(i, (d, _))| (i, d.format("%d.%m").to_string()));
    f.render_widget(axis(fit, labels), axis_area);
}

fn draw_weekdays(f: &mut Frame, ins: &Insights, area: Rect) {
    let block = Block::bordered().title(" By weekday ");
    let (width, _) = fit_bars(block.inner(area).width, 7);
    let bars: Vec<Bar> = WEEKDAYS
        .iter()
        .zip(ins.by_weekday)
        .map(|(d, ms)| {
            let text = if width >= 6 && ms > 0 { format_duration(ms) } else { String::new() };
            Bar::new(minutes(ms)).label(Line::from(*d)).text_value(text)
        })
        .collect();
    let chart = BarChart::vertical(bars)
        .bar_width(width)
        .bar_gap(1)
        .bar_style(Color::Green)
        .value_style(Style::new().fg(Color::Black).bg(Color::Green))
        .block(block);
    f.render_widget(chart, area);
}

fn draw_hours(f: &mut Frame, ins: &Insights, area: Rect) {
    let block = Block::bordered().title(" Time of day ");
    let (bars_area, axis_area) = chart_areas(&block, area);
    f.render_widget(block, area);
    let fit = fit_bars(bars_area.width, 24);
    let bars: Vec<Bar> = ins.by_hour.iter().map(|ms| Bar::new(minutes(*ms)).text_value(String::new())).collect();
    f.render_widget(BarChart::vertical(bars).bar_width(fit.0).bar_gap(fit.1).bar_style(Color::Magenta), bars_area);
    f.render_widget(axis(fit, [0, 6, 12, 18].map(|h| (h, format!("{h}h")))), axis_area);
}

/// Title, time and a bar relative to the largest entry.
fn ranked<'a>(rows: impl Iterator<Item = (String, i64)>, max: i64, width: u16, color: Color) -> Vec<Line<'a>> {
    let time_w = 9usize;
    // Names get room first; the bar shrinks (down to nothing) on narrow panels.
    let name_w = rows_name_width(width as usize, time_w);
    let bar_w = (width as usize).saturating_sub(name_w + time_w + 1).min(8);
    rows.map(|(name, ms)| {
        let filled = if max > 0 { ((ms as f64 / max as f64) * bar_w as f64).round() as usize } else { 0 }.min(bar_w);
        let name: String = if name.chars().count() > name_w {
            name.chars().take(name_w - 1).chain(['…']).collect()
        } else {
            name
        };
        Line::from(vec![
            Span::raw(format!("{name:<name_w$} ")),
            Span::styled("▇".repeat(filled), Style::new().fg(color)),
            Span::raw(" ".repeat(bar_w.saturating_sub(filled))),
            Span::styled(format!(" {:>8}", format_duration(ms)), Style::new().dim()),
        ])
    })
    .collect()
}

fn rows_name_width(width: usize, time_w: usize) -> usize {
    width.saturating_sub(time_w + 1 + 8).max(10).min(width.saturating_sub(time_w + 1)).max(1)
}

fn draw_books(f: &mut Frame, app: &Books, ins: &Insights, area: Rect) {
    let block = Block::bordered().title(" Most read ");
    let inner = block.inner(area);
    let max = ins.by_book.first().map_or(0, |b| b.1);
    let rows = ins.by_book.iter().take(inner.height as usize).map(|(id, ms)| (app.title_of(id).to_owned(), *ms));
    f.render_widget(Paragraph::new(ranked(rows, max, inner.width, ACCENT)).block(block), area);
}

fn draw_sources(f: &mut Frame, ins: &Insights, area: Rect) {
    let block = Block::bordered().title(" Where ");
    let inner = block.inner(area);
    let max = ins.by_source.iter().map(|s| s.1).chain(ins.by_device.iter().map(|d| d.1)).max().unwrap_or(0);
    let mut lines = vec![Line::from("source".dim())];
    lines.extend(ranked(ins.by_source.iter().map(|(s, ms)| (s.as_str().to_owned(), *ms)), max, inner.width, Color::Yellow));
    lines.push(Line::from("device".dim()));
    lines.extend(ranked(ins.by_device.iter().cloned(), max, inner.width, Color::Blue));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_always_fit() {
        for width in 1..200u16 {
            for n in [7u16, 24, 30] {
                let (w, gap) = fit_bars(width, n);
                assert!(w >= 1);
                if width >= n {
                    assert!(n * w + (n - 1) * gap <= width, "width {width}, n {n}: {w}+{gap}");
                }
            }
        }
    }

    #[test]
    fn axis_places_labels_under_bars_without_overlap() {
        let line = axis((1, 1), [(0, "0h".to_owned()), (6, "6h".to_owned()), (12, "12h".to_owned()), (18, "18h".to_owned())]);
        assert_eq!(line.to_string(), "0h          6h          12h         18h");
        // Too close together: the second label is dropped.
        assert_eq!(axis((1, 0), [(0, "01.10".to_owned()), (2, "03.10".to_owned())]).to_string(), "01.10");
    }
}
