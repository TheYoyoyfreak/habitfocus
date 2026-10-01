//! The five panes: Today, Blocks, Habits, Insights and Lock.

use super::app::App;
use super::dial;
use super::ui::{bar_spans, draw_session, group_state, habit_progress, local_time, pane_block, session_height};
use crate::stats_view::{self, streak_label};
use habit_core::config::{RewardMode, Strictness};
use habit_core::duration::{format_duration, format_duration_long};
use habit_core::snapshot::{Breakdown, GroupView, HabitView};
use habit_core::state::{Event, EventKind};
use habit_core::Snapshot;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

const WEEK_MS: u64 = 7 * 24 * 3_600_000;
/// Below this width, panes stack their columns.
const WIDE: u16 = 90;

fn selected() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

fn dim(text: impl Into<String>) -> Span<'static> {
    text.into().fg(Color::DarkGray)
}

fn width(text: &str) -> usize {
    text.chars().count()
}

fn pad(text: &str, len: usize) -> String {
    let cut: String = text.chars().take(len).collect();
    format!("{cut:<len$}")
}

/// Events of `kind` in the last week.
fn this_week(app: &App, now: u64, kind: EventKind) -> impl Iterator<Item = &Event> {
    app.events.iter().filter(move |e| e.kind == kind && now.saturating_sub(e.at) < WEEK_MS)
}

fn group_name<'a>(snapshot: &'a Snapshot, id: &'a str) -> &'a str {
    snapshot.groups.iter().find(|g| g.id == id).map_or(id, |g| g.name.as_str())
}

/// Habits a group waits for ("Walk + Anki", "Walk / Anki"), or the habits
/// that earn it.
fn unlocked_by(g: &GroupView, snapshot: &Snapshot) -> String {
    if !g.requires.is_empty() {
        let names: Vec<&str> = g.requires.iter().map(|r| r.name.as_str()).collect();
        return names.join(if g.require_all { " + " } else { " / " });
    }
    let earners: Vec<&str> = snapshot
        .habits
        .iter()
        .filter(|h| h.reward_groups.contains(&g.id))
        .map(|h| h.name.as_str())
        .collect();
    if earners.is_empty() { "nothing".into() } else { earners.join(" / ") }
}

fn unlock_rule(g: &GroupView) -> String {
    match g.unlock_mode.as_str() {
        "usage" => "minutes of use".into(),
        "rest_of_day" => format!("day pass {}", format_duration(g.rest_of_day_price_ms)),
        _ => "clock time".into(),
    }
}

fn blocked_items(g: &GroupView, snapshot: &Snapshot) -> String {
    let apps = g.apps.iter().map(|a| stats_view::app_display(&snapshot.app_names, a));
    apps.chain(g.domains.iter().cloned()).collect::<Vec<_>>().join(" · ")
}

// ---- Today ------------------------------------------------------------------

pub fn today(frame: &mut Frame, area: Rect, app: &App, snapshot: &Snapshot) {
    let [tiles, session, rest] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(session_height(snapshot)),
        Constraint::Min(0),
    ])
    .areas(area);
    draw_tiles(frame, tiles, snapshot);
    draw_session(frame, session, app, snapshot);

    let groups = snapshot.groups.len() as u16 + 2;
    if rest.width >= WIDE {
        let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(rest);
        let [blocks, log] = Layout::vertical([Constraint::Length(groups), Constraint::Min(0)]).areas(right);
        today_habits(frame, left, app, snapshot);
        today_blocks(frame, blocks, snapshot);
        today_log(frame, log, app);
    } else {
        let habits = snapshot.habits.len() as u16 + 2;
        let [h, b, l] =
            Layout::vertical([Constraint::Length(habits), Constraint::Length(groups), Constraint::Min(0)]).areas(rest);
        today_habits(frame, h, app, snapshot);
        today_blocks(frame, b, snapshot);
        today_log(frame, l, app);
    }
}

fn draw_tiles(frame: &mut Frame, area: Rect, snapshot: &Snapshot) {
    let blocking: Vec<&GroupView> = snapshot.groups.iter().filter(|g| g.blocked).collect();
    let items: usize = blocking.iter().map(|g| g.apps.len() + g.domains.len()).sum();
    let credit: u64 = snapshot.groups.iter().map(|g| g.credit_ms).sum();
    let best = snapshot.habits.iter().map(|h| h.best_streak_days).max().unwrap_or(0).max(snapshot.streak_days);
    let tiles = [
        (
            "Blocked",
            format!("{items} apps & sites"),
            format!("{} of {} blocks active", blocking.len(), snapshot.groups.len()),
            Color::Red,
        ),
        (
            "Credit",
            format_duration(credit),
            if credit > 0 { format!("expires {}", snapshot.credit_expires_label) } else { "finish a habit to earn some".into() },
            Color::Cyan,
        ),
        ("Streak", streak_label(snapshot.streak_days), format!("best {}", streak_label(best)), Color::Yellow),
    ];
    let areas = Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(area);
    for ((title, value, note, color), area) in tiles.into_iter().zip(areas.iter()) {
        let lines = vec![Line::from(format!(" {value}").bold().fg(color)), Line::from(dim(format!(" {note}")))];
        frame.render_widget(Paragraph::new(lines).block(pane_block(title)), *area);
    }
}

