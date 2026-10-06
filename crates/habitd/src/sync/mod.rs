//! Multi-device sync through a habitfocus sync server.
//!
//! A thread of its own does the sync, with its own connection to
//! `history.db`, so a slow or unreachable server never holds up blocking. It
//! syncs half a minute after start and then every ten minutes, checks every
//! minute that the server answers, and answers the `sync_*` requests, which
//! the event loop hands over. How it goes reaches snapshots as
//! `Event::SyncChanged`.
//!
//! The account (server, user, session token and the sync key) is kept in
//! `sync.json` next to state.json, readable only by the user.

mod api;
mod crypto;
mod run;

use crate::archive::Archive;
use api::{Api, ApiError, DeviceInfo, Http};
use crypto::Key;
use habit_core::snapshot::{SyncDeviceView, SyncView};
use habit_core::state::State;
use habit_ipc::{Request, Response};
use run::{Summary, SyncError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

const FIRST_SYNC: Duration = Duration::from_secs(30);
const EVERY: Duration = Duration::from_secs(10 * 60);
/// How often the server is asked whether it's there, between syncs.
const CHECK_EVERY: Duration = Duration::from_secs(60);

/// A request for the sync thread and where to send its answer.
pub type Job = (Request, oneshot::Sender<Response>);

/// `sync.json`.
#[derive(Clone, Serialize, Deserialize)]
struct Account {
    server: String,
    username: String,
    token: String,
    key: String,
}

impl Account {
    fn load(path: &Path) -> Option<Account> {
        let text = std::fs::read_to_string(path).ok()?;
        match serde_json::from_str(&text) {
            Ok(account) => Some(account),
            Err(e) => {
                eprintln!("habitd: ignoring {}: {e}", path.display());
                None
            }
        }
    }

    /// Writes the file readable only by the user, atomically.
    fn save(&self, path: &Path) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = path.with_extension("json.tmp");
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        file.write_all(serde_json::to_string_pretty(self).expect("serializes").as_bytes())?;
        file.sync_all()?;
        std::fs::rename(tmp, path)
    }

    fn http(&self) -> Result<Http, String> {
        Http::new(&self.server, Some(self.token.clone()))
    }

    fn key(&self) -> Result<Key, String> {
        Key::from_text(&self.key).map_err(|e| format!("sync.json: {e}"))
    }
}

/// The last sync, for `hf sync status`.
struct LastSync {
    at_ms: i64,
    result: Result<Summary, String>,
}

struct Syncer {
    archive: Archive,
    account_path: PathBuf,
    account: Option<Account>,
    last: Option<LastSync>,
    /// When the last sync succeeded.
    last_ok_ms: Option<i64>,
    next: Option<Instant>,
    next_check: Option<Instant>,
    /// The server answered the last request; `None` before the first.
    reachable: Option<bool>,
    /// The server ended this device's session.
    signed_out: bool,
    events: tokio::sync::mpsc::Sender<crate::Event>,
}

