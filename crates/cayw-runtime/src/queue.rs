//! Durable queue types are implemented in `cayw-queue`. This module keeps the
//! host Runtime's coordinated policy amendment and maps the queue's own error
//! type onto the Runtime error contract.

use crate::RuntimeError;
use std::path::Path;

pub use cayw_queue::{
    QueueError, QueuePolicy, TaskLease, TaskPriority, TaskRecord, TaskState, TaskTerminal,
};

/// The host Runtime's handle to one durable queue database. Policy amendment
/// needs the Runtime's cross-process coordination, so it stays here and is not
/// part of the queue domain.
pub(crate) struct Queue(cayw_queue::Queue);
impl Queue {
    pub(crate) fn open_with_resources(
        path: &Path,
        concurrency: usize,
        capacity: usize,
        fairness_after: usize,
        resources: &cayw_queue::ResourcePolicy,
    ) -> Result<Self, RuntimeError> {
        let requested = QueuePolicy::new(concurrency, capacity, fairness_after, resources)?;
        // A database that already recorded one complete policy adopts a
        // different explicit host policy only after every other Runtime has
        // stopped and no task is live. The exclusive policy lock prevents an
        // old in-memory connection from admitting work against a policy that
        // was already replaced.
        if cayw_queue::read_policy(path)?.is_some_and(|stored| stored != requested) {
            crate::coordination::amend_policy(path, || {
                Ok(cayw_queue::adopt_policy(path, requested)?)
            })?;
        }
        Ok(Self(cayw_queue::Queue::open_with_resources(
            path,
            concurrency,
            capacity,
            fairness_after,
            resources,
        )?))
    }

    /// The host settlement runs inside the queue's single terminal transaction.
    /// Host errors are preserved so a failed authority write rolls back both the
    /// task terminal and its audit row.
    pub(crate) fn finish(
        &self,
        lease: &TaskLease,
        terminal: TaskTerminal,
        error: Option<String>,
        settle: impl FnOnce(TaskTerminal) -> Result<(), RuntimeError>,
    ) -> Result<(), RuntimeError> {
        self.0.finish(lease, terminal, error, settle)
    }

    /// Final consistency check after all domains finished opening. It never
    /// amends the stored policy.
    pub(crate) fn check_policy(
        &self,
        concurrency: usize,
        capacity: usize,
        fairness_after: usize,
        resources: &cayw_queue::ResourcePolicy,
    ) -> Result<(), RuntimeError> {
        Ok(self
            .0
            .check_policy(concurrency, capacity, fairness_after, resources)?)
    }

    /// The Runtime reads cancellation for the original queue lease and Core
    /// attempt. The queue only needs their plain identity strings, so it does
    /// not depend on the Core crate.
    pub(crate) fn model_cancelled(
        &self,
        task: &TaskLease,
        lease: &cayw_core::AttemptLease,
    ) -> Result<bool, RuntimeError> {
        Ok(self
            .0
            .model_cancelled(task, &lease.session_id, &lease.attempt_id)?)
    }

    /// These two reads are returned directly by public Runtime methods, so the
    /// host error type is mapped here instead of at every call site.
    pub(crate) fn get(&self, id: &str) -> Result<TaskRecord, RuntimeError> {
        Ok(self.0.get(id)?)
    }
    pub(crate) fn list(&self, session: &str) -> Result<Vec<TaskRecord>, RuntimeError> {
        Ok(self.0.list(session)?)
    }
}
impl std::ops::Deref for Queue {
    type Target = cayw_queue::Queue;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<QueueError> for RuntimeError {
    fn from(error: QueueError) -> Self {
        match error {
            QueueError::InvalidInput(message) => Self::InvalidInput(message),
            QueueError::Busy => Self::Busy,
            QueueError::QueueFull => Self::QueueFull,
            QueueError::Cancelled => Self::Cancelled,
            QueueError::Worker(message) => Self::Worker(message),
            QueueError::StaleCancellation => Self::StaleCancellation,
            QueueError::Database(error) => Self::Journal(error),
            QueueError::Json(error) => Self::Json(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{RequestPolicy, Runtime, RuntimeConfig, RuntimeError, TaskState};
    use cayw_ai::{
        AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Model,
        ModelRequest, Provider, StopReason,
    };
    use std::sync::Arc;

    struct TerminalProvider;
    impl Provider for TerminalProvider {
        fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
            let mut message = AssistantMessage::new(&request.model);
            message
                .content
                .push(Content::text("synthetic known response"));
            message.stop_reason = StopReason::Stop;
            Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
        }
    }

    #[tokio::test]
    async fn authority_ended_before_failed_queue_commit_cannot_accept_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.sqlite");
        let config = RuntimeConfig {
            max_concurrent_runs: 1,
            max_queued_runs: 1,
            request_policy: Some(RequestPolicy {
                permission_policy_id: "synthetic-commit-failure".into(),
                operations: vec![cayw_requests::OperationScope::exact(
                    "read-fixture",
                    "fs.read",
                    serde_json::json!({"path":"unused.txt"}),
                    "workspace-policy",
                )],
            }),
            ..Default::default()
        };
        let rt = Runtime::open(
            &path,
            root.path(),
            Arc::new(TerminalProvider),
            Model::new("fixture", "commit-failure"),
            config.clone(),
        )
        .unwrap();
        let peer = Runtime::open(
            &path,
            root.path(),
            Arc::new(TerminalProvider),
            Model::new("fixture", "commit-failure"),
            config,
        )
        .unwrap();
        let id = rt.create(None).unwrap().id;
        // A deferred foreign key fails at COMMIT, after real Requests end_task
        // succeeded. No event/history is rewritten and no production hook exists.
        rt.inner
            .queue
            .connection_for_test()
            .execute_batch(
                "PRAGMA foreign_keys=ON;
                 CREATE TABLE fixture_parent(id INTEGER PRIMARY KEY);
                 CREATE TABLE fixture_deferred(parent INTEGER REFERENCES fixture_parent(id) DEFERRABLE INITIALLY DEFERRED);
                 CREATE TRIGGER fail_terminal_commit AFTER INSERT ON runtime_audit WHEN NEW.kind='run_finished'
                 BEGIN INSERT INTO fixture_deferred VALUES(1); END;",
            )
            .unwrap();
        let error = rt
            .prompt(
                &id,
                "synthetic final commit failure",
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::RecoveryRequired { .. }));
        let task = rt.tasks(&id).unwrap().remove(0);
        assert_eq!(task.state, TaskState::Draining);
        assert_eq!(rt.health().unwrap().running, 1);
        assert!(matches!(
            rt.request_task_state(&task.id).unwrap(),
            cayw_requests::TaskState::Ended {
                outcome: cayw_requests::TaskOutcome::Completed,
                ..
            }
        ));
        assert!(
            !rt.audit(&id, 0, 100)
                .unwrap()
                .iter()
                .any(|event| event.kind.starts_with("run_"))
        );
        assert!(
            matches!(
                peer.cancel(&task.cancel_target()),
                Err(RuntimeError::RecoveryRequired { .. })
            ),
            "a committed authority terminal must not acknowledge a conflicting cancellation"
        );
        rt.inner
            .queue
            .connection_for_test()
            .execute_batch("DROP TRIGGER fail_terminal_commit")
            .unwrap();
        peer.recover(&id).unwrap();
        assert_eq!(rt.health().unwrap().running, 0);
        assert!(matches!(
            rt.request_task_state(&task.id).unwrap(),
            cayw_requests::TaskState::Ended {
                outcome: cayw_requests::TaskOutcome::Completed,
                ..
            }
        ));
    }
}