fn today_habits(frame: &mut Frame, area: Rect, app: &App, snapshot: &Snapshot) {
    let name_width = snapshot.habits.iter().map(|h| width(&h.name)).max().unwrap_or(0).min(18);
    let labels: Vec<(f64, String)> = snapshot.habits.iter().map(|h| habit_progress(h, snapshot)).collect();
    let label_width = labels.iter().map(|(_, l)| width(l)).max().unwrap_or(0);
    // marker, name, bar, label, streak
    let fixed = 1 + 3 + name_width + 1 + 1 + label_width + 6 + 2;
    let bar_width = (area.width as usize).saturating_sub(fixed).clamp(4, 24);
    let items: Vec<ListItem> = snapshot
        .habits
        .iter()
        .zip(labels)
        .map(|(h, (ratio, label))| {
            let active = snapshot.session.as_ref().is_some_and(|s| s.habit == h.id);
            let (mark, color) = match (h.done_today, active) {
                (_, true) => ("●", Color::Green),
                (true, _) => ("✓", Color::Green),
                _ => ("·", Color::DarkGray),
            };
            let mut spans = vec![format!(" {mark} ").fg(color), pad(&h.name, name_width).bold(), " ".into()];
            spans.extend(bar_spans(ratio, bar_width, if h.done_today { Color::Green } else { Color::Cyan }));
            spans.push(dim(format!(" {label:<label_width$}")));
            if h.streak_days > 0 {
                spans.push(format!("  {}d", h.streak_days).fg(Color::Yellow));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let done = snapshot.habits.iter().filter(|h| h.done_today).count();
    let title = format!("Habits · {done}/{} done", snapshot.habits.len());
    let list = List::new(items).block(pane_block(&title)).highlight_style(selected());
    let mut state = ListState::default().with_selected(Some(app.habit_index));
    frame.render_stateful_widget(list, area, &mut state);
}

fn today_blocks(frame: &mut Frame, area: Rect, snapshot: &Snapshot) {
    let name_width = snapshot.groups.iter().map(|g| width(&g.name)).max().unwrap_or(0).min(16);
    let lines: Vec<Line> = snapshot
        .groups
        .iter()
        .map(|g| {
            let (mark, state, color) = group_state(g);
            let mut spans = vec![format!(" {mark} ").fg(color), pad(&g.name, name_width).bold(), "  ".into()];
            if g.blocked && !g.requirements_met {
                spans.push("needs ".fg(color));
                for (i, r) in g.requires.iter().enumerate() {
                    if i > 0 {
                        spans.push(dim(if g.require_all { " + " } else { " / " }));
                    }
                    spans.push(if r.done { format!("✓ {}", r.name).fg(Color::Green) } else { r.name.clone().into() });
                }
            } else {
                spans.push(state.fg(color));
            }
            if g.credit_ms > 0 && g.blocked {
                spans.push(format!(" · {} credit", format_duration(g.credit_ms)).fg(Color::Cyan));
            }
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(pane_block("Blocks")), area);
}

fn today_log(frame: &mut Frame, area: Rect, app: &App) {
    let rows = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = if app.events.is_empty() {
        vec![Line::from(dim(" Nothing yet."))]
    } else {
        app.events
            .iter()
            .take(rows)
            .map(|e| Line::from(vec![dim(format!(" {}  ", local_time(e.at, "%H:%M"))), e.text.clone().into()]))
            .collect()
    };
    frame.render_widget(Paragraph::new(lines).block(pane_block("Log")), area);
}

// ---- Blocks -----------------------------------------------------------------

pub fn blocks(frame: &mut Frame, area: Rect, app: &App, snapshot: &Snapshot) {
    let rows = snapshot.groups.len() as u16 + 3;
    let [table_area, detail] = Layout::vertical([Constraint::Length(rows), Constraint::Min(0)]).areas(area);
    let name_width = snapshot.groups.iter().map(|g| width(&g.name)).max().unwrap_or(4).max(4) as u16 + 2;
    let table_rows: Vec<Row> = snapshot
        .groups
        .iter()
        .map(|g| {
            let (mark, state, color) = group_state(g);
            let mut rule = unlock_rule(g);
            if g.scheduled {
                rule += ", scheduled";
            }
            Row::new(vec![
                Cell::from(Line::from(vec![format!("{mark} ").fg(color), g.name.clone().bold()])),
                Cell::from(blocked_items(g, snapshot)),
                Cell::from(unlocked_by(g, snapshot)),
                Cell::from(rule),
                Cell::from(state.fg(color)),
            ])
        })
        .collect();
    let header = Row::new(["Name", "Apps & sites", "Unlocked by", "Rule", "State"]).fg(Color::DarkGray);
    let table = Table::new(
        table_rows,
        [
            Constraint::Length(name_width),
            Constraint::Fill(3),
            Constraint::Fill(2),
            Constraint::Fill(2),
            Constraint::Fill(3),
        ],
    )
    .header(header)
    .column_spacing(2)
    .row_highlight_style(selected())
    .block(pane_block(&format!("Blocks · {}", snapshot.groups.len())));
    let mut state = TableState::default().with_selected(Some(app.group_index));
    frame.render_stateful_widget(table, table_area, &mut state);

    if let Some(g) = app.selected_group() {
        group_detail(frame, detail, app, snapshot, g);
    }
}

fn group_detail(frame: &mut Frame, area: Rect, app: &App, snapshot: &Snapshot, g: &GroupView) {
    let (_, state, color) = group_state(g);
    let row = |label: &str, value: String| Line::from(vec![dim(format!(" {label:<11}")), value.into()]);
    let mut lines = vec![Line::from(vec![format!(" {} ", g.name).bold(), state.fg(color)]), Line::from("")];

    let mut what = Vec::new();
    let apps: Vec<String> = g.apps.iter().map(|a| stats_view::app_display(&snapshot.app_names, a)).collect();
    for (label, items) in [("apps", &apps), ("sites", &g.domains), ("processes", &g.processes)] {
        if !items.is_empty() {
            what.push(format!("{label}: {}", items.join(", ")));
        }
    }
    lines.push(row("blocks", if what.is_empty() { "nothing yet".into() } else { what.join(" · ") }));
    if g.schedule_summary.is_empty() {
        lines.push(row("when", "all day, every day".into()));
    }
    for (i, window) in g.schedule_summary.iter().enumerate() {
        lines.push(row(if i == 0 { "when" } else { "" }, window.clone()));
    }
    let mut unlock = unlock_rule(g);
    if g.credit_ms > 0 {
        unlock += &format!(" · {} credit, expires {}", format_duration(g.credit_ms), snapshot.credit_expires_label);
    }
    lines.push(row("unlock", unlock));
    let earners: Vec<String> = snapshot
        .habits
        .iter()
        .filter(|h| h.reward_groups.contains(&g.id))
        .map(|h| format!("{} (+{})", h.name, format_duration(h.reward_ms)))
        .collect();
    lines.push(row("earned by", if earners.is_empty() { "no habit".into() } else { earners.join(", ") }));

    if !g.requires.is_empty() {
        lines.push(Line::from(""));
        let heading = if g.require_all { " DO ALL TODAY FIRST" } else { " DO ONE TODAY FIRST" };
        lines.push(Line::from(dim(heading)));
        let name_width = g.requires.iter().map(|r| width(&r.name)).max().unwrap_or(0);
        for r in &g.requires {
            let (mark, mark_color) = if r.done { ("✓", Color::Green) } else { ("·", Color::DarkGray) };
            let mut spans = vec![format!("  {mark} ").fg(mark_color), pad(&r.name, name_width).into(), " ".into()];
            spans.extend(bar_spans(r.progress, 16, if r.done { Color::Green } else { Color::Cyan }));
            spans.push(dim(if r.done { " done".to_string() } else { format!(" {:.0}%", r.progress * 100.0) }));
            lines.push(Line::from(spans));
        }
    }

    let now = snapshot.now_ms;
    let blocked = this_week(app, now, EventKind::BlockEnforced)
        .filter(|e| e.subject.as_ref().is_some_and(|s| g.apps.contains(s) || g.processes.contains(s)))
        .count();
    let unlocks = this_week(app, now, EventKind::Unlocked).filter(|e| e.subject.as_deref() == Some(&g.id)).count();
    lines.push(Line::from(""));
    lines.push(Line::from(dim(format!(" This week: closed {blocked} time(s), unlocked {unlocks} time(s)."))));

    let block = pane_block(&g.name);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
}

// ---- Habits -----------------------------------------------------------------

pub fn habits(frame: &mut Frame, area: Rect, app: &App, snapshot: &Snapshot) {
    let rows = snapshot.habits.len() as u16 + 3;
    let [table_area, heat] = Layout::vertical([Constraint::Length(rows), Constraint::Min(0)]).areas(area);
    let group_names = |ids: &[String]| ids.iter().map(|id| group_name(snapshot, id)).collect::<Vec<_>>().join(", ");
    let mut today_width = 5;
    let table_rows: Vec<Row> = snapshot
        .habits
        .iter()
        .map(|h| {
            let (_, progress) = habit_progress(h, snapshot);
            today_width = today_width.max(width(&progress) as u16 + 2);
            let mut kind = h.kind.name().to_string();
            if h.kind.is_timed() && h.strictness == Strictness::Strict {
                kind += " · strict";
            }
            let today = if h.done_today { format!("✓ {progress}") } else { progress };
            let reward = match h.reward_mode {
                RewardMode::Bank => format!("+{} {}", format_duration(h.reward_ms), group_names(&h.reward_groups)),
                RewardMode::Immediate => {
                    format!("opens {} {}", group_names(&h.reward_groups), format_duration(h.reward_ms))
                }
            };
            Row::new(vec![
                Cell::from(h.name.clone().bold()),
                Cell::from(dim(kind)),
                Cell::from(today.fg(if h.done_today { Color::Green } else { Color::Reset })),
                Cell::from(streak_label(h.streak_days).fg(if h.streak_days > 0 { Color::Yellow } else { Color::DarkGray })),
                Cell::from(dim(streak_label(h.best_streak_days))),
                Cell::from(reward),
            ])
        })
        .collect();
    let header = Row::new(["Habit", "Kind", "Today", "Streak", "Best", "Reward"]).fg(Color::DarkGray);
    let table = Table::new(
        table_rows,
        [
            Constraint::Fill(2),
            Constraint::Length(15),
            Constraint::Length(today_width),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Fill(3),
        ],
    )
    .header(header)
    .column_spacing(2)
    .row_highlight_style(selected())
    .block(pane_block("Habits"));
    let mut state = TableState::default().with_selected(Some(app.habit_index));
    frame.render_stateful_widget(table, table_area, &mut state);

    if let Some(h) = app.selected_habit() {
        heatmap(frame, heat, app, h);
    }
}

fn heatmap(frame: &mut Frame, area: Rect, app: &App, h: &HabitView) {
    const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    let Some(first) = app.stats.first() else {
        frame.render_widget(Paragraph::new(dim(" Loading…")).block(pane_block(&h.name)), area);
        return;
    };
    // Columns are weeks starting on Monday; a partial first week is left out.
    let days = &app.stats[(7 - stats_view::weekday(first.day)) % 7..];
    let total_weeks = days.len().div_ceil(7);
    let fit = (area.width as usize).saturating_sub(8);
    let skip = total_weeks.saturating_sub(fit);
    let mut grid = vec![vec![' '; total_weeks]; 7];
    for (i, day) in days.iter().enumerate() {
        grid[i % 7][i / 7] = stats_view::intensity(day.habits.get(&h.id), h.kind.tracks_time(), h.target_ms, h.goal);
    }
    let mut lines: Vec<Line> = grid
        .iter()
        .zip(DAYS)
        .map(|(row, name)| {
            let cells: String = row[skip..].iter().collect();
            Line::from(vec![dim(format!(" {name}  ")), cells.fg(Color::Green)])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(dim(" · nothing  ░ started  ▒ halfway  ▓ done  █ twice")));
    let week: Vec<_> = app.stats.iter().rev().take(7).filter_map(|d| d.habits.get(&h.id)).collect();
    let done: u32 = week.iter().map(|s| s.completions).sum();
    let amount = if h.kind.tracks_time() {
        format!("{} focused", format_duration(week.iter().map(|s| s.focused_ms).sum()))
    } else {
        format!("{} {}", week.iter().map(|s| s.count).sum::<u64>(), h.unit)
    };
    lines.push(Line::from(dim(format!(" Last 7 days: {amount}, done {done} time(s)"))));
    let weeks = total_weeks - skip;
    frame.render_widget(Paragraph::new(lines).block(pane_block(&format!("{} · last {weeks} weeks", h.name))), area);
}

// ---- Insights ---------------------------------------------------------------

/// Below this height the Insights pane leaves out the week and the sundial,
/// so the table keeps some rows.
const WEEK_MIN_HEIGHT: u16 = 34;
/// Height of each chart row in Insights.
const CHART_HEIGHT: u16 = 11;
/// Width of the sundial's block: the dial and its numbers.
const DIAL_WIDTH: u16 = 44;

pub fn insights(frame: &mut Frame, area: Rect, app: &App, snapshot: &Snapshot) {
    // The charts need the full width, so they sit below the table (and the
    // week's numbers, which only get their own column on wide terminals).
    let week_height = if area.height >= WEEK_MIN_HEIGHT { CHART_HEIGHT } else { 0 };
    let [top, week_area, bars_area] =
        Layout::vertical([Constraint::Min(6), Constraint::Length(week_height), Constraint::Length(CHART_HEIGHT)])
            .areas(area);
    let (table_area, week) = if area.width >= WIDE {
        let [table, week] = Layout::horizontal([Constraint::Percentage(64), Constraint::Percentage(36)]).areas(top);
        (table, week)
    } else {
        let [table, week] = Layout::vertical([Constraint::Min(6), Constraint::Length(10)]).areas(top);
        (table, week)
    };
    let rows = app.insight_rows();
    let total_label = match (app.insights_filter.is_empty(), app.by_category) {
        (false, _) => "All shown",
        (true, true) => "All categories",
        (true, false) => "All apps",
    };
    let total = stats_view::usage_total(&rows, total_label);
    let max = rows.iter().map(|r| r.total_ms).max().unwrap_or(0).max(1);
    let cell_row = |r: &stats_view::UsageRow, is_total: bool| {
        let (opens, avg) = if r.sessions > 0 && !is_total {
            (Cell::from(r.sessions.to_string()), Cell::from(format_duration(r.avg_session_ms)))
        } else {
            (Cell::from(dim("-")), Cell::from(dim("-")))
        };
        let mut name = vec![if is_total { r.label.clone().bold() } else { r.label.clone().into() }];
        match r.key.as_deref() {
            Some(k) if k.starts_with("site:") => name.push(dim(" site")),
            Some(k) if k.starts_with(habit_core::config::TERMINAL_PREFIX) => name.push(dim(" term")),
            _ => {}
        }
        let mut cells = vec![Cell::from(Line::from(name))];
        if !app.by_category {
            cells.push(Cell::from(dim(r.category.clone().unwrap_or_default())));
        }
        cells.extend([
            Cell::from(format_duration(r.today_ms)),
            Cell::from(format_duration(r.total_ms)),
            opens,
            avg,
            Cell::from(if is_total {
                Line::from("")
            } else {
                Line::from(bar_spans(r.total_ms as f64 / max as f64, 12, Color::Cyan).to_vec())
            }),
        ]);
        Row::new(cells)
    };
    let table_rows = std::iter::once(cell_row(&total, true)).chain(rows.iter().map(|r| cell_row(r, false)));
    let first = if app.by_category { "Category" } else { "App" };
    let mut header = vec![first];
    let mut widths = vec![Constraint::Min(16)];
    if !app.by_category {
        header.push("Category");
        widths.push(Constraint::Length(10));
    }
    header.extend(["Today", "7 days", "Opens", "Avg", ""]);
    widths.extend([
        Constraint::Length(7),
        Constraint::Length(7),
        Constraint::Length(5),
        Constraint::Length(7),
        Constraint::Fill(1),
    ]);
    let title = if app.by_category { "Screen time · by category" } else { "Screen time" };
    let mut block = pane_block(title);
    if app.filtering || !app.insights_filter.is_empty() {
        let mut filter = vec![" filter: ".fg(Color::Cyan), app.insights_filter.clone().bold()];
        if app.filtering {
            filter.extend(["█".fg(Color::Cyan), dim(" enter keeps · esc clears ")]);
        } else {
            filter.push(dim(" · f edits · esc clears "));
        }
        block = block.title_bottom(Line::from(filter));
    }
    let table = Table::new(table_rows, widths)
        .header(Row::new(header).fg(Color::DarkGray))
        .column_spacing(1)
        .row_highlight_style(selected())
        .block(block);
    let mut state = TableState::default().with_selected(Some(app.app_index.min(rows.len())));
    frame.render_stateful_widget(table, table_area, &mut state);
    if rows.is_empty() && !app.insights_filter.is_empty() {
        let inner = Rect { y: table_area.y + 3, height: 1, ..table_area.inner(ratatui::layout::Margin::new(1, 0)) };
        frame.render_widget(Paragraph::new(dim(format!(" Nothing matches {:?}", app.insights_filter))), inner);
    }
    let chosen = app.app_index.checked_sub(1).and_then(|i| rows.get(i));

    // One color per category or habit across all charts: the week's
    // categories first, then whatever the hours add.
    let stacks = stats_view::week_stacks(&rows);
    let mut palette = Palette::default();
    palette.add(group_totals(stacks.iter().flatten().map(|(label, ms)| (label.as_str(), *ms, false))));
    for breakdown in [&app.breakdown_period, &app.breakdown_day] {
        palette.add(group_totals(breakdown.slices.iter().map(|s| (s.label.as_str(), s.ms, s.habit))));
    }

    if week_height > 0 {
        let (week_chart_area, dial_area) = if week_area.width >= WIDE {
            let [chart, dial] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(DIAL_WIDTH)]).areas(week_area);
            (chart, Some(dial))
        } else {
            (week_area, None)
        };
        let labels = day_labels(app, stacks.len().max(chosen.map_or(0, |r| r.days_sessions.len())));
        match chosen {
            Some(row) => opens_chart(frame, week_chart_area, row, &labels),
            None => week_bars(frame, week_chart_area, &stacks, &labels, &palette),
        }
        if let Some(dial_area) = dial_area {
            sundial(frame, dial_area, app, snapshot, chosen, &palette);
        }
    }
    hourly_bars(frame, bars_area, app, snapshot, chosen, &palette);

    let now = snapshot.now_ms;
    let days: Vec<_> = app.stats.iter().rev().take(7).collect();
    let completions: u32 = days.iter().flat_map(|d| d.habits.values()).map(|s| s.completions).sum();
    let focused: u64 = days.iter().flat_map(|d| d.habits.values()).map(|s| s.focused_ms).sum();
    let unlocks = this_week(app, now, EventKind::Unlocked).count();
    let spent: u64 = this_week(app, now, EventKind::Unlocked).map(|e| e.amount).sum();
    let blocked = this_week(app, now, EventKind::BlockEnforced).count();
    let expired: u64 = this_week(app, now, EventKind::CreditExpired).map(|e| e.amount).sum();
    let screen_total: u64 = app.apps.iter().map(|a| a.total_ms).sum();
    let row = |label: &str, value: String| Line::from(vec![dim(format!(" {label:<17}")), value.bold()]);
    let mut lines = vec![
        row("habits completed", completions.to_string()),
        row("time on habits", format_duration(focused)),
        row("unlocks", unlocks.to_string()),
        row("credit spent", format_duration(spent)),
        row("credit expired", format_duration(expired)),
        row("windows closed", blocked.to_string()),
        row("screen time", format_duration(screen_total)),
        Line::from(""),
    ];
    let name_width = snapshot.habits.iter().map(|h| width(&h.name)).max().unwrap_or(0).min(16);
    let last_week = &app.stats[app.stats.len().saturating_sub(7)..];
    for h in &snapshot.habits {
        lines.push(Line::from(vec![
            format!(" {} ", pad(&h.name, name_width)).into(),
            stats_view::day_grid(last_week, &h.id).fg(Color::Green),
        ]));
    }
    frame.render_widget(Paragraph::new(lines).block(pane_block("This week")), week);
}

/// Colors for the labels of the charts, in order of their size.
const SLICE_COLORS: [Color; 8] = [
    Color::Cyan,
    Color::Green,
    Color::Magenta,
    Color::Yellow,
    Color::Blue,
    Color::Red,
    Color::LightCyan,
    Color::LightMagenta,
];

/// The colors of `SLICE_COLORS` as pictures need them. The terminal's theme
/// decides the real ones, so these are the usual defaults.
fn rgb_of(color: Color) -> dial::Rgb {
    match color {
        Color::Cyan => [86, 182, 194],
        Color::Green => [152, 195, 121],
        Color::Magenta => [198, 120, 221],
        Color::Yellow => [229, 192, 123],
        Color::Blue => [97, 175, 239],
        Color::Red => [224, 108, 117],
        Color::LightCyan => [140, 220, 230],
        Color::LightMagenta => [230, 160, 240],
        Color::Rgb(r, g, b) => [r, g, b],
        _ => [160, 160, 160],
    }
}

/// Labels (habits, categories, apps) in the order they got their color, so
/// a label keeps its color across the charts of a pane.
#[derive(Default)]
struct Palette(Vec<String>);

impl Palette {
    /// Gives the new labels of `totals` (biggest first) the next colors.
    fn add(&mut self, totals: Vec<(String, u64, bool)>) {
        for (label, _, _) in totals {
            if !self.0.contains(&label) {
                self.0.push(label);
            }
        }
    }

    fn color(&self, label: &str) -> Color {
        let rank = self.0.iter().position(|l| l == label).unwrap_or(self.0.len());
        SLICE_COLORS[rank % SLICE_COLORS.len()]
    }
}

/// `(label, ms, habit)` summed by label, biggest first.
fn group_totals<'a>(parts: impl IntoIterator<Item = (&'a str, u64, bool)>) -> Vec<(String, u64, bool)> {
    let mut totals: Vec<(String, u64, bool)> = Vec::new();
    for (label, ms, habit) in parts {
        match totals.iter_mut().find(|(l, _, _)| l == label) {
            Some(entry) => entry.1 += ms,
            None => totals.push((label.to_string(), ms, habit)),
        }
    }
    totals.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    totals
}

/// A legend line: the biggest `limit` labels with their color and time.
fn legend(totals: &[(String, u64, bool)], palette: &Palette, limit: usize) -> Line<'static> {
    let mut legend = vec![Span::from(" ")];
    for (label, ms, habit) in totals.iter().take(limit) {
        legend.push("█ ".fg(palette.color(label)));
        legend.push(format!("{}{label} ", if *habit { "✓" } else { "" }).into());
        legend.push(dim(format!("{}  ", format_duration(*ms))));
    }
    if totals.is_empty() {
        legend.push(dim("nothing recorded"));
    } else if totals.len() > limit {
        legend.push(dim(format!("+{} more", totals.len() - limit)));
    }
    Line::from(legend)
}

