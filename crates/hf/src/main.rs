mod native_host;
mod setup;
mod stats_view;
mod tui;
mod update;
mod web;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use habit_core::config::{HabitKind, RewardMode, Strictness};
use habit_core::duration::{format_duration, format_duration_long, parse_duration};
use habit_core::snapshot::PauseReason;
use habit_core::Snapshot;
use habit_ipc::{Request, Response};
use serde_json::json;

#[derive(Parser)]
#[command(name = "hf", version, about = "habitfocus: blocks apps and sites until your habits are done")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the current session, groups and habits
    Status {
        #[arg(long, conflicts_with = "waybar")]
        json: bool,
        /// Output a Waybar custom-module JSON object
        #[arg(long)]
        waybar: bool,
    },
    /// Interactive dashboard
    Tui,
    /// Recent activity: sessions, unlocks, blocks, commitment changes
    Events {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Screen time per app (and per site when the browser extension reports
    /// tabs): opens, session length and a time-of-day chart
    Apps {
        /// Days to total
        #[arg(long, default_value_t = 7)]
        days: u32,
        /// Only apps, sites and categories containing this, e.g. youtube or games
        /// (their numbers are summed in the chart)
        #[arg(long)]
        app: Option<String>,
        /// Totals per category instead of per app
        #[arg(long)]
        by_category: bool,
        #[arg(long)]
        json: bool,
    },
    /// Streaks and daily focus per habit
    Stats {
        /// Days to show in the grid
        #[arg(long, default_value_t = 28)]
        days: u32,
        /// Print the daily totals as JSON
        #[arg(long)]
        json: bool,
    },
    /// Give an app id or site a readable name in `hf apps` and the TUI:
    /// `hf rename steam_app_275850 "No Man's Sky"`; without a name the id shows again
    Rename {
        /// App id or site key as `hf apps --json` lists it, e.g. site:www.youtube.com
        app: String,
        name: Option<String>,
    },
    /// Put an app id or site in a category for `hf apps --by-category` and the TUI:
    /// `hf category steam_app_275850 Games`; without a category it's taken out
    Category {
        /// App id or site key as `hf apps --json` lists it
        app: String,
        category: Option<String>,
    },
    /// Insights in the browser: timeline, screen time, habits and the log
    /// (read-only, served on 127.0.0.1)
    Web {
        #[arg(long, default_value_t = web::DEFAULT_PORT)]
        port: u16,
        /// Open the page in the default browser
        #[arg(long)]
        open: bool,
    },
    /// Stream updates (one JSON snapshot per line with --json)
    Watch {
        #[arg(long)]
        json: bool,
        /// Keep running across habitd restarts (implies --json). Prints
        /// {"connected":false} while habitd is unreachable and
        /// {"connected":true} as a heartbeat every 10s, for shell plugins.
        #[arg(long)]
        reconnect: bool,
    },
    /// Start a habit, or resume its saved progress from today
    Start { habit: String },
    /// Log a manual or counter habit: `hf done journal`, `hf done anki 25`,
    /// `hf done anki -5` to correct, `hf done anki 40 --set`
    Done {
        habit: String,
        /// Amount to add (default 1); negative corrects a mistake
        #[arg(allow_negative_numbers = true)]
        amount: Option<i64>,
        /// Set today's count to `amount` instead of adding it
        #[arg(long, requires = "amount")]
        set: bool,
    },
    /// Stop the session and keep its progress for later today
    Stop,
    /// Alias of stop: rewards are banked when each round completes
    Finish,
    /// Keep going for another round after one completed
    Continue,
    /// End the session and discard its progress
    Abort {
        /// Required for strict sessions; blocks unlocks for a while afterwards
        #[arg(long)]
        emergency: bool,
    },
    /// Change a setting: day_start, idle_timeout, emergency_penalty, expiry_warning, notifications
    Set { key: String, value: String },
    /// Commitment lock: `hf lock 7d`, `hf lock --until 2026-10-01`, `hf lock status`,
    /// `hf lock end` (after 24h), `hf lock cancel-end`
    Lock {
        /// A duration like 7d or 12h, or: status, end, cancel-end
        what: Option<String>,
        /// Lock until a local date/time: "2026-10-01" or "2026-10-01 18:00"
        #[arg(long, conflicts_with = "what")]
        until: Option<String>,
    },
    /// Spend earned credit to unlock a group (default: all credit; a day-pass
    /// group always costs its rest_of_day_price)
    Unlock {
        group: String,
        /// e.g. 30m, 1h, 90s or plain minutes
        duration: Option<String>,
    },
    /// End an unlock early; unused time is refunded as credit
    Relock { group: String },
    /// Reload the config file
    Reload,
    /// Write the example config if none exists
    Init,
    /// Install the systemd service, browser host and config for this hf and the habitd next to it
    Setup {
        /// Also enable and (re)start habitd
        #[arg(long)]
        start: bool,
        /// Stop and remove the service and browser host (config and data stay)
        #[arg(long, conflicts_with = "start")]
        uninstall: bool,
    },
    /// Install the latest release from GitHub over this one
    Update {
        /// Only report; exits with 10 when a newer version is out
        #[arg(long)]
        check: bool,
    },
    /// Print config, state and socket paths
    Paths,
    /// Register `hf` as native messaging host for the browser extension
    InstallNativeHost {
        #[arg(long)]
        uninstall: bool,
    },
    /// Run as native messaging host (browsers start this automatically)
    NativeHost,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if native_host::is_browser_invocation(&args) {
        if let Err(e) = native_host::run() {
            eprintln!("hf native-host: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(e) = run(Cli::parse()) {
        eprintln!("hf: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Status { json, waybar } => {
            let snapshot = call(Request::Status)?.snapshot.context("missing snapshot")?;
            if json {
                println!("{}", serde_json::to_string(&snapshot)?);
            } else if waybar {
                println!("{}", waybar_json(&snapshot));
            } else {
                print!("{}", render(&snapshot));
            }
        }
        Command::Tui => tui::run()?,
        Command::Web { port, open } => web::run(port, open)?,
        Command::Events { limit, json } => {
            let events = call(Request::Events { limit })?.events.context("missing events")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&events)?);
            } else if events.is_empty() {
                println!("Nothing yet.");
            } else {
                for event in events.iter().rev() {
                    let at = chrono::DateTime::from_timestamp_millis(event.at as i64)
                        .map(|t| t.with_timezone(&chrono::Local).format("%a %H:%M").to_string())
                        .unwrap_or_default();
                    println!("{at}  {}", event.text);
                }
            }
        }
        Command::Apps { days, app, by_category, json } => {
            let response = call(Request::AppStats { days })?;
            let (names, categories) =
                response.snapshot.map(|s| (s.app_names, s.app_categories)).unwrap_or_default();
            let usage = response.app_stats.context("missing app stats")?;
            let rows = stats_view::usage_rows(
                &usage,
                &names,
                &categories,
                app.as_deref().unwrap_or(""),
                by_category,
            );
            if json {
                let rows: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "app": r.key, "name": r.label, "category": r.category,
                            "today_ms": r.today_ms, "total_ms": r.total_ms, "sessions": r.sessions,
                            "sessions_today": r.sessions_today, "avg_session_ms": r.avg_session_ms,
                            "longest_session_ms": r.longest_session_ms, "hours": r.hours,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                print!("{}", render_apps(&rows, days, app.as_deref(), by_category));
            }
        }
        Command::Category { app, category } => print_message(call(Request::SetAppCategory { app, category })?),
        Command::Rename { app, name } => print_message(call(Request::SetAppName { app, name })?),
        Command::Stats { days, json } => {
            let response = call(Request::Stats { days })?;
            let stats = response.stats.context("missing stats")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                let snapshot = response.snapshot.context("missing snapshot")?;
                print!("{}", render_stats(&snapshot, &stats));
            }
        }
        Command::Watch { reconnect: true, .. } => watch_reconnecting(),
        Command::Watch { json, .. } => {
            for line in habit_ipc::subscribe().map_err(anyhow::Error::msg)? {
                let line = line.context("connection to habitd lost")?;
                if json {
                    println!("{line}");
                } else {
                    let snapshot: Snapshot = serde_json::from_str(&line)?;
                    print!("\x1b[2J\x1b[H{}", render(&snapshot));
                }
            }
        }
        Command::Start { habit } => print_message(call(Request::Start { habit })?),
        Command::Done { habit, amount, set } => {
            print_message(call(Request::Done { habit, amount: amount.unwrap_or(1), set })?)
        }
        Command::Stop => print_message(call(Request::Stop)?),
        Command::Finish => print_message(call(Request::Finish)?),
        Command::Continue => print_message(call(Request::Continue)?),
        Command::Abort { emergency } => print_message(call(Request::Abort { emergency })?),
        Command::Set { key, value } => print_message(call(Request::SetSetting { key, value })?),
        Command::Lock { what, until } => lock_command(what.as_deref(), until.as_deref())?,
        Command::Unlock { group, duration } => {
            let duration_ms = duration
                .map(|d| parse_duration(&d))
                .transpose()
                .map_err(anyhow::Error::msg)?;
            print_message(call(Request::Unlock { group, duration_ms })?);
        }
        Command::Relock { group } => print_message(call(Request::Relock { group })?),
        Command::Reload => print_message(call(Request::Reload)?),
        Command::Init => {
            let path = habit_ipc::config_path();
            if !init_config()? {
                bail!("{} already exists", path.display());
            }
            println!("Wrote {}. Edit it, then start habitd.", path.display());
        }
        Command::Setup { start, uninstall } => setup::run(start, uninstall)?,
        Command::Update { check } => update::run(check)?,
        Command::InstallNativeHost { uninstall } => native_host::install(uninstall)?,
        Command::NativeHost => native_host::run()?,
        Command::Paths => {
            println!("config  {}", habit_ipc::config_path().display());
            println!("state   {}", habit_ipc::state_path().display());
            println!("archive {}", habit_ipc::archive_path_for(&habit_ipc::state_path()).display());
            println!("socket  {}", habit_ipc::socket_path().display());
        }
    }
    Ok(())
}

