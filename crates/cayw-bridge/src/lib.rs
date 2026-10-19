//! Host-owned source registry over injected application handles.
//!
//! Create one [`Registry`], register already-created handles, and retain a
//! [`Bridge`] consumer with its immutable [`BindingSnapshot`]. The trusted Runtime
//! calls [`Bridge::scoped`] once per capability invocation, supplying an immutable
//! [`InvocationContext`] and its [`DispatchJournal`]. The returned
//! `Arc<dyn cayw_host::ExternalHost>` is bound to one capability and one use.
//! Runtime/Host must supply this scoped object for that invocation while retaining
//! their own permission gate and capability audit. There is no mutable current-call
//! slot and no unjournaled execution entry point.
//!
//! Before every actual source dispatch the journal durably records full context,
//! [`RequestKey`], provenance and arguments/digest. Success, known failure,
//! NotExecuted and Unknown are persisted before returning. Dropping a prepared
//! future synchronously records NotExecuted; dropping a dispatched future records
//! Unknown. A failed terminal/drop journal write fences this registry. Existing
//! Host audit alone does not preserve source identity or replace this journal.
//!
//! Each new registry has a unique incarnation, included in provenance, request
//! keys and binding snapshots. Cloning a consumer shares the same owner and pins;
//! reconnecting requires a fresh consumer. Unknown effects survive reconnect and
//! revoke and have no reset/replay API. The Runtime must reconcile durable records
//! before explicitly creating a replacement registry.
//!
//! Metadata is immutable and bounded. Visibility and authorization are separate;
//! discovery never grants execution. Qualified Skill names fit Host's 256-byte
//! input limit. The library creates no connections, loaders, subprocesses,
//! filesystem handles, network requests or retries. Source identity/version/origin
//! are application assertions, not proof of trust.
//!
//! The `local_mcp` example and tests use one explicitly spawned local MCP
//! **2025-11-25 stdio** fixture and fixed application-supplied Skill content. They
//! exercise initialize/initialized, tool and resource discovery/read, cancellation,
//! late replies and generation fencing. This fixture is not a general transport:
//! HTTP/SSE, auth, paging, subscriptions, server requests and MCP 2026-07-28 are
//! outside its scope. Example persistence is test-only. macOS is locally tested;
//! Linux and Windows execution remain unverified.
//!
//! Protocol references:
//! <https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle>,
//! <https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/cancellation>,
//! <https://modelcontextprotocol.io/specification/2025-11-25/server/tools>,
//! <https://modelcontextprotocol.io/specification/2025-11-25/server/resources>.

mod bounds;
mod model;
mod registry;

pub use bounds::content_sha256;
pub use model::*;
pub use registry::Registry;

use bounds::{OwnedJson, failure, name};
use cayw_ai::CancellationToken;
use cayw_codemode::CapabilityError;
use cayw_host::ExternalHost;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::time::Instant;

/// Immutable catalog consumer. Execution requires a fresh per-invocation scope.
#[derive(Clone)]
pub struct Bridge {
    registry: Registry,
    bindings: BindingSnapshot,
}

