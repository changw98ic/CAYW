//! Host-owned MCP stdio lifecycle: on-demand discover, lazy start, idle recycle.
//!
//! Design reference: nicobailon/pi-mcp-adapter lifecycle modes. CAYW keeps Core
//! free of MCP clients: the host prepares a [`ManagedStdio`] and moves its single
//! [`cayw_bridge::SourceHandle`] into a Registry. Discovery metadata can stay
//! registered while the child process is stopped; the next tool/resource/prompt
//! call starts a fresh connection. Idle recycle never runs while a call is
//! in flight. Late replies from a recycled transport cannot settle after close.

use crate::stdio::{ServerInfo, StdioConfig, StdioControl, StdioSnapshot, StdioSource};
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_bridge::{Manifest, SourceHandle, SourceReply, SourceRequest};
use cayw_codemode::CapabilityError;
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Notify;

/// Per-server process residency policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LifecycleMode {
    /// Discover once, stop, reconnect on first use, stop after idle timeout.
    Lazy,
    /// Discover once, stop, reconnect on first use, then stay up while registered.
    LazyKeepAlive,
    /// Keep the connection from prepare until disconnect/revoke.
    Eager,
}

/// Trusted host launch + lifecycle settings. Never deserialized from model input.
#[derive(Debug, Clone)]
pub struct ServerSpec {
    pub id: String,
    pub version: String,
    pub origin: String,
    pub config: StdioConfig,
    pub lifecycle: LifecycleMode,
    /// Idle recycle timeout. `Duration::ZERO` disables recycle.
    pub idle_timeout: Duration,
}

/// Aggregated observations for one managed source across probe and live generations.
#[derive(Debug, Clone, Serialize)]
pub struct ManagedSnapshot {
    pub id: String,
    pub ownership_key: String,
    pub lifecycle: LifecycleMode,
    pub idle_timeout_ms: u64,
    pub connected: bool,
    pub closed: bool,
    pub connect_generation: u64,
    pub spawn_count: u64,
    pub recycle_count: u64,
    pub in_flight: u64,
    pub last_activity_ago_ms: Option<u64>,
    pub server_info: ServerInfo,
    pub live: Option<StdioSnapshot>,
}

struct Live {
    handle: Arc<dyn SourceHandle>,
    control: StdioControl,
    _server_info: ServerInfo,
}

struct Shared {
    id: String,
    ownership_key: String,
    config: StdioConfig,
    lifecycle: LifecycleMode,
    idle_timeout: Duration,
    closed: AtomicBool,
    notify: Notify,
    connect_lock: tokio::sync::Mutex<()>,
    state: Mutex<Inner>,
}

struct Inner {
    manifest: Manifest,
    server_info: ServerInfo,
    live: Option<Live>,
    connect_generation: u64,
    spawn_count: u64,
    recycle_count: u64,
    in_flight: u64,
    last_activity: Instant,
}

/// Prepared managed source. Consuming it transfers the only Registry handle.
pub struct ManagedStdio {
    shared: Arc<Shared>,
    manifest: Manifest,
    server_info: ServerInfo,
}

/// Observations and shutdown. Cannot issue tool/resource/prompt requests.
#[derive(Clone)]
pub struct ManagedControl {
    shared: Arc<Shared>,
}

impl ManagedControl {
    pub fn snapshot(&self) -> ManagedSnapshot {
        let state = lock(&self.shared.state);
        ManagedSnapshot {
            id: self.shared.id.clone(),
            ownership_key: self.shared.ownership_key.clone(),
            lifecycle: self.shared.lifecycle,
            idle_timeout_ms: self.shared.idle_timeout.as_millis() as u64,
            connected: state.live.is_some(),
            closed: self.shared.closed.load(Ordering::Acquire),
            connect_generation: state.connect_generation,
            spawn_count: state.spawn_count,
            recycle_count: state.recycle_count,
            in_flight: state.in_flight,
            last_activity_ago_ms: Some(
                Instant::now()
                    .saturating_duration_since(state.last_activity)
                    .as_millis() as u64,
            ),
            server_info: state.server_info.clone(),
            live: state.live.as_ref().map(|live| live.control.snapshot()),
        }
    }

    pub fn close(&self) {
        shutdown(&self.shared);
    }

    pub async fn wait_closed(&self) {
        loop {
            let notified = self.shared.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.shared.closed.load(Ordering::Acquire) {
                let done = {
                    let state = lock(&self.shared.state);
                    state.live.is_none() && state.in_flight == 0
                };
                if done {
                    return;
                }
            }
            notified.await;
        }
    }
}

