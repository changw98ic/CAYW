use std::io;

/// An observed OS process incarnation. PID plus startup time identifies the
/// original process; parent_pid records the topology at the time of observation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub parent_pid: u32,
    pub process_group: u32,
    pub started_seconds: u64,
    pub started_microseconds: u32,
}
impl ProcessIdentity {
    /// Read-only inspection. This never signals, reaps, or grants ownership of a
    /// process. An unreadable identity is an error, not proof that it has stopped.
    #[cfg(target_os = "macos")]
    pub fn observe(pid: u32) -> io::Result<Self> {
        let info = inspect(pid)?;
        Self::from_info(&info)
    }

    /// A process can change groups without changing its PID and startup time.
    /// Group membership and parentage describe topology, not a new incarnation.
    pub(super) fn same_process(self, other: Self) -> bool {
        self.pid == other.pid
            && self.started_seconds == other.started_seconds
            && self.started_microseconds == other.started_microseconds
    }

    /// Observe whether this original incarnation is still running. This never
    /// signals a process. An unreadable identity remains an error; a changed
    /// process group does not prove that the original process stopped.
    #[cfg(target_os = "macos")]
    pub fn is_running(self) -> io::Result<bool> {
        let info = match inspect(self.pid) {
            Ok(info) => info,
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(false),
            Err(error) => return Err(error),
        };
        let current = Self::from_info(&info)?;
        // Leaving the original group does not end the recorded incarnation.
        // A new incarnation at the same PID is never signalled or adopted.
        Ok(self.pid == current.pid
            && self.started_seconds == current.started_seconds
            && self.started_microseconds == current.started_microseconds
            && info.pbi_status != libc::SZOMB)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn from_info(info: &libc::proc_bsdinfo) -> io::Result<Self> {
        if info.pbi_pid == 0 || info.pbi_start_tvsec == 0 || info.pbi_start_tvusec >= 1_000_000 {
            return Err(io::Error::other("invalid process startup identity"));
        }
        Ok(Self {
            pid: info.pbi_pid,
            parent_pid: info.pbi_ppid,
            process_group: info.pbi_pgid,
            started_seconds: info.pbi_start_tvsec,
            started_microseconds: info.pbi_start_tvusec as u32,
        })
    }
}

#[cfg(target_os = "macos")]
pub(super) fn inspect(pid: u32) -> io::Result<libc::proc_bsdinfo> {
    let pid = libc::pid_t::try_from(pid)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| io::Error::other("invalid process ID"))?;
    // SAFETY: proc_bsdinfo is C POD and the buffer is writable for size bytes.
    // proc_pidinfo receives that exact buffer length.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as libc::c_int;
    let count = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            1,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if count != size {
        return Err(if count == 0 {
            io::Error::last_os_error()
        } else {
            io::Error::other("incomplete process identity observation")
        });
    }
    if info.pbi_pid != pid as u32 {
        return Err(io::Error::other(
            "process identity changed during inspection",
        ));
    }
    Ok(info)
}

/// Host-injected durable process records. Callbacks run synchronously on the
/// original supervisor, outside its identity/runner locks. Implementations must
/// return within their own storage deadlines: cancellation cannot interrupt a
/// callback that does not return. An error or unwinding panic stops the same
/// process and keeps the original failure and transferred permit after OS cleanup.
/// Drain then returns `CleanupUnconfirmed`; later records do not erase the failure.
pub trait ProcessObserver: Send + Sync + std::fmt::Debug {
    fn started(&self, identity: &ProcessIdentity) -> io::Result<()>;
    fn observed(&self, identities: &[ProcessIdentity]) -> io::Result<()>;
    fn stopped(&self, identity: &ProcessIdentity) -> io::Result<()>;
}
