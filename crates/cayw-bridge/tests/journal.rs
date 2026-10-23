#[path = "../examples/support/mod.rs"]
mod support;
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_bridge::*;
use cayw_codemode::CapabilityError;
use serde_json::{Value, json};
use std::{
    io::Write,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Allow;
#[async_trait]
impl RegistryPolicy for Allow {
    fn visible(&self, _: Visibility, _: &Provenance, _: &str) -> bool {
        true
    }
    async fn authorize(
        &self,
        _: &AccessRequest,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
}
fn identity() -> SourceIdentity {
    SourceIdentity {
        id: "source".into(),
        version: "one".into(),
        origin: "fixture:journal".into(),
    }
}
fn manifest() -> Manifest {
    Manifest {
        tools: vec![ToolSpec {
            name: "echo".into(),
            description: "echo".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            output_schema: None,
        }],
        ..Manifest::default()
    }
}
fn context(sequence: u64) -> InvocationContext {
    InvocationContext {
        session_id: "session-one".into(),
        task_id: "task-two".into(),
        attempt_id: "attempt-three".into(),
        execution_id: "execution-four".into(),
        sequence,
    }
}
struct Handle {
    calls: Arc<AtomicUsize>,
    path: PathBuf,
    wait: bool,
    failure: bool,
}
#[async_trait]
impl SourceHandle for Handle {
    fn ownership_key(&self) -> &str {
        "journal-physical-connection"
    }
    async fn request(
        &self,
        request: SourceRequest,
        _: CancellationToken,
    ) -> Result<SourceReply, CapabilityError> {
        // No effect is allowed until the complete bridge identity is fsync'd.
        let bytes = std::fs::read_to_string(&self.path).unwrap();
        let started: Value = serde_json::from_str(bytes.lines().last().unwrap()).unwrap();
        assert_eq!(started["kind"], "started");
        assert_eq!(
            started["record"]["key"],
            serde_json::to_value(&request.key).unwrap()
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.wait {
            std::future::pending::<()>().await;
        }
        if self.failure {
            return Err(CapabilityError::new("known rejected operation"));
        }
        Ok(SourceReply {
            key: request.key,
            value: json!({"executed":true}),
        })
    }
    fn disconnect(&self) {}
}
struct Journal {
    path: PathBuf,
    fail_start: bool,
    fail_settle: bool,
    fail_drop: bool,
    cancel: Option<CancellationToken>,
    block_start: bool,
    block_settle: bool,
    starting: AtomicBool,
    settling: AtomicBool,
    delay_start: bool,
    delay_settle: bool,
    on_start: Mutex<Option<Box<dyn Fn() + Send>>>,
}
impl Journal {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            fail_start: false,
            fail_settle: false,
            fail_drop: false,
            cancel: None,
            block_start: false,
            block_settle: false,
            starting: AtomicBool::new(false),
            settling: AtomicBool::new(false),
            delay_start: false,
            delay_settle: false,
            on_start: Mutex::new(None),
        }
    }
    fn record(&self, row: Value) -> Result<(), CapabilityError> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| CapabilityError::new(e.to_string()))?;
        serde_json::to_writer(&mut file, &row).unwrap();
        file.write_all(b"\n").unwrap();
        file.sync_all().unwrap();
        Ok(())
    }
    fn rows(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.path)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
}
#[async_trait]
impl DispatchJournal for Journal {
    async fn started(&self, record: &DispatchRecord) -> Result<(), CapabilityError> {
        self.starting.store(true, Ordering::SeqCst);
        self.record(json!({"kind":"started","record":record}))?;
        if let Some(callback) = self.on_start.lock().unwrap().take() {
            callback();
        }
        if self.block_start {
            std::future::pending::<()>().await;
        }
        if self.delay_start {
            std::thread::sleep(Duration::from_millis(300));
        }
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        if self.fail_start {
            Err(CapabilityError::unknown("journal prepare failed"))
        } else {
            Ok(())
        }
    }
    async fn settled(
        &self,
        record: &DispatchRecord,
        outcome: &DispatchOutcome,
    ) -> Result<(), CapabilityError> {
        self.settling.store(true, Ordering::SeqCst);
        if self.block_settle {
            std::future::pending::<()>().await;
        }
        if self.fail_settle {
            return Err(CapabilityError::new("journal settle failed"));
        }
        if self.delay_settle {
            std::thread::sleep(Duration::from_millis(300));
        }
        self.record(json!({"kind":"settled","record":record,"outcome":outcome}))
    }
    fn dropped(
        &self,
        record: &DispatchRecord,
        state: DispatchDropState,
    ) -> Result<(), CapabilityError> {
        if self.fail_drop {
            return Err(CapabilityError::new("synchronous durable drop failed"));
        }
        self.record(json!({"kind":"dropped","record":record,"outcome":{"state":state}}))
    }
}