impl ManagedStdio {
    /// Probe the server for a bounded catalog, then apply the lifecycle mode.
    ///
    /// `Lazy` / `LazyKeepAlive` stop the probe process before returning so the
    /// host can register metadata without keeping a resident child. `Eager`
    /// keeps the first connection.
    pub async fn prepare(
        spec: ServerSpec,
        cancel: CancellationToken,
    ) -> Result<Self, CapabilityError> {
        validate_spec(&spec)?;
        if cancel.is_cancelled() {
            return Err(failure("managed MCP prepare cancelled before launch"));
        }
        let ownership_key = format!("managed-{}", uuid::Uuid::new_v4());
        let source = StdioSource::connect(spec.config.clone(), cancel).await?;
        let manifest = source.manifest().clone();
        let server_info = source.server_info().clone();
        let control = source.control();
        let (_, probe_handle) = source.into_parts();
        let keep_live = matches!(spec.lifecycle, LifecycleMode::Eager);
        let live = if keep_live {
            Some(Live {
                handle: Arc::from(probe_handle),
                control,
                _server_info: server_info.clone(),
            })
        } else {
            probe_handle.disconnect();
            control.wait_closed().await;
            if !control.snapshot().reaped {
                return Err(failure(
                    "managed MCP probe cleanup unconfirmed; refusing lazy registration",
                ));
            }
            None
        };
        let shared = Arc::new(Shared {
            id: spec.id,
            ownership_key,
            config: spec.config,
            lifecycle: spec.lifecycle,
            idle_timeout: spec.idle_timeout,
            closed: AtomicBool::new(false),
            notify: Notify::new(),
            connect_lock: tokio::sync::Mutex::new(()),
            state: Mutex::new(Inner {
                manifest: manifest.clone(),
                server_info: server_info.clone(),
                live,
                connect_generation: u64::from(keep_live),
                spawn_count: 1,
                recycle_count: 0,
                in_flight: 0,
                last_activity: Instant::now(),
            }),
        });
        if matches!(spec.lifecycle, LifecycleMode::Lazy) && !spec.idle_timeout.is_zero() {
            spawn_idle_watch(shared.clone());
        }
        Ok(Self {
            shared,
            manifest,
            server_info,
        })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    pub fn control(&self) -> ManagedControl {
        ManagedControl {
            shared: self.shared.clone(),
        }
    }

    pub fn into_parts(self) -> (Manifest, Box<dyn SourceHandle>) {
        (
            self.manifest,
            Box::new(ManagedHandle {
                shared: self.shared,
            }),
        )
    }
}

struct ManagedHandle {
    shared: Arc<Shared>,
}

#[async_trait]
impl SourceHandle for ManagedHandle {
    fn ownership_key(&self) -> &str {
        &self.shared.ownership_key
    }

    async fn request(
        &self,
        request: SourceRequest,
        cancel: CancellationToken,
    ) -> Result<SourceReply, CapabilityError> {
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(failure("managed MCP source is closed"));
        }
        if cancel.is_cancelled() {
            return Err(failure("managed MCP request cancelled before connect"));
        }
        {
            let mut state = lock(&self.shared.state);
            state.in_flight = state.in_flight.saturating_add(1);
            state.last_activity = Instant::now();
        }
        let _guard = InFlightGuard {
            shared: self.shared.clone(),
        };
        let handle = ensure_connected(&self.shared, cancel.clone()).await?;
        let result = handle.request(request, cancel).await;
        {
            let mut state = lock(&self.shared.state);
            state.last_activity = Instant::now();
        }
        self.shared.notify.notify_waiters();
        result
    }

    fn disconnect(&self) {
        shutdown(&self.shared);
    }
}

struct InFlightGuard {
    shared: Arc<Shared>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut state = lock(&self.shared.state);
        state.in_flight = state.in_flight.saturating_sub(1);
        state.last_activity = Instant::now();
        drop(state);
        self.shared.notify.notify_waiters();
    }
}

