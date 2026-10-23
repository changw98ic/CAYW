use super::{ThreadCompletion, lock};
use crate::{
    DeviceError, DirectoryProcessSpec, ProcessExit, ProcessResult, ProcessRunner, ResourcePermit,
};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, oneshot};
use tokio_util::sync::CancellationToken;

/// Bounds the one pending write and unread stdout. Both limits must be positive
/// and representable as Rust allocation sizes. The spec's timeout/output budget
/// apply to the whole process lifetime, including idle and prefetched output.
/// In pipe mode, stderr is drained and charged without retaining or exposing its
/// contents; stderr read errors fail the shared outcome. PTY mode merges stderr
/// into the raw output stream, including terminal echo and line-ending conversion.
#[derive(Clone, Copy, Debug)]
pub struct InteractiveLimits {
    pub max_write_bytes: usize,
    pub max_buffered_stdout_bytes: usize,
}
impl InteractiveLimits {
    fn validate(self) -> Result<(), DeviceError> {
        if self.max_write_bytes == 0
            || self.max_buffered_stdout_bytes == 0
            || self.max_write_bytes > isize::MAX as usize
            || self.max_buffered_stdout_bytes > isize::MAX as usize
        {
            return Err(DeviceError::InvalidSpec(
                "interactive limits must be positive and representable",
            ));
        }
        Ok(())
    }
}

/// Unique protocol endpoints. Dropping either endpoint requests process stop,
/// even when a cloned control handle is retained. Use control.wait() for proof.
#[derive(Debug)]
pub struct InteractiveProcess {
    pub stdin: ManagedStdin,
    pub stdout: ManagedStdout,
    pub control: ProcessControl,
}

#[derive(Debug)]
pub struct ManagedStdin {
    pub(super) shared: Arc<InteractiveShared>,
}
#[derive(Debug)]
pub struct ManagedStdout {
    pub(super) shared: Arc<InteractiveShared>,
}
/// A cloneable observer/stop handle. It owns no OS descriptor, PID authority,
/// supervisor JoinHandle, or task reservation. Waiting never cancels the process.
#[derive(Clone, Debug)]
pub struct ProcessControl {
    pub(super) shared: Arc<InteractiveShared>,
}

type Outcome = Result<ProcessExit, Arc<DeviceError>>;
#[derive(Debug)]
pub(super) struct PendingWrite {
    pub bytes: Vec<u8>,
    pub offset: usize,
    pub completion: oneshot::Sender<Result<(), DeviceError>>,
}
#[derive(Debug, Default)]
pub(super) struct IoState {
    pub pending: Option<PendingWrite>,
    pub stdout: Vec<u8>,
    pub stdout_eof: bool,
    pub stopping: bool,
    pub error: Option<Arc<DeviceError>>,
}

/// This lock fences every waitid, signal and leader reap. It never covers a
/// blocking wait, group observation, TLS destructor, Scope lock, or true join.
#[derive(Debug, Default)]
pub(super) struct LeaderIdentity {
    #[cfg(unix)]
    pub pid: Option<u32>,
    #[cfg(unix)]
    pub exited: bool,
    #[cfg(unix)]
    pub lost: bool,
    #[cfg(target_os = "macos")]
    pub original: Option<super::ProcessIdentity>,
}

