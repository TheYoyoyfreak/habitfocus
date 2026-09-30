//! Hyprland adapter: window/focus events from the event socket
//! (`.socket2.sock`), the window list and actions through the request socket
//! (`.socket.sock`, the one `hyprctl` talks to).
//!
//! Window ids are the windows' addresses. The event socket leaves out what
//! the engine needs on some events (a new window's pid, class changes), so
//! the adapter keeps the window list and fetches `j/clients` again when an
//! event says it's out of date.

use crate::Event;
use anyhow::Context;
use habit_core::{Input, WindowInfo};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc::Sender;

pub fn available() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
}

/// `$XDG_RUNTIME_DIR/hypr/<signature>` (Hyprland ≥ 0.40), else `/tmp/hypr/<signature>`.
fn socket_dir() -> Option<PathBuf> {
    let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(|dir| PathBuf::from(dir).join("hypr"));
    runtime
        .into_iter()
        .chain([PathBuf::from("/tmp/hypr")])
        .map(|dir| dir.join(&signature))
        .find(|dir| dir.join(".socket2.sock").exists())
}

/// One request on the request socket; Hyprland answers and closes it.
async fn request(command: &str) -> anyhow::Result<String> {
    let dir = socket_dir().context("Hyprland socket not found")?;
    let mut stream = UnixStream::connect(dir.join(".socket.sock")).await?;
    stream.write_all(command.as_bytes()).await?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await?;
    Ok(reply)
}

pub fn spawn_events(tx: Sender<Event>) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = stream_events(&tx).await {
                eprintln!("habitd: Hyprland event stream failed: {e:#}");
            }
            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

async fn stream_events(tx: &Sender<Event>) -> anyhow::Result<()> {
    let dir = socket_dir().context("Hyprland socket not found")?;
    // Connect before fetching the list so no event falls in between.
    let stream = UnixStream::connect(dir.join(".socket2.sock")).await?;
    let mut lines = BufReader::new(stream).lines();
    let mut tracker = Tracker::default();
    let (clients, active) = fetch().await?;
    let mut inputs = tracker.reset(clients, active);
    loop {
        for input in inputs {
            if tx.send(Event::Input(input)).await.is_err() {
                return Ok(());
            }
        }
        let Some(line) = lines.next_line().await? else {
            anyhow::bail!("event stream ended");
        };
        inputs = match tracker.event(&line) {
            Step::Inputs(inputs) => inputs,
            Step::Refresh => {
                let (clients, active) = fetch().await?;
                tracker.refresh(clients, active)
            }
        };
    }
}

/// The window list and the focused window.
async fn fetch() -> anyhow::Result<(Vec<WindowInfo>, Option<u64>)> {
    let clients = parse_clients(&request("j/clients").await?)?;
    let active = parse_active(&request("j/activewindow").await?);
    Ok((clients, active))
}

/// `5612a3c0` in events, `0x5612a3c0` in JSON; empty when nothing is focused.
fn parse_address(text: &str) -> Option<u64> {
    let text = text.trim();
    u64::from_str_radix(text.strip_prefix("0x").unwrap_or(text), 16).ok()
}

fn parse_clients(text: &str) -> anyhow::Result<Vec<WindowInfo>> {
    let list: Vec<Value> = serde_json::from_str(text).context("unexpected reply to j/clients")?;
    Ok(list
        .iter()
        // Unmapped clients are still being set up or torn down.
        .filter(|c| c.get("mapped").and_then(Value::as_bool) != Some(false))
        .filter_map(|c| {
            Some(WindowInfo {
                id: parse_address(c.get("address")?.as_str()?)?,
                app_id: c.get("class").and_then(Value::as_str).unwrap_or("").to_string(),
                title: c.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
                pid: c.get("pid").and_then(Value::as_i64).filter(|&p| p > 0).map(|p| p as u32),
            })
        })
        .collect())
}

/// `j/activewindow` is `{}` when nothing is focused.
fn parse_active(text: &str) -> Option<u64> {
    let v: Value = serde_json::from_str(text).ok()?;
    parse_address(v.get("address")?.as_str()?)
}

enum Step {
    Inputs(Vec<Input>),
    /// The list is out of date; fetch it and diff.
    Refresh,
}

#[derive(Default)]
struct Tracker {
    windows: HashMap<u64, WindowInfo>,
    /// Class from the last `activewindow` event. Hyprland has no event for a
    /// class change, but the focus events carry the class, so a mismatch
    /// with the list shows up when the window gains focus.
    active_class: Option<String>,
}