async fn ensure_connected(
    shared: &Arc<Shared>,
    cancel: CancellationToken,
) -> Result<Arc<dyn SourceHandle>, CapabilityError> {
    if shared.closed.load(Ordering::Acquire) {
        return Err(failure("managed MCP source is closed"));
    }
    {
        let state = lock(&shared.state);
        if let Some(live) = state.live.as_ref() {
            return Ok(live.handle.clone());
        }
    }
    let _guard = shared.connect_lock.lock().await;
    if shared.closed.load(Ordering::Acquire) {
        return Err(failure("managed MCP source is closed"));
    }
    {
        let state = lock(&shared.state);
        if let Some(live) = state.live.as_ref() {
            return Ok(live.handle.clone());
        }
    }
    let source = StdioSource::connect(shared.config.clone(), cancel).await?;
    let server_info = source.server_info().clone();
    let control = source.control();
    let (manifest, handle) = source.into_parts();
    let handle: Arc<dyn SourceHandle> = Arc::from(handle);
    let compatible = {
        let state = lock(&shared.state);
        compatible_catalog(&state.manifest, &manifest) && state.server_info == server_info
    };
    if !compatible {
        handle.disconnect();
        control.wait_closed().await;
        return Err(failure(
            "managed MCP catalog or identity changed after recycle; reconnect explicitly",
        ));
    }
    if shared.closed.load(Ordering::Acquire) {
        handle.disconnect();
        control.wait_closed().await;
        return Err(failure("managed MCP source closed during connect"));
    }
    enum Install {
        Ready(Arc<dyn SourceHandle>),
        LostRace {
            existing: Arc<dyn SourceHandle>,
            unused: Arc<dyn SourceHandle>,
            control: StdioControl,
        },
    }
    let install = {
        let mut state = lock(&shared.state);
        if let Some(live) = state.live.as_ref() {
            Install::LostRace {
                existing: live.handle.clone(),
                unused: handle,
                control,
            }
        } else {
            state.connect_generation = state.connect_generation.saturating_add(1);
            state.spawn_count = state.spawn_count.saturating_add(1);
            state.last_activity = Instant::now();
            state.live = Some(Live {
                handle: handle.clone(),
                control,
                _server_info: server_info,
            });
            Install::Ready(handle)
        }
    };
    match install {
        Install::Ready(handle) => {
            shared.notify.notify_waiters();
            Ok(handle)
        }
        Install::LostRace {
            existing,
            unused,
            control,
        } => {
            unused.disconnect();
            control.wait_closed().await;
            Ok(existing)
        }
    }
}

fn compatible_catalog(pinned: &Manifest, fresh: &Manifest) -> bool {
    pinned.tools.len() == fresh.tools.len()
        && pinned.resources.len() == fresh.resources.len()
        && pinned.prompts.len() == fresh.prompts.len()
        && pinned
            .tools
            .iter()
            .zip(fresh.tools.iter())
            .all(|(left, right)| left.name == right.name)
        && pinned
            .resources
            .iter()
            .zip(fresh.resources.iter())
            .all(|(left, right)| left.uri == right.uri)
        && pinned
            .prompts
            .iter()
            .zip(fresh.prompts.iter())
            .all(|(left, right)| left.name == right.name)
}

fn shutdown(shared: &Arc<Shared>) {
    shared.closed.store(true, Ordering::Release);
    let live = {
        let mut state = lock(&shared.state);
        state.live.take()
    };
    if let Some(live) = live {
        live.handle.disconnect();
    }
    shared.notify.notify_waiters();
}

fn spawn_idle_watch(shared: Arc<Shared>) {
    tokio::spawn(async move {
        let slice = shared
            .idle_timeout
            .checked_div(4)
            .unwrap_or(Duration::from_millis(50))
            .max(Duration::from_millis(25));
        loop {
            if shared.closed.load(Ordering::Acquire) {
                return;
            }
            tokio::select! {
                _ = shared.notify.notified() => {}
                _ = tokio::time::sleep(slice) => {}
            }
            if shared.closed.load(Ordering::Acquire) {
                return;
            }
            if !matches!(shared.lifecycle, LifecycleMode::Lazy) || shared.idle_timeout.is_zero() {
                return;
            }
            let recycled = {
                let mut state = lock(&shared.state);
                if state.live.is_none() || state.in_flight > 0 {
                    None
                } else if Instant::now().saturating_duration_since(state.last_activity)
                    >= shared.idle_timeout
                {
                    let live = state.live.take();
                    if live.is_some() {
                        state.recycle_count = state.recycle_count.saturating_add(1);
                    }
                    live
                } else {
                    None
                }
            };
            if let Some(live) = recycled {
                live.handle.disconnect();
                // Do not block the watch loop on full reap; cleanup still runs.
                shared.notify.notify_waiters();
            }
        }
    });
}

fn validate_spec(spec: &ServerSpec) -> Result<(), CapabilityError> {
    if spec.id.is_empty()
        || spec.id.len() > 128
        || spec.version.is_empty()
        || spec.version.len() > 256
        || spec.origin.is_empty()
        || spec.origin.len() > 2048
        || spec.version.chars().any(char::is_control)
        || spec.origin.chars().any(char::is_control)
        || spec.idle_timeout > Duration::from_secs(24 * 60 * 60)
    {
        return Err(failure("invalid managed MCP server specification"));
    }
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn failure(message: &str) -> CapabilityError {
    CapabilityError::new(message)
}
