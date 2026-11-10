//! Host-controlled local execution and declared resource admission.
//!
//! Programs, arguments, working directories, and environment entries are chosen
//! explicitly by the host. No shell command parser or agent tool is registered.
//!
//! On macOS, one supervisor owns the original child and process group. It drains
//! stdout/stderr pipes or merged PTY output under optional lifetime byte limits,
//! stops remaining group members, and waits for the direct child. Without a
//! byte limit, output above [`INLINE_OUTPUT_BYTES`] spills to a private file
//! while only a bounded head and tail remain in memory. Without a timeout, the
//! process runs until it exits or the caller cancels it. Orphan descendants
//! are reaped by the operating system's parent process. With a [`ProcessObserver`],
//! it records PID/startup identities and also waits for every observed incarnation
//! to stop, even if that process later leaves the group.
//! [`ProcessRunner::drain`] closes one task's process admission and joins all
//! actual supervisors, including those whose run futures were dropped, and waits
//! for every registered resource's cleanup. Release or transfer registered
//! resources before awaiting drain. Failed or panicking process records preserve
//! the failure and transferred permit even after OS cleanup. A caller must keep
//! its task reservation until drain confirms cleanup. Drop requests cancellation
//! without asserting completion. Unsupported platforms fail closed; the macOS
//! validation does not establish acceptance on another platform.
//!
//! This is not a sandbox: allowed programs have the host user's OS access and
//! can deliberately escape a process group, for example using `setsid`. No signal
//! authority is granted over an escaped group. An observed escaped incarnation
//! keeps drain pending until it stops; an unreadable identity is an error. A
//! process that escapes before observation or starts and exits between observations
//! is outside that recorded coverage. Inherited output descriptors are closed
//! after a bounded cleanup interval, making [`ProcessResult::output_complete`]
//! false if EOF cannot be confirmed.
//! Resource reservations account only for host-declared memory and task count;
//! GPU requests, hard memory limits, and CPU quotas are explicitly rejected.

mod process;
mod scheduler;

pub use process::{
    CleanupResource, InteractiveLimits, InteractiveProcess, ManagedStdin, ManagedStdout,
    ProcessCleanup, ProcessControl, ProcessIdentity, ProcessObserver, ProcessRunner,
};
pub use scheduler::{
    ResourceLimits, ResourcePermit, ResourceRequest, ResourceScheduler, ResourceUsage, SessionId,
};

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

