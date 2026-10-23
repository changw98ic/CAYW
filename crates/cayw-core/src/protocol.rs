use crate::{
    ApprovalDecision, ApprovalInput, ApprovalRequest, AttemptLease, CapabilityCallKey,
    CapabilityCallRecord, CapabilityInput, CapabilityOutcome, CapabilityReconciliation, CoreError,
    EventRecord, ExecutionInput, ExecutionOutcome, ExecutionRecord, MAX_IDENTIFIER_BYTES,
    PROTOCOL_VERSION, Reconciliation, SessionSnapshot, SessionStore,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Expose every owned JSON leaf so protocol input can be bounded before it is
/// cloned, serialized or rejected. Implementations must return the same complete
/// set of leaves from both methods, without recursing inside a `Value`. Forward
/// through finite Rust wrappers to their fields; do not omit hidden JSON fields.
/// This lets rejection drain deep caller-owned JSON without recursive destruction.
/// The contract bounds JSON leaves, not arbitrary recursive Rust wrapper types.
/// Adding an envelope does not grant any host authority to its payload.
pub trait ProtocolPayload {
    fn json_values(&self) -> Vec<&serde_json::Value>;
    fn json_values_mut(&mut self) -> Vec<&mut serde_json::Value>;
}

/// The same envelope is used over any host-selected transport.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolEnvelope<T> {
    pub version: u32,
    pub request_id: String,
    pub payload: T,
}

impl<T: ProtocolPayload> ProtocolEnvelope<T> {
    pub fn new(request_id: impl Into<String>, payload: T) -> Result<Self, CoreError> {
        let payload = crate::store::validation::checked_input(payload)?;
        let request_id = request_id.into();
        if request_id.trim().is_empty() || request_id.len() > MAX_IDENTIFIER_BYTES {
            return Err(CoreError::InvalidInput(
                "request_id must not be empty".into(),
            ));
        }
        Ok(Self {
            version: PROTOCOL_VERSION,
            request_id,
            payload,
        })
    }
}

