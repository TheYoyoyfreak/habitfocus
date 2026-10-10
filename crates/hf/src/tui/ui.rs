//! Drawing: the frame (header, pane list, footer) and widgets shared by the
//! panes. Uses the terminal's named colors so it follows the terminal theme.

use super::app::{App, Pane};
use super::{form, panes, popup};
use habit_core::config::{HabitKind, Strictness};
use habit_core::duration::{format_duration, format_duration_long};
use habit_core::snapshot::{GroupView, HabitView, PauseReason, SessionView, SyncView};
use habit_core::Snapshot;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};
use ratatui::Frame;

/// From this width on the pane list is a sidebar, below it a tab bar.
const SIDEBAR_MIN_WIDTH: u16 = 100;
const SIDEBAR_WIDTH: u16 = 28;

pub fn draw(frame: &mut Frame, app: &App) {
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());
    draw_header(frame, header, app);
    let main = if body.width >= SIDEBAR_MIN_WIDTH {
        let [side, main] = Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(0)]).areas(body);
        draw_sidebar(frame, side, app);
        main
    } else {
        let [tabs, main] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(body);
        draw_tabs(frame, tabs, app);
        main
    };
    match &app.snapshot {
        Some(_) if app.editor.is_some() => form::draw(frame, main, app, app.editor.as_ref().expect("checked")),
        Some(snapshot) => match app.pane {
            Pane::Today => panes::today(frame, main, app, snapshot),
            Pane::Blocks => panes::blocks(frame, main, app, snapshot),
            Pane::Habits => panes::habits(frame, main, app, snapshot),
            Pane::Insights => panes::insights(frame, main, app, snapshot),
            Pane::Lock => panes::lock(frame, main, snapshot),
        },
        None => {
            let waiting = Paragraph::new(" Waiting for habitd… start it with `systemctl --user start habitd`.")
                .fg(Color::DarkGray)
                .block(Block::bordered());
            frame.render_widget(waiting, main);
        }
    }
    draw_footer(frame, footer, app);
    popup::draw(frame, app);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let mut title = vec![" habitfocus".bold()];
    if let Some(snapshot) = &app.snapshot {
        title.push(format!("  {}", local_time(snapshot.now_ms, "%a %-d %b · %H:%M")).fg(Color::DarkGray));
    }
    let mut spans = Vec::new();
    if let Some(s) = app.session() {
        let color = if s.running { Color::Green } else { Color::Yellow };
        spans.push(format!("● {} {} / {}   ", s.name, format_duration(s.elapsed_ms), format_duration(s.target_ms)).fg(color));
    }
    if let Some(lock) = app.snapshot.as_ref().and_then(|s| s.lock.as_ref()) {
        let text = if lock.end_requested {
            format!("commitment ends in {}   ", format_duration_long(lock.remaining_ms))
        } else {
            format!("committed {}   ", format_duration_long(lock.remaining_ms))
        };
        spans.push(text.fg(Color::Magenta));
    }
    if let Some(update) = app.snapshot.as_ref().and_then(|s| s.update.as_ref()) {
        spans.push(format!("↑ {} available · hf update   ", update.version).fg(Color::Yellow));
    }
    if let Some(sync) = app.snapshot.as_ref().and_then(|s| s.sync.as_ref()) {
        let (text, color) = sync_status(sync, now_ms());
        spans.push(format!("⇅ {text}   ").fg(color));
    }
    spans.push(if app.connected {
        "● connected ".fg(Color::Green)
    } else {
        "○ habitd not reachable ".fg(Color::Red)
    });
    frame.render_widget(Line::from(title), area);
    frame.render_widget(Line::from(spans).right_aligned(), area);
}