/// `hf watch --reconnect`: never exits on its own, so a shell plugin can run
/// it once as a long-lived stream.
fn watch_reconnecting() -> ! {
    use std::io::{BufRead, BufReader, ErrorKind, Write};
    use std::time::Duration;

    const HEARTBEAT: Duration = Duration::from_secs(10);
    const RETRY: Duration = Duration::from_secs(2);

    let print = |line: &str| {
        let mut out = std::io::stdout().lock();
        // The reader went away (plugin reloaded): nothing left to do.
        if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
            std::process::exit(0);
        }
    };
    loop {
        if let Ok(stream) = habit_ipc::subscribe_stream() {
            let _ = stream.set_read_timeout(Some(HEARTBEAT));
            let mut reader = BufReader::new(stream);
            // Kept across timeouts so a partially received line isn't lost.
            let mut line = String::new();
            loop {
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        print(line.trim_end());
                        line.clear();
                    }
                    Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                        print(r#"{"connected":true}"#);
                    }
                    Err(_) => break,
                }
            }
        }
        print(r#"{"connected":false}"#);
        std::thread::sleep(RETRY);
    }
}

fn parse_local_datetime(text: &str) -> anyhow::Result<u64> {
    use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
    let text = text.trim();
    let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M")
        .or_else(|_| NaiveDate::parse_from_str(text, "%Y-%m-%d").map(|d| d.and_hms_opt(0, 0, 0).expect("midnight")))
        .with_context(|| format!("invalid date {text:?}: use 2026-10-01 or \"2026-10-01 18:00\""))?;
    let local = Local
        .from_local_datetime(&naive)
        .earliest()
        .with_context(|| format!("{text:?} doesn't exist in local time"))?;
    Ok(local.timestamp_millis() as u64)
}