/// The hour slices that belong to `chosen`: all of them for the total row,
/// an app's own, or every app of a category.
fn chosen_slices<'a>(
    breakdown: &'a Breakdown,
    snapshot: &Snapshot,
    chosen: Option<&stats_view::UsageRow>,
) -> Vec<&'a habit_core::snapshot::HourSlice> {
    match chosen {
        None => breakdown.slices.iter().collect(),
        Some(row) => breakdown
            .slices
            .iter()
            .filter(|s| match &row.key {
                Some(key) => &s.key == key,
                None => snapshot.app_categories.get(&s.key).map(String::as_str) == Some(row.label.as_str()),
            })
            .collect(),
    }
}

/// Labels for the last `count` days, oldest first: weekdays, then "today".
fn day_labels(app: &App, count: usize) -> Vec<String> {
    const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let days = &app.stats[app.stats.len().saturating_sub(count)..];
    let mut labels: Vec<String> = days.iter().map(|d| WEEKDAYS[stats_view::weekday(d.day)].to_string()).collect();
    if let Some(last) = labels.last_mut() {
        *last = "today".into();
    }
    // Missing stats (still loading): leave those days unnamed.
    let mut out = vec![String::new(); count.saturating_sub(labels.len())];
    out.extend(labels);
    out
}

