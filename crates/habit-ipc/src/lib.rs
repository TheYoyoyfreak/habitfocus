//! Wire protocol between `habitd` and its clients: JSON lines over a unix socket.
//!
//! Every request gets one [`Response`] line, except `subscribe`, which is
//! answered with a stream of [`habit_core::Snapshot`] lines.

pub mod update;

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Subscribe,
    /// Start or resume a habit (stops another running habit first).
    Start { habit: String },
    /// End the session, keeping its progress for later today.
    Stop,
    /// Complete a session whose goal was reached, banking the reward.
    Finish,
    /// Raise the goal of a session that reached it by another target.
    Continue,
    /// End the session and discard its progress.
    Abort { emergency: bool },
    /// Log a manual or counter habit: add `amount` to today's count, or set
    /// the count to it with `set`.
    Done {
        habit: String,
        amount: i64,
        #[serde(default)]
        set: bool,
    },
    Unlock { group: String, duration_ms: Option<u64> },
    Relock { group: String },
    Reload,
    /// Most recent sessions, newest first.
    History { limit: usize },
    /// Daily totals per habit for the last `days` days, oldest first.
    Stats { days: u32 },
    /// Recent activity log entries, newest first.
    Events { limit: usize },
    /// Screen time per app over the last `days` days, most used first.
    AppStats { days: u32 },
    /// The hours of one logical day, `day_offset` days back (0 = today),
    /// by habit, category and app (`Response.breakdown`).
    HourStats { day_offset: u32 },
    /// Terminal focus of a timer client (`hf tui`); `focused: None` disconnects.
    TimerFocus { source: String, focused: Option<bool> },
    /// Change a `[general]` setting in the config file and reload.
    SetSetting { key: String, value: String },
    /// The blocks and habits of the config file, as written (`Response.config`).
    ConfigEntries,
    /// Give an app id or `site:<host>` key a display name (`[app_names]`),
    /// or remove it with `name: None`.
    SetAppName { app: String, name: Option<String> },
    /// Put an app id or `site:<host>` key in a category (`[app_categories]`),
    /// or take it out with `category: None`.
    SetAppCategory { app: String, category: Option<String> },
    /// Replace the keys of `[<section>.<id>]` (section `groups` or `habits`)
    /// with `table`, or delete the entry when `table` is null. `create`
    /// refuses an id that already exists.
    EditConfig {
        section: String,
        id: String,
        #[serde(default)]
        table: Option<serde_json::Value>,
        #[serde(default)]
        create: bool,
    },
    /// Start a commitment lock until `until_ms`, or extend the current one.
    Lock { until_ms: u64 },
    /// End the commitment early, after a 24 hour cooldown.
    LockEnd,
    /// Cancel a requested early end.
    LockCancelEnd,
    /// The browser extension's native host runs inside browser process `pid`.
    BrowserHello { source: String, pid: u32 },
    /// Active tab of a browser window (browser extension via `hf native-host`).
    /// Omitting `title`/`url` forgets the window; omitting `window` forgets the source.
    BrowserTab {
        source: String,
        window: Option<u64>,
        title: Option<String>,
        url: Option<String>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<habit_core::Snapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<habit_core::state::HistoryEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<Vec<habit_core::snapshot::DayView>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<habit_core::state::Event>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_stats: Option<Vec<habit_core::snapshot::AppUsageView>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ConfigEntries>,
    /// Hourly breakdown by habit, category or app (with `app_stats`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breakdown: Option<habit_core::snapshot::Breakdown>,
}

/// Blocks and habits of the config file by id, each table as written.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigEntries {
    pub groups: std::collections::BTreeMap<String, serde_json::Value>,
    pub habits: std::collections::BTreeMap<String, serde_json::Value>,
}

impl Response {
    pub fn ok(message: Option<String>, snapshot: habit_core::Snapshot) -> Self {
        Self { ok: true, message, snapshot: Some(snapshot), ..Default::default() }
    }

    pub fn err(error: impl Into<String>) -> Self {
        Self { ok: false, error: Some(error.into()), ..Default::default() }
    }
}

fn env_dir(var: &str, home_fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_else(|| "/".into());
            PathBuf::from(home).join(home_fallback)
        })
}

/// The GitHub repository releases come from.
pub const REPO: &str = "TheYoyoyfreak/habitfocus";

/// `REPO`, or `HABITFOCUS_REPO` (a fork, or testing the update check).
pub fn repo() -> String {
    std::env::var("HABITFOCUS_REPO").ok().filter(|r| !r.is_empty()).unwrap_or_else(|| REPO.to_string())
}

/// The systemd user unit `hf setup` writes.
pub fn unit_path() -> PathBuf {
    env_dir("XDG_CONFIG_HOME", ".config").join("systemd/user/habitd.service")
}

/// Where the installer keeps its marker file.
pub fn data_dir() -> PathBuf {
    env_dir("XDG_DATA_HOME", ".local/share").join("habitfocus")
}

pub fn config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("HABITFOCUS_CONFIG") {
        return path.into();
    }
    env_dir("XDG_CONFIG_HOME", ".config").join("habitfocus/config.toml")
}

pub fn state_path() -> PathBuf {
    env_dir("XDG_STATE_HOME", ".local/state").join("habitfocus/state.json")
}

/// The archive (`history.db`) lives next to the state file.
pub fn archive_path_for(state_path: &std::path::Path) -> PathBuf {
    state_path.with_file_name("history.db")
}

pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("HABITFOCUS_SOCKET") {
        return path.into();
    }
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("habitfocus.sock"),
        None => PathBuf::from("/tmp/habitfocus.sock"),
    }
}

fn connect() -> Result<UnixStream, String> {
    let path = socket_path();
    UnixStream::connect(&path).map_err(|e| {
        format!(
            "cannot reach habitd at {}: {e}\nIs it running? Try `systemctl --user start habitd`.",
            path.display()
        )
    })
}

fn send(stream: &mut UnixStream, request: &Request) -> Result<(), String> {
    let mut line = serde_json::to_string(request).map_err(|e| e.to_string())?;
    line.push('\n');
    stream.write_all(line.as_bytes()).map_err(|e| e.to_string())
}

/// Sends one request and waits for its response.
pub fn request(request: &Request) -> Result<Response, String> {
    let mut stream = connect()?;
    send(&mut stream, request)?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    if line.is_empty() {
        return Err("habitd closed the connection".into());
    }
    serde_json::from_str(&line).map_err(|e| {
        format!(
            "invalid response from habitd: {e}\n\
             hf and the running habitd are probably different versions; \
             restart it with `systemctl --user restart habitd`."
        )
    })
}

/// Subscribes to snapshot updates; yields raw JSON lines.
pub fn subscribe() -> Result<impl Iterator<Item = std::io::Result<String>>, String> {
    Ok(BufReader::new(subscribe_stream()?).lines())
}

/// Subscribes and returns the raw connection, for callers that need to set
/// timeouts. habitd writes one snapshot JSON object per line.
pub fn subscribe_stream() -> Result<UnixStream, String> {
    let mut stream = connect()?;
    send(&mut stream, &Request::Subscribe)?;
    Ok(stream)
}
