mod archive;
mod config_edit;
mod heartbeat;
mod hyprland;
mod idle;
mod niri;
mod procscan;
mod server;
mod sound;
mod sync;
mod settings;
mod terminal;
mod update;

use anyhow::Context;
use habit_core::duration::{format_duration, format_duration_long};
use habit_core::lock::{resolve_locked, unexplained_downtime, weakenings};
use habit_core::{BrowserTab, Config, Effect, Engine, Input, Snapshot, State};
use habit_ipc::{Request, Response};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, oneshot, watch};

pub enum Event {
    Input(Input),
    Request(Request, oneshot::Sender<Response>),
    /// The answer of the daily release check.
    UpdateChecked(Result<Option<habit_ipc::update::Release>, String>),
}

/// The compositor whose windows are tracked and closed or focused.
#[derive(Clone, Copy, Debug)]
enum Compositor {
    Niri,
    Hyprland,
}

impl Compositor {
    fn detect() -> Option<Compositor> {
        if niri::available() {
            Some(Compositor::Niri)
        } else if hyprland::available() {
            Some(Compositor::Hyprland)
        } else {
            None
        }
    }

    fn spawn_events(self, tx: mpsc::Sender<Event>) {
        match self {
            Compositor::Niri => niri::spawn_events(tx),
            Compositor::Hyprland => hyprland::spawn_events(tx),
        }
    }

    fn close_window(self, id: u64) {
        match self {
            Compositor::Niri => niri::close_window(id),
            Compositor::Hyprland => hyprland::close_window(id),
        }
    }

    fn focus_window(self, id: u64) {
        match self {
            Compositor::Niri => niri::focus_window(id),
            Compositor::Hyprland => hyprland::focus_window(id),
        }
    }
}

const PROCESS_SCAN_EVERY_TICKS: u64 = 5;
const SAVE_EVERY_TICKS: u64 = 30;
/// Heartbeat persistence while a commitment is active (bounds how much
/// downtime can go unmeasured).
const HEARTBEAT_EVERY_TICKS: u64 = 10;
/// How often the program in a focused terminal is looked up (and right
/// away when focus moves to a terminal).
const TERMINAL_CHECK_EVERY_TICKS: u64 = 2;
/// Screen time is saved on its own, slower cadence.
const USAGE_SAVE_EVERY_TICKS: u64 = 60;

/// Local UTC offset in milliseconds at a UTC instant (follows DST).
fn local_offset_ms(utc_ms: u64) -> i64 {
    use chrono::{Local, Offset, TimeZone};
    Local
        .timestamp_millis_opt(utc_ms as i64)
        .single()
        .map_or(0, |t| t.offset().fix().local_minus_utc() as i64 * 1000)
}

fn now_ms() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    now.saturating_add_signed(clock_offset_ms())
}

/// Debug builds can shift their clock with `HABITFOCUS_NOW_OFFSET_MS`, which
/// makes day boundaries and schedules testable in minutes instead of hours.
#[cfg(debug_assertions)]
fn clock_offset_ms() -> i64 {
    use std::sync::OnceLock;
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(|| {
        std::env::var("HABITFOCUS_NOW_OFFSET_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0)
    })
}

/// Release builds ignore the offset: it would be a commitment-lock loophole.
#[cfg(not(debug_assertions))]
fn clock_offset_ms() -> i64 {
    0
}

fn read_config_text(path: &Path) -> anyhow::Result<String> {
    std::fs::read_to_string(path)
        .with_context(|| format!("cannot read {} (create one with `hf init`)", path.display()))
}

fn load_state(path: &Path) -> anyhow::Result<State> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(State::default());
    };
    match State::from_json(&text) {
        Ok(state) => Ok(state),
        // Starting fresh would silently end a commitment and drop credit and
        // streaks, so keep the file and refuse when a lock is in it.
        Err(e) if text.contains("\"lock\"") => Err(anyhow::anyhow!(
            "{} holds a commitment but cannot be read ({e}). Fix or move the file; \
             starting fresh would end the commitment.",
            path.display()
        )),
        Err(e) => {
            let broken = path.with_extension("json.bad");
            let _ = std::fs::rename(path, &broken);
            eprintln!(
                "habitd: unreadable state {} ({e}); moved to {} and starting fresh",
                path.display(),
                broken.display()
            );
            Ok(State::default())
        }
    }
}

