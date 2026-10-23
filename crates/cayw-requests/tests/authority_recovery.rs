use cayw_ai::CancellationToken;
use cayw_codemode::CapabilityError;
use cayw_host::{AuditSink, HostConfig, PermissionGate};
use cayw_requests::*;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

struct Clock1000;
impl Clock for Clock1000 {
    fn now_ms(&self) -> Result<u64> {
        Ok(1000)
    }
}
struct Allow;
#[async_trait::async_trait]
impl PermissionGate for Allow {
    async fn authorize(
        &self,
        _: &str,
        _: &Value,
        _: CancellationToken,
    ) -> std::result::Result<(), CapabilityError> {
        Ok(())
    }
}
struct Audit;
#[async_trait::async_trait]
impl AuditSink for Audit {
    async fn started(&self, _: &str, _: &Value) -> std::result::Result<String, CapabilityError> {
        Ok("fixture-call".into())
    }
    async fn settled(
        &self,
        _: &str,
        _: &std::result::Result<Value, CapabilityError>,
    ) -> std::result::Result<(), CapabilityError> {
        Ok(())
    }
    fn dropped(&self, _: &str) -> std::result::Result<(), CapabilityError> {
        Ok(())
    }
}
struct StoppedTasks(Vec<TaskIdentity>);
impl AuthorityRecoveryGuard for StoppedTasks {
    fn verified_tasks(&self) -> &[TaskIdentity] {
        &self.0
    }
}
fn stopped(inspection: &AuthorityInspection) -> Result<StoppedTasks> {
    Ok(StoppedTasks(
        inspection
            .tasks
            .iter()
            .map(|task| task.identity.clone())
            .collect(),
    ))
}
fn actor() -> HostActor {
    HostActor::new("trusted-recovery-host").unwrap()
}
fn identity() -> TaskIdentity {
    TaskIdentity::new("original-session", "original-task", "original-attempt").unwrap()
}
fn arguments() -> Value {
    json!({"path":"result.txt","content":"real selected effect\n"})
}
fn catalog(root: &Path) -> OperationCatalog {
    RequestHostConfig {
        root: root.into(),
        host: HostConfig::default(),
        permission_policy_id: "recovery-policy".into(),
        operations: vec![OperationScope::exact(
            "write",
            "fs.write",
            arguments(),
            "recovery-policy",
        )],
        gate: Arc::new(Allow),
        audit: Arc::new(Audit),
    }
    .catalog()
    .unwrap()
}
fn open(path: &Path) -> RequestStore {
    RequestStore::open_with_clock(path, RequestConfig::default(), Arc::new(Clock1000)).unwrap()
}
fn input() -> RequestInput {
    RequestInput {
        purpose: "save the selected task result".into(),
        gap: "the selected file needs to be saved".into(),
        operation_id: "write".into(),
        capability: "fs.write".into(),
        arguments: arguments(),
    }
}
fn granted(path: &Path) -> (RequestStore, OperationCatalog, AuthorizationReceipt) {
    let store = open(path);
    let catalog = catalog(path.parent().unwrap());
    store
        .start_task(&identity(), TaskIntent::General, &catalog, &actor())
        .unwrap();
    store.request(&identity(), &catalog, input()).unwrap();
    let request = store.request(&identity(), &catalog, input()).unwrap();
    let ticket = store.evaluate(&identity(), &request.id, &actor()).unwrap();
    store
        .approve(&ticket, &catalog, 100, "the host checked this exact write")
        .unwrap();
    let receipt = store
        .admit(&identity(), &catalog, "fs.write", &arguments())
        .unwrap();
    (store, catalog, receipt)
}
fn fail_commit(path: &Path, store: &RequestStore) {
    let database = rusqlite::Connection::open(path).unwrap();
    database.execute_batch("CREATE TABLE broken_commit(id INTEGER REFERENCES request_meta(singleton) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER fail_observation AFTER UPDATE OF last_now_ms ON request_meta BEGIN INSERT INTO broken_commit VALUES(2); END;").unwrap();
    assert!(matches!(
        store.task_state(&identity()),
        Err(RequestError::Database(_))
    ));
    database
        .execute_batch("DROP TRIGGER fail_observation")
        .unwrap();
    assert!(matches!(
        RequestStore::open(path, RequestConfig::default()),
        Err(RequestError::AuthorityStopped)
    ));
}
fn repair_request(path: &Path, repair_id: &str) -> AuthorityRepairRequest {
    let inspection = RequestStore::inspect_authority(path, RequestConfig::default()).unwrap();
    AuthorityRepairRequest::new(inspection, repair_id, actor(), "original Worker stopped; all Task locks and Cells ownership held; Device has no pending processes").unwrap()
}
fn events(database: &rusqlite::Connection) -> Vec<(i64, Vec<u8>)> {
    database
        .prepare("SELECT sequence,payload FROM request_events ORDER BY sequence")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(std::result::Result::unwrap)
        .collect()
}