#[tokio::test]
async fn synchronous_settlement_cannot_return_success_after_absolute_deadline() {
    let (_workspace, registry, calls, path) = setup(false, false, Duration::from_millis(200));
    let mut journal = Journal::new(path);
    journal.delay_settle = true;
    let journal = Arc::new(journal);
    let result = invoke(&registry, journal.clone(), 1, CancellationToken::new()).await;
    assert!(
        result.is_err(),
        "synchronous terminal callback consumed deadline but success escaped"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        journal.rows().last().unwrap()["outcome"]["state"],
        "succeeded"
    );
    assert!(!registry.snapshot("source").unwrap().outcome_unknown);
}

#[tokio::test]
async fn failed_drop_hook_is_exposed_and_fences_subsequent_dispatch() {
    let (_workspace, registry, calls, path) = setup(true, false, Duration::from_secs(5));
    let mut journal = Journal::new(path);
    journal.fail_drop = true;
    let journal = Arc::new(journal);
    let mut pending = Box::pin(invoke(&registry, journal, 1, CancellationToken::new()));
    tokio::select! {result=&mut pending=>panic!("unexpected {result:?}"),_=async {while calls.load(Ordering::SeqCst)==0 {tokio::task::yield_now().await;}}=>{}}
    drop(pending);
    assert!(
        registry
            .journal_failure()
            .unwrap()
            .contains("synchronous durable drop failed")
    );
    assert!(registry.snapshot("source").unwrap().outcome_unknown);
    let second = Arc::new(Journal::new(_workspace.0.join("second.jsonl")));
    assert!(
        invoke(&registry, second.clone(), 2, CancellationToken::new())
            .await
            .is_err()
    );
    assert!(!second.path.exists());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn journal_preparation_consumes_source_operation_budget_even_without_dispatch() {
    let workspace = support::Workspace::new();
    let path = workspace.0.join("dispatch.jsonl");
    let calls = Arc::new(AtomicUsize::new(0));
    let registry = Registry::new(
        Limits {
            max_source_operations: 1,
            ..Limits::default()
        },
        Arc::new(Allow),
    )
    .unwrap();
    registry
        .register(
            identity(),
            manifest(),
            Box::new(Handle {
                calls: calls.clone(),
                path: path.clone(),
                wait: false,
                failure: false,
            }),
        )
        .unwrap();
    let mut first = Journal::new(path.clone());
    first.fail_start = true;
    assert!(
        invoke(&registry, Arc::new(first), 1, CancellationToken::new())
            .await
            .is_err()
    );
    assert!(
        invoke(
            &registry,
            Arc::new(Journal::new(path)),
            2,
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(registry.snapshot("source").unwrap().operations, 1);
}

fn setup(
    wait: bool,
    failure: bool,
    timeout: Duration,
) -> (support::Workspace, Registry, Arc<AtomicUsize>, PathBuf) {
    let workspace = support::Workspace::new();
    let path = workspace.0.join("dispatch.jsonl");
    let calls = Arc::new(AtomicUsize::new(0));
    let registry = Registry::new(
        Limits {
            operation_timeout: timeout,
            ..Limits::default()
        },
        Arc::new(Allow),
    )
    .unwrap();
    registry
        .register(
            identity(),
            manifest(),
            Box::new(Handle {
                calls: calls.clone(),
                path: path.clone(),
                wait,
                failure,
            }),
        )
        .unwrap();
    (workspace, registry, calls, path)
}
async fn invoke(
    registry: &Registry,
    journal: Arc<Journal>,
    sequence: u64,
    cancel: CancellationToken,
) -> Result<Value, CapabilityError> {
    registry
        .consumer()
        .scoped("mcp.call", context(sequence), journal)
        .unwrap()
        .call(
            "mcp.call",
            json!({"server":"source","tool":"echo","arguments":{}}),
            cancel,
        )
        .await
}

#[tokio::test]
async fn actual_local_child_dispatch_and_drop_keep_fsynced_runtime_mapping() {
    let workspace = support::Workspace::new();
    let stats = Arc::new(support::Stats::default());
    let client = support::LocalClient::spawn(
        std::path::Path::new(env!("CARGO_BIN_EXE_cayw-bridge-local-fixture")),
        &workspace.0,
        stats.clone(),
    )
    .await
    .unwrap();
    let registry = Registry::new(Limits::default(), Arc::new(Allow)).unwrap();
    registry
        .register(
            support::identity("durable-fixture-2"),
            client.manifest().await.unwrap(),
            Box::new(client.handle()),
        )
        .unwrap();
    let journal = Arc::new(Journal::new(workspace.0.join("real-dispatch.jsonl")));
    let bridge = registry.consumer();
    let binding = bridge.binding_snapshot();
    let scope = bridge
        .scoped("mcp.call", context(101), journal.clone())
        .unwrap();
    let result = scope
        .call(
            "mcp.call",
            json!({"server":"local","tool":"add","arguments":{"a":2,"b":3}}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result["value"]["sum"], 5);
    let scope = bridge
        .scoped("skills.read", context(102), journal.clone())
        .unwrap();
    assert_eq!(
        scope
            .call(
                "skills.read",
                json!({"name":"local/addition"}),
                CancellationToken::new()
            )
            .await
            .unwrap()["value"],
        support::SKILL
    );
    let scope = bridge
        .scoped("mcp.call", context(103), journal.clone())
        .unwrap();
    let mut pending = scope.call(
        "mcp.call",
        json!({"server":"local","tool":"wait","arguments":{}}),
        CancellationToken::new(),
    );
    tokio::select! {result=&mut pending=>panic!("unexpected {result:?}"),_=tokio::time::timeout(Duration::from_secs(5),async {while stats.progress.load(Ordering::SeqCst)==0 {tokio::task::yield_now().await;}})=>{}}
    drop(pending);
    let snapshot = registry.snapshot("local").unwrap();
    assert!(snapshot.outcome_unknown);
    assert_eq!(snapshot.unresolved.len(), 1);
    let rows = journal.rows();
    assert_eq!(rows.len(), 6);
    assert_eq!(rows[0]["record"]["key"], result["request"]);
    assert_eq!(
        rows[0]["record"]["key"]["source"],
        serde_json::to_value(&binding.sources["local"]).unwrap()
    );
    assert_eq!(rows[5]["outcome"]["state"], "unknown");
    assert_eq!(rows[5]["record"]["context"]["sequence"], 103);
    assert_eq!(
        rows[5]["record"]["key"],
        serde_json::to_value(&snapshot.unresolved[0].key).unwrap()
    );
    assert_eq!(rows[3]["outcome"]["state"], "succeeded");
    assert_eq!(rows[3]["record"]["key"]["operation"], "read_skill");
    assert_eq!(
        rows[3]["record"]["key"]["source"]["source"]["version"],
        "durable-fixture-2"
    );
    if let Some(path) = std::env::var_os("CAYW_BRIDGE_JOURNAL_EVIDENCE") {
        std::fs::copy(&journal.path, path).unwrap();
    }
    registry.disconnect("local").unwrap();
    client.wait_closed().await;
    assert_eq!(stats.spawned.load(Ordering::SeqCst), 1);
    assert_eq!(stats.connections.load(Ordering::SeqCst), 1);
    assert_eq!(stats.disconnected.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn durable_records_bind_context_incarnation_request_and_full_result() {
    let (_workspace, registry, calls, path) = setup(false, false, Duration::from_secs(5));
    let journal = Arc::new(Journal::new(path));
    let snapshot = registry.consumer().binding_snapshot();
    let result = invoke(&registry, journal.clone(), 17, CancellationToken::new())
        .await
        .unwrap();
    let rows = journal.rows();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["record"]["context"]["sequence"], 17);
    assert_eq!(
        rows[0]["record"]["context"]["execution_id"],
        "execution-four"
    );
    assert_eq!(rows[0]["record"]["key"], result["request"]);
    assert_eq!(
        rows[0]["record"]["key"]["source"],
        serde_json::to_value(&snapshot.sources["source"]).unwrap()
    );
    assert_eq!(rows[0]["record"]["arguments"], json!({}));
    assert_eq!(
        rows[0]["record"]["arguments_sha256"],
        "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
    );
    assert_eq!(rows[1]["outcome"]["state"], "succeeded");
    assert_eq!(rows[1]["outcome"]["value"], result);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(registry.snapshot("source").unwrap().unresolved.is_empty());
}
#[tokio::test]
async fn registry_incarnation_prevents_identical_source_replacement_and_pins_stay_immutable() {
    let (_a, first, _, _) = setup(false, false, Duration::from_secs(5));
    let (_b, second, _, _) = setup(false, false, Duration::from_secs(5));
    let bridge = first.consumer();
    let pinned = bridge.binding_snapshot();
    assert_ne!(pinned, second.consumer().binding_snapshot());
    assert_eq!(pinned.sources["source"].generation, 1);
    assert_eq!(
        pinned.sources["source"].manifest_sha256,
        second.consumer().binding_snapshot().sources["source"].manifest_sha256
    );
    first.disconnect("source").unwrap();
    assert_eq!(bridge.binding_snapshot(), pinned);
}
#[tokio::test]
async fn scope_is_single_capability_single_use_and_context_is_not_a_mutable_slot() {
    let (_workspace, registry, calls, path) = setup(false, false, Duration::from_secs(5));
    let journal = Arc::new(Journal::new(path));
    let bridge = registry.consumer();
    let scope = bridge
        .scoped("mcp.call", context(11), journal.clone())
        .unwrap();
    assert!(
        scope
            .call("skills.list", json!({}), CancellationToken::new())
            .await
            .is_err()
    );
    let a = invoke(&registry, journal.clone(), 12, CancellationToken::new());
    let b = invoke(&registry, journal.clone(), 13, CancellationToken::new());
    let (a, b) = tokio::join!(a, b);
    let a = a.unwrap();
    let b = b.unwrap();
    assert_ne!(a["request"], b["request"]);
    let scope = bridge
        .scoped("mcp.call", context(14), journal.clone())
        .unwrap();
    scope
        .call(
            "mcp.call",
            json!({"server":"source","tool":"echo","arguments":{}}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        scope
            .call(
                "mcp.call",
                json!({"server":"source","tool":"echo","arguments":{}}),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let rows = journal.rows();
    for sequence in [12, 13, 14] {
        let rows: Vec<_> = rows
            .iter()
            .filter(|r| r["record"]["context"]["sequence"] == sequence)
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["record"]["key"], rows[1]["record"]["key"]);
    }
}
#[tokio::test]
async fn two_pending_scopes_keep_separate_contexts_when_both_are_dropped() {
    let (_workspace, registry, calls, path) = setup(true, false, Duration::from_secs(5));
    let journal = Arc::new(Journal::new(path));
    let mut first = Box::pin(invoke(
        &registry,
        journal.clone(),
        201,
        CancellationToken::new(),
    ));
    let mut second = Box::pin(invoke(
        &registry,
        journal.clone(),
        202,
        CancellationToken::new(),
    ));
    tokio::select! {
        result=&mut first=>panic!("unexpected first {result:?}"),
        result=&mut second=>panic!("unexpected second {result:?}"),
        _=tokio::time::timeout(Duration::from_secs(5),async {while calls.load(Ordering::SeqCst)!=2 {tokio::task::yield_now().await;}})=>{}
    }
    assert_eq!(registry.snapshot("source").unwrap().unresolved.len(), 2);
    drop(first);
    drop(second);
    let rows = journal.rows();
    assert_eq!(rows.len(), 4);
    for sequence in [201, 202] {
        let own: Vec<_> = rows
            .iter()
            .filter(|row| row["record"]["context"]["sequence"] == sequence)
            .collect();
        assert_eq!(own.len(), 2);
        assert_eq!(own[0]["kind"], "started");
        assert_eq!(own[1]["outcome"]["state"], "unknown");
        assert_eq!(own[0]["record"], own[1]["record"]);
    }
    assert_ne!(rows[0]["record"]["key"], rows[1]["record"]["key"]);
}

#[tokio::test]
async fn preparation_failure_or_cancellation_is_durable_not_executed() {
    for fail in [false, true] {
        let (_workspace, registry, calls, path) = setup(false, false, Duration::from_secs(5));
        let cancel = CancellationToken::new();
        let mut journal = Journal::new(path);
        journal.fail_start = fail;
        journal.cancel = Some(cancel.clone());
        let journal = Arc::new(journal);
        let error = invoke(&registry, journal.clone(), 1, cancel)
            .await
            .unwrap_err();
        assert!(!error.outcome_unknown);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!registry.snapshot("source").unwrap().outcome_unknown);
        assert_eq!(
            journal.rows().last().unwrap()["outcome"]["state"],
            "not_executed"
        );
    }
}
#[tokio::test]
async fn journal_wait_and_synchronous_prepare_share_the_absolute_dispatch_deadline() {
    for block in [true, false] {
        let (_workspace, registry, calls, path) = setup(false, false, Duration::from_millis(200));
        let mut journal = Journal::new(path);
        journal.block_start = block;
        journal.delay_start = !block;
        let journal = Arc::new(journal);
        assert!(
            !invoke(&registry, journal.clone(), 1, CancellationToken::new())
                .await
                .unwrap_err()
                .outcome_unknown
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            journal.rows().last().unwrap()["outcome"]["state"],
            "not_executed"
        );
        assert!(!registry.snapshot("source").unwrap().outcome_unknown);
    }
}
#[tokio::test]
async fn invalidation_during_prepare_records_not_executed_and_callback_can_reenter_registry() {
    let (_workspace, registry, calls, path) = setup(false, false, Duration::from_secs(5));
    let journal = Journal::new(path);
    let inside = registry.clone();
    *journal.on_start.lock().unwrap() =
        Some(Box::new(move || inside.disconnect("source").unwrap()));
    let journal = Arc::new(journal);
    assert!(
        !invoke(&registry, journal.clone(), 1, CancellationToken::new())
            .await
            .unwrap_err()
            .outcome_unknown
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        journal.rows().last().unwrap()["outcome"]["state"],
        "not_executed"
    );
}
#[tokio::test]
async fn another_unknown_call_during_prepare_does_not_label_this_unexecuted_call_unknown() {
    let (_workspace, registry, calls, path) = setup(true, false, Duration::from_secs(5));
    let first_journal = Arc::new(Journal::new(path.clone()));
    let mut first = Box::pin(invoke(
        &registry,
        first_journal,
        301,
        CancellationToken::new(),
    ));
    tokio::select! {result=&mut first=>panic!("unexpected {result:?}"),_=async {while calls.load(Ordering::SeqCst)==0 {tokio::task::yield_now().await;}}=>{}}
    let journal = Journal::new(path);
    let reentrant = registry.clone();
    *journal.on_start.lock().unwrap() =
        Some(Box::new(move || reentrant.disconnect("source").unwrap()));
    let journal = Arc::new(journal);
    let error = invoke(&registry, journal.clone(), 302, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        !error.outcome_unknown,
        "this invocation never reached the source"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(registry.snapshot("source").unwrap().outcome_unknown);
    assert_eq!(
        journal.rows().last().unwrap()["outcome"]["state"],
        "not_executed"
    );
    drop(first);
}

#[tokio::test]
async fn known_source_failure_is_persisted_before_unresolved_record_is_removed() {
    let (_workspace, registry, calls, path) = setup(false, true, Duration::from_secs(5));
    let journal = Arc::new(Journal::new(path));
    assert!(
        !invoke(&registry, journal.clone(), 1, CancellationToken::new())
            .await
            .unwrap_err()
            .outcome_unknown
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(journal.rows().last().unwrap()["outcome"]["state"], "failed");
    assert!(!registry.snapshot("source").unwrap().outcome_unknown);
    assert!(registry.snapshot("source").unwrap().unresolved.is_empty());
}
#[tokio::test]
async fn terminal_write_failure_or_timeout_retains_unknown_before_return() {
    for fail in [true, false] {
        let (_workspace, registry, calls, path) = setup(false, false, Duration::from_secs(2));
        let mut journal = Journal::new(path);
        journal.fail_settle = fail;
        journal.block_settle = !fail;
        let journal = Arc::new(journal);
        assert!(
            invoke(&registry, journal.clone(), 1, CancellationToken::new())
                .await
                .unwrap_err()
                .outcome_unknown
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            journal.rows().last().unwrap()["outcome"]["state"],
            "unknown"
        );
        assert!(registry.snapshot("source").unwrap().outcome_unknown);
        assert_eq!(registry.snapshot("source").unwrap().unresolved.len(), 1);
    }
}
#[tokio::test]
async fn dropped_during_prepare_dispatch_or_settlement_keeps_correct_durable_state() {
    for phase in ["prepare", "dispatch", "settle"] {
        let (_workspace, registry, calls, path) =
            setup(phase == "dispatch", false, Duration::from_secs(5));
        let mut journal = Journal::new(path);
        journal.block_start = phase == "prepare";
        journal.block_settle = phase == "settle";
        let journal = Arc::new(journal);
        let mut pending = Box::pin(invoke(
            &registry,
            journal.clone(),
            1,
            CancellationToken::new(),
        ));
        tokio::select! {result=&mut pending=>panic!("unexpected {result:?}"),_=async {loop {let ready=match phase {"prepare"=>journal.starting.load(Ordering::SeqCst),"dispatch"=>calls.load(Ordering::SeqCst)>0,_=>journal.settling.load(Ordering::SeqCst)};if ready {break;}tokio::task::yield_now().await;}}=>{}}
        drop(pending);
        assert_eq!(
            journal.rows().last().unwrap()["outcome"]["state"],
            if phase == "prepare" {
                "not_executed"
            } else {
                "unknown"
            }
        );
        assert_eq!(
            registry.snapshot("source").unwrap().outcome_unknown,
            phase != "prepare"
        );
    }
}
