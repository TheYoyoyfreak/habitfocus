//! Webhooks (`webhook` of a timed habit): each day's minutes of the habit's
//! finished sessions go to its link as `{"value": <minutes>, "day":
//! "YYYY-MM-DD"}`, e.g. to a habitsfirst webhook. The minutes are those of
//! every device (the others' sessions arrive by sync), and the value replaces
//! the day's number there, so sending a day again (a retry, the phone app
//! sending it too) never counts twice.
//!
//! A thread of its own with its own connection to `history.db`, so a slow link
//! never holds up blocking. It sends after every finished session here, when
//! sessions of other devices arrive, and every ten minutes (what failed goes
//! again). What arrived is kept as `webhook:<habit>` in `sync_state`: the
//! link's hash and the minutes sent per day of the last week.

use crate::archive::Archive;
use chrono::{Local, NaiveDate};
use habit_core::state::{HistoryEntry, Outcome, State};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

const EVERY: Duration = Duration::from_secs(10 * 60);
/// Days looked after, today included: a session that arrives later still
/// reaches its day.
const DAYS: i64 = 7;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const TIMEOUT_SECS: &str = "20";

/// A habit's link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hook {
    pub habit: String,
    pub url: String,
}

/// The webhooks of the config in force; sending one also asks for a run.
#[derive(Debug, Clone, Default)]
pub struct Job {
    pub hooks: Vec<Hook>,
    /// `general.day_start`.
    pub day_start: u64,
}

/// `webhook:<habit>` in `sync_state`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Sent {
    /// Hash of the link: another link starts over.
    link: String,
    /// Minutes that arrived, by day.
    days: BTreeMap<String, u64>,
}

