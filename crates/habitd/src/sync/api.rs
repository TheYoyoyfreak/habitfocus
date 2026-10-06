//! The sync server's HTTP API (habitfocus_sync_server), spoken through
//! `curl` like the update check, which keeps TLS out of the static binaries.
//!
//! Secrets never go on curl's command line, where every process can read
//! them: the whole request, token and body included, is passed as a curl
//! config on stdin.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

const TIMEOUT_SECS: &str = "30";

/// A record as the server stores it. `data` is sealed by `crypto`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireRecord {
    pub id: String,
    pub idx: u64,
    pub host: WireHost,
    /// Nanoseconds since the epoch.
    pub timestamp: u64,
    pub version: String,
    pub tag: String,
    pub data: WireData,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireHost {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireData {
    pub raw: String,
    pub cek: String,
}

/// The last `idx` of every log: host -> tag -> idx.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerStatus {
    pub hosts: HashMap<String, HashMap<String, u64>>,
}

impl ServerStatus {
    pub fn tail(&self, host: &str, tag: &str) -> Option<u64> {
        self.hosts.get(host).and_then(|tags| tags.get(tag)).copied()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RemoteDevice {
    pub id: String,
    pub name: String,
    pub last_seen_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    /// The session is gone: this device was signed out elsewhere.
    SignedOut,
    /// No answer: offline, wrong address, or the server is down.
    Unreachable(String),
    Other(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::SignedOut => write!(f, "this device was signed out of the sync account; sign in again with `hf sync login`"),
            ApiError::Unreachable(e) | ApiError::Other(e) => write!(f, "{e}"),
        }
    }
}

/// What a signed-in device does on the server.
pub trait Api {
    fn status(&self) -> Result<ServerStatus, ApiError>;
    fn upload(&self, records: &[WireRecord]) -> Result<(), ApiError>;
    /// Up to `count` records of one log from `start` on.
    fn next(&self, host: &str, tag: &str, start: u64, count: u64) -> Result<Vec<WireRecord>, ApiError>;
    fn devices(&self) -> Result<Vec<RemoteDevice>, ApiError>;
    /// Signs a device out (this one, on logout).
    fn delete_device(&self, id: &str) -> Result<(), ApiError>;
}

/// The device signing in.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}

/// A sync server reached through curl.
pub struct Http {
    server: String,
    token: Option<String>,
}

impl Http {
    /// `server` is the base URL, e.g. `https://sync.example.org`.
    pub fn new(server: &str, token: Option<String>) -> Result<Http, String> {
        let server = server.trim().trim_end_matches('/');
        if !(server.starts_with("https://") || server.starts_with("http://")) {
            return Err(format!("the server address must start with https:// (got {server:?})"));
        }
        if server.chars().any(|c| c.is_whitespace() || c == '"' || c == '\\') {
            return Err(format!("invalid server address {server:?}"));
        }
        Ok(Http { server: server.to_string(), token })
    }

    pub fn register(&self, username: &str, email: &str, password: &str, device: &DeviceInfo) -> Result<String, String> {
        let body = serde_json::json!({ "username": username, "email": email, "password": password, "device": device });
        self.session("POST", "/register", &body)
    }

    pub fn login(&self, username: &str, password: &str, device: &DeviceInfo) -> Result<String, String> {
        let body = serde_json::json!({ "username": username, "password": password, "device": device });
        self.session("POST", "/login", &body)
    }

    /// Whether the server answers.
    pub fn health(&self) -> Result<(), ApiError> {
        self.empty("GET", "/healthz", None)
    }

    fn session(&self, method: &str, path: &str, body: &serde_json::Value) -> Result<String, String> {
        let (code, text) = self.call(method, path, Some(&body.to_string())).map_err(|e| e.to_string())?;
        match code {
            200 => serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v["session"].as_str().map(str::to_string))
                .ok_or_else(|| "unexpected answer from the sync server".to_string()),
            _ => Err(reason(code, &text)),
        }
    }

