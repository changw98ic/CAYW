use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{AuditSink, FenceRecovery, Host, HostConfig, PermissionGate};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct Root(PathBuf);
static NEXT_ROOT: AtomicUsize = AtomicUsize::new(1);
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cayw-fence-probe-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_ROOT.fetch_add(1, Ordering::SeqCst)
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
struct Audit {
    calls: AtomicUsize,
    fail_settle: bool,
}
#[async_trait]
impl AuditSink for Audit {
    async fn started(&self, name: &str, _: &Value) -> Result<String, CapabilityError> {
        Ok(format!(
            "{name}:{}",
            self.calls.fetch_add(1, Ordering::SeqCst)
        ))
    }
    async fn settled(
        &self,
        _: &str,
        _: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        if self.fail_settle {
            Err(CapabilityError::unknown("fixture audit commit failed"))
        } else {
            Ok(())
        }
    }
    fn dropped(&self, _: &str) -> Result<(), CapabilityError> {
        Ok(())
    }
}
fn host(root: &Root, config: HostConfig, fail_settle: bool) -> Host {
    Host::new(
        &root.0,
        Arc::new(Allow),
        Arc::new(Audit {
            calls: AtomicUsize::new(0),
            fail_settle,
        }),
        config,
    )
    .unwrap()
}
fn config() -> HostConfig {
    HostConfig {
        frozen_reads: true,
        ..HostConfig::default()
    }
}

#[tokio::test]
async fn held_root_rename_and_replacement_cannot_redirect_frozen_capture_or_workspace_identity() {
    let parent = Root::new();
    let path = parent.0.join("root");
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join("a.txt"), "held bytes").unwrap();
    let reader = Host::new(
        &path,
        Arc::new(Allow),
        Arc::new(Audit {
            calls: AtomicUsize::new(0),
            fail_settle: false,
        }),
        config(),
    )
    .unwrap();
    let mut ticket = reader
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    let stamp = ticket.stamp().clone();
    std::fs::rename(&path, parent.0.join("moved")).unwrap();
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join("a.txt"), "replacement bytes").unwrap();
    let captured = ticket.capture().await.unwrap();
    assert_eq!(captured.value()["content"], "held bytes");
    ticket.settle(Ok(captured)).await.unwrap();
    ticket.acknowledge();
    let mut same = reader
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(same.stamp(), &stamp);
    let captured = same.capture().await.unwrap();
    same.settle(Ok(captured)).await.unwrap();
    same.acknowledge();
    let replacement = Host::new(
        &path,
        Arc::new(Allow),
        Arc::new(Audit {
            calls: AtomicUsize::new(0),
            fail_settle: false,
        }),
        config(),
    )
    .unwrap();
    let mut other = replacement
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    assert_ne!(other.stamp().workspace, stamp.workspace);
    let captured = other.capture().await.unwrap();
    assert_eq!(captured.value()["content"], "replacement bytes");
    other.settle(Ok(captured)).await.unwrap();
    other.acknowledge();
}

#[tokio::test]
async fn invalid_utf8_directories_symlinks_and_oversized_files_are_never_reusable_values() {
    let root = Root::new();
    std::fs::write(root.0.join("invalid"), [0xff, 0xfe]).unwrap();
    std::fs::write(root.0.join("large"), "12345").unwrap();
    std::fs::create_dir(root.0.join("directory")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.0.join("large"), root.0.join("link")).unwrap();
    let reader = host(
        &root,
        HostConfig {
            max_file_bytes: 4,
            ..config()
        },
        false,
    );
    let mut paths = vec!["invalid", "large", "directory"];
    #[cfg(unix)]
    paths.push("link");
    for path in paths {
        let mut ticket = reader
            .admit_frozen_read(json!({"path":path}), CancellationToken::new())
            .await
            .unwrap();
        let error = ticket.capture().await.err().unwrap();
        assert!(!error.outcome_unknown);
        assert!(
            !ticket
                .settle(Err(error))
                .await
                .err()
                .unwrap()
                .outcome_unknown
        );
        ticket.acknowledge();
    }
    assert!(!reader.outcome_unknown());
}

