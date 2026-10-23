use super::*;
use std::{
    io::{BufRead, Read, Write},
    os::unix::process::ExitStatusExt,
};

const EVIDENCE_ROOT_ENV: &str = "CAYW_AUTHORITY_CRASH_EVIDENCE_ROOT";

fn write_new(path: &Path, bytes: &[u8]) {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

fn write_json(path: &Path, value: &Value) {
    let mut bytes = serde_json::to_vec_pretty(value).unwrap();
    bytes.push(b'\n');
    write_new(path, &bytes);
}

fn evidence_root() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os(EVIDENCE_ROOT_ENV)?);
    assert!(root.is_absolute(), "evidence root must be absolute");
    let parent = root.parent().unwrap().canonicalize().unwrap();
    let root = parent.join(root.file_name().expect("evidence root needs a name"));
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    assert!(
        !root.starts_with(workspace),
        "evidence must be outside the source workspace"
    );
    fs::create_dir(&root).expect("refuse to replace an existing evidence root");
    let run = root.join(Uuid::new_v4().to_string());
    fs::create_dir(&run).expect("refuse to replace an existing evidence run");
    println!("AUTHORITY_CRASH_EVIDENCE_ROOT={}", run.display());
    Some(run)
}

fn original_bytes(path: &Path) -> Vec<(String, Option<Vec<u8>>)> {
    ["", "-wal", "-shm", "-journal", ".authority"]
        .into_iter()
        .map(|suffix| {
            let name = format!("requests.sqlite{suffix}");
            let mut source = path.as_os_str().to_os_string();
            source.push(suffix);
            let bytes = match fs::read(PathBuf::from(source)) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => panic!("cannot preserve {name}: {error}"),
            };
            (name, bytes)
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn table_rows(statement: &mut rusqlite::Statement<'_>) -> Value {
    use rusqlite::types::ValueRef;
    let columns: Vec<_> = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut cursor = statement.query([]).unwrap();
    let mut rows = Vec::new();
    while let Some(row) = cursor.next().unwrap() {
        let cells: Vec<_> = (0..columns.len())
            .map(|index| match row.get_ref(index).unwrap() {
                ValueRef::Null => json!({"type":"null"}),
                ValueRef::Integer(value) => json!({"type":"integer","value":value}),
                ValueRef::Real(value) => {
                    json!({"type":"real","bits_hex":format!("{:016x}",value.to_bits())})
                }
                ValueRef::Text(value) => json!({"type":"text","bytes_hex":hex(value)}),
                ValueRef::Blob(value) => json!({"type":"blob","bytes_hex":hex(value)}),
            })
            .collect();
        rows.push(Value::Array(cells));
    }
    rows.sort_by_cached_key(Value::to_string);
    json!({"columns":columns,"rows":rows})
}

fn readonly_export(path: &Path) -> Value {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    connection
        .execute_batch("PRAGMA query_only=ON; BEGIN;")
        .unwrap();
    let schema = table_rows(
        &mut connection
            .prepare("SELECT type,name,tbl_name,rootpage,sql FROM sqlite_schema ORDER BY type,name")
            .unwrap(),
    );
    let tables: Vec<String> = connection
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    let tables: Vec<_> = tables
        .into_iter()
        .map(|name| {
            let sql = format!("SELECT * FROM \"{}\"", name.replace('"', "\"\""));
            json!({"name":name,"data":table_rows(&mut connection.prepare(&sql).unwrap())})
        })
        .collect();
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    connection.execute_batch("ROLLBACK").unwrap();
    assert_eq!(integrity, "ok");
    json!({"schema":schema,"tables":tables,"integrity_check":integrity})
}

// All writers are idle here: the repair child has exited or has not started,
// and the parent performs no RequestStore operation until this capture returns.
// Export from a second copy so SQLite cannot change the preserved raw SHM bytes.
fn preserve_snapshot(root: &Path, stage: &str, path: &Path) -> Value {
    let directory = root.join(stage);
    fs::create_dir(&directory).unwrap();
    let raw = directory.join("raw");
    let query = directory.join("readonly-copy");
    fs::create_dir(&raw).unwrap();
    fs::create_dir(&query).unwrap();
    let before = original_bytes(path);
    assert!(before[0].1.is_some(), "the actual SQLite file must exist");
    assert!(
        before[4].1.is_some(),
        "the original authority marker must exist"
    );
    let mut files = Vec::new();
    for (name, bytes) in &before {
        if let Some(bytes) = bytes {
            write_new(&raw.join(name), bytes);
            write_new(&query.join(name), bytes);
            files.push(json!({
                "name":name,"present":true,"bytes":bytes.len(),
                "sha256":format!("{:x}",Sha256::digest(bytes))
            }));
        } else {
            files.push(json!({"name":name,"present":false}));
        }
    }
    assert_eq!(
        original_bytes(path),
        before,
        "source changed during byte capture"
    );
    let exported = readonly_export(&query.join("requests.sqlite"));
    write_json(&directory.join("tables.json"), &exported);
    for (name, bytes) in &before {
        if let Some(bytes) = bytes {
            assert_eq!(fs::read(raw.join(name)).unwrap(), *bytes);
        }
    }
    assert_eq!(
        original_bytes(path),
        before,
        "read-only export changed the source"
    );
    write_json(
        &directory.join("snapshot.json"),
        &json!({
            "stage":stage,"source_database":path,"files":files,
            "source_bytes_unchanged_during_capture":true,
            "raw_bytes_unchanged_during_export":true,
            "export_source":"readonly-copy/requests.sqlite (copied from the same raw byte set)",
            "export_flags":"SQLITE_OPEN_READ_ONLY; PRAGMA query_only=ON; BEGIN"
        }),
    );
    fs::File::open(&raw).unwrap().sync_all().unwrap();
    fs::File::open(&directory).unwrap().sync_all().unwrap();
    exported
}

pub(super) fn crash_window(point: &str) {
    if std::env::var("CAYW_AUTHORITY_CRASH_WINDOW").ok().as_deref() == Some(point) {
        println!("authority-repair-window:{point}");
        std::io::stdout().flush().unwrap();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
        panic!("parent must kill the administration process at its durable window");
    }
}

#[test]
fn repair_crash_child() {
    let Some(path) = std::env::var_os("CAYW_AUTHORITY_CRASH_DB") else {
        return;
    };
    let input: (AuthorityInspection, String, String) =
        serde_json::from_str(&std::env::var("CAYW_AUTHORITY_CRASH_REQUEST").unwrap()).unwrap();
    let request = AuthorityRepairRequest::new(
        input.0,
        input.1,
        HostActor::new("trusted-host").unwrap(),
        input.2,
    )
    .unwrap();
    RequestStore::repair_authority(
        PathBuf::from(path),
        RequestConfig::default(),
        &request,
        owners,
    )
    .unwrap();
}

// Catches releasing grants at a process-loss window, losing a committed repair,
// or letting process-local stopped state prevent exact cross-process completion.
#[test]
fn killed_repair_process_retries_preparation_ready_and_activation_windows() {
    let retained = evidence_root();
    for point in ["prepared", "ready", "activated"] {
        let temp = tempfile::tempdir().unwrap();
        let case = retained.as_ref().map(|root| {
            let case = root.join(point);
            fs::create_dir(&case).unwrap();
            fs::create_dir(case.join("live")).unwrap();
            case
        });
        let path = case.as_ref().map_or_else(
            || temp.path().join("requests.sqlite"),
            |case| case.join("live/requests.sqlite"),
        );
        let store = RequestStore::open(&path, RequestConfig::default()).unwrap();
        let database = Connection::open(&path).unwrap();
        let stop_sql = "CREATE TRIGGER fail_clock BEFORE UPDATE OF last_now_ms ON request_meta BEGIN SELECT RAISE(ABORT,'stop original authority'); END;";
        database.execute_batch(stop_sql).unwrap();
        let original_task = TaskIdentity::new("session", "task", "attempt").unwrap();
        let failure = store.task_state(&original_task).unwrap_err();
        database.execute_batch("DROP TRIGGER fail_clock").unwrap();
        let inspection = RequestStore::inspect_authority(&path, RequestConfig::default()).unwrap();
        let evidence = "original owner stopped and all execution scopes held";
        let request = AuthorityRepairRequest::new(
            inspection.clone(),
            "killed-repair",
            HostActor::new("trusted-host").unwrap(),
            evidence,
        )
        .unwrap();
        let executable = std::env::current_exe().unwrap();
        let arguments = [
            "--exact",
            "store::recovery_tests::repair_crash_child",
            "--nocapture",
        ];
        let child_request =
            serde_json::to_string(&(&inspection, "killed-repair", evidence)).unwrap();
        if let Some(case) = &case {
            write_json(
                &case.join("input.json"),
                &json!({
                    "window":point,"database":path,"parent_pid":std::process::id(),
                    "current_exe":executable,
                    "current_exe_sha256":format!("{:x}",Sha256::digest(fs::read(&executable).unwrap())),
                    "child_argv":arguments,"config":RequestConfig::default(),
                    "stop_trigger_sql":stop_sql,"original_task":original_task,
                    "original_failure":failure.to_string(),"original_inspection":inspection,
                    "repair_request":{
                        "inspection":inspection,"repair_id":"killed-repair",
                        "actor":"trusted-host","evidence":evidence
                    },
                    "child_request_environment_json":child_request,
                    "ownership_guard":"original test guard verifies exactly inspection.tasks",
                    "boundary":match point {
                        "prepared" => "preparation transaction committed, before sidecar rotation",
                        "ready" => "sidecar Ready persisted, before activation transaction commit",
                        "activated" => "activation transaction committed, before local generation publish",
                        _ => unreachable!()
                    }
                }),
            );
            preserve_snapshot(case, "original-stopped", &path);
        }
        let mut child = std::process::Command::new(&executable)
            .args(arguments)
            .env("CAYW_AUTHORITY_CRASH_DB", &path)
            .env("CAYW_AUTHORITY_CRASH_WINDOW", point)
            .env("CAYW_AUTHORITY_CRASH_REQUEST", &child_request)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let errors = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let (send, receive) = std::sync::mpsc::channel();
        let checkpoint = format!("authority-repair-window:{point}\n");
        let expected = checkpoint.clone();
        let reader = std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            let mut output = Vec::new();
            let mut line = Vec::new();
            while reader.read_until(b'\n', &mut line).unwrap() != 0 {
                if line == expected.as_bytes() {
                    let _ = send.send(());
                }
                output.extend_from_slice(&line);
                line.clear();
            }
            output
        });
        let ready = receive.recv_timeout(Duration::from_secs(15));
        // Kill even on a missing gate so the failure cannot leave a child parked
        // on stdin; ready and signal are checked separately after saving output.
        let kill = child.kill();
        let exit = child.wait().unwrap();
        let output = reader.join().unwrap();
        let errors = errors.join().unwrap();
        if let Some(case) = &case {
            write_new(&case.join("child.stdout"), &output);
            write_new(&case.join("child.stderr"), &errors);
            write_json(
                &case.join("child-exit.json"),
                &json!({
                    "window":point,"parent_pid":std::process::id(),"child_pid":child.id(),
                    "current_exe":executable,"argv":arguments,"checkpoint":checkpoint,
                    "checkpoint_observed":ready.is_ok(),
                    "checkpoint_error":ready.as_ref().err().map(ToString::to_string),
                    "kill_succeeded":kill.is_ok(),
                    "kill_error":kill.as_ref().err().map(ToString::to_string),
                    "status":exit.to_string(),"raw_status":exit.into_raw(),
                    "signal":exit.signal(),"code":exit.code(),"success":exit.success()
                }),
            );
            preserve_snapshot(case, "killed-before-retry", &path);
        }
        println!(
            "repair window={point} child={} exit={exit} stdout={} stderr={}",
            child.id(),
            String::from_utf8_lossy(&output),
            String::from_utf8_lossy(&errors)
        );
        assert!(
            ready.is_ok(),
            "child did not reach the durable repair window"
        );
        kill.unwrap();
        assert_eq!(
            exit.signal(),
            Some(9),
            "the checkpoint child must die by SIGKILL"
        );
        assert!(!exit.success());
        if point != "activated" {
            let refused = RequestStore::open(&path, RequestConfig::default()).is_err();
            if let Some(case) = &case {
                write_json(
                    &case.join("ordinary-open.json"),
                    &json!({"refused":refused}),
                );
                preserve_snapshot(case, "after-open-refusal", &path);
            }
            assert!(refused);
        }
        let result =
            RequestStore::repair_authority(&path, RequestConfig::default(), &request, owners)
                .unwrap();
        assert_ne!(result.authority_id, result.previous_authority_id);
        let after_retry = case.as_ref().map(|case| {
            write_json(&case.join("repair-receipt.json"), &json!(result));
            preserve_snapshot(case, "after-exact-retry", &path)
        });
        let repeated =
            RequestStore::repair_authority(&path, RequestConfig::default(), &request, owners)
                .unwrap();
        assert_eq!(repeated, result);
        if let Some(case) = &case {
            write_json(&case.join("repeat-receipt.json"), &json!(repeated));
            let after_repeat = preserve_snapshot(case, "after-repeat", &path);
            assert_eq!(
                Some(after_repeat),
                after_retry,
                "exact retry changed durable rows"
            );
        }
        assert!(RequestStore::open(&path, RequestConfig::default()).is_ok());
        assert_eq!(
            database
                .query_row(
                    "SELECT count(*) FROM request_authority_repairs",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            database
                .query_row(
                    "SELECT count(*) FROM request_authority_activations",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert!(
            store.task_state(&original_task).is_err(),
            "the repaired generation must not revive the original stopped handle"
        );
        if let Some(case) = &case {
            write_json(
                &case.join("case-complete.json"),
                &json!({
                    "window":point,"signal":exit.signal(),"checkpoint_observed":true,
                    "repair_rows":1,"activation_rows":1,"exact_repeat_receipt_equal":true,
                    "exact_repeat_tables_equal":true,"ordinary_open_after_retry":true
                }),
            );
        }
    }
    if let Some(root) = retained {
        write_json(
            &root.join("complete.json"),
            &json!({"windows":["prepared","ready","activated"],"signals":[9,9,9]}),
        );
        fs::File::open(root).unwrap().sync_all().unwrap();
    }
}

struct Owners(Vec<TaskIdentity>);
impl AuthorityRecoveryGuard for Owners {
    fn verified_tasks(&self) -> &[TaskIdentity] {
        &self.0
    }
}
fn owners(inspection: &AuthorityInspection) -> Result<Owners> {
    Ok(Owners(
        inspection
            .tasks
            .iter()
            .map(|task| task.identity.clone())
            .collect(),
    ))
}

// Catches a failure of the identity preflight losing the durable stop that
// Pending previously provided before any SQLite query could fail.
#[test]
fn identity_read_failure_and_torn_marker_retain_the_durable_stop() {
    for fault in ["missing_meta", "torn_marker"] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("requests.sqlite");
        let store = RequestStore::open(&path, RequestConfig::default()).unwrap();
        let database = Connection::open(&path).unwrap();
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(".authority");
        let sidecar = PathBuf::from(sidecar);
        let original = fs::read(&sidecar).unwrap();
        if fault == "missing_meta" {
            database
                .execute_batch("ALTER TABLE request_meta RENAME TO unavailable_meta")
                .unwrap();
        } else {
            fs::write(&sidecar, b"{owned marker interrupted").unwrap();
        }
        assert!(
            store
                .task_state(&TaskIdentity::new("session", "task", "attempt").unwrap())
                .is_err()
        );
        if fault == "missing_meta" {
            assert_ne!(
                fs::read(&sidecar).unwrap(),
                original,
                "failed identity read left the old Ready record usable after process loss"
            );
        } else {
            assert_eq!(
                fs::read(&sidecar).unwrap(),
                b"{owned marker interrupted",
                "failed operation destroyed its original marker evidence"
            );
        }
    }
}

// Builds the exact durable product transaction state at the window between
// preparation COMMIT and sidecar rotation, without exposing a production hook.
#[test]
fn committed_repair_preparation_proves_its_original_marker_identity() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("requests.sqlite");
    let store = RequestStore::open(&path, RequestConfig::default()).unwrap();
    let database = Connection::open(&path).unwrap();
    database.execute_batch("CREATE TRIGGER fail_clock BEFORE UPDATE OF last_now_ms ON request_meta BEGIN SELECT RAISE(ABORT,'stop original authority'); END;").unwrap();
    let original_identity = TaskIdentity::new("session", "task", "attempt").unwrap();
    assert!(store.task_state(&original_identity).is_err());
    database.execute_batch("DROP TRIGGER fail_clock").unwrap();
    let inspection = RequestStore::inspect_authority(&path, RequestConfig::default()).unwrap();
    let actor = HostActor::new("trusted-host").unwrap();
    let request = AuthorityRepairRequest::new(
        inspection.clone(),
        "prepared-repair",
        actor,
        "original process stopped with every scope held",
    )
    .unwrap();
    let authority = AuthorityFence::inspect(&path).unwrap();
    let mut fence = authority.inspect_lock(Duration::from_millis(50)).unwrap();
    let now = inspection.last_now_ms.max(1);
    fence
        .write(&inspection.authority_id, now, Phase::Pending)
        .unwrap();
    let mut connection = recovery_connection(&path, &RequestConfig::default(), true).unwrap();
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    tx.execute_batch(REPAIR_SCHEMA).unwrap();
    tx.execute("UPDATE request_meta SET authority_id='repaired-authority',authority_stopped=1,last_now_ms=?1 WHERE singleton=1", [sql_integer(now).unwrap()]).unwrap();
    let receipt = AuthorityRepairReceipt {
        repair_id: request.repair_id.clone(),
        previous_authority_id: inspection.authority_id.clone(),
        authority_id: "repaired-authority".into(),
        inspection_id: inspection.inspection_id.clone(),
    };
    let repair = RepairData {
        inspection: inspection.clone(),
        actor: request.actor.as_str().into(),
        evidence: request.evidence.clone(),
        receipt: receipt.clone(),
        prepared_digest: facts_digest(&tx).unwrap(),
        at_ms: now,
    };
    tx.execute(
        "INSERT INTO request_authority_repairs VALUES(?1,?2,?3,?4)",
        params![
            request.repair_id,
            inspection.authority_id,
            receipt.authority_id,
            serde_json::to_vec(&repair).unwrap()
        ],
    )
    .unwrap();
    tx.commit().unwrap();
    drop(fence);
    // An old marker with a known prepared transaction is evidence of the
    // interrupted rotation, not permission to discard either identity.
    let pending = RequestStore::inspect_authority(&path, RequestConfig::default()).unwrap();
    assert!(pending.stopped);
    assert_eq!(pending.authority_id, "repaired-authority");
    assert_eq!(
        RequestStore::repair_authority(&path, RequestConfig::default(), &request, owners).unwrap(),
        receipt
    );
    assert!(RequestStore::open(&path, RequestConfig::default()).is_ok());
}