/// Screen time per day of the week, stacked by category.
fn week_bars(frame: &mut Frame, area: Rect, stacks: &[Vec<(String, u64)>], labels: &[String], palette: &Palette) {
    let days: Vec<u64> = stacks.iter().map(|parts| parts.iter().map(|(_, ms)| ms).sum()).collect();
    let total: u64 = days.iter().sum();
    let mut header = vec![" Screen time ".fg(Color::DarkGray), format_duration(total).bold().fg(Color::Cyan)];
    if !days.is_empty() {
        header.push(dim(format!("  ·  {}/day", format_duration(total / days.len() as u64))));
    }
    if let Some((busiest, _)) = days.iter().enumerate().filter(|(_, ms)| **ms > 0).max_by_key(|(_, ms)| **ms) {
        header.push(dim(format!("  ·  most on {}", labels.get(busiest).map_or("", String::as_str))));
    }
    let columns = stacks
        .iter()
        .zip(&days)
        .map(|(parts, total)| (*total, parts.iter().map(|(label, ms)| (palette.color(label), *ms)).collect()))
        .collect();
    let totals = group_totals(stacks.iter().flatten().map(|(label, ms)| (label.as_str(), *ms, false)));
    let footer = legend(&totals, palette, 4);
    draw_bars(frame, area, "This week · by category", Line::from(header), columns, &Columns::Days(labels), footer);
}

