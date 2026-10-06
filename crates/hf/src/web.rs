//! `hf web`: the insights page in a browser. A small HTTP server on
//! 127.0.0.1 that serves one compiled-in page and passes read-only requests
//! through to habitd. Nothing on the page can change state: `api_request`
//! only knows the requests that read.

use anyhow::Context;
use habit_ipc::Request;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 5601;

const PAGE: &str = include_str!("web/index.html");
/// Longest request head read before giving up.
const MAX_HEAD: usize = 8 * 1024;

pub fn run(port: u16, open: bool) -> anyhow::Result<()> {
    let listener =
        TcpListener::bind(("127.0.0.1", port)).with_context(|| format!("can't listen on 127.0.0.1:{port}"))?;
    let url = format!("http://127.0.0.1:{port}");
    println!("habitfocus insights on {url} (ctrl-c stops)");
    if open {
        if let Err(e) = std::process::Command::new("xdg-open").arg(&url).spawn() {
            eprintln!("hf: couldn't open a browser: {e}");
        }
    }
    for stream in listener.incoming().flatten() {
        std::thread::spawn(move || {
            let _ = handle(stream, port);
        });
    }
    Ok(())
}

fn handle(mut stream: TcpStream, port: u16) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let (status, content_type, body) = match read_head(&stream) {
        Some(head) => respond(&head, port),
        None => (400, "text/plain", "bad request".to_string()),
    };
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Service Unavailable",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

/// The request line and the Host header.
#[derive(Debug, PartialEq, Eq)]
struct Head {
    method: String,
    target: String,
    host: Option<String>,
}

fn read_head(stream: &TcpStream) -> Option<Head> {
    let mut reader = BufReader::new(stream).take(MAX_HEAD as u64);
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end().to_string();
        if line.is_empty() {
            break;
        }
        lines.push(line);
    }
    parse_head(&lines)
}

fn parse_head(lines: &[String]) -> Option<Head> {
    let mut parts = lines.first()?.split(' ');
    let (method, target) = (parts.next()?.to_string(), parts.next()?.to_string());
    let host = lines[1..].iter().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("host").then(|| value.trim().to_string())
    });
    Some(Head { method, target, host })
}

/// Only the page's own address: another site can't reach the API through a
/// name that resolves to 127.0.0.1 (DNS rebinding).
fn host_allowed(host: Option<&str>, port: u16) -> bool {
    host.is_some_and(|h| h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}"))
}

fn respond(head: &Head, port: u16) -> (u16, &'static str, String) {
    if !host_allowed(head.host.as_deref(), port) {
        return (403, "text/plain", "forbidden".into());
    }
    if head.method != "GET" {
        return (405, "text/plain", "only GET".into());
    }
    let (path, query) = head.target.split_once('?').unwrap_or((&head.target, ""));
    if path == "/" {
        return (200, "text/html", PAGE.into());
    }
    let Some(request) = path.strip_prefix("/api/").and_then(|cmd| api_request(cmd, query)) else {
        return (404, "text/plain", "not found".into());
    };
    match habit_ipc::request(&request) {
        Ok(response) => (200, "application/json", serde_json::to_string(&response).expect("serializes")),
        Err(e) => (503, "application/json", serde_json::json!({ "ok": false, "error": e }).to_string()),
    }
}

/// The read-only requests the page may make, from `/api/<cmd>?<query>`.
fn api_request(cmd: &str, query: &str) -> Option<Request> {
    let param = |name: &str, default: u32, max: u32| -> u32 {
        query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(key, _)| *key == name)
            .and_then(|(_, value)| value.parse().ok())
            .unwrap_or(default)
            .min(max)
    };
    // A device id or `all`; ids are UUIDs, so anything else is ignored.
    let device = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "device")
        .map(|(_, value)| value.to_string())
        .filter(|d| !d.is_empty() && d.len() <= 64 && d.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    Some(match cmd {
        "status" => Request::Status,
        "stats" => Request::Stats { days: param("days", 365, 3660) },
        "app_stats" => Request::AppStats { days: param("days", 7, 3660), device },
        "hour_stats" => Request::HourStats { day_offset: param("day_offset", 0, 3660), device },
        "timeline" => Request::Timeline { day_offset: param("day_offset", 0, 3660), device },
        "events" => Request::Events { limit: param("limit", 1000, 10_000) as usize },
        "history" => Request::History { limit: param("limit", 500, 10_000) as usize },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(lines: &[&str]) -> Option<Head> {
        parse_head(&lines.iter().map(|l| l.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parses_the_request_line_and_host() {
        assert_eq!(
            head(&["GET /api/stats?days=30 HTTP/1.1", "User-Agent: x", "host: 127.0.0.1:5601"]),
            Some(Head { method: "GET".into(), target: "/api/stats?days=30".into(), host: Some("127.0.0.1:5601".into()) })
        );
        assert_eq!(head(&["GET"]), None);
        assert_eq!(head(&[]), None);
    }

    #[test]
    fn only_reading_requests_reach_habitd() {
        assert_eq!(api_request("stats", "days=30"), Some(Request::Stats { days: 30 }));
        assert_eq!(api_request("timeline", "x=1&day_offset=2"), Some(Request::Timeline { day_offset: 2, device: None }));
        assert_eq!(
            api_request("app_stats", "device=all"),
            Some(Request::AppStats { days: 7, device: Some("all".into()) })
        );
        assert_eq!(api_request("app_stats", "device=%27x"), Some(Request::AppStats { days: 7, device: None }));
        assert_eq!(api_request("events", "limit=abc"), Some(Request::Events { limit: 1000 }));
        assert_eq!(api_request("app_stats", "days=99999"), Some(Request::AppStats { days: 3660, device: None }));
        for cmd in ["start", "unlock", "set_setting", "edit_config", "lock", "subscribe", ""] {
            assert_eq!(api_request(cmd, "habit=read"), None, "{cmd}");
        }
    }

    #[test]
    fn foreign_hosts_and_methods_are_refused() {
        let get = |target: &str, host: Option<&str>| Head {
            method: "GET".into(),
            target: target.into(),
            host: host.map(str::to_string),
        };
        assert_eq!(respond(&get("/", Some("evil.example:5601")), 5601).0, 403);
        assert_eq!(respond(&get("/", None), 5601).0, 403);
        assert_eq!(respond(&get("/", Some("localhost:5602")), 5601).0, 403);
        assert_eq!(respond(&get("/", Some("localhost:5601")), 5601).0, 200);
        assert_eq!(respond(&get("/api/start?habit=x", Some("127.0.0.1:5601")), 5601).0, 404);
        let post = Head { method: "POST".into(), ..get("/api/status", Some("127.0.0.1:5601")) };
        assert_eq!(respond(&post, 5601).0, 405);
    }
}
