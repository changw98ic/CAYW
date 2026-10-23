use crate::{DeviceError, DirectoryProcessSpec, ProcessResult, ProcessSpec, ResourcePermit};
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

mod identity;
mod interactive;
pub use identity::{ProcessIdentity, ProcessObserver};
use interactive::InteractiveShared;
pub use interactive::{
    InteractiveLimits, InteractiveProcess, ManagedStdin, ManagedStdout, ProcessControl,
};

#[cfg(unix)]
struct Running {
    receiver: tokio::sync::oneshot::Receiver<Result<ProcessResult, DeviceError>>,
    completion: Arc<Mutex<ThreadCompletion>>,
    cancel_on_drop: CancelOnDrop,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[derive(Debug, Default)]
struct ScopeState {
    closed: bool,
    #[cfg(unix)]
    next: u64,
    supervisors: std::collections::BTreeMap<u64, Supervisor>,
    failed: bool,
}

#[derive(Debug, Default)]
struct CleanupState {
    pending: usize,
    failure: Option<String>,
    retained: Vec<Retained>,
}

struct Retained(Box<dyn Send>);
impl std::fmt::Debug for Retained {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let _ = &self.0;
        f.write_str("Retained")
    }
}

impl Drop for CleanupState {
    fn drop(&mut self) {
        // An unconfirmed resource/permit must not be released by dropping the
        // last runner. Deliberately quarantine it for this process's lifetime;
        // there is no destructor retry or reset that can prove reconciliation.
        for resource in self.retained.drain(..) {
            std::mem::forget(resource);
        }
    }
}

#[derive(Debug, Default)]
struct CleanupLedger {
    state: Mutex<CleanupState>,
    changed: Notify,
}

impl CleanupLedger {
    fn register(self: &Arc<Self>) -> PendingCleanup {
        lock(&self.state).pending += 1;
        PendingCleanup(self.clone())
    }
}

struct PendingCleanup(Arc<CleanupLedger>);
impl Drop for PendingCleanup {
    fn drop(&mut self) {
        lock(&self.0.state).pending -= 1;
        self.0.changed.notify_waiters();
    }
}

// Fields drop in declaration order: completion includes the resource's own Drop.
struct RegisteredResources<R> {
    _resource: R,
    _pending: PendingCleanup,
}

/// A resource whose cleanup must be confirmed before task admission is released.
/// Cleanup runs after managed processes and their pipes have stopped. On error or
/// panic the runner retains the resource, closes admission, and drain returns an
/// error for the rest of that runner's lifetime. There is no implicit retry/reset.
/// The implementation must never delete an entry it cannot establish it owns.
pub trait ProcessCleanup: Send + 'static {
    fn cleanup(&mut self) -> std::io::Result<()>;
}

/// Owned cleanup registered with one runner, including before process launch.
/// Transfer this to `run_in_directory_with_resources` to hold it until supervisor
/// cleanup. Drain waits for every registration to finish, so release or transfer
/// this resource before awaiting drain. Earlier returns use the same ledger.
pub struct CleanupResource<R: ProcessCleanup> {
    resource: Option<R>,
    // The ledger never owns Scope or a registration. Failed resources are
    // retained without their registration, so last-owner drop cannot form a cycle.
    pending: Option<PendingCleanup>,
}
impl<R: ProcessCleanup> std::ops::Deref for CleanupResource<R> {
    type Target = R;
    fn deref(&self) -> &R {
        self.resource.as_ref().expect("owned cleanup resource")
    }
}
impl<R: ProcessCleanup> Drop for CleanupResource<R> {
    fn drop(&mut self) {
        let Some(mut resource) = self.resource.take() else {
            return;
        };
        let pending = self.pending.take().expect("registered cleanup resource");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| resource.cleanup()));
        let failure = match result {
            Ok(Ok(())) => {
                drop(resource);
                return;
            }
            Ok(Err(error)) => error.to_string(),
            Err(_) => "resource cleanup panicked".into(),
        };
        {
            // Resource cleanup and recording failure never acquire Scope::state.
            let mut cleanup = lock(&pending.0.state);
            cleanup.failure.get_or_insert(failure);
            cleanup.retained.push(Retained(Box::new(resource)));
        }
        drop(pending);
    }
}

#[derive(Debug, Default)]
struct ThreadCompletion {
    joined: bool,
    failed: bool,
    outcome: Option<Result<crate::ProcessExit, Arc<DeviceError>>>,
}

#[derive(Debug)]
struct Supervisor {
    cancel: CancellationToken,
    completion: Arc<Mutex<ThreadCompletion>>,
}

#[cfg(unix)]
struct JoinWork {
    thread: Option<std::thread::JoinHandle<()>>,
    resources: SupervisorResources,
}

#[cfg(unix)]
type JoinQueue = Arc<(Mutex<Option<JoinWork>>, std::sync::Condvar)>;

// The join worker is started before the supervisor. This guard always transfers
// its work, even if admission unwinds, without a fallible channel send that could
// drop a live supervisor's handle. Neither side owns Scope or Owner.
#[cfg(unix)]
struct JoinAdmission {
    queue: JoinQueue,
    work: Option<JoinWork>,
}

#[cfg(unix)]
impl Drop for JoinAdmission {
    fn drop(&mut self) {
        let work = self.work.as_ref().expect("join admission work");
        if work.thread.is_none() {
            // No supervisor can own this cleanup. Run it on the admission caller
            // after its Scope guard has dropped, never on the join worker (whose
            // own TLS is not a managed supervisor's TLS).
            let prepared = lock(&work.resources.prepared).take();
            drop(prepared);
        }
        *lock(&self.queue.0) = self.work.take();
        self.queue.1.notify_one();
    }
}

#[cfg(unix)]
fn start_join_owner(
    completion: Arc<Mutex<ThreadCompletion>>,
    resources: SupervisorResources,
) -> std::io::Result<JoinAdmission> {
    let queue: JoinQueue = Arc::default();
    let incoming = queue.clone();
    let ledger = resources.ledger.clone();
    #[cfg(all(test, target_os = "macos"))]
    if unix::tests::fail_join_start() {
        return Err(std::io::Error::from_raw_os_error(libc::EAGAIN));
    }
    std::thread::Builder::new()
        .name("cayw-process-join".into())
        .spawn(move || {
            let work = {
                let mut work = lock(&incoming.0);
                while work.is_none() {
                    work = incoming
                        .1
                        .wait(work)
                        .unwrap_or_else(|error| error.into_inner());
                }
                work.take().expect("supervisor join work")
            };
            // is_finished does not include TLS destruction. Only this true join
            // proves thread exit, and it holds no Scope mutex or executor thread.
            let failed = work.thread.is_some_and(|thread| thread.join().is_err());
            let interactive = work.resources.interactive.clone();
            drop(work.resources);
            let outcome = interactive.as_ref().map(|shared| {
                if let Some(failure) = &lock(&ledger.state).failure {
                    Err(Arc::new(DeviceError::CleanupUnconfirmed(failure.clone())))
                } else if failed {
                    Err(Arc::new(DeviceError::SupervisorStopped))
                } else {
                    shared.outcome()
                }
            });
            *lock(&completion) = ThreadCompletion {
                joined: true,
                failed,
                outcome,
            };
            ledger.changed.notify_waiters();
            if let Some(shared) = interactive {
                shared.changed.notify_waiters();
            }
        })?;
    Ok(JoinAdmission {
        queue,
        work: Some(JoinWork {
            thread: None,
            resources,
        }),
    })
}

#[derive(Debug, Default)]
struct Scope {
    state: Mutex<ScopeState>,
    cleanup: Arc<CleanupLedger>,
    // Cleanup failure can close admission without taking Scope::state from an
    // OS identity lock, a destructor, or an indefinitely retrying supervisor.
    admission_closed: Arc<std::sync::atomic::AtomicBool>,
}

impl Scope {
    fn close(&self) {
        let mut state = lock(&self.state);
        state.closed = true;
        for supervisor in state.supervisors.values() {
            supervisor.cancel.cancel();
        }
        self.cleanup.changed.notify_waiters();
    }

    fn reap(state: &mut ScopeState) {
        // The join worker records completion only after the real join and
        // resource/permit cleanup. No blocking work runs under this mutex.
        state.supervisors.retain(|_, supervisor| {
            let completion = lock(&supervisor.completion);
            if completion.joined {
                state.failed |= completion.failed;
                false
            } else {
                true
            }
        });
    }

    async fn wait(&self, completion: Option<&Mutex<ThreadCompletion>>) -> Result<(), DeviceError> {
        loop {
            let changed = self.cleanup.changed.notified();
            {
                let mut state = lock(&self.state);
                Self::reap(&mut state);
                let cleanup = lock(&self.cleanup.state);
                let complete = match completion {
                    Some(completion) => lock(completion).joined,
                    None => state.supervisors.is_empty() && cleanup.pending == 0,
                };
                if complete {
                    return if let Some(message) = &cleanup.failure {
                        Err(DeviceError::CleanupUnconfirmed(message.clone()))
                    } else if state.failed {
                        Err(DeviceError::SupervisorStopped)
                    } else {
                        Ok(())
                    };
                }
            }
            changed.await;
        }
    }
}

#[derive(Debug, Default)]
struct Owner(Arc<Scope>);

impl Drop for Owner {
    fn drop(&mut self) {
        // Drop requests cancellation only. drain is the cleanup proof boundary.
        self.0.close();
    }
}

#[cfg_attr(not(unix), allow(dead_code))]
struct PreparedProcess {
    spec: DirectoryProcessSpec,
    // Keep application-owned snapshots/resources alive even if the run future
    // drops, until all managed processes and their output pipes are cleaned up.
    resources: Option<Box<dyn Send>>,
    observer: Option<Arc<dyn ProcessObserver>>,
}

// The admission guard owns failed-start preparation; the join worker holds its
// permit until that cleanup ends. Neither runs resource cleanup under Scope.
#[cfg(unix)]
struct SupervisorResources {
    prepared: Arc<Mutex<Option<PreparedProcess>>>,
    permit: Option<ResourcePermit>,
    interactive: Option<Arc<InteractiveShared>>,
    ledger: Arc<CleanupLedger>,
    _pending: PendingCleanup,
}
#[cfg(unix)]
impl SupervisorResources {
    fn new(
        prepared: PreparedProcess,
        permit: Option<ResourcePermit>,
        ledger: Arc<CleanupLedger>,
    ) -> Self {
        let pending = ledger.register();
        Self {
            prepared: Arc::new(Mutex::new(Some(prepared))),
            permit,
            interactive: None,
            ledger,
            _pending: pending,
        }
    }
}
#[cfg(unix)]
impl Drop for SupervisorResources {
    fn drop(&mut self) {
        let prepared = lock(&self.prepared).take();
        drop(prepared);
        let mut cleanup = lock(&self.ledger.state);
        if cleanup.failure.is_some()
            && let Some(permit) = self.permit.take()
        {
            cleanup.retained.push(Retained(Box::new(permit)));
        }
    }
}

#[cfg(unix)]
fn start_supervisor(
    task: impl FnOnce() + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    #[cfg(all(test, target_os = "macos"))]
    if unix::tests::fail_thread_start() {
        return Err(std::io::Error::from_raw_os_error(libc::EAGAIN));
    }
    std::thread::Builder::new()
        .name("cayw-process".into())
        .spawn(task)
}

/// Runs only executable paths explicitly selected by the host.
///
/// Commands receive literal arguments and an empty environment populated only
/// from `ProcessSpec::env`. An allowlisted executable still has the host's OS
/// access: this is process lifecycle management, not a security sandbox.
/// Create one runner per task. Clones share admission and supervision; closing
/// any clone closes all of them. Last-owner Drop only requests cancellation;
/// retain the runner and await `drain` to prove cleanup before releasing a task.
#[derive(Clone, Debug)]
pub struct ProcessRunner {
    programs: BTreeSet<PathBuf>,
    owner: Arc<Owner>,
    observer: Option<Arc<dyn ProcessObserver>>,
}

impl ProcessRunner {
    pub fn new(programs: impl IntoIterator<Item = PathBuf>) -> Result<Self, DeviceError> {
        let mut allowed = BTreeSet::new();
        for program in programs {
            if !program.is_absolute() {
                return Err(DeviceError::InvalidSpec(
                    "allowed programs must be absolute",
                ));
            }
            let program = fs::canonicalize(program)
                .map_err(|error| DeviceError::not_started("resolve allowed program", error))?;
            if !program.is_file() {
                return Err(DeviceError::InvalidSpec("allowed programs must be files"));
            }
            allowed.insert(program);
        }
        Ok(Self {
            programs: allowed,
            owner: Arc::default(),
            observer: None,
        })
    }

    /// Adds host records to future launches while keeping the original task's
    /// admission, cancellation, resource ledger, and supervisor ownership.
    pub fn observed(&self, observer: Arc<dyn ProcessObserver>) -> Self {
        Self {
            observer: Some(observer),
            ..self.clone()
        }
    }

    pub async fn run(
        &self,
        spec: ProcessSpec,
        cancellation: CancellationToken,
    ) -> Result<ProcessResult, DeviceError> {
        let spec = self.prepare(spec)?;
        self.run_inner(spec, cancellation, None).await
    }

    /// Transfers the reservation to the process supervisor.
    ///
    /// Dropping the returned future requests cancellation, but the reservation
    /// stays with the join owner until process cleanup and thread exit finish.
    pub async fn run_with_permit(
        &self,
        spec: ProcessSpec,
        cancellation: CancellationToken,
        permit: ResourcePermit,
    ) -> Result<ProcessResult, DeviceError> {
        let spec = self.prepare(spec)?;
        self.run_inner(spec, cancellation, Some(permit)).await
    }

    /// Runs with an owned open directory, without reopening its name.
    pub async fn run_in_directory(
        &self,
        spec: DirectoryProcessSpec,
        cancellation: CancellationToken,
    ) -> Result<ProcessResult, DeviceError> {
        self.run_inner(self.prepare_directory(spec)?, cancellation, None)
            .await
    }

    pub async fn run_in_directory_with_permit(
        &self,
        spec: DirectoryProcessSpec,
        cancellation: CancellationToken,
        permit: ResourcePermit,
    ) -> Result<ProcessResult, DeviceError> {
        self.run_inner(self.prepare_directory(spec)?, cancellation, Some(permit))
            .await
    }

