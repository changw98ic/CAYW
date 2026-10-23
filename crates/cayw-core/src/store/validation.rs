use super::*;
use cayw_ai::{AssistantEvent, SystemMessage, ToolDefinition};

use std::collections::BTreeMap;

impl ProtocolPayload for Value {
    fn json_values(&self) -> Vec<&Value> {
        vec![self]
    }
    fn json_values_mut(&mut self) -> Vec<&mut Value> {
        vec![self]
    }
}
impl<T: ProtocolPayload> ProtocolPayload for Option<T> {
    fn json_values(&self) -> Vec<&Value> {
        self.iter().flat_map(ProtocolPayload::json_values).collect()
    }
    fn json_values_mut(&mut self) -> Vec<&mut Value> {
        self.iter_mut()
            .flat_map(ProtocolPayload::json_values_mut)
            .collect()
    }
}
impl<T: ProtocolPayload> ProtocolPayload for Vec<T> {
    fn json_values(&self) -> Vec<&Value> {
        self.iter().flat_map(ProtocolPayload::json_values).collect()
    }
    fn json_values_mut(&mut self) -> Vec<&mut Value> {
        self.iter_mut()
            .flat_map(ProtocolPayload::json_values_mut)
            .collect()
    }
}
impl<T: ProtocolPayload> ProtocolPayload for Box<T> {
    fn json_values(&self) -> Vec<&Value> {
        self.as_ref().json_values()
    }
    fn json_values_mut(&mut self) -> Vec<&mut Value> {
        self.as_mut().json_values_mut()
    }
}

impl<T: ProtocolPayload> ProtocolPayload for BTreeMap<String, T> {
    fn json_values(&self) -> Vec<&Value> {
        self.values()
            .flat_map(ProtocolPayload::json_values)
            .collect()
    }
    fn json_values_mut(&mut self) -> Vec<&mut Value> {
        self.values_mut()
            .flat_map(ProtocolPayload::json_values_mut)
            .collect()
    }
}

macro_rules! scalar_payloads {
    ($($ty:ty),+ $(,)?) => {
        $(impl ProtocolPayload for $ty {
            fn json_values(&self) -> Vec<&Value> { vec![] }
            fn json_values_mut(&mut self) -> Vec<&mut Value> { vec![] }
        })+
    };
}
scalar_payloads!(
    (),
    bool,
    char,
    String,
    &str,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    f32,
    f64
);

macro_rules! json_fields {
    ($ty:ty, $($field:ident),+ $(,)?) => {
        impl ProtocolPayload for $ty {
            fn json_values(&self) -> Vec<&Value> {
                let mut values = vec![];
                $(values.extend(self.$field.json_values());)+
                values
            }
            fn json_values_mut(&mut self) -> Vec<&mut Value> {
                let mut values = vec![];
                $(values.extend(self.$field.json_values_mut());)+
                values
            }
        }
    };
}
macro_rules! json_variants {
    ($ty:ty, $($pattern:pat => [$($field:ident),*]),+ $(,)?) => {
        impl ProtocolPayload for $ty {
            fn json_values(&self) -> Vec<&Value> {
                match self {
                    $($pattern => {
                        #[allow(unused_mut)]
                        let mut values = vec![];
                        $(values.extend($field.json_values());)*
                        values
                    }),+
                }
            }
            fn json_values_mut(&mut self) -> Vec<&mut Value> {
                match self {
                    $($pattern => {
                        #[allow(unused_mut)]
                        let mut values = vec![];
                        $(values.extend($field.json_values_mut());)*
                        values
                    }),+
                }
            }
        }
    };
}

