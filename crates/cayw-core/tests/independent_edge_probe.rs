use cayw_core::{CoreEvent, CoreRequest, ProtocolEnvelope, SessionStore};
use rusqlite::{Connection, params};
use serde_json::Value;

#[test]
fn invalid_envelope_id_drops_owned_deep_request_without_aborting() {
    const CHILD: &str = "CAYW_INDEPENDENT_ENVELOPE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "invalid_envelope_id_drops_owned_deep_request_without_aborting",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        println!(
            "child status: {:?}\n{}{}",
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            result.status.success(),
            "ProtocolEnvelope::new rejection aborted while dropping owned deep CoreRequest"
        );
        return;
    }
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let data = (0..25_000).fold(Value::Null, |value, _| Value::Array(vec![value]));
            let request = CoreRequest::AppendHostEvent {
                lease: cayw_core::AttemptLease {
                    session_id: "s".into(),
                    attempt_id: "a".into(),
                    worker_id: "w".into(),
                    fence: 1,
                },
                kind: "event".into(),
                data,
            };
            assert!(ProtocolEnvelope::new("", request).is_err());
            println!("envelope constructor rejection returned normally");
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn append_only_guards_reject_replacing_an_existing_event_key() {
    struct ProbeDirectory(std::path::PathBuf);
    impl Drop for ProbeDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let directory = ProbeDirectory(
        std::env::temp_dir().join(format!("cayw-core-independent-{}", uuid::Uuid::new_v4())),
    );
    std::fs::create_dir(&directory.0).unwrap();
    let path = directory.0.join("replace.sqlite");
    let store = SessionStore::open(&path).unwrap();
    let session = store.create_session("original").unwrap();
    let original = store.events(&session.id, 0, 1000).unwrap();
    let db = Connection::open(&path).unwrap();
    let guards: u32 = db.query_row("SELECT count(*) FROM sqlite_schema WHERE type='trigger' AND tbl_name='core_events' AND name IN ('core_events_no_update','core_events_no_delete')", [], |r| r.get(0)).unwrap();
    assert_eq!(guards, 2);
    assert!(
        db.execute("UPDATE core_events SET record='{}'", [])
            .is_err()
    );
    assert!(db.execute("DELETE FROM core_events", []).is_err());
    let mut replacement = original[0].clone();
    replacement.event = CoreEvent::Host {
        kind: "replacement".into(),
        data: Value::Null,
    };
    let result = db.execute(
        "INSERT OR REPLACE INTO core_events(session_id,cursor,record) VALUES(?1,?2,?3)",
        params![session.id, 1, serde_json::to_string(&replacement).unwrap()],
    );
    println!("{path:?}: original UPDATE/DELETE blocked; key replacement result={result:?}");
    if result.is_ok() {
        let after = store.events(&session.id, 0, 1000).unwrap();
        assert_eq!(after, vec![replacement]);
        assert_ne!(after, original);
        println!(
            "replacement changed original event while both append-only triggers remain installed"
        );
    }
    assert!(
        result.is_err(),
        "append-only trigger pair permitted INSERT OR REPLACE to rewrite cursor 1"
    );
}