/// How often the chosen app or category was opened each day.
fn opens_chart(frame: &mut Frame, area: Rect, row: &stats_view::UsageRow, labels: &[String]) {
    use ratatui::symbols::Marker;
    use ratatui::widgets::{Axis, Chart, Dataset, GraphType};
    let block = pane_block(&format!("{} · opens per day", row.label));
    let opens = &row.days_sessions;
    if opens.iter().all(|&n| n == 0) {
        let text = dim(" No opens recorded this week (they come from habitd's archive).");
        frame.render_widget(Paragraph::new(text).block(block), area);
        return;
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [header_area, chart_area] = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
    let total: u32 = opens.iter().sum();
    let mut header = vec![" Opened ".fg(Color::DarkGray), format!("{total}×").bold().fg(Color::Cyan)];
    header.push(dim(format!("  ·  {:.1}/day", f64::from(total) / opens.len() as f64)));
    if row.days_ms.iter().any(|&ms| ms > 0) {
        header.push(dim(format!("  ·  {} used", format_duration(row.days_ms.iter().sum()))));
    }
    if row.avg_session_ms > 0 {
        header.push(dim(format!("  ·  {}/visit", format_duration(row.avg_session_ms))));
    }
    frame.render_widget(Paragraph::new(Line::from(header)), header_area);

    let max = opens.iter().copied().max().unwrap_or(0).max(1);
    let points: Vec<(f64, f64)> = opens.iter().enumerate().map(|(day, &n)| (day as f64, f64::from(n))).collect();
    let line = Dataset::default()
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::new().fg(Color::Cyan))
        .data(&points);
    let dots = Dataset::default().marker(Marker::Dot).style(Style::new().fg(Color::Cyan).bold()).data(&points);
    let x_labels: Vec<Line> = labels.iter().map(|l| Line::from(dim(l.clone()))).collect();
    let chart = Chart::new(vec![line, dots])
        .x_axis(Axis::default().bounds([0.0, (opens.len().max(2) - 1) as f64]).labels(x_labels))
        .y_axis(
            Axis::default()
                .bounds([0.0, f64::from(max)])
                .labels([Line::from(dim("0")), Line::from(dim(max.to_string()))]),
        );
    frame.render_widget(chart, chart_area);
}

