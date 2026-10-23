use cayw_core::{CoreError, CoreRequest, CoreResponse, PROTOCOL_VERSION, ProtocolEnvelope};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

struct ProtocolDatabase(PathBuf);
impl ProtocolDatabase {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("cayw-core-protocol-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn open(&self) -> cayw_core::SessionStore {
        cayw_core::SessionStore::open(self.0.join("core.sqlite")).unwrap()
    }
}
impl Drop for ProtocolDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct ProtocolVerifier(Vec<cayw_core::ReconciliationRequest>);
impl cayw_core::ReconciliationVerifier for ProtocolVerifier {
    fn verify(&self, request: &cayw_core::ReconciliationRequest) -> Result<(), String> {
        if self.0.contains(request) {
            Ok(())
        } else {
            Err("this exact decision has no receipt accepted by the test host".into())
        }
    }
}

fn dispatch_wire(
    store: &cayw_core::SessionStore,
    payload: Value,
) -> Result<CoreResponse, CoreError> {
    let wire = json!({"version": PROTOCOL_VERSION, "request_id": "req", "payload": payload});
    let request = ProtocolEnvelope::<CoreRequest>::decode(&wire.to_string())?;
    let response = ProtocolEnvelope::new(request.request_id, store.dispatch(request.payload)?)?;
    Ok(ProtocolEnvelope::<CoreResponse>::decode(&serde_json::to_string(&response)?)?.payload)
}

fn protocol_snapshot(store: &cayw_core::SessionStore, id: &str) -> cayw_core::SessionSnapshot {
    match dispatch_wire(store, json!({"type":"snapshot", "session_id":id})).unwrap() {
        CoreResponse::Session { snapshot } => *snapshot,
        response => panic!("expected a snapshot, got {response:?}"),
    }
}

fn protocol_events(store: &cayw_core::SessionStore, id: &str) -> Vec<cayw_core::EventRecord> {
    match dispatch_wire(
        store,
        json!({"type":"events", "session_id":id, "after":0, "limit":1000}),
    )
    .unwrap()
    {
        CoreResponse::Events { events } => events,
        response => panic!("expected events, got {response:?}"),
    }
}

