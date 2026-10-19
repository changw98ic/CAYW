//! Shared bytes retain independent original authority and cross-store terminals.
use crate::cells::CallScope;
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_cells::{
    BackendCompletion, BackendResponse, CellExecution, DependencySnapshot, ReadCandidate,
    ReadOrigin, ReadRole, ReadSource, ReadStamp,
};
use cayw_codemode::CapabilityError;
use cayw_host::{FrozenReadStamp, FrozenReadTicket};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Evidence {
    pub origin: ReadOrigin,
    pub lease: cayw_core::AttemptLease,
    pub key: cayw_core::CapabilityCallKey,
    pub task_id: String,
    pub name: String,
    pub arguments: Value,
    pub authorization: cayw_core::CapabilityAuthorization,
    pub grant: Option<crate::GrantRecord>,
    pub request: Option<Value>,
    pub audit_id: String,
    pub source: ReadOrigin,
    pub role: ReadRole,
    pub stamp: FrozenReadStamp,
    pub dependencies: DependencySnapshot,
    pub result_sha256: String,
}
struct Source<'a> {
    ticket: &'a mut FrozenReadTicket,
    scope: &'a CallScope,
}
#[async_trait]
impl ReadSource for Source<'_> {
    fn can_share(&self) -> bool {
        self.ticket.can_share()
    }
    async fn capture(&mut self, cancel: CancellationToken) -> Result<Value, CapabilityError> {
        self.scope.check_authority(&cancel)?;
        let captured = self.ticket.capture().await?;
        self.scope.check_authority(&cancel)?;
        Ok(captured.value().clone())
    }
}
pub(crate) async fn call(
    scope: &Arc<CallScope>,
    base: &cayw_host::Host,
    execution: &CellExecution,
    sequence: u64,
    arguments: Value,
    cancel: CancellationToken,
) -> Result<BackendResponse, CapabilityError> {
    let host = crate::bridge::scoped_host(base, scope)?;
    let child = cancel.child_token();
    let admission = host.admit_frozen_read(arguments.clone(), child.clone());
    let admitted = crate::requests::watch(scope, child.clone(), admission).await;
    let mut ticket = match admitted {
        Ok(ticket) => ticket,
        Err(error) => {
            let result = Err(error);
            scope.settle(&result)?;
            return Ok(BackendResponse::owned(
                result,
                None,
                Box::new(Completion::ordinary(scope.clone())),
            ));
        }
    };
    let stamp = ticket.stamp().clone();
    let origin = ReadOrigin {
        execution: execution.clone(),
        sequence,
    };
    let reads = scope.context.inner.reads.clone();
    let deadline = ticket.deadline();
    let mut source = Source {
        ticket: &mut ticket,
        scope,
    };
    let read = reads.read(
        origin.clone(),
        "fs.read",
        &arguments,
        ReadStamp {
            workspace: stamp.workspace.clone(),
            incarnation: stamp.incarnation.clone(),
            generation: stamp.generation,
            semantics: stamp.semantics.clone(),
        },
        &mut source,
        child.clone(),
    );
    let candidate=crate::requests::watch(scope,child.clone(),async {
        tokio::select! { biased;
            _=child.cancelled()=>Err(CapabilityError::new("original read cancelled")),
            _=tokio::time::sleep_until(deadline.into())=>Err(CapabilityError::new("frozen read deadline reached")),
            result=read=>result,
        }
    }).await;
    let (candidate, accepted) = match candidate {
        Ok(candidate) => {
            let result = scope
                .check_authority(&child)
                .and_then(|()| ticket.accept(&stamp, candidate.value().clone()));
            (Some(candidate), result)
        }
        Err(error) => (None, Err(error)),
    };
    if let (Some(candidate), Ok(value)) = (&candidate, &accepted) {
        let (authorization, grant) = scope.authorization_evidence()?;
        let evidence = Evidence {
            origin,
            lease: scope.context.lease.clone(),
            key: scope.key.clone(),
            task_id: scope.context.task.id.clone(),
            name: scope.name.clone(),
            arguments,
            authorization,
            grant,
            request: scope.request.evidence(),
            audit_id: ticket.audit_id().into(),
            source: candidate.source.clone(),
            role: candidate.role.clone(),
            stamp,
            dependencies: candidate.dependencies.clone(),
            result_sha256: cayw_cells::HostResult::value_hash(value.value()),
        };
        let data = serde_json::to_value(evidence)
            .map_err(|error| scope.context.persistence_failed(error))?;
        if serde_json::to_vec(&data)
            .map_err(|error| scope.context.persistence_failed(error))?
            .len()
            > cayw_core::MAX_METADATA_BYTES
        {
            return Err(scope
                .context
                .persistence_failed("shared read evidence exceeds canonical metadata limit"));
        }
        scope.context.host_event("runtime_shared_read", data)?;
    }
    let result = ticket.settle(accepted).await;
    let known = !result.as_ref().is_err_and(|error| error.outcome_unknown);
    let candidate = if known {
        candidate
    } else {
        drop(candidate);
        None
    };
    let rejection = result
        .as_ref()
        .err()
        .filter(|error| !error.outcome_unknown)
        .map(|error| error.message.clone());
    let dependencies = if result.is_ok() {
        candidate
            .as_ref()
            .map(|candidate| candidate.dependencies.clone())
    } else {
        None
    };
    scope.settle(&result)?;
    Ok(BackendResponse::owned(
        result,
        dependencies,
        Box::new(Completion {
            scope: scope.clone(),
            ticket: Some(ticket),
            candidate,
            rejection,
            acknowledged: false,
        }),
    ))
}
pub(crate) struct Completion {
    scope: Arc<CallScope>,
    ticket: Option<FrozenReadTicket>,
    candidate: Option<ReadCandidate>,
    rejection: Option<String>,
    acknowledged: bool,
}
impl Completion {
    pub(crate) fn ordinary(scope: Arc<CallScope>) -> Self {
        Self {
            scope,
            ticket: None,
            candidate: None,
            rejection: None,
            acknowledged: false,
        }
    }
}
impl BackendCompletion for Completion {
    fn acknowledge(mut self: Box<Self>) {
        if let Some(candidate) = self.candidate.take() {
            if let Some(reason) = &self.rejection {
                candidate.reject(reason);
            } else {
                candidate.acknowledge();
            }
        }
        if let Some(ticket) = self.ticket.take() {
            ticket.acknowledge();
        }
        self.acknowledged = true;
    }
}
impl Drop for Completion {
    fn drop(&mut self) {
        if !self.acknowledged {
            self.scope
                .context
                .inner
                .reads
                .freeze(&self.scope.context.lease.session_id);
            self.scope.context.persistence_failed(
                "original Cells acknowledgment was abandoned after Core settlement",
            );
        }
    }
}