/// The hours of the last week as a dial, for the chosen app or category or
/// for everything.
fn sundial(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    snapshot: &Snapshot,
    chosen: Option<&stats_view::UsageRow>,
    palette: &Palette,
) {
    let title = match chosen {
        Some(row) => format!("Rhythm · {}", row.label),
        None => "Rhythm · last 7 days".into(),
    };
    let block = pane_block(&title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let slices = chosen_slices(&app.breakdown_period, snapshot, chosen);
    let hours: Vec<Vec<(String, u64)>> = (0..24u8)
        .map(|hour| {
            group_totals(slices.iter().filter(|s| s.hour == hour).map(|s| (s.label.as_str(), s.ms, s.habit)))
                .into_iter()
                .map(|(label, ms, _)| (label, ms))
                .collect()
        })
        .collect();
    let totals = group_totals(slices.iter().map(|s| (s.label.as_str(), s.ms, s.habit)));
    let total: u64 = totals.iter().map(|(_, ms, _)| ms).sum();

    // The dial is round in pixels, so its width in cells follows the font.
    let font = app.picker.as_ref().map_or((10, 20), |p| (p.font_size().width.max(1), p.font_size().height.max(1)));
    let rows = inner.height;
    let cols = ((u32::from(rows) * u32::from(font.1)).div_ceil(u32::from(font.0)) as u16).min(inner.width.saturating_sub(4));
    // On the right, so midnight's mark on the top border misses the title.
    let dial_area = Rect { x: inner.right().saturating_sub(cols + 2), y: inner.y, width: cols, height: rows };
    let middle = inner.y + rows / 2;
    // Hour marks around the dial; midnight and noon sit on the border.
    let mark = |frame: &mut Frame, x: u16, y: u16, text: &str| {
        if x >= area.x && x + width(text) as u16 <= area.right() {
            frame.render_widget(Paragraph::new(dim(text.to_string())), Rect { x, y, width: width(text) as u16, height: 1 });
        }
    };
    let centre = dial_area.x + cols / 2;
    mark(frame, centre.saturating_sub(1), area.y, "0h");
    mark(frame, centre.saturating_sub(1), area.bottom().saturating_sub(1), "12");
    mark(frame, dial_area.x.saturating_sub(2), middle, "18");
    mark(frame, dial_area.right(), middle, "6");

    let covered = app.popup.is_some() || app.editor.is_some();
    if let (Some(picker), false) = (&app.picker, covered) {
        let parts: Vec<dial::Hour> =
            hours.iter().map(|h| h.iter().map(|(label, ms)| (rgb_of(palette.color(label)), *ms)).collect()).collect();
        let now = chrono::DateTime::from_timestamp_millis(snapshot.now_ms as i64)
            .map(|t| t.with_timezone(&chrono::Local))
            .map(|t| {
                use chrono::Timelike;
                t.hour() as f32 + t.minute() as f32 / 60.0
            });
        let key = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            (&parts, now.map(|n| (n * 12.0) as u32), cols, rows).hash(&mut hasher);
            hasher.finish()
        };
        let mut cache = app.dial.borrow_mut();
        if cache.as_ref().is_none_or(|(k, _)| *k != key) {
            let picture = dial::render(&parts, now, u32::from(cols) * u32::from(font.0), u32::from(rows) * u32::from(font.1));
            let size = ratatui::layout::Size::new(cols, rows);
            *cache = picker
                .new_protocol(image::DynamicImage::ImageRgba8(picture), size, ratatui_image::Resize::Fit(None))
                .ok()
                .map(|protocol| (key, protocol));
        }
        if let Some((_, protocol)) = cache.as_ref() {
            frame.render_widget(ratatui_image::Image::new(protocol), dial_area);
        }
    }

    // The numbers beside it.
    let text_area = Rect { width: dial_area.x.saturating_sub(inner.x + 3), ..inner };
    let hour_totals: Vec<u64> = hours.iter().map(|h| h.iter().map(|(_, ms)| ms).sum()).collect();
    let mut lines = vec![
        Line::from(vec![" ".into(), format_duration(total).bold().fg(Color::Cyan), dim(" in 7 days")]),
    ];
    if let Some(peak) = stats_view::peak_hour(&hour_totals) {
        lines.push(Line::from(vec![dim(" busiest "), peak.into()]));
    }
    let night: u64 = hour_totals.iter().enumerate().filter(|(h, _)| *h < 6).map(|(_, ms)| ms).sum();
    if night > 0 {
        lines.push(Line::from(vec![dim(" 0–6h    "), format_duration(night).into()]));
    }
    lines.push(Line::from(""));
    for (label, ms, habit) in totals.iter().take(usize::from(text_area.height).saturating_sub(lines.len())) {
        let share = (*ms * 100).checked_div(total).unwrap_or(0);
        let name = pad(&format!("{}{label}", if *habit { "✓" } else { "" }), usize::from(text_area.width).saturating_sub(8));
        lines.push(Line::from(vec![" █ ".fg(palette.color(label)), name.into(), dim(format!("{share:>4}%"))]));
    }
    if totals.is_empty() {
        lines.push(Line::from(dim(" nothing recorded")));
    }
    frame.render_widget(Paragraph::new(lines), text_area);
}

