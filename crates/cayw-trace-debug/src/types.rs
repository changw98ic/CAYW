use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const TRACE_SCHEMA: &str = "cayw.trace.v1";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub id: String,
    pub label: String,
    pub module: String,
    pub source: String,
    #[serde(default)]
    pub params: Value,
    /// Optional JSON pointer into the incoming output; empty means the whole input.
    #[serde(default)]
    pub input_pointer: String,
    pub implementation: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphPlan {
    pub id: String,
    pub label: String,
    pub entry: String,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub input: Value,
    pub initial_state: Value,
    pub context: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeResult {
    pub output: Value,
    pub state: Value,
    #[serde(default)]
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    Completed,
    Failed,
    Blocked,
    OutcomeUnknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub node_id: String,
    pub status: Status,
    pub input: Value,
    pub state_before: Value,
    pub output: Option<Value>,
    pub state_after: Option<Value>,
    pub branch: Option<String>,
    pub error: Option<String>,
    pub started_at_ms: u64,
    pub elapsed_ms: f64,
    pub evidence_cursor: Option<u64>,
    pub reused: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gap {
    pub node_id: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evaluation {
    pub id: String,
    pub version: u64,
    /// observed = actual Runtime; replay = full-entry local Luau; recompute = from edited node.
    pub mode: String,
    pub status: Status,
    pub steps: Vec<Step>,
    pub output: Option<Value>,
    pub gaps: Vec<Gap>,
    pub elapsed_ms: f64,
    pub saved_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Revision {
    pub version: u64,
    pub parent: Option<u64>,
    pub changed_node: Option<String>,
    pub plan: GraphPlan,
    pub created_at_ms: u64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedCall {
    pub execution_id: String,
    pub sequence: u64,
    pub name: String,
    pub arguments: Value,
    pub outcome: Option<cayw_core::CapabilityOutcome>,
    pub started_at_ms: Option<u64>,
    pub settled_at_ms: Option<u64>,
    pub evidence_cursor: u64,
    pub node_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub session_id: String,
    pub attempt_id: String,
    pub task_id: String,
    pub source_hash: String,
    pub parent_call_id: String,
    pub lease: Option<cayw_core::AttemptLease>,
    pub revisions: Vec<Revision>,
    pub evaluations: Vec<Evaluation>,
    pub calls: Vec<RecordedCall>,
    pub gaps: Vec<Gap>,
    pub active_node: Option<String>,
    pub trace_error: Option<String>,
    pub runtime_status: Status,
}

impl Project {
    pub fn version(&self) -> u64 {
        self.revisions.last().map_or(0, |revision| revision.version)
    }
    pub fn latest(&self) -> Option<&Evaluation> {
        self.evaluations.last()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub trace_to_store_ms: Vec<f64>,
    pub error_to_summary_ms: Vec<f64>,
    pub save_to_result_ms: Vec<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionCapture {
    pub after: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub schema: String,
    pub sequence: u64,
    pub sessions: BTreeMap<String, SessionCapture>,
    pub projects: BTreeMap<String, Project>,
    #[serde(default)]
    pub events: Vec<cayw_core::EventRecord>,
    pub metrics: Metrics,
}
impl Default for Document {
    fn default() -> Self {
        Self {
            schema: TRACE_SCHEMA.into(),
            sequence: 0,
            sessions: BTreeMap::new(),
            projects: BTreeMap::new(),
            events: Vec::new(),
            metrics: Metrics::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub expected_version: u64,
    pub node_id: String,
    pub source: String,
    pub params: Value,
    #[serde(default)]
    pub input_pointer: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Undo {
    pub expected_version: u64,
    pub target_version: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NodeTrace {
    Manifest {
        plan: GraphPlan,
    },
    Started {
        node_id: String,
        input: Value,
        state_before: Value,
    },
    Completed {
        node_id: String,
        result: NodeResult,
        elapsed_ms: f64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceEnvelope {
    pub schema: String,
    pub trace: NodeTrace,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