fn lock_command(what: Option<&str>, until: Option<&str>) -> anyhow::Result<()> {
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let request = match (what, until) {
        (_, Some(date)) => Request::Lock { until_ms: parse_local_datetime(date)? },
        (None | Some("status"), None) => {
            let snapshot = call(Request::Status)?.snapshot.context("missing snapshot")?;
            print!("{}", render_lock(&snapshot).unwrap_or_else(|| "No commitment is active.\n".into()));
            return Ok(());
        }
        (Some("end"), None) => Request::LockEnd,
        (Some("cancel-end"), None) => Request::LockCancelEnd,
        (Some(duration), None) => {
            let duration = parse_duration(duration).map_err(anyhow::Error::msg)?;
            Request::Lock { until_ms: now + duration }
        }
    };
    print_message(call(request)?);
    Ok(())
}

fn render_lock(snapshot: &Snapshot) -> Option<String> {
    let lock = snapshot.lock.as_ref()?;
    let ends = chrono::DateTime::from_timestamp_millis(lock.ends_at_ms as i64)
        .map(|t| t.with_timezone(&chrono::Local).format("%a %Y-%m-%d %H:%M").to_string())
        .unwrap_or_default();
    let mut out = format!("Lock     committed for {} (until {ends})", format_duration_long(lock.remaining_ms));
    if lock.end_requested {
        out += ", early end requested";
    }
    if lock.extended_ms > 0 {
        out += &format!(", extended {} for stopping habitd", format_duration_long(lock.extended_ms));
    }
    out += "\n";
    for change in &lock.pending {
        out += &format!("         held back: {change}\n");
    }
    Some(out)
}