#[test]
fn public_protocol_binds_original_task_before_reconciling_without_changing_history() {
    use cayw_agent::{AgentEvent, ToolResult};
    use cayw_ai::{AssistantMessage, Content, Message, Model, StopReason, ToolCall};
    use cayw_core::{
        ReconciledOutcome, Reconciliation, ReconciliationRequest, SessionState,
        ToolReconciliationOrigin,
    };

    let database = ProtocolDatabase::new();
    let store = database.open();
    let id = match dispatch_wire(
        &store,
        json!({"type":"create_session", "label":"original task"}),
    )
    .unwrap()
    {
        CoreResponse::Session { snapshot } => snapshot.id.clone(),
        response => panic!("expected a session, got {response:?}"),
    };
    let lease = match dispatch_wire(
        &store,
        json!({"type":"begin_attempt", "session_id":id, "worker_id":"original-worker"}),
    )
    .unwrap()
    {
        CoreResponse::Attempt { lease } => lease,
        response => panic!("expected an attempt, got {response:?}"),
    };
    let call = ToolCall::new(
        "original-call",
        "write_file",
        json!({"path":"original.txt"}),
    );
    let mut assistant = AssistantMessage::new(&Model::new("fixture", "offline"));
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content = vec![Content::ToolCall(call.clone())];
    let message = Message::Assistant(assistant);
    for event in [
        AgentEvent::AgentStart,
        AgentEvent::TurnStart,
        AgentEvent::MessageStart {
            message: message.clone(),
        },
        AgentEvent::MessageEnd { message },
        AgentEvent::ToolExecutionStart { call: call.clone() },
    ] {
        dispatch_wire(
            &store,
            json!({"type":"append_agent_event", "lease":lease, "event":event}),
        )
        .unwrap();
    }
    let binding = json!({"type":"bind_tool_origin", "lease":lease, "call_id":call.id, "task_id":"original-task"});
    let bound = dispatch_wire(&store, binding.clone())
        .expect("the public Core protocol must persist the original Task before execution");
    assert_eq!(dispatch_wire(&store, binding.clone()).unwrap(), bound);
    assert!(
        matches!(bound, CoreResponse::Event { event } if matches!(event.event, cayw_core::CoreEvent::ToolOriginBound { .. }))
    );
    let mut changed_binding = binding;
    changed_binding["task_id"] = json!("replacement-task");
    assert!(dispatch_wire(&store, changed_binding).is_err());
    dispatch_wire(
        &store,
        json!({"type":"mark_tool_started", "lease":lease, "call_id":call.id}),
    )
    .unwrap();
    dispatch_wire(&store, json!({"type":"recover", "session_id":id})).unwrap();
    let original = protocol_snapshot(&store, &id);
    let original_events = protocol_events(&store, &id);
    assert_eq!(original.state, SessionState::NeedsReconciliation);
    assert!(
        matches!(original.messages.last(), Some(Message::ToolResult(result)) if result.outcome_unknown)
    );

    let input = Reconciliation {
        call_id: call.id.clone(),
        expected: Box::new(ToolReconciliationOrigin {
            lease: lease.clone(),
            task_id: "original-task".into(),
            call,
            execution: None,
        }),
        actor: "trusted-host".into(),
        evidence: "host receipt for this exact original operation".into(),
        outcome: ReconciledOutcome::Completed {
            result: ToolResult::text("verified original result"),
        },
    };
    let request = |input: &Reconciliation| json!({"type":"reconcile", "session_id":id, "reconciliation":input});
    assert!(matches!(
        dispatch_wire(&store, request(&input)),
        Err(CoreError::ApprovalDenied)
    ));
    assert_eq!(protocol_snapshot(&store, &id), original);
    assert_eq!(protocol_events(&store, &id), original_events);
    let mut wrong = input.clone();
    wrong.expected.task_id = "replacement-task".into();
    let mut contradiction = input.clone();
    contradiction.outcome = ReconciledOutcome::NotExecuted {
        reason: "changed decision".into(),
    };
    let authorized = store
        .clone()
        .with_reconciliation_verifier(Arc::new(ProtocolVerifier(
            [input.clone(), wrong.clone(), contradiction.clone()]
                .into_iter()
                .map(|reconciliation| ReconciliationRequest::Tool { reconciliation })
                .collect(),
        )));
    assert!(dispatch_wire(&authorized, request(&wrong)).is_err());
    let mut weak = request(&input);
    weak["reconciliation"]
        .as_object_mut()
        .unwrap()
        .remove("expected");
    assert!(dispatch_wire(&authorized, weak).is_err());
    assert_eq!(protocol_snapshot(&store, &id), original);
    assert_eq!(protocol_events(&store, &id), original_events);

    let settled = dispatch_wire(&authorized, request(&input)).unwrap();
    assert_eq!(
        dispatch_wire(&authorized, request(&input)).unwrap(),
        settled
    );
    assert!(dispatch_wire(&authorized, request(&contradiction)).is_err());
    assert!(matches!(
        dispatch_wire(&store, request(&input)),
        Err(CoreError::ApprovalDenied)
    ));
    let reconciled = protocol_snapshot(&store, &id);
    assert_eq!(reconciled.state, SessionState::Idle);
    assert_eq!(reconciled.messages, original.messages);
    let events = protocol_events(&store, &id);
    assert_eq!(&events[..original_events.len()], original_events);
    assert_eq!(events.len(), original_events.len() + 1);
    assert!(matches!(
        events.last().unwrap().event,
        cayw_core::CoreEvent::Reconciled { .. }
    ));
    assert!(matches!(
        dispatch_wire(
            &authorized,
            json!({"type":"begin_attempt", "session_id":id, "worker_id":"explicit-new-worker"})
        )
        .unwrap(),
        CoreResponse::Attempt { .. }
    ));
    drop(authorized);
    drop(store);
    let reopened = database.open();
    assert_eq!(
        protocol_snapshot(&reopened, &id).messages,
        original.messages
    );
    assert_eq!(
        &protocol_events(&reopened, &id)[..original_events.len()],
        original_events
    );
}

#[test]
fn version_mismatch_is_rejected_before_interpreting_a_request() {
    let wire = json!({"version": PROTOCOL_VERSION + 1, "request_id":"req", "payload":{"type":"future_feature"}}).to_string();
    assert!(matches!(
        ProtocolEnvelope::<CoreRequest>::decode(&wire),
        Err(CoreError::ProtocolVersion { .. })
    ));
}