// Catches resetting the original authority, losing original evidence, retaining
// grants/tickets, or allowing an old handle to poison the repaired generation.
#[test]
fn explicit_repair_preserves_original_history_and_fences_every_old_grant() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("requests.sqlite");
    let (store, catalog, receipt) = granted(&path);
    let peer = open(&path);
    let pending =
        TaskIdentity::new("original-session", "evaluating-task", "evaluating-attempt").unwrap();
    store
        .start_task(&pending, TaskIntent::General, &catalog, &actor())
        .unwrap();
    store.request(&pending, &catalog, input()).unwrap();
    let next = store.request(&pending, &catalog, input()).unwrap();
    let ticket = store.evaluate(&pending, &next.id, &actor()).unwrap();
    store.check_receipt(&receipt, &catalog).unwrap();
    let database = rusqlite::Connection::open(&path).unwrap();
    let original_events = events(&database);
    fail_commit(&path, &store);
    let request = repair_request(&path, "repair-original");
    let repaired =
        RequestStore::repair_authority(&path, RequestConfig::default(), &request, stopped).unwrap();
    assert_ne!(repaired.authority_id, repaired.previous_authority_id);
    let current = open(&path);
    assert!(matches!(
        current.task_state(&identity()).unwrap(),
        TaskState::Closing { .. }
    ));
    assert!(current.check_receipt(&receipt, &catalog).is_err());
    assert!(
        current
            .approve(&ticket, &catalog, 100, "old decision arrived late")
            .is_err()
    );
    assert!(store.check_receipt(&receipt, &catalog).is_err());
    assert!(peer.check_receipt(&receipt, &catalog).is_err());
    assert!(
        RequestStore::open(&path, RequestConfig::default()).is_ok(),
        "old peer poisoned the new authority"
    );
    assert_eq!(
        &events(&database)[..original_events.len()],
        original_events.as_slice()
    );
    let before_repeat = events(&database);
    assert_eq!(
        RequestStore::repair_authority(&path, RequestConfig::default(), &request, stopped).unwrap(),
        repaired
    );
    assert_eq!(events(&database), before_repeat);
    let old_identity = AuthorityRepairRequest::new(
        request.inspection.clone(),
        "different-repair",
        actor(),
        "changed identity",
    )
    .unwrap();
    assert!(
        RequestStore::repair_authority(&path, RequestConfig::default(), &old_identity, stopped)
            .is_err()
    );
    let altered = AuthorityRepairRequest::new(
        request.inspection.clone(),
        "repair-original",
        actor(),
        "different evidence must not match the old repair",
    )
    .unwrap();
    assert!(
        RequestStore::repair_authority(&path, RequestConfig::default(), &altered, stopped).is_err()
    );
    let next_task = TaskIdentity::new("original-session", "fresh-task", "fresh-attempt").unwrap();
    current
        .start_task(&next_task, TaskIntent::General, &catalog, &actor())
        .unwrap();
    assert_eq!(current.task_state(&next_task).unwrap(), TaskState::Active);
}

