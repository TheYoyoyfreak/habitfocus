//! Syncing the book library (`books.db`) through the same account: its tables
//! are logs of this device next to the archive's, sealed the same way. Rows
//! carry their `rev` where the archive's carry their rowid; arriving rows are
//! merged last write wins (`habit_books::sync`).
//!
//! Bookkeeping in `books.db`'s settings:
//! - `sync:push:<tag>` = `<next idx>:<last rev sent>`
//! - `sync:pull:<device>:<tag>` = the next idx to fetch

use super::api::{Api, ApiError};
use super::crypto::Key;
use super::run::{open, seal, Batch, Summary, SyncError, BATCH_BYTES, BATCH_ROWS, PAGE};
use crate::archive::Device;
use habit_books::sync::SyncTable;
use habit_books::Db;

fn books_err(e: anyhow::Error) -> SyncError {
    SyncError::Books(format!("{e:#}"))
}

pub fn sync(api: &dyn Api, db: &Db, key: &Key, device: &Device) -> Result<Summary, SyncError> {
    let status = api.status()?;
    let mut summary = Summary::default();

    for table in SyncTable::ALL {
        let tag = table.tag();
        let key_name = format!("push:{tag}");
        let saved = db.sync_value(&key_name).map_err(books_err)?.and_then(|v| {
            let (next, cursor) = v.split_once(':')?;
            Some((next.parse::<u64>().ok()?, cursor.parse::<i64>().ok()?))
        });
        let (mut next, mut cursor) = saved.unwrap_or((0, 0));
        // Like the archive: the server's end of this device's log decides.
        let server_next = status.tail(&device.id, tag).map_or(0, |tail| tail + 1);
        if server_next != next {
            (next, cursor) = match server_next {
                0 => (0, 0),
                _ => {
                    let last = api.next(&device.id, tag, server_next - 1, 1)?;
                    let last = last.first().ok_or_else(|| ApiError::Other("the server lost a record".into()))?;
                    let sent = open(key, last)?.rows.iter().map(|(rev, _)| *rev).max().unwrap_or(0);
                    (server_next, sent)
                }
            };
            db.set_sync_value(&key_name, &format!("{next}:{cursor}")).map_err(books_err)?;
        }
        loop {
            let rows = db.local_changes(table, cursor, BATCH_ROWS).map_err(books_err)?;
            let Some((first_rev, first)) = rows.first() else { break };
            let mut bytes = first.to_string().len();
            let mut batch = Batch { rows: vec![(*first_rev, first.clone())] };
            for (rev, row) in &rows[1..] {
                bytes += row.to_string().len();
                if bytes > BATCH_BYTES {
                    break;
                }
                batch.rows.push((*rev, row.clone()));
            }
            api.upload(&[seal(key, device, tag, next, &batch)])?;
            cursor = batch.rows.last().expect("not empty").0;
            next += 1;
            summary.sent += batch.rows.len();
            db.set_sync_value(&key_name, &format!("{next}:{cursor}")).map_err(books_err)?;
        }
    }

    for (host, tags) in &status.hosts {
        if *host == device.id {
            continue;
        }
        for (tag, &tail) in tags {
            let Some(table) = SyncTable::of_tag(tag) else { continue };
            let key_name = format!("pull:{host}:{tag}");
            let mut start: u64 = db.sync_value(&key_name).map_err(books_err)?.and_then(|v| v.parse().ok()).unwrap_or(0);
            while start <= tail {
                let records = api.next(host, tag, start, PAGE)?;
                if records.is_empty() {
                    break;
                }
                for record in &records {
                    let rows: Vec<serde_json::Value> = open(key, record)?.rows.into_iter().map(|(_, row)| row).collect();
                    summary.received += db.merge_remote(table, &rows).map_err(books_err)?;
                    start = record.idx + 1;
                    db.set_sync_value(&key_name, &start.to_string()).map_err(books_err)?;
                }
            }
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::run::tests::FakeServer;
    use habit_books::{Book, Status};

    fn device(id: &str, name: &str) -> (Device, Db) {
        let dir = std::env::temp_dir().join(format!("habitd-books-sync-{}", uuid::Uuid::now_v7()));
        let db = Db::open(&dir.join("books.db"), id, name).unwrap();
        (Device { id: id.into(), name: name.into() }, db)
    }

    #[test]
    fn libraries_meet_through_the_server() {
        let server = FakeServer::default();
        let key = Key::generate();
        let (laptop, on_laptop) = device("laptop-id", "laptop");
        let (phone, on_phone) = device("phone-id", "phone");

        let mut dune = Book::new("Dune");
        dune.status = Status::Reading;
        on_laptop.save_book(&mut dune).unwrap();
        on_laptop.record_habit_session("read", 1_000, 61_000, 60_000).unwrap();

        // book, reading and session out; nothing in yet
        assert_eq!(sync(&server, &on_laptop, &key, &laptop).unwrap(), Summary { sent: 3, received: 0 });
        assert_eq!(sync(&server, &on_phone, &key, &phone).unwrap(), Summary { sent: 0, received: 3 });
        assert_eq!(on_phone.list_sessions(None).unwrap()[0].device, "laptop");
        assert_eq!(sync(&server, &on_laptop, &key, &laptop).unwrap(), Summary::default(), "nothing new");

        // An edit on the phone comes back to the laptop.
        on_phone.set_last_page(&dune.id, 42).unwrap();
        assert_eq!(sync(&server, &on_phone, &key, &phone).unwrap().sent, 1);
        assert_eq!(sync(&server, &on_laptop, &key, &laptop).unwrap().received, 1);
        assert_eq!(on_laptop.current_page(&dune.id).unwrap(), Some(42));

        // The server never sees a title.
        let stored = serde_json::to_string(&*server.logs.borrow().values().collect::<Vec<_>>()).unwrap();
        assert!(!stored.contains("Dune"));
        // Old habitfocus versions skip these tags; new ones keep them apart from the archive.
        assert!(server.logs.borrow().keys().any(|(_, tag)| tag == "reading_sessions"));
    }

    #[test]
    fn signing_in_again_continues_the_log() {
        let server = FakeServer::default();
        let key = Key::generate();
        let (laptop, db) = device("laptop-id", "laptop");
        db.save_book(&mut Book::new("Dune")).unwrap();
        sync(&server, &db, &key, &laptop).unwrap();
        db.clear_sync_state().unwrap();
        db.save_book(&mut Book::new("Emma")).unwrap();
        assert_eq!(sync(&server, &db, &key, &laptop).unwrap().sent, 1, "only the new book");
    }
}
