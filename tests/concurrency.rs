//! Concurrency (spec §14): reading the source while a second connection
//! writes to it in WAL mode must never produce `database is locked`.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::Fixture;
use remember_mcp::sources::build_adapters;

#[test]
fn reads_while_writer_writes_in_wal() {
    let f: Fixture = common::build();
    let stop = Arc::new(AtomicBool::new(false));

    // Writer: keeps inserting messages and bumping time_updated.
    let writer = {
        let db = f.db();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(&db).expect("writer open");
            conn.busy_timeout(Duration::from_millis(5_000)).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            for i in 0..60 {
                let ts: i64 = 1_800_000_000_000 + i as i64;
                let r = conn.execute(
                    "INSERT INTO message(id, session_id, time_created, time_updated, data)
                     VALUES(?1,'ses_docker_fix',?2,?2,?3)
                     ON CONFLICT(id) DO UPDATE SET time_updated=excluded.time_updated",
                    rusqlite::params![
                        format!("msg_wal_{i}"),
                        ts,
                        format!(r#"{{"role":"user","time":{{"created":{ts}}}}}"#)
                    ],
                );
                assert!(r.is_ok(), "writer failed at {i}: {r:?}");
                conn.execute(
                    "INSERT INTO part(id, message_id, session_id, time_created, time_updated, data)
                     VALUES(?1,?2,'ses_docker_fix',?3,?3,?4)
                     ON CONFLICT(id) DO UPDATE SET time_updated=excluded.time_updated",
                    rusqlite::params![
                        format!("prt_wal_{i}"),
                        format!("msg_wal_{i}"),
                        ts,
                        format!(r#"{{"type":"text","text":"wal write number {i}"}}"#)
                    ],
                )
                .unwrap();
                std::thread::sleep(Duration::from_millis(2));
            }
            stop.store(true, Ordering::Relaxed);
        })
    };

    // Reader: adapters open read-only connections repeatedly while writing runs.
    let adapters = build_adapters(&f.db(), &f.storage());
    let mut reads = 0;
    while !stop.load(Ordering::Relaxed) {
        for a in &adapters {
            let sessions = a
                .list_sessions()
                .expect("list_sessions must not fail with concurrent writer");
            assert!(!sessions.is_empty());
            let loaded = a
                .load_session("ses_docker_fix")
                .expect("load_session must not fail with concurrent writer");
            if a.name() == "sqlite_v1" {
                assert!(loaded.is_some(), "v1 adapter must see the session");
            }
            reads += 1;
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    writer.join().expect("writer thread");

    assert!(reads > 10, "expected repeated reads, got {reads}");

    // and the final state is consistent
    let adapters = build_adapters(&f.db(), &f.storage());
    let rec = adapters[0].load_session("ses_docker_fix").unwrap().unwrap();
    assert!(rec.messages.len() >= 2);
}