/// Starts the sync thread. It opens `history.db` itself; the event loop sends
/// it the `sync_*` requests.
pub fn spawn(archive_path: PathBuf, account_path: PathBuf, events: tokio::sync::mpsc::Sender<crate::Event>) -> mpsc::Sender<Job> {
    let (tx, rx) = mpsc::channel::<Job>();
    std::thread::spawn(move || {
        let archive = match Archive::open(&archive_path, &State::default()) {
            Ok(archive) => archive,
            Err(e) => {
                eprintln!("habitd: sync disabled, can't open {}: {e:#}", archive_path.display());
                for (_, reply) in rx {
                    let _ = reply.send(Response::err(format!("sync is off: history.db can't be opened ({e:#})")));
                }
                return;
            }
        };
        let account = Account::load(&account_path);
        let next = account.as_ref().map(|_| Instant::now() + FIRST_SYNC);
        let mut syncer = Syncer {
            archive,
            account_path,
            account,
            last: None,
            last_ok_ms: None,
            next,
            next_check: next.map(|_| Instant::now()),
            reachable: None,
            signed_out: false,
            events,
        };
        syncer.publish();
        loop {
            let due = [syncer.next, syncer.next_check].into_iter().flatten().min();
            let wait = due.map_or(Duration::from_secs(3600), |at| at.saturating_duration_since(Instant::now()));
            match rx.recv_timeout(wait) {
                Ok((request, reply)) => {
                    let response = syncer.handle(request);
                    let _ = reply.send(response);
                }
                Err(RecvTimeoutError::Timeout) => {
                    let now = Instant::now();
                    if syncer.next.is_some_and(|at| at <= now) {
                        syncer.sync();
                    } else if syncer.next_check.is_some_and(|at| at <= now) {
                        syncer.check();
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
    tx
}

fn ok(message: String) -> Response {
    Response { ok: true, message: Some(message), ..Default::default() }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn local_time(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.with_timezone(&chrono::Local).format("%a %H:%M").to_string())
        .unwrap_or_default()
}

impl Syncer {
    fn handle(&mut self, request: Request) -> Response {
        let result = match request {
            Request::SyncRegister { server, username, email, password } => self.register(&server, &username, &email, &password),
            Request::SyncLogin { server, username, password, key } => self.login(&server, &username, &password, &key),
            Request::SyncLogout => self.logout(),
            Request::SyncStatus => Ok(self.status()),
            Request::SyncNow => self.sync_now(),
            Request::SyncKey => match &self.account {
                Some(account) => Ok(account.key.clone()),
                None => Err(not_signed_in()),
            },
            _ => Err("not a sync request".into()),
        };
        result.map_or_else(Response::err, ok)
    }

    fn device(&self) -> DeviceInfo {
        let device = self.archive.device();
        DeviceInfo { id: device.id.clone(), name: device.name.clone() }
    }

    fn signed_in_already(&self) -> Result<(), String> {
        match &self.account {
            Some(a) => Err(format!("already signed in as {} on {}; `hf sync logout` first", a.username, a.server)),
            None => Ok(()),
        }
    }

    /// Keeps the account and syncs right after answering.
    fn sign_in(&mut self, account: Account) -> Result<(), String> {
        account.save(&self.account_path).map_err(|e| format!("can't write {}: {e}", self.account_path.display()))?;
        self.account = Some(account);
        self.last = None;
        self.last_ok_ms = None;
        self.reachable = Some(true);
        self.signed_out = false;
        self.next = Some(Instant::now());
        self.next_check = Some(Instant::now() + CHECK_EVERY);
        self.publish();
        Ok(())
    }

    fn register(&mut self, server: &str, username: &str, email: &str, password: &str) -> Result<String, String> {
        self.signed_in_already()?;
        let http = Http::new(server, None)?;
        let key = Key::generate();
        let device = self.device();
        let token = http.register(username, email, password, &device)?;
        let server = server.trim().trim_end_matches('/').to_string();
        self.sign_in(Account { server: server.clone(), username: username.into(), token, key: key.to_text() })?;
        Ok(format!(
            "Registered {username} on {server} and signed in this device ({}).\n\n\
             Your sync key:\n\n    {}\n\n\
             Keep it somewhere safe: every other device needs it to sign in (`hf sync login`), and the data on \
             the server can't be read without it. `hf sync key` shows it again.\n\
             The first sync starts now; `hf sync status` shows how it went.",
            device.name,
            key.to_text()
        ))
    }

    fn login(&mut self, server: &str, username: &str, password: &str, key: &str) -> Result<String, String> {
        self.signed_in_already()?;
        let key = Key::from_text(key)?;
        let device = self.device();
        let token = Http::new(server, None)?.login(username, password, &device)?;
        let http = Http::new(server, Some(token.clone()))?;
        if let Err(e) = run::check_key(&http, &key) {
            // Don't leave a session behind that can't be used.
            let _ = http.delete_device(&device.id);
            return Err(match e {
                SyncError::Crypto(_) => "this sync key doesn't belong to the account: its data doesn't open with it".into(),
                e => e.to_string(),
            });
        }
        let server = server.trim().trim_end_matches('/').to_string();
        self.sign_in(Account { server: server.clone(), username: username.into(), token, key: key.to_text() })?;
        Ok(format!(
            "Signed in to {server} as {username} with this device ({}). The first sync starts now; \
             `hf sync status` shows how it went.",
            device.name
        ))
    }

    fn logout(&mut self) -> Result<String, String> {
        let account = self.account.take().ok_or_else(not_signed_in)?;
        self.next = None;
        self.next_check = None;
        self.last = None;
        self.last_ok_ms = None;
        self.publish();
        // Best effort: the session may be gone already, or the server away.
        let signed_out = account.http().map(|http| http.delete_device(&self.archive.device().id));
        if let Err(e) = std::fs::remove_file(&self.account_path) {
            eprintln!("habitd: can't remove {}: {e}", self.account_path.display());
        }
        self.archive.clear_sync_state().map_err(|e| format!("{e:#}"))?;
        let note = match signed_out {
            Ok(Ok(())) | Ok(Err(ApiError::SignedOut)) => String::new(),
            Ok(Err(e)) => format!("\nThe server wasn't told ({e}); remove the device there later."),
            Err(e) => format!("\n{e}"),
        };
        Ok(format!(
            "Signed out of {}. What this device got from the others stays in history.db.{note}",
            account.server
        ))
    }

    fn sync_now(&mut self) -> Result<String, String> {
        if self.account.is_none() {
            return Err(not_signed_in());
        }
        self.sync();
        match &self.last {
            Some(LastSync { result: Ok(s), .. }) => Ok(format!("Synced: sent {}, received {}.", rows(s.sent), rows(s.received))),
            Some(LastSync { result: Err(e), .. }) => Err(format!("sync failed: {e}")),
            None => Err("sync didn't run".into()),
        }
    }

    fn sync(&mut self) {
        if self.account.is_none() {
            self.next = None;
            return;
        }
        self.publish_syncing();
        let account = self.account.as_ref().expect("checked");
        let result = account
            .http()
            .and_then(|http| Ok((http, account.key()?)))
            .map_err(SyncError::Archive)
            .and_then(|(http, key)| run::sync(&http as &dyn Api, &mut self.archive, &key));
        let failed_before = self.last.as_ref().is_some_and(|l| l.result.is_err());
        self.next = Some(Instant::now() + EVERY);
        self.next_check = Some(Instant::now() + CHECK_EVERY);
        self.reachable = Some(!matches!(result, Err(SyncError::Api(ApiError::Unreachable(_)))));
        if result.is_ok() {
            self.last_ok_ms = Some(now_ms());
        }
        match &result {
            Ok(s) if s.sent + s.received > 0 => eprintln!("habitd: synced: sent {} rows, received {}", s.sent, s.received),
            Ok(_) if failed_before => eprintln!("habitd: sync works again"),
            Ok(_) => {}
            Err(SyncError::Api(ApiError::SignedOut)) => {
                eprintln!("habitd: sync stopped: this device was signed out");
                self.next = None;
                self.next_check = None;
                self.signed_out = true;
            }
            Err(e) if !failed_before => eprintln!("habitd: sync failed: {e}"),
            Err(_) => {}
        }
        self.last = Some(LastSync { at_ms: now_ms(), result: result.map_err(|e| e.to_string()) });
        self.publish();
    }

    /// Asks the server whether it's there, between syncs.
    fn check(&mut self) {
        self.next_check = Some(Instant::now() + CHECK_EVERY);
        let Some(account) = &self.account else { return };
        let reachable = Http::new(&account.server, None).is_ok_and(|http| http.health().is_ok());
        if self.reachable != Some(reachable) {
            self.reachable = Some(reachable);
            self.publish();
        }
    }

    /// The account and how syncing goes, as snapshots show it.
    fn view(&self, syncing: bool) -> Option<SyncView> {
        let account = self.account.as_ref()?;
        let devices = self.archive.remote_devices().unwrap_or_default();
        Some(SyncView {
            server: account.server.clone(),
            username: account.username.clone(),
            device: self.archive.device().name.clone(),
            reachable: self.reachable,
            syncing,
            last_sync_ms: self.last_ok_ms.map(|ms| ms as u64),
            error: self.last.as_ref().and_then(|l| l.result.as_ref().err().cloned()),
            signed_out: self.signed_out,
            devices: devices
                .into_iter()
                .map(|d| SyncDeviceView { id: d.id, name: d.name, last_seen_ms: d.last_seen_ms, rows: d.rows })
                .collect(),
        })
    }

    fn publish(&self) {
        let _ = self.events.blocking_send(crate::Event::SyncChanged(self.view(false)));
    }

    fn publish_syncing(&self) {
        let _ = self.events.blocking_send(crate::Event::SyncChanged(self.view(true)));
    }

    fn status(&self) -> String {
        let Some(account) = &self.account else {
            return format!("{}.", not_signed_in());
        };
        let device = self.archive.device();
        let mut text = format!(
            "Signed in to {} as {}; this device is {} ({}).\n",
            account.server,
            account.username,
            device.name,
            &device.id[..8]
        );
        text += &match &self.last {
            None => "Not synced since habitd started.\n".to_string(),
            Some(LastSync { at_ms, result: Ok(s) }) => {
                format!("Last sync {}: sent {}, received {}.\n", local_time(*at_ms), rows(s.sent), rows(s.received))
            }
            Some(LastSync { at_ms, result: Err(e) }) => format!("Last sync {} failed: {e}\n", local_time(*at_ms)),
        };
        if self.next.is_none() {
            text += "Syncing is stopped; sign in again.\n";
        }
        match self.archive.remote_devices() {
            Ok(devices) if devices.is_empty() => text += "No other devices yet.\n",
            Ok(devices) => {
                text += "Other devices:\n";
                for d in devices {
                    let seen = d.last_seen_ms.map(|ms| format!("seen {}", local_time(ms as i64))).unwrap_or_default();
                    let rows = if d.rows == 1 { "1 row".to_string() } else { format!("{} rows", d.rows) };
                    text += &format!("  {:<20} {:<16} {rows} here\n", d.name, seen);
                }
            }
            Err(e) => text += &format!("Devices unknown: {e:#}\n"),
        }
        text.trim_end().to_string()
    }
}

fn rows(n: usize) -> String {
    if n == 1 { "1 row".into() } else { format!("{n} rows") }
}

fn not_signed_in() -> String {
    "not signed in to a sync server; `hf sync register <server> <user>` creates an account, \
     `hf sync login <server> <user>` joins one"
        .into()
}
