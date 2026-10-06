//! One sync: send this device's new archive rows, fetch the other devices'.
//!
//! Every archive table is one log per device on the server (tag = table
//! name). A server record holds a batch of rows, sealed by `crypto`, and is
//! numbered by `idx` from 0 in its log. Rows go out in rowid order.
//!
//! Bookkeeping in `sync_state`:
//! - `push:<table>` = `<next idx>:<last rowid sent>`
//! - `pull:<device>:<table>` = the next idx to fetch
//!
//! Before sending, the server's last idx of this device's log is compared with
//! the local one. When they differ (a crash between upload and saving the
//! cursor, signing in again, a deleted store) the cursor is taken from the
//! server's last record, so nothing is skipped or sent twice under a new idx.

use super::api::{Api, ApiError, WireData, WireHost, WireRecord};
use super::crypto::{self, Key};
use crate::archive::{Archive, Device, Table};
use habit_core::archive::{Afk, Record, Visit};
use habit_core::state::{Event, HistoryEntry};
use serde::{Deserialize, Serialize};

/// Most rows in one record.
const BATCH_ROWS: usize = 500;
/// A record stops growing past this much JSON (the server takes 1 MiB, and
/// sealing adds a third).
const BATCH_BYTES: usize = 512 * 1024;
/// Records fetched per request.
const PAGE: u64 = 20;
/// Format of the sealed data.
const VERSION: &str = "v1";

/// What a sync moved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub sent: usize,
    pub received: usize,
}

#[derive(Debug, PartialEq)]
pub enum SyncError {
    Api(ApiError),
    /// The server's data doesn't open with this key.
    Crypto(String),
    Archive(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Api(e) => write!(f, "{e}"),
            SyncError::Crypto(e) => write!(f, "{e}"),
            SyncError::Archive(e) => write!(f, "history.db: {e}"),
        }
    }
}

impl From<ApiError> for SyncError {
    fn from(e: ApiError) -> Self {
        SyncError::Api(e)
    }
}

impl From<anyhow::Error> for SyncError {
    fn from(e: anyhow::Error) -> Self {
        SyncError::Archive(format!("{e:#}"))
    }
}

/// The decrypted data of a record: rows with their rowid on the device that
/// wrote them.
#[derive(Debug, Serialize, Deserialize)]
struct Batch {
    rows: Vec<(i64, serde_json::Value)>,
}

fn aad(record: &WireRecord) -> String {
    format!("habitfocus sync|{}|{}|{}|{}|{}", record.id, record.idx, record.host.id, record.tag, record.version)
}

fn now_ns() -> u64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
}

fn table_of(tag: &str) -> Option<Table> {
    Table::ALL.into_iter().find(|t| t.name() == tag)
}

fn to_json(record: &Record) -> serde_json::Value {
    match record {
        Record::Event(e) => serde_json::to_value(e),
        Record::Session(s) => serde_json::to_value(s),
        Record::Visit(v) => serde_json::to_value(v),
        Record::Afk(a) => serde_json::to_value(a),
    }
    .expect("records serialize")
}

/// A row of `table`, or `None` when it's from a newer habitfocus (an event
/// kind this one doesn't know).
fn from_json(table: Table, value: serde_json::Value) -> Option<Record> {
    match table {
        Table::Events => serde_json::from_value::<Event>(value).ok().map(Record::Event),
        Table::Sessions => serde_json::from_value::<HistoryEntry>(value).ok().map(Record::Session),
        Table::Visits => serde_json::from_value::<Visit>(value).ok().map(Record::Visit),
        Table::Afk => serde_json::from_value::<Afk>(value).ok().map(Record::Afk),
    }
}

fn seal(key: &Key, device: &Device, table: Table, idx: u64, batch: &Batch) -> WireRecord {
    let mut record = WireRecord {
        id: uuid::Uuid::now_v7().to_string(),
        idx,
        host: WireHost { id: device.id.clone() },
        timestamp: now_ns(),
        version: VERSION.into(),
        tag: table.name().into(),
        data: WireData { raw: String::new(), cek: String::new() },
    };
    let plaintext = serde_json::to_vec(batch).expect("batches serialize");
    let (raw, cek) = crypto::seal(key, &plaintext, &aad(&record));
    record.data = WireData { raw, cek };
    record
}

fn open(key: &Key, record: &WireRecord) -> Result<Batch, SyncError> {
    if record.version != VERSION {
        return Err(SyncError::Crypto(format!(
            "a record has format {}; update habitfocus on this device",
            record.version
        )));
    }
    let plaintext = crypto::open(key, &record.data.raw, &record.data.cek, &aad(record)).map_err(SyncError::Crypto)?;
    serde_json::from_slice(&plaintext).map_err(|e| SyncError::Crypto(format!("a record can't be read: {e}")))
}