    /// Transfers a snapshot or other owned resource to the same supervisor.
    /// Its destructor runs after managed process cleanup even on caller drop.
    /// Early-return cleanup also remains pending in drain until its destructor ends.
    /// Use `own_cleanup` for a fallible cleanup; ordinary resources keep their
    /// existing destructor semantics and cannot supply cleanup evidence to drain.
    pub async fn run_in_directory_with_resources<R: Send + 'static>(
        &self,
        spec: DirectoryProcessSpec,
        cancellation: CancellationToken,
        resources: R,
    ) -> Result<ProcessResult, DeviceError> {
        let resources = RegisteredResources {
            _resource: resources,
            _pending: self.owner.0.cleanup.register(),
        };
        let mut spec = self.prepare_directory(spec)?;
        spec.resources = Some(Box::new(resources));
        self.run_inner(spec, cancellation, None).await
    }

    /// Registers fallible resource cleanup in this runner's shared scope. It is
    /// safe to create before preparing a command: early-return cleanup failures
    /// remain visible to drain instead of being lost in a best-effort destructor.
    pub fn own_cleanup<R: ProcessCleanup>(&self, resource: R) -> CleanupResource<R> {
        CleanupResource {
            resource: Some(resource),
            pending: Some(self.owner.0.cleanup.register()),
        }
    }

    pub fn cleanup_failure(&self) -> Option<String> {
        lock(&self.owner.0.cleanup.state).failure.clone()
    }

    /// Permanently prevents admission and requests cancellation of every run.
    /// This does not establish that any process has finished.
    pub fn close(&self) {
        self.owner.0.close();
    }

    pub fn is_closed(&self) -> bool {
        lock(&self.owner.0.state).closed
            || self
                .owner
                .0
                .admission_closed
                .load(std::sync::atomic::Ordering::Acquire)
            || self.cleanup_failure().is_some()
    }

    /// Includes supervisors whose run future was dropped and cleanup is pending.
    pub fn active_count(&self) -> usize {
        let mut state = lock(&self.owner.0.state);
        Scope::reap(&mut state);
        state.supervisors.len()
    }

    /// Closes admission, cancels all runs, then joins their actual supervisors.
    /// Cancellation or dropping this future leaves the runner closed and all
    /// pending supervisors tracked; call it again to finish waiting. Registered
    /// resources must be released or transferred before awaiting drain. No deadline
    /// is inferred: unconfirmed OS cleanup must keep the owner's reservation.
    pub async fn drain(&self) -> Result<(), DeviceError> {
        self.close();
        self.owner.0.wait(None).await
    }

    fn prepare(&self, spec: ProcessSpec) -> Result<PreparedProcess, DeviceError> {
        #[cfg(not(unix))]
        {
            let _ = spec;
            Err(DeviceError::UnsupportedPlatform)
        }
        #[cfg(unix)]
        {
            if !spec.cwd.is_absolute() {
                return Err(DeviceError::InvalidSpec("cwd must be absolute"));
            }
            use std::os::unix::fs::OpenOptionsExt;
            let cwd = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NONBLOCK)
                .open(&spec.cwd)
                .map_err(|error| DeviceError::not_started("open working directory", error))?;
            self.prepare_directory(DirectoryProcessSpec {
                program: spec.program,
                args: spec.args,
                cwd,
                env: spec.env,
                timeout: spec.timeout,
                max_output_bytes: spec.max_output_bytes,
                io: spec.io,
            })
        }
    }

    fn prepare_directory(
        &self,
        spec: DirectoryProcessSpec,
    ) -> Result<PreparedProcess, DeviceError> {
        if !spec.program.is_absolute() {
            return Err(DeviceError::InvalidSpec("program must be absolute"));
        }
        if spec.timeout.is_zero() || Instant::now().checked_add(spec.timeout).is_none() {
            return Err(DeviceError::InvalidSpec(
                "timeout must be positive and representable",
            ));
        }
        if spec.max_output_bytes == 0 {
            return Err(DeviceError::InvalidSpec(
                "max_output_bytes must be positive",
            ));
        }
        if let crate::ProcessIo::Pty { columns, rows } = spec.io
            && (columns == 0 || rows == 0)
        {
            return Err(DeviceError::InvalidSpec(
                "terminal dimensions must be positive",
            ));
        }
        #[cfg(not(target_os = "macos"))]
        if matches!(spec.io, crate::ProcessIo::Pty { .. }) {
            return Err(DeviceError::UnsupportedPlatform);
        }
        if spec.args.iter().any(|argument| argument.contains('\0')) {
            return Err(DeviceError::InvalidSpec("arguments must not contain NUL"));
        }
        if spec
            .env
            .iter()
            .any(|(key, value)| key.is_empty() || key.contains(['=', '\0']) || value.contains('\0'))
        {
            return Err(DeviceError::InvalidSpec("invalid environment entry"));
        }
        let program = fs::canonicalize(&spec.program)
            .map_err(|error| DeviceError::not_started("resolve program", error))?;
        if !self.programs.contains(&program) {
            return Err(DeviceError::ProgramNotAllowed(program));
        }
        if !spec
            .cwd
            .metadata()
            .map_err(|error| DeviceError::not_started("inspect working directory", error))?
            .is_dir()
        {
            return Err(DeviceError::InvalidSpec("cwd must be a directory"));
        }
        #[cfg(unix)]
        let cwd = {
            use std::os::fd::{AsRawFd, FromRawFd};
            // The caller may own an inherited fd, even 0/1/2. Duplicate above
            // stdio with CLOEXEC before fork, so spawn's stdio setup cannot
            // replace our cwd and the executed program does not inherit it.
            // SAFETY: cwd owns this live descriptor; fcntl creates a new owned fd.
            let fd = unsafe { libc::fcntl(spec.cwd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
            if fd == -1 {
                return Err(DeviceError::not_started(
                    "protect directory descriptor",
                    std::io::Error::last_os_error(),
                ));
            }
            // SAFETY: the successful fcntl result is a new descriptor owned here.
            // The original stays alive through replacement, so both refer to
            // the same opened directory regardless of later filesystem renames.
            unsafe { fs::File::from_raw_fd(fd) }
        };
        #[cfg(not(unix))]
        let cwd = spec.cwd;
        Ok(PreparedProcess {
            spec: DirectoryProcessSpec {
                program,
                args: spec.args,
                cwd,
                env: spec.env,
                timeout: spec.timeout,
                max_output_bytes: spec.max_output_bytes,
                io: spec.io,
            },
            resources: None,
            observer: self.observer.clone(),
        })
    }

    async fn run_inner(
        &self,
        spec: PreparedProcess,
        cancellation: CancellationToken,
        permit: Option<ResourcePermit>,
    ) -> Result<ProcessResult, DeviceError> {
        #[cfg(not(unix))]
        {
            let _ = (spec, permit);
            if cancellation.is_cancelled() {
                return Err(DeviceError::Cancelled);
            }
            Err(DeviceError::UnsupportedPlatform)
        }
        #[cfg(unix)]
        {
            let running = self.launch_inner(spec, cancellation, permit, None).await?;
            let result = running
                .receiver
                .await
                .unwrap_or(Err(DeviceError::SupervisorStopped));
            self.owner.0.wait(Some(&running.completion)).await?;
            result
        }
    }

    #[cfg(unix)]
    async fn launch_inner(
        &self,
        spec: PreparedProcess,
        cancellation: CancellationToken,
        permit: Option<ResourcePermit>,
        interactive: Option<Arc<InteractiveShared>>,
    ) -> Result<Running, DeviceError> {
        let scope = &self.owner.0;
        let abandoned = interactive
            .as_ref()
            .map_or_else(CancellationToken::new, |shared| shared.cancel.clone());
        let cancel_on_drop = CancelOnDrop(Some(abandoned.clone()));
        let (sender, receiver) = tokio::sync::oneshot::channel();
        #[cfg(all(test, target_os = "macos"))]
        let install_faults = unix::tests::transfer_injection();
        // Register before acquiring admission. Even a rejected launch or
        // failed thread creation must finish resource cleanup before drain.
        let mut resources = SupervisorResources::new(spec, permit, scope.cleanup.clone());
        resources.interactive = interactive.clone();
        // Every real launch carries its original Scope's admission flag into
        // the common process owner, including ordinary and unwinding runs.
        let admission_closed = scope.admission_closed.clone();
        let cleanup_ledger = scope.cleanup.clone();
        let prepared = resources.prepared.clone();
        let completion: Arc<Mutex<ThreadCompletion>> = interactive
            .as_ref()
            .map_or_else(Arc::default, |shared| shared.completion.clone());
        // This owner is outside the Scope lock: a failed worker start may
        // drop prepared resources, whose cleanup can block or reenter Scope.
        let mut join = start_join_owner(completion.clone(), resources)
            .map_err(|error| DeviceError::not_started("start supervisor join worker", error))?;
        let start = (|| {
            // Admission, thread creation and registration have one lock with
            // close: drain cannot observe empty and then admit another run.
            let mut state = lock(&scope.state);
            if cancellation.is_cancelled() {
                return Err(DeviceError::Cancelled);
            }
            if let Some(message) = &lock(&scope.cleanup.state).failure {
                return Err(DeviceError::CleanupUnconfirmed(message.clone()));
            }
            if state.closed
                || scope
                    .admission_closed
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                return Err(DeviceError::RunnerClosed);
            }
            Scope::reap(&mut state);
            let id = state.next;
            state.next = state
                .next
                .checked_add(1)
                .ok_or(DeviceError::InvalidSpec("supervisor ID exhausted"))?;
            let supervisor_cancel = abandoned.clone();
            let thread = start_supervisor(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    #[cfg(all(test, target_os = "macos"))]
                    let _injection = install_faults();
                    let prepared = lock(&prepared).take().expect("prepared process");
                    unix::supervise_mode(
                        prepared,
                        cancellation,
                        abandoned,
                        interactive.clone(),
                        admission_closed,
                        cleanup_ledger,
                    )
                }))
                .unwrap_or(Err(DeviceError::SupervisorStopped));
                if let Some(shared) = interactive {
                    shared.stage(&result);
                }
                let _ = sender.send(result);
            });
            // Register failed starts too, until admission cleanup and the
            // join worker's permit handling have both finished.
            let error = match thread {
                Ok(thread) => {
                    join.work.as_mut().expect("join admission").thread = Some(thread);
                    None
                }
                Err(error) => Some(DeviceError::not_started("start process supervisor", error)),
            };
            state.supervisors.insert(
                id,
                Supervisor {
                    cancel: supervisor_cancel,
                    completion: completion.clone(),
                },
            );
            error.map_or(Ok(()), Err)
        })();
        drop(join);
        if let Err(error) = start {
            // Rejected admission cleaned preparation outside the Scope lock.
            // Wait for the join worker's permit handling before returning.
            scope.wait(Some(&completion)).await?;
            return Err(error);
        }
        Ok(Running {
            receiver,
            completion,
            cancel_on_drop,
        })
    }
}

