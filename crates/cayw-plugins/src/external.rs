use crate::*;
use serde_json::Value;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

/// Explicit executable and argument vector. No shell, inherited credentials or
/// environment are used. This process boundary is not a security sandbox.
#[derive(Debug, Clone)]
pub struct ExternalConfig {
    pub executable: PathBuf,
    pub argv: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub working_directory: Option<PathBuf>,
    pub expected_manifest: PluginManifest,
    pub handshake_timeout: Duration,
    pub call_timeout: Duration,
    pub max_line_bytes: usize,
    /// Lifetime stdout + stderr bytes, including handshake and line delimiters.
    pub max_output_bytes: usize,
}

impl ExternalConfig {
    pub fn new(executable: PathBuf, argv: Vec<String>, expected_manifest: PluginManifest) -> Self {
        Self {
            executable,
            argv,
            expected_manifest,
            environment: BTreeMap::new(),
            working_directory: None,
            handshake_timeout: Duration::from_secs(10),
            call_timeout: Duration::from_secs(30),
            max_line_bytes: 64 * 1024,
            max_output_bytes: 4 * 1024 * 1024,
        }
    }

    fn validate(&self) -> Result<(), PluginError> {
        if !self.executable.is_absolute()
            || self
                .working_directory
                .as_ref()
                .is_some_and(|path| !path.is_absolute())
            || self.handshake_timeout.is_zero()
            || self.call_timeout.is_zero()
            || self.max_line_bytes == 0
            || self.max_output_bytes < self.max_line_bytes
        {
            return Err(PluginError::InvalidManifest(
                "invalid process configuration or limits".into(),
            ));
        }
        if self.expected_manifest.kind != PluginKind::External {
            return Err(PluginError::InvalidManifest(
                "expected external plugin".into(),
            ));
        }
        crate::registry::validate_manifest(&self.expected_manifest)?;
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostMessage {
    Handshake {
        protocol_version: u32,
    },
    Call {
        request_id: u64,
        method: String,
        arguments: Value,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExternalMessage {
    Handshake {
        manifest: PluginManifest,
    },
    Response {
        request_id: u64,
        result: ExternalResult,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExternalResult {
    Ok {
        data: Value,
    },
    Error {
        message: String,
        outcome_unknown: bool,
    },
}

struct Call {
    method: String,
    arguments: Value,
    cancel: CancellationToken,
    reply: oneshot::Sender<Result<Value, PluginError>>,
}

pub struct ExternalPlugin {
    manifest: PluginManifest,
    sender: mpsc::Sender<Call>,
    stop: CancellationToken,
    process: Arc<Mutex<Option<ProcessGuard>>>,
}

impl ExternalPlugin {
    pub async fn spawn(
        config: ExternalConfig,
        cancel: CancellationToken,
    ) -> Result<Self, PluginError> {
        config.validate()?;
        if cancel.is_cancelled() {
            return Err(PluginError::Cancelled);
        }
        // Subscribe before spawning so an early exit cannot be missed.
        let watcher = ProcessWatcher::new().map_err(io_error)?;
        let mut command = Command::new(&config.executable);
        command
            .args(&config.argv)
            .env_clear()
            .envs(&config.environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // ProcessGuard owns termination, including the order of group kill and
        // reaping on Unix. Leave Tokio's kill_on_drop disabled.
        if let Some(directory) = &config.working_directory {
            command.current_dir(directory);
        }
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn().map_err(io_error)?;
        let mut process = ProcessGuard::new(child);
        let mut stdin = process.child.stdin.take().expect("piped stdin");
        let mut stdout = BufReader::new(process.child.stdout.take().expect("piped stdout"));
        let mut stderr = process.child.stderr.take().expect("piped stderr");
        let budget = Arc::new(OutputBudget {
            bytes: AtomicUsize::new(0),
            limit: config.max_output_bytes,
            exceeded: CancellationToken::new(),
        });
        let stderr_budget = budget.clone();
        let stderr_task = AbortOnDrop(tokio::spawn(async move {
            let mut bytes = [0_u8; 4096];
            loop {
                match stderr.read(&mut bytes).await {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if !stderr_budget.add(count) {
                            break;
                        }
                    }
                }
            }
        }));
        let handshake = async {
            write_message(
                &mut stdin,
                &HostMessage::Handshake {
                    protocol_version: PLUGIN_PROTOCOL_VERSION,
                },
                config.max_line_bytes,
            )
            .await?;
            let message = read_message(&mut stdout, &budget, config.max_line_bytes).await?;
            match message {
                ExternalMessage::Handshake { manifest } if manifest == config.expected_manifest => {
                    Ok(manifest)
                }
                ExternalMessage::Handshake { .. } => Err(PluginError::Protocol(
                    "handshake manifest differs from host declaration".into(),
                )),
                _ => Err(PluginError::Protocol("expected handshake".into())),
            }
        };
        let manifest = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(PluginError::Cancelled),
            _ = budget.exceeded.cancelled() => Err(output_limit()),
            result = tokio::time::timeout(config.handshake_timeout, handshake) => result.unwrap_or(Err(PluginError::Timeout)),
        }?;
        let (sender, receiver) = mpsc::channel(16);
        let stop = CancellationToken::new();
        let process = Arc::new(Mutex::new(Some(process)));
        tokio::spawn(run_process(
            ProcessSession {
                process: process.clone(),
                watcher,
                stdin,
                stdout,
                budget,
                _stderr_task: stderr_task,
                config,
            },
            receiver,
            stop.clone(),
        ));
        Ok(Self {
            manifest,
            sender,
            stop,
            process,
        })
    }
}

#[async_trait]
impl Plugin for ExternalPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn check_available(&self) -> Result<(), PluginError> {
        if self.stop.is_cancelled() || self.sender.is_closed() {
            return Err(PluginError::Unavailable("process stopped".into()));
        }
        // Query the owned child, even if the supervisor has not been scheduled.
        // The lock covers only nonblocking OS operations, never an async wait.
        let status = {
            let mut process = self.process.lock().expect("plugin process poisoned");
            let Some(process) = process.as_mut() else {
                return Err(PluginError::Unavailable("process stopped".into()));
            };
            process.try_wait()
        };
        match status {
            Ok(None) if !self.stop.is_cancelled() => Ok(()),
            Ok(_) => {
                self.stop.cancel();
                Err(PluginError::Unavailable("process stopped".into()))
            }
            Err(error) => {
                self.stop.cancel();
                Err(PluginError::Unavailable(format!(
                    "cannot check process: {error}"
                )))
            }
        }
    }

    async fn call(
        &self,
        method: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, PluginError> {
        let call_cancel = cancel.child_token();
        let _guard = CancelOnDrop(call_cancel.clone());
        let (reply, receiver) = oneshot::channel();
        let call = Call {
            method: method.into(),
            arguments,
            cancel: call_cancel,
            reply,
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(PluginError::Cancelled),
            _ = self.stop.cancelled() => return Err(PluginError::Unavailable("process stopped".into())),
            result = self.sender.send(call) => result.map_err(|_| PluginError::Unavailable("process stopped".into()))?,
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(PluginError::Cancelled),
            result = receiver => result.unwrap_or_else(|_| Err(PluginError::OutcomeUnknown("process stopped before replying".into()))),
        }
    }

    fn shutdown(&self) {
        self.stop.cancel();
    }
}

impl Drop for ExternalPlugin {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct ProcessSession {
    process: Arc<Mutex<Option<ProcessGuard>>>,
    watcher: ProcessWatcher,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    budget: Arc<OutputBudget>,
    _stderr_task: AbortOnDrop,
    config: ExternalConfig,
}

impl Drop for ProcessSession {
    fn drop(&mut self) {
        // The plugin also retains the shared handle. Aborting the supervisor
        // must still terminate the child, even if the plugin itself stays alive.
        let process = self.process.lock().expect("plugin process poisoned").take();
        // Drop the handle outside the mutex. Tokio then reaps immediately or
        // transfers an unreaped Unix child to its orphan queue.
        drop(process);
    }
}

async fn run_process(
    mut session: ProcessSession,
    mut receiver: mpsc::Receiver<Call>,
    stop: CancellationToken,
) {
    let _stop_guard = CancelOnDrop(stop.clone());
    let mut request_id = 0_u64;
    loop {
        let call = tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            _ = session.budget.exceeded.cancelled() => break,
            // Detect parent exit even when a descendant still holds stdout open.
            _ = session.watcher.wait(&session.process) => break,
            // Unsolicited output is a protocol violation, even while idle.
            _ = session.stdout.fill_buf() => break,
            call = receiver.recv() => match call { Some(call) => call, None => break },
        };
        if call.cancel.is_cancelled() || call.reply.is_closed() {
            let _ = call.reply.send(Err(PluginError::Cancelled));
            continue;
        }
        let Some(next_id) = request_id.checked_add(1) else {
            break;
        };
        request_id = next_id;
        let exchange = async {
            write_message(
                &mut session.stdin,
                &HostMessage::Call {
                    request_id,
                    method: call.method,
                    arguments: call.arguments,
                },
                session.config.max_line_bytes,
            )
            .await?;
            match read_message(
                &mut session.stdout,
                &session.budget,
                session.config.max_line_bytes,
            )
            .await?
            {
                ExternalMessage::Response {
                    request_id: response_id,
                    result,
                } if response_id == request_id => match result {
                    ExternalResult::Ok { data } => Ok(data),
                    ExternalResult::Error {
                        message,
                        outcome_unknown: false,
                    } => Err(PluginError::Failed(message)),
                    ExternalResult::Error {
                        message,
                        outcome_unknown: true,
                    } => Err(PluginError::OutcomeUnknown(message)),
                },
                _ => Err(PluginError::Protocol(
                    "response does not match request".into(),
                )),
            }
        };
        let result = tokio::select! {
            biased;
            _ = stop.cancelled() => Err(PluginError::Unloaded),
            _ = call.cancel.cancelled() => Err(PluginError::Cancelled),
            _ = session.budget.exceeded.cancelled() => Err(output_limit()),
            _ = session.watcher.wait(&session.process) => Err(PluginError::OutcomeUnknown("process exited before replying".into())),
            result = tokio::time::timeout(session.config.call_timeout, exchange) => result.unwrap_or(Err(PluginError::Timeout)),
        };
        let terminal = result
            .as_ref()
            .is_err_and(|error| !matches!(error, PluginError::Failed(_)));
        if terminal {
            // Close admission before replying so a following call cannot race the
            // process teardown and enter a dead request queue.
            stop.cancel();
            if let Some(process) = session
                .process
                .lock()
                .expect("plugin process poisoned")
                .as_mut()
            {
                process.terminate();
            }
        }
        let _ = call.reply.send(result);
        if terminal {
            break;
        }
    }
    if let Some(process) = session
        .process
        .lock()
        .expect("plugin process poisoned")
        .as_mut()
    {
        process.terminate();
    }
    let _ = session.watcher.wait(&session.process).await;
}

/// The only owner allowed to inspect, signal or reap this child. On Unix an
/// unreaped child reserves its PID; a cached numeric PID after reaping does not.
/// On Windows Child queries/signals its process handle, never a reopened PID.
struct ProcessGuard {
    child: Child,
    terminated: bool,
    #[cfg(unix)]
    reaped: bool,
}

impl ProcessGuard {
    fn new(child: Child) -> Self {
        Self {
            child,
            terminated: false,
            #[cfg(unix)]
            reaped: false,
        }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        #[cfg(unix)]
        if !self.reaped {
            match self.exit_pending() {
                Ok(false) => return Ok(None),
                Ok(true) => self.terminate(),
                Err(error) => return Err(error),
            }
        }
        let result = self.child.try_wait();
        #[cfg(unix)]
        if matches!(result, Ok(Some(_))) {
            self.reaped = true;
        }
        result
    }

    /// Observe without reaping, then kill the group before try_wait releases the
    /// leader's PID. No other task polls Child::wait on Unix. As with std::Child,
    /// the host must not separately reap this child or configure SIGCHLD to reap
    /// it automatically. ECHILD revokes our ability to signal by numeric PID.
    #[cfg(unix)]
    fn exit_pending(&mut self) -> std::io::Result<bool> {
        use nix::libc;

        let Some(pid) = self.child.id() else {
            self.reaped = true;
            return Ok(true);
        };
        // SAFETY: zero is valid for siginfo_t. waitid gets a valid, writable
        // structure and WNOHANG makes it nonblocking. WNOWAIT preserves the PID.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        loop {
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result != -1 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if error.raw_os_error() == Some(libc::ECHILD) {
                self.reaped = true;
            }
            return Err(error);
        }
        // Apple exposes the field; the other Unix libc definitions use an
        // accessor for the siginfo union. Only read it after successful waitid.
        #[cfg(target_vendor = "apple")]
        let observed = info.si_pid;
        #[cfg(not(target_vendor = "apple"))]
        let observed = unsafe { info.si_pid() };
        Ok(observed != 0)
    }

    fn terminate(&mut self) {
        if self.terminated {
            return;
        }
        self.terminated = true;
        #[cfg(unix)]
        {
            // Already-reaped or externally reaped children no longer reserve
            // this PID. Never signal an old group ID after that boundary.
            if self.reaped || self.exit_pending().is_err() {
                return;
            }
            if let Some(group) = self
                .child
                .id()
                .and_then(|id| i32::try_from(id).ok())
                .map(nix::unistd::Pid::from_raw)
            {
                let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
            }
        }
        let _ = self.child.start_kill();
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

struct ProcessWatcher {
    #[cfg(unix)]
    signal: tokio::signal::unix::Signal,
}

impl ProcessWatcher {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            signal: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?,
        })
    }

    async fn wait(&mut self, process: &Mutex<Option<ProcessGuard>>) -> std::io::Result<ExitStatus> {
        #[cfg(unix)]
        loop {
            {
                let mut process = process.lock().expect("plugin process poisoned");
                let process = process
                    .as_mut()
                    .ok_or_else(|| std::io::Error::other("process stopped"))?;
                if let Some(status) = process.try_wait()? {
                    return Ok(status);
                }
            }
            // Subscription was installed before spawn. A signal arriving
            // between the query and recv remains pending. Other children can
            // cause a wakeup; always recheck this owned child.
            self.signal
                .recv()
                .await
                .ok_or_else(|| std::io::Error::other("process exit notification closed"))?;
        }
        #[cfg(not(unix))]
        std::future::poll_fn(|cx| {
            let mut process = process.lock().expect("plugin process poisoned");
            let process = process
                .as_mut()
                .ok_or_else(|| std::io::Error::other("process stopped"))?;
            let wait = process.child.wait();
            tokio::pin!(wait);
            // Child::wait is cancel safe; only this poll holds the lock. On
            // Windows it registers a wait on the original process handle.
            wait.poll(cx)
        })
        .await
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct OutputBudget {
    bytes: AtomicUsize,
    limit: usize,
    exceeded: CancellationToken,
}
impl OutputBudget {
    fn add(&self, count: usize) -> bool {
        if self
            .bytes
            .fetch_add(count, Ordering::Relaxed)
            .saturating_add(count)
            > self.limit
        {
            self.exceeded.cancel();
            false
        } else {
            true
        }
    }
}

async fn write_message(
    stdin: &mut ChildStdin,
    message: &HostMessage,
    limit: usize,
) -> Result<(), PluginError> {
    let mut bytes =
        serde_json::to_vec(message).map_err(|error| PluginError::Protocol(error.to_string()))?;
    if bytes.len() > limit {
        return Err(PluginError::Protocol("request exceeds line limit".into()));
    }
    bytes.push(b'\n');
    stdin.write_all(&bytes).await.map_err(io_error)?;
    stdin.flush().await.map_err(io_error)
}

async fn read_message(
    stdout: &mut BufReader<ChildStdout>,
    budget: &OutputBudget,
    limit: usize,
) -> Result<ExternalMessage, PluginError> {
    let mut line = Vec::new();
    loop {
        let available = stdout.fill_buf().await.map_err(io_error)?;
        if available.is_empty() {
            return Err(PluginError::Protocol("plugin closed stdout".into()));
        }
        let end = available.iter().position(|byte| *byte == b'\n');
        let count = end.map_or(available.len(), |index| index + 1);
        if !budget.add(count) {
            return Err(output_limit());
        }
        let content_count = count - usize::from(end.is_some());
        if line.len().saturating_add(content_count) > limit {
            return Err(PluginError::Protocol("response exceeds line limit".into()));
        }
        line.extend_from_slice(&available[..content_count]);
        stdout.consume(count);
        if end.is_some() {
            return serde_json::from_slice(&line)
                .map_err(|error| PluginError::Protocol(error.to_string()));
        }
    }
}

fn io_error(error: std::io::Error) -> PluginError {
    PluginError::Io(error.to_string())
}
fn output_limit() -> PluginError {
    PluginError::Protocol("plugin output budget exceeded".into())
}
