use crate::{RequestError, Result, types::*};
use cayw_ai::{ArgumentValidator, ToolDefinition};
use cayw_host::{Host, HostConfig, Profile, TerminalLimits};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Identity of the actual workspace, configured process and terminal profiles,
/// Git writer identities and switches, frozen-read mode, budgets, and upstream
/// permission policy. Rotate permission_policy_id when an external service,
/// Device authority, or upstream permission policy changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostIdentity {
    workspace: String,
    permission_policy_id: String,
    configuration_fingerprint: String,
}
impl HostIdentity {
    /// Bind the exact already-open Host directory used for execution. The
    /// supplied configuration must be the one used to construct this Host.
    pub fn from_host(host: &Host, config: &HostConfig, permission_policy_id: &str) -> Result<Self> {
        identifier(permission_policy_id)?;
        let workspace = host.workspace_identity().map_err(|_| {
            RequestError::InvalidInput("opened Host workspace identity is unavailable".into())
        })?;
        if workspace.is_empty() || workspace.len() > 4096 {
            return Err(RequestError::InvalidInput(
                "invalid Host workspace identity".into(),
            ));
        }
        let profiles: BTreeMap<_, _> = config
            .profiles
            .iter()
            .map(|(id, profile)| (id, profile_json(profile)))
            .collect();
        // Terminal profiles are a separate execution allowlist. Both every
        // selected profile and the manager-wide limits change what an admitted
        // call may execute, so both belong to the authority identity.
        let terminal_handoffs = config.terminal_handoffs.as_ref().map(|terminal| {
            let profiles: BTreeMap<_, _> = terminal
                .profiles
                .iter()
                .map(|(id, profile)| (id, profile_json(profile)))
                .collect();
            json!({
                "profiles":profiles,
                "limits":terminal_limits(terminal.limits),
            })
        });
        let git_writes = config.git_writes.as_ref().map(|policy| {
            json!({
                "identity_name":policy.identity_name,
                "identity_email":policy.identity_email,
                "allow_stage":policy.allow_stage,
                "allow_commit":policy.allow_commit,
                "allow_branch":policy.allow_branch,
                "allow_worktree":policy.allow_worktree,
            })
        });
        let data = config.data.as_ref().map(|limits| json!({
            "input_bytes":limits.input_bytes,"output_bytes":limits.output_bytes,
            "max_depth":limits.max_depth,"max_nodes":limits.max_nodes,"max_rows":limits.max_rows,
            "max_diff_work":limits.max_diff_work,
        }));
        // An opaque syntax validator is identified by permission_policy_id, as
        // are injected permission gates and external service implementations.
        let workspace_limits = config.workspace.as_ref().map(|limits| json!({
            "max_file_bytes":limits.max_file_bytes,"max_batch_bytes":limits.max_batch_bytes,
            "max_batch_operations":limits.max_batch_operations,"max_batches":limits.max_batches,
            "max_tree_entries":limits.max_tree_entries,"max_depth":limits.max_depth,
            "max_argument_bytes":limits.max_argument_bytes,"max_output_bytes":limits.max_output_bytes,
            "max_archive_bytes":limits.max_archive_bytes,"max_archive_entries":limits.max_archive_entries,
            "max_archive_expanded_bytes":limits.max_archive_expanded_bytes,
            "max_compression_ratio":limits.max_compression_ratio,"timeout_ns":limits.timeout.as_nanos().to_string(),
        }));
        let configuration = json!({
            "frozen_reads":config.frozen_reads,
            "profiles":profiles,"git_program":config.git_program,
            "git_writes":git_writes,"terminal_handoffs":terminal_handoffs,
            "external":config.external.is_some(),
            "data":data,"workspace_limits":workspace_limits,
            "max_argument_bytes":config.max_argument_bytes,"max_result_bytes":config.max_result_bytes,
            "max_total_bytes":config.max_total_bytes,"max_calls":config.max_calls,
            "max_file_bytes":config.max_file_bytes,"max_list_entries":config.max_list_entries,
            "call_timeout_ns":config.call_timeout.as_nanos().to_string(),
            "process_timeout_ns":config.process_timeout.as_nanos().to_string(),
            "max_process_output_bytes":config.max_process_output_bytes,
        });
        let bytes = bounded_json(&configuration, CATALOG_BYTES)?;
        Ok(Self {
            workspace,
            permission_policy_id: permission_policy_id.into(),
            configuration_fingerprint: digest(&bytes),
        })
    }
    pub fn fingerprint(&self) -> String {
        digest(&serde_json::to_vec(self).expect("host identity serializes"))
    }
}