fn render_apps(rows: &[stats_view::UsageRow], days: u32, filter: Option<&str>, by_category: bool) -> String {
    if rows.is_empty() {
        return match filter {
            Some(filter) => format!("No screen time for apps, sites or categories matching {filter:?}.\n"),
            None => "No screen time recorded yet.\n".into(),
        };
    }
    let total = stats_view::usage_total(rows, "all");
    let mut out = format!(
        "Screen time  today {} · last {days} days {}\n\n",
        format_duration(total.today_ms),
        format_duration(total.total_ms)
    );
    let width = rows.iter().map(|r| r.label.chars().count()).max().unwrap_or(3).clamp(8, 32);
    let categories = !by_category && rows.iter().any(|r| r.category.is_some());
    let max = rows.iter().map(|r| r.total_ms).max().unwrap_or(1).max(1);
    let first = if by_category { "Category" } else { "App" };
    let category_head = if categories { format!("  {:12}", "category") } else { String::new() };
    out += &format!(
        "{first:width$}{category_head}  {:>9}  {:>10}  {:>6}  {:>9}\n",
        "today",
        format!("{days} days"),
        "opens",
        "avg"
    );
    for r in rows.iter().take(25) {
        let bar = "█".repeat(((r.total_ms * 20).div_ceil(max)) as usize);
        let name: String = r.label.chars().take(width).collect();
        let category = if categories {
            format!("  {:12}", r.category.as_deref().unwrap_or("").chars().take(12).collect::<String>())
        } else {
            String::new()
        };
        let (opens, avg) = if r.sessions > 0 {
            (r.sessions.to_string(), format_duration(r.avg_session_ms))
        } else {
            ("-".into(), "-".into())
        };
        out += &format!(
            "{name:width$}{category}  {:>9}  {:>10}  {opens:>6}  {avg:>9}  {bar}\n",
            format_duration(r.today_ms),
            format_duration(r.total_ms)
        );
    }
    if rows.len() > 25 {
        out += &format!("… {} more (hf apps --json)\n", rows.len() - 25);
    }

    let (hours, only) = match rows {
        [only] => (only.hours.clone(), Some(only)),
        _ => (total.hours.clone(), None),
    };
    if let Some(peak) = stats_view::peak_hour(&hours) {
        let what = match (only, filter) {
            (Some(r), _) => match &r.key {
                Some(key) if stats_view::strip_key(key) != r.label => format!("{} ({key})", r.label),
                _ => r.label.clone(),
            },
            (None, Some(filter)) => format!("everything matching {filter:?}"),
            (None, None) => "all apps".into(),
        };
        out += &format!("\nTime of day · {what} · most used {peak}\n");
        for row in stats_view::hour_chart(&hours, 4) {
            out += &format!("  {}\n", row.trim_end());
        }
        out += &format!("  {}\n", stats_view::hour_axis().trim_end());
        if let Some(r) = only {
            out += &format!(
                "\nopened {}× today, {}× in {days} days · average {} · longest {}\n",
                r.sessions_today,
                r.sessions,
                format_duration(r.avg_session_ms),
                format_duration(r.longest_session_ms)
            );
        }
    }
    out
}