#[derive(Debug)]
pub(super) struct InteractiveShared {
    pub limits: InteractiveLimits,
    pub io: Mutex<IoState>,
    pub changed: Notify,
    pub cancel: CancellationToken,
    pub identity: Arc<Mutex<LeaderIdentity>>,
    pub completion: Arc<Mutex<ThreadCompletion>>,
    started: Mutex<bool>,
    staged: Mutex<Option<Outcome>>,
}
impl InteractiveShared {
    pub fn new(limits: InteractiveLimits) -> Arc<Self> {
        Arc::new(Self {
            limits,
            io: Mutex::default(),
            changed: Notify::new(),
            cancel: CancellationToken::new(),
            identity: Arc::default(),
            completion: Arc::default(),
            started: Mutex::new(false),
            staged: Mutex::default(),
        })
    }
    pub fn stop(&self) {
        lock(&self.io).stopping = true;
        self.cancel.cancel();
        self.changed.notify_waiters();
    }
    pub fn started(&self) {
        *lock(&self.started) = true;
        self.changed.notify_waiters();
    }
    // The supervisor stages an outcome after its owned process/pipes cleanup.
    // Only the existing join owner may publish it in ThreadCompletion.
    pub fn stage(&self, result: &Result<ProcessResult, DeviceError>) {
        let outcome = match result {
            Ok(result) => Ok(ProcessExit {
                status: result.status,
                termination: result.termination,
                output_complete: result.output_complete,
            }),
            Err(error) => Err(Arc::new(copy_error(error))),
        };
        {
            let mut io = lock(&self.io);
            io.stopping = true;
            io.stdout_eof = true;
            if let Err(error) = &outcome {
                io.error = Some(error.clone());
            }
            if let Some(write) = io.pending.take() {
                let _ = write.completion.send(Err(io
                    .error
                    .as_deref()
                    .map_or(DeviceError::ProcessClosed, copy_error)));
            }
        }
        *lock(&self.staged) = Some(outcome);
        self.changed.notify_waiters();
    }
    pub fn outcome(&self) -> Outcome {
        lock(&self.staged)
            .clone()
            .unwrap_or_else(|| Err(Arc::new(DeviceError::SupervisorStopped)))
    }
    pub fn stdout_room(&self) -> usize {
        self.limits.max_buffered_stdout_bytes - lock(&self.io).stdout.len()
    }
    pub fn push_stdout(&self, bytes: &[u8]) -> std::io::Result<bool> {
        let mut io = lock(&self.io);
        let room = self.limits.max_buffered_stdout_bytes - io.stdout.len();
        let keep = bytes.len().min(room);
        io.stdout
            .try_reserve_exact(keep)
            .map_err(std::io::Error::other)?;
        io.stdout.extend_from_slice(&bytes[..keep]);
        drop(io);
        self.changed.notify_waiters();
        Ok(keep == bytes.len())
    }
    pub fn stdout_eof(&self) {
        lock(&self.io).stdout_eof = true;
        self.changed.notify_waiters();
    }
}

impl ManagedStdin {
    /// Validates before allocating/copying. At most one pending write exists.
    /// Success confirms every byte was written into the original pipe or PTY;
    /// it does not establish that the child consumed the bytes.
    /// Dropping a polled write before it returns stops the original process;
    /// after a partial protocol frame no later write can be admitted.
    pub async fn write_all(&mut self, bytes: &[u8]) -> Result<(), DeviceError> {
        if bytes.len() > self.shared.limits.max_write_bytes {
            return Err(DeviceError::InvalidSpec(
                "interactive write exceeds max_write_bytes",
            ));
        }
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = lock(&self.shared.io);
            if state.stopping {
                return Err(state
                    .error
                    .as_deref()
                    .map_or(DeviceError::ProcessClosed, copy_error));
            }
            if state.pending.is_some() {
                return Err(DeviceError::ProcessClosed);
            }
            if bytes.is_empty() {
                return Ok(());
            }
            let mut owned = Vec::new();
            owned.try_reserve_exact(bytes.len()).map_err(|error| {
                DeviceError::io("allocate interactive write", std::io::Error::other(error))
            })?;
            owned.extend_from_slice(bytes);
            state.pending = Some(PendingWrite {
                bytes: owned,
                offset: 0,
                completion: sender,
            });
        }
        let mut guard = WriteGuard {
            shared: &self.shared,
            armed: true,
        };
        let result = receiver
            .await
            .unwrap_or(Err(DeviceError::SupervisorStopped));
        guard.armed = false;
        result
    }
}
struct WriteGuard<'a> {
    shared: &'a InteractiveShared,
    armed: bool,
}
impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.shared.stop();
        }
    }
}
impl Drop for ManagedStdin {
    fn drop(&mut self) {
        self.shared.stop();
    }
}

impl ManagedStdout {
    /// Returns currently buffered raw bytes. Cancellation/drop of recv itself
    /// loses no bytes; dropping the endpoint requests process stop. EOF alone is
    /// not a cleanup proof: await control.wait() before releasing task resources.
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>, DeviceError> {
        loop {
            let changed = self.shared.changed.notified();
            {
                let mut state = lock(&self.shared.io);
                if !state.stdout.is_empty() {
                    return Ok(Some(std::mem::take(&mut state.stdout)));
                }
                if let Some(error) = &state.error {
                    return Err(copy_error(error));
                }
                if state.stdout_eof {
                    return Ok(None);
                }
            }
            changed.await;
        }
    }
}
impl Drop for ManagedStdout {
    fn drop(&mut self) {
        self.shared.stop();
    }
}

