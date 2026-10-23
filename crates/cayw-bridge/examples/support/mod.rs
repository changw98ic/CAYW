//! Test/example utilities using the reusable host-owned MCP stdio source.
//! The fixed Skill below is only a Registry fixture, not file-loader evidence.
#![allow(dead_code)]
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_bridge::*;
use cayw_codemode::{CapabilityError, CapabilityHost, CodeMode, Limits as CodeLimits};
use cayw_host::{AuditSink, ExternalHost, Host, HostConfig, PermissionGate};
use cayw_sources::{StdioConfig, StdioControl, StdioSnapshot, StdioSource};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

pub const PROTOCOL: &str = cayw_sources::MCP_PROTOCOL;
pub const SKILL: &str = "# Local addition\nUse the add tool with two integer arguments.\n";
static NEXT: AtomicU64 = AtomicU64::new(1);

pub struct Counter {
    control: Arc<OnceLock<StdioControl>>,
    value: fn(&StdioSnapshot) -> u64,
}
impl Counter {
    pub fn load(&self, _: Ordering) -> usize {
        self.control
            .get()
            .map_or(0, |control| (self.value)(&control.snapshot()) as usize)
    }
}
pub struct Stats {
    control: Arc<OnceLock<StdioControl>>,
    pub spawned: Counter,
    pub connections: Counter,
    pub tool_calls: Counter,
    pub skill_reads: AtomicUsize,
    pub resource_reads: Counter,
    pub cancellations: Counter,
    pub progress: Counter,
    pub ignored_late: Counter,
    pub disconnected: Counter,
}
impl Default for Stats {
    fn default() -> Self {
        let control = Arc::new(OnceLock::new());
        let counter = |value| Counter {
            control: control.clone(),
            value,
        };
        Self {
            spawned: counter(|state| state.spawned),
            connections: counter(|state| state.spawned),
            tool_calls: counter(|state| state.tool_calls),
            resource_reads: counter(|state| state.resource_reads),
            cancellations: counter(|state| state.cancellations),
            progress: counter(|state| state.progress),
            ignored_late: counter(|state| state.ignored_late),
            disconnected: counter(|state| state.disconnected),
            skill_reads: AtomicUsize::new(0),
            control,
        }
    }
}
/// Shared wrappers deliberately test Registry duplicate physical ownership.
/// Ordinary host applications move StdioSource::into_parts() directly to Registry.
#[derive(Clone)]
pub struct LocalClient {
    source: Arc<dyn SourceHandle>,
    manifest: Manifest,
    control: StdioControl,
    stats: Arc<Stats>,
}
impl LocalClient {
    pub async fn spawn(
        program: &Path,
        directory: &Path,
        stats: Arc<Stats>,
    ) -> Result<Self, CapabilityError> {
        let prepared = StdioSource::connect(
            StdioConfig::new(program, directory),
            CancellationToken::new(),
        )
        .await?;
        let control = prepared.control();
        if stats.control.set(control.clone()).is_err() {
            control.close();
            control.wait_closed().await;
            return Err(CapabilityError::new(
                "fixture statistics already own a source",
            ));
        }
        let (mut manifest, source) = prepared.into_parts();
        manifest.skills.push(SkillSpec {
            name: "addition".into(),
            description: "Fixed test application Skill handle".into(),
            content_sha256: content_sha256(SKILL),
        });
        Ok(Self {
            source: Arc::from(source),
            manifest,
            control,
            stats,
        })
    }
    pub fn close(&self) {
        self.control.close();
    }
    pub async fn wait_closed(&self) {
        self.control.wait_closed().await;
    }
    pub async fn manifest(&self) -> Result<Manifest, CapabilityError> {
        Ok(self.manifest.clone())
    }
    pub fn handle(&self) -> LocalHandle {
        LocalHandle {
            client: self.clone(),
        }
    }
}
pub struct LocalHandle {
    client: LocalClient,
}
#[async_trait]
impl SourceHandle for LocalHandle {
    fn ownership_key(&self) -> &str {
        self.client.source.ownership_key()
    }
    async fn request(
        &self,
        request: SourceRequest,
        cancel: CancellationToken,
    ) -> Result<SourceReply, CapabilityError> {
        if let SourceOperation::ReadSkill { name } = &request.operation {
            if name != "addition" || cancel.is_cancelled() {
                return Err(CapabilityError::new(
                    "fixture skill unavailable or cancelled",
                ));
            }
            self.client.stats.skill_reads.fetch_add(1, Ordering::AcqRel);
            return Ok(SourceReply {
                key: request.key,
                value: json!(SKILL),
            });
        }
        self.client.source.request(request, cancel).await
    }
    fn disconnect(&self) {
        self.client.source.disconnect();
    }
}

