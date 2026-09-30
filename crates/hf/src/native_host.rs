//! Native messaging host for the browser extension. Browsers start `hf` with
//! their own arguments and speak length-prefixed JSON over stdio.

use anyhow::{bail, Context};
use habit_ipc::{Request, Response};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const HOST_NAME: &str = "dev.habitfocus.host";
pub const FIREFOX_EXTENSION_ID: &str = "habitfocus@habitfocus.dev";
/// Derived from the public `key` in extension/manifest.json.
pub const CHROME_EXTENSION_ID: &str = "llodhgcgpbcebpccjlllfnbbdcibedpb";

const FIREFOX_DIRS: &[&str] = &[".mozilla", ".zen", ".librewolf", ".waterfox", ".floorp"];
const CHROMIUM_DIRS: &[&str] = &[
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware/Brave-Browser",
    ".config/vivaldi",
    ".config/microsoft-edge",
    ".config/net.imput.helium",
];

/// Chrome passes `chrome-extension://<id>/`, Firefox passes the manifest path
/// and the extension id.
pub fn is_browser_invocation(args: &[String]) -> bool {
    args.get(1)
        .is_some_and(|a| a.starts_with("chrome-extension://") || a.ends_with(".json"))
        || args.get(2).is_some_and(|a| a == FIREFOX_EXTENSION_ID)
}

fn read_message(input: &mut impl Read) -> std::io::Result<Option<Value>> {
    let mut len = [0u8; 4];
    match input.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut buf = vec![0u8; u32::from_ne_bytes(len) as usize];
    input.read_exact(&mut buf)?;
    serde_json::from_slice(&buf)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn write_message(out: &Mutex<std::io::Stdout>, message: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(message).expect("serializes");
    let mut out = out.lock().expect("not poisoned");
    out.write_all(&(body.len() as u32).to_ne_bytes())?;
    out.write_all(&body)?;
    out.flush()
}

/// Requests the extension may make on behalf of the blocked page.
fn allowed_from_browser(request: &Request) -> bool {
    matches!(
        request,
        Request::Status | Request::Start { .. } | Request::Unlock { .. } | Request::Relock { .. }
    )
}

fn handle(message: &Value, source: &str, out: &Mutex<std::io::Stdout>) -> std::io::Result<()> {
    let window = message.get("window").and_then(Value::as_u64);
    let text = |key: &str| message.get(key).and_then(Value::as_str).map(String::from);
    match message.get("type").and_then(Value::as_str) {
        Some("tab") => {
            let _ = habit_ipc::request(&Request::BrowserTab {
                source: source.into(),
                window,
                title: text("title"),
                url: text("url"),
            });
        }
        Some("window_closed") => {
            let _ = habit_ipc::request(&Request::BrowserTab {
                source: source.into(),
                window,
                title: None,
                url: None,
            });
        }
        Some("request") => {
            let response = match serde_json::from_value::<Request>(message["request"].clone()) {
                Ok(request) if allowed_from_browser(&request) => {
                    habit_ipc::request(&request).unwrap_or_else(Response::err)
                }
                Ok(_) => Response::err("request not allowed from the browser"),
                Err(e) => Response::err(format!("invalid request: {e}")),
            };
            write_message(out, &json!({ "type": "response", "id": message["id"], "response": response }))?;
        }
        _ => {}
    }
    Ok(())
}

pub fn run() -> anyhow::Result<()> {
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let source = format!("browser-{}", std::process::id());

    let stream_out = out.clone();
    std::thread::spawn(move || loop {
        if let Ok(lines) = habit_ipc::subscribe() {
            for line in lines {
                let Ok(line) = line else { break };
                let Ok(snapshot) = serde_json::from_str::<Value>(&line) else { continue };
                if write_message(&stream_out, &json!({ "type": "snapshot", "snapshot": snapshot })).is_err() {
                    std::process::exit(0);
                }
            }
        }
        if write_message(&stream_out, &json!({ "type": "disconnected" })).is_err() {
            std::process::exit(0);
        }
        std::thread::sleep(Duration::from_secs(3));
    });

    // Tells habitd which browser process this extension lives in, so a
    // commitment lock can close browsers that run without it.
    let hello_source = source.clone();
    std::thread::spawn(move || {
        let pid = std::os::unix::process::parent_id();
        loop {
            let _ = habit_ipc::request(&Request::BrowserHello { source: hello_source.clone(), pid });
            std::thread::sleep(Duration::from_secs(20));
        }
    });

    let mut stdin = std::io::stdin().lock();
    while let Some(message) = read_message(&mut stdin)? {
        handle(&message, &source, &out)?;
    }
    // Browser closed: forget its tabs.
    let _ = habit_ipc::request(&Request::BrowserTab { source, window: None, title: None, url: None });
    Ok(())
}

fn manifest_targets(home: &Path) -> Vec<(PathBuf, Value)> {
    let exe = std::env::current_exe().and_then(|p| p.canonicalize());
    let path = exe.map(|p| p.display().to_string()).unwrap_or_default();
    let base = json!({
        "name": HOST_NAME,
        "description": "habitfocus native messaging host",
        "path": path,
        "type": "stdio",
    });
    let mut targets = Vec::new();
    for (i, dir) in FIREFOX_DIRS.iter().enumerate() {
        // Always install for Firefox itself; other forks only when present.
        if i == 0 || home.join(dir).is_dir() {
            let mut manifest = base.clone();
            manifest["allowed_extensions"] = json!([FIREFOX_EXTENSION_ID]);
            targets.push((home.join(dir).join("native-messaging-hosts"), manifest));
        }
    }
    for dir in CHROMIUM_DIRS {
        if home.join(dir).is_dir() {
            let mut manifest = base.clone();
            manifest["allowed_origins"] = json!([format!("chrome-extension://{CHROME_EXTENSION_ID}/")]);
            targets.push((home.join(dir).join("NativeMessagingHosts"), manifest));
        }
    }
    targets
}

pub fn install(uninstall: bool) -> anyhow::Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let exe = std::env::current_exe()?.canonicalize()?;
    if !uninstall && exe.components().any(|c| c.as_os_str() == "target") {
        eprintln!(
            "hf: warning: registering a build-tree binary ({}); prefer `cargo install --path crates/hf` first",
            exe.display()
        );
    }
    let targets = manifest_targets(&home);
    if targets.is_empty() {
        bail!("no supported browser profile directories found");
    }
    for (dir, manifest) in targets {
        let file = dir.join(format!("{HOST_NAME}.json"));
        if uninstall {
            if std::fs::remove_file(&file).is_ok() {
                println!("removed {}", file.display());
            }
            continue;
        }
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&file, serde_json::to_string_pretty(&manifest)?)?;
        println!("wrote {}", file.display());
    }
    Ok(())
}