json_fields!(ToolCall, arguments);
json_fields!(ToolDefinition, parameters);
json_fields!(SystemMessage, tools_added);
json_fields!(AssistantMessage, content, usage_fields);
json_fields!(ToolResult, content, details, structured_content);
json_fields!(ToolResultMessage, content, details, structured_content);
json_fields!(crate::ToolReconciliationOrigin, call);
json_fields!(Reconciliation, expected, outcome);
json_fields!(CallRecord, call, result);
json_fields!(ExecutionRecord, terminal);
json_fields!(ExecutionWaitRecord, arguments);
json_fields!(
    CapabilityCallRecord,
    arguments,
    authorization,
    outcome,
    external_provenance
);
json_fields!(StoredCall, record);
json_fields!(
    StoredSession,
    lifecycle,
    history,
    calls,
    executions,
    execution_waits,
    capabilities
);
json_fields!(Lifecycle, open_message, assistant);
json_fields!(EventRecord, event);
json_fields!(
    SessionSnapshot,
    messages,
    unresolved_calls,
    executions,
    unresolved_capabilities
);
json_fields!(CapabilityAuthorization, external_provenance);
json_variants!(CoreResponse,
    CoreResponse::Session { snapshot } => [snapshot],
    CoreResponse::Sessions { sessions } => [sessions],
    CoreResponse::Events { events } => [events],
    CoreResponse::Event { event } => [event],
    CoreResponse::Execution { execution } => [execution],
    CoreResponse::Capability { call } => [call],
    CoreResponse::Attempt { .. } | CoreResponse::Approval { .. }
        | CoreResponse::Approvals { .. } | CoreResponse::Acknowledged
        | CoreResponse::Error { .. } => [],
);
json_variants!(HistoryEntry,
    HistoryEntry::Message { message } => [message],
    HistoryEntry::ToolSlot { .. } => [],
);
json_variants!(CoreEvent,
    CoreEvent::SessionCreated { .. } | CoreEvent::AttemptStarted { .. }
        | CoreEvent::ToolStarted { .. } | CoreEvent::ToolOriginBound { .. }
        | CoreEvent::AttemptFinished { .. }
        | CoreEvent::ApprovalRequested { .. } | CoreEvent::ApprovalDecided { .. } => [],
    CoreEvent::SessionForked { messages, .. } => [messages],
    CoreEvent::Agent { event } => [event],
    CoreEvent::ExecutionStarted { execution } | CoreEvent::ExecutionYielded { execution }
        | CoreEvent::ExecutionFinished { execution } => [execution],
    CoreEvent::ExecutionWaitRegistered { binding } => [binding],
    CoreEvent::CapabilityPlanned { call } | CoreEvent::CapabilityStarted { call }
        | CoreEvent::CapabilitySettled { call } => [call],
    CoreEvent::CapabilityReconciled { reconciliation, call } => [reconciliation, call],
    CoreEvent::AttemptRecovered { executions, capabilities, .. } => [executions, capabilities],
    CoreEvent::Reconciled { reconciliation } => [reconciliation],
    CoreEvent::Host { data, .. } => [data],
);
json_variants!(CoreRequest,
    CoreRequest::AppendHostEvent { data, .. } | CoreRequest::AppendControlEvent { data, .. } => [data],
    CoreRequest::AppendAgentEvent { event, .. } => [event],
    CoreRequest::RegisterExecutionWait { arguments, .. } => [arguments],
    CoreRequest::PlanCapability { input, .. } => [input],
    CoreRequest::StartCapability { authorization, .. } => [authorization],
    CoreRequest::SettleCapability { outcome, .. } => [outcome],
    CoreRequest::FinishExecution { outcome, .. } => [outcome],
    CoreRequest::ReconcileCapability { reconciliation, .. } => [reconciliation],
    CoreRequest::Reconcile { reconciliation, .. } => [reconciliation],
    CoreRequest::CreateSession { .. } | CoreRequest::ForkSession { .. }
        | CoreRequest::ListSessions | CoreRequest::Snapshot { .. } | CoreRequest::Events { .. }
        | CoreRequest::BeginAttempt { .. } | CoreRequest::ValidateLease { .. }
        | CoreRequest::BindToolOrigin { .. } | CoreRequest::MarkToolStarted { .. }
        | CoreRequest::BeginExecution { .. }
        | CoreRequest::YieldExecution { .. } | CoreRequest::FinishAttempt { .. }
        | CoreRequest::Recover { .. } | CoreRequest::RequestApproval { .. }
        | CoreRequest::Approvals { .. } | CoreRequest::Approval { .. }
        | CoreRequest::DecideApproval { .. } => [],
);
json_fields!(CapabilityInput, arguments);
json_fields!(CapabilityReconciliation, outcome, external_provenance);
json_variants!(Content,
    Content::ToolCall(call) => [call],
    Content::Text { .. } | Content::Image { .. } | Content::Thinking { .. } => [],
);
json_variants!(Message,
    Message::System(system) => [system],
    Message::User { content, .. } => [content],
    Message::Assistant(message) => [message],
    Message::ToolResult(result) => [result],
    Message::Custom { data, .. } => [data],
);
json_variants!(AssistantEvent,
    AssistantEvent::Start { message } | AssistantEvent::Done { message }
        | AssistantEvent::Error { message } => [message],
    AssistantEvent::UsageUpdate { partial }
        | AssistantEvent::TextStart { partial, .. } | AssistantEvent::TextDelta { partial, .. }
        | AssistantEvent::TextEnd { partial, .. } | AssistantEvent::ThinkingStart { partial, .. }
        | AssistantEvent::ThinkingDelta { partial, .. } | AssistantEvent::ThinkingEnd { partial, .. }
        | AssistantEvent::ToolCallStart { partial, .. } | AssistantEvent::ToolCallDelta { partial, .. }
        | AssistantEvent::ToolCallEnd { partial, .. } => [partial],
);
json_variants!(AgentEvent,
    AgentEvent::AgentStart | AgentEvent::TurnStart => [],
    AgentEvent::AgentEnd { messages } => [messages],
    AgentEvent::TurnEnd { message, tool_results } => [message, tool_results],
    AgentEvent::MessageStart { message } | AgentEvent::MessageEnd { message } => [message],
    AgentEvent::MessageUpdate { message, event } => [message, event],
    AgentEvent::ToolExecutionStart { call } => [call],
    AgentEvent::ToolExecutionUpdate { call, result }
        | AgentEvent::ToolExecutionEnd { call, result, .. } => [call, result],
);
json_variants!(ReconciledOutcome,
    ReconciledOutcome::Completed { result } => [result],
    ReconciledOutcome::NotExecuted { .. } => [],
);
json_variants!(CapabilityOutcome,
    CapabilityOutcome::Completed { result } => [result],
    CapabilityOutcome::Failed { .. } | CapabilityOutcome::NotExecuted { .. }
        | CapabilityOutcome::OutcomeUnknown { .. } => [],
);
json_variants!(ExecutionOutcome,
    ExecutionOutcome::Completed { result } => [result],
    ExecutionOutcome::Failed { .. } | ExecutionOutcome::Cancelled { .. }
        | ExecutionOutcome::Interrupted { .. } => [],
);

pub(super) fn check_values(input: &impl ProtocolPayload) -> Result<(), CoreError> {
    for value in input.json_values() {
        bounded_value(value)?;
    }
    Ok(())
}

/// Public APIs own some inputs. On rejection, normal serde_json::Value Drop
/// would still recurse through arbitrarily deep input and could abort a worker.
pub(crate) fn checked_input<T: ProtocolPayload>(mut input: T) -> Result<T, CoreError> {
    if let Err(error) = check_values(&input) {
        for value in input.json_values_mut() {
            drop_value(std::mem::take(value));
        }
        return Err(error);
    }
    Ok(input)
}

fn drop_value(value: Value) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Array(values) => pending.extend(values),
            Value::Object(values) => pending.extend(values.into_values()),
            _ => {}
        }
    }
}

/// Read using the same typed decoder as snapshot/events before committing. This
/// catches envelope nesting too, without changing serde's recursion protection.
pub(super) fn encode_readable<T: Serialize + serde::de::DeserializeOwned + ProtocolPayload>(
    input: &T,
) -> Result<String, CoreError> {
    check_values(input)?;
    let json = serde_json::to_string(input)?;
    let _: T = serde_json::from_str(&json)?;
    Ok(json)
}
