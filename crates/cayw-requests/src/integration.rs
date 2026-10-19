use crate::{
    AuthorizationReceipt, HostIdentity, OperationCatalog, OperationScope, RequestError,
    RequestStore, RequestTools, Result, TaskIdentity,
};
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{AuditSink, ExternalHost, Host, HostConfig, PermissionGate};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};

// An upstream-only capability has no fabricated request/grant/receipt.
enum Admission {
    Protected(Box<AuthorizationReceipt>),
    Upstream,
    // Retain the refusal while Host checks safety, validates and charges its
    // shared budget. Returning early would bypass those upstream invariants.
    Rejected(CapabilityError),
}

/// Adds request checks for selected capabilities while retaining the existing
/// application PermissionGate for EVERY call. Selected capability names cannot
/// fall through to an ordinary grant by changing arguments or a profile ID.
/// Each permission wait rechecks the same receipt, never a replacement grant.
/// A gate alone cannot recheck an audit wait after authorize() has returned.
/// Use RequestHost for the complete permission/audit scope, or pin an explicit
/// receipt in the host's immutable call-start boundary as Runtime does.
pub struct RequestGate {
    store: Arc<RequestStore>,
    identity: TaskIdentity,
    catalog: OperationCatalog,
    upstream: Arc<dyn PermissionGate>,
    admission: Option<Admission>,
}
impl RequestGate {
    pub fn new(
        store: Arc<RequestStore>,
        identity: TaskIdentity,
        catalog: OperationCatalog,
        upstream: Arc<dyn PermissionGate>,
    ) -> Result<Self> {
        store.check_task(&identity, &catalog)?;
        Ok(Self {
            store,
            identity,
            catalog,
            upstream,
            admission: None,
        })
    }
    fn for_call(&self, name: &str, arguments: &Value) -> Self {
        let admission = if self.catalog.requires_request(name) {
            self.store
                .admit(&self.identity, &self.catalog, name, arguments)
                .map(|receipt| Admission::Protected(Box::new(receipt)))
        } else {
            self.store
                .check_task(&self.identity, &self.catalog)
                .map(|()| Admission::Upstream)
        }
        .unwrap_or_else(|error| Admission::Rejected(permission_error(error)));
        Self {
            store: self.store.clone(),
            identity: self.identity.clone(),
            catalog: self.catalog.clone(),
            upstream: self.upstream.clone(),
            admission: Some(admission),
        }
    }
    fn check(&self, name: &str, arguments: &Value) -> std::result::Result<(), CapabilityError> {
        match &self.admission {
            Some(Admission::Protected(receipt))
                if receipt.scope().capability == name
                    && receipt.scope().arguments == *arguments =>
            {
                self.store
                    .check_receipt(receipt, &self.catalog)
                    .map_err(permission_error)
            }
            Some(Admission::Upstream) if !self.catalog.requires_request(name) => self
                .store
                .check_task(&self.identity, &self.catalog)
                .map_err(permission_error),
            Some(Admission::Rejected(error)) => Err(error.clone()),
            _ => Err(permission_error(RequestError::Stale)),
        }
    }
}
fn permission_error(_: RequestError) -> CapabilityError {
    CapabilityError::new("task request permission is missing, stale, expired, or unavailable")
}
#[async_trait]
impl PermissionGate for RequestGate {
    async fn authorize(
        &self,
        capability: &str,
        arguments: &Value,
        cancel: CancellationToken,
    ) -> std::result::Result<(), CapabilityError> {
        if cancel.is_cancelled() {
            return Err(CapabilityError::new("request permission cancelled"));
        }
        let scoped;
        let gate = if self.admission.is_some() {
            self
        } else {
            scoped = self.for_call(capability, arguments);
            &scoped
        };
        gate.check(capability, arguments)?;
        self.upstream
            .authorize(capability, arguments, cancel.clone())
            .await?;
        if cancel.is_cancelled() {
            return Err(CapabilityError::new("request permission cancelled"));
        }
        gate.check(capability, arguments)
    }
}

// Host awaits durable audit start after permission. The same per-call gate
// retains the admitted receipt across BOTH waits. Settlement never requires a
// live grant: Closing/revoke/end must not discard an already-started result.
struct RequestAudit {
    gate: Arc<RequestGate>,
    upstream: Arc<dyn AuditSink>,
}
#[async_trait]
impl AuditSink for RequestAudit {
    async fn started(
        &self,
        capability: &str,
        arguments: &Value,
    ) -> std::result::Result<String, CapabilityError> {
        self.gate.check(capability, arguments)?;
        let id = self.upstream.started(capability, arguments).await?;
        if let Err(denied) = self.gate.check(capability, arguments) {
            if self
                .upstream
                .settled(&id, &Err(denied.clone()))
                .await
                .is_err()
            {
                return Err(CapabilityError::unknown(
                    "request permission changed after audit start and settlement failed",
                ));
            }
            return Err(denied);
        }
        Ok(id)
    }
    async fn settled(
        &self,
        id: &str,
        result: &std::result::Result<Value, CapabilityError>,
    ) -> std::result::Result<(), CapabilityError> {
        self.upstream.settled(id, result).await
    }
    fn dropped(&self, id: &str) -> std::result::Result<(), CapabilityError> {
        self.upstream.dropped(id)
    }
}