#[test]
fn old_approval_shape_and_missing_execution_facts_cannot_enter_the_current_protocol() {
    let old = json!({
        "version":1, "request_id":"legacy", "payload":{
            "type":"request_approval", "lease":{"session_id":"s","attempt_id":"a","worker_id":"w","fence":1},
            "input":{"task_id":"task","call_id":"c","capability":"fs.write","scope":"file","reason":"ask"}
        }
    });
    assert!(matches!(
        ProtocolEnvelope::<CoreRequest>::decode(&old.to_string()),
        Err(CoreError::ProtocolVersion { found: 1, .. })
    ));
    let mut disguised = old;
    disguised["version"] = json!(PROTOCOL_VERSION);
    assert!(ProtocolEnvelope::<CoreRequest>::decode(&disguised.to_string()).is_err());
    let incomplete_snapshot = json!({
        "id":"s","label":"retained", "messages":[],"cursor":1,"state":"idle","attempt":null,
        "pending_approvals":[],"unresolved_calls":[]
    });
    assert!(serde_json::from_value::<cayw_core::SessionSnapshot>(incomplete_snapshot).is_err());
}

#[test]
fn typed_execution_requests_and_events_round_trip_without_losing_their_origin() {
    use cayw_core::{
        AttemptLease, CapabilityCallKey, CapabilityInput, CoreEvent, ExecutionInput,
        ExecutionRecord, ExecutionState,
    };
    let lease = AttemptLease {
        session_id: "s".into(),
        attempt_id: "a".into(),
        worker_id: "w".into(),
        fence: 9,
    };
    let input = ExecutionInput {
        execution_id: "vm".into(),
        parent_call_id: "original-parent".into(),
        task_id: "host-task".into(),
        source_hash: "sha256:source".into(),
    };
    for request in [
        CoreRequest::BeginExecution {
            lease: lease.clone(),
            input: input.clone(),
        },
        CoreRequest::PlanCapability {
            lease: lease.clone(),
            input: CapabilityInput {
                key: CapabilityCallKey {
                    execution_id: "vm".into(),
                    sequence: 0,
                },
                name: "fs.write".into(),
                arguments: json!({"path":"note.txt"}),
            },
        },
    ] {
        let envelope = ProtocolEnvelope::new("req", request).unwrap();
        let wire = serde_json::to_string(&envelope).unwrap();
        assert_eq!(
            ProtocolEnvelope::<CoreRequest>::decode(&wire).unwrap(),
            envelope
        );
    }
    let execution = ExecutionRecord {
        execution_id: input.execution_id,
        parent_call_id: input.parent_call_id,
        task_id: input.task_id,
        source_hash: input.source_hash,
        lease,
        state: ExecutionState::Running,
        terminal: None,
    };
    let event = CoreEvent::ExecutionStarted { execution };
    let wire = serde_json::to_value(&event).unwrap();
    assert_eq!(wire["type"], "execution_started");
    assert_eq!(wire["execution"]["lease"]["fence"], 9);
    assert_eq!(serde_json::from_value::<CoreEvent>(wire).unwrap(), event);
}

#[test]
fn current_protocol_round_trips_and_rejects_unknown_fields_and_empty_request_ids() {
    let envelope = ProtocolEnvelope::new(
        "req-1",
        CoreRequest::Snapshot {
            session_id: "session-1".into(),
        },
    )
    .unwrap();
    let encoded = serde_json::to_string(&envelope).unwrap();
    assert_eq!(
        ProtocolEnvelope::<CoreRequest>::decode(&encoded).unwrap(),
        envelope
    );
    assert!(ProtocolEnvelope::<CoreRequest>::decode(&json!({"version":PROTOCOL_VERSION,"request_id":"req","payload":{"type":"snapshot","session_id":"s","extra":true}}).to_string()).is_err());
    assert!(
        ProtocolEnvelope::<CoreRequest>::decode(
            &json!({"version":PROTOCOL_VERSION,"request_id":"","payload":{"type":"list_sessions"}})
                .to_string()
        )
        .is_err()
    );
    assert!(ProtocolEnvelope::<CoreRequest>::decode(&json!({"version":PROTOCOL_VERSION,"request_id":"req","payload":{"type":"list_sessions"},"extra":true}).to_string()).is_err());
    let response = ProtocolEnvelope::new(
        "req-1",
        CoreResponse::Error {
            code: "needs_reconciliation".into(),
            message: "inspect effects before continuing".into(),
        },
    )
    .unwrap();
    let encoded = serde_json::to_string(&response).unwrap();
    assert_eq!(
        ProtocolEnvelope::<CoreResponse>::decode(&encoded).unwrap(),
        response
    );
}