/// An exact, already-supported call selected by trusted host code. Permission
/// binding identifies the underlying host/Device policy; it is not a permission
/// by itself and cannot extend a process program or argument allowlist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationScope {
    pub id: String,
    pub capability: String,
    pub arguments: Value,
    pub permission_binding: String,
}
impl OperationScope {
    pub fn exact(
        id: impl Into<String>,
        capability: impl Into<String>,
        arguments: Value,
        permission_binding: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            capability: capability.into(),
            arguments: canonical(&arguments),
            permission_binding: permission_binding.into(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BoundOperation {
    pub scope: OperationScope,
    pub schema: Value,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CatalogSnapshot {
    pub host: HostIdentity,
    pub operations: Vec<BoundOperation>,
}
impl CatalogSnapshot {
    pub fn fingerprint(&self) -> String {
        digest(
            &serde_json::to_vec(&canonical(
                &serde_json::to_value(self).expect("catalog serializes"),
            ))
            .expect("catalog serializes"),
        )
    }
    pub fn exact(&self, input: &RequestInput) -> Option<&BoundOperation> {
        self.operations.iter().find(|op| {
            op.scope.id == input.operation_id
                && op.scope.capability == input.capability
                && op.scope.arguments == input.arguments
        })
    }
}

/// Bounded snapshot of selected operations and their actual advertised schemas.
/// Call bind() only with definitions from the actual configured Host. RequestHost
/// does this itself, using one unchanged HostConfig for identity and execution.
#[derive(Debug, Clone)]
pub struct OperationCatalog {
    pub(crate) snapshot: CatalogSnapshot,
}
impl OperationCatalog {
    pub fn bind(
        host: HostIdentity,
        definitions: &[ToolDefinition],
        scopes: Vec<OperationScope>,
    ) -> Result<Self> {
        if scopes.is_empty() || scopes.len() > 64 || definitions.len() > 128 {
            return Err(RequestError::InvalidInput(
                "catalog requires 1..64 selected operations and at most 128 definitions".into(),
            ));
        }
        let mut catalog = BTreeMap::new();
        for definition in definitions {
            identifier(&definition.name)?;
            bounded_json(&definition.parameters, ARG_BYTES)?;
            let validator = ArgumentValidator::new(&definition.parameters).map_err(|_| {
                RequestError::InvalidInput("invalid or externally loading host schema".into())
            })?;
            if catalog
                .insert(&definition.name, (definition, validator))
                .is_some()
            {
                return Err(RequestError::InvalidInput(
                    "duplicate host definition".into(),
                ));
            }
        }
        let mut operations = Vec::with_capacity(scopes.len());
        let mut ids = std::collections::BTreeSet::new();
        for mut scope in scopes {
            identifier(&scope.id)?;
            identifier(&scope.capability)?;
            identifier(&scope.permission_binding)?;
            argument_bounds(&scope.arguments)?;
            scope.arguments = canonical(&scope.arguments);
            if !ids.insert(scope.id.clone()) {
                return Err(RequestError::InvalidInput("duplicate operation ID".into()));
            }
            let (definition, validator) = catalog.get(&scope.capability).ok_or_else(|| {
                RequestError::InvalidInput(
                    "operation is absent from the configured Host catalog".into(),
                )
            })?;
            validator.validate(&scope.arguments).map_err(|_| {
                RequestError::InvalidInput("operation arguments do not match Host schema".into())
            })?;
            operations.push(BoundOperation {
                scope,
                schema: canonical(&definition.parameters),
            });
        }
        operations.sort_by(|a, b| a.scope.id.cmp(&b.scope.id));
        let snapshot = CatalogSnapshot { host, operations };
        bounded_json(&snapshot, CATALOG_BYTES)?;
        Ok(Self { snapshot })
    }
    pub fn fingerprint(&self) -> String {
        self.snapshot.fingerprint()
    }
    /// Protection is by capability name, including arguments outside a selected
    /// exact scope. Those arguments must fail admission rather than fall through
    /// to an ordinary upstream permission.
    pub fn requires_request(&self, capability: &str) -> bool {
        self.snapshot
            .operations
            .iter()
            .any(|operation| operation.scope.capability == capability)
    }
    pub fn operations(&self) -> Vec<OperationScope> {
        self.snapshot
            .operations
            .iter()
            .map(|op| op.scope.clone())
            .collect()
    }
    pub(crate) fn matches(&self, snapshot: &CatalogSnapshot) -> bool {
        self.snapshot == *snapshot
    }
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Every field that can change which executable, arguments, environment, I/O
/// mode, time, or output budget a selected profile uses.
fn profile_json(profile: &Profile) -> Value {
    json!({
        "kind":format!("{:?}", profile.kind),
        "program":profile.program,
        "args":profile.args,
        "env":profile.env,
        "timeout_ns":profile.timeout.as_nanos().to_string(),
        "max_output_bytes":profile.max_output_bytes,
        "allow_filter":profile.allow_filter,
        "io":match profile.io {
            cayw_host::ProcessIo::Pipes => json!({"mode":"pipes"}),
            cayw_host::ProcessIo::Pty { columns, rows } => json!({"mode":"pty","columns":columns,"rows":rows}),
        },
    })
}

fn terminal_limits(limits: TerminalLimits) -> Value {
    json!({
        "max_sessions":limits.max_sessions,
        "max_sessions_total":limits.max_sessions_total,
        "max_input_bytes_per_write":limits.max_input_bytes_per_write,
        "max_input_bytes_per_session":limits.max_input_bytes_per_session,
        "max_output_bytes_per_session":limits.max_output_bytes_per_session,
        "max_read_bytes":limits.max_read_bytes,
        "max_wait_ns":limits.max_wait.as_nanos().to_string(),
    })
}
