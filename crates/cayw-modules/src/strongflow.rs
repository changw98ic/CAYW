//! Workflow decisions and serializable binding data. A runtime must persist these
//! decisions and connect unload notifications; this crate does not own sessions.

use crate::*;
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub id: String,
    pub depends_on: BTreeSet<String>,
    pub acceptance_checks: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPlan {
    pub id: String,
    pub steps: Vec<PlanStep>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Running,
    Passed,
    Failed,
}

/// Only the host can submit evidence through `StrongFlow::submit_evidence`.
/// No module method can accept evidence or mark a step passed. These fields
/// identify a real run; the host must verify their provenance before submission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionEvidence {
    pub source_hash: String,
    pub build_id: String,
    pub configuration: Value,
    pub platform: String,
    pub input: Value,
    pub command: Vec<String>,
    pub actual_result: Value,
    pub checks: BTreeSet<String>,
    pub passed: bool,
}

impl ExecutionEvidence {
    fn validate(&self) -> Result<(), ModuleError> {
        if self.source_hash.len() != 64
            || !self
                .source_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || self.build_id.trim().is_empty()
            || self.platform.trim().is_empty()
            || !self.configuration.is_object()
            || self.input.is_null()
            || !self.actual_result.is_object()
            || self
                .actual_result
                .as_object()
                .is_none_or(|object| object.is_empty())
            || self.command.is_empty()
            || self.command.iter().any(|arg| arg.trim().is_empty())
            || self.checks.is_empty()
            || self.checks.iter().any(|check| !valid_name(check))
        {
            return Err(ModuleError::InvalidArguments(
                "execution evidence is incomplete".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSnapshot {
    pub plan: WorkflowPlan,
    pub statuses: BTreeMap<String, StepStatus>,
    pub evidence: BTreeMap<String, Vec<ExecutionEvidence>>,
}

impl WorkflowSnapshot {
    /// Validate persisted state without running a step or treating it as ready.
    pub fn validate(&self) -> Result<(), ModuleError> {
        self.plan.validate()?;
        let ids: BTreeSet<_> = self.plan.steps.iter().map(|step| step.id.clone()).collect();
        if self.statuses.keys().cloned().collect::<BTreeSet<_>>() != ids
            || self.evidence.keys().any(|id| !ids.contains(id))
        {
            return Err(ModuleError::InvalidArguments(
                "workflow state has missing or unknown steps".into(),
            ));
        }
        for step in &self.plan.steps {
            let status = self.statuses[&step.id];
            let records = self.evidence.get(&step.id);
            if records.is_some_and(Vec::is_empty)
                || (status == StepStatus::Pending && records.is_some())
                || (status != StepStatus::Pending
                    && step
                        .depends_on
                        .iter()
                        .any(|id| self.statuses[id] != StepStatus::Passed))
            {
                return Err(ModuleError::InvalidArguments(
                    "workflow state violates step dependencies".into(),
                ));
            }
            let mut derived = if status == StepStatus::Pending {
                StepStatus::Pending
            } else {
                StepStatus::Running
            };
            let mut covered = BTreeSet::new();
            if let Some(records) = records {
                for record in records {
                    record.validate()?;
                    if derived != StepStatus::Running
                        || !record.checks.is_subset(&step.acceptance_checks)
                        || records.first().is_some_and(|first| {
                            first.source_hash != record.source_hash
                                || first.build_id != record.build_id
                                || first.configuration != record.configuration
                        })
                    {
                        return Err(ModuleError::InvalidArguments(
                            "workflow evidence is inconsistent or follows a terminal result".into(),
                        ));
                    }
                    covered.extend(record.checks.iter().cloned());
                    derived = if !record.passed {
                        StepStatus::Failed
                    } else if step.acceptance_checks.is_subset(&covered) {
                        StepStatus::Passed
                    } else {
                        StepStatus::Running
                    };
                }
            }
            if status != derived {
                return Err(ModuleError::InvalidArguments(
                    "step status does not match execution evidence".into(),
                ));
            }
        }
        Ok(())
    }
}

pub const STRONGFLOW_STATE_VERSION: u32 = 1;

/// Trusted-host persistence data. The host authenticates its source, stores it
/// atomically with its session binding, and verifies evidence provenance. No
/// credentials, files or configuration are loaded implicitly during restoration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrongFlowState {
    pub format_version: u32,
    pub module_manifest: ModuleManifest,
    pub workflows: BTreeMap<String, WorkflowSnapshot>,
}

impl StrongFlowState {
    pub fn validate(&self) -> Result<(), ModuleError> {
        if self.format_version != STRONGFLOW_STATE_VERSION
            || self.module_manifest != StrongFlow::new().manifest
        {
            return Err(ModuleError::InvalidArguments(
                "unsupported StrongFlow state version or module manifest".into(),
            ));
        }
        for (id, workflow) in &self.workflows {
            if id != &workflow.plan.id {
                return Err(ModuleError::InvalidArguments(
                    "workflow key differs from plan id".into(),
                ));
            }
            workflow.validate()?;
        }
        Ok(())
    }

    fn preserves(&self, previous: &Self) -> bool {
        self.format_version == previous.format_version
            && self.module_manifest == previous.module_manifest
            && previous.workflows.iter().all(|(id, old)| {
                self.workflows.get(id).is_some_and(|current| {
                    current.plan == old.plan
                        && old.evidence.iter().all(|(step, records)| {
                            current
                                .evidence
                                .get(step)
                                .is_some_and(|next| next.starts_with(records))
                        })
                        && old.statuses.iter().all(|(step, status)| {
                            let next = current.statuses[step];
                            match status {
                                StepStatus::Pending => true,
                                StepStatus::Running => next != StepStatus::Pending,
                                StepStatus::Passed | StepStatus::Failed => next == *status,
                            }
                        })
                })
            })
    }
}

impl WorkflowPlan {
    pub fn validate(&self) -> Result<(), ModuleError> {
        if !valid_name(&self.id) || self.steps.is_empty() {
            return Err(ModuleError::InvalidArguments(
                "plan needs an id and steps".into(),
            ));
        }
        let mut ids = BTreeSet::new();
        for step in &self.steps {
            if !valid_name(&step.id)
                || !ids.insert(step.id.clone())
                || step.acceptance_checks.is_empty()
                || step
                    .acceptance_checks
                    .iter()
                    .any(|check| !valid_name(check))
            {
                return Err(ModuleError::InvalidArguments(
                    "invalid step or acceptance checks".into(),
                ));
            }
        }
        for step in &self.steps {
            if !step.depends_on.is_subset(&ids) {
                return Err(ModuleError::InvalidArguments("unknown dependency".into()));
            }
        }
        let mut visited = BTreeSet::new();
        loop {
            let before = visited.len();
            for step in &self.steps {
                if step.depends_on.is_subset(&visited) {
                    visited.insert(step.id.clone());
                }
            }
            if visited.len() == ids.len() {
                return Ok(());
            }
            if visited.len() == before {
                return Err(ModuleError::InvalidArguments(
                    "workflow dependency cycle".into(),
                ));
            }
        }
    }
}

pub struct StrongFlow {
    manifest: ModuleManifest,
    workflows: Mutex<BTreeMap<String, WorkflowSnapshot>>,
}

impl Default for StrongFlow {
    fn default() -> Self {
        Self::new()
    }
}

impl StrongFlow {
    pub fn new() -> Self {
        let parameters = serde_json::json!({
            "type":"object", "properties":{"workflow_id":{"type":"string"}},
            "required":["workflow_id"], "additionalProperties":false,
        });
        Self {
            manifest: ModuleManifest {
                name: "strongflow".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                protocol_version: MODULE_PROTOCOL_VERSION,
                kind: ModuleKind::Native,
                capabilities: ["strongflow.describe", "strongflow.ready"]
                    .into_iter()
                    .map(|name| ToolDefinition {
                        name: name.into(),
                        description: "Read host-created workflow state".into(),
                        parameters: parameters.clone(),
                    })
                    .collect(),
                permissions: BTreeMap::new(),
            },
            workflows: Mutex::new(BTreeMap::new()),
        }
    }

    /// Export all original plans, statuses and execution evidence. The caller
    /// owns durable storage and must commit this with its binding checkpoint.
    pub fn export_state(&self) -> StrongFlowState {
        StrongFlowState {
            format_version: STRONGFLOW_STATE_VERSION,
            module_manifest: self.manifest.clone(),
            workflows: self
                .workflows
                .lock()
                .expect("workflow state poisoned")
                .clone(),
        }
    }

    /// Restore only a complete, validated state for this module version. Running
    /// steps remain running: the host must reconcile effects instead of replaying.
    pub fn restore(state: StrongFlowState) -> Result<Self, ModuleError> {
        state.validate()?;
        Ok(Self {
            manifest: state.module_manifest,
            workflows: Mutex::new(state.workflows),
        })
    }

    pub fn create_plan(&self, plan: WorkflowPlan) -> Result<(), ModuleError> {
        plan.validate()?;
        let mut workflows = self.workflows.lock().expect("workflow state poisoned");
        if workflows.contains_key(&plan.id) {
            return Err(ModuleError::Conflict(plan.id));
        }
        let statuses = plan
            .steps
            .iter()
            .map(|step| (step.id.clone(), StepStatus::Pending))
            .collect();
        workflows.insert(
            plan.id.clone(),
            WorkflowSnapshot {
                plan,
                statuses,
                evidence: BTreeMap::new(),
            },
        );
        Ok(())
    }

    pub fn describe(&self, workflow_id: &str) -> Result<WorkflowSnapshot, ModuleError> {
        self.workflows
            .lock()
            .expect("workflow state poisoned")
            .get(workflow_id)
            .cloned()
            .ok_or_else(|| ModuleError::NotFound(workflow_id.into()))
    }

    pub fn ready(&self, workflow_id: &str) -> Result<Vec<String>, ModuleError> {
        let workflow = self.describe(workflow_id)?;
        Ok(workflow
            .plan
            .steps
            .iter()
            .filter(|step| {
                workflow.statuses[&step.id] == StepStatus::Pending
                    && step
                        .depends_on
                        .iter()
                        .all(|id| workflow.statuses[id] == StepStatus::Passed)
            })
            .map(|step| step.id.clone())
            .collect())
    }

    pub fn start_step(&self, workflow_id: &str, step_id: &str) -> Result<(), ModuleError> {
        let mut workflows = self.workflows.lock().expect("workflow state poisoned");
        let workflow = workflows
            .get_mut(workflow_id)
            .ok_or_else(|| ModuleError::NotFound(workflow_id.into()))?;
        let step = workflow
            .plan
            .steps
            .iter()
            .find(|step| step.id == step_id)
            .ok_or_else(|| ModuleError::NotFound(step_id.into()))?;
        if workflow.statuses[step_id] != StepStatus::Pending
            || !step
                .depends_on
                .iter()
                .all(|id| workflow.statuses[id] == StepStatus::Passed)
        {
            return Err(ModuleError::Failed("step is not ready".into()));
        }
        workflow
            .statuses
            .insert(step_id.into(), StepStatus::Running);
        Ok(())
    }

    /// Accumulate host-verified checks. A failed run makes the step terminally
    /// failed; only complete coverage of all required checks can pass it.
    pub fn submit_evidence(
        &self,
        workflow_id: &str,
        step_id: &str,
        evidence: ExecutionEvidence,
    ) -> Result<StepStatus, ModuleError> {
        evidence.validate()?;
        let mut workflows = self.workflows.lock().expect("workflow state poisoned");
        let workflow = workflows
            .get_mut(workflow_id)
            .ok_or_else(|| ModuleError::NotFound(workflow_id.into()))?;
        let step = workflow
            .plan
            .steps
            .iter()
            .find(|step| step.id == step_id)
            .ok_or_else(|| ModuleError::NotFound(step_id.into()))?;
        if workflow.statuses[step_id] != StepStatus::Running
            || !evidence.checks.is_subset(&step.acceptance_checks)
        {
            return Err(ModuleError::Failed(
                "step must be running and evidence must name declared checks".into(),
            ));
        }
        let passed = evidence.passed;
        let records = workflow.evidence.entry(step_id.into()).or_default();
        if records.first().is_some_and(|first| {
            first.source_hash != evidence.source_hash
                || first.build_id != evidence.build_id
                || first.configuration != evidence.configuration
        }) {
            return Err(ModuleError::InvalidArguments(
                "evidence must refer to the same source, build and configuration".into(),
            ));
        }
        records.push(evidence);
        let covered: BTreeSet<String> = records
            .iter()
            .flat_map(|record| record.checks.iter().cloned())
            .collect();
        let status = if !passed {
            StepStatus::Failed
        } else if step.acceptance_checks.is_subset(&covered) {
            StepStatus::Passed
        } else {
            StepStatus::Running
        };
        workflow.statuses.insert(step_id.into(), status);
        Ok(status)
    }
}

#[async_trait]
impl Module for StrongFlow {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }
    fn strongflow_state(&self) -> Result<StrongFlowState, ModuleError> {
        Ok(self.export_state())
    }
    async fn call(
        &self,
        method: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ModuleError> {
        if cancel.is_cancelled() {
            return Err(ModuleError::Cancelled);
        }
        let workflow_id = arguments
            .get("workflow_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ModuleError::InvalidArguments("workflow_id is required".into()))?;
        match method {
            "strongflow.describe" => serde_json::to_value(self.describe(workflow_id)?),
            "strongflow.ready" => serde_json::to_value(self.ready(workflow_id)?),
            _ => return Err(ModuleError::Denied(method.into())),
        }
        .map_err(|error| ModuleError::Failed(error.to_string()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    Ordinary,
    StrongFlow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleBinding {
    pub name: String,
    pub version: String,
    pub registry_id: String,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionBinding {
    session_id: String,
    mode: SessionMode,
    bound_module_version: Option<ModuleBinding>,
    module_manifest: Option<ModuleManifest>,
    workflow_state: Option<StrongFlowState>,
    frozen_reason: Option<String>,
    source_session_id: Option<String>,
}

impl SessionBinding {
    pub fn ordinary(session_id: String) -> Result<Self, ModuleError> {
        if !valid_name(&session_id) {
            return Err(ModuleError::InvalidArguments("invalid session id".into()));
        }
        Ok(Self {
            session_id,
            mode: SessionMode::Ordinary,
            bound_module_version: None,
            module_manifest: None,
            workflow_state: None,
            frozen_reason: None,
            source_session_id: None,
        })
    }

    /// Create a new binding only. Before persisting or executing a new session,
    /// call `checkpoint_strongflow` against its loaded instance. Restored sessions
    /// retain this binding and use `resume_strongflow`, never this constructor.
    pub fn strongflow(session_id: String, module: &ModuleDescription) -> Result<Self, ModuleError> {
        if module.manifest.name != "strongflow"
            || module.generation == 0
            || uuid::Uuid::parse_str(&module.registry_id).is_err()
        {
            return Err(ModuleError::InvalidArguments(
                "expected a loaded StrongFlow module".into(),
            ));
        }
        let mut binding = Self::ordinary(session_id)?;
        binding.mode = SessionMode::StrongFlow;
        binding.bound_module_version = Some(ModuleBinding {
            name: module.manifest.name.clone(),
            version: module.manifest.version.clone(),
            registry_id: module.registry_id.clone(),
            generation: module.generation,
        });
        binding.module_manifest = Some(module.manifest.clone());
        binding.validate()?;
        Ok(binding)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub fn mode(&self) -> SessionMode {
        self.mode
    }
    pub fn bound_module(&self) -> Option<&ModuleBinding> {
        self.bound_module_version.as_ref()
    }
    pub fn frozen_reason(&self) -> Option<&str> {
        self.frozen_reason.as_deref()
    }
    pub fn source_session_id(&self) -> Option<&str> {
        self.source_session_id.as_deref()
    }

    pub fn workflow_state(&self) -> Option<&StrongFlowState> {
        self.workflow_state.as_ref()
    }

    /// Capture the live instance's full obligations for durable storage with the
    /// session. Existing plans and evidence can advance, but cannot disappear or
    /// be rewritten. This never creates a new binding or clears a frozen reason.
    pub fn checkpoint_strongflow(
        &mut self,
        registry: &ModuleRegistry,
    ) -> Result<StrongFlowState, ModuleError> {
        self.check_available(registry)?;
        let bound = self.bound_module_version.as_ref().ok_or_else(|| {
            ModuleError::Denied("ordinary session has no StrongFlow checkpoint".into())
        })?;
        let expected = ModuleDescription {
            manifest: self
                .module_manifest
                .clone()
                .expect("validated module manifest"),
            registry_id: bound.registry_id.clone(),
            generation: bound.generation,
        };
        let state = registry.strongflow_state(&expected)?;
        if self
            .workflow_state
            .as_ref()
            .is_some_and(|old| !state.preserves(old))
        {
            return Err(ModuleError::Unavailable(
                "checkpoint lost original plans or execution evidence".into(),
            ));
        }
        self.workflow_state = Some(state.clone());
        Ok(state)
    }

    /// Explicit trusted-host recovery after a reload or process restart. The host
    /// supplies the exact target returned by load, including its registry identity
    /// and generation. A previously captured full checkpoint is required; the new
    /// instance must have the identical manifest, plans, statuses and evidence.
    /// Persist the updated binding before allowing execution. No module response
    /// or user-confirmation boolean can authorize this host-only operation.
    pub fn resume_strongflow(
        &mut self,
        registry: &ModuleRegistry,
        target: &ModuleDescription,
    ) -> Result<(), ModuleError> {
        self.validate()?;
        if self.mode != SessionMode::StrongFlow || self.frozen_reason.is_none() {
            return Err(ModuleError::Denied(
                "recovery requires a frozen StrongFlow session".into(),
            ));
        }
        let previous = self.workflow_state.as_ref().ok_or_else(|| {
            ModuleError::Unavailable(
                "no original workflow checkpoint; recovery cannot discard obligations".into(),
            )
        })?;
        if self.module_manifest.as_ref() != Some(&target.manifest) {
            return Err(ModuleError::Unavailable(
                "recovery module manifest differs from original".into(),
            ));
        }
        let current = registry.strongflow_state(target)?;
        if current != *previous {
            return Err(ModuleError::Unavailable(
                "recovery must preserve every original plan, step status and evidence record"
                    .into(),
            ));
        }
        self.bound_module_version = Some(ModuleBinding {
            name: target.manifest.name.clone(),
            version: target.manifest.version.clone(),
            registry_id: target.registry_id.clone(),
            generation: target.generation,
        });
        self.frozen_reason = None;
        Ok(())
    }

    /// The runtime calls this before using a restored binding and after unload.
    /// Reload alone cannot thaw it; only explicit, validated recovery can.
    pub fn check_available(&mut self, registry: &ModuleRegistry) -> Result<(), ModuleError> {
        self.validate()?;
        if let Some(reason) = &self.frozen_reason {
            return Err(ModuleError::Unavailable(reason.clone()));
        }
        if let Some(bound) = &self.bound_module_version {
            let available = registry
                .check_available(&bound.name)
                .is_ok_and(|description| {
                    description.registry_id == bound.registry_id
                        && description.generation == bound.generation
                        && self.module_manifest.as_ref() == Some(&description.manifest)
                });
            if !available {
                let reason = format!(
                    "{} {} is unavailable; session remains frozen until explicit verified recovery or a confirmed ordinary fork",
                    bound.name, bound.version
                );
                self.frozen_reason = Some(reason.clone());
                return Err(ModuleError::Unavailable(reason));
            }
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ModuleError> {
        if !valid_name(&self.session_id)
            || self
                .source_session_id
                .as_ref()
                .is_some_and(|source| !valid_name(source) || source == &self.session_id)
            || match (&self.mode, &self.bound_module_version) {
                (SessionMode::Ordinary, None) => {
                    self.frozen_reason.is_some()
                        || self.module_manifest.is_some()
                        || self.workflow_state.is_some()
                }
                (SessionMode::StrongFlow, Some(bound)) => {
                    bound.name != "strongflow"
                        || bound.version.trim().is_empty()
                        || uuid::Uuid::parse_str(&bound.registry_id).is_err()
                        || bound.generation == 0
                        || self.module_manifest.as_ref().is_none_or(|manifest| {
                            manifest.name != bound.name
                                || manifest.version != bound.version
                                || crate::registry::validate_manifest(manifest).is_err()
                        })
                        || self.workflow_state.as_ref().is_some_and(|state| {
                            state.validate().is_err()
                                || Some(&state.module_manifest) != self.module_manifest.as_ref()
                        })
                }
                _ => true,
            }
        {
            return Err(ModuleError::InvalidArguments(
                "invalid session binding".into(),
            ));
        }
        Ok(())
    }

    /// This is a decision record. The runtime must atomically reserve the new ID
    /// and copy/reconcile persisted history before making a real session fork.
    pub fn request_ordinary_fork(
        &self,
        new_session_id: String,
        user_confirmed: bool,
    ) -> Result<ForkDecision, ModuleError> {
        self.validate()?;
        if self.mode != SessionMode::StrongFlow
            || !user_confirmed
            || new_session_id == self.session_id
        {
            return Err(ModuleError::Denied(
                "ordinary fork requires user confirmation and a distinct new session id".into(),
            ));
        }
        let mut target = Self::ordinary(new_session_id)?;
        target.source_session_id = Some(self.session_id.clone());
        Ok(ForkDecision {
            source: self.clone(),
            target,
            user_confirmed,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForkDecision {
    pub source: SessionBinding,
    pub target: SessionBinding,
    pub user_confirmed: bool,
}