#[tokio::test]
async fn captured_bytes_and_original_stamp_are_validated_for_every_independently_admitted_ticket() {
    let root = Root::new();
    std::fs::write(root.0.join("a.txt"), "four").unwrap();
    let reader = host(&root, config(), false);
    let mut first = reader
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    let captured = first.capture().await.unwrap();
    let stamp = first.stamp().clone();
    let mut second = reader
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    let mut bad = captured.value().clone();
    bad["content"] = json!("forged");
    assert!(second.accept(&stamp, bad).is_err());
    let mut bad_stamp = stamp.clone();
    bad_stamp.generation += 1;
    assert!(second.accept(&bad_stamp, captured.value().clone()).is_err());
    let accepted = second.accept(&stamp, captured.value().clone()).unwrap();
    second.settle(Ok(accepted)).await.unwrap();
    second.acknowledge();
    first.settle(Ok(captured)).await.unwrap();
    first.acknowledge();
}

#[tokio::test]
async fn audit_failure_leaves_exact_original_mutation_pending_until_stopped_owner_recovery() {
    let root = Root::new();
    let writer = host(&root, config(), true);
    let error = writer
        .call(
            "fs.write",
            json!({"path":"a.txt","content":"once"}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.outcome_unknown);
    assert_eq!(
        std::fs::read_to_string(root.0.join("a.txt")).unwrap(),
        "once"
    );
    let inspector = host(&root, config(), false);
    let pending = inspector
        .pending_mutation(CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.audit_id, "fs.write:0");
    assert_eq!(pending.generation, 1);
    let evidence = FenceRecovery {
        pending: pending.clone(),
        stopped_owner: "original task lock retained after join".into(),
        cleanup_evidence: "original Device process supervisors drained".into(),
        terminal_evidence: "trusted original durable terminal repaired".into(),
    };
    let mut wrong = evidence.clone();
    wrong.pending.arguments_sha256.push('0');
    assert!(
        inspector
            .recover_mutation(wrong, CancellationToken::new())
            .await
            .is_err()
    );
    inspector
        .recover_mutation(evidence, CancellationToken::new())
        .await
        .unwrap();
    assert!(
        inspector
            .pending_mutation(CancellationToken::new())
            .await
            .unwrap()
            .is_none()
    );
    let mut ticket = inspector
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(ticket.stamp().generation, pending.generation);
    let captured = ticket.capture().await.unwrap();
    assert_eq!(captured.value()["content"], "once");
    ticket.settle(Ok(captured)).await.unwrap();
    ticket.acknowledge();
    assert!(
        writer.outcome_unknown(),
        "recovery silently reset original unsafe Host instance"
    );
}

#[tokio::test]
async fn wrong_or_symlinked_fault_cannot_erase_original_pending_mutation() {
    for symlink in [false, true] {
        if symlink && !cfg!(unix) {
            continue;
        }
        let root = Root::new();
        let writer = host(&root, config(), true);
        assert!(
            writer
                .call(
                    "fs.write",
                    json!({"path":"a.txt","content":"once"}),
                    CancellationToken::new()
                )
                .await
                .unwrap_err()
                .outcome_unknown
        );
        let inspector = host(&root, config(), false);
        let pending = inspector
            .pending_mutation(CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        let marker = root.0.join(".cayw-host-read-fault");
        if symlink {
            std::fs::write(root.0.join("outside-marker"), &pending.audit_id).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(root.0.join("outside-marker"), &marker).unwrap();
        } else {
            std::fs::write(&marker, "different-original-audit").unwrap();
        }
        let evidence = FenceRecovery {
            pending: pending.clone(),
            stopped_owner: "original worker stopped".into(),
            cleanup_evidence: "original process cleanup confirmed".into(),
            terminal_evidence: "original known terminal retained".into(),
        };
        assert!(
            inspector
                .recover_mutation(evidence.clone(), CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(
            inspector
                .pending_mutation(CancellationToken::new())
                .await
                .unwrap(),
            Some(pending)
        );
        std::fs::remove_file(marker).unwrap();
        inspector
            .recover_mutation(evidence, CancellationToken::new())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn cancellation_while_waiting_for_fence_settles_original_call_without_advancing_generation() {
    let root = Root::new();
    std::fs::write(root.0.join("a.txt"), "same").unwrap();
    let reader = host(&root, config(), false);
    let mut ticket = reader
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    let generation = ticket.stamp().generation;
    let writer = host(&root, config(), false);
    let cancel = CancellationToken::new();
    let run = writer.call(
        "fs.write",
        json!({"path":"a.txt","content":"different"}),
        cancel.clone(),
    );
    tokio::pin!(run);
    assert!(futures::poll!(&mut run).is_pending());
    cancel.cancel();
    assert!(!run.await.unwrap_err().outcome_unknown);
    let captured = ticket.capture().await.unwrap();
    ticket.settle(Ok(captured)).await.unwrap();
    ticket.acknowledge();
    assert!(
        writer
            .pending_mutation(CancellationToken::new())
            .await
            .unwrap()
            .is_none()
    );
    let mut next = reader
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(next.stamp().generation, generation);
    let captured = next.capture().await.unwrap();
    next.settle(Ok(captured)).await.unwrap();
    next.acknowledge();
}

#[tokio::test]
async fn root_state_corruption_and_generation_exhaustion_fail_before_mutation() {
    for corrupt in [false, true] {
        let root = Root::new();
        std::fs::write(root.0.join("a.txt"), "same").unwrap();
        let reader = host(&root, config(), false);
        let mut ticket = reader
            .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
            .await
            .unwrap();
        let captured = ticket.capture().await.unwrap();
        ticket.settle(Ok(captured)).await.unwrap();
        ticket.acknowledge();
        let path = root.0.join(".cayw-host-read-state");
        let mut state: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if corrupt {
            state["workspace"] = json!("wrong-held-root");
        } else {
            state["generation"] = json!(u64::MAX);
        }
        std::fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(
            reader
                .call(
                    "fs.write",
                    json!({"path":"a.txt","content":"never"}),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(root.0.join("a.txt")).unwrap(),
            "same"
        );
    }
}

#[tokio::test]
async fn malformed_state_digests_never_create_reusable_tickets_or_recoverable_mutations() {
    for pending_hash in [false, true] {
        let root = Root::new();
        std::fs::write(root.0.join("a.txt"), "same").unwrap();
        let reader = host(&root, config(), false);
        let mut ticket = reader
            .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
            .await
            .unwrap();
        let value = ticket.capture().await.unwrap();
        ticket.settle(Ok(value)).await.unwrap();
        ticket.acknowledge();
        let path = root.0.join(".cayw-host-read-state");
        let mut state: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if pending_hash {
            state["pending"] = json!({"workspace":state["workspace"],"incarnation":state["incarnation"],"generation":state["generation"],"audit_id":"original-audit","capability":"fs.write","arguments_sha256":format!("sha256:{}","z".repeat(64))});
        } else {
            state["incarnation"] = json!(format!("sha256:{}", "z".repeat(64)));
        }
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        if pending_hash {
            assert!(
                reader
                    .pending_mutation(CancellationToken::new())
                    .await
                    .is_err(),
                "invalid original arguments digest accepted"
            );
        } else {
            let admitted = reader
                .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
                .await;
            assert!(
                admitted.is_err(),
                "invalid incarnation digest admitted reusable bytes"
            );
        }
    }
}

#[tokio::test]
async fn unclassified_external_result_loss_is_unknown_and_retains_original_mutation() {
    struct Unclassified(AtomicUsize);
    #[async_trait]
    impl cayw_host::ExternalHost for Unclassified {
        async fn call(
            &self,
            _: &str,
            _: Value,
            _: CancellationToken,
        ) -> Result<Value, CapabilityError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"effect":"x".repeat(1024)}))
        }
    }
    let root = Root::new();
    let external = Arc::new(Unclassified(AtomicUsize::new(0)));
    let mut cfg = config();
    cfg.max_result_bytes = 8;
    cfg.external = Some(external.clone());
    let writer = host(&root, cfg, false);
    let error = writer
        .call("mcp.discover", json!({}), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(external.0.load(Ordering::SeqCst), 1);
    assert!(
        error.outcome_unknown,
        "unclassified external result loss was a known failure"
    );
    let inspector = host(&root, config(), false);
    assert_eq!(
        inspector
            .pending_mutation(CancellationToken::new())
            .await
            .unwrap()
            .unwrap()
            .capability,
        "mcp.discover"
    );
}

#[tokio::test]
async fn deadline_is_retained_while_waiting_for_original_fence() {
    let root = Root::new();
    let fence = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.0.join(".cayw-host-read-fence"))
        .unwrap();
    fence.lock().unwrap();
    let reader = host(
        &root,
        HostConfig {
            call_timeout: Duration::from_millis(40),
            ..config()
        },
        false,
    );
    let error = reader
        .admit_frozen_read(json!({"path":"a.txt"}), CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert!(!error.outcome_unknown);
    assert!(!reader.outcome_unknown());
}