/// Screen time per hour, stacked by habit, category or app, like a screen
/// time report. Time during a habit counts as that habit.
fn hourly_bars(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    snapshot: &Snapshot,
    chosen: Option<&stats_view::UsageRow>,
    palette: &Palette,
) {
    let (breakdown, period) = if app.chart_period {
        (&app.breakdown_period, "Last 7 days".to_string())
    } else {
        (&app.breakdown_day, day_label(&app.breakdown_day))
    };
    // A chosen row shows its own hours; the total row stacks everything.
    let slices = chosen_slices(breakdown, snapshot, chosen);
    let totals = group_totals(slices.iter().map(|s| (s.label.as_str(), s.ms, s.habit)));
    let total: u64 = totals.iter().map(|(_, ms, _)| ms).sum();

    let mut header = vec![" Screen time ".fg(Color::DarkGray), format_duration(total).bold().fg(Color::Cyan)];
    let hours: Vec<u64> = (0..24).map(|hour| slices.iter().filter(|s| s.hour == hour).map(|s| s.ms).sum()).collect();
    if let Some(peak) = stats_view::peak_hour(&hours) {
        header.push(dim(format!("  busiest {}", &peak[..5])));
    }
    let habit_ms: u64 = totals.iter().filter(|(_, _, habit)| *habit).map(|(_, ms, _)| ms).sum();
    if habit_ms > 0 {
        header.push(dim(format!("  ·  {} on habits", format_duration(habit_ms))));
    }

    let stacks: Vec<(u64, Vec<(Color, u64)>)> = (0..24u8)
        .map(|hour| {
            let mut parts: Vec<(Color, u64)> =
                slices.iter().filter(|s| s.hour == hour).map(|s| (palette.color(&s.label), s.ms)).collect();
            parts.sort_by_key(|(_, ms)| std::cmp::Reverse(*ms));
            (hours[hour as usize], parts)
        })
        .collect();

    let title = match chosen {
        Some(row) => format!("{period} · {}", row.label),
        None => period,
    };
    draw_bars(frame, area, &title, Line::from(header), stacks, &Columns::Hours, legend(&totals, palette, 6));
}

