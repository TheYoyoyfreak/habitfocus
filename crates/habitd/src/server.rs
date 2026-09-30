//! Unix socket server: one JSON request per line, answered by the engine loop.

use crate::Event;
use habit_core::Snapshot;
use habit_ipc::{Request, Response};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc::Sender, oneshot, watch};

pub fn bind(path: &Path) -> anyhow::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            anyhow::bail!("another habitd is already listening on {}", path.display());
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

pub fn spawn(listener: UnixListener, tx: Sender<Event>, snapshots: watch::Receiver<Snapshot>) {
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(handle(stream, tx.clone(), snapshots.clone()));
                }
                Err(e) => eprintln!("habitd: accept failed: {e}"),
            }
        }
    });
}

async fn write_json<T: serde::Serialize>(stream: &mut (impl AsyncWriteExt + Unpin), value: &T) -> bool {
    let mut line = serde_json::to_string(value).expect("serializes");
    line.push('\n');
    stream.write_all(line.as_bytes()).await.is_ok()
}

async fn handle(stream: UnixStream, tx: Sender<Event>, mut snapshots: watch::Receiver<Snapshot>) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let request = match serde_json::from_str::<Request>(&line) {
            Ok(r) => r,
            Err(e) => {
                if !write_json(&mut write, &Response::err(format!("invalid request: {e}"))).await {
                    return;
                }
                continue;
            }
        };
        if let Request::Subscribe = request {
            loop {
                let snapshot = snapshots.borrow_and_update().clone();
                if !write_json(&mut write, &snapshot).await || snapshots.changed().await.is_err() {
                    return;
                }
            }
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        if tx.send(Event::Request(request, reply_tx)).await.is_err() {
            return;
        }
        let Ok(response) = reply_rx.await else { return };
        if !write_json(&mut write, &response).await {
            return;
        }
    }
}