/// Failures before launch, admission failures, or failures of process supervision.
#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("invalid process specification: {0}")]
    InvalidSpec(&'static str),
    #[error("program is not allowed: {0}")]
    ProgramNotAllowed(PathBuf),
    #[error("operation cancelled before admission or launch")]
    Cancelled,
    #[error("process runner is closed")]
    RunnerClosed,
    #[error("resource capacity is currently reserved")]
    ResourcesBusy,
    #[error("request exceeds the device budget")]
    ResourceRequestTooLarge,
    #[error("unsupported resource: {0}")]
    UnsupportedResource(&'static str),
    #[error("invalid resource configuration: {0}")]
    InvalidLimits(&'static str),
    #[error("invalid session ID")]
    InvalidSessionId,
    #[error("process tree management is unsupported on this platform")]
    UnsupportedPlatform,
    /// The allowlisted program never started. Preparation and exec failures use
    /// this variant. Post-launch failures cannot establish that nothing ran;
    /// unconfirmed cleanup or process records use `CleanupUnconfirmed`.
    #[error("program did not start ({operation}): {source}")]
    NotStarted {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    /// A transferred resource could not be safely cleaned up. The runner keeps
    /// the resource, closes admission, and cannot confirm a successful drain.
    #[error("process resource cleanup is unconfirmed: {0}")]
    CleanupUnconfirmed(String),
    #[error("{operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("process supervisor stopped without returning a result")]
    SupervisorStopped,
    #[error("interactive process pipe is closed")]
    ProcessClosed,
}

impl DeviceError {
    fn io(operation: &'static str, source: std::io::Error) -> Self {
        Self::Io { operation, source }
    }
    fn not_started(operation: &'static str, source: std::io::Error) -> Self {
        Self::NotStarted { operation, source }
    }
}

/// Host-selected process I/O. PTY output combines stdout and stderr, including
/// terminal echo and line discipline bytes, under the same lifetime output limit.
/// PTY execution is supported on macOS; it is not a process security boundary.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProcessIo {
    #[default]
    Pipes,
    Pty {
        columns: u16,
        rows: u16,
    },
}

/// Output per stream that remains in memory before spilling to disk.
pub const INLINE_OUTPUT_BYTES: usize = 256 * 1024;
/// Bytes kept from the start of a spilled stream.
pub const HEAD_BYTES: usize = 128 * 1024;
/// Bytes kept from the end of a spilled stream.
pub const TAIL_BYTES: usize = 128 * 1024;

/// A host-created command with explicit argv, working directory, and environment.
///
/// `program` and `cwd` must be absolute. The environment starts empty; no OS or
/// host variables are added implicitly. Pipe runs have closed stdin unless started
/// through the interactive API. PTY runs attach all three standard descriptors to
/// one controlling terminal; only the interactive API writes input. Output bytes
/// include terminal echo and line-ending conversion when PTY mode is selected.
///
/// `timeout: None` runs until the child exits or the caller cancels. `Some`
/// enforces a positive, representable lifetime. `max_output_bytes: None` never
/// kills the child for output volume: up to [`INLINE_OUTPUT_BYTES`] stay in
/// memory, and larger streams spill to `output_spill_dir` (or the OS temporary
/// directory). `Some` enforces the exact combined stdout+stderr lifetime limit.
#[derive(Clone, Debug)]
pub struct ProcessSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub timeout: Option<Duration>,
    pub max_output_bytes: Option<usize>,
    pub output_spill_dir: Option<PathBuf>,
    pub io: ProcessIo,
}

/// A host command whose working directory is an already-opened directory.
///
/// The supervisor owns `cwd` through cleanup. On Unix the child sets cwd using
/// `fchdir` before exec; the directory is never reopened by pathname. Other
/// platforms explicitly reject execution until their handle support is provided.
#[derive(Debug)]
pub struct DirectoryProcessSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: std::fs::File,
    pub env: BTreeMap<String, String>,
    pub timeout: Option<Duration>,
    pub max_output_bytes: Option<usize>,
    pub output_spill_dir: Option<PathBuf>,
    pub io: ProcessIo,
}

/// The first stop condition observed by the supervisor/caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminationReason {
    Exited,
    Cancelled,
    TimedOut,
    OutputLimit,
}

/// An interactive process outcome, published only after process cleanup, true
/// supervisor join (including TLS destructors), and reservation handling finish.
#[derive(Clone, Copy, Debug)]
pub struct ProcessExit {
    pub status: ExitStatus,
    pub termination: TerminationReason,
    /// All raw output was read and all stdout was retained for the consumer.
    /// Stopping with a full stdout buffer can make this false.
    pub output_complete: bool,
}

/// A fully captured output stream that exceeded [`INLINE_OUTPUT_BYTES`].
///
/// The spill file contains every byte of the stream in order. `head` and `tail`
/// are the bounded in-memory copies also inserted into the corresponding
/// `ProcessResult` field around an omission marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpilledOutput {
    pub path: PathBuf,
    pub total_bytes: u64,
    pub sha256: String,
    pub head: Vec<u8>,
    pub tail: Vec<u8>,
}

/// A launched process, including nonzero exits, timeout, cancellation, or overflow.
#[derive(Debug)]
pub struct ProcessResult {
    pub status: ExitStatus,
    pub termination: TerminationReason,
    /// Head, omission marker, and tail when this stream spilled; otherwise the
    /// raw bytes. A spilled stream is always bounded near [`INLINE_OUTPUT_BYTES`].
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Present only when `max_output_bytes` was `None` and stdout exceeded
    /// [`INLINE_OUTPUT_BYTES`] and the spill file was written successfully.
    pub spilled_stdout: Option<SpilledOutput>,
    /// Present only when `max_output_bytes` was `None` and stderr exceeded
    /// [`INLINE_OUTPUT_BYTES`] and the spill file was written successfully.
    pub spilled_stderr: Option<SpilledOutput>,
    /// Both output pipes reached EOF after normal exit without truncation.
    /// A descendant that escapes its process group may force bounded pipe closure.
    /// A failed spill write also makes this false.
    pub output_complete: bool,
}