impl ProcessControl {
    /// Observes the original unreaped leader synchronously with WNOHANG/WNOWAIT.
    /// It never reopens a PID, and never waits for group cleanup or thread exit.
    pub fn is_running(&self) -> Result<bool, DeviceError> {
        #[cfg(unix)]
        {
            super::unix::identity_running(&self.shared.identity)
                .map_err(|error| DeviceError::io("observe interactive leader", error))
        }
        #[cfg(not(unix))]
        {
            Err(DeviceError::UnsupportedPlatform)
        }
    }
    pub fn close(&self) {
        self.shared.stop();
    }
    /// Repeatable and safe for multiple/cancelled waiters; only the true join
    /// owner's completion fact makes this ready, shared with ProcessRunner::drain.
    pub async fn wait(&self) -> Outcome {
        loop {
            let changed = self.shared.changed.notified();
            {
                let completion = lock(&self.shared.completion);
                if completion.joined {
                    return completion
                        .outcome
                        .clone()
                        .unwrap_or_else(|| Err(Arc::new(DeviceError::SupervisorStopped)));
                }
            }
            changed.await;
        }
    }
}

impl ProcessRunner {
    /// Starts one managed interactive process with an explicit lifetime timeout
    /// and raw stdout+stderr lifetime budget from spec. The original supervisor
    /// retains the child, group, pipes or PTY, opened cwd, and transferred permit.
    pub async fn start_interactive_in_directory_with_permit(
        &self,
        spec: DirectoryProcessSpec,
        limits: InteractiveLimits,
        cancellation: CancellationToken,
        permit: ResourcePermit,
    ) -> Result<InteractiveProcess, DeviceError> {
        #[cfg(not(unix))]
        {
            let _ = (spec, limits, cancellation, permit);
            Err(DeviceError::UnsupportedPlatform)
        }
        #[cfg(unix)]
        {
            limits.validate()?;
            let spec = self.prepare_directory(spec)?;
            self.start_interactive(spec, limits, cancellation, permit)
                .await
        }
    }

    #[cfg(unix)]
    pub(super) async fn start_interactive(
        &self,
        spec: super::PreparedProcess,
        limits: InteractiveLimits,
        cancellation: CancellationToken,
        permit: ResourcePermit,
    ) -> Result<InteractiveProcess, DeviceError> {
        let shared = InteractiveShared::new(limits);
        let mut running = self
            .launch_inner(spec, cancellation, Some(permit), Some(shared.clone()))
            .await?;
        let control = ProcessControl {
            shared: shared.clone(),
        };
        loop {
            let changed = shared.changed.notified();
            if *lock(&shared.started) {
                // Transfer cancellation responsibility to the unique endpoints.
                // A successful startup future must not kill a long-lived peer.
                running.cancel_on_drop.0.take();
                return Ok(InteractiveProcess {
                    stdin: ManagedStdin {
                        shared: shared.clone(),
                    },
                    stdout: ManagedStdout {
                        shared: shared.clone(),
                    },
                    control,
                });
            }
            if lock(&shared.completion).joined {
                return match control.wait().await {
                    Err(error) => Err(copy_error(&error)),
                    Ok(_) => Err(DeviceError::SupervisorStopped),
                };
            }
            changed.await;
        }
    }
}

// DeviceError owns an io::Error. Preserve errno/kind/message when an error must
// be returned both to the unique endpoint and the shared repeatable wait result.
pub(super) fn copy_error(error: &DeviceError) -> DeviceError {
    fn copy_io(error: &std::io::Error) -> std::io::Error {
        error.raw_os_error().map_or_else(
            || std::io::Error::new(error.kind(), error.to_string()),
            std::io::Error::from_raw_os_error,
        )
    }
    match error {
        DeviceError::InvalidSpec(value) => DeviceError::InvalidSpec(value),
        DeviceError::ProgramNotAllowed(value) => DeviceError::ProgramNotAllowed(value.clone()),
        DeviceError::Cancelled => DeviceError::Cancelled,
        DeviceError::RunnerClosed => DeviceError::RunnerClosed,
        DeviceError::ResourcesBusy => DeviceError::ResourcesBusy,
        DeviceError::ResourceRequestTooLarge => DeviceError::ResourceRequestTooLarge,
        DeviceError::UnsupportedResource(value) => DeviceError::UnsupportedResource(value),
        DeviceError::InvalidLimits(value) => DeviceError::InvalidLimits(value),
        DeviceError::InvalidSessionId => DeviceError::InvalidSessionId,
        DeviceError::UnsupportedPlatform => DeviceError::UnsupportedPlatform,
        DeviceError::NotStarted { operation, source } => {
            DeviceError::not_started(operation, copy_io(source))
        }
        DeviceError::CleanupUnconfirmed(value) => DeviceError::CleanupUnconfirmed(value.clone()),
        DeviceError::Io { operation, source } => DeviceError::io(operation, copy_io(source)),
        DeviceError::SupervisorStopped => DeviceError::SupervisorStopped,
        DeviceError::ProcessClosed => DeviceError::ProcessClosed,
    }
}