#[cfg(all(test, unix))]
mod cleanup_ownership {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    struct Refused(Arc<AtomicBool>);
    impl ProcessCleanup for Refused {
        fn cleanup(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("test cleanup refusal"))
        }
    }
    impl Drop for Refused {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    #[test]
    fn failed_cleanup_does_not_own_the_scope_or_release_resources_on_last_owner_drop() {
        let runner = ProcessRunner::new([]).unwrap();
        let scope = Arc::downgrade(&runner.owner.0);
        let ledger = Arc::downgrade(&runner.owner.0.cleanup);
        let dropped = Arc::new(AtomicBool::new(false));
        let resource = runner.own_cleanup(Refused(dropped.clone()));
        drop(runner);
        assert!(
            scope.upgrade().is_none(),
            "resource kept its supervisor scope alive"
        );
        drop(resource);
        assert!(
            ledger.upgrade().is_none(),
            "failed resource formed a ledger cycle"
        );
        assert!(
            !dropped.load(Ordering::SeqCst),
            "unconfirmed resource was released"
        );
    }

    #[tokio::test]
    async fn unstarted_supervisor_retains_permit_when_its_preparation_cleanup_fails() {
        let runner = ProcessRunner::new([PathBuf::from("/bin/sleep")]).unwrap();
        let scheduler = crate::ResourceScheduler::new(crate::ResourceLimits {
            max_concurrent_tasks: 1,
            max_declared_memory_bytes: 1,
        })
        .unwrap();
        let permit = scheduler
            .try_acquire(
                crate::SessionId::new("failed-start").unwrap(),
                crate::ResourceRequest::default(),
            )
            .unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let resource = runner.own_cleanup(Refused(dropped.clone()));
        let mut prepared = runner
            .prepare(ProcessSpec {
                program: "/bin/sleep".into(),
                args: vec!["60".into()],
                cwd: std::env::temp_dir(),
                env: Default::default(),
                timeout: Duration::from_secs(10),
                max_output_bytes: 1024,
                io: crate::ProcessIo::Pipes,
            })
            .unwrap();
        prepared.resources = Some(Box::new(resource));
        // No supervisor could start; preparation cleanup precedes release.
        drop(SupervisorResources::new(
            prepared,
            Some(permit),
            runner.owner.0.cleanup.clone(),
        ));
        assert!(matches!(
            runner.drain().await,
            Err(DeviceError::CleanupUnconfirmed(_))
        ));
        assert_eq!(runner.active_count(), 0);
        assert_eq!(scheduler.usage().active_tasks, 1);
        drop(runner);
        assert_eq!(scheduler.usage().active_tasks, 1);
        assert!(!dropped.load(Ordering::SeqCst));
    }

    struct ExitBlock {
        entered: std::sync::mpsc::SyncSender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Drop for ExitBlock {
        fn drop(&mut self) {
            self.entered.send(()).unwrap();
            self.release.recv_timeout(Duration::from_secs(10)).unwrap();
        }
    }
    thread_local! {
        static EXIT: std::cell::RefCell<Option<ExitBlock>> = const { std::cell::RefCell::new(None) };
    }
    struct InstallExit(Option<ExitBlock>);
    impl ProcessCleanup for InstallExit {
        fn cleanup(&mut self) -> std::io::Result<()> {
            EXIT.with(|slot| *slot.borrow_mut() = self.0.take());
            Ok(())
        }
    }

    fn exit_prepared(
        runner: &ProcessRunner,
        entered: std::sync::mpsc::SyncSender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) -> PreparedProcess {
        let mut prepared = runner
            .prepare(ProcessSpec {
                program: "/usr/bin/true".into(),
                args: vec![],
                cwd: std::env::temp_dir(),
                env: Default::default(),
                timeout: Duration::from_secs(10),
                max_output_bytes: 1024,
                io: crate::ProcessIo::Pipes,
            })
            .unwrap();
        prepared.resources =
            Some(Box::new(runner.own_cleanup(InstallExit(Some(ExitBlock {
                entered,
                release,
            })))));
        prepared
    }

    #[tokio::test(flavor = "current_thread")]
    async fn transferred_permit_is_held_until_supervisor_tls_destruction_and_true_join_finish() {
        let runner = ProcessRunner::new([PathBuf::from("/usr/bin/true")]).unwrap();
        let scheduler = crate::ResourceScheduler::new(crate::ResourceLimits {
            max_concurrent_tasks: 1,
            max_declared_memory_bytes: 1,
        })
        .unwrap();
        let permit = scheduler
            .try_acquire(
                crate::SessionId::new("thread-exit").unwrap(),
                crate::ResourceRequest::default(),
            )
            .unwrap();
        let (entered, started) = std::sync::mpsc::sync_channel(1);
        let (release, resume) = std::sync::mpsc::sync_channel(1);
        let prepared = exit_prepared(&runner, entered, resume);
        let mut run = Box::pin(runner.run_inner(prepared, CancellationToken::new(), Some(permit)));
        std::future::poll_fn(|cx| {
            assert!(run.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        tokio::task::spawn_blocking(move || started.recv_timeout(Duration::from_secs(10)).unwrap())
            .await
            .unwrap();
        let reserved = scheduler.usage().active_tasks;
        release.send(()).unwrap();
        runner.drain().await.unwrap();
        run.await.unwrap();
        assert_eq!(
            reserved, 1,
            "permit was released before TLS cleanup finished"
        );
        assert_eq!(scheduler.usage().active_tasks, 0);
        assert_eq!(runner.active_count(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interactive_wait_and_drain_share_true_join_and_hold_permit_during_tls_exit() {
        let runner = ProcessRunner::new([PathBuf::from("/usr/bin/true")]).unwrap();
        let scheduler = crate::ResourceScheduler::new(crate::ResourceLimits {
            max_concurrent_tasks: 1,
            max_declared_memory_bytes: 1,
        })
        .unwrap();
        let permit = scheduler
            .try_acquire(
                crate::SessionId::new("interactive-tls").unwrap(),
                crate::ResourceRequest::default(),
            )
            .unwrap();
        let (entered, started) = std::sync::mpsc::sync_channel(1);
        let (release, resume) = std::sync::mpsc::sync_channel(1);
        let prepared = exit_prepared(&runner, entered, resume);
        let process = runner
            .start_interactive(
                prepared,
                InteractiveLimits {
                    max_write_bytes: 10,
                    max_buffered_stdout_bytes: 10,
                },
                CancellationToken::new(),
                permit,
            )
            .await
            .unwrap();
        tokio::task::spawn_blocking(move || started.recv_timeout(Duration::from_secs(10)).unwrap())
            .await
            .unwrap();
        let (polled, returned) = std::sync::mpsc::sync_channel(1);
        let watchdog = std::thread::spawn(move || {
            let yielded = returned.recv_timeout(Duration::from_secs(2)).is_ok();
            release.send(()).unwrap();
            yielded
        });
        let mut first = Box::pin(process.control.wait());
        let mut second = Box::pin(process.control.wait());
        let mut drain = Box::pin(runner.drain());
        let pending = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(
                first.as_mut().poll(cx).is_pending()
                    && second.as_mut().poll(cx).is_pending()
                    && drain.as_mut().poll(cx).is_pending(),
            )
        })
        .await;
        let running = process.control.is_running().unwrap();
        let reserved = scheduler.usage().active_tasks;
        drop(first);
        polled.send(()).unwrap();
        let yielded = watchdog.join().unwrap();
        let (one, two, drained) = tokio::join!(second, process.control.wait(), drain);
        assert!(one.unwrap().status.success());
        assert!(two.unwrap().status.success());
        drained.unwrap();
        assert!(pending && yielded);
        assert!(!running);
        assert_eq!(reserved, 1);
        assert_eq!(scheduler.usage().active_tasks, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interactive_failed_resource_cleanup_is_shared_with_wait_and_retains_permit() {
        let runner = ProcessRunner::new([PathBuf::from("/usr/bin/true")]).unwrap();
        let scheduler = crate::ResourceScheduler::new(crate::ResourceLimits {
            max_concurrent_tasks: 1,
            max_declared_memory_bytes: 1,
        })
        .unwrap();
        let permit = scheduler
            .try_acquire(
                crate::SessionId::new("interactive-refusal").unwrap(),
                crate::ResourceRequest::default(),
            )
            .unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let mut prepared = runner
            .prepare(ProcessSpec {
                program: "/usr/bin/true".into(),
                args: vec![],
                cwd: std::env::temp_dir(),
                env: Default::default(),
                timeout: Duration::from_secs(10),
                max_output_bytes: 10,
                io: crate::ProcessIo::Pipes,
            })
            .unwrap();
        prepared.resources = Some(Box::new(runner.own_cleanup(Refused(dropped.clone()))));
        let process = runner
            .start_interactive(
                prepared,
                InteractiveLimits {
                    max_write_bytes: 10,
                    max_buffered_stdout_bytes: 10,
                },
                CancellationToken::new(),
                permit,
            )
            .await
            .unwrap();
        let first = process.control.wait().await.unwrap_err();
        let second = process.control.wait().await.unwrap_err();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(matches!(&*first, DeviceError::CleanupUnconfirmed(_)));
        assert!(matches!(
            runner.drain().await,
            Err(DeviceError::CleanupUnconfirmed(_))
        ));
        assert!(runner.is_closed());
        assert_eq!(scheduler.usage().active_tasks, 1);
        drop(runner);
        assert_eq!(scheduler.usage().active_tasks, 1);
        assert!(!dropped.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interactive_last_runner_drop_does_not_keep_scope_alive_or_lose_true_join() {
        let runner = ProcessRunner::new([PathBuf::from("/usr/bin/true")]).unwrap();
        let scope = Arc::downgrade(&runner.owner.0);
        let scheduler = crate::ResourceScheduler::new(crate::ResourceLimits {
            max_concurrent_tasks: 1,
            max_declared_memory_bytes: 1,
        })
        .unwrap();
        let permit = scheduler
            .try_acquire(
                crate::SessionId::new("interactive-last").unwrap(),
                crate::ResourceRequest::default(),
            )
            .unwrap();
        let (entered, started) = std::sync::mpsc::sync_channel(1);
        let (release, resume) = std::sync::mpsc::sync_channel(1);
        let prepared = exit_prepared(&runner, entered, resume);
        let process = runner
            .start_interactive(
                prepared,
                InteractiveLimits {
                    max_write_bytes: 10,
                    max_buffered_stdout_bytes: 10,
                },
                CancellationToken::new(),
                permit,
            )
            .await
            .unwrap();
        tokio::task::spawn_blocking(move || started.recv_timeout(Duration::from_secs(10)).unwrap())
            .await
            .unwrap();
        drop(runner);
        let freed = scope.upgrade().is_none();
        let reserved = scheduler.usage().active_tasks;
        release.send(()).unwrap();
        process.control.wait().await.unwrap();
        assert!(freed);
        assert_eq!(reserved, 1);
        assert_eq!(scheduler.usage().active_tasks, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn last_runner_drop_keeps_join_responsibility_without_retaining_scope_or_a_ledger_cycle()
    {
        let runner = ProcessRunner::new([PathBuf::from("/usr/bin/true")]).unwrap();
        let scope = Arc::downgrade(&runner.owner.0);
        let ledger = Arc::downgrade(&runner.owner.0.cleanup);
        let (entered, started) = std::sync::mpsc::sync_channel(1);
        let (release, resume) = std::sync::mpsc::sync_channel(1);
        let prepared = exit_prepared(&runner, entered, resume);
        let mut run = Box::pin(runner.run_inner(prepared, CancellationToken::new(), None));
        std::future::poll_fn(|cx| {
            assert!(run.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        tokio::task::spawn_blocking(move || started.recv_timeout(Duration::from_secs(10)).unwrap())
            .await
            .unwrap();
        drop(run);
        drop(runner);
        let scope_released = scope.upgrade().is_none();
        let join_retained = ledger.upgrade().is_some();
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while ledger.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(scope_released, "join work retained the supervisor scope");
        assert!(join_retained, "last owner lost outstanding true-join work");
    }
}

#[cfg(all(test, unix))]
mod directory_races {
    use super::*;
    use std::collections::BTreeMap;
    use std::time::Duration;

    // Reopening the prepared pathname instead of using the opened directory
    // reads "replacement". Run both entry replacement forms deterministically.
    #[test]
    fn prepared_cwd_survives_directory_and_symlink_replacement() {
        for symlink in [false, true] {
            let path = std::env::temp_dir()
                .join(format!("cayw-cwd-race-{}-{symlink}", std::process::id()));
            fs::create_dir(&path).unwrap();
            let original = path.join("cwd");
            let moved = path.join("moved");
            let other = path.join("other");
            fs::create_dir(&original).unwrap();
            fs::create_dir(&other).unwrap();
            fs::write(original.join("marker"), "opened").unwrap();
            fs::write(other.join("marker"), "replacement").unwrap();
            let runner = ProcessRunner::new([PathBuf::from("/bin/cat")]).unwrap();
            let prepared = runner
                .prepare(ProcessSpec {
                    program: "/bin/cat".into(),
                    args: vec!["marker".into()],
                    cwd: original.clone(),
                    env: BTreeMap::new(),
                    timeout: Duration::from_secs(10),
                    max_output_bytes: 1024,
                    io: crate::ProcessIo::Pipes,
                })
                .unwrap();
            fs::rename(&original, &moved).unwrap();
            if symlink {
                std::os::unix::fs::symlink(&other, &original).unwrap();
            } else {
                fs::rename(&other, &original).unwrap();
            }
            let result =
                unix::supervise(prepared, CancellationToken::new(), CancellationToken::new())
                    .unwrap();
            fs::remove_dir_all(&path).unwrap();
            assert!(result.status.success());
            assert_eq!(result.stdout, b"opened", "symlink={symlink}");
        }
    }
}

#[cfg_attr(not(unix), allow(dead_code))]
struct CancelOnDrop(Option<CancellationToken>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(token) = &self.0 {
            token.cancel();
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::TerminationReason;
    use std::io::{self, Read};
    use std::os::fd::{AsRawFd, RawFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::time::Duration;

    const POLL_MILLISECONDS: libc::c_int = 10;
    // Escaped descendants can retain inherited descriptors. Do not wait forever
    // on pipes belonging to processes outside the group that we can manage.
    const PIPE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
    #[cfg(target_os = "macos")]
    const TERMINATION_RETRY_TIMEOUT: Duration = Duration::from_secs(1);

    // A fork can temporarily inherit another launch's CLOEXEC pipe writers.
    // Serialize Device setup through exec and closure of the parent's copies;
    // execution and cleanup remain concurrent. Host forks outside Device do not
    // participate in this lock.
    #[cfg(target_os = "macos")]
    static LAUNCH_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(target_os = "macos")]
    fn lock_launch(
        cancellation: &CancellationToken,
        abandoned: &CancellationToken,
        deadline: Instant,
    ) -> Result<std::sync::MutexGuard<'static, ()>, DeviceError> {
        loop {
            if cancellation.is_cancelled() || abandoned.is_cancelled() {
                return Err(DeviceError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(DeviceError::not_started(
                    "launch deadline reached",
                    io::Error::from_raw_os_error(libc::ETIMEDOUT),
                ));
            }
            let guard = match LAUNCH_LOCK.try_lock() {
                Ok(guard) => guard,
                Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    #[cfg(test)]
                    tests::launch_waiting();
                    std::thread::sleep(Duration::from_millis(POLL_MILLISECONDS as u64));
                    continue;
                }
            };
            if cancellation.is_cancelled() || abandoned.is_cancelled() {
                return Err(DeviceError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(DeviceError::not_started(
                    "launch deadline reached",
                    io::Error::from_raw_os_error(libc::ETIMEDOUT),
                ));
            }
            return Ok(guard);
        }
    }

    struct ProcessGroup {
        child: Child,
        identity: Arc<Mutex<interactive::LeaderIdentity>>,
        admission_closed: Arc<std::sync::atomic::AtomicBool>,
        termination_sent: bool,
        reaped: bool,
        observer: Option<Arc<dyn ProcessObserver>>,
        observed: BTreeSet<ProcessIdentity>,
        records_closed: bool,
        ledger: Arc<CleanupLedger>,
    }

    pub(super) fn identity_running(
        identity: &Mutex<interactive::LeaderIdentity>,
    ) -> io::Result<bool> {
        let mut identity = lock(identity);
        observe_identity(&mut identity)
    }

    fn observe_identity(identity: &mut interactive::LeaderIdentity) -> io::Result<bool> {
        let Some(pid) = identity.pid else {
            return Ok(false);
        };
        if identity.lost {
            return Err(io::Error::from_raw_os_error(libc::ECHILD));
        }
        if identity.exited {
            return Ok(false);
        }
        // SAFETY: this shared lock pins the original unreaped child identity.
        // WNOWAIT observes without releasing it; all reaping uses the same lock.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            identity.exited = info.si_signo != 0;
            Ok(!identity.exited)
        } else {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                identity.lost = true;
            }
            if error.kind() == io::ErrorKind::Interrupted {
                Ok(true)
            } else {
                Err(error)
            }
        }
    }

    impl ProcessGroup {
        fn record(
            &self,
            phase: &str,
            write: impl FnOnce(&dyn ProcessObserver) -> io::Result<()>,
        ) -> io::Result<()> {
            let Some(observer) = &self.observer else {
                return Ok(());
            };
            // Host callbacks run without runner or identity locks. Keep every
            // failed record sticky even if later OS cleanup succeeds, and keep
            // a callback panic from unwinding the supervisor's cleanup owner.
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write(observer.as_ref())))
                    .unwrap_or_else(|_| {
                        Err(io::Error::other(format!("process {phase} record panicked")))
                    });
            if let Err(error) = &result {
                self.admission_closed
                    .store(true, std::sync::atomic::Ordering::Release);
                lock(&self.ledger.state)
                    .failure
                    .get_or_insert_with(|| format!("process {phase} record failed: {error}"));
            }
            result
        }

        #[cfg(target_os = "macos")]
        fn record_started(&mut self) -> io::Result<()> {
            let identity = lock(&self.identity)
                .original
                .ok_or_else(|| io::Error::other("original process startup identity is absent"))?;
            self.observed.insert(identity);
            self.record("start", |observer| observer.started(&identity))
        }

        #[cfg(target_os = "macos")]
        fn record_members(&mut self) -> io::Result<()> {
            if self.observer.is_none() {
                return Ok(());
            }
            let members = process_group_members(self.child.id())?;
            let changed: Vec<_> = members
                .into_iter()
                .filter(|identity| {
                    !self
                        .observed
                        .iter()
                        .any(|known| known.same_process(*identity))
                })
                .collect();
            if !changed.is_empty() {
                self.observed.extend(changed.iter().copied());
                self.record("observation", |observer| observer.observed(&changed))?;
            }
            Ok(())
        }

        fn record_stopped(&mut self) -> io::Result<()> {
            if self.records_closed || !self.reaped {
                return Ok(());
            }
            self.records_closed = true;
            for identity in &self.observed {
                self.record("cleanup", |observer| observer.stopped(identity))?;
            }
            Ok(())
        }

        fn leader_exited(&self) -> io::Result<bool> {
            identity_running(&self.identity).map(|running| !running)
        }

        fn signal(&mut self) -> io::Result<Option<io::Error>> {
            let mut identity = lock(&self.identity);
            // Revalidate even an already observed zombie: an external waiter
            // may have reaped it since the last observation. Never signal after
            // ECHILD, including a cached exited state.
            let Some(pid) = identity.pid else {
                return Ok(None);
            };
            if pid != self.child.id() {
                return Err(io::Error::other(
                    "process identity does not match the owned child",
                ));
            }
            identity.exited = false;
            observe_identity(&mut identity)?;
            #[cfg(target_os = "macos")]
            if let Some(original) = identity.original {
                let current = ProcessIdentity::observe(pid)?;
                if !original.same_process(current) {
                    return Err(io::Error::other(
                        "process startup identity no longer matches",
                    ));
                }
            }
            if identity.lost {
                return Err(io::Error::from_raw_os_error(libc::ECHILD));
            }
            let group_error = signal_group(pid).err();
            match self.child.kill() {
                Err(error) if error.raw_os_error() != Some(libc::ESRCH) => Err(error),
                _ => Ok(group_error),
            }
        }

        fn terminate(&mut self) -> io::Result<()> {
            if self.termination_sent {
                return Ok(());
            }
            // Every numeric signal is fenced against both our own reap and an
            // observed ECHILD. The lock is released before group cleanup/retries.
            let group_error = self.signal()?;
            if let Some(error) = group_error {
                let absent = error.raw_os_error() == Some(libc::ESRCH);
                #[cfg(target_os = "macos")]
                let absent = absent
                    || (error.raw_os_error() == Some(libc::EPERM)
                        && self.retry_denied_group_signal()?);
                if !absent {
                    return Err(error);
                }
            }
            self.termination_sent = true;
            Ok(())
        }

        #[cfg(target_os = "macos")]
        fn retry_denied_group_signal(&mut self) -> io::Result<bool> {
            let deadline = Instant::now()
                .checked_add(TERMINATION_RETRY_TIMEOUT)
                .ok_or_else(|| {
                    io::Error::other("termination retry deadline is not representable")
                })?;
            while group_has_running_members(self.child.id())? {
                // Darwin may still list a dying child as running after refusing
                // the group signal. The unreaped leader pins the PGID throughout
                // these retries; a persistent denial remains an error.
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                std::thread::sleep(Duration::from_millis(5));
                match self.signal()? {
                    None => return Ok(true),
                    Some(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(true),
                    Some(error) if error.raw_os_error() == Some(libc::EPERM) => {}
                    Some(error) => return Err(error),
                }
            }
            Ok(true)
        }

        fn finish(&mut self) -> io::Result<ExitStatus> {
            self.terminate()?;
            // EOF and waiting for the leader do not prove that descendants
            // which closed their pipes have stopped. Keep the reservation until
            // the OS reports no running member in the managed process group.
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            while group_has_running_members(self.child.id())? {
                std::thread::sleep(Duration::from_millis(5));
            }
            #[cfg(target_os = "macos")]
            for identity in &self.observed {
                while identity.is_running()? {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            loop {
                let result = {
                    let mut identity = lock(&self.identity);
                    if identity.lost {
                        return Err(io::Error::from_raw_os_error(libc::ECHILD));
                    }
                    // try_wait is WNOHANG. Never block is_running behind wait().
                    match self.child.try_wait() {
                        Ok(Some(status)) => {
                            identity.pid = None;
                            self.reaped = true;
                            Ok(Some(status))
                        }
                        Err(error) => {
                            if error.raw_os_error() == Some(libc::ECHILD) {
                                identity.lost = true;
                            }
                            Err(error)
                        }
                        Ok(None) => Ok(None),
                    }
                };
                match result {
                    Ok(Some(status)) => return Ok(status),
                    Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn process_group_members(group: u32) -> io::Result<Vec<ProcessIdentity>> {
        #[cfg(all(test, target_os = "macos"))]
        if tests::member_query_error() {
            // Inject a failed group-membership syscall at its narrowest boundary.
            // The real child, supervisor, observer, and permit still run.
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        const PROC_PGRP_ONLY: u32 = 2;
        let mut capacity = 64usize;
        loop {
            let mut pids = vec![0 as libc::pid_t; capacity];
            let bytes = std::mem::size_of_val(pids.as_slice());
            let size = libc::c_int::try_from(bytes).map_err(io::Error::other)?;
            // SAFETY: pids is writable for size bytes and this is read-only.
            let count = unsafe {
                libc::proc_listpids(PROC_PGRP_ONLY, group, pids.as_mut_ptr().cast(), size)
            };
            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            if count as usize == bytes {
                capacity = capacity
                    .checked_mul(2)
                    .ok_or_else(|| io::Error::other("process group is too large"))?;
                continue;
            }
            if count as usize > bytes
                || !(count as usize).is_multiple_of(std::mem::size_of::<libc::pid_t>())
            {
                return Err(io::Error::other("incomplete process group observation"));
            }
            pids.truncate(count as usize / std::mem::size_of::<libc::pid_t>());
            let mut members = Vec::new();
            for pid in pids {
                match ProcessIdentity::observe(pid as u32) {
                    Ok(identity) if identity.process_group == group => members.push(identity),
                    Ok(_) => {}
                    Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
                    Err(error) => return Err(error),
                }
            }
            return Ok(members);
        }
    }

    impl Drop for ProcessGroup {
        fn drop(&mut self) {
            if !self.reaped {
                self.admission_closed
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            #[cfg(all(test, target_os = "macos"))]
            tests::pause_failed_cleanup();
            while !self.reaped {
                // Keep the supervisor (and its reservation) alive on cleanup
                // errors. Releasing admission after an unconfirmed kill or an
                // unreadable group state could run the next task over budget.
                if self.finish().is_err() {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            let _ = self.record_stopped();
        }
    }

    fn signal_group(group: u32) -> io::Result<()> {
        #[cfg(all(test, target_os = "macos"))]
        if tests::deny_group_signal(group) {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        // SAFETY: process_group(0) created a new group whose ID equals this
        // unreaped child's PID. It cannot be reused while we hold the child.
        if unsafe { libc::kill(-(group as libc::pid_t), libc::SIGKILL) } == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn group_has_running_members(group: u32) -> io::Result<bool> {
        #[cfg(all(test, target_os = "macos"))]
        if let Some(result) = tests::group_observation() {
            return result;
        }
        inspect_group(group)
    }

    #[cfg(target_os = "macos")]
    fn inspect_group(group: u32) -> io::Result<bool> {
        // Darwin returns EPERM when a group contains only zombies. Check the
        // actual group members before treating this as successful cleanup; a
        // persistent EPERM for a live process remains an unconfirmed termination.
        // PROC_PGRP_ONLY from Apple's sys/proc_info.h; libc does not export it.
        const PROC_PGRP_ONLY: u32 = 2;
        let mut capacity = 64usize;
        loop {
            let mut pids = vec![0 as libc::pid_t; capacity];
            let bytes = std::mem::size_of_val(pids.as_slice());
            let buffer_size = libc::c_int::try_from(bytes).map_err(io::Error::other)?;
            // SAFETY: pids is writable for buffer_size bytes. This query does
            // not modify the processes it returns and includes zombie members.
            let count = unsafe {
                libc::proc_listpids(PROC_PGRP_ONLY, group, pids.as_mut_ptr().cast(), buffer_size)
            };
            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            if count as usize == bytes {
                capacity = capacity
                    .checked_mul(2)
                    .ok_or_else(|| io::Error::other("process group is too large"))?;
                continue;
            }
            pids.truncate(count as usize / std::mem::size_of::<libc::pid_t>());
            for pid in pids {
                // SAFETY: this C POD struct is initialized by proc_pidinfo.
                let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
                let size = std::mem::size_of_val(&info) as libc::c_int;
                // SAFETY: info is writable for exactly the supplied size bytes.
                let result = unsafe {
                    libc::proc_pidinfo(
                        pid,
                        libc::PROC_PIDTBSDINFO,
                        1,
                        (&mut info as *mut libc::proc_bsdinfo).cast(),
                        size,
                    )
                };
                if result == size {
                    if info.pbi_pgid == group && info.pbi_status != libc::SZOMB {
                        return Ok(true);
                    }
                } else {
                    let error = io::Error::last_os_error();
                    if result != 0 || error.raw_os_error() != Some(libc::ESRCH) {
                        return Err(error);
                    }
                }
            }
            return Ok(false);
        }
    }

    #[cfg(target_os = "linux")]
    fn process_group_id(pid: libc::pid_t) -> io::Result<Option<u32>> {
        // SAFETY: getpgid only reads the group of this positive PID. A reused
        // candidate PID is harmless: we check its current group again in stat.
        let group = unsafe { libc::getpgid(pid) };
        if group >= 0 {
            Ok(Some(group as u32))
        } else {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn parse_process_stat(stat: &[u8]) -> io::Result<(u32, u8)> {
        let invalid = || io::Error::other("invalid process state");
        // comm is an arbitrary byte string, including whitespace, ')' and
        // invalid UTF-8. Only the fields after its LAST ')' have a text format.
        let close = stat
            .iter()
            .rposition(|byte| *byte == b')')
            .ok_or_else(invalid)?;
        let tail = stat.get(close + 1..).ok_or_else(invalid)?;
        if !tail.starts_with(b" ") {
            return Err(invalid());
        }
        let mut fields = tail
            .split(u8::is_ascii_whitespace)
            .filter(|field| !field.is_empty());
        let state = fields.next().ok_or_else(invalid)?;
        if state.len() != 1 || !state[0].is_ascii_alphabetic() {
            return Err(invalid());
        }
        let _parent = fields.next().ok_or_else(invalid)?;
        let group = fields.next().ok_or_else(invalid)?;
        let group = std::str::from_utf8(group)
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|group| *group > 0)
            .ok_or_else(invalid)?;
        Ok((group, state[0]))
    }

    #[cfg(target_os = "linux")]
    fn inspect_group(group: u32) -> io::Result<bool> {
        // Linux kill(-pgid, 0) also finds zombie members. Inspect their state so
        // orphan zombies held by a container's init do not retain reservations.
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<libc::pid_t>().ok())
                .filter(|pid| *pid > 0)
            else {
                continue;
            };
            // Do not read unrelated processes' stat files: permissions and exit
            // races elsewhere on the machine must not hold this group's permit.
            if process_group_id(pid)? != Some(group) {
                continue;
            }
            let state =
                fs::read(entry.path().join("stat")).and_then(|stat| parse_process_stat(&stat));
            match state {
                Ok((process_group, state)) => {
                    if process_group == group && !matches!(state, b'Z' | b'X' | b'x') {
                        return Ok(true);
                    }
                }
                Err(error) => {
                    // It may have exited, moved groups, or had its PID reused
                    // between getpgid and read. Ignore only a confirmed absence
                    // from this group; an unreadable member is NOT proof of death.
                    if process_group_id(pid)? == Some(group) {
                        return Err(error);
                    }
                }
            }
        }
        Ok(false)
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(super) mod tests {
        use super::*;
        use crate::{ResourceLimits, ResourceRequest, ResourceScheduler, SessionId};
        use std::cell::RefCell;
        use std::collections::BTreeMap;
        use std::io::Write;
        use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

        struct PausedCleanup {
            started: SyncSender<(u32, u32)>,
            resume: Receiver<()>,
        }

        #[derive(Default)]
        struct Faults {
            denied: bool,
            transient_observations: usize,
            observation_error: bool,
            member_query_error: bool,
            survivor: Option<Child>,
            leader: Option<u32>,
            pause: Option<PausedCleanup>,
            failed_cleanup: Option<PausedCleanup>,
            release: Option<Receiver<()>>,
            panic_after_spawn: Option<PausedCleanup>,
            before_launch_grant: Option<std::os::unix::net::UnixStream>,
            launch_waiting: Option<SyncSender<()>>,
        }

        thread_local! {
            static THREAD_START_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
            static JOIN_START_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
            static FAULTS: RefCell<Faults> = RefCell::new(Faults::default());
        }

        pub(crate) struct Injection;

        impl Injection {
            fn transient() -> Self {
                FAULTS.with(|faults| {
                    *faults.borrow_mut() = Faults {
                        denied: true,
                        transient_observations: 2,
                        ..Faults::default()
                    };
                });
                Self
            }

            fn persistent(pause: Option<PausedCleanup>) -> Self {
                FAULTS.with(|faults| {
                    *faults.borrow_mut() = Faults {
                        denied: true,
                        transient_observations: usize::MAX,
                        pause,
                        ..Faults::default()
                    };
                });
                Self
            }

            fn member_query() -> Self {
                FAULTS.with(|faults| {
                    *faults.borrow_mut() = Faults {
                        member_query_error: true,
                        ..Faults::default()
                    };
                });
                Self
            }
        }

        impl Drop for Injection {
            fn drop(&mut self) {
                FAULTS.with(|faults| {
                    let mut faults = std::mem::take(&mut *faults.borrow_mut());
                    if let Some(mut child) = faults.survivor.take() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                });
            }
        }

        pub(crate) fn transfer_injection() -> impl FnOnce() -> Injection + Send {
            let faults = FAULTS.with(|faults| std::mem::take(&mut *faults.borrow_mut()));
            move || {
                FAULTS.with(|current| *current.borrow_mut() = faults);
                Injection
            }
        }

        pub(crate) fn fail_thread_start() -> bool {
            THREAD_START_FAILURE.with(|fail| fail.replace(false))
        }

        pub(crate) fn fail_join_start() -> bool {
            JOIN_START_FAILURE.with(|fail| fail.replace(false))
        }

        pub(super) fn take_launch_pause() -> Option<std::os::unix::net::UnixStream> {
            FAULTS.with(|faults| faults.borrow_mut().before_launch_grant.take())
        }

        pub(super) fn launch_waiting() {
            if let Some(waiting) = FAULTS.with(|faults| faults.borrow_mut().launch_waiting.take()) {
                let _ = waiting.send(());
            }
        }

        pub(super) fn panic_after_spawn(pid: u32) {
            let pause = FAULTS.with(|faults| faults.borrow_mut().panic_after_spawn.take());
            if let Some(pause) = pause {
                pause.started.send((pid, pid)).unwrap();
                pause.resume.recv_timeout(Duration::from_secs(10)).unwrap();
                panic!("injected supervisor panic with live child");
            }
        }

        pub(super) fn deny_group_signal(group: u32) -> bool {
            FAULTS.with(|faults| {
                let mut faults = faults.borrow_mut();
                if faults
                    .release
                    .as_ref()
                    .is_some_and(|release| release.try_recv().is_ok())
                {
                    faults.denied = false;
                }
                if faults.denied && faults.survivor.is_none() {
                    faults.leader = Some(group);
                    // Retain a real descendant in the managed group until the
                    // observation fault is cleared. It owns no output pipes.
                    faults.survivor = Some(
                        Command::new("/bin/sleep")
                            .arg("60")
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .process_group(group as libc::pid_t)
                            .spawn()
                            .unwrap(),
                    );
                }
                faults.denied
            })
        }

        pub(super) fn pause_failed_cleanup() {
            FAULTS.with(|faults| {
                let mut faults = faults.borrow_mut();
                if let Some(pause) = faults.failed_cleanup.take() {
                    pause
                        .started
                        .send((
                            faults.leader.unwrap(),
                            faults.survivor.as_ref().unwrap().id(),
                        ))
                        .unwrap();
                    pause
                        .resume
                        .recv_timeout(Duration::from_secs(10))
                        .expect("test did not resume failed cleanup");
                }
            });
        }

        pub(super) fn member_query_error() -> bool {
            FAULTS.with(|faults| faults.borrow().member_query_error)
        }

        pub(super) fn group_observation() -> Option<io::Result<bool>> {
            FAULTS.with(|faults| {
                let mut faults = faults.borrow_mut();
                if !faults.denied {
                    return None;
                }
                if let Some(pause) = faults.pause.take() {
                    pause
                        .started
                        .send((
                            faults.leader.unwrap(),
                            faults.survivor.as_ref().unwrap().id(),
                        ))
                        .unwrap();
                    pause
                        .resume
                        .recv_timeout(Duration::from_secs(10))
                        .expect("test did not resume cleanup");
                }
                if faults.observation_error {
                    return Some(Err(io::Error::from_raw_os_error(libc::EIO)));
                }
                if faults.transient_observations > 0 {
                    faults.transient_observations -= 1;
                    // Model the delayed Darwin snapshot while also keeping a
                    // real live member, so cleanup cannot succeed by swallowing
                    // EPERM or by waiting only for the leader.
                    Some(Ok(true))
                } else {
                    faults.denied = false;
                    None
                }
            })
        }

        fn scheduler() -> ResourceScheduler {
            ResourceScheduler::new(ResourceLimits {
                max_concurrent_tasks: 1,
                max_declared_memory_bytes: 100,
            })
            .unwrap()
        }

        fn running_group() -> ProcessGroup {
            let child = Command::new("/bin/sleep")
                .arg("60")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap();
            let identity = Arc::new(Mutex::new(interactive::LeaderIdentity {
                pid: Some(child.id()),
                ..Default::default()
            }));
            ProcessGroup {
                child,
                identity,
                admission_closed: Arc::default(),
                termination_sent: false,
                reaped: false,
                observer: None,
                observed: BTreeSet::new(),
                records_closed: false,
                ledger: Arc::default(),
            }
        }

        fn assert_reaped(pid: u32) {
            let mut status = 0;
            // SAFETY: the runner must have waited for this test's direct child.
            assert_eq!(
                unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }

        #[test]
        fn finite_overflow_retains_its_reason_after_transient_group_permission_denial() {
            let scheduler = scheduler();
            let permit = scheduler
                .try_acquire(
                    SessionId::new("overflow").unwrap(),
                    ResourceRequest::default(),
                )
                .unwrap();
            let injection = Injection::transient();
            let spec = ProcessSpec {
                program: "/usr/bin/printf".into(),
                args: vec!["overflow".into()],
                cwd: std::env::temp_dir(),
                env: BTreeMap::new(),
                timeout: Duration::from_secs(10),
                max_output_bytes: 1,
                io: crate::ProcessIo::Pipes,
            };
            let runner = ProcessRunner::new([spec.program.clone()]).unwrap();
            let result = supervise(
                runner.prepare(spec).unwrap(),
                CancellationToken::new(),
                CancellationToken::new(),
            );
            drop(injection);
            let result = result.expect("transient group denial must not erase output overflow");
            assert_eq!(result.termination, TerminationReason::OutputLimit);
            assert_eq!(result.stdout, b"o");
            assert!(result.stderr.is_empty());
            assert!(!result.output_complete);
            assert_eq!(scheduler.usage().active_tasks, 1);
            drop(permit);
            assert_eq!(scheduler.usage().active_tasks, 0);
        }

        #[tokio::test]
        async fn drain_waits_for_the_same_cleanup_after_run_future_drop() {
            use std::task::Poll;
            let scheduler = scheduler();
            let permit = scheduler
                .try_acquire(SessionId::new("drain").unwrap(), ResourceRequest::default())
                .unwrap();
            let (started, started_rx) = sync_channel(1);
            let (resume, resume_rx) = sync_channel(1);
            let (release, release_rx) = sync_channel(1);
            let injection = Injection::persistent(Some(PausedCleanup {
                started,
                resume: resume_rx,
            }));
            FAULTS.with(|faults| faults.borrow_mut().release = Some(release_rx));
            let runner = ProcessRunner::new([PathBuf::from("/usr/bin/printf")]).unwrap();
            let mut run = Box::pin(runner.run_with_permit(
                ProcessSpec {
                    program: "/usr/bin/printf".into(),
                    args: vec!["overflow".into()],
                    cwd: std::env::temp_dir(),
                    env: BTreeMap::new(),
                    timeout: Duration::from_secs(10),
                    max_output_bytes: 1,
                    io: crate::ProcessIo::Pipes,
                },
                CancellationToken::new(),
                permit,
            ));
            std::future::poll_fn(|cx| {
                assert!(run.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            let (leader, survivor) = started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            drop(run);
            let mut drain = Box::pin(runner.drain());
            let drain_pending =
                std::future::poll_fn(|cx| Poll::Ready(drain.as_mut().poll(cx).is_pending())).await;
            let closed = runner.is_closed();
            let active = runner.active_count();
            let reserved = scheduler.usage().active_tasks;
            // SAFETY: zero observes the retained real member without changing it.
            let live = unsafe { libc::kill(survivor as libc::pid_t, 0) } == 0;
            // Dropping the waiter must leave the same supervisor tracked. Always
            // release the real group before assertions, even against a broken
            // drain that falsely returned early. The test can then fail cleanly.
            drop(drain);
            release.send(()).unwrap();
            resume.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(10), runner.owner.0.wait(None))
                .await
                .unwrap()
                .unwrap();
            drop(injection);
            assert!(
                drain_pending,
                "drain returned before real cleanup was released"
            );
            assert!(closed);
            assert_eq!(active, 1);
            assert_eq!(reserved, 1);
            assert!(live);
            assert_eq!(runner.active_count(), 0);
            assert_eq!(scheduler.usage().active_tasks, 0);
            assert_reaped(leader);
            assert_reaped(survivor);
        }

        #[derive(Debug, Default)]
        struct Recording(std::sync::Mutex<Vec<(&'static str, ProcessIdentity)>>);
        impl ProcessObserver for Recording {
            fn started(&self, identity: &ProcessIdentity) -> io::Result<()> {
                self.0.lock().unwrap().push(("started", *identity));
                Ok(())
            }
            fn observed(&self, identities: &[ProcessIdentity]) -> io::Result<()> {
                self.0
                    .lock()
                    .unwrap()
                    .extend(identities.iter().map(|identity| ("observed", *identity)));
                Ok(())
            }
            fn stopped(&self, identity: &ProcessIdentity) -> io::Result<()> {
                self.0.lock().unwrap().push(("stopped", *identity));
                Ok(())
            }
        }

        // A non-ESRCH failure of the group-membership query aborts the run with
        // an Io error, but it does NOT bypass cleanup: finish() confirms the
        // managed group is empty through the INDEPENDENT group-inspection query
        // (`unreadable_group_state_does_not_confirm_cleanup` proves that query is
        // itself fail-closed). The direct child is still killed and reaped, so the
        // original permit is released only after the resource is provably cleared.
        // An incarnation that left the managed group before it could be observed
        // is already outside recorded coverage. Both transports run here.
        #[tokio::test]
        async fn a_failed_group_membership_query_aborts_the_run_but_confirms_cleanup() {
            for io in [
                crate::ProcessIo::Pipes,
                crate::ProcessIo::Pty {
                    columns: 80,
                    rows: 24,
                },
            ] {
                let scheduler = scheduler();
                let permit = scheduler
                    .try_acquire(
                        SessionId::new("member-query").unwrap(),
                        ResourceRequest::default(),
                    )
                    .unwrap();
                let records = Arc::new(Recording::default());
                let runner = ProcessRunner::new([PathBuf::from("/bin/sleep")])
                    .unwrap()
                    .observed(records.clone());
                let injection = Injection::member_query();
                let result = runner
                    .run_with_permit(
                        ProcessSpec {
                            program: "/bin/sleep".into(),
                            args: vec!["60".into()],
                            cwd: std::env::temp_dir(),
                            env: BTreeMap::new(),
                            timeout: Duration::from_secs(10),
                            max_output_bytes: 1024,
                            io,
                        },
                        CancellationToken::new(),
                        permit,
                    )
                    .await;
                drop(injection);
                let drain = runner.drain().await;
                let reserved = scheduler.usage().active_tasks;
                let records = records.0.lock().unwrap().clone();
                let leader = records.first().expect("no start record").1;
                println!(
                    "MEMBER_QUERY io={io:?} result={result:?} drain={drain:?} reserved={reserved} records={records:?}"
                );
                // The host is told the run failed, so it is not misled about the
                // missing member records.
                assert!(
                    matches!(
                        result,
                        Err(DeviceError::Io {
                            operation: "record process members",
                            ..
                        })
                    ),
                    "a failed membership query must surface as an error: {result:?}"
                );
                // The same supervisor still killed and reaped the direct child, and
                // released the permit only after that confirmed cleanup.
                assert_reaped(leader.pid);
                assert_eq!(runner.active_count(), 0);
                assert!(matches!(drain, Ok(())), "{drain:?}");
                assert_eq!(reserved, 0);
                drop(runner);
                assert_eq!(scheduler.usage().active_tasks, 0);
            }
        }

        #[tokio::test]
        async fn supervisor_start_failure_pairs_reservation_without_a_thread() {
            let scheduler = scheduler();
            let permit = scheduler
                .try_acquire(
                    SessionId::new("start-failure").unwrap(),
                    ResourceRequest::default(),
                )
                .unwrap();
            let runner = ProcessRunner::new([PathBuf::from("/bin/sleep")]).unwrap();
            THREAD_START_FAILURE.with(|fail| fail.set(true));
            let error = runner
                .run_with_permit(
                    ProcessSpec {
                        program: "/bin/sleep".into(),
                        args: vec!["60".into()],
                        cwd: std::env::temp_dir(),
                        env: BTreeMap::new(),
                        timeout: Duration::from_secs(10),
                        max_output_bytes: 1024,
                        io: crate::ProcessIo::Pipes,
                    },
                    CancellationToken::new(),
                    permit,
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                DeviceError::NotStarted {
                    operation: "start process supervisor",
                    ..
                }
            ));
            assert_eq!(scheduler.usage().active_tasks, 0);
            assert_eq!(runner.active_count(), 0);
            runner.drain().await.unwrap();
        }

        #[tokio::test]
        async fn failed_supervisor_or_join_worker_creation_cleans_resources_without_holding_the_scope_mutex()
         {
            struct ScopeProbe(std::sync::Weak<Scope>, Arc<std::sync::atomic::AtomicBool>);
            impl ProcessCleanup for ScopeProbe {
                fn cleanup(&mut self) -> io::Result<()> {
                    let scope = self.0.upgrade().expect("runner still alive");
                    let available = scope.state.try_lock().is_ok();
                    self.1.store(available, std::sync::atomic::Ordering::SeqCst);
                    Err(io::Error::other("failed supervisor cleanup refusal"))
                }
            }
            for join_failure in [false, true] {
                let scheduler = scheduler();
                let permit = scheduler
                    .try_acquire(
                        SessionId::new("failed-thread").unwrap(),
                        ResourceRequest::default(),
                    )
                    .unwrap();
                let runner = ProcessRunner::new([PathBuf::from("/usr/bin/true")]).unwrap();
                let available = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let resource = runner.own_cleanup(ScopeProbe(
                    Arc::downgrade(&runner.owner.0),
                    available.clone(),
                ));
                let mut prepared = runner
                    .prepare(ProcessSpec {
                        program: "/usr/bin/true".into(),
                        args: vec![],
                        cwd: std::env::temp_dir(),
                        env: BTreeMap::new(),
                        timeout: Duration::from_secs(10),
                        max_output_bytes: 1024,
                        io: crate::ProcessIo::Pipes,
                    })
                    .unwrap();
                prepared.resources = Some(Box::new(resource));
                if join_failure {
                    JOIN_START_FAILURE.with(|fail| fail.set(true));
                } else {
                    THREAD_START_FAILURE.with(|fail| fail.set(true));
                }
                assert!(
                    runner
                        .run_inner(prepared, CancellationToken::new(), Some(permit))
                        .await
                        .is_err()
                );
                assert!(matches!(
                    runner.drain().await,
                    Err(DeviceError::CleanupUnconfirmed(_))
                ));
                assert_eq!(runner.active_count(), 0);
                assert_eq!(scheduler.usage().active_tasks, 1);
                drop(runner);
                assert_eq!(scheduler.usage().active_tasks, 1);
                assert!(
                    available.load(std::sync::atomic::Ordering::SeqCst),
                    "thread-create failure ran cleanup under the scope mutex"
                );
            }
        }

        #[tokio::test]
        async fn supervisor_panic_cleans_the_live_child_before_releasing_its_permit() {
            use std::task::Poll;
            let scheduler = scheduler();
            let permit = scheduler
                .try_acquire(SessionId::new("panic").unwrap(), ResourceRequest::default())
                .unwrap();
            let (started, started_rx) = sync_channel(1);
            let (resume, resume_rx) = sync_channel(1);
            let injection = Injection::transient();
            FAULTS.with(|faults| {
                let mut faults = faults.borrow_mut();
                faults.denied = false;
                faults.panic_after_spawn = Some(PausedCleanup {
                    started,
                    resume: resume_rx,
                });
            });
            let runner = ProcessRunner::new([PathBuf::from("/bin/sleep")]).unwrap();
            let mut run = Box::pin(runner.run_with_permit(
                ProcessSpec {
                    program: "/bin/sleep".into(),
                    args: vec!["60".into()],
                    cwd: std::env::temp_dir(),
                    env: BTreeMap::new(),
                    timeout: Duration::from_secs(10),
                    max_output_bytes: 1024,
                    io: crate::ProcessIo::Pipes,
                },
                CancellationToken::new(),
                permit,
            ));
            std::future::poll_fn(|cx| {
                assert!(run.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            let (pid, _) = started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            assert_eq!(scheduler.usage().active_tasks, 1);
            assert_eq!(runner.active_count(), 1);
            // SAFETY: observes our paused supervisor's live child.
            assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, 0);
            resume.send(()).unwrap();
            assert!(matches!(run.await, Err(DeviceError::SupervisorStopped)));
            runner.drain().await.unwrap();
            drop(injection);
            assert_reaped(pid);
            assert_eq!(scheduler.usage().active_tasks, 0);
        }

        #[tokio::test(flavor = "current_thread")]
        async fn interactive_join_and_supervisor_start_failures_leave_no_permit_or_thread() {
            for join_failure in [false, true] {
                let runner = ProcessRunner::new([PathBuf::from("/usr/bin/true")]).unwrap();
                let scheduler = scheduler();
                let permit = scheduler
                    .try_acquire(
                        SessionId::new("interactive-start-failure").unwrap(),
                        ResourceRequest::default(),
                    )
                    .unwrap();
                if join_failure {
                    JOIN_START_FAILURE.with(|fail| fail.set(true));
                } else {
                    THREAD_START_FAILURE.with(|fail| fail.set(true));
                }
                let result = runner
                    .start_interactive_in_directory_with_permit(
                        DirectoryProcessSpec {
                            program: "/usr/bin/true".into(),
                            args: vec![],
                            cwd: fs::File::open(std::env::temp_dir()).unwrap(),
                            env: BTreeMap::new(),
                            timeout: Duration::from_secs(10),
                            max_output_bytes: 10,
                            io: crate::ProcessIo::Pipes,
                        },
                        InteractiveLimits {
                            max_write_bytes: 10,
                            max_buffered_stdout_bytes: 10,
                        },
                        CancellationToken::new(),
                        permit,
                    )
                    .await;
                assert!(matches!(result, Err(DeviceError::NotStarted { .. })));
                runner.drain().await.unwrap();
                assert_eq!(runner.active_count(), 0);
                assert_eq!(scheduler.usage().active_tasks, 0);
            }
        }

        // Granting exec without rechecking cancellation would run /usr/bin/true
        // after this controlled pre-exec boundary, or release the live child slot.
        #[tokio::test(flavor = "current_thread")]
        async fn cancellation_during_real_pipe_and_pty_startup_reaps_the_ungranted_child() {
            use std::io::{Read, Write};
            for io in [
                crate::ProcessIo::Pipes,
                crate::ProcessIo::Pty {
                    columns: 80,
                    rows: 24,
                },
            ] {
                let runner = ProcessRunner::new([PathBuf::from("/usr/bin/true")]).unwrap();
                let scheduler = scheduler();
                let permit = scheduler
                    .try_acquire(
                        SessionId::new("startup").unwrap(),
                        ResourceRequest::default(),
                    )
                    .unwrap();
                let (pause, mut control) = std::os::unix::net::UnixStream::pair().unwrap();
                control
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                FAULTS.with(|faults| faults.borrow_mut().before_launch_grant = Some(pause));
                let cancel = CancellationToken::new();
                let mut run = Box::pin(runner.run_with_permit(
                    ProcessSpec {
                        program: "/usr/bin/true".into(),
                        args: vec![],
                        cwd: std::env::temp_dir(),
                        env: BTreeMap::new(),
                        timeout: Duration::from_secs(30),
                        max_output_bytes: 1024,
                        io,
                    },
                    cancel.clone(),
                    permit,
                ));
                std::future::poll_fn(|cx| {
                    assert!(run.as_mut().poll(cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
                let (pid, mut control) = tokio::task::spawn_blocking(move || {
                    let mut bytes = [0u8; std::mem::size_of::<libc::pid_t>()];
                    control.read_exact(&mut bytes).unwrap();
                    (libc::pid_t::from_ne_bytes(bytes), control)
                })
                .await
                .unwrap();
                let identity = ProcessIdentity::observe(pid as u32).unwrap();
                cancel.cancel();
                let mut drain = Box::pin(runner.drain());
                let early =
                    std::future::poll_fn(|cx| std::task::Poll::Ready(drain.as_mut().poll(cx)))
                        .await;
                let reserved = scheduler.usage().active_tasks;
                let active = runner.active_count();
                let pending = early.is_pending();
                control.write_all(&[1]).unwrap();
                let result = tokio::time::timeout(Duration::from_secs(10), &mut run)
                    .await
                    .unwrap();
                match early {
                    std::task::Poll::Ready(value) => value.unwrap(),
                    std::task::Poll::Pending => {
                        tokio::time::timeout(Duration::from_secs(10), &mut drain)
                            .await
                            .unwrap()
                            .unwrap()
                    }
                }
                println!(
                    "STARTUP_CANCEL io={io:?} identity={identity:?} reserved={reserved} active={active} result={result:?}"
                );
                assert!(pending, "startup child was omitted from drain");
                assert_eq!(reserved, 1);
                assert_eq!(active, 1);
                assert!(
                    matches!(result, Err(DeviceError::Cancelled)),
                    "cancelled pre-exec child ran the program: {result:?}"
                );
                assert_reaped(pid as u32);
                assert_eq!(scheduler.usage().active_tasks, 0);
                assert_eq!(runner.active_count(), 0);
            }
        }

        struct LaunchControl {
            socket: std::os::unix::net::UnixStream,
            released: bool,
        }
        impl LaunchControl {
            fn pair() -> (std::os::unix::net::UnixStream, Self) {
                let (child, socket) = std::os::unix::net::UnixStream::pair().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                (
                    child,
                    Self {
                        socket,
                        released: false,
                    },
                )
            }
            fn started(&mut self) -> libc::pid_t {
                let mut bytes = [0; std::mem::size_of::<libc::pid_t>()];
                self.socket.read_exact(&mut bytes).unwrap();
                libc::pid_t::from_ne_bytes(bytes)
            }
            fn ready(&self) -> bool {
                let mut descriptor = libc::pollfd {
                    fd: self.socket.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: the one descriptor and its owned socket are live.
                unsafe { libc::poll(&mut descriptor, 1, 0) > 0 }
            }
            fn release(&mut self) {
                self.socket.write_all(&[1]).unwrap();
                self.released = true;
            }
        }
        impl Drop for LaunchControl {
            fn drop(&mut self) {
                if !self.released {
                    // shutdown also wakes the child if another fork inherited
                    // this control endpoint. A failing test cannot hang exec.
                    let _ = self.socket.shutdown(std::net::Shutdown::Both);
                }
            }
        }

        // A second fork must not inherit the first launch's pipe writers or PTY
        // replicas while that first child is waiting for its original grant.
        #[tokio::test(flavor = "current_thread")]
        async fn overlapping_startup_cannot_hold_another_calls_output_open() {
            let mut observations = Vec::new();
            for io in [
                crate::ProcessIo::Pipes,
                crate::ProcessIo::Pty {
                    columns: 80,
                    rows: 24,
                },
            ] {
                let first_runner = ProcessRunner::new([PathBuf::from("/usr/bin/printf")]).unwrap();
                let second_runner = ProcessRunner::new([PathBuf::from("/usr/bin/printf")]).unwrap();
                let spec = || ProcessSpec {
                    program: "/usr/bin/printf".into(),
                    args: vec!["abc".into()],
                    cwd: std::env::temp_dir(),
                    env: BTreeMap::new(),
                    timeout: Duration::from_secs(30),
                    max_output_bytes: 3,
                    io,
                };
                let (first_pause, mut first_control) = LaunchControl::pair();
                FAULTS.with(|faults| faults.borrow_mut().before_launch_grant = Some(first_pause));
                let mut first = Box::pin(first_runner.run(spec(), CancellationToken::new()));
                std::future::poll_fn(|cx| {
                    assert!(first.as_mut().poll(cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
                let first_pid = first_control.started();

                let (second_pause, mut second_control) = LaunchControl::pair();
                let (waiting, wait_rx) = sync_channel(1);
                FAULTS.with(|faults| {
                    let mut faults = faults.borrow_mut();
                    faults.before_launch_grant = Some(second_pause);
                    faults.launch_waiting = Some(waiting);
                });
                let mut second = Box::pin(second_runner.run(spec(), CancellationToken::new()));
                std::future::poll_fn(|cx| {
                    assert!(second.as_mut().poll(cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
                let waited_before_fork = tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if wait_rx.try_recv().is_ok() {
                            break true;
                        }
                        if second_control.ready() {
                            break false;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("second launch reached neither lock nor child gate");
                first_control.release();
                let second_pid = second_control.started();
                // Keep B stopped before exec while A must reach genuine EOF.
                let first_result = tokio::time::timeout(Duration::from_secs(10), &mut first).await;
                second_control.release();
                let second_result =
                    tokio::time::timeout(Duration::from_secs(10), &mut second).await;
                first_runner.drain().await.unwrap();
                second_runner.drain().await.unwrap();
                assert_reaped(first_pid as u32);
                assert_reaped(second_pid as u32);
                let first_result = first_result.unwrap().unwrap();
                let second_result = second_result.unwrap().unwrap();
                println!(
                    "OVERLAPPING_STARTUP io={io:?} waited_before_fork={waited_before_fork} first={first_result:?} second={second_result:?}"
                );
                observations.push((waited_before_fork, first_result, second_result));
            }
            for (waited, first, second) in observations {
                assert!(
                    waited,
                    "a second Device fork entered the first launch's descriptor window"
                );
                assert_eq!(first.termination, TerminationReason::Exited);
                assert!(
                    first.output_complete,
                    "another launch held the first output open"
                );
                assert_eq!(first.stdout, b"abc");
                assert_eq!(second.termination, TerminationReason::Exited);
                assert!(second.output_complete);
                assert_eq!(second.stdout, b"abc");
            }
        }

        #[tokio::test(flavor = "current_thread")]
        async fn launch_wait_respects_original_cancel_deadline_and_reservation() {
            for cancel_waiter in [true, false] {
                let first_runner = ProcessRunner::new([PathBuf::from("/usr/bin/true")]).unwrap();
                let second_runner = ProcessRunner::new([PathBuf::from("/usr/bin/true")]).unwrap();
                let spec = |timeout| ProcessSpec {
                    program: "/usr/bin/true".into(),
                    args: vec![],
                    cwd: std::env::temp_dir(),
                    env: BTreeMap::new(),
                    timeout,
                    max_output_bytes: 10,
                    io: crate::ProcessIo::Pipes,
                };
                let (pause, mut control) = LaunchControl::pair();
                FAULTS.with(|faults| faults.borrow_mut().before_launch_grant = Some(pause));
                let mut first = Box::pin(
                    first_runner.run(spec(Duration::from_secs(30)), CancellationToken::new()),
                );
                std::future::poll_fn(|cx| {
                    assert!(first.as_mut().poll(cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
                let first_pid = control.started();
                let resources = scheduler();
                let permit = resources
                    .try_acquire(
                        SessionId::new("launch-wait").unwrap(),
                        ResourceRequest::default(),
                    )
                    .unwrap();
                let (waiting, wait_rx) = sync_channel(1);
                let (wait_pause, mut wait_control) = LaunchControl::pair();
                FAULTS.with(|faults| {
                    let mut faults = faults.borrow_mut();
                    faults.launch_waiting = Some(waiting);
                    faults.before_launch_grant = Some(wait_pause);
                });
                let cancellation = CancellationToken::new();
                let timeout = if cancel_waiter {
                    Duration::from_secs(20)
                } else {
                    Duration::from_secs(1)
                };
                let mut second = Box::pin(second_runner.run_with_permit(
                    spec(timeout),
                    cancellation.clone(),
                    permit,
                ));
                std::future::poll_fn(|cx| {
                    assert!(second.as_mut().poll(cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if wait_rx.try_recv().is_ok() {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                let reserved_while_waiting = resources.usage().active_tasks;
                if cancel_waiter {
                    cancellation.cancel();
                }
                let result = tokio::time::timeout(Duration::from_secs(10), &mut second).await;
                // The first child still owns its original ungranted launch here.
                let first_running = identity_running(&Mutex::new(interactive::LeaderIdentity {
                    pid: Some(first_pid as u32),
                    ..Default::default()
                }))
                .unwrap();
                control.release();
                let first_result = tokio::time::timeout(Duration::from_secs(10), &mut first).await;
                first_runner.drain().await.unwrap();
                second_runner.drain().await.unwrap();
                assert_reaped(first_pid as u32);
                assert!(first_result.unwrap().unwrap().status.success());
                assert!(first_running);
                assert_eq!(reserved_while_waiting, 1);
                assert_eq!(resources.usage().active_tasks, 0);
                assert_eq!(second_runner.active_count(), 0);
                let error = result.unwrap().unwrap_err();
                if cancel_waiter {
                    assert!(matches!(error, DeviceError::Cancelled));
                } else {
                    assert!(matches!(
                        error,
                        DeviceError::NotStarted {
                            operation: "launch deadline reached",
                            ..
                        }
                    ));
                }
                wait_control.socket.set_nonblocking(true).unwrap();
                let mut bytes = [0; std::mem::size_of::<libc::pid_t>()];
                let read = wait_control.socket.read(&mut bytes);
                assert!(
                    matches!(read, Ok(0))
                        || matches!(read, Err(ref error) if error.kind() == io::ErrorKind::WouldBlock),
                    "waiter forked before cancel/deadline: {read:?}"
                );
                wait_control.released = true;
                println!(
                    "LAUNCH_WAIT cancel={cancel_waiter} reserved={reserved_while_waiting} remaining={} first_running={first_running}",
                    resources.usage().active_tasks
                );
            }
        }

        #[tokio::test(flavor = "current_thread")]
        async fn interactive_start_future_drop_during_live_child_panic_keeps_cleanup_owned() {
            let runner = ProcessRunner::new([PathBuf::from("/bin/sleep")]).unwrap();
            let scheduler = scheduler();
            let permit = scheduler
                .try_acquire(
                    SessionId::new("interactive-panic").unwrap(),
                    ResourceRequest::default(),
                )
                .unwrap();
            let (entered, started) = sync_channel(1);
            let (release, resume) = sync_channel(1);
            let injection = Injection::transient();
            FAULTS.with(|faults| {
                let mut faults = faults.borrow_mut();
                faults.denied = false;
                faults.panic_after_spawn = Some(PausedCleanup {
                    started: entered,
                    resume,
                });
            });
            let mut start = Box::pin(runner.start_interactive_in_directory_with_permit(
                DirectoryProcessSpec {
                    program: "/bin/sleep".into(),
                    args: vec!["60".into()],
                    cwd: fs::File::open(std::env::temp_dir()).unwrap(),
                    env: BTreeMap::new(),
                    timeout: Duration::from_secs(10),
                    max_output_bytes: 10,
                    io: crate::ProcessIo::Pipes,
                },
                InteractiveLimits {
                    max_write_bytes: 10,
                    max_buffered_stdout_bytes: 10,
                },
                CancellationToken::new(),
                permit,
            ));
            std::future::poll_fn(|cx| {
                assert!(start.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            let (pid, _) = tokio::task::spawn_blocking(move || {
                started.recv_timeout(Duration::from_secs(10)).unwrap()
            })
            .await
            .unwrap();
            drop(start);
            let reserved = scheduler.usage().active_tasks;
            let active = runner.active_count();
            release.send(()).unwrap();
            runner.drain().await.unwrap();
            drop(injection);
            assert_eq!(reserved, 1);
            assert_eq!(active, 1);
            assert_eq!(scheduler.usage().active_tasks, 0);
            assert_reaped(pid);
        }

        #[tokio::test(flavor = "current_thread")]
        async fn interactive_persistent_cleanup_closes_admission_keeps_wait_pending_and_retains_permit()
         {
            let runner = ProcessRunner::new([PathBuf::from("/usr/bin/printf")]).unwrap();
            let scheduler = scheduler();
            let permit = scheduler
                .try_acquire(
                    SessionId::new("interactive-denied").unwrap(),
                    ResourceRequest::default(),
                )
                .unwrap();
            let (failed, failed_rx) = sync_channel(1);
            let (resume, resume_rx) = sync_channel(1);
            let (release, release_rx) = sync_channel(1);
            let injection = Injection::persistent(None);
            FAULTS.with(|faults| {
                let mut faults = faults.borrow_mut();
                faults.failed_cleanup = Some(PausedCleanup {
                    started: failed,
                    resume: resume_rx,
                });
                faults.release = Some(release_rx);
            });
            let process = runner
                .start_interactive_in_directory_with_permit(
                    DirectoryProcessSpec {
                        program: "/usr/bin/printf".into(),
                        args: vec!["overflow".into()],
                        cwd: fs::File::open(std::env::temp_dir()).unwrap(),
                        env: BTreeMap::new(),
                        timeout: Duration::from_secs(10),
                        max_output_bytes: 1,
                        io: crate::ProcessIo::Pipes,
                    },
                    InteractiveLimits {
                        max_write_bytes: 10,
                        max_buffered_stdout_bytes: 10,
                    },
                    CancellationToken::new(),
                    permit,
                )
                .await
                .unwrap();
            let (leader, survivor) = tokio::task::spawn_blocking(move || {
                failed_rx.recv_timeout(Duration::from_secs(10)).unwrap()
            })
            .await
            .unwrap();
            let closed_before_drain = runner.is_closed();
            let reserved = scheduler.usage().active_tasks;
            let clone = runner.clone();
            let ordinary = clone
                .run(
                    ProcessSpec {
                        program: "/usr/bin/printf".into(),
                        args: vec!["ok".into()],
                        cwd: std::env::temp_dir(),
                        env: BTreeMap::new(),
                        timeout: Duration::from_secs(10),
                        max_output_bytes: 10,
                        io: crate::ProcessIo::Pipes,
                    },
                    CancellationToken::new(),
                )
                .await;
            let (interactive, interactive_reserved) = interactive_admission_probe(&clone).await;
            let mut wait = Box::pin(process.control.wait());
            let wait_pending = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(wait.as_mut().poll(cx).is_pending())
            })
            .await;
            let mut drain = Box::pin(runner.drain());
            let drain_pending = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(drain.as_mut().poll(cx).is_pending())
            })
            .await;
            drop(wait);
            drop(drain);
            release.send(()).unwrap();
            resume.send(()).unwrap();
            let outcome = process.control.wait().await;
            let drained = runner.drain().await;
            drop(injection);
            assert_reaped(leader);
            assert_reaped(survivor);
            assert!(
                matches!(outcome, Err(error) if matches!(&*error, DeviceError::Io { operation: "terminate process group", .. }))
            );
            drained.unwrap();
            assert!(
                closed_before_drain,
                "unconfirmed OS cleanup left admission open"
            );
            assert!(matches!(ordinary, Err(DeviceError::RunnerClosed)));
            assert!(matches!(interactive, Err(DeviceError::RunnerClosed)));
            assert_eq!(interactive_reserved, 0);
            assert!(wait_pending && drain_pending);
            assert_eq!(reserved, 1);
            assert_eq!(scheduler.usage().active_tasks, 0);
        }

        // Probe actual public interactive admission, including its own permit and
        // joined outcome if a broken runner unexpectedly accepts the launch.
        async fn interactive_admission_probe(
            runner: &ProcessRunner,
        ) -> (Result<crate::ProcessExit, DeviceError>, usize) {
            let scheduler = scheduler();
            let permit = scheduler
                .try_acquire(
                    SessionId::new("admission-probe").unwrap(),
                    ResourceRequest::default(),
                )
                .unwrap();
            let start = runner
                .start_interactive_in_directory_with_permit(
                    DirectoryProcessSpec {
                        program: "/usr/bin/printf".into(),
                        args: vec!["ok".into()],
                        cwd: fs::File::open(std::env::temp_dir()).unwrap(),
                        env: BTreeMap::new(),
                        timeout: Duration::from_secs(10),
                        max_output_bytes: 10,
                        io: crate::ProcessIo::Pipes,
                    },
                    InteractiveLimits {
                        max_write_bytes: 10,
                        max_buffered_stdout_bytes: 10,
                    },
                    CancellationToken::new(),
                    permit,
                )
                .await;
            let outcome = match start {
                Ok(process) => process
                    .control
                    .wait()
                    .await
                    .map_err(|error| interactive::copy_error(&error)),
                Err(error) => Err(error),
            };
            (outcome, scheduler.usage().active_tasks)
        }

        #[tokio::test(flavor = "current_thread")]
        async fn independent_noninteractive_cleanup_failure_closes_shared_admission() {
            let runner = ProcessRunner::new([PathBuf::from("/usr/bin/printf")]).unwrap();
            let scheduler = scheduler();
            let permit = scheduler
                .try_acquire(
                    SessionId::new("independent-cleanup-denial").unwrap(),
                    ResourceRequest::default(),
                )
                .unwrap();
            let (failed, failed_rx) = sync_channel(1);
            let (resume, resume_rx) = sync_channel(1);
            let (release, release_rx) = sync_channel(1);
            let injection = Injection::persistent(None);
            FAULTS.with(|faults| {
                let mut faults = faults.borrow_mut();
                faults.failed_cleanup = Some(PausedCleanup {
                    started: failed,
                    resume: resume_rx,
                });
                faults.release = Some(release_rx);
            });
            let make_spec = || ProcessSpec {
                program: "/usr/bin/printf".into(),
                args: vec!["overflow".into()],
                cwd: std::env::temp_dir(),
                env: BTreeMap::new(),
                timeout: Duration::from_secs(10),
                max_output_bytes: 1,
                io: crate::ProcessIo::Pipes,
            };
            let mut run =
                Box::pin(runner.run_with_permit(make_spec(), CancellationToken::new(), permit));
            std::future::poll_fn(|cx| {
                assert!(run.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            let (leader, survivor) = tokio::task::spawn_blocking(move || {
                failed_rx.recv_timeout(Duration::from_secs(10)).unwrap()
            })
            .await
            .unwrap();
            let closed = runner.is_closed();
            let reserved = scheduler.usage().active_tasks;
            let clone = runner.clone();
            let next = clone.run(make_spec(), CancellationToken::new()).await;
            let rejected = matches!(next, Err(DeviceError::RunnerClosed));
            let (interactive, interactive_reserved) = interactive_admission_probe(&clone).await;
            let mut pending_run = run.as_mut();
            let run_pending = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(pending_run.as_mut().poll(cx).is_pending())
            })
            .await;
            let independent = ProcessRunner::new([PathBuf::from("/usr/bin/printf")]).unwrap();
            let isolated = independent.run(make_spec(), CancellationToken::new()).await;
            let (isolated_interactive, isolated_reserved) =
                interactive_admission_probe(&independent).await;
            // Release only our registered injection before any assertion. The
            // actual Device owner stops/reaps both owned children and joins.
            release.send(()).unwrap();
            resume.send(()).unwrap();
            let original = run.await;
            // Check sticky failure before drain itself closes the Scope.
            let closed_after_cleanup = runner.is_closed();
            let next_after_cleanup = clone.run(make_spec(), CancellationToken::new()).await;
            let (interactive_after_cleanup, after_reserved) =
                interactive_admission_probe(&clone).await;
            let drained = runner.drain().await;
            let isolated_drained = independent.drain().await;
            drop(injection);
            assert_reaped(leader);
            assert_reaped(survivor);
            assert_eq!(scheduler.usage().active_tasks, 0);
            println!(
                "NONINTERACTIVE_DENIAL closed={closed} reserved={reserved} next={next:?} interactive={interactive:?} run_pending={run_pending} original={original:?} closed_after_cleanup={closed_after_cleanup} next_after_cleanup={next_after_cleanup:?} interactive_after_cleanup={interactive_after_cleanup:?} isolated={isolated:?} isolated_interactive={isolated_interactive:?}"
            );
            assert!(
                closed && rejected,
                "unconfirmed normal-run cleanup admitted another process"
            );
            assert!(matches!(interactive, Err(DeviceError::RunnerClosed)));
            assert_eq!(interactive_reserved, 0);
            assert_eq!(reserved, 1);
            assert!(run_pending);
            assert!(matches!(
                original,
                Err(DeviceError::Io { operation: "terminate process group", source })
                    if source.raw_os_error() == Some(libc::EPERM)
            ));
            assert!(closed_after_cleanup);
            assert!(matches!(next_after_cleanup, Err(DeviceError::RunnerClosed)));
            assert!(matches!(
                interactive_after_cleanup,
                Err(DeviceError::RunnerClosed)
            ));
            assert_eq!(after_reserved, 0);
            assert!(
                matches!(isolated, Ok(result) if result.termination == TerminationReason::OutputLimit)
            );
            assert!(matches!(isolated_interactive, Ok(result) if result.status.success()));
            assert_eq!(isolated_reserved, 0);
            drained.unwrap();
            isolated_drained.unwrap();
            assert_eq!(runner.active_count(), 0);
        }

        // Returning as soon as retained bytes reach the limit would leave one
        // or both OS pipes unread. The retained prefix and raw bytes are separate.
        #[test]
        fn truncated_output_still_drains_both_real_pipes_to_eof() {
            use std::io::Write;
            let (mut out_writer, out_reader) = std::os::unix::net::UnixStream::pair().unwrap();
            let (mut err_writer, err_reader) = std::os::unix::net::UnixStream::pair().unwrap();
            out_writer.write_all(b"stdout-after-budget").unwrap();
            err_writer.write_all(b"stderr-after-budget").unwrap();
            out_writer.shutdown(std::net::Shutdown::Write).unwrap();
            err_writer.shutdown(std::net::Shutdown::Write).unwrap();
            let mut stdout = Pipe::new(out_reader, PipeSink::Collected).unwrap();
            let mut stderr = Pipe::new(err_reader, PipeSink::Collected).unwrap();
            let mut budget = OutputBudget {
                remaining: 3,
                read: 0,
            };
            let out_overflow = stdout.drain(&mut budget).unwrap();
            let err_overflow = stderr.drain(&mut budget).unwrap();
            assert!(out_overflow && err_overflow);
            assert_eq!(stdout.bytes, b"std");
            assert!(stderr.bytes.is_empty());
            assert_eq!(budget.remaining, 0);
            assert_eq!(budget.read, 38);
            assert!(stdout.reader.is_none() && stderr.reader.is_none());
        }

        #[test]
        fn stdout_and_stderr_read_errors_and_raw_counter_exhaustion_are_not_discarded() {
            struct FailingReader(fs::File);
            impl AsRawFd for FailingReader {
                fn as_raw_fd(&self) -> RawFd {
                    self.0.as_raw_fd()
                }
            }
            impl Read for FailingReader {
                fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                    Err(io::Error::from_raw_os_error(libc::EIO))
                }
            }
            for sink in [
                PipeSink::Discard,
                PipeSink::Stdout(InteractiveShared::new(InteractiveLimits {
                    max_write_bytes: 10,
                    max_buffered_stdout_bytes: 10,
                })),
            ] {
                let mut pipe =
                    Pipe::new(FailingReader(fs::File::open("/dev/null").unwrap()), sink).unwrap();
                let mut budget = OutputBudget {
                    remaining: 10,
                    read: 0,
                };
                assert_eq!(
                    pipe.drain(&mut budget).unwrap_err().raw_os_error(),
                    Some(libc::EIO)
                );
            }
            let mut pipe =
                Pipe::new(fs::File::open("/dev/zero").unwrap(), PipeSink::Discard).unwrap();
            let mut budget = OutputBudget {
                remaining: 10,
                read: usize::MAX,
            };
            assert!(pipe.drain(&mut budget).is_err());
        }

        #[test]
        fn stdout_capacity_is_enforced_at_the_original_pipe_read() {
            use std::io::Write;
            let (mut producer, consumer) = std::os::unix::net::UnixStream::pair().unwrap();
            let shared = InteractiveShared::new(InteractiveLimits {
                max_write_bytes: 10,
                max_buffered_stdout_bytes: 3,
            });
            let mut pipe = Pipe::new(consumer, PipeSink::Stdout(shared.clone())).unwrap();
            producer.write_all(b"abcdef").unwrap();
            let mut budget = OutputBudget {
                remaining: 10,
                read: 0,
            };
            assert!(!pipe.drain(&mut budget).unwrap());
            assert_eq!(budget.read, 3);
            assert_eq!(&lock(&shared.io).stdout, b"abc");
            assert_eq!(pipe.fd(), -1);
            assert!(!pipe.drain(&mut budget).unwrap());
            assert_eq!(budget.read, 3);
            lock(&shared.io).stdout.clear();
            assert!(!pipe.drain(&mut budget).unwrap());
            assert_eq!(budget.read, 6);
            assert_eq!(&lock(&shared.io).stdout, b"def");
        }

        #[test]
        fn replacing_a_leader_pid_cannot_signal_another_live_process_group() {
            let mut original = running_group();
            let mut unrelated = running_group();
            let pid = original.child.id();
            let replacement = unrelated.child.id();
            lock(&original.identity).pid = Some(replacement);
            let stopped = original.terminate();
            let original_alive = identity_running(&Mutex::new(interactive::LeaderIdentity {
                pid: Some(pid),
                ..Default::default()
            }))
            .unwrap();
            let unrelated_alive = identity_running(&unrelated.identity).unwrap();
            // Restore only this test's injected identity before cleaning both real
            // children. Assertions run after cleanup even against the broken path.
            lock(&original.identity).pid = Some(pid);
            original.termination_sent = false;
            original.finish().unwrap();
            unrelated.finish().unwrap();
            assert!(stopped.is_err(), "a replaced identity was accepted");
            assert!(
                original_alive,
                "mismatch sent a signal to the original leader"
            );
            assert!(
                unrelated_alive,
                "mismatch killed unrelated group {replacement}"
            );
        }

        #[test]
        fn a_replaced_start_time_cannot_signal_the_same_numeric_pid() {
            let mut group = running_group();
            let actual = ProcessIdentity::observe(group.child.id()).unwrap();
            lock(&group.identity).original = Some(ProcessIdentity {
                started_microseconds: (actual.started_microseconds + 1) % 1_000_000,
                ..actual
            });
            let result = group.terminate();
            let alive = identity_running(&group.identity).unwrap();
            lock(&group.identity).original = Some(actual);
            group.termination_sent = false;
            group.finish().unwrap();
            assert!(
                result.is_err(),
                "the same PID with a different startup identity was accepted"
            );
            assert!(alive, "a mismatched incarnation was signalled");
        }

        #[test]
        fn external_reap_revokes_numeric_signals_even_before_supervisor_observation() {
            let mut group = running_group();
            group.child.kill().unwrap();
            let mut status = 0;
            // SAFETY: deliberately simulate a foreign SIGCHLD reaper on our own
            // direct child. No unrelated PID or group is involved.
            assert_eq!(
                unsafe { libc::waitpid(group.child.id() as libc::pid_t, &mut status, 0) },
                group.child.id() as libc::pid_t
            );
            let result = group.terminate();
            // There is no child left to clean up; avoid the intentional unknown
            // identity quarantine in Drop after recording behavior.
            group.reaped = true;
            assert!(matches!(result, Err(ref error) if error.raw_os_error() == Some(libc::ECHILD)));
            assert!(
                lock(&group.identity).lost,
                "external reap must revoke the original identity before any signal"
            );
        }

        #[test]
        fn transient_group_permission_denial_is_retried_before_cleanup_is_confirmed() {
            let scheduler = scheduler();
            let permit = scheduler
                .try_acquire(
                    SessionId::new("cleanup").unwrap(),
                    ResourceRequest::default(),
                )
                .unwrap();
            let mut group = running_group();
            let pid = group.child.id();
            let injection = Injection::transient();
            let termination = group.terminate();
            // Always clear the faults and stop both children before assertions,
            // including when run against the unfixed implementation.
            drop(injection);
            drop(group);
            assert_reaped(pid);
            assert_eq!(scheduler.usage().active_tasks, 1);
            drop(permit);
            assert_eq!(scheduler.usage().active_tasks, 0);
            termination.expect("a transient group denial must be observed again before failing");
        }

        #[test]
        fn persistent_permission_denial_keeps_the_group_unconfirmed() {
            let scheduler = scheduler();
            let permit = scheduler
                .try_acquire(
                    SessionId::new("denied").unwrap(),
                    ResourceRequest::default(),
                )
                .unwrap();
            let mut group = running_group();
            let injection = Injection::persistent(None);
            let error = group.terminate().unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EPERM));
            assert!(!group.termination_sent);
            assert!(!group.reaped);
            assert_eq!(scheduler.usage().active_tasks, 1);
            FAULTS.with(|faults| {
                let mut faults = faults.borrow_mut();
                let child = faults.survivor.as_mut().unwrap();
                assert!(child.try_wait().unwrap().is_none());
                // Successful direct-child kill does not confirm descendants.
                assert!(group.leader_exited().unwrap());
            });
            drop(injection);
            let pid = group.child.id();
            group.finish().unwrap();
            assert_reaped(pid);
            drop(group);
            drop(permit);
            assert_eq!(scheduler.usage().active_tasks, 0);
        }

        #[test]
        fn unreadable_group_state_does_not_confirm_cleanup() {
            let mut group = running_group();
            let injection = Injection::persistent(None);
            FAULTS.with(|faults| faults.borrow_mut().observation_error = true);
            let error = group.terminate().unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EIO));
            assert!(!group.termination_sent);
            assert!(!group.reaped);
            drop(injection);
            let pid = group.child.id();
            group.finish().unwrap();
            assert_reaped(pid);
        }

        #[tokio::test]
        async fn supervisor_holds_the_permit_during_denied_cleanup_and_reaps_before_release() {
            use std::task::Poll;

            let scheduler = scheduler();
            let permit = scheduler
                .try_acquire(SessionId::new("held").unwrap(), ResourceRequest::default())
                .unwrap();
            let (started, started_rx) = sync_channel(1);
            let (resume, resume_rx) = sync_channel(1);
            let (release, release_rx) = sync_channel(1);
            let (failed, failed_rx) = sync_channel(1);
            let (resume_failed, resume_failed_rx) = sync_channel(1);
            let injection = Injection::persistent(Some(PausedCleanup {
                started,
                resume: resume_rx,
            }));
            FAULTS.with(|faults| {
                let mut faults = faults.borrow_mut();
                faults.release = Some(release_rx);
                faults.failed_cleanup = Some(PausedCleanup {
                    started: failed,
                    resume: resume_failed_rx,
                });
            });
            let runner = ProcessRunner::new([PathBuf::from("/usr/bin/printf")]).unwrap();
            let spec = ProcessSpec {
                program: "/usr/bin/printf".into(),
                args: vec!["overflow".into()],
                cwd: std::env::temp_dir(),
                env: BTreeMap::new(),
                timeout: Duration::from_secs(10),
                max_output_bytes: 1,
                io: crate::ProcessIo::Pipes,
            };
            let run = runner.run_with_permit(spec, CancellationToken::new(), permit);
            tokio::pin!(run);
            std::future::poll_fn(|cx| {
                assert!(run.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            let (leader, survivor) = started_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("cleanup did not reach group inspection");
            let reserved = scheduler.usage().active_tasks;
            let blocked = matches!(
                scheduler.try_acquire(SessionId::new("next").unwrap(), ResourceRequest::default()),
                Err(DeviceError::ResourcesBusy)
            );
            // SAFETY: signal zero only observes the deliberately retained child.
            let survivor_running = unsafe { libc::kill(survivor as libc::pid_t, 0) } == 0;
            resume.send(()).unwrap();
            let failed_group = failed_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("persistent denial never reached fallback cleanup");
            let still_reserved = scheduler.usage().active_tasks;
            // SAFETY: signal zero only observes the still-retained child.
            let still_running = unsafe { libc::kill(survivor as libc::pid_t, 0) } == 0;
            // End denial only after the bounded attempt returned an error and
            // Drop took over, with no scheduling assumption about child exit.
            release.send(()).unwrap();
            resume_failed.send(()).unwrap();
            let error = tokio::time::timeout(Duration::from_secs(10), run)
                .await
                .expect("cleanup did not finish after fault removal")
                .unwrap_err();
            drop(injection);
            assert_eq!(reserved, 1);
            assert!(blocked);
            assert!(survivor_running);
            assert_eq!(failed_group, (leader, survivor));
            assert_eq!(still_reserved, 1);
            assert!(still_running);
            assert!(matches!(
                error,
                DeviceError::Io { operation: "terminate process group", source }
                    if source.raw_os_error() == Some(libc::EPERM)
            ));
            assert_reaped(leader);
            assert_reaped(survivor);
            assert_eq!(scheduler.usage().active_tasks, 0);
        }
    }

    #[cfg(target_os = "macos")]
    struct Terminal {
        primary: fs::File,
        replica: fs::File,
    }

    #[cfg(target_os = "macos")]
    impl Terminal {
        fn open(columns: u16, rows: u16) -> io::Result<Self> {
            use std::os::fd::FromRawFd;
            use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
            // Set CLOEXEC atomically at open, before any concurrent spawn. Keep
            // every descriptor above stdio even if this host closed 0, 1 or 2.
            fn protect(file: fs::File) -> io::Result<fs::File> {
                let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
                if fd == -1 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: fcntl returned a new owned descriptor.
                Ok(unsafe { fs::File::from_raw_fd(fd) })
            }
            let primary = protect(
                fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
                    .open("/dev/ptmx")?,
            )?;
            // SAFETY: these operate on this newly opened primary, not host stdio.
            if unsafe { libc::grantpt(primary.as_raw_fd()) } == -1
                || unsafe { libc::unlockpt(primary.as_raw_fd()) } == -1
            {
                return Err(io::Error::last_os_error());
            }
            // Darwin's TIOCPTYGNAME fills a caller-owned 128-byte buffer, avoiding
            // ptsname's shared static storage during concurrent launches.
            let mut name = [0u8; 128];
            if unsafe {
                libc::ioctl(
                    primary.as_raw_fd(),
                    libc::TIOCPTYGNAME.into(),
                    name.as_mut_ptr(),
                )
            } == -1
            {
                return Err(io::Error::last_os_error());
            }
            let name = std::ffi::CStr::from_bytes_until_nul(&name).map_err(io::Error::other)?;
            let replica = protect(
                fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
                    .open(std::ffi::OsStr::from_bytes(name.to_bytes()))?,
            )?;
            let size = libc::winsize {
                ws_row: rows,
                ws_col: columns,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            // SAFETY: size is a live winsize and this fd owns the replica.
            if unsafe { libc::ioctl(replica.as_raw_fd(), libc::TIOCSWINSZ, &size) } == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { primary, replica })
        }
    }

    struct OutputBudget {
        remaining: usize,
        read: usize,
    }

    enum PipeSink {
        Collected,
        Stdout(Arc<InteractiveShared>),
        Discard,
    }

    struct Pipe<T> {
        reader: Option<T>,
        bytes: Vec<u8>,
        sink: PipeSink,
        cleaning: bool,
        discarded: bool,
        terminal_eof: bool,
    }

    impl<T: Read + AsRawFd> Pipe<T> {
        fn new(reader: T, sink: PipeSink) -> io::Result<Self> {
            nonblocking(&reader)?;
            Ok(Self {
                reader: Some(reader),
                bytes: Vec::new(),
                sink,
                cleaning: false,
                discarded: false,
                terminal_eof: false,
            })
        }

        fn absent(sink: PipeSink) -> Self {
            Self {
                reader: None,
                bytes: Vec::new(),
                sink,
                cleaning: false,
                discarded: false,
                terminal_eof: false,
            }
        }

        fn fd(&self) -> RawFd {
            if !self.cleaning
                && let PipeSink::Stdout(shared) = &self.sink
                && shared.stdout_room() == 0
            {
                return -1;
            }
            self.reader.as_ref().map_or(-1, AsRawFd::as_raw_fd)
        }

        fn drain(&mut self, budget: &mut OutputBudget) -> io::Result<bool> {
            let mut overflow = false;
            // Bound each turn so a continuously writing stdout cannot starve
            // stderr, cancellation, or the timeout check.
            for _ in 0..16 {
                let Some(reader) = self.reader.as_mut() else {
                    break;
                };
                let room = match &self.sink {
                    PipeSink::Stdout(shared) if !self.cleaning => shared.stdout_room(),
                    _ => 8192,
                };
                if room == 0 {
                    break;
                }
                let mut buffer = [0; 8192];
                let length = room.min(buffer.len());
                match reader.read(&mut buffer[..length]) {
                    Ok(0) => {
                        self.reader = None;
                        if let PipeSink::Stdout(shared) = &self.sink {
                            shared.stdout_eof();
                        }
                        break;
                    }
                    Ok(count) => {
                        budget.read = budget
                            .read
                            .checked_add(count)
                            .ok_or_else(|| io::Error::other("raw output byte count exhausted"))?;
                        let keep = count.min(budget.remaining);
                        match &self.sink {
                            PipeSink::Collected => {
                                self.bytes
                                    .try_reserve_exact(keep)
                                    .map_err(io::Error::other)?;
                                self.bytes.extend_from_slice(&buffer[..keep]);
                            }
                            PipeSink::Stdout(shared) => {
                                self.discarded |= !shared.push_stdout(&buffer[..keep])?
                            }
                            PipeSink::Discard => {}
                        }
                        budget.remaining = budget
                            .remaining
                            .checked_sub(keep)
                            .ok_or_else(|| io::Error::other("output budget underflow"))?;
                        overflow |= keep != count;
                    }
                    Err(error) if self.terminal_eof && error.raw_os_error() == Some(libc::EIO) => {
                        // A PTY primary reports EIO after the last replica closes.
                        self.reader = None;
                        if let PipeSink::Stdout(shared) = &self.sink {
                            shared.stdout_eof();
                        }
                        break;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
            Ok(overflow)
        }
    }

    fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
        // SAFETY: pipe owns this live descriptor; fcntl changes only its flags.
        let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn write_pending(
        stdin: &mut impl std::io::Write,
        shared: &InteractiveShared,
    ) -> io::Result<()> {
        let mut state = lock(&shared.io);
        if state.stopping {
            return Ok(());
        }
        for _ in 0..16 {
            let Some(write) = state.pending.as_mut() else {
                break;
            };
            match stdin.write(&write.bytes[write.offset..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "stdin wrote zero bytes",
                    ));
                }
                Ok(count) => {
                    write.offset = write
                        .offset
                        .checked_add(count)
                        .ok_or_else(|| io::Error::other("stdin byte count exhausted"))?;
                    if write.offset == write.bytes.len() {
                        let write = state.pending.take().expect("pending write");
                        let _ = write.completion.send(Ok(()));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn wait_for_pipes(stdout: RawFd, stderr: RawFd, stdin: RawFd) -> io::Result<()> {
        let mut descriptors = [
            libc::pollfd {
                fd: stdin,
                events: libc::POLLOUT,
                revents: 0,
            },
            libc::pollfd {
                fd: stdout,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stderr,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: descriptors is a live array of the specified length; poll
        // neither closes nor takes ownership of the pipe descriptors.
        if unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                POLL_MILLISECONDS,
            )
        } == -1
        {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        Ok(())
    }

    // Build argv/envp in the parent. execvp/Command's ENOEXEC fallback may run
    // /bin/sh for arbitrary executable text; direct execve never does that.
    struct ExecImage {
        program: std::ffi::CString,
        _args: Vec<std::ffi::CString>,
        _env: Vec<std::ffi::CString>,
        argv: Vec<*const libc::c_char>,
        envp: Vec<*const libc::c_char>,
    }
    // SAFETY: the pointers only reference immutable CString allocations owned
    // by this image. Moving it does not move those allocations; no field can be
    // mutated after construction. The forked child reads them only in execve.
    unsafe impl Send for ExecImage {}
    // SAFETY: shared access never mutates either owned strings or pointer arrays.
    unsafe impl Sync for ExecImage {}

    impl ExecImage {
        fn new(
            program: &std::path::Path,
            args: &[String],
            env: &std::collections::BTreeMap<String, String>,
        ) -> Result<Self, DeviceError> {
            use std::ffi::CString;
            use std::os::unix::ffi::OsStrExt;
            let invalid = |_| DeviceError::InvalidSpec("exec bytes must not contain NUL");
            let program = CString::new(program.as_os_str().as_bytes()).map_err(invalid)?;
            let mut arguments = vec![program.clone()];
            for argument in args {
                arguments.push(CString::new(argument.as_bytes()).map_err(invalid)?);
            }
            let environment = env
                .iter()
                .map(|(key, value)| CString::new(format!("{key}={value}")))
                .collect::<Result<Vec<_>, _>>()
                .map_err(invalid)?;
            let mut argv: Vec<_> = arguments.iter().map(|arg| arg.as_ptr()).collect();
            argv.push(std::ptr::null());
            let mut envp: Vec<_> = environment.iter().map(|value| value.as_ptr()).collect();
            envp.push(std::ptr::null());
            Ok(Self {
                program,
                _args: arguments,
                _env: environment,
                argv,
                envp,
            })
        }

        fn execute(&self) -> io::Error {
            // SAFETY: program and the null-terminated pointer arrays reference
            // owned NUL-terminated bytes alive through this call. execve is
            // async-signal-safe and does not return on successful replacement.
            unsafe {
                libc::execve(
                    self.program.as_ptr(),
                    self.argv.as_ptr(),
                    self.envp.as_ptr(),
                );
            }
            io::Error::last_os_error()
        }
    }

    #[derive(Debug)]
    enum LaunchDecision {
        Allowed,
        Cancelled,
        TimedOut,
    }

    // Command::spawn waits for exec, so the supervisor itself cannot observe a
    // cancellation while the child is in pre_exec. This parent-side gate stays
    // responsive and grants launch only after the child's final fchdir returns.
    // The grant is the launch boundary: cancellation after it requires cleanup
    // of a potentially started image and must not become a known failure.
    fn launch_gate(
        mut socket: std::os::unix::net::UnixStream,
        cancellation: CancellationToken,
        abandoned: CancellationToken,
        deadline: Instant,
    ) -> io::Result<LaunchDecision> {
        let decision = decide_launch(&mut socket, &cancellation, &abandoned, deadline);
        if !matches!(decision, Ok(LaunchDecision::Allowed)) {
            // Every failure must wake a child blocked on its grant, even when
            // another descriptor inherited across fork still owns this endpoint.
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
        decision
    }

    fn decide_launch(
        socket: &mut std::os::unix::net::UnixStream,
        cancellation: &CancellationToken,
        abandoned: &CancellationToken,
        deadline: Instant,
    ) -> io::Result<LaunchDecision> {
        loop {
            if cancellation.is_cancelled() || abandoned.is_cancelled() {
                // shutdown, unlike close alone, also disables the descriptor
                // inherited by the child across fork before it closes that copy.
                socket.shutdown(std::net::Shutdown::Both)?;
                return Ok(LaunchDecision::Cancelled);
            }
            if Instant::now() >= deadline {
                socket.shutdown(std::net::Shutdown::Both)?;
                return Ok(LaunchDecision::TimedOut);
            }
            let mut descriptor = libc::pollfd {
                fd: socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: descriptor is live and writable for this one-entry poll.
            let ready = unsafe { libc::poll(&mut descriptor, 1, POLL_MILLISECONDS) };
            if ready == -1 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            } else if ready > 0 {
                let mut byte = [0];
                match socket.read(&mut byte) {
                    Ok(1) if byte[0] == 1 => {
                        // Recheck after readiness: fchdir may have been paused
                        // while cancellation or the original deadline arrived.
                        if cancellation.is_cancelled() || abandoned.is_cancelled() {
                            socket.shutdown(std::net::Shutdown::Both)?;
                            return Ok(LaunchDecision::Cancelled);
                        }
                        if Instant::now() >= deadline {
                            socket.shutdown(std::net::Shutdown::Both)?;
                            return Ok(LaunchDecision::TimedOut);
                        }
                        use std::io::Write;
                        socket.write_all(&[1])?;
                        return Ok(LaunchDecision::Allowed);
                    }
                    Ok(_) => return Err(io::Error::from_raw_os_error(libc::ECANCELED)),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }

    fn child_launch_gate(fd: RawFd) -> io::Result<()> {
        let mut byte = 1u8;
        loop {
            // SAFETY: the socket is owned through pre_exec and byte is live.
            let result =
                unsafe { libc::send(fd, (&byte as *const u8).cast(), 1, libc::MSG_NOSIGNAL) };
            if result == 1 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        loop {
            // SAFETY: byte is writable, and this only reads the owned socket.
            let result = unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
            if result == 1 && byte == 1 {
                return Ok(());
            }
            if result == 0 || result == 1 {
                return Err(io::Error::from_raw_os_error(libc::ECANCELED));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    #[cfg(test)]
    pub(super) fn supervise(
        prepared: PreparedProcess,
        cancellation: CancellationToken,
        abandoned: CancellationToken,
    ) -> Result<ProcessResult, DeviceError> {
        supervise_mode(
            prepared,
            cancellation,
            abandoned,
            None,
            Arc::default(),
            Arc::default(),
        )
    }

    pub(super) fn supervise_mode(
        prepared: PreparedProcess,
        cancellation: CancellationToken,
        abandoned: CancellationToken,
        interactive: Option<Arc<InteractiveShared>>,
        admission_closed: Arc<std::sync::atomic::AtomicBool>,
        ledger: Arc<CleanupLedger>,
    ) -> Result<ProcessResult, DeviceError> {
        let PreparedProcess {
            spec,
            resources: _resources,
            observer,
        } = prepared;
        let DirectoryProcessSpec {
            program,
            args,
            cwd,
            env,
            timeout,
            max_output_bytes,
            io,
        } = spec;
        if cancellation.is_cancelled() || abandoned.is_cancelled() {
            return Err(DeviceError::Cancelled);
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(DeviceError::InvalidSpec("timeout is not representable"))?;
        let image = ExecImage::new(&program, &args, &env)?;
        #[cfg(target_os = "macos")]
        let launch_lock = lock_launch(&cancellation, &abandoned, deadline)?;
        #[cfg(target_os = "macos")]
        let terminal = match io {
            crate::ProcessIo::Pipes => None,
            crate::ProcessIo::Pty { columns, rows } => Some(
                Terminal::open(columns, rows)
                    .map_err(|error| DeviceError::not_started("open terminal", error))?,
            ),
        };
        #[cfg(not(target_os = "macos"))]
        if !matches!(io, crate::ProcessIo::Pipes) {
            return Err(DeviceError::UnsupportedPlatform);
        }
        let mut command = Command::new(program);
        command.args(args).env_clear().envs(env);
        #[cfg(target_os = "macos")]
        let terminal_fd = terminal
            .as_ref()
            .map(|terminal| terminal.replica.as_raw_fd());
        #[cfg(not(target_os = "macos"))]
        let terminal_fd: Option<RawFd> = None;
        if terminal_fd.is_some() {
            #[cfg(target_os = "macos")]
            {
                let replica = &terminal.as_ref().expect("terminal").replica;
                let clone = || {
                    replica
                        .try_clone()
                        .map_err(|error| DeviceError::not_started("clone terminal replica", error))
                };
                command
                    .stdin(Stdio::from(clone()?))
                    .stdout(Stdio::from(clone()?))
                    .stderr(Stdio::from(clone()?));
            }
        } else {
            command
                .stdin(if interactive.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0);
        }
        let cwd_fd = cwd.as_raw_fd();
        let (parent_gate, child_gate) = std::os::unix::net::UnixStream::pair()
            .map_err(|error| DeviceError::not_started("create launch gate", error))?;
        let protect =
            |socket: std::os::unix::net::UnixStream| -> io::Result<std::os::unix::net::UnixStream> {
                use std::os::fd::FromRawFd;
                // SAFETY: socket is live; this creates a new CLOEXEC descriptor above
                // stdio so Command cannot replace it during child stdio setup.
                let fd = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
                if fd == -1 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: fcntl returned a newly owned socket descriptor.
                Ok(unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) })
            };
        let parent_gate = protect(parent_gate)
            .map_err(|error| DeviceError::not_started("protect launch gate", error))?;
        let child_gate = protect(child_gate)
            .map_err(|error| DeviceError::not_started("protect launch gate", error))?;
        let parent_fd = parent_gate.as_raw_fd();
        let gate_socket = parent_gate
            .try_clone()
            .map_err(|error| DeviceError::not_started("clone launch gate", error))?;
        let child_fd = child_gate.as_raw_fd();
        let gate_cancel = cancellation.clone();
        let gate_abandoned = abandoned.clone();
        #[cfg(all(test, target_os = "macos"))]
        let launch_pause = tests::take_launch_pause();
        #[cfg(all(test, target_os = "macos"))]
        let launch_pause_fd = launch_pause.as_ref().map(AsRawFd::as_raw_fd);
        let gate = std::thread::Builder::new()
            .name("cayw-launch-gate".into())
            .spawn(move || launch_gate(gate_socket, gate_cancel, gate_abandoned, deadline))
            .map_err(|error| DeviceError::not_started("start launch gate", error))?;
        // SAFETY: the callback performs only async-signal-safe descriptor and
        // exec syscalls. All strings/arrays were allocated in the parent. The
        // supervisor owns cwd through cleanup and its CLOEXEC fd is not inherited.
        // Exec failure returns through Command's error pipe/reap, never execvp's
        // implicit shell fallback. Tokens are observed only by the parent gate.
        unsafe {
            command.pre_exec(move || {
                libc::close(parent_fd);
                if libc::fchdir(cwd_fd) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if let Some(fd) = terminal_fd {
                    if libc::setsid() == -1 || libc::ioctl(fd, libc::TIOCSCTTY.into(), 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::tcsetpgrp(fd, libc::getpgrp()) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                }
                #[cfg(all(test, target_os = "macos"))]
                if let Some(fd) = launch_pause_fd {
                    // Only the test binary carries this gate. The forked child
                    // uses raw descriptor syscalls, never a mutex or allocator.
                    let pid = libc::getpid();
                    if libc::send(
                        fd,
                        (&pid as *const libc::pid_t).cast(),
                        std::mem::size_of_val(&pid),
                        libc::MSG_NOSIGNAL,
                    ) != std::mem::size_of_val(&pid) as isize
                    {
                        return Err(io::Error::last_os_error());
                    }
                    let mut release = 0u8;
                    if libc::read(fd, (&mut release as *mut u8).cast(), 1) != 1 {
                        return Err(io::Error::from_raw_os_error(libc::ECANCELED));
                    }
                }
                child_launch_gate(child_fd)?;
                Err(image.execute())
            });
        }
        let spawned = command.spawn();
        drop(parent_gate);
        drop(child_gate);
        // Always join the launch gate before returning, including spawn errors.
        // Command has already reaped a rejected pre_exec child at this point.
        let decision = gate.join();
        #[cfg(all(test, target_os = "macos"))]
        drop(launch_pause);
        // Close every parent-side replica before another Device launch can fork.
        drop(command);
        let mut terminal_primary: Option<fs::File> = None;
        #[cfg(target_os = "macos")]
        if let Some(Terminal { primary, replica }) = terminal {
            drop(replica);
            terminal_primary = Some(primary);
        }
        #[cfg(target_os = "macos")]
        drop(launch_lock);
        let child = match spawned {
            Ok(child) => child,
            Err(error) => {
                return Err(match decision {
                    Ok(Ok(LaunchDecision::Cancelled)) => DeviceError::Cancelled,
                    Ok(Ok(LaunchDecision::TimedOut)) => DeviceError::not_started(
                        "launch deadline reached",
                        io::Error::from_raw_os_error(libc::ETIMEDOUT),
                    ),
                    _ => DeviceError::not_started("launch process", error),
                });
            }
        };
        // Keep the original directory handle alive through group cleanup.
        let identity = interactive.as_ref().map_or_else(
            || Arc::new(Mutex::new(interactive::LeaderIdentity::default())),
            |shared| shared.identity.clone(),
        );
        lock(&identity).pid = Some(child.id());
        let mut group = ProcessGroup {
            child,
            identity,
            admission_closed,
            termination_sent: false,
            reaped: false,
            observer,
            observed: BTreeSet::new(),
            records_closed: false,
            ledger,
        };
        #[cfg(target_os = "macos")]
        {
            let original = ProcessIdentity::observe(group.child.id())
                .map_err(|error| DeviceError::io("capture process startup identity", error))?;
            if original.process_group != original.pid {
                return Err(DeviceError::io(
                    "capture process startup identity",
                    io::Error::other("child is not its managed process group leader"),
                ));
            }
            lock(&group.identity).original = Some(original);
            group
                .record_started()
                .map_err(|error| DeviceError::io("record process start", error))?;
        }
        #[cfg(all(test, target_os = "macos"))]
        tests::panic_after_spawn(group.child.id());
        let out_sink = interactive.as_ref().map_or(PipeSink::Collected, |shared| {
            PipeSink::Stdout(shared.clone())
        });
        let err_sink = if interactive.is_some() {
            PipeSink::Discard
        } else {
            PipeSink::Collected
        };
        let (mut stdout, mut stderr, mut stdin): (
            Pipe<fs::File>,
            Pipe<fs::File>,
            Option<fs::File>,
        ) = if let Some(primary) = terminal_primary {
            let writer = if interactive.is_some() {
                Some(
                    primary
                        .try_clone()
                        .map_err(|error| DeviceError::io("clone terminal primary", error))?,
                )
            } else {
                None
            };
            let mut output = Pipe::new(primary, out_sink)
                .map_err(|error| DeviceError::io("configure terminal output", error))?;
            output.terminal_eof = true;
            (output, Pipe::absent(err_sink), writer)
        } else {
            (
                Pipe::new(
                    fs::File::from(std::os::fd::OwnedFd::from(
                        group.child.stdout.take().expect("stdout was piped"),
                    )),
                    out_sink,
                )
                .map_err(|error| DeviceError::io("configure stdout pipe", error))?,
                Pipe::new(
                    fs::File::from(std::os::fd::OwnedFd::from(
                        group.child.stderr.take().expect("stderr was piped"),
                    )),
                    err_sink,
                )
                .map_err(|error| DeviceError::io("configure stderr pipe", error))?,
                group
                    .child
                    .stdin
                    .take()
                    .map(|stdin| fs::File::from(std::os::fd::OwnedFd::from(stdin))),
            )
        };
        if let Some(stdin) = &stdin {
            nonblocking(stdin).map_err(|error| DeviceError::io("configure stdin pipe", error))?;
        }
        if let Some(shared) = &interactive {
            shared.started();
        }
        let mut budget = OutputBudget {
            remaining: max_output_bytes,
            read: 0,
        };
        let mut termination = loop {
            if cancellation.is_cancelled() || abandoned.is_cancelled() {
                break TerminationReason::Cancelled;
            }
            if Instant::now() >= deadline {
                break TerminationReason::TimedOut;
            }
            #[cfg(target_os = "macos")]
            group
                .record_members()
                .map_err(|error| DeviceError::io("record process members", error))?;
            if let (Some(stdin), Some(shared)) = (&mut stdin, &interactive) {
                write_pending(stdin, shared)
                    .map_err(|error| DeviceError::io("write stdin", error))?;
            }
            let out_overflow = stdout
                .drain(&mut budget)
                .map_err(|error| DeviceError::io("read stdout", error))?;
            let err_overflow = stderr
                .drain(&mut budget)
                .map_err(|error| DeviceError::io("read stderr", error))?;
            if out_overflow || err_overflow {
                break TerminationReason::OutputLimit;
            }
            if group
                .leader_exited()
                .map_err(|error| DeviceError::io("observe process exit", error))?
            {
                break TerminationReason::Exited;
            }
            let write_fd = match (&stdin, &interactive) {
                (Some(stdin), Some(shared)) if lock(&shared.io).pending.is_some() => {
                    stdin.as_raw_fd()
                }
                _ => -1,
            };
            wait_for_pipes(stdout.fd(), stderr.fd(), write_fd)
                .map_err(|error| DeviceError::io("poll output pipes", error))?;
        };

        if let Some(shared) = &interactive {
            lock(&shared.io).stopping = true;
        }
        drop(stdin);
        group
            .terminate()
            .map_err(|error| DeviceError::io("terminate process group", error))?;
        stdout.cleaning = true;
        stderr.cleaning = true;
        let cleanup_deadline =
            Instant::now()
                .checked_add(PIPE_CLEANUP_TIMEOUT)
                .ok_or(DeviceError::InvalidSpec(
                    "pipe cleanup deadline is not representable",
                ))?;
        loop {
            let out_overflow = stdout
                .drain(&mut budget)
                .map_err(|error| DeviceError::io("drain stdout", error))?;
            let err_overflow = stderr
                .drain(&mut budget)
                .map_err(|error| DeviceError::io("drain stderr", error))?;
            if (out_overflow || err_overflow) && termination == TerminationReason::Exited {
                termination = TerminationReason::OutputLimit;
            }
            if (stdout.reader.is_none() && stderr.reader.is_none())
                || Instant::now() >= cleanup_deadline
            {
                break;
            }
            wait_for_pipes(stdout.fd(), stderr.fd(), -1)
                .map_err(|error| DeviceError::io("poll cleanup pipes", error))?;
        }
        let status = group
            .finish()
            .map_err(|error| DeviceError::io("reap process", error))?;
        group
            .record_stopped()
            .map_err(|error| DeviceError::io("record process cleanup", error))?;
        Ok(ProcessResult {
            status,
            termination,
            output_complete: termination == TerminationReason::Exited
                && stdout.reader.is_none()
                && stderr.reader.is_none()
                && !stdout.discarded,
            stdout: stdout.bytes,
            stderr: stderr.bytes,
        })
    }
}