/// "Today", "Yesterday" or "Mon 21 Sep" for the day a breakdown covers.
fn day_label(breakdown: &Breakdown) -> String {
    match breakdown.offset {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ => chrono::NaiveDate::parse_from_str(&breakdown.date, "%Y-%m-%d")
            .map(|date| date.format("%a %-d %b").to_string())
            .unwrap_or_else(|_| breakdown.date.clone()),
    }
}

/// What the columns of `draw_bars` stand for.
enum Columns<'a> {
    /// The 24 hours of a day, narrow and labelled every third hour.
    Hours,
    /// Days, wide and labelled one by one.
    Days(&'a [String]),
}

/// One column per hour or day: `stacks` gives each column's total and its
/// parts from the bottom up, with `header` above and `footer` (legend or
/// numbers) below.
fn draw_bars(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    header: Line<'static>,
    stacks: Vec<(u64, Vec<(Color, u64)>)>,
    columns: &Columns,
    footer: Line<'static>,
) {
    let mut lines = vec![header];
    let inner_width = area.width.saturating_sub(2) as usize;
    let (cell, gap) = match columns {
        Columns::Hours => (if inner_width > 24 * 2 { 2 } else { 1 }, 0),
        Columns::Days(_) => ((inner_width.saturating_sub(1) / stacks.len().max(1)).clamp(2, 9) - 1, 1),
    };
    let height = (area.height as usize).saturating_sub(5).max(1);
    let max = stacks.iter().map(|(total, _)| *total).max().unwrap_or(0).max(1);
    let bars: Vec<Vec<Color>> = stacks
        .iter()
        .map(|(total, parts)| {
            let cells = ((total * height as u64).div_ceil(max) as usize).min(height);
            let mut column = Vec::new();
            for (color, ms) in parts.iter().filter(|(_, ms)| *ms > 0) {
                let want = ((ms * cells as u64).div_ceil((*total).max(1)) as usize).max(1);
                for _ in 0..want.min(cells.saturating_sub(column.len())) {
                    column.push(*color);
                }
            }
            column
        })
        .collect();
    for row in 0..height {
        let from_bottom = height - 1 - row;
        let mut spans = vec![Span::from(" ")];
        for column in &bars {
            spans.push(match column.get(from_bottom) {
                Some(color) => "█".repeat(cell).fg(*color),
                None => " ".repeat(cell).into(),
            });
            if gap > 0 {
                spans.push(" ".repeat(gap).into());
            }
        }
        lines.push(Line::from(spans));
    }
    let mut axis = String::from(" ");
    match columns {
        // Hour labels under the columns, every third hour.
        Columns::Hours => {
            for hour in (0..24).step_by(3) {
                axis += &format!("{hour:<width$}", width = 3 * cell);
            }
        }
        Columns::Days(labels) => {
            for label in labels.iter() {
                axis += &pad(label, cell + gap);
            }
        }
    }
    lines.push(Line::from(dim(axis)));
    lines.push(footer);
    frame.render_widget(Paragraph::new(lines).block(pane_block(title)), area);
}


// ---- Lock -------------------------------------------------------------------

pub fn lock(frame: &mut Frame, area: Rect, snapshot: &Snapshot) {
    let mut lines = vec![Line::from("")];
    match &snapshot.lock {
        None => lines.extend([
            Line::from(" No commitment lock.".bold()),
            Line::from(""),
            Line::from(dim(" Commit for a while (enter), and habitfocus holds back every config")),
            Line::from(dim(" change that would make things easier until the time is up.")),
            Line::from(dim(" It can be extended, not shortened; ending early takes effect 24h after you ask.")),
        ]),
        Some(lock) => {
            lines.push(Line::from(vec![
                " Committed for ".into(),
                format_duration_long(lock.remaining_ms).bold().fg(Color::Magenta),
                dim(format!("  until {}", local_time(lock.ends_at_ms, "%a %-d %b %Y %H:%M"))),
            ]));
            if lock.end_requested {
                lines.push(Line::from(" Early end requested; e cancels it.").fg(Color::Yellow));
            }
            if lock.extended_ms > 0 {
                lines.push(
                    Line::from(format!(" Extended {} for stopping habitd", format_duration_long(lock.extended_ms)))
                        .fg(Color::Red),
                );
            }
            if lock.browser_guard {
                lines.push(Line::from(dim(" Browsers without the extension get closed.")));
            }
            lines.push(Line::from(""));
            if lock.pending.is_empty() {
                lines.push(Line::from(dim(" No changes held back.")));
            } else {
                lines.push(Line::from(dim(" Held back until it ends:")));
                lines.extend(lock.pending.iter().map(|change| Line::from(format!("   · {change}"))));
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(dim(" WHILE IT HOLDS")));
    for (allowed, text) in [
        (false, "remove apps, sites or schedules from a block"),
        (false, "lower a goal or target, raise a reward or daily limit"),
        (false, "drop a required habit or switch the unlock mode to an easier one"),
        (false, "add habits you log by hand"),
        (true, "add apps, sites, habits you do on screen, or raise a goal"),
    ] {
        let (mark, color) = if allowed { ("✓", Color::Green) } else { ("✗", Color::Red) };
        lines.push(Line::from(vec![format!("   {mark} ").fg(color), text.into()]));
    }
    let block = Block::bordered().title(" Commitment lock ".bold()).border_style(Color::Magenta);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
}