    /// One request; returns the status code and body.
    fn call(&self, method: &str, path: &str, body: Option<&str>) -> Result<(u16, String), ApiError> {
        let config = curl_config(method, &format!("{}{path}", self.server), self.token.as_deref(), body);
        let mut child = Command::new("curl")
            .args(["--config", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ApiError::Other(format!("can't run curl: {e}")))?;
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let written = stdin.write_all(config.as_bytes());
        drop(stdin);
        let output = child.wait_with_output().map_err(|e| ApiError::Other(format!("curl failed: {e}")))?;
        written.map_err(|e| ApiError::Other(format!("curl failed: {e}")))?;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            return Err(ApiError::Unreachable(format!("can't reach the sync server: {}", error.trim())));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let (body, code) = text.rsplit_once('\n').unwrap_or(("", &text));
        let code = code.trim().parse().map_err(|_| ApiError::Other("unexpected answer from curl".into()))?;
        if code == 403 && self.token.is_some() {
            return Err(ApiError::SignedOut);
        }
        Ok((code, body.to_string()))
    }

    fn json<T: for<'de> Deserialize<'de>>(&self, method: &str, path: &str, body: Option<&str>) -> Result<T, ApiError> {
        let (code, text) = self.call(method, path, body)?;
        if code != 200 {
            return Err(ApiError::Other(reason(code, &text)));
        }
        serde_json::from_str(&text).map_err(|e| ApiError::Other(format!("unexpected answer from the sync server: {e}")))
    }

    fn empty(&self, method: &str, path: &str, body: Option<&str>) -> Result<(), ApiError> {
        let (code, text) = self.call(method, path, body)?;
        match code {
            200 => Ok(()),
            _ => Err(ApiError::Other(reason(code, &text))),
        }
    }
}

impl Api for Http {
    fn status(&self) -> Result<ServerStatus, ApiError> {
        self.json("GET", "/api/v0/record", None)
    }

    fn upload(&self, records: &[WireRecord]) -> Result<(), ApiError> {
        let body = serde_json::to_string(records).expect("records serialize");
        self.empty("POST", "/api/v0/record", Some(&body))
    }

    fn next(&self, host: &str, tag: &str, start: u64, count: u64) -> Result<Vec<WireRecord>, ApiError> {
        // Hosts are UUIDs and tags table names: nothing to escape.
        self.json("GET", &format!("/api/v0/record/next?host={host}&tag={tag}&start={start}&count={count}"), None)
    }

    fn devices(&self) -> Result<Vec<RemoteDevice>, ApiError> {
        self.json("GET", "/api/v0/devices", None)
    }

    fn delete_device(&self, id: &str) -> Result<(), ApiError> {
        self.empty("DELETE", &format!("/api/v0/devices/{id}"), None)
    }
}

/// The server's `{"reason": …}`, or the status code.
fn reason(code: u16, body: &str) -> String {
    let reason = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["reason"].as_str().map(str::to_string));
    match reason {
        Some(reason) => format!("the sync server refused: {reason}"),
        None => format!("the sync server answered with status {code}"),
    }
}

/// A quoted value in a curl config file.
fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\r', "\\r"))
}

/// The request as a curl config: the status code goes on a last line.
fn curl_config(method: &str, url: &str, token: Option<&str>, body: Option<&str>) -> String {
    let mut config = String::new();
    let mut line = |key: &str, value: &str| config.push_str(&format!("{key} = {}\n", quote(value)));
    line("url", url);
    line("request", method);
    line("max-time", TIMEOUT_SECS);
    line("write-out", "\n%{http_code}");
    if let Some(token) = token {
        line("header", &format!("Authorization: Token {token}"));
    }
    if let Some(body) = body {
        line("header", "Content-Type: application/json");
        line("data-binary", body);
    }
    config.push_str("silent\nshow-error\n");
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_go_to_curl_as_quoted_config() {
        let body = r#"{"password":"p\"w\\x"}"#;
        let config = curl_config("POST", "https://sync.example/login", Some("tok"), Some(body));
        assert!(config.contains("url = \"https://sync.example/login\"\n"));
        assert!(config.contains("header = \"Authorization: Token tok\"\n"));
        assert!(config.contains(r#"data-binary = "{\"password\":\"p\\\"w\\\\x\"}""#), "{config}");
        assert!(config.contains("write-out = \"\\n%{http_code}\"\n"));
        assert!(!curl_config("GET", "https://x", None, None).contains("Authorization"));
    }

    #[test]
    fn server_addresses_are_checked() {
        assert_eq!(Http::new(" https://sync.example/ ", None).unwrap().server, "https://sync.example");
        assert!(Http::new("http://127.0.0.1:8888", None).is_ok());
        assert!(Http::new("sync.example", None).is_err());
        assert!(Http::new("https://sync.example\" -o /etc/x", None).is_err());
    }

    #[test]
    fn refusals_show_the_servers_reason() {
        assert_eq!(reason(401, r#"{"reason":"invalid username or password"}"#), "the sync server refused: invalid username or password");
        assert_eq!(reason(502, "<html>"), "the sync server answered with status 502");
    }
}