// Catches trusting a partial owner proof or a snapshot copied from another file.
#[test]
fn repair_requires_all_original_task_owners_and_the_same_storage() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("requests.sqlite");
    let (store, _, _) = granted(&path);
    fail_commit(&path, &store);
    let request = repair_request(&path, "repair-owner");
    let database = rusqlite::Connection::open(&path).unwrap();
    let before = events(&database);
    assert!(
        RequestStore::repair_authority(&path, RequestConfig::default(), &request, |_| Ok(
            StoppedTasks(vec![])
        ))
        .is_err()
    );
    assert_eq!(events(&database), before);
    let other = temp.path().join("other.sqlite");
    let (other_store, _, _) = granted(&other);
    fail_commit(&other, &other_store);
    assert!(
        RequestStore::repair_authority(&other, RequestConfig::default(), &request, stopped)
            .is_err()
    );
    assert!(matches!(
        RequestStore::open(&path, RequestConfig::default()),
        Err(RequestError::AuthorityStopped)
    ));
    RequestStore::repair_authority(&path, RequestConfig::default(), &request, stopped).unwrap();
}

// Catches releasing admission after a real repair COMMIT failure, and catches
// treating an old successful repair as permission to erase a later failure.
#[test]
fn repair_commit_failure_blocks_admission_and_exact_retry_completes_once() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("requests.sqlite");
    let (store, catalog, receipt) = granted(&path);
    fail_commit(&path, &store);
    let request = repair_request(&path, "repair-commit");
    let database = rusqlite::Connection::open(&path).unwrap();
    database.execute_batch("CREATE TRIGGER fail_repair_commit AFTER UPDATE OF authority_id ON request_meta BEGIN INSERT INTO broken_commit VALUES(2); END;").unwrap();
    let before = events(&database);
    assert!(matches!(
        RequestStore::repair_authority(&path, RequestConfig::default(), &request, stopped),
        Err(RequestError::Database(_))
    ));
    assert_eq!(events(&database), before);
    assert!(matches!(
        RequestStore::open(&path, RequestConfig::default()),
        Err(RequestError::AuthorityStopped)
    ));
    assert!(store.check_receipt(&receipt, &catalog).is_err());
    database
        .execute_batch("DROP TRIGGER fail_repair_commit")
        .unwrap();
    RequestStore::repair_authority(&path, RequestConfig::default(), &request, stopped).unwrap();
    let current = open(&path);
    fail_commit_again(&path, &current);
    assert!(
        RequestStore::repair_authority(&path, RequestConfig::default(), &request, stopped).is_err()
    );
    assert!(RequestStore::open(&path, RequestConfig::default()).is_err());
}
fn fail_commit_again(path: &Path, store: &RequestStore) {
    let database = rusqlite::Connection::open(path).unwrap();
    database.execute_batch("CREATE TRIGGER fail_observation AFTER UPDATE OF last_now_ms ON request_meta BEGIN INSERT INTO broken_commit VALUES(2); END;").unwrap();
    assert!(store.task_state(&identity()).is_err());
    database
        .execute_batch("DROP TRIGGER fail_observation")
        .unwrap();
}

// Catches a successful projection COMMIT followed by a failed activation COMMIT
// being mistaken for a finished repair or requiring destructive store replacement.
#[test]
fn activation_commit_failure_retains_prepared_repair_and_retries_exactly() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("requests.sqlite");
    let (store, _, _) = granted(&path);
    fail_commit(&path, &store);
    let request = repair_request(&path, "repair-activate");
    let database = rusqlite::Connection::open(&path).unwrap();
    // The table exists before the first administrative recovery opens it.
    database.execute_batch("CREATE TRIGGER fail_activation AFTER UPDATE OF authority_stopped ON request_meta WHEN NEW.authority_stopped=0 AND OLD.authority_stopped=1 BEGIN INSERT INTO broken_commit VALUES(2); END;").unwrap();
    assert!(matches!(
        RequestStore::repair_authority(&path, RequestConfig::default(), &request, stopped),
        Err(RequestError::Database(_))
    ));
    let prepared_events = events(&database);
    assert!(RequestStore::open(&path, RequestConfig::default()).is_err());
    database
        .execute_batch("DROP TRIGGER fail_activation")
        .unwrap();
    let result =
        RequestStore::repair_authority(&path, RequestConfig::default(), &request, stopped).unwrap();
    assert_eq!(events(&database), prepared_events);
    assert_ne!(result.authority_id, result.previous_authority_id);
    assert!(RequestStore::open(&path, RequestConfig::default()).is_ok());
}