fn save_state(path: &Path, state: &State) {
    let result = (|| -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, state.to_json())?;
        std::fs::rename(tmp, path)
    })();
    if let Err(e) = result {
        eprintln!("habitd: failed to save state to {}: {e}", path.display());
    }
}

fn held_back_message(ends_in: u64, pending: &[String]) -> String {
    format!(
        "Committed for another {}; held back until then: {}",
        format_duration_long(ends_in),
        pending.join("; ")
    )
}

struct Daemon {
    engine: Engine,
    /// Text of the config in force (the lock baseline while committed).
    config_text: String,
    config_path: PathBuf,
    state_path: PathBuf,
    /// `history.db`; `None` if it can't be opened (the daemon runs without it).
    archive: Option<archive::Archive>,
    /// An archive write failed; logged once until one succeeds again.
    archive_failing: bool,
    /// `None`: no supported compositor, windows are neither tracked nor closed.
    compositor: Option<Compositor>,
}

impl Daemon {
    fn start(config_path: PathBuf, state_path: PathBuf, now: u64) -> anyhow::Result<Daemon> {
        let state = load_state(&state_path)?;
        let file_text = read_config_text(&config_path);
        let locked_baseline = state
            .lock
            .as_ref()
            .filter(|lock| now < lock.ends_at())
            .map(|lock| lock.baseline.clone());

        let (config, config_text, pending) = match (&locked_baseline, file_text) {
            (Some(baseline), Ok(file)) => match resolve_locked(baseline, &file) {
                Ok(resolved) => (resolved.config, resolved.baseline, Some(resolved.pending)),
                Err(e) => {
                    eprintln!("habitd: {e}; running the committed config");
                    let config = Config::from_toml(baseline).map_err(|e| anyhow::anyhow!(e))?;
                    (config, baseline.clone(), None)
                }
            },
            (Some(baseline), Err(e)) => {
                eprintln!("habitd: {e:#}; running the committed config");
                let config = Config::from_toml(baseline).map_err(|e| anyhow::anyhow!(e))?;
                (config, baseline.clone(), None)
            }
            (None, file) => {
                let file = file?;
                let config = Config::from_toml(&file)
                    .map_err(|e| anyhow::anyhow!(e))
                    .with_context(|| format!("invalid config {}", config_path.display()))?;
                (config, file, None)
            }
        };

        // Loaded anyway so habitd still runs; the engine skips these.
        for entry in config.lockout_entries() {
            eprintln!("habitd: ignoring: {entry}");
        }
        let mut engine = Engine::new(config, state).with_local_offset(local_offset_ms);
        engine.backfill_day_stats();
        if let Some(pending) = pending {
            if !pending.is_empty() {
                eprintln!("habitd: commitment active, holding back: {}", pending.join("; "));
            }
            engine.set_lock_resolution(config_text.clone(), pending);
        }
        let archive_path = habit_ipc::archive_path_for(&state_path);
        let archive = match archive::Archive::open(&archive_path, engine.state()) {
            Ok(archive) => Some(archive),
            Err(e) => {
                eprintln!("habitd: cannot open {} ({e:#}); running without the archive", archive_path.display());
                None
            }
        };
        Ok(Daemon {
            engine,
            config_text,
            config_path,
            state_path,
            archive,
            archive_failing: false,
            compositor: Compositor::detect(),
        })
    }

    /// Extends the lock for unexplained downtime since the last heartbeat.
    fn check_downtime(&mut self, now: u64) {
        let Some(previous) = self.engine.state().heartbeat.clone() else {
            return;
        };
        let downtime =
            unexplained_downtime(&previous, &heartbeat::boot_id(), heartbeat::monotonic_ms(), heartbeat::tamper_grace_ms());
        if let Some(downtime) = downtime {
            let extended_before = self.engine.state().lock.as_ref().map_or(0, |l| l.extended_ms);
            let effects = self.engine.apply_downtime(previous.wall_ms, downtime);
            let extended_after = self.engine.state().lock.as_ref().map_or(0, |l| l.extended_ms);
            if extended_after > extended_before {
                eprintln!("habitd: down for {} during a commitment; lock extended", format_duration_long(downtime));
            }
            self.apply(effects, now);
        }
    }

    /// Writes what the engine queued for the archive.
    fn write_archive(&mut self) {
        let records = self.engine.take_archive();
        let Some(archive) = &mut self.archive else { return };
        match archive.write(&records) {
            Ok(()) => self.archive_failing = false,
            Err(e) if !self.archive_failing => {
                eprintln!("habitd: archive write failed: {e:#}");
                self.archive_failing = true;
            }
            Err(_) => {}
        }
    }

