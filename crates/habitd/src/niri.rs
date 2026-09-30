//! niri adapter: window/focus events from `niri msg --json event-stream`,
//! actions via `niri msg action`.

use crate::Event;
use habit_core::{Input, WindowInfo};
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::Sender;

pub fn available() -> bool {
    std::env::var_os("NIRI_SOCKET").is_some()
}

pub fn spawn_events(tx: Sender<Event>) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = stream_events(&tx).await {
                eprintln!("habitd: niri event stream failed: {e}");
            }
            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

async fn stream_events(tx: &Sender<Event>) -> anyhow::Result<()> {
    let mut child = Command::new("niri")
        .args(["msg", "--json", "event-stream"])
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await? {
        for input in parse_event(&line) {
            if tx.send(Event::Input(input)).await.is_err() {
                return Ok(());
            }
        }
    }
    anyhow::bail!("event stream ended")
}

fn parse_window(v: &Value) -> Option<WindowInfo> {
    Some(WindowInfo {
        id: v.get("id")?.as_u64()?,
        app_id: v.get("app_id").and_then(Value::as_str).unwrap_or("").to_string(),
        title: v.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
        pid: v.get("pid").and_then(Value::as_u64).map(|p| p as u32),
    })
}

fn is_focused(v: &Value) -> bool {
    v.get("is_focused").and_then(Value::as_bool).unwrap_or(false)
}

pub fn parse_event(line: &str) -> Vec<Input> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    if let Some(list) = v.pointer("/WindowsChanged/windows").and_then(Value::as_array) {
        let focused = list
            .iter()
            .find(|w| is_focused(w))
            .and_then(|w| w.get("id")?.as_u64());
        let windows = list.iter().filter_map(parse_window).collect();
        return vec![Input::WindowsReset(windows), Input::FocusChanged(focused)];
    }
    if let Some(raw) = v.pointer("/WindowOpenedOrChanged/window") {
        let Some(window) = parse_window(raw) else {
            return Vec::new();
        };
        let id = window.id;
        let mut inputs = vec![Input::WindowChanged(window)];
        if is_focused(raw) {
            inputs.push(Input::FocusChanged(Some(id)));
        }
        return inputs;
    }
    if let Some(id) = v.pointer("/WindowClosed/id").and_then(Value::as_u64) {
        return vec![Input::WindowClosed(id)];
    }
    if let Some(event) = v.get("WindowFocusChanged") {
        return vec![Input::FocusChanged(event.get("id").and_then(Value::as_u64))];
    }
    Vec::new()
}

fn action(args: Vec<String>) {
    tokio::spawn(async move {
        let result = Command::new("niri")
            .args(["msg", "action"])
            .args(&args)
            .stdout(Stdio::null())
            .status()
            .await;
        if let Err(e) = result {
            eprintln!("habitd: niri msg action {args:?} failed: {e}");
        }
    });
}

pub fn close_window(id: u64) {
    action(vec!["close-window".into(), "--id".into(), id.to_string()]);
}

pub fn focus_window(id: u64) {
    action(vec!["focus-window".into(), "--id".into(), id.to_string()]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Terminal programs, tmux sessions and passive habits only need the
    /// window's pid and focus, which niri's events carry.
    #[test]
    fn terminal_tracking_and_passive_habits_work_on_niri() {
        let config = habit_core::Config::from_toml(
            "[habits.code]\nkind = \"passive\"\nallow = [{ tmux_session = \"work\", program = \"nvim\" }]\n",
        )
        .unwrap();
        let mut engine = habit_core::Engine::new(config, habit_core::State::default());
        let events = [
            r#"{"WindowsChanged":{"windows":[{"id":4,"title":"~","app_id":"kitty","pid":9,"is_focused":false},{"id":5,"title":"x","app_id":"zen","pid":10,"is_focused":true}]}}"#,
            r#"{"WindowFocusChanged":{"id":4}}"#,
        ];
        for input in events.iter().flat_map(|e| parse_event(e)) {
            engine.handle(input, 0);
        }
        let window = engine.focused_terminal().expect("kitty is a focused terminal");
        assert_eq!((window.id, window.pid), (4, Some(9)));
        // What terminal.rs would report for it.
        let front = Input::TerminalProgram { window: 4, program: Some("nvim".into()), tmux_session: Some("work".into()) };
        engine.handle(front, 0);
        for t in 1..=30 {
            engine.handle(Input::Tick, t * 1000);
        }
        let usage = engine.app_usage(1, 30_000);
        assert_eq!(usage[0].app, "term:nvim");
        let snapshot = engine.snapshot(30_000);
        assert_eq!(snapshot.app_categories["term:nvim"], "Terminal");
        assert_eq!(snapshot.habits[0].today_focused_ms, 30_000);
    }

    #[test]
    fn parses_niri_events() {
        let inputs = parse_event(
            r#"{"WindowsChanged":{"windows":[{"id":4,"title":"~","app_id":"kitty","pid":9,"is_focused":true},{"id":5,"title":"x","app_id":"zen","pid":10,"is_focused":false}]}}"#,
        );
        assert!(matches!(&inputs[..], [Input::WindowsReset(w), Input::FocusChanged(Some(4))] if w.len() == 2));

        let inputs = parse_event(r#"{"WindowFocusChanged":{"id":null}}"#);
        assert_eq!(inputs, vec![Input::FocusChanged(None)]);

        let inputs = parse_event(r#"{"WindowClosed":{"id":7}}"#);
        assert_eq!(inputs, vec![Input::WindowClosed(7)]);

        let inputs = parse_event(
            r#"{"WindowOpenedOrChanged":{"window":{"id":8,"title":"t","app_id":"discord","pid":1,"is_focused":true}}}"#,
        );
        assert_eq!(inputs.len(), 2);
        assert!(parse_event(r#"{"WorkspacesChanged":{}}"#).is_empty());
    }
}