fn render_stats(snapshot: &Snapshot, days: &[habit_core::snapshot::DayView]) -> String {
    use stats_view::{day_grid, focused_in_last, streak_label};
    let mut out = format!("Streak   {} with a completed habit\n\n", streak_label(snapshot.streak_days));
    let name_width = snapshot.habits.iter().map(|h| h.name.chars().count()).max().unwrap_or(5).max(5);
    let first = days.first().map_or("", |d| d.date.as_str());
    out += &format!(
        "{:name_width$}  {:>8}  {:>8}  {:>9}  {:>9}  last {} days (from {first}, ■ done ▪ some focus)\n",
        "Habit",
        "streak",
        "best",
        "today",
        "7 days",
        days.len()
    );
    for h in &snapshot.habits {
        let today = if h.done_today {
            format!("✓ {}", format_duration(h.today_focused_ms))
        } else {
            format_duration(h.today_focused_ms)
        };
        out += &format!(
            "{:name_width$}  {:>8}  {:>8}  {:>9}  {:>9}  {}\n",
            h.name,
            streak_label(h.streak_days),
            streak_label(h.best_streak_days),
            today,
            format_duration(focused_in_last(days, &h.id, 7)),
            day_grid(days, &h.id)
        );
    }
    out
}

fn call(request: Request) -> anyhow::Result<Response> {
    let response = habit_ipc::request(&request).map_err(anyhow::Error::msg)?;
    if !response.ok {
        bail!("{}", response.error.unwrap_or_else(|| "request failed".into()));
    }
    Ok(response)
}

fn print_message(response: Response) {
    if let Some(message) = response.message {
        println!("{message}");
    }
}

fn session_line(snapshot: &Snapshot) -> Option<String> {
    let s = snapshot.session.as_ref()?;
    let status = match (s.awaiting_decision, s.pause_reason, s.kind) {
        (true, _, _) => "round done: `hf continue` for another, `hf stop` to end".to_string(),
        (false, None, _) => "running".to_string(),
        (false, Some(PauseReason::Unfocused), HabitKind::Timer) => "paused (hf tui not focused)".to_string(),
        (false, Some(PauseReason::Unfocused), _) => "paused (allowed window not focused)".to_string(),
        (false, Some(PauseReason::Idle), _) => "paused (idle)".to_string(),
    };
    let strict = if s.strictness == Strictness::Strict { " [strict]" } else { "" };
    let rounds = match s.rounds_completed {
        0 => String::new(),
        n => format!(", {n} round(s) done, {} earned", format_duration(s.banked_ms)),
    };
    Some(format!(
        "{}: {} / {} ({:.0}%), {status}{strict}{rounds}",
        s.name,
        format_duration(s.elapsed_ms),
        format_duration(s.target_ms),
        s.progress * 100.0
    ))
}