#[test]
fn old_peer_process_child() {
    let Some(path) = std::env::var_os("CAYW_AUTHORITY_OLD_PEER_DB") else {
        return;
    };
    use std::io::Write;
    let store = open(Path::new(&path));
    println!("old-authority-peer-ready");
    std::io::stdout().flush().unwrap();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    assert!(matches!(
        store.task_state(&identity()),
        Err(RequestError::AuthorityStopped)
    ));
}

// Catches an independently opened old process overwriting the new Ready marker
// with its stale authority ID when it discovers the repaired generation.
#[test]
fn old_peer_process_cannot_stop_the_repaired_generation() {
    use std::io::{BufRead, Write};
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("requests.sqlite");
    let (store, _, _) = granted(&path);
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "old_peer_process_child", "--nocapture"])
        .env("CAYW_AUTHORITY_OLD_PEER_DB", &path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    loop {
        let count = stdout.read_line(&mut ready).unwrap();
        assert_ne!(
            count, 0,
            "peer exited before opening its original authority"
        );
        if ready.ends_with("old-authority-peer-ready\n") {
            break;
        }
    }
    fail_commit(&path, &store);
    let request = repair_request(&path, "peer-repair");
    RequestStore::repair_authority(&path, RequestConfig::default(), &request, stopped).unwrap();
    let mut marker_path = path.as_os_str().to_os_string();
    marker_path.push(".authority");
    let marker_path = std::path::PathBuf::from(marker_path);
    let ready_marker = std::fs::read(&marker_path).unwrap();
    std::fs::write(&marker_path, b"{new authority interrupted marker").unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"check old authority\n")
        .unwrap();
    let exit = child.wait().unwrap();
    assert!(exit.success());
    assert_eq!(
        std::fs::read(&marker_path).unwrap(),
        b"{new authority interrupted marker",
        "old peer replaced the new authority's original fault evidence"
    );
    let database = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        database
            .query_row("SELECT authority_stopped FROM request_meta", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        0,
        "old peer stopped the new database generation"
    );
    // Only restore the fixture's known original Ready bytes after proving that
    // the old process did not write either new-generation record.
    std::fs::write(&marker_path, ready_marker).unwrap();
    assert!(matches!(
        open(&path).task_state(&identity()).unwrap(),
        TaskState::Closing { .. }
    ));
}

// Catches an ordinary rejected reopen destroying a torn marker's evidence or
// making explicit recovery impossible despite retaining the original SQLite ID.
#[test]
fn torn_marker_is_retained_for_trusted_inspection_after_reopen_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("requests.sqlite");
    let (store, catalog, receipt) = granted(&path);
    let mut marker = path.as_os_str().to_os_string();
    marker.push(".authority");
    let marker = std::path::PathBuf::from(marker);
    std::fs::write(&marker, b"{interrupted authority record").unwrap();
    assert!(RequestStore::open(&path, RequestConfig::default()).is_err());
    let request = repair_request(&path, "repair-torn-marker");
    assert_eq!(request.inspection.marker, b"{interrupted authority record");
    RequestStore::repair_authority(&path, RequestConfig::default(), &request, stopped).unwrap();
    assert!(store.check_receipt(&receipt, &catalog).is_err());
    assert!(open(&path).check_receipt(&receipt, &catalog).is_err());
}

// Catches unreserved administrative records silently escaping max_state_bytes.
#[test]
fn repair_respects_the_original_persistent_storage_budget() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("requests.sqlite");
    let (store, _, _) = granted(&path);
    fail_commit(&path, &store);
    let database = rusqlite::Connection::open(&path).unwrap();
    database
        .execute(
            "UPDATE request_meta SET reserved_bytes=?1 WHERE singleton=1",
            [i64::try_from(RequestConfig::default().max_state_bytes).unwrap()],
        )
        .unwrap();
    let request = repair_request(&path, "budget-repair");
    assert!(matches!(
        RequestStore::repair_authority(&path, RequestConfig::default(), &request, stopped),
        Err(RequestError::Budget)
    ));
    assert!(RequestStore::open(&path, RequestConfig::default()).is_err());
}