/// Checks that the account's data opens with `key`, by opening its first
/// record. An empty account accepts any key.
pub fn check_key(api: &dyn Api, key: &Key) -> Result<(), SyncError> {
    let status = api.status()?;
    let first = status.hosts.iter().flat_map(|(host, tags)| tags.keys().map(move |tag| (host, tag))).next();
    if let Some((host, tag)) = first {
        if let Some(record) = api.next(host, tag, 0, 1)?.first() {
            open(key, record)?;
        }
    }
    Ok(())
}

pub fn sync(api: &dyn Api, archive: &mut Archive, key: &Key) -> Result<Summary, SyncError> {
    let device = archive.device().clone();
    let status = api.status()?;
    let mut summary = Summary::default();

    for table in Table::ALL {
        let key_name = format!("push:{}", table.name());
        let saved = archive.sync_value(&key_name)?.and_then(|v| {
            let (next, cursor) = v.split_once(':')?;
            Some((next.parse::<u64>().ok()?, cursor.parse::<i64>().ok()?))
        });
        let (mut next, mut cursor) = saved.unwrap_or((0, 0));
        let server_next = status.tail(&device.id, table.name()).map_or(0, |tail| tail + 1);
        if server_next != next {
            (next, cursor) = match server_next {
                0 => (0, 0),
                _ => {
                    let last = api.next(&device.id, table.name(), server_next - 1, 1)?;
                    let last = last.first().ok_or_else(|| ApiError::Other("the server lost a record".into()))?;
                    let sent = open(key, last)?.rows.iter().map(|(rowid, _)| *rowid).max().unwrap_or(0);
                    (server_next, sent)
                }
            };
            archive.set_sync_value(&key_name, &format!("{next}:{cursor}"))?;
        }
        loop {
            let rows = archive.local_rows(table, cursor, BATCH_ROWS)?;
            let Some(first) = rows.first() else { break };
            let mut batch = Batch { rows: vec![(first.rowid, to_json(&first.record))] };
            let mut bytes = batch.rows[0].1.to_string().len();
            for row in &rows[1..] {
                let value = to_json(&row.record);
                bytes += value.to_string().len();
                if bytes > BATCH_BYTES {
                    break;
                }
                batch.rows.push((row.rowid, value));
            }
            api.upload(&[seal(key, &device, table, next, &batch)])?;
            cursor = batch.rows.last().expect("not empty").0;
            next += 1;
            summary.sent += batch.rows.len();
            archive.set_sync_value(&key_name, &format!("{next}:{cursor}"))?;
        }
    }

    for (host, tags) in &status.hosts {
        if *host == device.id {
            continue;
        }
        for (tag, &tail) in tags {
            // A table from a newer habitfocus: left for when this one knows it.
            let Some(table) = table_of(tag) else { continue };
            let key_name = format!("pull:{host}:{tag}");
            let mut start: u64 = archive.sync_value(&key_name)?.and_then(|v| v.parse().ok()).unwrap_or(0);
            while start <= tail {
                let records = api.next(host, tag, start, PAGE)?;
                if records.is_empty() {
                    break;
                }
                for record in &records {
                    let rows: Vec<(String, Record)> = open(key, record)?
                        .rows
                        .into_iter()
                        .filter_map(|(rowid, value)| Some((format!("{host}:{tag}:{rowid}"), from_json(table, value)?)))
                        .collect();
                    summary.received += archive.insert_remote(host, &rows)?;
                    start = record.idx + 1;
                    archive.set_sync_value(&key_name, &start.to_string())?;
                }
            }
        }
    }

    for remote in api.devices()? {
        if remote.id != device.id {
            archive.set_remote_device(&remote.id, &remote.name, Some(remote.last_seen_ms))?;
        }
    }
    Ok(summary)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::sync::api::{RemoteDevice, ServerStatus};
    use habit_core::archive::AfkReason;
    use habit_core::state::{EventKind, State};
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// The server's record store in memory, for any number of devices.
    #[derive(Default)]
    pub struct FakeServer {
        pub logs: RefCell<BTreeMap<(String, String), BTreeMap<u64, WireRecord>>>,
        pub devices: RefCell<Vec<RemoteDevice>>,
        pub uploads: RefCell<usize>,
        /// Fails uploads after this many more.
        pub fail_uploads_after: RefCell<Option<usize>>,
    }

    impl Api for FakeServer {
        fn status(&self) -> Result<ServerStatus, ApiError> {
            let mut status = ServerStatus::default();
            for ((host, tag), log) in self.logs.borrow().iter() {
                if let Some(&tail) = log.keys().last() {
                    status.hosts.entry(host.clone()).or_default().insert(tag.clone(), tail);
                }
            }
            Ok(status)
        }

        fn upload(&self, records: &[WireRecord]) -> Result<(), ApiError> {
            if let Some(left) = self.fail_uploads_after.borrow_mut().as_mut() {
                if *left == 0 {
                    return Err(ApiError::Other("connection lost".into()));
                }
                *left -= 1;
            }
            *self.uploads.borrow_mut() += 1;
            for record in records {
                let mut logs = self.logs.borrow_mut();
                let log = logs.entry((record.host.id.clone(), record.tag.clone())).or_default();
                // The first upload of an idx wins, like on the server.
                log.entry(record.idx).or_insert_with(|| record.clone());
            }
            Ok(())
        }

        fn next(&self, host: &str, tag: &str, start: u64, count: u64) -> Result<Vec<WireRecord>, ApiError> {
            let logs = self.logs.borrow();
            let Some(log) = logs.get(&(host.to_string(), tag.to_string())) else { return Ok(Vec::new()) };
            Ok(log.range(start..).take(count as usize).map(|(_, r)| r.clone()).collect())
        }

        fn devices(&self) -> Result<Vec<RemoteDevice>, ApiError> {
            Ok(self.devices.borrow().clone())
        }

        fn delete_device(&self, _id: &str) -> Result<(), ApiError> {
            Ok(())
        }
    }

    pub fn visit(key: &str, start: u64) -> Record {
        Record::Visit(Visit { key: key.into(), start, end: start + 10, active_ms: 10, habit: None, ..Default::default() })
    }

    fn event(at: u64) -> Record {
        Record::Event(Event { at, kind: EventKind::CreditEarned, subject: None, amount: 1, text: format!("earned {at}") })
    }

    fn device_visits(archive: &Archive, device: &str) -> Vec<String> {
        archive.remote_visit_keys(device)
    }

    #[test]
    fn devices_get_each_others_rows_once() {
        let server = FakeServer::default();
        let key = Key::generate();
        let (mut laptop, mut desktop) = (Archive::in_memory(&State::default()), Archive::in_memory(&State::default()));
        laptop.write(&[visit("zen", 0), visit("kitty", 10), event(5)]).unwrap();
        laptop.write(&[Record::Afk(Afk { start: 20, end: 30, reason: AfkReason::Idle })]).unwrap();
        desktop.write(&[visit("steam", 100)]).unwrap();

        assert_eq!(sync(&server, &mut laptop, &key).unwrap(), Summary { sent: 4, received: 0 });
        assert_eq!(sync(&server, &mut desktop, &key).unwrap(), Summary { sent: 1, received: 4 });
        assert_eq!(sync(&server, &mut laptop, &key).unwrap(), Summary { sent: 0, received: 1 });
        assert_eq!(sync(&server, &mut desktop, &key).unwrap(), Summary::default(), "nothing new");

        let laptop_id = laptop.device().id.clone();
        assert_eq!(device_visits(&desktop, &laptop_id), ["zen", "kitty"]);
        assert_eq!(device_visits(&laptop, &desktop.device().id), ["steam"]);
        // This device's views stay its own.
        assert_eq!(desktop.visits(0, 1000).unwrap().len(), 1);

        // New rows go out in a new record of the same log.
        laptop.write(&[visit("zed", 200)]).unwrap();
        assert_eq!(sync(&server, &mut laptop, &key).unwrap().sent, 1);
        assert_eq!(sync(&server, &mut desktop, &key).unwrap().received, 1);
        assert_eq!(device_visits(&desktop, &laptop_id), ["zen", "kitty", "zed"]);
        assert_eq!(server.logs.borrow()[&(laptop_id, "visits".to_string())].len(), 2);
    }

    #[test]
    fn the_server_only_sees_sealed_data() {
        let server = FakeServer::default();
        let mut laptop = Archive::in_memory(&State::default());
        laptop.write(&[visit("site:secret.example", 0)]).unwrap();
        sync(&server, &mut laptop, &Key::generate()).unwrap();
        let stored = serde_json::to_string(&*server.logs.borrow().values().collect::<Vec<_>>()).unwrap();
        assert!(!stored.contains("secret.example"));
    }

    #[test]
    fn big_tables_go_out_in_batches() {
        let server = FakeServer::default();
        let key = Key::generate();
        let mut laptop = Archive::in_memory(&State::default());
        let visits: Vec<Record> = (0..1203).map(|i| visit("kitty", i * 10)).collect();
        laptop.write(&visits).unwrap();
        assert_eq!(sync(&server, &mut laptop, &key).unwrap().sent, 1203);
        let laptop_id = laptop.device().id.clone();
        let idx: Vec<u64> = server.logs.borrow()[&(laptop_id.clone(), "visits".into())].keys().copied().collect();
        assert_eq!(idx, [0, 1, 2]);

        // A record also stops at its size limit.
        let long = "x".repeat(200 * 1024);
        laptop.write(&(0..4).map(|i| visit(&long, i)).collect::<Vec<_>>()).unwrap();
        sync(&server, &mut laptop, &key).unwrap();
        assert_eq!(server.logs.borrow()[&(laptop_id, "visits".into())].len(), 5, "two rows per record");

        let mut desktop = Archive::in_memory(&State::default());
        assert_eq!(sync(&server, &mut desktop, &key).unwrap().received, 1207);
    }

    #[test]
    fn an_interrupted_sync_goes_on_where_the_server_is() {
        let server = FakeServer::default();
        let key = Key::generate();
        let mut laptop = Archive::in_memory(&State::default());
        laptop.write(&(0..1100).map(|i| visit("kitty", i)).collect::<Vec<_>>()).unwrap();
        *server.fail_uploads_after.borrow_mut() = Some(1);
        assert!(sync(&server, &mut laptop, &key).is_err());
        *server.fail_uploads_after.borrow_mut() = None;
        assert_eq!(sync(&server, &mut laptop, &key).unwrap().sent, 600, "the rest");

        // An upload that arrived but whose cursor wasn't saved.
        laptop.write(&[visit("zen", 5000)]).unwrap();
        let saved = laptop.sync_value("push:visits").unwrap().unwrap();
        sync(&server, &mut laptop, &key).unwrap();
        laptop.set_sync_value("push:visits", &saved).unwrap();
        laptop.write(&[visit("zed", 6000)]).unwrap();
        assert_eq!(sync(&server, &mut laptop, &key).unwrap().sent, 1, "only the new one");

        let mut desktop = Archive::in_memory(&State::default());
        sync(&server, &mut desktop, &key).unwrap();
        assert_eq!(device_visits(&desktop, &laptop.device().id).len(), 1102);
    }

    #[test]
    fn signing_in_again_continues_the_devices_logs() {
        let server = FakeServer::default();
        let key = Key::generate();
        let mut laptop = Archive::in_memory(&State::default());
        laptop.write(&[visit("zen", 0), visit("kitty", 10)]).unwrap();
        sync(&server, &mut laptop, &key).unwrap();
        laptop.clear_sync_state().unwrap();
        laptop.write(&[visit("zed", 20)]).unwrap();
        assert_eq!(sync(&server, &mut laptop, &key).unwrap().sent, 1);

        // An emptied store gets everything again.
        server.logs.borrow_mut().clear();
        assert_eq!(sync(&server, &mut laptop, &key).unwrap().sent, 3);
    }

    #[test]
    fn another_accounts_key_is_refused() {
        let server = FakeServer::default();
        let mut laptop = Archive::in_memory(&State::default());
        laptop.write(&[visit("zen", 0)]).unwrap();
        sync(&server, &mut laptop, &Key::generate()).unwrap();

        let wrong = Key::generate();
        assert!(matches!(check_key(&server, &wrong), Err(SyncError::Crypto(_))));
        let mut desktop = Archive::in_memory(&State::default());
        assert!(matches!(sync(&server, &mut desktop, &wrong), Err(SyncError::Crypto(_))));
        assert!(check_key(&FakeServer::default(), &wrong).is_ok(), "an empty account takes any key");
    }

    #[test]
    fn devices_are_remembered() {
        let server = FakeServer::default();
        let mut laptop = Archive::in_memory(&State::default());
        let me = laptop.device().id.clone();
        server.devices.borrow_mut().extend([
            RemoteDevice { id: me, name: "laptop".into(), last_seen_ms: 1 },
            RemoteDevice { id: "d2".into(), name: "desktop".into(), last_seen_ms: 2 },
        ]);
        sync(&server, &mut laptop, &Key::generate()).unwrap();
        let devices = laptop.remote_devices().unwrap();
        assert_eq!(devices.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["desktop"]);
    }

    #[test]
    fn rows_from_a_newer_habitfocus_are_skipped() {
        let key = Key::generate();
        let batch = Batch {
            rows: vec![
                (1, serde_json::json!({"at": 1, "kind": "from_the_future", "amount": 0, "text": "?"})),
                (2, to_json(&event(2))),
            ],
        };
        let device = Device { id: "d2".into(), name: "desktop".into() };
        let server = FakeServer::default();
        server.upload(&[seal(&key, &device, Table::Events, 0, &batch)]).unwrap();
        let mut laptop = Archive::in_memory(&State::default());
        assert_eq!(sync(&server, &mut laptop, &key).unwrap().received, 1);
    }
}