    fn write_heartbeat(&mut self, now: u64, clean_shutdown: bool) {
        self.engine.set_heartbeat(heartbeat::now(now, clean_shutdown));
        save_state(&self.state_path, self.engine.state());
    }

    fn apply(&mut self, effects: Vec<Effect>, now: u64) {
        for effect in effects {
            match effect {
                Effect::CloseWindow(id) => {
                    if let Some(compositor) = self.compositor {
                        compositor.close_window(id);
                    }
                }
                Effect::FocusWindow(id) => {
                    if let Some(compositor) = self.compositor {
                        compositor.focus_window(id);
                    }
                }
                Effect::Notify { title, body } => {
                    tokio::spawn(async move {
                        let _ = tokio::process::Command::new("notify-send")
                            .args(["--app-name=habitfocus", &title, &body])
                            .status()
                            .await;
                    });
                }
                Effect::PlaySound(name) => sound::play(&name),
                Effect::ReloadConfig => match self.reload(now) {
                    Ok(message) => eprintln!("habitd: commitment ended: {message}"),
                    Err(e) => eprintln!("habitd: commitment ended, but the config can't be loaded: {e}"),
                },
            }
        }
    }

    /// Loads the config file, respecting an active commitment.
    fn reload(&mut self, now: u64) -> Result<String, String> {
        let file = read_config_text(&self.config_path).map_err(|e| format!("{e:#}"))?;
        let Some(baseline) = self.engine.lock_baseline(now).map(str::to_string) else {
            let config = Config::from_toml(&file).map_err(|e| format!("invalid config: {e}"))?;
            config.check_lockout(Some(self.engine.config())).map_err(|e| format!("invalid config: {e}"))?;
            let effects = self.engine.reload(config, now);
            self.config_text = file;
            self.apply(effects, now);
            return Ok("Config reloaded".into());
        };
        let resolved = resolve_locked(&baseline, &file).map_err(|e| format!("invalid config: {e}"))?;
        let pending = resolved.pending.clone();
        self.engine.set_lock_resolution(resolved.baseline.clone(), resolved.pending);
        if !pending.is_empty() {
            let ends_in = self.engine.snapshot(now).lock.map_or(0, |l| l.remaining_ms);
            return Err(held_back_message(ends_in, &pending));
        }
        let effects = self.engine.reload(resolved.config, now);
        self.config_text = resolved.baseline;
        self.apply(effects, now);
        Ok("Config reloaded (stricter changes apply during the commitment)".into())
    }

    fn set_setting(&mut self, key: &str, value: &str, now: u64) -> Result<String, String> {
        let file = read_config_text(&self.config_path).map_err(|e| format!("{e:#}"))?;
        let (updated, config) = settings::apply(&file, key, value)?;
        self.write_config(updated, config, now)?;
        Ok(format!("Set {key} = {value}"))
    }