fn draw_sidebar(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines = vec![Line::from(""), Line::from(" PANES").fg(Color::DarkGray)];
    for (i, pane) in Pane::ALL.iter().enumerate() {
        let line = Line::from(vec![format!(" {} ", i + 1).fg(Color::DarkGray), format!("{:<20}", pane.title()).into()]);
        lines.push(if *pane == app.pane { line.add_modifier(Modifier::REVERSED) } else { line });
    }
    if let Some(snapshot) = &app.snapshot {
        lines.push(Line::from(""));
        lines.push(Line::from(" TODAY").fg(Color::DarkGray));
        let row = |label: &str, value: String, color: Color| {
            Line::from(vec![format!(" {label:<12}").fg(Color::DarkGray), value.fg(color)])
        };
        let done = snapshot.habits.iter().filter(|h| h.done_today).count();
        lines.push(row("habits", format!("{done}/{}", snapshot.habits.len()), Color::Reset));
        let credit: u64 = snapshot.groups.iter().map(|g| g.credit_ms).sum();
        lines.push(row("credit", format_duration(credit), if credit > 0 { Color::Cyan } else { Color::DarkGray }));
        if credit > 0 {
            lines.push(Line::from(format!("   expires {}", snapshot.credit_expires_label)).fg(Color::DarkGray));
        }
        let streak = crate::stats_view::streak_label(snapshot.streak_days);
        lines.push(row("streak", streak, if snapshot.streak_days > 0 { Color::Yellow } else { Color::DarkGray }));
        let screen: u64 = app.this_device_apps().iter().map(|a| a.today_ms).sum();
        lines.push(row("screen time", format_duration(screen), Color::Reset));
        let blocking = snapshot.groups.iter().filter(|g| g.blocked).count();
        lines.push(row("blocking", format!("{blocking} of {}", snapshot.groups.len()), Color::Reset));
        if snapshot.penalty_remaining_ms > 0 {
            lines.push(row("penalty", format_duration(snapshot.penalty_remaining_ms), Color::Red));
        }
        if let Some(sync) = &snapshot.sync {
            lines.push(Line::from(""));
            lines.push(Line::from(" SYNC").fg(Color::DarkGray));
            // The sidebar is narrow: "synced" | "3 min ago".
            let (status, color) = sync_status(sync, now_ms());
            let (label, value) = match status.strip_prefix("synced ") {
                Some(ago) => ("synced", ago.to_string()),
                None => ("status", status.replace("server unreachable", "unreachable")),
            };
            lines.push(row(label, value, color));
            let others = match sync.devices.len() {
                0 => "none yet".to_string(),
                1 => sync.devices[0].name.clone(),
                n => format!("{n} devices"),
            };
            lines.push(row("others", others, Color::Reset));
        }
    }
    let block = Block::new().borders(Borders::RIGHT).border_style(Color::DarkGray);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_tabs(frame: &mut Frame, area: Rect, app: &App) {
    let spans: Vec<Span> = Pane::ALL
        .iter()
        .enumerate()
        .flat_map(|(i, pane)| {
            let label = format!(" {} {} ", i + 1, pane.title());
            let tab = if *pane == app.pane { label.reversed() } else { label.fg(Color::DarkGray) };
            [tab, " ".into()]
        })
        .collect();
    frame.render_widget(Line::from(spans), area);
}

/// Key hints for the current pane, the running session and global keys.
fn hints(app: &App) -> Vec<(&'static str, &'static str)> {
    if let Some(editor) = &app.editor {
        return match (&editor.picker, &editor.input) {
            (Some(_), _) => vec![("↑/↓", "choose"), ("enter", "add"), ("esc", "cancel")],
            (_, Some(_)) => vec![("enter", "set"), ("esc", "cancel")],
            _ => vec![
                ("j/k", "move"),
                ("enter", "edit"),
                ("h/l", "change"),
                ("space", "toggle"),
                ("a", "add"),
                ("d", "remove"),
                ("ctrl+s", "save"),
                ("esc", "close"),
            ],
        };
    }
    if app.pane == Pane::Insights && app.filtering {
        return vec![("type", "to filter by app, site or category"), ("enter", "keep"), ("esc", "clear")];
    }
    let mut keys = match app.pane {
        Pane::Today | Pane::Habits => {
            let kind = app.selected_habit().map(|h| h.kind);
            let mut keys = if kind == Some(HabitKind::Passive) {
                vec![("j/k", "move")]
            } else if kind.is_some_and(|k| !k.is_timed()) {
                vec![("j/k", "move"), ("enter/+", "log"), ("-", "undo"), ("=", "amount")]
            } else {
                vec![("j/k", "move"), ("enter", "start")]
            };
            if app.pane == Pane::Habits {
                keys.extend([("e", "edit"), ("n", "new"), ("D", "delete")]);
            }
            keys
        }
        Pane::Blocks => vec![
            ("j/k", "move"),
            ("enter", "unlock all"),
            ("u", "unlock…"),
            ("r", "relock"),
            ("e", "edit"),
            ("n", "new"),
            ("D", "delete"),
        ],
        Pane::Insights => {
            let mut keys = if app.by_category {
                vec![("j/k", "choose"), ("f", "filter"), ("g", "by app")]
            } else {
                vec![("j/k", "choose"), ("f", "filter"), ("g", "by category"), ("c", "category"), ("r", "rename")]
            };
            keys.extend([("[/]", "day"), ("d", if app.chart_period { "day" } else { "7 days" })]);
            if app.snapshot.as_ref().is_some_and(|s| super::app::device_views(s).len() > 1) {
                keys.push(("v", "device"));
            }
            keys
        }
        Pane::Lock => match app.snapshot.as_ref().and_then(|s| s.lock.as_ref()) {
            Some(lock) if lock.end_requested => vec![("enter", "extend"), ("e", "cancel early end")],
            Some(_) => vec![("enter", "extend"), ("e", "end early (24h)")],
            None => vec![("enter", "commit")],
        },
    };
    if let Some(s) = app.session() {
        keys.push(("s", "stop"));
        if s.awaiting_decision {
            keys.push(("c", "continue"));
        }
        keys.push(("a", "abort"));
    }
    keys.extend([("1-5", "panes"), ("o", "settings"), ("q", "quit")]);
    keys
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let line = match app.flash() {
        Some(flash) => Line::from(format!(" {}", flash.text)).fg(if flash.error { Color::Red } else { Color::Green }),
        None => Line::from(
            hints(app)
                .into_iter()
                .flat_map(|(key, label)| [format!(" {key}").bold(), format!(" {label} ").fg(Color::DarkGray)])
                .collect::<Vec<_>>(),
        ),
    };
    frame.render_widget(line, area);
}

pub fn now_ms() -> u64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    since.as_millis() as u64
}

/// "just now", "5 min ago", "2 h ago", or the day and time.
pub fn ago(ms: u64, now: u64) -> String {
    let minutes = now.saturating_sub(ms) / 60_000;
    match minutes {
        0 => "just now".into(),
        1..=59 => format!("{minutes} min ago"),
        60..=1439 => format!("{} h ago", minutes / 60),
        _ => local_time(ms, "%a %H:%M"),
    }
}

/// How sync goes, in a few words, and its color.
pub fn sync_status(sync: &SyncView, now: u64) -> (String, Color) {
    if sync.signed_out {
        ("signed out".into(), Color::Yellow)
    } else if sync.reachable == Some(false) {
        ("server unreachable".into(), Color::Red)
    } else if sync.syncing {
        ("syncing…".into(), Color::Cyan)
    } else if sync.error.is_some() {
        ("sync failed".into(), Color::Red)
    } else if let Some(at) = sync.last_sync_ms {
        (format!("synced {}", ago(at, now)), Color::Green)
    } else {
        ("waiting to sync".into(), Color::DarkGray)
    }
}