/// Starts the webhook thread; it opens `history.db` itself.
pub fn spawn(archive_path: PathBuf) -> mpsc::Sender<Job> {
    let (tx, rx) = mpsc::channel::<Job>();
    std::thread::spawn(move || {
        let mut archive = match Archive::open(&archive_path, &State::default()) {
            Ok(archive) => archive,
            Err(e) => {
                eprintln!("habitd: webhooks disabled, can't open {}: {e:#}", archive_path.display());
                return;
            }
        };
        let mut job = Job::default();
        let mut failing = HashSet::new();
        loop {
            match rx.recv_timeout(EVERY) {
                Ok(next) => job = next,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            for hook in &job.hooks {
                match send(&mut archive, hook, job.day_start) {
                    Ok(()) if failing.remove(&hook.habit) => eprintln!("habitd: webhook of {} works again", hook.habit),
                    Ok(()) => {}
                    Err(e) if failing.insert(hook.habit.clone()) => eprintln!("habitd: webhook of {} failed: {e}", hook.habit),
                    Err(_) => {}
                }
            }
        }
    });
    tx
}

/// Logical day of `ms` in local time (days begin at `day_start`).
fn day_of(ms: u64, day_start: u64) -> NaiveDate {
    let shifted = ms.saturating_sub(day_start) as i64;
    chrono::DateTime::from_timestamp_millis(shifted).unwrap_or_default().with_timezone(&Local).date_naive()
}

/// Minutes of `habit` per day from `first` on: finished sessions with time
/// that counted (aborted ones earn nothing, so they don't count here either).
fn minutes_per_day(
    sessions: &[HistoryEntry],
    habit: &str,
    first: NaiveDate,
    day_of: impl Fn(u64) -> NaiveDate,
) -> BTreeMap<String, u64> {
    let mut ms: BTreeMap<NaiveDate, u64> = BTreeMap::new();
    for s in sessions {
        if s.habit == habit && s.focused_ms > 0 && matches!(s.outcome, Outcome::Completed | Outcome::Stopped) {
            let day = day_of(s.finished_at);
            if day >= first {
                *ms.entry(day).or_default() += s.focused_ms;
            }
        }
    }
    ms.into_iter().map(|(day, ms)| (day.format("%Y-%m-%d").to_string(), (ms + 30_000) / 60_000)).collect()
}

/// Days whose minutes haven't arrived yet, as `(day, minutes)`.
fn due(minutes: &BTreeMap<String, u64>, sent: &Sent) -> Vec<(String, u64)> {
    minutes
        .iter()
        .filter(|(day, &m)| m > 0 && sent.days.get(*day) != Some(&m))
        .map(|(day, &m)| (day.clone(), m))
        .collect()
}

fn link_hash(url: &str) -> String {
    Sha256::digest(url.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Sends what's due for `hook`, oldest day first; stops at the first failure.
fn send(archive: &mut Archive, hook: &Hook, day_start: u64) -> Result<(), String> {
    let now = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or_default();
    let first = day_of(now, day_start) - chrono::Days::new(DAYS as u64 - 1);
    let sessions = archive.sessions_since(now.saturating_sub((DAYS as u64 + 1) * DAY_MS)).map_err(|e| format!("{e:#}"))?;
    let minutes = minutes_per_day(&sessions, &hook.habit, first, |ms| day_of(ms, day_start));

    let key = format!("webhook:{}", hook.habit);
    let mut sent: Sent = archive
        .sync_value(&key)
        .map_err(|e| format!("{e:#}"))?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default();
    let link = link_hash(&hook.url);
    if sent.link != link {
        sent = Sent { link, days: BTreeMap::new() };
    }
    let first = first.format("%Y-%m-%d").to_string();
    sent.days.retain(|day, _| *day >= first);
    for (day, m) in due(&minutes, &sent) {
        post(&hook.url, &serde_json::json!({ "value": m, "day": day }).to_string())?;
        sent.days.insert(day, m);
        archive.set_sync_value(&key, &serde_json::to_string(&sent).expect("serializes")).map_err(|e| format!("{e:#}"))?;
    }
    Ok(())
}

/// A quoted value in a curl config file.
fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\r', "\\r"))
}

/// POSTs `body` as JSON. The link goes to curl on stdin: it's a secret of its own.
fn post(url: &str, body: &str) -> Result<(), String> {
    let config = format!(
        "url = {}\nrequest = \"POST\"\nmax-time = {TIMEOUT_SECS}\nheader = \"Content-Type: application/json\"\n\
         data-binary = {}\nwrite-out = \"%{{http_code}}\"\noutput = \"/dev/null\"\nsilent\nshow-error\n",
        quote(url),
        quote(body)
    );
    let mut child = Command::new("curl")
        .args(["--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("can't run curl: {e}"))?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let written = stdin.write_all(config.as_bytes());
    drop(stdin);
    let output = child.wait_with_output().map_err(|e| format!("curl failed: {e}"))?;
    written.map_err(|e| format!("curl failed: {e}"))?;
    if !output.status.success() {
        return Err(format!("can't reach it: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    match String::from_utf8_lossy(&output.stdout).trim().parse::<u16>() {
        Ok(code) if (200..300).contains(&code) => Ok(()),
        Ok(code) => Err(format!("it answered with status {code}")),
        Err(_) => Err("unexpected answer from curl".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(habit: &str, finished_at: u64, minutes: u64, outcome: Outcome) -> HistoryEntry {
        HistoryEntry { habit: habit.into(), started_at: 0, finished_at, focused_ms: minutes * 60_000, outcome }
    }

    /// Day 0 is 2026-10-12; days are plain UTC days here.
    fn day(ms: u64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 12).unwrap() + chrono::Days::new(ms / DAY_MS)
    }

    #[test]
    fn minutes_add_up_per_day_over_devices_and_only_what_counted() {
        let sessions = [
            session("read", 1_000, 20, Outcome::Completed),
            session("read", 2_000, 10, Outcome::Stopped),
            session("read", 3_000, 30, Outcome::Aborted),
            session("code", 4_000, 60, Outcome::Completed),
            session("read", DAY_MS + 1, 5, Outcome::Completed),
        ];
        let minutes = minutes_per_day(&sessions, "read", day(0), day);
        assert_eq!(minutes, BTreeMap::from([("2026-10-12".into(), 30), ("2026-10-13".into(), 5)]));
        assert!(minutes_per_day(&sessions, "read", day(DAY_MS), day).len() == 1, "days before `first` are left out");
    }

    #[test]
    fn half_minutes_round() {
        let mut s = session("read", 0, 0, Outcome::Completed);
        s.focused_ms = 89_000;
        assert_eq!(minutes_per_day(&[s.clone()], "read", day(0), day)["2026-10-12"], 1);
        s.focused_ms = 90_000;
        assert_eq!(minutes_per_day(&[s], "read", day(0), day)["2026-10-12"], 2);
    }

    /// Answers one request with `status` and hands over what came.
    fn one_request(status: u16) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{BufRead, BufReader, Read};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/hook/abc", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                head += &line;
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            write!(&stream, "HTTP/1.1 {status} X\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").unwrap();
            head + &String::from_utf8(body).unwrap()
        });
        (url, handle)
    }

    #[test]
    fn posts_json_and_checks_the_status() {
        let (url, server) = one_request(200);
        post(&url, r#"{"value":30,"day":"2026-10-18"}"#).unwrap();
        let request = server.join().unwrap();
        assert!(request.starts_with("POST /hook/abc "), "{request}");
        assert!(request.contains("Content-Type: application/json"), "{request}");
        assert!(request.ends_with(r#"{"value":30,"day":"2026-10-18"}"#), "{request}");

        let (url, server) = one_request(404);
        assert_eq!(post(&url, "{}").unwrap_err(), "it answered with status 404");
        server.join().unwrap();
    }

    #[test]
    fn only_changed_days_are_due() {
        let minutes = BTreeMap::from([("2026-10-12".into(), 30), ("2026-10-13".into(), 5), ("2026-10-14".into(), 0)]);
        let mut sent = Sent::default();
        assert_eq!(due(&minutes, &sent), [("2026-10-12".into(), 30), ("2026-10-13".into(), 5)]);
        sent.days.insert("2026-10-12".into(), 30);
        sent.days.insert("2026-10-13".into(), 4);
        assert_eq!(due(&minutes, &sent), [("2026-10-13".into(), 5)], "a later session changes its day");
    }
}