// A fixed service over these already-bound tools. Host::call_service owns
// admission, permission, audit, cancellation, budgets and safety for metadata.
struct RequestService<'a>(&'a RequestTools);
#[async_trait]
impl ExternalHost for RequestService<'_> {
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> std::result::Result<Value, CapabilityError> {
        self.0.call(name, arguments, cancel).await
    }
}

/// Host-selected wiring only. No serialized configuration or implicit loader.
/// Keep the profile executables and environment trusted and rotate policy ID if
/// an injected Device authority, external service, or permission policy changes.
pub struct RequestHostConfig {
    pub root: PathBuf,
    pub host: HostConfig,
    pub permission_policy_id: String,
    pub operations: Vec<OperationScope>,
    pub gate: Arc<dyn PermissionGate>,
    pub audit: Arc<dyn AuditSink>,
}
impl RequestHostConfig {
    /// Capture schemas from an actual, validated Host with this exact config.
    /// Construction opens the workspace/runner but never calls a capability.
    pub fn catalog(&self) -> Result<OperationCatalog> {
        let host = Host::new(
            &self.root,
            self.gate.clone(),
            self.audit.clone(),
            self.host.clone(),
        )
        .map_err(|_| {
            RequestError::InvalidInput("configured Host could not be constructed".into())
        })?;
        let identity = HostIdentity::from_host(&host, &self.host, &self.permission_policy_id)?;
        OperationCatalog::bind(identity, &host.capabilities(), self.operations.clone())
    }
}

/// Runnable integration over one actual Host. Each capability future gets an
/// immutable permission/audit scope sharing that Host's budgets, execution lane,
/// safety, root and process runner. Runtime cancels/drains this same future.
pub struct RequestHost {
    host: Host,
    tools: RequestTools,
    catalog: OperationCatalog,
    gate: Arc<RequestGate>,
    audit: Arc<dyn AuditSink>,
}
impl RequestHost {
    pub fn new(
        store: Arc<RequestStore>,
        identity: TaskIdentity,
        config: RequestHostConfig,
    ) -> Result<Self> {
        let host = Host::new(
            &config.root,
            config.gate.clone(),
            config.audit.clone(),
            config.host.clone(),
        )
        .map_err(|_| {
            RequestError::InvalidInput("configured Host could not be constructed".into())
        })?;
        let host_identity =
            HostIdentity::from_host(&host, &config.host, &config.permission_policy_id)?;
        let catalog =
            OperationCatalog::bind(host_identity, &host.capabilities(), config.operations)?;
        let gate = Arc::new(RequestGate::new(
            store.clone(),
            identity.clone(),
            catalog.clone(),
            config.gate,
        )?);
        let tools = RequestTools::new(store, identity, catalog.clone())?;
        Ok(Self {
            host,
            tools,
            catalog,
            gate,
            audit: config.audit,
        })
    }
    pub fn operation_catalog(&self) -> &OperationCatalog {
        &self.catalog
    }
    pub fn outcome_unknown(&self) -> bool {
        self.host.outcome_unknown()
    }
    pub fn audit_failure(&self) -> Option<String> {
        self.host.audit_failure()
    }
    /// Close process admission on the SAME Host used by every call scope.
    pub fn close_processes(&self) {
        self.host.close_processes();
    }
    pub fn active_processes(&self) -> usize {
        self.host.active_processes()
    }
    /// Keep task reservations until cleanup of this Host's processes finishes.
    pub async fn drain_processes(&self) -> std::result::Result<(), cayw_host::DeviceError> {
        self.host.drain_processes().await
    }
}
#[async_trait]
impl CapabilityHost for RequestHost {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        let mut definitions = self.host.capabilities();
        definitions.extend(self.tools.definitions());
        definitions
    }
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> std::result::Result<Value, CapabilityError> {
        let gate = Arc::new(self.gate.for_call(name, &arguments));
        let audit = Arc::new(RequestAudit {
            gate: gate.clone(),
            upstream: self.audit.clone(),
        });
        let scope = self.host.scoped(gate, audit)?;
        if let Some(definition) = RequestTools::tool_definitions()
            .into_iter()
            .find(|definition| definition.name == name)
        {
            scope
                .call_service(&definition, arguments, cancel, &RequestService(&self.tools))
                .await
        } else {
            scope.call(name, arguments, cancel).await
        }
    }
}