fn render(snapshot: &Snapshot) -> String {
    let mut out = String::new();
    if let Some(update) = &snapshot.update {
        out += &format!("habitfocus {} is available: hf update ({})\n\n", update.version, update.url);
    }
    match session_line(snapshot) {
        Some(line) => out += &format!("Session  {line}\n"),
        None => out += "Session  none\n",
    }
    if let Some(lock) = render_lock(snapshot) {
        out += &lock;
    }
    if snapshot.penalty_remaining_ms > 0 {
        out += &format!(
            "Penalty  unlocks refused for {}\n",
            format_duration(snapshot.penalty_remaining_ms)
        );
    }

    out += "\nGroups\n";
    let width = snapshot.groups.iter().map(|g| g.id.len()).max().unwrap_or(0);
    for g in &snapshot.groups {
        let status = match (g.off_schedule, g.blocked, &g.schedule_label) {
            (true, _, label) => label.clone().unwrap_or_else(|| "off hours".into()),
            (false, true, Some(label)) => format!("blocked {label}"),
            (false, true, None) => "blocked".to_string(),
            (false, false, _) => format!("unlocked, {}", g.unlock_label.as_deref().unwrap_or("open")),
        };
        let credit = if g.credit_ms > 0 {
            format!(", credit {} (expires {})", format_duration(g.credit_ms), snapshot.credit_expires_label)
        } else {
            String::new()
        };
        out += &format!("  {:width$}  {status}{credit}\n", g.id);
        if !g.requires.is_empty() {
            let list: Vec<String> = g
                .requires
                .iter()
                .map(|r| {
                    let mark = if r.done { "✓" } else { "·" };
                    format!("{mark} {} {:.0}%", r.name, r.progress * 100.0)
                })
                .collect();
            let rule = if g.require_all { "needs all" } else { "needs one" };
            out += &format!("  {:width$}    {rule}: {}\n", "", list.join(", "));
        }
    }

    out += "\nHabits\n";
    let width = snapshot.habits.iter().map(|h| h.id.len()).max().unwrap_or(0);
    for h in &snapshot.habits {
        let mode = match h.reward_mode {
            RewardMode::Bank => "banked",
            RewardMode::Immediate => "immediate",
        };
        let strict = if h.strictness == Strictness::Strict { ", strict" } else { "" };
        let timer = if h.kind == HabitKind::Timer { ", timer" } else { "" };
        if h.kind == HabitKind::Passive {
            let today = format_duration(h.today_focused_ms);
            out += &match (h.target_ms, h.reward_groups.is_empty()) {
                (0, _) => format!("  {:width$}  {} {today} today (passive, tracked)\n", h.id, h.name),
                (goal, true) => format!("  {:width$}  {} {today} / {} today (passive)\n", h.id, h.name, format_duration(goal)),
                (goal, false) => format!(
                    "  {:width$}  {} {today} / {} today → {} +{} (passive, {mode})\n",
                    h.id,
                    h.name,
                    format_duration(goal),
                    h.reward_groups.join(", "),
                    format_duration(h.reward_ms),
                ),
            };
            continue;
        }
        if !h.kind.is_timed() {
            let limit = h.daily_limit.map(|l| format!(", max {l}/day")).unwrap_or_default();
            let count = if h.kind == HabitKind::Manual && h.goal == 1 {
                if h.count_today > 0 { "done" } else { "to do" }.to_string()
            } else if h.daily_limit.is_some_and(|l| h.rounds_today >= l) {
                format!("{} {}", h.count_today, h.unit)
            } else {
                format!("{} / {} {}", h.count_today, h.goal * u64::from(h.rounds_today + 1), h.unit)
            };
            out += &format!(
                "  {:width$}  {} {count} → {} +{} ({mode}, {} round(s) today{limit})\n",
                h.id,
                h.name,
                h.reward_groups.join(", "),
                format_duration(h.reward_ms),
                h.rounds_today,
            );
            continue;
        }
        let saved = if h.saved_ms > 0 {
            format!(", {} saved today", format_duration(h.saved_ms))
        } else {
            String::new()
        };
        out += &format!(
            "  {:width$}  {} {} → {} +{} ({mode}{strict}{timer}{saved})\n",
            h.id,
            h.name,
            format_duration(h.target_ms),
            h.reward_groups.join(", "),
            format_duration(h.reward_ms),
        );
    }
    out
}

/// Writes the example config unless there is one; whether it wrote it.
fn init_config() -> anyhow::Result<bool> {
    let path = habit_ipc::config_path();
    if path.exists() {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, habit_core::EXAMPLE_CONFIG)?;
    Ok(true)
}

fn waybar_json(snapshot: &Snapshot) -> serde_json::Value {
    let unlocked = snapshot
        .groups
        .iter()
        .filter(|g| !g.blocked && !g.off_schedule)
        .max_by_key(|g| g.unlock_remaining_ms);
    let (text, class) = if let Some(s) = &snapshot.session {
        let text = format!("{} {}", format_duration(s.remaining_ms), s.name);
        (text, if s.running { "running" } else { "paused" })
    } else if let Some(g) = unlocked {
        (format!("{} {}", g.name, format_duration(g.unlock_remaining_ms)), "unlocked")
    } else {
        ("locked".to_string(), "locked")
    };
    let percentage = snapshot.session.as_ref().map_or(0.0, |s| (s.progress * 100.0).round());
    json!({ "text": text, "tooltip": render(snapshot), "class": class, "percentage": percentage })
}

