//! `hf tui`: interactive dashboard over the habitd socket.
//!
//! The TUI is also the "timer window" for timer habits: it reports its
//! terminal focus (focus-in/out escape sequences) to habitd, re-reporting every
//! few seconds while focused so a killed TUI stops counting.

mod app;
mod editor;
mod form;
mod panes;
mod popup;
mod ui;

use app::{Action, App};
use habit_core::Snapshot;
use habit_ipc::Request;
use ratatui::crossterm::event::{self, DisableFocusChange, EnableFocusChange, Event, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::DefaultTerminal;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

/// Enough for a week of activity in Insights.
const EVENTS_LIMIT: usize = 500;
/// Twenty weeks of heatmap.
const STATS_DAYS: u32 = 140;
const APP_DAYS: u32 = 7;
/// Screen time grows without events, so it's refetched this often anyway.
const REFRESH: Duration = Duration::from_secs(30);
const RECONNECT: Duration = Duration::from_secs(2);
const FRAME: Duration = Duration::from_millis(200);
/// Must stay well below habit_core's TIMER_STALE_MS.
const FOCUS_HEARTBEAT: Duration = Duration::from_secs(2);

enum Message {
    Snapshot(Box<Snapshot>),
    Disconnected,
}

fn stream_snapshots(tx: Sender<Message>) {
    loop {
        if let Ok(lines) = habit_ipc::subscribe() {
            for line in lines {
                let Ok(line) = line else { break };
                if let Ok(snapshot) = serde_json::from_str(&line) {
                    if tx.send(Message::Snapshot(Box::new(snapshot))).is_err() {
                        return;
                    }
                }
            }
        }
        if tx.send(Message::Disconnected).is_err() {
            return;
        }
        std::thread::sleep(RECONNECT);
    }
}

/// Refetches what snapshots don't carry: the log, daily stats, screen time.
fn refresh(app: &mut App) {
    if let Ok(response) = habit_ipc::request(&Request::Events { limit: EVENTS_LIMIT }) {
        if let Some(events) = response.events {
            app.events = events;
        }
    }
    if let Ok(response) = habit_ipc::request(&Request::Stats { days: STATS_DAYS }) {
        if let Some(stats) = response.stats {
            app.stats = stats;
        }
    }
    if let Ok(response) = habit_ipc::request(&Request::AppStats { days: APP_DAYS }) {
        if let Some(apps) = response.app_stats {
            app.apps = apps;
        }
        if let Some(breakdown) = response.breakdown {
            app.breakdown_period = breakdown;
        }
    }
    if let Ok(response) = habit_ipc::request(&Request::HourStats { day_offset: app.chart_day }) {
        if let Some(breakdown) = response.breakdown {
            app.breakdown_day = breakdown;
        }
    }
}

/// Why focus can't be tracked in this terminal, if it can't. tmux only
/// forwards focus events with `focus-events on`; without them the timer would
/// keep counting after switching away.
fn focus_tracking_problem() -> Option<String> {
    std::env::var_os("TMUX")?;
    let output = std::process::Command::new("tmux").args(["show", "-gv", "focus-events"]).output().ok()?;
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (value != "on").then(|| {
        "tmux focus-events is off: run `tmux set -g focus-events on` and add it to your tmux.conf".to_string()
    })
}

/// Decides what to report to habitd about this window's focus. The timer
/// counts while the terminal is focused and no pausing view (settings) is open.
struct FocusState {
    terminal_focused: bool,
    paused: bool,
    reported: Option<bool>,
    last_report: Option<Instant>,
}

impl FocusState {
    fn new() -> Self {
        // The TUI was just launched from this terminal, so it has focus.
        Self { terminal_focused: true, paused: false, reported: None, last_report: None }
    }

    /// The value to send now, if any: on every change, and as a heartbeat
    /// while focused.
    fn due(&mut self, now: Instant) -> Option<bool> {
        let focused = self.terminal_focused && !self.paused;
        let changed = self.reported != Some(focused);
        let heartbeat = focused && self.last_report.is_none_or(|t| now.duration_since(t) >= FOCUS_HEARTBEAT);
        if !changed && !heartbeat {
            return None;
        }
        self.reported = Some(focused);
        self.last_report = Some(now);
        Some(focused)
    }

    /// Forces a fresh report, e.g. after the daemon restarted.
    fn reset(&mut self) {
        self.reported = None;
        self.last_report = None;
    }
}

struct FocusReporter {
    source: String,
    enabled: bool,
    state: FocusState,
}

impl FocusReporter {
    fn sync(&mut self, paused: bool) {
        self.state.paused = paused;
        if !self.enabled {
            return;
        }
        if let Some(focused) = self.state.due(Instant::now()) {
            let request = Request::TimerFocus { source: self.source.clone(), focused: Some(focused) };
            // Best effort: the daemon may be down, and heartbeats retry.
            let _ = habit_ipc::request(&request);
        }
    }

    fn disconnect(&self) {
        if self.enabled {
            let _ = habit_ipc::request(&Request::TimerFocus { source: self.source.clone(), focused: None });
        }
    }
}

pub fn run() -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || stream_snapshots(tx));
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableFocusChange);

    let mut app = App::new();
    app.timer_warning = focus_tracking_problem();
    let mut focus = FocusReporter {
        source: format!("tui-{}", std::process::id()),
        enabled: app.timer_warning.is_none(),
        state: FocusState::new(),
    };

    let result = event_loop(&mut terminal, &rx, &mut app, &mut focus);
    focus.disconnect();
    let _ = execute!(std::io::stdout(), DisableFocusChange);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    rx: &Receiver<Message>,
    app: &mut App,
    focus: &mut FocusReporter,
) -> anyhow::Result<()> {
    let mut refreshed = Instant::now();
    loop {
        while let Ok(message) = rx.try_recv() {
            match message {
                Message::Snapshot(snapshot) => {
                    let was_connected = app.connected;
                    if app.set_snapshot(*snapshot) || !was_connected {
                        refresh(app);
                        refreshed = Instant::now();
                    }
                    if !was_connected {
                        // Re-register focus with a restarted daemon.
                        focus.state.reset();
                    }
                }
                Message::Disconnected => app.connected = false,
            }
        }
        if app.connected && refreshed.elapsed() >= REFRESH {
            refresh(app);
            refreshed = Instant::now();
        }
        focus.sync(app.pauses_timer());

        terminal.draw(|frame| ui::draw(frame, app))?;

        if !event::poll(FRAME)? {
            continue;
        }
        let key = match event::read()? {
            Event::FocusGained => {
                focus.state.terminal_focused = true;
                continue;
            }
            Event::FocusLost => {
                focus.state.terminal_focused = false;
                continue;
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => key,
            _ => continue,
        };
        match app.handle_key(key) {
            Action::None => {}
            Action::Quit => return Ok(()),
            Action::Send(request) => {
                app.apply_response(habit_ipc::request(&request));
                refresh(app);
                refreshed = Instant::now();
            }
            Action::Refresh => {
                refresh(app);
                refreshed = Instant::now();
            }
            Action::OpenEditor { section, id } => match habit_ipc::request(&Request::ConfigEntries) {
                Ok(response) => match (response.ok, &response.config) {
                    (true, Some(entries)) => {
                        app.open_editor(section, id, entries);
                        if let Some(editor) = &mut app.editor {
                            editor.context.tmux_sessions = tmux_sessions();
                        }
                    }
                    _ => app.set_flash(response.error.unwrap_or_else(|| "habitd is too old to edit the config; restart it".into()), true),
                },
                Err(e) => app.set_flash(e, true),
            },
        }
    }
}

/// Names of the tmux sessions on the default server, for allow rules.
fn tmux_sessions() -> Vec<String> {
    std::process::Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .stderr(std::process::Stdio::null())
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_changes_and_heartbeats_only_while_focused() {
        let start = Instant::now();
        let mut state = FocusState::new();
        assert_eq!(state.due(start), Some(true));
        assert_eq!(state.due(start + Duration::from_millis(500)), None);
        assert_eq!(state.due(start + FOCUS_HEARTBEAT), Some(true));

        // Opening settings pauses right away, without heartbeats.
        state.paused = true;
        let t = start + FOCUS_HEARTBEAT + Duration::from_millis(100);
        assert_eq!(state.due(t), Some(false));
        assert_eq!(state.due(t + FOCUS_HEARTBEAT * 3), None);

        // Closing settings resumes.
        state.paused = false;
        assert_eq!(state.due(t + FOCUS_HEARTBEAT * 3), Some(true));

        // Terminal focus loss also reports once.
        state.terminal_focused = false;
        assert_eq!(state.due(t + FOCUS_HEARTBEAT * 4), Some(false));
        assert_eq!(state.due(t + FOCUS_HEARTBEAT * 5), None);

        state.reset();
        assert_eq!(state.due(t + FOCUS_HEARTBEAT * 5), Some(false));
    }
}