#[derive(Default)]
pub struct Policy {
    pub hidden: Mutex<BTreeSet<(u8, String)>>,
    pub denied: Mutex<BTreeSet<String>>,
    pub calls: Mutex<Vec<AccessRequest>>,
}
#[async_trait]
impl RegistryPolicy for Policy {
    fn visible(&self, kind: Visibility, _: &Provenance, name: &str) -> bool {
        !self
            .hidden
            .lock()
            .unwrap()
            .contains(&(kind as u8, name.into()))
    }
    async fn authorize(
        &self,
        request: &AccessRequest,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError> {
        self.calls.lock().unwrap().push(request.clone());
        if cancel.is_cancelled()
            || self
                .denied
                .lock()
                .unwrap()
                .contains(&format!("{:?}", request.operation))
        {
            Err(CapabilityError::new("registry policy denied"))
        } else {
            Ok(())
        }
    }
}
#[derive(Default)]
pub struct Gate {
    pub denied: Mutex<BTreeSet<String>>,
    pub calls: Mutex<Vec<String>>,
}
#[async_trait]
impl PermissionGate for Gate {
    async fn authorize(
        &self,
        name: &str,
        _: &Value,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError> {
        self.calls.lock().unwrap().push(name.into());
        if cancel.is_cancelled() || self.denied.lock().unwrap().contains(name) {
            Err(CapabilityError::new("host gate denied"))
        } else {
            Ok(())
        }
    }
}
#[derive(Default)]
pub struct Audit {
    pub records: Mutex<Vec<String>>,
}
#[async_trait]
impl AuditSink for Audit {
    async fn started(&self, capability: &str, _: &Value) -> Result<String, CapabilityError> {
        let mut records = self.records.lock().unwrap();
        let id = records.len().to_string();
        records.push(format!("start:{capability}"));
        Ok(id)
    }
    async fn settled(
        &self,
        id: &str,
        result: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        self.records.lock().unwrap().push(format!(
            "settle:{id}:{}",
            match result {
                Ok(_) => "success",
                Err(error) if error.outcome_unknown => "unknown",
                Err(_) => "failure",
            }
        ));
        Ok(())
    }
    fn dropped(&self, id: &str) -> Result<(), CapabilityError> {
        self.records
            .lock()
            .unwrap()
            .push(format!("drop:{id}:unknown"));
        Ok(())
    }
}
/// Test-only storage; production Runtime supplies its durable per-call journal.
#[derive(Default)]
pub struct FixtureJournal {
    pub records: Mutex<Vec<Value>>,
}
#[async_trait]
impl DispatchJournal for FixtureJournal {
    async fn started(&self, record: &DispatchRecord) -> Result<(), CapabilityError> {
        self.records
            .lock()
            .unwrap()
            .push(json!({"kind":"started","record":record}));
        Ok(())
    }
    async fn settled(
        &self,
        record: &DispatchRecord,
        outcome: &DispatchOutcome,
    ) -> Result<(), CapabilityError> {
        self.records
            .lock()
            .unwrap()
            .push(json!({"kind":"settled","record":record,"outcome":outcome}));
        Ok(())
    }
    fn dropped(
        &self,
        record: &DispatchRecord,
        state: DispatchDropState,
    ) -> Result<(), CapabilityError> {
        self.records
            .lock()
            .unwrap()
            .push(json!({"kind":"dropped","record":record,"state":state}));
        Ok(())
    }
}
pub fn invocation(sequence: u64) -> InvocationContext {
    InvocationContext {
        session_id: "fixture-session".into(),
        task_id: "fixture-task".into(),
        attempt_id: "fixture-attempt".into(),
        execution_id: "fixture-execution".into(),
        sequence,
    }
}
pub struct FixtureRuntime {
    pub bridge: Bridge,
    next: AtomicU64,
    pub journal: Arc<FixtureJournal>,
}
impl FixtureRuntime {
    pub fn new(bridge: Bridge) -> Self {
        Self {
            bridge,
            next: AtomicU64::new(1),
            journal: Arc::default(),
        }
    }
    pub async fn call(
        &self,
        capability: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        let scope = self.bridge.scoped(
            capability,
            invocation(self.next.fetch_add(1, Ordering::Relaxed)),
            self.journal.clone(),
        )?;
        scope.call(capability, args, cancel).await
    }
}
#[async_trait]
impl ExternalHost for FixtureRuntime {
    async fn call(
        &self,
        capability: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.call(capability, args, cancel).await
    }
}
pub fn consumer(registry: &Registry) -> FixtureRuntime {
    FixtureRuntime::new(registry.consumer())
}
pub fn host(root: &Path, bridge: FixtureRuntime, gate: Arc<Gate>, audit: Arc<Audit>) -> Arc<Host> {
    Arc::new(
        Host::new(
            root,
            gate,
            audit,
            HostConfig {
                external: Some(Arc::new(bridge)),
                ..HostConfig::default()
            },
        )
        .unwrap(),
    )
}
pub fn mode(host: Arc<Host>) -> CodeMode {
    CodeMode::new(host, CodeLimits::default()).unwrap()
}
pub fn identity(version: &str) -> SourceIdentity {
    SourceIdentity {
        id: "local".into(),
        version: version.into(),
        origin: "application:explicit-local-stdio-fixture".into(),
    }
}
pub struct Workspace(pub PathBuf);
impl Workspace {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cayw-bridge-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
pub async fn host_call(host: &Host, name: &str, args: Value) -> Result<Value, CapabilityError> {
    host.call(name, args, CancellationToken::new()).await
}
