use cayw_agent::{AgentEvent, ToolResult};
use cayw_ai::{AssistantMessage, Content, Message, Model, StopReason, ToolCall};
use cayw_core::{
    ApprovalDecision, ApprovalInput, ApprovalTarget, AttemptLease, CapabilityAuthorization,
    CapabilityCallKey, CapabilityInput, CapabilityOutcome, CapabilityReconciliation,
    CapabilityStage, CoreError, CoreEvent, ExecutionInput, ExecutionOutcome, ExecutionState,
    SessionState, SessionStore,
};
use serde_json::{Value, json};
use std::path::PathBuf;
use uuid::Uuid;

struct Fixture {
    directory: PathBuf,
    store: SessionStore,
    lease: AttemptLease,
    parent: ToolCall,
    assistant: AssistantMessage,
}

impl Fixture {
    fn new(arguments: Value) -> Self {
        let directory = std::env::temp_dir().join(format!("cayw-core-vm-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let store = SessionStore::open(directory.join("session.sqlite")).unwrap();
        let id = store.create_session("VM records").unwrap().id;
        let lease = store.begin_attempt(&id, "original-worker").unwrap();
        store
            .append_agent_event(&lease, &AgentEvent::AgentStart)
            .unwrap();
        store
            .append_agent_event(&lease, &AgentEvent::TurnStart)
            .unwrap();
        let parent = ToolCall::new("original-parent", "execute_code", arguments);
        let assistant = model_message(vec![parent.clone()]);
        append_message(&store, &lease, Message::Assistant(assistant.clone()));
        store
            .append_agent_event(
                &lease,
                &AgentEvent::ToolExecutionStart {
                    call: parent.clone(),
                },
            )
            .unwrap();
        Self {
            directory,
            store,
            lease,
            parent,
            assistant,
        }
    }

    fn input(&self, id: &str) -> ExecutionInput {
        ExecutionInput {
            execution_id: id.into(),
            parent_call_id: self.parent.id.clone(),
            task_id: "host-task".into(),
            source_hash: "sha256:original-source".into(),
        }
    }

    fn begin(&self, id: &str) {
        self.store
            .mark_tool_started(&self.lease, &self.parent.id)
            .unwrap();
        self.store
            .begin_execution(&self.lease, self.input(id))
            .unwrap();
    }

    fn parent_end(&self) {
        self.store
            .append_agent_event(
                &self.lease,
                &AgentEvent::ToolExecutionEnd {
                    call: self.parent.clone(),
                    result: ToolResult::text("VM continues in background"),
                    duration_ms: Some(1),
                },
            )
            .unwrap();
    }

    fn agent_end(&self) {
        let results = self
            .store
            .snapshot(&self.lease.session_id)
            .unwrap()
            .messages
            .into_iter()
            .filter_map(|m| {
                if let Message::ToolResult(r) = m {
                    Some(r)
                } else {
                    None
                }
            })
            .collect();
        self.store
            .append_agent_event(
                &self.lease,
                &AgentEvent::TurnEnd {
                    message: self.assistant.clone(),
                    tool_results: results,
                },
            )
            .unwrap();
        self.store
            .append_agent_event(&self.lease, &AgentEvent::AgentEnd { messages: vec![] })
            .unwrap();
    }

    fn plan(&self, sequence: u64) -> CapabilityCallKey {
        let key = key(sequence);
        self.store
            .plan_capability(&self.lease, capability(key.clone()))
            .unwrap();
        key
    }

    fn projection(&self) -> String {
        self.connection()
            .query_row(
                "SELECT projection FROM core_sessions WHERE id=?1",
                [&self.lease.session_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn connection(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.directory.join("session.sqlite")).unwrap()
    }

    fn assert_rollback<T: std::fmt::Debug>(
        &self,
        operation: impl FnOnce() -> Result<T, CoreError>,
    ) {
        let before = self.store.snapshot(&self.lease.session_id).unwrap();
        let projection = self.projection();
        let db = self.connection();
        db.execute_batch("CREATE TRIGGER reject_projection BEFORE UPDATE ON core_sessions BEGIN SELECT RAISE(ABORT,'injected projection failure'); END;").unwrap();
        assert!(matches!(operation(), Err(CoreError::Database(_))));
        assert_eq!(self.store.snapshot(&self.lease.session_id).unwrap(), before);
        assert_eq!(self.projection(), projection);
        assert!(
            self.store
                .events(&self.lease.session_id, before.cursor, 100)
                .unwrap()
                .is_empty()
        );
        db.execute_batch("DROP TRIGGER reject_projection").unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn model_message(calls: Vec<ToolCall>) -> AssistantMessage {
    let mut assistant = AssistantMessage::new(&Model::new("fixture", "offline"));
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content = calls.into_iter().map(Content::ToolCall).collect();
    assistant
}

fn append_message(store: &SessionStore, lease: &AttemptLease, message: Message) {
    store
        .append_agent_event(
            lease,
            &AgentEvent::MessageStart {
                message: message.clone(),
            },
        )
        .unwrap();
    store
        .append_agent_event(lease, &AgentEvent::MessageEnd { message })
        .unwrap();
}

fn key(sequence: u64) -> CapabilityCallKey {
    CapabilityCallKey {
        execution_id: "vm".into(),
        sequence,
    }
}
fn capability(key: CapabilityCallKey) -> CapabilityInput {
    CapabilityInput {
        key,
        name: "fs.write".into(),
        arguments: json!({"path":"note.txt", "content":"saved"}),
    }
}
fn approval(key: CapabilityCallKey) -> ApprovalInput {
    ApprovalInput {
        task_id: "host-task".into(),
        target: ApprovalTarget::CapabilityCall { key },
        capability: "fs.write".into(),
        scope: "note.txt".into(),
        reason: "host must approve this write".into(),
    }
}
fn authorization(
    key: CapabilityCallKey,
    approval_id: Option<String>,
    external_provenance: Option<Value>,
) -> Value {
    serde_json::to_value(CapabilityAuthorization {
        actor: "host".into(),
        target: key,
        task_id: "host-task".into(),
        capability: "fs.write".into(),
        scope: "note.txt".into(),
        decision: ApprovalDecision::Approve,
        approval_id,
        external_provenance,
    })
    .unwrap()
}
fn not_executed() -> CapabilityOutcome {
    CapabilityOutcome::NotExecuted {
        reason: "cancelled before execution".into(),
    }
}
fn completed() -> CapabilityOutcome {
    CapabilityOutcome::Completed {
        result: json!({"bytes":5}),
    }
}
fn reconciliation(
    key: CapabilityCallKey,
    external_provenance: Option<Value>,
) -> CapabilityReconciliation {
    CapabilityReconciliation {
        key,
        actor: "inspecting-host".into(),
        evidence: "checked the durable operation receipt".into(),
        outcome: completed(),
        external_provenance,
    }
}

#[test]
fn legacy_effect_host_events_cannot_bypass_the_typed_execution_ledger() {
    let fixture = Fixture::new(json!({"script":"return 1"}));
    fixture.begin("vm");
    for kind in [
        "execution_started",
        "execution_yielded",
        "execution_finished",
        "capability_planned",
        "capability_started",
        "capability_settled",
        "capability_reconciled",
    ] {
        let before = fixture.store.snapshot(&fixture.lease.session_id).unwrap();
        let projection = fixture.projection();
        assert!(
            fixture
                .store
                .append_host_event(&fixture.lease, kind, json!({"execution_id":"untracked"}))
                .is_err()
        );
        assert!(
            fixture
                .store
                .append_control_event(
                    &before.id,
                    before.cursor,
                    Some(&fixture.lease),
                    "host",
                    kind,
                    json!({"execution_id":"untracked"})
                )
                .is_err()
        );
        assert_eq!(fixture.store.snapshot(&before.id).unwrap(), before);
        assert_eq!(fixture.projection(), projection);
    }
}

#[test]
fn a_started_child_is_the_only_unknown_when_the_parent_response_is_lost() {
    let fixture = Fixture::new(json!({"script":"return 1"}));
    fixture.begin("vm");
    let key = fixture.plan(0);
    fixture
        .store
        .start_capability(&fixture.lease, &key, authorization(key.clone(), None, None))
        .unwrap();
    let recovered = fixture.store.recover(&fixture.lease.session_id).unwrap();
    assert_eq!(recovered.state, SessionState::NeedsReconciliation);
    assert!(recovered.unresolved_calls.is_empty());
    assert_eq!(recovered.unresolved_capabilities.len(), 1);
    assert_eq!(recovered.unresolved_capabilities[0].key, key);
    assert_eq!(recovered.executions[0].state, ExecutionState::Interrupted);
    let recovered = fixture
        .store
        .reconcile_capability(&recovered.id, reconciliation(key, None))
        .unwrap();
    assert_eq!(recovered.state, SessionState::Idle);
    fixture
        .store
        .fork_session(&recovered.id, "paired copy", true)
        .unwrap();
}

// The host task remains authoritative even if the model claims another task.
#[test]
fn background_vm_keeps_its_origin_after_parent_completion_and_wait_call() {
    let fixture = Fixture::new(json!({"script":"return 1", "task_id":"model-forged-task"}));
    fixture.begin("vm");
    fixture.store.yield_execution(&fixture.lease, "vm").unwrap();
    fixture.parent_end();
    let results = fixture
        .store
        .snapshot(&fixture.lease.session_id)
        .unwrap()
        .messages
        .into_iter()
        .filter_map(|m| {
            if let Message::ToolResult(r) = m {
                Some(r)
            } else {
                None
            }
        })
        .collect();
    fixture
        .store
        .append_agent_event(
            &fixture.lease,
            &AgentEvent::TurnEnd {
                message: fixture.assistant.clone(),
                tool_results: results,
            },
        )
        .unwrap();
    fixture
        .store
        .append_agent_event(&fixture.lease, &AgentEvent::TurnStart)
        .unwrap();
    let wait = ToolCall::new("wait-parent", "wait", json!({"execution_id":"vm"}));
    append_message(
        &fixture.store,
        &fixture.lease,
        Message::Assistant(model_message(vec![wait.clone()])),
    );
    fixture
        .store
        .append_agent_event(
            &fixture.lease,
            &AgentEvent::ToolExecutionStart { call: wait.clone() },
        )
        .unwrap();
    fixture
        .store
        .mark_tool_started(&fixture.lease, &wait.id)
        .unwrap();
    assert!(
        fixture
            .store
            .begin_execution(
                &fixture.lease,
                ExecutionInput {
                    execution_id: "replacement".into(),
                    parent_call_id: wait.id,
                    ..fixture.input("replacement")
                }
            )
            .is_err()
    );
    let key = fixture.plan(0);
    let requested = fixture
        .store
        .request_approval(&fixture.lease, approval(key.clone()))
        .unwrap();
    fixture
        .store
        .decide_approval(
            &fixture.lease.session_id,
            &requested.id,
            ApprovalDecision::Approve,
            "approver",
        )
        .unwrap();
    fixture
        .store
        .start_capability(
            &fixture.lease,
            &key,
            authorization(key.clone(), Some(requested.id), None),
        )
        .unwrap();
    fixture
        .store
        .settle_capability(&fixture.lease, &key, completed())
        .unwrap();
    fixture
        .store
        .finish_execution(
            &fixture.lease,
            "vm",
            ExecutionOutcome::Completed { result: json!(1) },
        )
        .unwrap();
    let snapshot = fixture.store.snapshot(&fixture.lease.session_id).unwrap();
    let execution = &snapshot.executions[0];
    assert_eq!(execution.parent_call_id, "original-parent");
    assert_eq!(execution.task_id, "host-task");
    assert_eq!(execution.source_hash, "sha256:original-source");
    assert_eq!(execution.lease, fixture.lease);
    assert_eq!(execution.state, ExecutionState::Completed);
    let events = fixture.store.events(&snapshot.id, 0, 100).unwrap();
    let children: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            CoreEvent::CapabilityPlanned { call }
            | CoreEvent::CapabilityStarted { call }
            | CoreEvent::CapabilitySettled { call } => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(children.len(), 3);
    assert!(
        children
            .iter()
            .all(|call| call.parent_call_id == "original-parent"
                && call.task_id == "host-task"
                && call.lease == fixture.lease)
    );
    assert_eq!(children[0].stage, CapabilityStage::Planned);
    assert_eq!(children[1].stage, CapabilityStage::Started);
    assert_eq!(children[2].stage, CapabilityStage::Settled);
    let reopened = SessionStore::open(fixture.directory.join("session.sqlite")).unwrap();
    assert_eq!(reopened.snapshot(&snapshot.id).unwrap(), snapshot);
}

#[test]
fn exact_parent_approval_and_prior_execution_fix_the_host_task() {
    let fixture = Fixture::new(json!({"script":"return 1"}));
    let input = ApprovalInput {
        task_id: "host-task".into(),
        target: ApprovalTarget::ToolCall {
            call_id: fixture.parent.id.clone(),
        },
        capability: "execute_code".into(),
        scope: "script".into(),
        reason: "trusted host task".into(),
    };
    let request = fixture
        .store
        .request_approval(&fixture.lease, input)
        .unwrap();
    fixture
        .store
        .decide_approval(
            &fixture.lease.session_id,
            &request.id,
            ApprovalDecision::Approve,
            "host",
        )
        .unwrap();
    fixture
        .store
        .mark_tool_started(&fixture.lease, &fixture.parent.id)
        .unwrap();
    let before = fixture.store.snapshot(&fixture.lease.session_id).unwrap();
    assert!(
        fixture
            .store
            .begin_execution(
                &fixture.lease,
                ExecutionInput {
                    task_id: "another-task".into(),
                    ..fixture.input("vm")
                }
            )
            .is_err()
    );
    assert_eq!(
        fixture.store.snapshot(&fixture.lease.session_id).unwrap(),
        before
    );
    fixture
        .store
        .begin_execution(&fixture.lease, fixture.input("vm"))
        .unwrap();
    assert!(
        fixture
            .store
            .begin_execution(&fixture.lease, fixture.input("vm"))
            .is_err()
    );
    assert!(
        fixture
            .store
            .begin_execution(
                &fixture.lease,
                ExecutionInput {
                    task_id: "another-task".into(),
                    ..fixture.input("second")
                }
            )
            .is_err()
    );
    let key = fixture.plan(0);
    assert!(
        fixture
            .store
            .request_approval(
                &fixture.lease,
                ApprovalInput {
                    task_id: "another-task".into(),
                    ..approval(key.clone())
                }
            )
            .is_err()
    );
    let mut forged = authorization(key.clone(), None, None);
    forged["task_id"] = json!("another-task");
    assert!(
        fixture
            .store
            .start_capability(&fixture.lease, &key, forged)
            .is_err()
    );
    // No model task field is needed when the host supplies its binding.
    assert_eq!(
        fixture
            .store
            .snapshot(&fixture.lease.session_id)
            .unwrap()
            .executions[0]
            .task_id,
        "host-task"
    );
}

#[test]
fn capability_sequence_and_execution_boundary_are_durable() {
    let fixture = Fixture::new(json!({"script":"return 1"}));
    fixture.begin("vm");
    let before = fixture.projection();
    assert!(
        fixture
            .store
            .plan_capability(&fixture.lease, capability(key(1)))
            .is_err()
    );
    assert!(
        fixture
            .store
            .request_approval(&fixture.lease, approval(key(0)))
            .is_err()
    );
    assert!(
        fixture
            .store
            .start_capability(&fixture.lease, &key(0), authorization(key(0), None, None))
            .is_err()
    );
    assert_eq!(fixture.projection(), before);
    let first = fixture.plan(0);
    assert!(
        fixture
            .store
            .plan_capability(&fixture.lease, capability(first.clone()))
            .is_err()
    );
    assert!(
        fixture
            .store
            .settle_capability(&fixture.lease, &first, completed())
            .is_err()
    );
    fixture
        .store
        .settle_capability(&fixture.lease, &first, not_executed())
        .unwrap();
    assert!(
        fixture
            .store
            .start_capability(
                &fixture.lease,
                &first,
                authorization(first.clone(), None, None)
            )
            .is_err()
    );
    let second = fixture.plan(1);
    fixture
        .store
        .start_capability(
            &fixture.lease,
            &second,
            authorization(second.clone(), None, None),
        )
        .unwrap();
    assert!(
        fixture
            .store
            .start_capability(
                &fixture.lease,
                &second,
                authorization(second.clone(), None, None)
            )
            .is_err()
    );
    fixture
        .store
        .settle_capability(&fixture.lease, &second, completed())
        .unwrap();
    assert!(
        fixture
            .store
            .settle_capability(&fixture.lease, &second, completed())
            .is_err()
    );
    assert!(
        fixture
            .store
            .plan_capability(&fixture.lease, capability(key(3)))
            .is_err()
    );
    assert!(
        fixture
            .store
            .snapshot(&fixture.lease.session_id)
            .unwrap()
            .unresolved_capabilities
            .is_empty()
    );
}

#[test]
fn prompts_require_exact_approved_receipts_and_denials_cannot_be_retried() {
    let fixture = Fixture::new(json!({"script":"return 1"}));
    fixture.begin("vm");
    fixture.parent_end();
    let first = fixture.plan(0);
    let requested = fixture
        .store
        .request_approval(&fixture.lease, approval(first.clone()))
        .unwrap();
    assert!(matches!(
        fixture.store.start_capability(
            &fixture.lease,
            &first,
            authorization(first.clone(), None, None)
        ),
        Err(CoreError::PendingApproval)
    ));
    fixture
        .store
        .decide_approval(
            &fixture.lease.session_id,
            &requested.id,
            ApprovalDecision::Approve,
            "host",
        )
        .unwrap();
    for receipt in [None, Some("another-approval".into())] {
        assert!(matches!(
            fixture.store.start_capability(
                &fixture.lease,
                &first,
                authorization(first.clone(), receipt, None)
            ),
            Err(CoreError::ApprovalDenied)
        ));
    }
    let mut wrong_scope = authorization(first.clone(), Some(requested.id.clone()), None);
    wrong_scope["scope"] = json!("another.txt");
    assert!(
        fixture
            .store
            .start_capability(&fixture.lease, &first, wrong_scope)
            .is_err()
    );
    fixture
        .store
        .start_capability(
            &fixture.lease,
            &first,
            authorization(first.clone(), Some(requested.id.clone()), None),
        )
        .unwrap();
    fixture
        .store
        .settle_capability(&fixture.lease, &first, completed())
        .unwrap();
    let second = fixture.plan(1);
    assert!(
        fixture
            .store
            .start_capability(
                &fixture.lease,
                &second,
                authorization(second.clone(), Some(requested.id), None)
            )
            .is_err()
    );
    let denied = fixture
        .store
        .request_approval(&fixture.lease, approval(second.clone()))
        .unwrap();
    fixture
        .store
        .decide_approval(
            &fixture.lease.session_id,
            &denied.id,
            ApprovalDecision::Deny,
            "host",
        )
        .unwrap();
    fixture
        .store
        .settle_capability(&fixture.lease, &second, not_executed())
        .unwrap();
    let retry = fixture.plan(2);
    assert!(matches!(
        fixture
            .store
            .request_approval(&fixture.lease, approval(retry.clone())),
        Err(CoreError::ApprovalDenied)
    ));
    assert!(matches!(
        fixture.store.start_capability(
            &fixture.lease,
            &retry,
            authorization(retry.clone(), None, None)
        ),
        Err(CoreError::ApprovalDenied)
    ));
    fixture
        .store
        .settle_capability(&fixture.lease, &retry, not_executed())
        .unwrap();
}

// A pending prompt must not prevent recording that a planned effect was cancelled.
#[test]
fn cancelling_a_planned_capability_denies_its_pending_prompt_atomically() {
    let fixture = Fixture::new(json!({"script":"return 1"}));
    fixture.begin("vm");
    fixture.parent_end();
    let key = fixture.plan(0);
    let requested = fixture
        .store
        .request_approval(&fixture.lease, approval(key.clone()))
        .unwrap();
    let before = fixture.store.snapshot(&fixture.lease.session_id).unwrap();
    fixture
        .store
        .settle_capability(&fixture.lease, &key, not_executed())
        .unwrap();
    let decided = fixture
        .store
        .approval(&fixture.lease.session_id, &requested.id)
        .unwrap();
    assert_eq!(
        decided.decision.as_ref().unwrap().decision,
        ApprovalDecision::Deny
    );
    assert!(
        fixture
            .store
            .snapshot(&fixture.lease.session_id)
            .unwrap()
            .pending_approvals
            .is_empty()
    );
    let events = fixture
        .store
        .events(&fixture.lease.session_id, before.cursor, 100)
        .unwrap();
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0].event, CoreEvent::ApprovalDecided { .. }));
    assert!(matches!(
        events[1].event,
        CoreEvent::CapabilitySettled { .. }
    ));
    assert!(
        fixture
            .store
            .decide_approval(
                &fixture.lease.session_id,
                &requested.id,
                ApprovalDecision::Approve,
                "late-host"
            )
            .is_err()
    );
    fixture
        .store
        .finish_execution(
            &fixture.lease,
            "vm",
            ExecutionOutcome::Cancelled {
                reason: "host cancellation".into(),
            },
        )
        .unwrap();
}

#[test]
fn a_replacement_attempt_cannot_retry_a_denied_scope_for_the_same_host_task() {
    let fixture = Fixture::new(json!({"script":"return 1"}));
    fixture.begin("vm");
    fixture.parent_end();
    let first = fixture.plan(0);
    let denied = fixture
        .store
        .request_approval(&fixture.lease, approval(first.clone()))
        .unwrap();
    fixture
        .store
        .decide_approval(
            &fixture.lease.session_id,
            &denied.id,
            ApprovalDecision::Deny,
            "host",
        )
        .unwrap();
    fixture
        .store
        .settle_capability(&fixture.lease, &first, not_executed())
        .unwrap();
    fixture.store.recover(&fixture.lease.session_id).unwrap();
    let lease = fixture
        .store
        .begin_attempt(&fixture.lease.session_id, "replacement")
        .unwrap();
    fixture
        .store
        .append_agent_event(&lease, &AgentEvent::AgentStart)
        .unwrap();
    fixture
        .store
        .append_agent_event(&lease, &AgentEvent::TurnStart)
        .unwrap();
    let parent = ToolCall::new(
        "replacement-parent",
        "execute_code",
        json!({"script":"return 1"}),
    );
    append_message(
        &fixture.store,
        &lease,
        Message::Assistant(model_message(vec![parent.clone()])),
    );
    fixture
        .store
        .append_agent_event(
            &lease,
            &AgentEvent::ToolExecutionStart {
                call: parent.clone(),
            },
        )
        .unwrap();
    fixture.store.mark_tool_started(&lease, &parent.id).unwrap();
    fixture
        .store
        .begin_execution(
            &lease,
            ExecutionInput {
                execution_id: "replacement-vm".into(),
                parent_call_id: parent.id,
                ..fixture.input("replacement-vm")
            },
        )
        .unwrap();
    let key = CapabilityCallKey {
        execution_id: "replacement-vm".into(),
        sequence: 0,
    };
    fixture
        .store
        .plan_capability(&lease, capability(key.clone()))
        .unwrap();
    assert!(matches!(
        fixture
            .store
            .request_approval(&lease, approval(key.clone())),
        Err(CoreError::ApprovalDenied)
    ));
    assert!(matches!(
        fixture
            .store
            .start_capability(&lease, &key, authorization(key.clone(), None, None)),
        Err(CoreError::ApprovalDenied)
    ));
    fixture
        .store
        .settle_capability(&lease, &key, not_executed())
        .unwrap();
}

#[test]
fn cancelling_a_prompt_rolls_back_its_denials_if_the_child_terminal_cannot_be_saved() {
    let fixture = Fixture::new(json!({"script":"return 1"}));
    fixture.begin("vm");
    let key = fixture.plan(0);
    let request = fixture
        .store
        .request_approval(&fixture.lease, approval(key.clone()))
        .unwrap();
    let before = fixture.store.snapshot(&fixture.lease.session_id).unwrap();
    let projection = fixture.projection();
    let db = fixture.connection();
    db.execute_batch("CREATE TRIGGER reject_capability_terminal BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.type')='capability_settled' BEGIN SELECT RAISE(ABORT,'injected capability terminal failure'); END;").unwrap();
    assert!(matches!(
        fixture
            .store
            .settle_capability(&fixture.lease, &key, not_executed()),
        Err(CoreError::Database(_))
    ));
    assert_eq!(fixture.store.snapshot(&before.id).unwrap(), before);
    assert_eq!(fixture.projection(), projection);
    assert_eq!(
        fixture.store.approval(&before.id, &request.id).unwrap(),
        request
    );
    assert!(
        fixture
            .store
            .events(&before.id, before.cursor, 100)
            .unwrap()
            .is_empty()
    );
    db.execute_batch("DROP TRIGGER reject_capability_terminal")
        .unwrap();
    fixture
        .store
        .settle_capability(&fixture.lease, &key, not_executed())
        .unwrap();
    assert!(
        fixture
            .store
            .snapshot(&before.id)
            .unwrap()
            .pending_approvals
            .is_empty()
    );
}

#[test]
fn admitted_terminals_survive_agent_end_but_attempt_waits_for_the_vm() {
    let fixture = Fixture::new(json!({"script":"return 1"}));
    fixture.begin("vm");
    fixture.parent_end();
    let key = fixture.plan(0);
    fixture
        .store
        .start_capability(&fixture.lease, &key, authorization(key.clone(), None, None))
        .unwrap();
    fixture.agent_end();
    assert!(fixture.store.finish_attempt(&fixture.lease).is_err());
    assert!(
        fixture
            .store
            .begin_execution(&fixture.lease, fixture.input("later"))
            .is_err()
    );
    assert!(
        fixture
            .store
            .plan_capability(&fixture.lease, capability(self::key(1)))
            .is_err()
    );
    fixture
        .store
        .settle_capability(&fixture.lease, &key, completed())
        .unwrap();
    assert!(fixture.store.finish_attempt(&fixture.lease).is_err());
    fixture
        .store
        .finish_execution(
            &fixture.lease,
            "vm",
            ExecutionOutcome::Cancelled {
                reason: "host cancelled after model completion".into(),
            },
        )
        .unwrap();
    fixture.store.finish_attempt(&fixture.lease).unwrap();
    let snapshot = fixture.store.snapshot(&fixture.lease.session_id).unwrap();
    assert_eq!(snapshot.state, SessionState::Idle);
    assert_eq!(snapshot.executions[0].state, ExecutionState::Cancelled);
    assert!(snapshot.unresolved_capabilities.is_empty());
}

#[test]
fn unknown_child_blocks_new_model_messages_but_preserves_other_terminals() {
    let fixture = Fixture::new(json!({"script":"return 1"}));
    fixture.begin("vm");
    fixture.parent_end();
    let first = fixture.plan(0);
    let sibling = fixture.plan(1);
    for key in [&first, &sibling] {
        fixture
            .store
            .start_capability(&fixture.lease, key, authorization(key.clone(), None, None))
            .unwrap();
    }
    fixture
        .store
        .settle_capability(
            &fixture.lease,
            &first,
            CapabilityOutcome::OutcomeUnknown {
                error: "lost remote response".into(),
            },
        )
        .unwrap();
    fixture
        .store
        .settle_capability(&fixture.lease, &sibling, completed())
        .unwrap();
    assert!(matches!(
        fixture
            .store
            .begin_execution(&fixture.lease, fixture.input("another")),
        Err(CoreError::NeedsReconciliation)
    ));
    assert!(matches!(
        fixture
            .store
            .plan_capability(&fixture.lease, capability(key(2))),
        Err(CoreError::NeedsReconciliation)
    ));
    let results = fixture
        .store
        .snapshot(&fixture.lease.session_id)
        .unwrap()
        .messages
        .into_iter()
        .filter_map(|m| {
            if let Message::ToolResult(r) = m {
                Some(r)
            } else {
                None
            }
        })
        .collect();
    fixture
        .store
        .append_agent_event(
            &fixture.lease,
            &AgentEvent::TurnEnd {
                message: fixture.assistant.clone(),
                tool_results: results,
            },
        )
        .unwrap();
    fixture
        .store
        .append_agent_event(&fixture.lease, &AgentEvent::TurnStart)
        .unwrap();
    let pending = AssistantMessage::new(&Model::new("fixture", "offline"));
    let before = fixture.store.snapshot(&fixture.lease.session_id).unwrap();
    assert!(matches!(
        fixture.store.append_agent_event(
            &fixture.lease,
            &AgentEvent::MessageStart {
                message: Message::Assistant(pending)
            }
        ),
        Err(CoreError::NeedsReconciliation)
    ));
    assert_eq!(
        fixture.store.snapshot(&fixture.lease.session_id).unwrap(),
        before
    );
    let mut failure = AssistantMessage::new(&Model::new("fixture", "offline"));
    failure.stop_reason = StopReason::Error;
    append_message(
        &fixture.store,
        &fixture.lease,
        Message::Assistant(failure.clone()),
    );
    fixture
        .store
        .append_agent_event(
            &fixture.lease,
            &AgentEvent::TurnEnd {
                message: failure,
                tool_results: vec![],
            },
        )
        .unwrap();
    fixture
        .store
        .append_agent_event(&fixture.lease, &AgentEvent::AgentEnd { messages: vec![] })
        .unwrap();
    fixture
        .store
        .finish_execution(
            &fixture.lease,
            "vm",
            ExecutionOutcome::Failed {
                error: "inspect child effect".into(),
            },
        )
        .unwrap();
    fixture.store.finish_attempt(&fixture.lease).unwrap();
    let unknown = fixture.store.snapshot(&fixture.lease.session_id).unwrap();
    assert_eq!(unknown.unresolved_capabilities.len(), 1);
    assert!(unknown.unresolved_calls.is_empty());
    assert!(matches!(
        fixture.store.fork_session(&unknown.id, "copy", true),
        Err(CoreError::NeedsReconciliation)
    ));
    assert!(matches!(
        fixture.store.begin_attempt(&unknown.id, "new-worker"),
        Err(CoreError::NeedsReconciliation)
    ));
}

#[test]
fn lost_vm_without_unknown_children_interrupts_without_a_duplicate_parent_unknown() {
    let fixture = Fixture::new(json!({"script":"return 1"}));
    fixture.begin("vm");
    fixture.store.yield_execution(&fixture.lease, "vm").unwrap();
    fixture.plan(0);
    let recovered = fixture.store.recover(&fixture.lease.session_id).unwrap();
    assert_eq!(recovered.state, SessionState::Idle);
    assert_eq!(recovered.executions[0].state, ExecutionState::Interrupted);
    assert!(matches!(
        recovered.executions[0].terminal,
        Some(ExecutionOutcome::Interrupted { .. })
    ));
    assert!(recovered.unresolved_calls.is_empty());
    assert!(recovered.unresolved_capabilities.is_empty());
    let parent_result = recovered
        .messages
        .iter()
        .find_map(|m| {
            if let Message::ToolResult(r) = m {
                Some(r)
            } else {
                None
            }
        })
        .unwrap();
    assert!(parent_result.is_error);
    assert!(!parent_result.outcome_unknown);
    assert_eq!(parent_result.tool_call_id, "original-parent");
    let fork = fixture
        .store
        .fork_session(&recovered.id, "paired copy", true)
        .unwrap();
    assert_eq!(fork.messages, recovered.messages);
    assert!(fork.executions.is_empty());
    let next = fixture
        .store
        .begin_attempt(&recovered.id, "replacement-worker")
        .unwrap();
    assert!(next.fence > fixture.lease.fence);
    assert!(matches!(
        fixture.store.finish_execution(
            &fixture.lease,
            "vm",
            ExecutionOutcome::Completed { result: json!(1) }
        ),
        Err(CoreError::StaleLease)
    ));
    assert_eq!(
        fixture
            .store
            .snapshot(&recovered.id)
            .unwrap()
            .executions
            .len(),
        1
    );
}

#[test]
fn recovery_preserves_known_children_and_only_reconciles_started_effects() {
    let fixture = Fixture::new(json!({"script":"return 1"}));
    fixture.begin("vm");
    fixture.parent_end();
    let known = fixture.plan(0);
    fixture
        .store
        .start_capability(
            &fixture.lease,
            &known,
            authorization(known.clone(), None, None),
        )
        .unwrap();
    fixture
        .store
        .settle_capability(&fixture.lease, &known, completed())
        .unwrap();
    let started = fixture.plan(1);
    fixture
        .store
        .start_capability(
            &fixture.lease,
            &started,
            authorization(started.clone(), None, None),
        )
        .unwrap();
    let planned = fixture.plan(2);
    let pending = fixture
        .store
        .request_approval(&fixture.lease, approval(planned.clone()))
        .unwrap();
    let before = fixture.store.snapshot(&fixture.lease.session_id).unwrap();
    let parent = before.messages.clone();
    let old_events = fixture.store.events(&before.id, 0, 100).unwrap();
    let recovered = fixture.store.recover(&before.id).unwrap();
    assert_eq!(recovered.state, SessionState::NeedsReconciliation);
    assert_eq!(recovered.executions[0].state, ExecutionState::Interrupted);
    assert!(recovered.pending_approvals.is_empty());
    assert_eq!(recovered.unresolved_capabilities.len(), 1);
    assert_eq!(recovered.unresolved_capabilities[0].key, started);
    assert!(matches!(
        recovered.unresolved_capabilities[0].outcome,
        Some(CapabilityOutcome::OutcomeUnknown { .. })
    ));
    assert!(recovered.unresolved_calls.is_empty());
    assert_eq!(recovered.messages, parent);
    assert_eq!(
        fixture
            .store
            .approval(&before.id, &pending.id)
            .unwrap()
            .decision
            .unwrap()
            .decision,
        ApprovalDecision::Deny
    );
    assert_eq!(
        fixture
            .store
            .events(&before.id, 0, old_events.len())
            .unwrap(),
        old_events
    );
    let events = fixture
        .store
        .events(&before.id, before.cursor, 100)
        .unwrap();
    let capabilities = events
        .iter()
        .find_map(|e| {
            if let CoreEvent::AttemptRecovered { capabilities, .. } = &e.event {
                Some(capabilities)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(capabilities.len(), 2);
    assert!(
        capabilities.iter().any(|c| c.key == planned
            && matches!(c.outcome, Some(CapabilityOutcome::NotExecuted { .. })))
    );
    assert!(capabilities.iter().all(|c| c.key != known));
    assert!(matches!(
        fixture.store.fork_session(&before.id, "blocked", true),
        Err(CoreError::NeedsReconciliation)
    ));
    let reconciled = fixture
        .store
        .reconcile_capability(&before.id, reconciliation(started.clone(), None))
        .unwrap();
    assert_eq!(reconciled.state, SessionState::Idle);
    assert!(reconciled.unresolved_capabilities.is_empty());
    assert_eq!(reconciled.messages, parent);
    assert_eq!(reconciled.executions[0].state, ExecutionState::Interrupted);
    assert!(
        fixture
            .store
            .reconcile_capability(&before.id, reconciliation(known, None))
            .is_err()
    );
    assert!(
        fixture
            .store
            .reconcile_capability(&before.id, reconciliation(started, None))
            .is_err()
    );
    fixture
        .store
        .fork_session(&before.id, "allowed", true)
        .unwrap();
}

// A receipt from a different external operation must never reconcile this effect.
#[test]
fn reconciliation_checks_the_provenance_bound_before_external_execution() {
    let fixture = Fixture::new(json!({"script":"return 1"}));
    fixture.begin("vm");
    fixture.parent_end();
    let key = fixture.plan(0);
    let provenance = json!({"system":"device", "operation_id":"original-operation"});
    fixture
        .store
        .start_capability(
            &fixture.lease,
            &key,
            authorization(key.clone(), None, Some(provenance.clone())),
        )
        .unwrap();
    fixture.store.recover(&fixture.lease.session_id).unwrap();
    for external in [
        None,
        Some(json!({"system":"device", "operation_id":"different-operation"})),
    ] {
        let before = fixture.projection();
        assert!(
            fixture
                .store
                .reconcile_capability(
                    &fixture.lease.session_id,
                    reconciliation(key.clone(), external)
                )
                .is_err()
        );
        assert_eq!(fixture.projection(), before);
    }
    let recovered = fixture
        .store
        .reconcile_capability(
            &fixture.lease.session_id,
            reconciliation(key, Some(provenance)),
        )
        .unwrap();
    assert_eq!(recovered.state, SessionState::Idle);
    assert!(recovered.unresolved_capabilities.is_empty());
}

#[test]
fn execution_and_capability_projection_failures_roll_back_their_events() {
    let fixture = Fixture::new(json!({"script":"return 1"}));
    fixture
        .store
        .mark_tool_started(&fixture.lease, &fixture.parent.id)
        .unwrap();
    fixture.assert_rollback(|| {
        fixture
            .store
            .begin_execution(&fixture.lease, fixture.input("vm"))
    });
    fixture
        .store
        .begin_execution(&fixture.lease, fixture.input("vm"))
        .unwrap();
    fixture.assert_rollback(|| fixture.store.yield_execution(&fixture.lease, "vm"));
    fixture.store.yield_execution(&fixture.lease, "vm").unwrap();
    fixture.assert_rollback(|| {
        fixture
            .store
            .plan_capability(&fixture.lease, capability(key(0)))
    });
    let key = fixture.plan(0);
    fixture.assert_rollback(|| {
        fixture
            .store
            .start_capability(&fixture.lease, &key, authorization(key.clone(), None, None))
    });
    fixture
        .store
        .start_capability(&fixture.lease, &key, authorization(key.clone(), None, None))
        .unwrap();
    fixture.assert_rollback(|| {
        fixture
            .store
            .settle_capability(&fixture.lease, &key, completed())
    });
    fixture
        .store
        .settle_capability(&fixture.lease, &key, completed())
        .unwrap();
    fixture.assert_rollback(|| {
        fixture.store.finish_execution(
            &fixture.lease,
            "vm",
            ExecutionOutcome::Completed { result: json!(1) },
        )
    });
    fixture
        .store
        .finish_execution(
            &fixture.lease,
            "vm",
            ExecutionOutcome::Completed { result: json!(1) },
        )
        .unwrap();
}

#[test]
fn recovery_and_reconciliation_roll_back_all_child_facts_on_insert_failure() {
    let fixture = Fixture::new(json!({"script":"return 1"}));
    fixture.begin("vm");
    fixture.parent_end();
    let started = fixture.plan(0);
    fixture
        .store
        .start_capability(
            &fixture.lease,
            &started,
            authorization(started.clone(), None, None),
        )
        .unwrap();
    let planned = fixture.plan(1);
    let request = fixture
        .store
        .request_approval(&fixture.lease, approval(planned))
        .unwrap();
    let before = fixture.store.snapshot(&fixture.lease.session_id).unwrap();
    let projection = fixture.projection();
    let db = fixture.connection();
    db.execute_batch("CREATE TRIGGER reject_recovery BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.type')='attempt_recovered' BEGIN SELECT RAISE(ABORT,'injected terminal failure'); END;").unwrap();
    assert!(matches!(
        fixture.store.recover(&before.id),
        Err(CoreError::Database(_))
    ));
    assert_eq!(fixture.store.snapshot(&before.id).unwrap(), before);
    assert_eq!(fixture.projection(), projection);
    assert_eq!(
        fixture.store.approval(&before.id, &request.id).unwrap(),
        request
    );
    assert!(
        fixture
            .store
            .events(&before.id, before.cursor, 100)
            .unwrap()
            .is_empty()
    );
    fixture.store.validate_lease(&fixture.lease).unwrap();
    db.execute_batch("DROP TRIGGER reject_recovery").unwrap();
    fixture.store.recover(&before.id).unwrap();
    fixture.assert_rollback(|| {
        fixture
            .store
            .reconcile_capability(&before.id, reconciliation(started.clone(), None))
    });
    fixture
        .store
        .reconcile_capability(&before.id, reconciliation(started, None))
        .unwrap();
}

#[test]
fn every_execution_write_checks_all_lease_fields_and_fences_lost_workers() {
    let fixture = Fixture::new(json!({"script":"return 1"}));
    fixture.begin("vm");
    fixture.parent_end();
    let key = fixture.plan(0);
    for field in ["session", "attempt", "worker", "fence"] {
        let mut forged = fixture.lease.clone();
        match field {
            "session" => {
                forged.session_id = fixture.store.create_session("another session").unwrap().id
            }
            "attempt" => forged.attempt_id = "another-attempt".into(),
            "worker" => forged.worker_id = "another-worker".into(),
            _ => forged.fence += 1,
        }
        let before = fixture.projection();
        assert!(matches!(
            fixture.store.begin_execution(&forged, fixture.input("new")),
            Err(CoreError::StaleLease)
        ));
        assert!(matches!(
            fixture.store.yield_execution(&forged, "vm"),
            Err(CoreError::StaleLease)
        ));
        assert!(matches!(
            fixture
                .store
                .plan_capability(&forged, capability(self::key(1))),
            Err(CoreError::StaleLease)
        ));
        assert!(matches!(
            fixture
                .store
                .start_capability(&forged, &key, authorization(key.clone(), None, None)),
            Err(CoreError::StaleLease)
        ));
        assert!(matches!(
            fixture
                .store
                .settle_capability(&forged, &key, not_executed()),
            Err(CoreError::StaleLease)
        ));
        assert!(matches!(
            fixture.store.finish_execution(
                &forged,
                "vm",
                ExecutionOutcome::Cancelled {
                    reason: "cancel".into()
                }
            ),
            Err(CoreError::StaleLease)
        ));
        assert_eq!(fixture.projection(), before);
    }
    fixture.store.recover(&fixture.lease.session_id).unwrap();
    let current = fixture
        .store
        .begin_attempt(&fixture.lease.session_id, "replacement")
        .unwrap();
    assert!(current.fence > fixture.lease.fence);
    assert!(matches!(
        fixture.store.start_capability(
            &fixture.lease,
            &key,
            authorization(key.clone(), None, None)
        ),
        Err(CoreError::StaleLease)
    ));
    assert!(matches!(
        fixture
            .store
            .settle_capability(&fixture.lease, &key, not_executed()),
        Err(CoreError::StaleLease)
    ));
    assert!(matches!(
        fixture.store.yield_execution(&fixture.lease, "vm"),
        Err(CoreError::StaleLease)
    ));
    assert!(matches!(
        fixture.store.finish_execution(
            &fixture.lease,
            "vm",
            ExecutionOutcome::Cancelled {
                reason: "cancel".into()
            }
        ),
        Err(CoreError::StaleLease)
    ));
}