impl<T: DeserializeOwned + ProtocolPayload> ProtocolEnvelope<T> {
    /// Check the version before decoding the payload, including unknown future variants.
    pub fn decode(json: &str) -> Result<Self, CoreError> {
        let raw: ProtocolEnvelope<serde_json::Value> = serde_json::from_str(json)?;
        if raw.version != PROTOCOL_VERSION {
            return Err(CoreError::ProtocolVersion {
                found: raw.version,
                expected: PROTOCOL_VERSION,
            });
        }
        Self::new(raw.request_id, serde_json::from_value(raw.payload)?)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CoreRequest {
    CreateSession {
        label: String,
    },
    ForkSession {
        source_session_id: String,
        label: String,
        confirmed: bool,
    },
    ListSessions,
    Snapshot {
        session_id: String,
    },
    Events {
        session_id: String,
        after: u64,
        limit: usize,
    },
    BeginAttempt {
        session_id: String,
        worker_id: String,
    },
    ValidateLease {
        lease: AttemptLease,
    },
    AppendHostEvent {
        lease: AttemptLease,
        kind: String,
        data: serde_json::Value,
    },
    AppendAgentEvent {
        lease: AttemptLease,
        event: Box<cayw_agent::AgentEvent>,
    },
    BindToolOrigin {
        lease: AttemptLease,
        call_id: String,
        task_id: String,
    },
    MarkToolStarted {
        lease: AttemptLease,
        call_id: String,
    },
    BeginExecution {
        lease: AttemptLease,
        input: ExecutionInput,
    },
    YieldExecution {
        lease: AttemptLease,
        execution_id: String,
    },
    RegisterExecutionWait {
        lease: AttemptLease,
        call_id: String,
        execution_id: String,
        task_id: String,
        arguments: serde_json::Value,
    },
    PlanCapability {
        lease: AttemptLease,
        input: CapabilityInput,
    },
    StartCapability {
        lease: AttemptLease,
        key: CapabilityCallKey,
        authorization: serde_json::Value,
    },
    SettleCapability {
        lease: AttemptLease,
        key: CapabilityCallKey,
        outcome: CapabilityOutcome,
    },
    FinishExecution {
        lease: AttemptLease,
        execution_id: String,
        outcome: ExecutionOutcome,
    },
    ReconcileCapability {
        session_id: String,
        reconciliation: CapabilityReconciliation,
    },
    AppendControlEvent {
        session_id: String,
        expected_cursor: u64,
        expected_attempt: Option<AttemptLease>,
        actor: String,
        kind: String,
        data: serde_json::Value,
    },
    FinishAttempt {
        lease: AttemptLease,
    },
    Recover {
        session_id: String,
    },
    RequestApproval {
        lease: AttemptLease,
        input: ApprovalInput,
    },
    Approvals {
        session_id: String,
    },
    Approval {
        session_id: String,
        approval_id: String,
    },
    DecideApproval {
        session_id: String,
        approval_id: String,
        decision: ApprovalDecision,
        actor: String,
    },
    Reconcile {
        session_id: String,
        reconciliation: Reconciliation,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CoreResponse {
    Session { snapshot: Box<SessionSnapshot> },
    Sessions { sessions: Vec<SessionSnapshot> },
    Events { events: Vec<EventRecord> },
    Event { event: Box<EventRecord> },
    Attempt { lease: AttemptLease },
    Execution { execution: Box<ExecutionRecord> },
    Capability { call: Box<CapabilityCallRecord> },
    Approval { approval: ApprovalRequest },
    Approvals { approvals: Vec<ApprovalRequest> },
    Acknowledged,
    Error { code: String, message: String },
}

impl SessionStore {
    /// Dispatch only from a trusted host transport. Exposing mutation requests
    /// directly to a model or untrusted peer would expose host authority.
    pub fn dispatch(&self, request: CoreRequest) -> Result<CoreResponse, CoreError> {
        let request = crate::store::validation::checked_input(request)?;
        let snapshot = |snapshot| CoreResponse::Session {
            snapshot: Box::new(snapshot),
        };
        let event = |event| CoreResponse::Event {
            event: Box::new(event),
        };
        Ok(match request {
            CoreRequest::CreateSession { label } => snapshot(self.create_session(label)?),
            CoreRequest::ForkSession {
                source_session_id,
                label,
                confirmed,
            } => snapshot(self.fork_session(&source_session_id, &label, confirmed)?),
            CoreRequest::ListSessions => CoreResponse::Sessions {
                sessions: self.list_sessions()?,
            },
            CoreRequest::Snapshot { session_id } => snapshot(self.snapshot(&session_id)?),
            CoreRequest::Events {
                session_id,
                after,
                limit,
            } => CoreResponse::Events {
                events: self.events(&session_id, after, limit)?,
            },
            CoreRequest::BeginAttempt {
                session_id,
                worker_id,
            } => CoreResponse::Attempt {
                lease: self.begin_attempt(&session_id, &worker_id)?,
            },
            CoreRequest::ValidateLease { lease } => {
                self.validate_lease(&lease)?;
                CoreResponse::Acknowledged
            }
            CoreRequest::AppendHostEvent { lease, kind, data } => {
                event(self.append_host_event(&lease, &kind, data)?)
            }
            CoreRequest::AppendControlEvent {
                session_id,
                expected_cursor,
                expected_attempt,
                actor,
                kind,
                data,
            } => event(self.append_control_event(
                &session_id,
                expected_cursor,
                expected_attempt.as_ref(),
                &actor,
                &kind,
                data,
            )?),
            CoreRequest::AppendAgentEvent {
                lease,
                event: agent_event,
            } => {
                let agent_event = crate::store::validation::checked_input(agent_event)?;
                event(self.append_agent_event(&lease, &agent_event)?)
            }
            CoreRequest::BindToolOrigin {
                lease,
                call_id,
                task_id,
            } => event(self.bind_tool_origin(&lease, &call_id, &task_id)?),
            CoreRequest::MarkToolStarted { lease, call_id } => {
                event(self.mark_tool_started(&lease, &call_id)?)
            }
            CoreRequest::BeginExecution { lease, input } => CoreResponse::Execution {
                execution: Box::new(self.begin_execution(&lease, input)?),
            },
            CoreRequest::YieldExecution {
                lease,
                execution_id,
            } => event(self.yield_execution(&lease, &execution_id)?),
            CoreRequest::RegisterExecutionWait {
                lease,
                call_id,
                execution_id,
                task_id,
                arguments,
            } => {
                let arguments = crate::store::validation::checked_input(arguments)?;
                event(self.register_execution_wait(
                    &lease,
                    &call_id,
                    &execution_id,
                    &task_id,
                    &arguments,
                )?)
            }
            CoreRequest::PlanCapability { lease, input } => CoreResponse::Capability {
                call: Box::new(self.plan_capability(&lease, input)?),
            },
            CoreRequest::StartCapability {
                lease,
                key,
                authorization,
            } => event(self.start_capability(&lease, &key, authorization)?),
            CoreRequest::SettleCapability {
                lease,
                key,
                outcome,
            } => event(self.settle_capability(&lease, &key, outcome)?),
            CoreRequest::FinishExecution {
                lease,
                execution_id,
                outcome,
            } => event(self.finish_execution(&lease, &execution_id, outcome)?),
            CoreRequest::ReconcileCapability {
                session_id,
                reconciliation,
            } => snapshot(self.reconcile_capability(&session_id, reconciliation)?),
            CoreRequest::FinishAttempt { lease } => {
                self.finish_attempt(&lease)?;
                CoreResponse::Acknowledged
            }
            CoreRequest::Recover { session_id } => snapshot(self.recover(&session_id)?),
            CoreRequest::RequestApproval { lease, input } => CoreResponse::Approval {
                approval: self.request_approval(&lease, input)?,
            },
            CoreRequest::Approvals { session_id } => CoreResponse::Approvals {
                approvals: self.approvals(&session_id)?,
            },
            CoreRequest::Approval {
                session_id,
                approval_id,
            } => CoreResponse::Approval {
                approval: self.approval(&session_id, &approval_id)?,
            },
            CoreRequest::DecideApproval {
                session_id,
                approval_id,
                decision,
                actor,
            } => CoreResponse::Approval {
                approval: self.decide_approval(&session_id, &approval_id, decision, &actor)?,
            },
            CoreRequest::Reconcile {
                session_id,
                reconciliation,
            } => snapshot(self.reconcile(&session_id, reconciliation)?),
        })
    }
}
