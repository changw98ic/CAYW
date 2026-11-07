#![cfg(feature = "fault-injection")]

use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Model,
    ModelRequest, Provider, StopReason, UsageStatus,
};
use cayw_runtime::{Runtime, RuntimeConfig, RuntimeError, TaskState, WorkerFailure};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

struct CompletedProvider;
impl Provider for CompletedProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let mut message = AssistantMessage::new(&request.model);
        message
            .content
            .push(Content::text("synthetic completed turn"));
        message.stop_reason = StopReason::Stop;
        message.usage.input = 1;
        message.usage.output = 1;
        message.usage.update_total();
        message.usage_status = UsageStatus::Reported;
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

#[tokio::test]
async fn worker_panic_keeps_its_safe_reason_and_unknown_state() {
    const REASON: &str = "worker cleanup failed: 缓存\u{1b}[31m\nsecond line\u{202e}";
    let panics = Arc::new(AtomicUsize::new(0));
    let observed = panics.clone();
    // This existing checkpoint runs in worker cleanup, after the Agent has
    // returned. It reaches the Runtime panic boundary without a new test hook.
    cayw_ai::faults::install(Arc::new(move |point, _| {
        if point == "core.attempt_terminal.committed" {
            match observed.fetch_add(1, Ordering::SeqCst) {
                0 => std::panic::panic_any(REASON),
                1 => std::panic::panic_any(REASON.to_owned()),
                2 => std::panic::panic_any(42u32),
                _ => panic!("unexpected extra worker execution"),
            }
        }
    }))
    .unwrap();

    for expected_reason in [
        REASON.escape_debug().to_string(),
        REASON.escape_debug().to_string(),
        "non-string panic payload".into(),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let rt = Runtime::open(
            temp.path().join("session.db"),
            temp.path(),
            Arc::new(CompletedProvider),
            Model::new("fixture", "worker-panic"),
            RuntimeConfig::default(),
        )
        .unwrap();
        let session = rt.create(None).unwrap().id;
        let task = rt.enqueue(&session, "worker panic fixture").unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(10),
            rt.resume(&task.id, CancellationToken::new()),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(error.code(), "worker_panicked_outcome_unknown", "{error:?}");
        assert!(error.outcome_unknown());
        assert_eq!(error.worker_failure(), Some(WorkerFailure::Panic));
        let RuntimeError::RecoveryRequired {
            cause,
            outcome_unknown: true,
            ..
        } = &error
        else {
            panic!("worker panic must retain explicit recovery: {error:?}");
        };
        let RuntimeError::WorkerPanicked(reason) = cause.as_ref() else {
            panic!("worker panic must retain its typed cause: {error:?}");
        };
        assert_eq!(reason, &expected_reason);
        assert!(!reason.chars().any(char::is_control));
        assert!(!reason.contains('\u{202e}'));
        let message = error.to_string();
        let client = cayw_ipc::ClientError::from(cayw_ipc::RpcError::from(error));
        assert_eq!(client.code, "worker_panicked_outcome_unknown");
        assert!(client.outcome_unknown);
        assert_eq!(client.message, message);
        let stored = rt.task(&task.id).unwrap();
        assert_eq!(stored.state, TaskState::Draining);
        assert!(stored.holds_slot);
        assert_eq!(stored.error.as_deref(), Some(message.as_str()));
        assert_eq!(rt.health().unwrap().running, 1);
        assert!(rt.enqueue(&session, "do not replay worker panic").is_err());
    }
    assert_eq!(panics.load(Ordering::SeqCst), 3);
}