impl Tracker {
    /// After (re)connecting: the whole list.
    fn reset(&mut self, clients: Vec<WindowInfo>, active: Option<u64>) -> Vec<Input> {
        self.windows = clients.iter().map(|w| (w.id, w.clone())).collect();
        vec![Input::WindowsReset(clients), Input::FocusChanged(active)]
    }

    /// Only what changed since the last list, like niri's incremental events.
    fn refresh(&mut self, clients: Vec<WindowInfo>, active: Option<u64>) -> Vec<Input> {
        let mut inputs = Vec::new();
        let mut old = std::mem::take(&mut self.windows);
        for window in clients {
            if old.remove(&window.id).as_ref() != Some(&window) {
                inputs.push(Input::WindowChanged(window.clone()));
            }
            self.windows.insert(window.id, window);
        }
        let mut closed: Vec<u64> = old.into_keys().collect();
        closed.sort_unstable();
        inputs.extend(closed.into_iter().map(Input::WindowClosed));
        inputs.push(Input::FocusChanged(active));
        inputs
    }

    fn event(&mut self, line: &str) -> Step {
        let Some((name, data)) = line.split_once(">>") else {
            return Step::Inputs(Vec::new());
        };
        let inputs = match name {
            // Carries class and title but not the pid, which the browser guard needs.
            "openwindow" => return Step::Refresh,
            "closewindow" => match parse_address(data) {
                Some(id) if self.windows.remove(&id).is_some() => vec![Input::WindowClosed(id)],
                _ => Vec::new(),
            },
            "activewindow" => {
                let class = data.split_once(',').map_or(data, |(class, _)| class);
                self.active_class = Some(class.to_string());
                Vec::new()
            }
            "activewindowv2" => {
                let active = parse_address(data);
                let class = self.active_class.take();
                if let Some(id) = active {
                    match self.windows.get(&id) {
                        None => return Step::Refresh,
                        Some(w) if class.is_some_and(|class| class != w.app_id) => return Step::Refresh,
                        Some(_) => {}
                    }
                }
                vec![Input::FocusChanged(active)]
            }
            "windowtitlev2" => {
                let Some((address, title)) = data.split_once(',') else {
                    return Step::Inputs(Vec::new());
                };
                match parse_address(address).and_then(|id| self.windows.get_mut(&id)) {
                    Some(window) if window.title != title => {
                        window.title = title.to_string();
                        vec![Input::WindowChanged(window.clone())]
                    }
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        };
        Step::Inputs(inputs)
    }
}

/// Set once a classic dispatcher worked where the Lua one didn't.
static CLASSIC_SYNTAX: AtomicBool = AtomicBool::new(false);

async fn try_dispatch(args: &str) -> Result<(), String> {
    match request(&format!("dispatch {args}")).await {
        Ok(reply) if reply.trim() == "ok" => Ok(()),
        Ok(reply) => Err(reply.trim().to_string()),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// A Lua config (Hyprland ≥ 0.56) takes `hl.dsp.…` dispatchers, a classic
/// one `closewindow …`. Tries the one that worked last, then the other.
fn dispatch(lua: String, classic: String) {
    tokio::spawn(async move {
        let classic_first = CLASSIC_SYNTAX.load(Ordering::Relaxed);
        let (first, second) = if classic_first { (&classic, &lua) } else { (&lua, &classic) };
        let Err(first_error) = try_dispatch(first).await else { return };
        match try_dispatch(second).await {
            Ok(()) => CLASSIC_SYNTAX.store(!classic_first, Ordering::Relaxed),
            Err(second_error) => {
                eprintln!("habitd: Hyprland dispatch failed: {first}: {first_error}; {second}: {second_error}")
            }
        }
    });
}

pub fn close_window(id: u64) {
    dispatch(
        format!("hl.dsp.window.close({{ window = \"address:0x{id:x}\" }})"),
        format!("closewindow address:0x{id:x}"),
    );
}

pub fn focus_window(id: u64) {
    dispatch(
        format!("hl.dsp.focus({{ window = \"address:0x{id:x}\" }})"),
        format!("focuswindow address:0x{id:x}"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENTS: &str = r#"[
        {"address":"0x55d0a1b2c3d0","mapped":true,"hidden":false,"class":"kitty","title":"~","pid":900,"focusHistoryID":0},
        {"address":"0x55d0a1b2c4e0","mapped":true,"hidden":false,"class":"zen","title":"Start","pid":1000,"focusHistoryID":1},
        {"address":"0x55d0a1b2c5f0","mapped":false,"class":"","title":"","pid":-1}
    ]"#;

    fn inputs(step: Step) -> Vec<Input> {
        match step {
            Step::Inputs(inputs) => inputs,
            Step::Refresh => panic!("expected inputs, got a refresh"),
        }
    }

    fn tracker() -> Tracker {
        let mut tracker = Tracker::default();
        tracker.reset(parse_clients(CLIENTS).unwrap(), Some(0x55d0a1b2c3d0));
        tracker
    }

    #[test]
    fn parses_the_window_list() {
        let clients = parse_clients(CLIENTS).unwrap();
        assert_eq!(clients.len(), 2, "unmapped clients are skipped");
        assert_eq!(
            clients[0],
            WindowInfo { id: 0x55d0a1b2c3d0, app_id: "kitty".into(), title: "~".into(), pid: Some(900) }
        );
        assert_eq!(parse_active(r#"{"address":"0x55d0a1b2c4e0","class":"zen"}"#), Some(0x55d0a1b2c4e0));
        assert_eq!(parse_active("{}"), None);
        assert!(parse_clients("unknown request").is_err());
    }

    #[test]
    fn reset_sends_the_list_and_focus() {
        let inputs = Tracker::default().reset(parse_clients(CLIENTS).unwrap(), Some(0x55d0a1b2c3d0));
        assert!(matches!(&inputs[..], [Input::WindowsReset(w), Input::FocusChanged(Some(0x55d0a1b2c3d0))] if w.len() == 2));
    }

    #[test]
    fn translates_events() {
        let mut t = tracker();
        t.event("activewindow>>zen,Start");
        assert_eq!(inputs(t.event("activewindowv2>>55d0a1b2c4e0")), vec![Input::FocusChanged(Some(0x55d0a1b2c4e0))]);
        // Nothing focused: both events have empty fields.
        t.event("activewindow>>,");
        assert_eq!(inputs(t.event("activewindowv2>>")), vec![Input::FocusChanged(None)]);

        let changed = inputs(t.event("windowtitlev2>>55d0a1b2c4e0,YouTube, a video - Zen"));
        assert!(matches!(&changed[..], [Input::WindowChanged(w)] if w.title == "YouTube, a video - Zen" && w.pid == Some(1000)));
        assert!(inputs(t.event("windowtitlev2>>55d0a1b2c4e0,YouTube, a video - Zen")).is_empty(), "same title");

        assert_eq!(inputs(t.event("closewindow>>55d0a1b2c3d0")), vec![Input::WindowClosed(0x55d0a1b2c3d0)]);
        assert!(inputs(t.event("closewindow>>55d0a1b2c3d0")).is_empty(), "already closed");
        assert!(inputs(t.event("workspacev2>>2,2")).is_empty());
        assert!(inputs(t.event("garbage")).is_empty());
    }

    #[test]
    fn refreshes_when_the_list_is_out_of_date() {
        let mut t = tracker();
        assert!(matches!(t.event("openwindow>>55d0a1b2c6a0,1,discord,Discord"), Step::Refresh));
        // Focus on a window that isn't known yet.
        t.event("activewindow>>discord,Discord");
        assert!(matches!(t.event("activewindowv2>>55d0a1b2c6a0"), Step::Refresh));
        // A known window whose class changed since the list was fetched.
        t.event("activewindow>>steam_app_275850,No Man's Sky");
        assert!(matches!(t.event("activewindowv2>>55d0a1b2c4e0"), Step::Refresh));
    }

    #[test]
    fn refresh_sends_only_differences() {
        let mut t = tracker();
        let clients = parse_clients(
            r#"[
            {"address":"0x55d0a1b2c3d0","mapped":true,"class":"kitty","title":"~","pid":900},
            {"address":"0x55d0a1b2c6a0","mapped":true,"class":"discord","title":"Discord","pid":1100}
        ]"#,
        )
        .unwrap();
        let inputs = t.refresh(clients, Some(0x55d0a1b2c6a0));
        assert!(matches!(
            &inputs[..],
            [Input::WindowChanged(w), Input::WindowClosed(0x55d0a1b2c4e0), Input::FocusChanged(Some(0x55d0a1b2c6a0))]
                if w.app_id == "discord" && w.pid == Some(1100)
        ));
    }
}