    /// A hint when no screen time was recorded under `key` (ids are exact).
    fn unknown_key_note(&self, key: &str) -> &'static str {
        if self.engine.has_usage(key) {
            ""
        } else {
            " (no screen time recorded under that id yet; ids are case-sensitive, see `hf apps --json`)"
        }
    }

    fn set_app_label(&mut self, table: &str, app: &str, label: Option<&str>, now: u64) -> Result<(), String> {
        let file = read_config_text(&self.config_path).map_err(|e| format!("{e:#}"))?;
        let (updated, config) = config_edit::set_app_label(&file, table, app, label)?;
        self.write_config(updated, config, now)
    }

    fn edit_config(
        &mut self,
        section: &str,
        id: &str,
        table: Option<serde_json::Value>,
        create: bool,
        now: u64,
    ) -> Result<String, String> {
        let file = read_config_text(&self.config_path).map_err(|e| format!("{e:#}"))?;
        let table = match &table {
            Some(serde_json::Value::Object(map)) => Some(map),
            Some(_) => return Err("the entry must be a table".into()),
            None => None,
        };
        let (updated, config) = config_edit::apply(&file, section, id, table, create)?;
        let deleted = table.is_none();
        self.write_config(updated, config, now)?;
        let noun = config_edit::noun(section);
        let name = if section == "groups" { self.engine.config().group_name(id) } else { self.engine.config().habit_name(id) };
        Ok(match (deleted, create) {
            (true, _) => format!("Deleted {noun} {id}"),
            (false, true) => format!("Created {noun} {name}"),
            (false, false) => format!("Saved {noun} {name}"),
        })
    }

    /// Writes an edited config text and runs it, unless a commitment forbids
    /// what it makes easier.
    fn write_config(&mut self, updated: String, config: Config, now: u64) -> Result<(), String> {
        if let Some(baseline) = self.engine.lock_baseline(now) {
            let baseline = Config::from_toml(baseline)?;
            let weaker = weakenings(&baseline, &config);
            if !weaker.is_empty() {
                let ends_in = self.engine.snapshot(now).lock.map_or(0, |l| l.remaining_ms);
                return Err(format!(
                    "Not during your commitment ({} left): {}",
                    format_duration_long(ends_in),
                    weaker.join("; ")
                ));
            }
        }
        settings::write_file(&self.config_path, &updated)?;
        if self.engine.lock_baseline(now).is_some() {
            self.engine.set_lock_resolution(updated.clone(), Vec::new());
        }
        let effects = self.engine.reload(config, now);
        self.config_text = updated;
        self.apply(effects, now);
        Ok(())
    }

    fn handle_request(&mut self, request: Request, now: u64) -> Response {
        let engine = &mut self.engine;
        if let Request::History { limit } = request {
            let mut response = Response::ok(None, engine.snapshot(now));
            response.history = Some(engine.history(limit));
            return response;
        }
        if let Request::Stats { days } = request {
            let mut response = Response::ok(None, engine.snapshot(now));
            response.stats = Some(engine.day_stats(days.min(3660), now));
            return response;
        }
        if let Request::Events { limit } = request {
            let mut response = Response::ok(None, engine.snapshot(now));
            // The archive keeps everything; state.json only the last 500.
            let archived = self.archive.as_ref().and_then(|a| a.events(limit.min(100_000)).ok());
            response.events = Some(archived.unwrap_or_else(|| engine.events(limit.min(500))));
            return response;
        }
        if let Request::ConfigEntries = request {
            let entries = read_config_text(&self.config_path)
                .map_err(|e| format!("{e:#}"))
                .and_then(|text| config_edit::entries(&text));
            return match entries {
                Ok((groups, habits)) => {
                    let mut response = Response::ok(None, self.engine.snapshot(now));
                    response.config = Some(habit_ipc::ConfigEntries { groups, habits });
                    response
                }
                Err(e) => Response::err(e),
            };
        }
        if let Request::HourStats { day_offset } | Request::Timeline { day_offset } = request {
            let day = engine.day_of(now) - i64::from(day_offset);
            let (from, to) = (engine.day_start_ms(day), engine.day_start_ms(day + 1).min(now));
            let archive = self.archive.as_ref();
            let visits = archive.and_then(|a| a.visits(from, to).ok()).unwrap_or_default();
            let more_before = archive.and_then(|a| a.first_visit().ok().flatten()).is_some_and(|first| first < from);
            let mut response = Response::ok(None, engine.snapshot(now));
            response.breakdown = Some(engine.day_breakdown(&visits, day_offset, now, more_before));
            if let Request::Timeline { .. } = request {
                let afk = archive.and_then(|a| a.afk(from, to).ok()).unwrap_or_default();
                response.timeline = Some(engine.day_timeline(&visits, &afk, day_offset, now, more_before));
            }
            return response;
        }
        if let Request::AppStats { days } = request {
            let days = days.min(3660);
            let mut response = Response::ok(None, engine.snapshot(now));
            let from = engine.usage_period_start(days, now);
            let visits = self.archive.as_ref().and_then(|a| a.visits(from, now).ok()).unwrap_or_default();
            response.breakdown = Some(engine.period_breakdown(&visits, days, now));
            response.app_stats = Some(engine.app_insights(days, &visits, now));
            return response;
        }
        let result: Result<(Vec<Effect>, Option<String>), String> = match request {
            Request::Status
            | Request::History { .. }
            | Request::Stats { .. }
            | Request::Events { .. }
            | Request::AppStats { .. }
            | Request::HourStats { .. }
            | Request::Timeline { .. }
            | Request::ConfigEntries => {
                Ok((Vec::new(), None))
            }
            Request::Subscribe => Err("subscribe is handled by the connection".into()),
            Request::Start { habit } => engine.start(&habit, now).map(|(fx, msg)| (fx, Some(msg))),
            Request::Stop => engine.stop(now).map(|(fx, msg)| (fx, Some(msg))),
            Request::Finish => engine.finish(now).map(|(fx, msg)| (fx, Some(msg))),
            Request::Continue => engine.continue_session(now).map(|(fx, msg)| (fx, Some(msg))),
            Request::TimerFocus { source, focused } => {
                Ok((engine.handle(Input::TimerFocus { source, focused }, now), None))
            }
            Request::BrowserHello { source, pid } => {
                Ok((engine.handle(Input::BrowserHello { source, pid }, now), None))
            }
            Request::Abort { emergency } => {
                engine.abort(emergency, now).map(|fx| (fx, Some("Session ended".to_string())))
            }
            Request::Unlock { group, duration_ms } => engine.unlock(&group, duration_ms, now).map(|(fx, granted)| {
                let msg = format!("{} unlocked for {}", engine.config().group_name(&group), format_duration(granted));
                (fx, Some(msg))
            }),
            Request::Done { habit, amount, set } => {
                engine.log_done(&habit, amount, set, now).map(|(fx, msg)| (fx, Some(msg)))
            }
            Request::Relock { group } => engine
                .relock(&group, now)
                .map(|fx| (fx, Some(format!("{} locked", engine.config().group_name(&group))))),
            Request::BrowserTab { source, window, title, url } => {
                let tab = title.zip(url).map(|(title, url)| BrowserTab { title, url });
                Ok((engine.handle(Input::BrowserTab { source, window, tab }, now), None))
            }
            Request::BrowserMedia { source, urls } => {
                Ok((engine.handle(Input::BrowserMedia { source, urls }, now), None))
            }
            Request::SyncRegister { .. }
            | Request::SyncLogin { .. }
            | Request::SyncLogout
            | Request::SyncStatus
            | Request::SyncNow
            | Request::SyncKey => Err("sync requests go to the sync thread".into()),
            Request::Lock { until_ms } => {
                let text = self.config_text.clone();
                self.engine.lock(until_ms, &text, now).map(|msg| (Vec::new(), Some(msg)))
            }
            Request::LockEnd => self.engine.request_lock_end(now).map(|msg| (Vec::new(), Some(msg))),
            Request::LockCancelEnd => self.engine.cancel_lock_end(now).map(|msg| (Vec::new(), Some(msg))),
            Request::Reload => self.reload(now).map(|msg| (Vec::new(), Some(msg))),
            Request::SetSetting { key, value } => {
                self.set_setting(&key, &value, now).map(|msg| (Vec::new(), Some(msg)))
            }
            Request::SetAppName { app, name } => self.set_app_label("app_names", &app, name.as_deref(), now).map(|()| {
                let message = match name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                    Some(name) => format!("{app} is now called {name}{}", self.unknown_key_note(&app)),
                    None => match self.engine.config().default_app_name(&app) {
                        Some(name) => format!("{app} is called {name} again"),
                        None => format!("{app} shows its id again"),
                    },
                };
                (Vec::new(), Some(message))
            }),
            Request::SetAppCategory { app, category } => {
                self.set_app_label("app_categories", &app, category.as_deref(), now).map(|()| {
                    let name = self.engine.config().app_name(&app).to_string();
                    let message = match category.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
                        Some(category) => format!("{name} is in {category}{}", self.unknown_key_note(&app)),
                        None => match self.engine.config().default_app_category(&app) {
                            Some(category) => format!("{name} is in {category} again"),
                            None => format!("{name} has no category"),
                        },
                    };
                    (Vec::new(), Some(message))
                })
            }
            Request::EditConfig { section, id, table, create } => {
                self.edit_config(&section, &id, table, create, now).map(|msg| (Vec::new(), Some(msg)))
            }
        };
        match result {
            Ok((effects, message)) => {
                self.apply(effects, now);
                Response::ok(message, self.engine.snapshot(now))
            }
            Err(e) => Response::err(e),
        }
    }
}

