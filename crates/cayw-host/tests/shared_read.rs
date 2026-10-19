use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{AuditSink, Host, HostConfig, PermissionGate};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

static NEXT_ROOT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        // Keep the leaf short: macOS sockaddr_un paths must stay under SUN_LEN
        // (~104). A long "cayw-shared-read-{pid}-{nanos}-…" name plus `.moved/special`
        // for UnixListener::bind overflowed that limit on GHA macos-latest.
        let path = std::env::temp_dir().join(format!(
            "csr-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Allow;
#[async_trait]
impl PermissionGate for Allow {
    async fn authorize(
        &self,
        _: &str,
        _: &Value,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
}
#[derive(Default)]
struct Audit(Mutex<Vec<String>>);
#[async_trait]
impl AuditSink for Audit {
    async fn started(&self, name: &str, _: &Value) -> Result<String, CapabilityError> {
        let mut calls = self.0.lock().unwrap();
        let id = format!("{name}:{}", calls.len());
        calls.push(id.clone());
        Ok(id)
    }
    async fn settled(
        &self,
        _: &str,
        _: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
    fn dropped(&self, _: &str) -> Result<(), CapabilityError> {
        Ok(())
    }
}
fn host(root: &Root, audit: Arc<Audit>) -> Host {
    Host::new(
        &root.0,
        Arc::new(Allow),
        audit,
        HostConfig {
            frozen_reads: true,
            ..HostConfig::default()
        },
    )
    .unwrap()
}

struct UnsettledAudit {
    interrupted: bool,
    entered: tokio::sync::Notify,
    dropped: std::sync::atomic::AtomicUsize,
}
#[async_trait]
impl AuditSink for UnsettledAudit {
    async fn started(&self, _: &str, _: &Value) -> Result<String, CapabilityError> {
        Ok("original-audit-failure".into())
    }
    async fn settled(
        &self,
        _: &str,
        _: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        self.entered.notify_one();
        if self.interrupted {
            std::future::pending().await
        } else {
            Err(CapabilityError::new("original audit storage failed"))
        }
    }
    fn dropped(&self, id: &str) -> Result<(), CapabilityError> {
        assert_eq!(id, "original-audit-failure");
        self.dropped
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

// Releasing the root fence at Host settlement rather than original Cells
// acknowledgment, or dropping the audit guard on interruption, breaks this test.
#[tokio::test]
async fn failed_or_interrupted_read_audit_keeps_original_fence_and_stops_dispatch() {
    for interrupted in [false, true] {
        let root = Root::new();
        std::fs::write(root.0.join("a.txt"), "frozen").unwrap();
        let audit = Arc::new(UnsettledAudit {
            interrupted,
            entered: tokio::sync::Notify::new(),
            dropped: std::sync::atomic::AtomicUsize::new(0),
        });
        let reader = Host::new(
            &root.0,
            Arc::new(Allow),
            audit.clone(),
            HostConfig {
                frozen_reads: true,
                ..HostConfig::default()
            },
        )
        .unwrap();
        let mut ticket = reader
            .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
            .await
            .unwrap();
        let value = ticket.capture().await.unwrap();
        if interrupted {
            let settlement = ticket.settle(Ok(value));
            tokio::pin!(settlement);
            assert!(futures::poll!(&mut settlement).is_pending());
            audit.entered.notified().await;
        } else {
            assert!(ticket.settle(Ok(value)).await.unwrap_err().outcome_unknown);
            assert!(reader.outcome_unknown());
        }
        let writer = host(&root, Arc::default());
        let write = writer.call(
            "fs.write",
            json!({"path":"never.txt","content":"never"}),
            CancellationToken::new(),
        );
        tokio::pin!(write);
        assert!(futures::poll!(&mut write).is_pending());
        assert!(!root.0.join("never.txt").exists());
        drop(ticket);
        assert!(write.await.unwrap_err().outcome_unknown);
        assert!(reader.outcome_unknown());
        assert_eq!(audit.dropped.load(std::sync::atomic::Ordering::SeqCst), 1);
        let reopened = host(&root, Arc::default());
        let faults = reopened
            .pending_read_faults(CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(faults.len(), 1);
        assert_eq!(faults[0].audit_id, "original-audit-failure");
        assert!(
            reopened
                .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
                .await
                .err()
                .unwrap()
                .outcome_unknown
        );
        assert_eq!(
            std::fs::read_to_string(root.0.join("a.txt")).unwrap(),
            "frozen"
        );
        assert!(!root.0.join("never.txt").exists());
    }
}

// Removing the root-wide shared fence from mutation admission makes this fail:
// the real fs.write completes while another connection still holds a read lease.
#[tokio::test]
async fn independently_opened_writer_waits_for_root_read_fence() {
    let root = Root::new();
    let fence = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.0.join(".cayw-host-read-fence"))
        .unwrap();
    fence.lock_shared().unwrap();
    let host = host(&root, Arc::default());
    let call = host.call(
        "fs.write",
        json!({"path":"file.txt","content":"first"}),
        CancellationToken::new(),
    );
    tokio::pin!(call);
    assert!(
        futures::poll!(&mut call).is_pending(),
        "writer crossed a live frozen read fence"
    );
    assert!(!root.0.join("file.txt").exists());
    drop(fence);
    assert_eq!(call.await.unwrap()["bytes"], 5);
}

// Acknowledging one read must retain the other read's lease and original audit.
#[tokio::test]
async fn tickets_keep_independent_audits_and_identical_write_changes_generation() {
    let root = Root::new();
    std::fs::write(root.0.join("a.txt"), "frozen").unwrap();
    let audit = Arc::new(Audit::default());
    let reader = host(&root, audit.clone());
    let mut first = reader
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    let mut second = reader
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    assert_ne!(first.audit_id(), second.audit_id());
    let stamp = first.stamp().clone();
    let captured = first.capture().await.unwrap();
    let accepted = second.accept(&stamp, captured.value().clone()).unwrap();
    assert_eq!(
        first.settle(Ok(captured)).await.unwrap()["content"],
        "frozen"
    );
    let writer = host(&root, Arc::default());
    let write = writer.call("fs.write", json!({"path":"a.txt","content":"frozen","overwrite":true,"expected_sha256":accepted.value()["sha256"]}), CancellationToken::new());
    tokio::pin!(write);
    assert!(futures::poll!(&mut write).is_pending());
    first.acknowledge();
    assert!(futures::poll!(&mut write).is_pending());
    assert_eq!(
        second.settle(Ok(accepted)).await.unwrap()["content"],
        "frozen"
    );
    assert!(futures::poll!(&mut write).is_pending());
    second.acknowledge();
    write.await.unwrap();
    let mut next = reader
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(next.stamp().workspace, stamp.workspace);
    assert_eq!(next.stamp().incarnation, stamp.incarnation);
    assert_eq!(next.stamp().generation, stamp.generation + 1);
    let value = next.capture().await.unwrap();
    next.settle(Ok(value)).await.unwrap();
    next.acknowledge();
    assert_eq!(audit.0.lock().unwrap().len(), 3);
}

// An abandoned original Cells acknowledgment freezes every Host connection.
#[tokio::test]
async fn abandoned_read_requires_exact_stopped_owner_recovery() {
    let root = Root::new();
    std::fs::write(root.0.join("a.txt"), "frozen").unwrap();
    let reader = host(&root, Arc::default());
    let ticket = reader
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    let audit_id = ticket.audit_id().to_owned();
    drop(ticket);
    let reopened = host(&root, Arc::default());
    let faults = reopened
        .pending_read_faults(CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(faults.len(), 1);
    assert_eq!(faults[0].audit_id, audit_id);
    assert!(
        reopened
            .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
            .await
            .err()
            .unwrap()
            .outcome_unknown
    );
    let recovery = cayw_host::ReadFenceRecovery {
        fault: faults[0].clone(),
        stopped_owner: "original worker joined".into(),
        cleanup_evidence: "original Device supervisor drained".into(),
        terminal_evidence: "canonical original Core and Cells terminals paired".into(),
    };
    let mut wrong = recovery.clone();
    wrong.fault.audit_id.push_str("wrong");
    assert!(
        reopened
            .recover_read_fault(wrong, CancellationToken::new())
            .await
            .is_err()
    );
    reopened
        .recover_read_fault(recovery, CancellationToken::new())
        .await
        .unwrap();
    let fresh = host(&root, Arc::default());
    let mut ticket = fresh
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    let captured = ticket.capture().await.unwrap();
    ticket.settle(Ok(captured)).await.unwrap();
    ticket.acknowledge();
}

#[tokio::test]
async fn frozen_tickets_keep_held_root_after_rename_and_reject_links_and_byte_limits() {
    let root = Root::new();
    std::fs::write(root.0.join("a.txt"), "original").unwrap();
    let reader = host(&root, Arc::default());
    let identity = reader.workspace_identity().unwrap();
    let moved = root.0.with_extension("moved");
    std::fs::rename(&root.0, &moved).unwrap();
    std::fs::create_dir(&root.0).unwrap();
    std::fs::write(root.0.join("a.txt"), "replacement").unwrap();
    let task = reader
        .task_host(Arc::new(Allow), Arc::new(Audit::default()))
        .unwrap();
    assert_eq!(task.workspace_identity().unwrap(), identity);
    let mut ticket = task
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    let value = ticket.capture().await.unwrap();
    assert_eq!(value.value()["content"], "original");
    ticket.settle(Ok(value)).await.unwrap();
    ticket.acknowledge();
    assert_eq!(
        std::fs::read_to_string(root.0.join("a.txt")).unwrap(),
        "replacement"
    );
    assert!(moved.join(".cayw-host-read-state").exists());
    assert!(!root.0.join(".cayw-host-read-state").exists());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("a.txt", moved.join("link.txt")).unwrap();
        let mut ticket = task
            .admit_frozen_read(json!({"path":"link.txt"}), CancellationToken::new())
            .await
            .unwrap();
        let error = ticket.capture().await.unwrap_err();
        assert!(!error.outcome_unknown);
        assert!(ticket.settle(Err(error)).await.is_err());
        ticket.acknowledge();
        std::os::unix::net::UnixListener::bind(moved.join("special")).unwrap();
        let mut ticket = task
            .admit_frozen_read(json!({"path":"special"}), CancellationToken::new())
            .await
            .unwrap();
        let error = ticket.capture().await.unwrap_err();
        assert!(!error.outcome_unknown);
        ticket.settle(Err(error)).await.unwrap_err();
        ticket.acknowledge();
    }
    let limited = Host::new(
        &moved,
        Arc::new(Allow),
        Arc::new(Audit::default()),
        HostConfig {
            frozen_reads: true,
            max_file_bytes: 3,
            ..Default::default()
        },
    )
    .unwrap();
    let mut ticket = limited
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    let error = ticket.capture().await.unwrap_err();
    assert!(error.message.contains("byte limit"));
    ticket.settle(Err(error)).await.unwrap_err();
    ticket.acknowledge();
    std::fs::remove_dir_all(moved).unwrap();
}

#[tokio::test]
async fn each_original_ticket_charges_the_shared_host_call_and_response_budgets() {
    let root = Root::new();
    std::fs::write(root.0.join("a.txt"), "frozen").unwrap();
    let reader = Host::new(
        &root.0,
        Arc::new(Allow),
        Arc::new(Audit::default()),
        HostConfig {
            frozen_reads: true,
            max_calls: 2,
            ..Default::default()
        },
    )
    .unwrap();
    let mut first = reader
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    let mut second = reader
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    let captured = first.capture().await.unwrap();
    let accepted = second
        .accept(first.stamp(), captured.value().clone())
        .unwrap();
    first.settle(Ok(captured)).await.unwrap();
    first.acknowledge();
    second.settle(Ok(accepted)).await.unwrap();
    second.acknowledge();
    assert!(
        reader
            .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
            .await
            .err()
            .unwrap()
            .message
            .contains("budget")
    );
    let args = json!({"path":"a.txt"});
    let result = json!({"path":"a.txt","content":"frozen","bytes":6,"sha256":"3a5ee21b9b350dcdd76323b13ba5eb8bdf7ad474a84bfcb77e31a4e7079f0250"});
    let allowance = 2 * serde_json::to_vec(&args).unwrap().len()
        + serde_json::to_vec(&result).unwrap().len()
        + 16;
    let reader = Host::new(
        &root.0,
        Arc::new(Allow),
        Arc::new(Audit::default()),
        HostConfig {
            frozen_reads: true,
            max_total_bytes: allowance,
            ..Default::default()
        },
    )
    .unwrap();
    let mut first = reader
        .admit_frozen_read(args.clone(), CancellationToken::new())
        .await
        .unwrap();
    let mut second = reader
        .admit_frozen_read(args, CancellationToken::new())
        .await
        .unwrap();
    let captured = first.capture().await.unwrap();
    let accepted = second
        .accept(first.stamp(), captured.value().clone())
        .unwrap();
    first.settle(Ok(captured)).await.unwrap();
    first.acknowledge();
    let error = second.settle(Ok(accepted)).await.unwrap_err();
    assert!(!error.outcome_unknown);
    second.acknowledge();
}

#[tokio::test]
async fn cancelled_fence_wait_never_creates_a_read_fault_or_consumes_another_ticket() {
    let root = Root::new();
    std::fs::write(root.0.join("a.txt"), "frozen").unwrap();
    let reader = host(&root, Arc::default());
    let fence = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.0.join(".cayw-host-read-fence"))
        .unwrap();
    fence.lock().unwrap();
    let cancel = CancellationToken::new();
    let pending = reader.admit_frozen_read(json!({"path":"a.txt"}), cancel.clone());
    tokio::pin!(pending);
    assert!(futures::poll!(&mut pending).is_pending());
    cancel.cancel();
    assert!(!pending.await.err().unwrap().outcome_unknown);
    drop(fence);
    assert!(
        reader
            .pending_read_faults(CancellationToken::new())
            .await
            .unwrap()
            .is_empty()
    );
    let mut ticket = reader
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    let value = ticket.capture().await.unwrap();
    ticket.settle(Ok(value)).await.unwrap();
    ticket.acknowledge();
}

// Rejecting another owner's fault must leave the original pending mutation
// untouched. Clearing it before checking that evidence would permit new work.
#[tokio::test]
async fn mismatched_persistence_fault_never_clears_original_pending_mutation() {
    struct Unknown;
    #[async_trait]
    impl cayw_host::ExternalHost for Unknown {
        async fn call(
            &self,
            _: &str,
            _: Value,
            _: CancellationToken,
        ) -> Result<Value, CapabilityError> {
            Err(CapabilityError::unknown(
                "original external completion unknown",
            ))
        }
    }
    let root = Root::new();
    let reader = Host::new(
        &root.0,
        Arc::new(Allow),
        Arc::new(Audit::default()),
        HostConfig {
            frozen_reads: true,
            external: Some(Arc::new(Unknown)),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        reader
            .call(
                "search.query",
                json!({"query":"fixture"}),
                CancellationToken::new()
            )
            .await
            .unwrap_err()
            .outcome_unknown
    );
    let pending = reader
        .pending_mutation(CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    std::fs::write(
        root.0.join(".cayw-host-read-fault"),
        "another-original-audit",
    )
    .unwrap();
    let recovery = cayw_host::FenceRecovery {
        pending: pending.clone(),
        stopped_owner: "original worker stopped".into(),
        cleanup_evidence: "original Device drained".into(),
        terminal_evidence: "known original Core and Cells terminals".into(),
    };
    assert!(
        reader
            .recover_mutation(recovery, CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(
        reader
            .pending_mutation(CancellationToken::new())
            .await
            .unwrap(),
        Some(pending)
    );
}

// A failed directory sync can leave the replacement state already visible with
// pending cleared. The separately persisted fault must still name that exact
// original mutation, so another connection can finish stopped-owner recovery.
#[tokio::test]
async fn visible_cleared_state_retains_exact_failed_mutation_until_recovery() {
    struct Unknown;
    #[async_trait]
    impl cayw_host::ExternalHost for Unknown {
        async fn call(
            &self,
            _: &str,
            _: Value,
            _: CancellationToken,
        ) -> Result<Value, CapabilityError> {
            Err(CapabilityError::unknown(
                "original external completion unknown",
            ))
        }
    }
    let root = Root::new();
    let original = Host::new(
        &root.0,
        Arc::new(Allow),
        Arc::new(Audit::default()),
        HostConfig {
            frozen_reads: true,
            external: Some(Arc::new(Unknown)),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        original
            .call(
                "search.query",
                json!({"query":"fixture"}),
                CancellationToken::new()
            )
            .await
            .unwrap_err()
            .outcome_unknown
    );
    let pending = original
        .pending_mutation(CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    let state_path = root.0.join(".cayw-host-read-state");
    let mut state: Value = serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    state["pending"] = Value::Null;
    std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    std::fs::write(
        root.0.join(".cayw-host-read-fault"),
        serde_json::to_vec(&json!({"kind":"mutation","original":pending})).unwrap(),
    )
    .unwrap();
    let reopened = host(&root, Arc::default());
    assert_eq!(
        reopened
            .pending_mutation(CancellationToken::new())
            .await
            .unwrap(),
        Some(pending.clone())
    );
    reopened
        .recover_mutation(
            cayw_host::FenceRecovery {
                pending,
                stopped_owner: "original worker stopped".into(),
                cleanup_evidence: "Device drained".into(),
                terminal_evidence: "original Core and Cells terminals".into(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        reopened
            .pending_mutation(CancellationToken::new())
            .await
            .unwrap()
            .is_none()
    );
    assert!(!root.0.join(".cayw-host-read-fault").exists());
}

// Fixed storage-window fixture: the next generation marker survived but its
// state replacement did not. Recover that original identity before admitting work.
#[tokio::test]
async fn failed_generation_replacement_retains_exact_original_mutation() {
    struct Unknown;
    #[async_trait]
    impl cayw_host::ExternalHost for Unknown {
        async fn call(
            &self,
            _: &str,
            _: Value,
            _: CancellationToken,
        ) -> Result<Value, CapabilityError> {
            Err(CapabilityError::unknown(
                "original external completion unknown",
            ))
        }
    }
    for absent in [false, true] {
        let root = Root::new();
        let original = Host::new(
            &root.0,
            Arc::new(Allow),
            Arc::new(Audit::default()),
            HostConfig {
                frozen_reads: true,
                external: Some(Arc::new(Unknown)),
                ..Default::default()
            },
        )
        .unwrap();
        original
            .call(
                "search.query",
                json!({"query":"fixture"}),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let pending = original
            .pending_mutation(CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        let state_path = root.0.join(".cayw-host-read-state");
        if absent {
            std::fs::remove_file(&state_path).unwrap();
        } else {
            let mut state: Value =
                serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
            state["pending"] = Value::Null;
            state["generation"] = json!(pending.generation - 1);
            std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        }
        std::fs::write(
            root.0.join(".cayw-host-read-fault"),
            serde_json::to_vec(&json!({"kind":"mutation","original":pending})).unwrap(),
        )
        .unwrap();
        let reopened = host(&root, Arc::default());
        assert_eq!(
            reopened
                .pending_mutation(CancellationToken::new())
                .await
                .unwrap(),
            Some(pending.clone())
        );
        reopened
            .recover_mutation(
                cayw_host::FenceRecovery {
                    pending: pending.clone(),
                    stopped_owner: "original worker stopped".into(),
                    cleanup_evidence: "Device drained".into(),
                    terminal_evidence: "original call confirmed not dispatched".into(),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            reopened
                .pending_mutation(CancellationToken::new())
                .await
                .unwrap()
                .is_none()
        );
        let state: Value = serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(state["generation"], pending.generation);
        assert_eq!(state["incarnation"], pending.incarnation);
    }
}

// Force the actual Drop persistence path to fail create_new, leaving the exact
// hashed original slot empty. The separately durable original record must repair
// that one slot under stopped-owner evidence, without a generic clear operation.
#[tokio::test]
async fn failed_read_fault_creation_retains_exact_recoverable_original() {
    use sha2::{Digest, Sha256};
    let root = Root::new();
    std::fs::write(root.0.join("a.txt"), "frozen").unwrap();
    let original = host(&root, Arc::default());
    let ticket = original
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    let audit_id = ticket.audit_id().to_owned();
    let stamp = ticket.stamp().clone();
    let dir = root.0.join(".cayw-host-read-faults");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{:x}", Sha256::digest(audit_id.as_bytes()))),
        [],
    )
    .unwrap();
    drop(ticket);
    let fresh = host(&root, Arc::default());
    let faults = fresh
        .pending_read_faults(CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(faults.len(), 1);
    assert_eq!(faults[0].audit_id, audit_id);
    assert_eq!(faults[0].stamp, stamp);
    assert!(
        fresh
            .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
            .await
            .err()
            .unwrap()
            .outcome_unknown
    );
    fresh
        .recover_read_fault(
            cayw_host::ReadFenceRecovery {
                fault: faults[0].clone(),
                stopped_owner: "original worker stopped".into(),
                cleanup_evidence: "Device drained".into(),
                terminal_evidence: "original Core and Cells paired".into(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!root.0.join(".cayw-host-read-fault").exists());
    assert!(
        fresh
            .pending_read_faults(CancellationToken::new())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        fresh.outcome_unknown(),
        "stopped Host must remain stopped after recovery"
    );
    let reopened = host(&root, Arc::default());
    let mut ticket = reopened
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    let value = ticket.capture().await.unwrap();
    ticket.settle(Ok(value)).await.unwrap();
    ticket.acknowledge();
}

// Accepting a digest-shaped string with non-hex bytes would reuse a corrupt
// generation. Exhaustion must refuse a write before dispatch or wraparound.
#[tokio::test]
async fn corrupt_generation_identity_and_counter_exhaustion_refuse_dispatch() {
    for field in [
        "incarnation",
        "pending_hash",
        "pending_audit",
        "pending_capability",
        "generation",
    ] {
        let root = Root::new();
        std::fs::write(root.0.join("a.txt"), "frozen").unwrap();
        let reader = host(&root, Arc::default());
        let mut ticket = reader
            .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
            .await
            .unwrap();
        let value = ticket.capture().await.unwrap();
        ticket.settle(Ok(value)).await.unwrap();
        ticket.acknowledge();
        let state_path = root.0.join(".cayw-host-read-state");
        let mut state: Value =
            serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        if field == "generation" {
            state["generation"] = json!(u64::MAX);
        } else if field == "incarnation" {
            state["incarnation"] = json!(format!("sha256:{}", "z".repeat(64)));
        } else {
            state["pending"] = json!({"workspace":state["workspace"],"incarnation":state["incarnation"],"generation":state["generation"],"audit_id":"original-mutation","capability":"fs.write","arguments_sha256":format!("sha256:{}", "a".repeat(64))});
            match field {
                "pending_hash" => {
                    state["pending"]["arguments_sha256"] =
                        json!(format!("sha256:{}", "z".repeat(64)))
                }
                "pending_audit" => state["pending"]["audit_id"] = json!("x".repeat(4097)),
                "pending_capability" => state["pending"]["capability"] = json!("x".repeat(257)),
                _ => unreachable!(),
            }
        }
        std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        let fresh = host(&root, Arc::default());
        if field == "generation" {
            let error = fresh
                .call(
                    "fs.write",
                    json!({"path":"never.txt","content":"never"}),
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert!(error.message.contains("exhausted"));
            assert!(!root.0.join("never.txt").exists());
            let saved: Value =
                serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
            assert_eq!(saved["generation"], u64::MAX);
        } else {
            assert!(
                fresh
                    .pending_mutation(CancellationToken::new())
                    .await
                    .is_err(),
                "accepted corrupt {field}"
            );
            assert!(
                fresh
                    .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
                    .await
                    .is_err()
            );
        }
    }
}

#[tokio::test]
async fn abandoned_original_read_faults_recover_independently() {
    let root = Root::new();
    std::fs::write(root.0.join("a.txt"), "frozen").unwrap();
    let reader = host(&root, Arc::default());
    let first = reader
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    let second = reader
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    drop(first);
    drop(second);
    let fresh = host(&root, Arc::default());
    let faults = fresh
        .pending_read_faults(CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(faults.len(), 2);
    for (index, fault) in faults.into_iter().enumerate() {
        fresh
            .recover_read_fault(
                cayw_host::ReadFenceRecovery {
                    fault,
                    stopped_owner: "original worker stopped".into(),
                    cleanup_evidence: "Device drained".into(),
                    terminal_evidence: "exact original terminals".into(),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        if index == 0 {
            assert!(
                fresh
                    .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
                    .await
                    .err()
                    .unwrap()
                    .outcome_unknown
            );
        }
    }
    assert!(
        fresh
            .pending_read_faults(CancellationToken::new())
            .await
            .unwrap()
            .is_empty()
    );
}