struct ScopedBridge {
    bridge: Bridge,
    capability: String,
    context: InvocationContext,
    journal: Arc<dyn DispatchJournal>,
    used: AtomicBool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Discover {
    server: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Describe {
    server: String,
    tool: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Invoke {
    server: String,
    tool: String,
    arguments: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillRead {
    name: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    query: String,
    path: Option<String>,
    limit: Option<u64>,
}

fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, CapabilityError> {
    serde_json::from_value(value).map_err(|_| failure("invalid bridge capability arguments"))
}
impl Bridge {
    /// Exact pinned identity even if live sources subsequently change or disappear.
    pub fn binding_snapshot(&self) -> BindingSnapshot {
        self.bindings.clone()
    }

    /// Trusted Runtime creates a new scope for each capability call. The context
    /// must come from its durable session/task/attempt/execution records, never
    /// from model arguments. Clone the returned Arc only to share that one call.
    pub fn scoped(
        &self,
        capability: &str,
        context: InvocationContext,
        journal: Arc<dyn DispatchJournal>,
    ) -> Result<Arc<dyn ExternalHost>, CapabilityError> {
        if !matches!(
            capability,
            "mcp.discover"
                | "mcp.describe"
                | "mcp.call"
                | "skills.list"
                | "skills.read"
                | "search.query"
        ) {
            return Err(failure("unknown external capability"));
        }
        if [
            &context.session_id,
            &context.task_id,
            &context.attempt_id,
            &context.execution_id,
        ]
        .iter()
        .any(|id| id.is_empty() || id.len() > 256 || id.chars().any(char::is_control))
            || context.sequence == 0
        {
            return Err(failure("invalid runtime invocation context"));
        }
        Ok(Arc::new(ScopedBridge {
            bridge: self.clone(),
            capability: capability.into(),
            context,
            journal,
            used: AtomicBool::new(false),
        }))
    }
    fn generation(&self, id: &str) -> Result<u64, CapabilityError> {
        self.bindings
            .sources
            .get(id)
            .map(|p| p.generation)
            .ok_or_else(|| failure("source not present in consumer snapshot"))
    }
    async fn collect(
        &self,
        scope: &ScopedBridge,
        server: Option<&str>,
        operation: Operation,
        field: &str,
        cancel: CancellationToken,
        deadline: Instant,
    ) -> Result<Value, CapabilityError> {
        let ids: Vec<_> = match server {
            Some(id) => {
                self.generation(id)?;
                vec![id]
            }
            None => self.bindings.sources.keys().map(String::as_str).collect(),
        };
        let mut result = json!({field:[]});
        for id in &ids {
            let mut response = self
                .registry
                .operation(
                    id,
                    self.generation(id)?,
                    operation,
                    "",
                    json!({}),
                    cancel.clone(),
                    deadline,
                    scope,
                )
                .await?;
            let incoming = response[field]
                .as_array_mut()
                .expect("registry list envelope");
            result[field]
                .as_array_mut()
                .expect("aggregate list envelope")
                .append(incoming);
            self.registry.bound_result(&result)?;
        }
        if cancel.is_cancelled() || Instant::now() >= deadline {
            return Err(failure("metadata collection cancelled or timed out"));
        }
        self.registry.validate_pins(
            ids.into_iter()
                .map(|id| (id, self.bindings.sources[id].generation)),
        )?;
        Ok(result)
    }
}

// Implement the async-trait ABI explicitly: the owned-JSON guard must exist when
// call() returns, including when its boxed future is never polled.
impl ExternalHost for ScopedBridge {
    fn call<'life0, 'life1, 'async_trait>(
        &'life0 self,
        capability: &'life1 str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CapabilityError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        let arguments = OwnedJson(arguments);
        let deadline = Instant::now() + self.bridge.registry.operation_timeout();
        let admitted = self.capability == capability && !self.used.swap(true, Ordering::AcqRel);
        Box::pin(async move {
            if !admitted {
                return Err(failure(
                    "invocation scope capability mismatch or already consumed",
                ));
            }
            self.call_owned(capability, arguments, cancel, deadline)
                .await
        })
    }
}
impl ScopedBridge {
    async fn run(
        &self,
        id: &str,
        operation: Operation,
        target: &str,
        arguments: Value,
        cancel: CancellationToken,
        deadline: Instant,
    ) -> Result<Value, CapabilityError> {
        self.bridge
            .registry
            .operation(
                id,
                self.bridge.generation(id)?,
                operation,
                target,
                arguments,
                cancel,
                deadline,
                self,
            )
            .await
    }
    async fn call_owned(
        &self,
        capability: &str,
        mut arguments: OwnedJson,
        cancel: CancellationToken,
        deadline: Instant,
    ) -> Result<Value, CapabilityError> {
        self.bridge.registry.bound_arguments(&arguments)?;
        if !arguments.is_object() {
            return Err(failure("bridge arguments must be an object"));
        }
        if cancel.is_cancelled() || Instant::now() >= deadline {
            return Err(failure(
                "bridge call cancelled or timed out before admission",
            ));
        }
        let arguments = arguments.take();
        match capability {
            "mcp.discover" => {
                let args: Discover = parse(arguments)?;
                if args.server.as_ref().is_some_and(|s| !name(s)) {
                    return Err(failure("invalid source name"));
                }
                self.bridge
                    .collect(
                        self,
                        args.server.as_deref(),
                        Operation::Discover,
                        "tools",
                        cancel,
                        deadline,
                    )
                    .await
            }
            "mcp.describe" => {
                let args: Describe = parse(arguments)?;
                if !name(&args.server) || !name(&args.tool) {
                    return Err(failure("invalid source or tool name"));
                }
                self.run(
                    &args.server,
                    Operation::Describe,
                    &args.tool,
                    json!({}),
                    cancel,
                    deadline,
                )
                .await
            }
            "mcp.call" => {
                let args: Invoke = parse(arguments)?;
                if !name(&args.server) || !name(&args.tool) {
                    return Err(failure("invalid source or tool name"));
                }
                if !args.arguments.is_object() {
                    return Err(failure("tool arguments must be an object"));
                }
                self.run(
                    &args.server,
                    Operation::Invoke,
                    &args.tool,
                    args.arguments,
                    cancel,
                    deadline,
                )
                .await
            }
            "skills.list" => {
                let _: Empty = parse(arguments)?;
                self.bridge
                    .collect(
                        self,
                        None,
                        Operation::ListSkills,
                        "skills",
                        cancel,
                        deadline,
                    )
                    .await
            }
            "skills.read" => {
                let args: SkillRead = parse(arguments)?;
                let (id, target) = args
                    .name
                    .split_once('/')
                    .ok_or_else(|| failure("skill name must be source_id/name"))?;
                if args.name.len() > 256 || !name(id) || !name(target) {
                    return Err(failure("invalid qualified skill name"));
                }
                self.run(
                    id,
                    Operation::ReadSkill,
                    target,
                    json!({}),
                    cancel,
                    deadline,
                )
                .await
            }
            "search.query" => {
                let args: Search = parse(arguments.clone())?;
                if args.query.is_empty()
                    || args.query.len() > 8192
                    || args
                        .path
                        .as_ref()
                        .is_some_and(|p| p.is_empty() || p.len() > 4096 || p.contains('\0'))
                    || args.limit.is_some_and(|n| n == 0 || n > 1000)
                {
                    return Err(failure("invalid search bounds"));
                }
                let pins: BTreeMap<_, _> = self
                    .bridge
                    .bindings
                    .sources
                    .iter()
                    .map(|(id, p)| (id.clone(), p.generation))
                    .collect();
                let id = self.bridge.registry.search_owner(&pins)?;
                self.run(
                    &id,
                    Operation::Search,
                    "search.query",
                    arguments,
                    cancel,
                    deadline,
                )
                .await
            }
            _ => Err(failure("unknown external capability")),
        }
    }
}