/// Snapshot comparison that ignores the clock.
fn same_ignoring_time(a: &Snapshot, b: &Snapshot) -> bool {
    let mut b = b.clone();
    b.now_ms = a.now_ms;
    *a == b
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let socket_path = habit_ipc::socket_path();
    let now = now_ms();
    let mut daemon = Daemon::start(habit_ipc::config_path(), habit_ipc::state_path(), now)?;
    daemon.check_downtime(now);
    daemon.engine.log_startup(now);
    daemon.write_heartbeat(now, false);

    let (tx, mut rx) = mpsc::channel::<Event>(256);
    let (snapshot_tx, snapshot_rx) = watch::channel(daemon.engine.snapshot(now));

    let listener = server::bind(&socket_path)
        .with_context(|| format!("cannot listen on {}", socket_path.display()))?;
    server::spawn(listener, tx.clone(), snapshot_rx);

    match daemon.compositor {
        Some(compositor) => {
            eprintln!("habitd: tracking windows through {compositor:?}");
            compositor.spawn_events(tx.clone());
        }
        None => eprintln!(
            "habitd: neither NIRI_SOCKET nor HYPRLAND_INSTANCE_SIGNATURE is set; \
             window blocking and focus tracking are disabled"
        ),
    }
    let mut idle_watch = idle::Watch::default();
    idle_watch.poll(daemon.engine.config().general.idle_timeout, &tx);

    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut ticks: u64 = 0;
    let mut terminal_watch = terminal::Watch::default();
    let mut update_checker = update::Checker::default();
    // Sync reads and writes history.db through a connection of its own.
    let sync = daemon.archive.is_some().then(|| {
        let archive_path = habit_ipc::archive_path_for(&daemon.state_path);
        sync::spawn(archive_path, daemon.state_path.with_file_name("sync.json"))
    });

    eprintln!("habitd: listening on {}", socket_path.display());
    loop {
        let is_tick = tokio::select! {
            _ = ticker.tick() => {
                ticks += 1;
                let now = now_ms();
                let effects = daemon.engine.handle(Input::Tick, now);
                daemon.apply(effects, now);
                if ticks.is_multiple_of(PROCESS_SCAN_EVERY_TICKS) && daemon.engine.has_process_rules() {
                    procscan::enforce(&daemon.engine, now);
                }
                true
            }
            Some(event) = rx.recv() => {
                let now = now_ms();
                match event {
                    Event::Input(input) => {
                        let effects = daemon.engine.handle(input, now);
                        daemon.apply(effects, now);
                    }
                    Event::Request(request, reply) if request.is_sync() => match &sync {
                        Some(sync) => {
                            if let Err(std::sync::mpsc::SendError((_, reply))) = sync.send((request, reply)) {
                                let _ = reply.send(Response::err("the sync thread stopped; see `journalctl --user -u habitd`"));
                            }
                        }
                        None => {
                            let _ = reply.send(Response::err("sync is off: history.db can't be opened"));
                        }
                    },
                    Event::Request(request, reply) => {
                        let _ = reply.send(daemon.handle_request(request, now));
                    }
                    Event::UpdateChecked(result) => {
                        if let Some(update) = update_checker.finished(result) {
                            daemon.engine.set_available_update(update);
                        }
                    }
                }
                false
            }
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        };

        let now = now_ms();
        let locked = daemon.engine.is_locked(now);
        if is_tick && locked && ticks.is_multiple_of(HEARTBEAT_EVERY_TICKS) {
            daemon.write_heartbeat(now, false);
            daemon.engine.take_dirty();
        } else {
            let dirty = daemon.engine.take_dirty();
            let periodic =
                is_tick && ticks.is_multiple_of(SAVE_EVERY_TICKS) && daemon.engine.state().session.is_some();
            let usage = is_tick && ticks.is_multiple_of(USAGE_SAVE_EVERY_TICKS) && daemon.engine.take_usage_dirty();
            if dirty || periodic || usage {
                daemon.engine.set_heartbeat(heartbeat::now(now, false));
                save_state(&daemon.state_path, daemon.engine.state());
            }
        }

        daemon.write_archive();
        terminal_watch.poll(&daemon.engine, is_tick && ticks.is_multiple_of(TERMINAL_CHECK_EVERY_TICKS), &tx);
        update_checker.poll(daemon.engine.config().general.update_check, &tx);
        idle_watch.poll(daemon.engine.config().general.idle_timeout, &tx);

        let snapshot = daemon.engine.snapshot(now);
        let time_update = is_tick && daemon.engine.is_time_sensitive(now);
        snapshot_tx.send_if_modified(|current| {
            if time_update || !same_ignoring_time(current, &snapshot) {
                *current = snapshot;
                true
            } else {
                false
            }
        });
    }

    let clean = heartbeat::session_is_ending();
    daemon.engine.flush_usage(now_ms());
    daemon.write_archive();
    daemon.write_heartbeat(now_ms(), clean);
    let _ = std::fs::remove_file(&socket_path);
    eprintln!("habitd: stopped{}", if clean { " (session ending)" } else { "" });
    Ok(())
}