/// Formats a unix ms timestamp in local time.
pub fn local_time(ms: u64, format: &str) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|t| t.with_timezone(&chrono::Local).format(format).to_string())
        .unwrap_or_default()
}

pub fn pane_block(title: &str) -> Block<'static> {
    Block::bordered().title(format!(" {title} ").bold()).border_style(Color::DarkGray)
}

/// A text progress bar of `width` cells.
pub fn bar(ratio: f64, width: usize) -> (String, String) {
    let filled = ((ratio.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    ("█".repeat(filled), "░".repeat(width - filled))
}

pub fn bar_spans(ratio: f64, width: usize, color: Color) -> [Span<'static>; 2] {
    let (done, rest) = bar(ratio, width);
    [done.fg(color), rest.fg(Color::DarkGray)]
}

/// Today's progress of a habit, 0–1, and a label like "12:00 / 30:00" or
/// "36 / 50 cards".
pub fn habit_progress(h: &HabitView, snapshot: &Snapshot) -> (f64, String) {
    if h.kind == HabitKind::Passive {
        let done = h.today_focused_ms;
        if h.target_ms == 0 {
            // Only tracked: no goal to fill.
            (0.0, format!("{} today", format_duration(done)))
        } else {
            let ratio = if h.done_today { 1.0 } else { done as f64 / h.target_ms as f64 };
            (ratio, format!("{} / {}", format_duration(done), format_duration(h.target_ms)))
        }
    } else if h.kind.is_timed() {
        let running = snapshot.session.as_ref().filter(|s| s.habit == h.id);
        // A session running in another round counts against that round's goal.
        let (done, goal) = running.map_or((h.today_focused_ms, h.target_ms), |s| (s.elapsed_ms, s.target_ms));
        let ratio = if h.done_today && running.is_none() { 1.0 } else { done as f64 / goal.max(1) as f64 };
        (ratio, format!("{} / {}", format_duration(done), format_duration(goal)))
    } else if h.kind == HabitKind::Manual && h.goal == 1 {
        let done = h.count_today > 0;
        (if done { 1.0 } else { 0.0 }, if done { "done" } else { "to do" }.to_string())
    } else if h.daily_limit.is_some_and(|l| h.rounds_today >= l) {
        (1.0, format!("{} {}", h.count_today, h.unit))
    } else {
        let goal = h.goal * u64::from(h.rounds_today + 1);
        let into_round = h.count_today.saturating_sub(h.goal * u64::from(h.rounds_today));
        (into_round as f64 / h.goal.max(1) as f64, format!("{} / {goal} {}", h.count_today, h.unit))
    }
}

/// A group's state: a marker, a short label and its color.
pub fn group_state(g: &GroupView) -> (&'static str, String, Color) {
    if g.off_schedule {
        ("◇", g.schedule_label.clone().unwrap_or_else(|| "off hours".into()), Color::Blue)
    } else if !g.blocked {
        ("□", format!("unlocked {}", g.unlock_label.as_deref().unwrap_or("")).trim_end().to_string(), Color::Green)
    } else if !g.requirements_met {
        ("■", "waiting for habits".into(), Color::Red)
    } else {
        let label = g.schedule_label.as_ref().map_or("locked".to_string(), |l| format!("locked {l}"));
        ("■", label, Color::Red)
    }
}

pub fn session_status(s: &SessionView, app: &App) -> (String, Color) {
    if s.awaiting_decision {
        return ("round done: c continue · s stop ".into(), Color::Cyan);
    }
    match (s.pause_reason, s.kind) {
        (_, HabitKind::Timer) if app.timer_warning.is_some() => ("can't track focus here ".into(), Color::Red),
        (None, HabitKind::Timer) => ("counting while this window is focused ".into(), Color::Green),
        (None, _) => ("running ".into(), Color::Green),
        (Some(PauseReason::Unfocused), HabitKind::Timer) => ("paused: focus this window ".into(), Color::Yellow),
        (Some(PauseReason::Unfocused), _) => ("paused: switch to the habit window ".into(), Color::Yellow),
        (Some(PauseReason::Idle), _) => ("paused: you're idle ".into(), Color::Yellow),
    }
}

/// Rows `draw_session` needs.
pub fn session_height(snapshot: &Snapshot) -> u16 {
    match &snapshot.session {
        Some(s) if s.kind == HabitKind::Timer => 11,
        Some(_) => 5,
        None => 3 + u16::from(snapshot.penalty_remaining_ms > 0),
    }
}

pub fn draw_session(frame: &mut Frame, area: Rect, app: &App, snapshot: &Snapshot) {
    let block = pane_block("Session");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(s) = &snapshot.session else {
        let mut lines = vec![Line::from(" No session running. Select a habit and press enter.").fg(Color::DarkGray)];
        if snapshot.penalty_remaining_ms > 0 {
            lines.push(
                Line::from(format!(
                    " Unlocks refused for {} after an emergency abort.",
                    format_duration(snapshot.penalty_remaining_ms)
                ))
                .fg(Color::Red),
            );
        }
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    };

    let timer = s.kind == HabitKind::Timer;
    let [top, clock, gauge_area, note] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(if timer { 6 } else { 0 }),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let mut name = vec![Span::raw(" "), s.name.as_str().bold()];
    if timer {
        name.push("  timer".fg(Color::Blue));
    }
    if s.strictness == Strictness::Strict {
        name.push("  strict".fg(Color::Magenta));
    }
    frame.render_widget(Line::from(name), top);
    let (status, color) = session_status(s, app);
    frame.render_widget(Line::from(status.fg(color)).right_aligned(), top);

    if timer {
        let lines = big_clock(s.elapsed_ms).into_iter().map(|row| Line::from(row.fg(color)));
        // One blank row above the digits.
        let digits = Rect { y: clock.y + 1, height: clock.height.saturating_sub(1), ..clock };
        frame.render_widget(Paragraph::new(lines.collect::<Vec<_>>()).alignment(Alignment::Center), digits);
    }

    let mut label = format!(
        "{} / {}  ·  {} left",
        format_duration(s.elapsed_ms),
        format_duration(s.target_ms),
        format_duration(s.remaining_ms)
    );
    if s.rounds_completed > 0 {
        label += &format!(
            "  ·  {} round(s), {} earned",
            s.rounds_completed,
            format_duration(s.banked_ms)
        );
    }
    let gauge = Gauge::default()
        .ratio(s.progress.clamp(0.0, 1.0))
        .label(label)
        .gauge_style(Style::new().fg(color).bg(Color::Reset));
    frame.render_widget(gauge, gauge_area.inner(Margin::new(1, 0)));

    let note_line = match (&app.timer_warning, timer, s.resumed_ms) {
        (Some(warning), true, _) => Line::from(format!(" {warning}")).fg(Color::Red),
        _ if s.awaiting_decision => Line::from(format!(
            " Round done and banked. c keeps going ({} for another {}), s stops",
            format_duration(s.target_ms),
            format_duration(s.banked_ms / u64::from(s.rounds_completed.max(1)))
        ))
        .fg(Color::Cyan),
        (_, _, resumed) if resumed > 0 => {
            Line::from(format!(" resumed with {} from earlier today", format_duration(resumed))).fg(Color::DarkGray)
        }
        _ => Line::from(" s stops and keeps your progress").fg(Color::DarkGray),
    };
    frame.render_widget(note_line, note);
}

/// Five-row block digits for `m:ss` / `h:mm:ss`.
pub fn big_clock(ms: u64) -> Vec<String> {
    const DIGITS: [[&str; 5]; 10] = [
        ["███", "█ █", "█ █", "█ █", "███"],
        ["  █", "  █", "  █", "  █", "  █"],
        ["███", "  █", "███", "█  ", "███"],
        ["███", "  █", "███", "  █", "███"],
        ["█ █", "█ █", "███", "  █", "  █"],
        ["███", "█  ", "███", "  █", "███"],
        ["███", "█  ", "███", "█ █", "███"],
        ["███", "  █", "  █", "  █", "  █"],
        ["███", "█ █", "███", "█ █", "███"],
        ["███", "█ █", "███", "  █", "███"],
    ];
    const COLON: [&str; 5] = [" ", "█", " ", "█", " "];
    let text = format_duration(ms);
    (0..5)
        .map(|row| {
            text.chars()
                .map(|c| match c.to_digit(10) {
                    Some(d) => DIGITS[d as usize][row],
                    None => COLON[row],
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}


#[cfg(test)]
mod tests {
    use super::super::app::tests::{engine, engine_at_goal, CONFIG};
    use super::super::app::Action;
    use super::*;
    use habit_core::{Config, Engine, Input, State};
    use habit_ipc::Request;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;

    const MIN: u64 = 60_000;

    fn render(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Row and column (in characters) of the first `text` on the screen.
    fn find(lines: &[Vec<char>], text: &str) -> Option<(usize, usize)> {
        let text: Vec<char> = text.chars().collect();
        lines.iter().enumerate().find_map(|(row, line)| {
            line.windows(text.len()).position(|w| w == text.as_slice()).map(|col| (row, col))
        })
    }

    fn assert_shows(screen: &str, expected: &[&str]) {
        for text in expected {
            assert!(screen.contains(text), "missing {text:?} in\n{screen}");
        }
    }

    /// An app on `engine` at `now`, with its log, stats and screen time loaded.
    fn app_for(e: &Engine, now: u64) -> App {
        let mut app = App::new();
        app.set_snapshot(e.snapshot(now));
        app.events = e.events(500);
        app.stats = e.day_stats(140, now);
        app.apps = e.app_insights(7, &[], now);
        app
    }

    /// Games requires walk, social has credit, a counter and a log entry.
    fn rich_engine() -> Engine {
        let config = CONFIG.replace("apps = [\"steam\"]", "apps = [\"steam\"]\nrequires = [\"walk\", \"yoga\"]\nrequire = \"all\"")
            + "\n[habits.yoga]\nname = \"Yoga\"\nkind = \"counter\"\ngoal = 20\nunit = \"poses\"\nreward = { groups = [\"social\"], duration = \"10m\" }\n";
        let mut e = Engine::new(Config::from_toml(&config).unwrap(), State::default());
        e.handle(Input::WindowsReset(vec![]), 0);
        e.log_done("yoga", 30, false, MIN).unwrap();
        e.start("walk", MIN).unwrap();
        e.handle(Input::Tick, 7 * MIN);
        e
    }

    #[test]
    fn today_shows_tiles_session_habits_blocks_and_log() {
        let e = rich_engine();
        let app = app_for(&e, 7 * MIN);
        let screen = render(&app, 120, 40);
        assert_shows(&screen, &[
            "PANES", "1 Today", "5 Lock", "habits", "1/4", "credit", "10:00",
            "Blocked", "2 apps & sites", "Credit", "Streak",
            "Walk", "running", "6:00 / 30:00",
            "Habits · 1/4 done", "30 / 40 poses", "● Walk",
            "Blocks", "Games", "needs Walk + ✓ Yoga", "Social  locked · 10:00 credit",
            "Log", "Yoga: 30 poses today",
            "s stop", "1-5 panes",
        ]);
    }

    #[test]
    fn narrow_terminals_get_a_tab_bar() {
        let e = rich_engine();
        let app = app_for(&e, 7 * MIN);
        let screen = render(&app, 80, 40);
        assert!(!screen.contains("PANES"), "{screen}");
        assert_shows(&screen, &[" 1 Today ", " 2 Blocks ", "Habits · 1/4 done", "Log"]);
    }

    #[test]
    fn blocks_pane_lists_rules_and_details() {
        let e = rich_engine();
        let mut app = app_for(&e, 7 * MIN);
        app.handle_key(key(KeyCode::Char('2')));
        let screen = render(&app, 130, 40);
        assert_shows(&screen, &[
            "Blocks · 2", "Apps & sites", "Unlocked by", "Walk + Yoga", "clock time", "reddit.com",
            "all day, every day", "earned by", "Walk (+30:00)",
            "DO ALL TODAY FIRST", "✓ Yoga", "done", "· Walk", "20%",
            "enter unlock all",
        ]);
        app.handle_key(key(KeyCode::Down));
        let screen = render(&app, 130, 40);
        assert_shows(&screen, &["Social", "10:00 credit, expires", "Reading (+1:00:00)"]);
    }

    #[test]
    fn habits_pane_has_a_table_and_heatmap() {
        let mut e = engine();
        e.start("walk", 0).unwrap();
        e.handle(Input::Tick, 31 * MIN);
        let mut app = app_for(&e, 32 * MIN);
        app.handle_key(key(KeyCode::Char('3')));
        app.habit_index = 2;
        let screen = render(&app, 120, 40);
        assert_shows(&screen, &[
            "Habit", "Kind", "Streak", "Book", "timer", "apps · strict", "✓ 31:00 / 30:00", "1 day",
            "opens Games 30:00", "+1:00:00 Social",
            "Walk · last 20 weeks", "mon", "sun", "▓", "▓ done", "Last 7 days: 31:00 focused, done 1 time(s)",
        ]);
    }

    #[test]
    fn the_header_announces_a_new_version() {
        let mut e = engine();
        let mut app = app_for(&e, 0);
        assert!(!render(&app, 120, 30).contains("available"));
        e.set_available_update(Some(habit_core::snapshot::UpdateView { version: "0.2.0".into(), url: String::new() }));
        app.set_snapshot(e.snapshot(0));
        assert_shows(&render(&app, 120, 30), &["↑ 0.2.0 available · hf update", "● connected"]);
    }

    fn synced(devices: &[(&str, &str)]) -> habit_core::snapshot::SyncView {
        habit_core::snapshot::SyncView {
            server: "https://sync.example".into(),
            username: "julian".into(),
            device: "desktop".into(),
            reachable: Some(true),
            last_sync_ms: Some(super::now_ms() - 3 * MIN),
            devices: devices
                .iter()
                .map(|(id, name)| habit_core::snapshot::SyncDeviceView {
                    id: id.to_string(),
                    name: name.to_string(),
                    last_seen_ms: None,
                    rows: 10,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn sync_shows_only_once_set_up() {
        let mut e = engine();
        let mut app = app_for(&e, 0);
        let screen = render(&app, 120, 30);
        assert!(!screen.contains("⇅") && !screen.contains("SYNC"), "{screen}");
        assert!(!hints(&app).iter().any(|(k, _)| *k == "v"));
        app.handle_key(key(KeyCode::Char('o')));
        assert_shows(&render(&app, 120, 40), &[
            "Sync between devices  off", "hf sync register <server> <user>", "hf sync login <server> <user>",
            "habitfocus_sync_server",
        ]);

        e.set_sync(Some(synced(&[("aaaa", "laptop")])));
        app.set_snapshot(e.snapshot(0));
        app.popup = None;
        assert_shows(&render(&app, 120, 30), &["⇅ synced 3 min ago", "SYNC", "synced      3 min ago", "laptop"]);
        app.handle_key(key(KeyCode::Char('o')));
        assert_shows(&render(&app, 120, 40), &[
            "Sync between devices  synced 3 min ago", "julian on https://sync.example", "others: laptop",
            "hf sync logout",
        ]);

        let mut down = synced(&[]);
        down.reachable = Some(false);
        down.error = Some("can't reach the sync server".into());
        e.set_sync(Some(down));
        app.set_snapshot(e.snapshot(0));
        app.popup = None;
        assert_shows(&render(&app, 120, 30), &["⇅ server unreachable"]);
    }

    #[test]
    fn insights_switch_between_devices() {
        let mut e = engine();
        let mut app = app_for(&e, 0);
        app.handle_key(key(KeyCode::Char('4')));
        assert_eq!(app.handle_key(key(KeyCode::Char('v'))), Action::None, "only this device");
        assert!(!render(&app, 120, 30).contains("Screen time ·"));

        e.set_sync(Some(synced(&[("aaaa", "laptop"), ("bbbb", "phone")])));
        app.set_snapshot(e.snapshot(0));
        assert!(hints(&app).contains(&("v", "device")));
        assert_shows(&render(&app, 120, 30), &["Screen time · desktop (this device)"]);
        let mut seen = Vec::new();
        for _ in 0..4 {
            assert_eq!(app.handle_key(key(KeyCode::Char('v'))), Action::Refresh);
            seen.push(app.device.clone());
        }
        let all = Some(habit_ipc::ALL_DEVICES.to_string());
        assert_eq!(seen, [Some("aaaa".into()), Some("bbbb".into()), all, None]);

        app.handle_key(key(KeyCode::Char('v')));
        assert_shows(&render(&app, 120, 30), &["Screen time · laptop"]);
        app.device = Some(habit_ipc::ALL_DEVICES.into());
        app.wall_clock = Some(habit_core::snapshot::WallClock { today_ms: MIN, total_ms: 90 * MIN });
        assert_shows(&render(&app, 120, 30), &["Screen time · all devices", "at any screen", "1:30:00"]);

        // The laptop signs out: back to this device.
        app.device = Some("aaaa".into());
        e.set_sync(Some(synced(&[("bbbb", "phone")])));
        assert!(app.set_snapshot(e.snapshot(0)), "refetch for this device");
        assert_eq!(app.device, None);
    }

    #[test]
    fn passive_habits_show_their_time_and_start_nothing() {
        let config = crate::tui::app::tests::CONFIG.to_string()
            + "\n[habits.code]\nname = \"Coding\"\nkind = \"passive\"\ntarget = \"2m\"\nallow = [{ app = \"zed\" }]\n\
               \n[habits.agents]\nname = \"Agents\"\nkind = \"passive\"\nallow = [{ program = \"claude\" }]\n";
        let mut e = Engine::new(habit_core::Config::from_toml(&config).unwrap(), habit_core::State::default());
        e.handle(Input::WindowsReset(vec![habit_core::WindowInfo { id: 1, app_id: "zed".into(), title: String::new(), pid: None }]), 0);
        e.handle(Input::FocusChanged(Some(1)), 0);
        for t in 1..=60 {
            e.handle(Input::Tick, t * 1000);
        }
        let mut app = app_for(&e, 60_000);
        app.handle_key(key(KeyCode::Char('3')));
        while app.selected_habit().is_some_and(|h| h.id != "code") {
            app.handle_key(key(KeyCode::Down));
        }
        let screen = render(&app, 120, 30);
        assert_shows(&screen, &["Coding", "1:00 / 2:00", "Agents", "0:00 today"]);
        assert!(!screen.contains("enter start"), "passive habits have nothing to start");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    }

    #[test]
    fn insights_pane_shows_the_week_opens_and_sundial() {
        let mut e = engine();
        e.handle(Input::WindowsReset(vec![habit_core::WindowInfo { id: 1, app_id: "zed".into(), title: String::new(), pid: None }]), 0);
        e.handle(Input::FocusChanged(Some(1)), 0);
        for t in 1..=90 {
            e.handle(Input::Tick, t * 1000);
        }
        let mut app = app_for(&e, 150_000);
        let visit = |key: &str, start, end| habit_core::archive::Visit {
            key: key.into(),
            start,
            end,
            active_ms: end - start,
            habit: None,
            tmux_session: None,
        };
        let visits = [visit("zed", 0, 30_000), visit("zed", 100_000, 150_000)];
        app.apps = e.app_insights(7, &visits, 150_000);
        app.breakdown_day = e.day_breakdown(&visits, 0, 150_000, false);
        app.breakdown_period = e.period_breakdown(&visits, 7, 150_000);
        app.handle_key(key(KeyCode::Char('4')));

        // Too short a terminal keeps the old layout.
        assert!(!render(&app, 120, 30).contains("This week · by category"));
        let screen = render(&app, 120, 44);
        assert_shows(&screen, &["This week · by category", "today", "Rhythm · last 7 days", "0h", "12", "18", "in 7 days"]);
        // Each day's bar carries its total on top.
        let lines: Vec<Vec<char>> = screen.lines().map(|l| l.chars().collect()).collect();
        let on_bar = (0..lines.len() - 1).any(|row| {
            find(&lines[row..=row], "1:30").is_some_and(|(_, col)| (col..col + 4).all(|c| lines[row + 1].get(c) == Some(&'█')))
        });
        assert!(on_bar, "today's total sits on its bar:\n{screen}");

        // Choosing a row turns the week into its opens.
        app.handle_key(key(KeyCode::Down));
        let screen = render(&app, 120, 44);
        assert_shows(&screen, &["zed · opens per day", "Opened 2×", "Rhythm · zed"]);
        // Every day's name sits under its dot.
        let lines: Vec<Vec<char>> = screen.lines().map(|l| l.chars().collect()).collect();
        let (label_row, today) = find(&lines, "today").unwrap();
        let dot_in = |cols: std::ops::Range<usize>| cols.clone().any(|c| (0..label_row).any(|r| lines[r].get(c) == Some(&'•')));
        // "today" ends at the chart's edge, over the last dot.
        assert!(dot_in(today..today + 5), "no dot above today:\n{screen}");
        let weekdays: Vec<usize> = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
            .iter()
            .filter_map(|day| find(&lines[label_row..=label_row], day).map(|(_, c)| c))
            .collect();
        assert_eq!(weekdays.len(), 6, "the other days are labelled:\n{screen}");
        for col in weekdays {
            assert!(dot_in(col + 1..col + 2), "no dot above column {}:\n{screen}", col + 1);
        }

        // With pictures (halfblocks here), the dial is drawn and kept.
        app.picker = Some(ratatui_image::picker::Picker::halfblocks());
        render(&app, 120, 44);
        let key_before = app.dial.borrow().as_ref().map(|(k, _)| *k);
        assert!(key_before.is_some(), "the dial was drawn");
        render(&app, 120, 44);
        assert_eq!(app.dial.borrow().as_ref().map(|(k, _)| *k), key_before, "and reused while nothing changed");
    }

    #[test]
    fn insights_pane_shows_screen_time_and_week() {
        let mut e = engine();
        e.handle(Input::WindowsReset(vec![habit_core::WindowInfo { id: 1, app_id: "zed".into(), title: String::new(), pid: None }]), 0);
        e.handle(Input::FocusChanged(Some(1)), 0);
        for t in 1..=90 {
            e.handle(Input::Tick, t * 1000);
        }
        let mut app = app_for(&e, 90_000);
        app.handle_key(key(KeyCode::Char('4')));
        let screen = render(&app, 120, 30);
        assert_shows(&screen, &[
            "Screen time", "All apps", "zed", "1:30", "Opens", "This week", "habits completed", "windows closed",
        ]);
        app.handle_key(key(KeyCode::Down));

        // Filtering, btop-style: live while typing, kept on enter.
        app.handle_key(key(KeyCode::Char('f')));
        for c in "zz".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_shows(&render(&app, 120, 30), &["filter: zz█", "Nothing matches \"zz\"", "All shown", "to filter by app"]);
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Backspace));
        for c in "ZE".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        let screen = render(&app, 120, 30);
        assert_shows(&screen, &["filter: ZE · f edits · esc clears", "zed"]);
        app.handle_key(key(KeyCode::Esc));
        assert!(app.insights_filter.is_empty(), "esc clears the filter before quitting");

        app.handle_key(key(KeyCode::Char('g')));
        assert_shows(&render(&app, 120, 30), &["Screen time · by category", "All categories", "Uncategorized", "g by app"]);
        app.handle_key(key(KeyCode::Char('g')));

        // The hourly chart: stacked by habit, category or app.
        let visit = |key: &str, start, end, habit: Option<&str>| habit_core::archive::Visit {
            key: key.into(),
            start,
            end,
            active_ms: end - start,
            habit: habit.map(str::to_string),
            tmux_session: None,
        };
        let visits = [visit("zed", 0, 90_000, None), visit("zathura", 90_000, 150_000, Some("reading"))];
        app.apps = e.app_insights(7, &visits, 150_000);
        app.breakdown_day = e.day_breakdown(&visits, 0, 150_000, true);
        app.breakdown_period = e.period_breakdown(&visits, 7, 150_000);
        let screen = render(&app, 120, 30);
        assert_shows(&screen, &[
            "Today", "Screen time 4:00", "busiest 00:00", "1:00 on habits", "██",
            "0     3     6", "█ zed 3:00", "█ ✓Reading 1:00", "d 7 days",
        ]);
        // Each label gets its own color in the stacked columns.
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let colors: std::collections::BTreeSet<String> = (20..30)
            .flat_map(|y| (0..120).map(move |x| (x, y)))
            .filter(|&(x, y)| buffer[(x, y)].symbol() == "█")
            .map(|(x, y)| format!("{:?}", buffer[(x, y)].fg))
            .collect();
        assert!(colors.len() >= 2, "one color per label: {colors:?}");

        // Choosing a row shows that app's hours instead of the stack.
        app.handle_key(key(KeyCode::Down));
        assert_shows(&render(&app, 120, 30), &["Today · zed", "Screen time 3:00", "█ zed 3:00"]);
        app.handle_key(key(KeyCode::Up));
        assert_shows(&render(&app, 120, 30), &["█ zed 3:00"]);

        // Stepping through the days reloads the chart for that day.
        assert_eq!(app.handle_key(key(KeyCode::Char('['))), Action::Refresh);
        assert_eq!(app.chart_day, 1);
        app.breakdown_day = e.day_breakdown(&visits, 1, 150_000, false);
        assert_shows(&render(&app, 120, 30), &["Yesterday", "nothing recorded"]);
        assert_eq!(app.handle_key(key(KeyCode::Char('['))), Action::None);
        assert!(app.flash().is_some_and(|f| f.error), "it says when there is nothing older");
        assert_eq!(app.handle_key(key(KeyCode::Char(']'))), Action::Refresh);
        assert_eq!(app.chart_day, 0);
        app.breakdown_day = e.day_breakdown(&visits, 0, 150_000, true);

        assert_eq!(app.handle_key(key(KeyCode::Char('d'))), Action::Refresh);
        assert!(app.chart_period);
        assert_shows(&render(&app, 120, 30), &["Last 7 days"]);
        render(&app, 84, 28);
    }

    #[test]
    fn lock_pane_explains_and_shows_held_back_changes() {
        let mut app = App::new();
        app.set_snapshot(engine().snapshot(0));
        app.handle_key(key(KeyCode::Char('5')));
        assert_shows(&render(&app, 110, 30), &["No commitment lock", "WHILE IT HOLDS", "enter commit"]);

        let mut e = engine();
        e.lock(7 * 86_400_000, "", 0).unwrap();
        e.set_lock_resolution(String::new(), vec!["lowers Book target 20:00 → 5:00".into()]);
        let mut app = App::new();
        app.set_snapshot(e.snapshot(0));
        app.handle_key(key(KeyCode::Char('L')));
        let screen = render(&app, 110, 30);
        assert_shows(&screen, &["committed 7d 0h", "Committed for", "Held back until it ends", "lowers Book target", "e end early (24h)"]);
        app.handle_key(key(KeyCode::Enter));
        assert_shows(&render(&app, 110, 30), &["Extend commitment", "Extend by"]);
    }

    #[test]
    fn timer_session_shows_big_clock_and_round_prompt() {
        let e = engine_at_goal();
        let mut app = App::new();
        app.set_snapshot(e.snapshot(20 * MIN));
        let screen = render(&app, 110, 40);
        assert_shows(&screen, &["Round complete", "round 1 done, 1:00:00 banked", "another 20:00 for 1:00:00", "███"]);
    }

    #[test]
    fn timer_warning_is_shown() {
        let mut e = engine();
        e.start("book", 0).unwrap();
        let mut app = App::new();
        app.timer_warning = Some("tmux focus-events is off".into());
        app.set_snapshot(e.snapshot(0));
        let screen = render(&app, 110, 40);
        assert_shows(&screen, &["can't track focus here", "tmux focus-events is off"]);
    }

    #[test]
    fn settings_and_count_popups() {
        let mut app = App::new();
        app.set_snapshot(engine().snapshot(0));
        app.handle_key(key(KeyCode::Char('o')));
        assert_shows(&render(&app, 100, 30), &[
            "Settings", "New day starts at", "04:00", "Sound when a habit is done", "complete", "Notifications", "on",
        ]);

        let mut app = app_for(&rich_engine(), 7 * MIN);
        app.habit_index = 3;
        app.handle_key(key(KeyCode::Char('=')));
        assert_shows(&render(&app, 100, 30), &["Log Yoga", "Today: 30 poses", "=40 sets"]);
    }

    #[test]
    fn big_clock_renders_digits() {
        let rows = big_clock(65_000); // 1:05
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0], "  █   ███ ███");
        assert_eq!(rows[1], "  █ █ █ █ █  ");
    }

    #[test]
    fn every_pane_renders_disconnected_and_tiny() {
        let app = App::new();
        assert!(render(&app, 60, 20).contains("habitd not reachable"));

        let mut app = app_for(&rich_engine(), 7 * MIN);
        for pane in ['1', '2', '3', '4', '5'] {
            app.handle_key(key(KeyCode::Char(pane)));
            render(&app, 120, 40);
            render(&app, 40, 12);
            render(&app, 10, 3);
        }
        app.handle_key(key(KeyCode::Char('2')));
        app.handle_key(key(KeyCode::Char('u')));
        assert!(render(&app, 60, 20).contains("Unlock Games"));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::Send(Request::Unlock { group: "games".into(), duration_ms: None })
        );
    }

    #[test]
    fn block_and_habit_forms() {
        let mut app = app_for(&rich_engine(), 7 * MIN);
        let mut groups = std::collections::BTreeMap::new();
        groups.insert(
            "games".to_string(),
            serde_json::json!({ "name": "Games", "apps": ["steam"], "requires": ["walk", "yoga"], "require": "all" }),
        );
        let mut habits = std::collections::BTreeMap::new();
        habits.insert("yoga".to_string(), serde_json::json!({ "name": "Yoga", "kind": "counter", "goal": 20, "unit": "poses", "reward": { "groups": ["social"], "duration": "10m" } }));
        let entries = habit_ipc::ConfigEntries { groups, habits };

        app.open_editor(super::super::editor::Section::Groups, Some("games".into()), &entries);
        let screen = render(&app, 130, 40);
        assert_shows(&screen, &[
            "Edit block · Games", "Apps", "✓ steam", "+ add (enter or a)", "Sites", "Unlock mode", "◀ clock time ▶",
            "Do first", "[x] Walk", "[ ] Book", "Needs", "all of them", "Schedule", "none: always blocks",
            "config.toml", "[groups.games]", r#"apps = ["steam"]"#, "ctrl+s save",
        ]);
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char('a')));
        assert_shows(&render(&app, 130, 40), &["Add app", "Find or type"]);

        // A long list scrolls with the choice instead of losing it below.
        let editor = app.editor.as_mut().unwrap();
        editor.context.recent = (0..15).map(|i| (format!("app{i:02}"), 60_000)).collect();
        let screen = render(&app, 130, 40);
        assert_shows(&screen, &["app09", "↓ 5 more"]);
        assert!(!screen.contains("app10") && !screen.contains("↑ "), "{screen}");
        for _ in 0..12 {
            app.handle_key(key(KeyCode::Down));
        }
        let screen = render(&app, 130, 40);
        assert_shows(&screen, &["↑ 3 more", "app12", "↓ 2 more"]);
        assert!(!screen.contains("app02"), "{screen}");
        let chosen = app.editor.as_ref().unwrap().picker.as_ref().unwrap().index;
        assert_eq!(chosen, 12);
        // A short screen shows fewer rows, still with the choice.
        assert_shows(&render(&app, 130, 16), &["app12", "↑ ", "↓ "]);
        app.handle_key(key(KeyCode::Esc));

        app.editor = None;
        app.open_editor(super::super::editor::Section::Habits, Some("yoga".into()), &entries);
        let screen = render(&app, 130, 40);
        assert_shows(&screen, &["Edit habit · Yoga", "counter: a number toward a goal", "Goal", "20", "poses", "Max rounds/day", "[x] Social", "bank as credit"]);
        assert!(!screen.contains("Target"), "{screen}");
        render(&app, 60, 20);

        // Allow rules are edited in the form, from recent use or typed.
        let mut habits = std::collections::BTreeMap::new();
        habits.insert(
            "code".to_string(),
            serde_json::json!({ "name": "Code", "kind": "passive", "allow": [{ "app": "kitty", "tmux_session": "work" }, { "url": "^https?://([^/]*\\.)?github\\.com(/|$)" }] }),
        );
        let entries = habit_ipc::ConfigEntries { groups: Default::default(), habits };
        app.editor = None;
        app.open_editor(super::super::editor::Section::Habits, Some("code".into()), &entries);
        let screen = render(&app, 130, 40);
        assert_shows(&screen, &["Counts in", "✓ kitty in tmux work", "✓ site github.com", "any of them counts"]);
        assert!(!screen.contains("edit in config.toml"), "{screen}");
        let editor = app.editor.as_mut().unwrap();
        editor.cursor = editor.rows().iter().position(|r| matches!(r, super::super::editor::Row::Add { field: 10 })).unwrap();
        app.handle_key(key(KeyCode::Enter));
        assert_shows(&render(&app, 130, 40), &["Add where it counts", "term:nvim, tmux:thesis"]);
    }
}
