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
    ///
    /// On macOS the startup time is the kernel's wall-clock process start. On
    /// Linux it is `starttime` from `/proc/<pid>/stat` (clock ticks since boot),
    /// split into seconds and microseconds; it is never derived from `btime`,
    /// which moves when the wall clock is stepped. Absence is reported as
    /// `ESRCH` only after `kill(pid, 0)` independently confirms it.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub fn observe(pid: u32) -> io::Result<Self> {
        #[cfg(target_os = "macos")]
        {
            let info = inspect(pid)?;
            Self::from_info(&info)
        }
        #[cfg(target_os = "linux")]
        {
            linux::inspect(pid).map(|(identity, _)| identity)
        }
    }

    /// A process can change groups without changing its PID and startup time.
    /// Group membership and parentage describe topology, not a new incarnation.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(super) fn same_process(self, other: Self) -> bool {
        self.pid == other.pid
            && self.started_seconds == other.started_seconds
            && self.started_microseconds == other.started_microseconds
    }

    /// Observe whether this original incarnation is still running. This never
    /// signals a process. An unreadable identity remains an error; a changed
    /// process group does not prove that the original process stopped.
    #[cfg(target_os = "linux")]
    pub fn is_running(self) -> io::Result<bool> {
        let (current, state) = match linux::inspect(self.pid) {
            Ok(observed) => observed,
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(false),
            Err(error) => return Err(error),
        };
        // A different startup time at the same PID is a new incarnation; the
        // recorded one has exited. Zombie/dead states have exited but are not
        // reaped. Stopped (T/t) and every other state can still run.
        Ok(self.same_process(current) && !matches!(state, b'Z' | b'X' | b'x'))
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

#[cfg(target_os = "linux")]
pub(super) mod linux {
    use super::ProcessIdentity;
    use std::io;

    /// Read one `/proc/<pid>/stat` record: identity plus the one-letter state.
    /// A single read is one consistent kernel snapshot of pid, parent, group,
    /// and startup time. `kill(pid, 0)` sends no signal; it only confirms
    /// absence. The PID must belong to the PID namespace mounted at `/proc`.
    pub(in crate::process) fn inspect(pid: u32) -> io::Result<(ProcessIdentity, u8)> {
        let raw = libc::pid_t::try_from(pid)
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or_else(|| io::Error::other("invalid process ID"))?;
        let stat = match std::fs::read(format!("/proc/{raw}/stat")) {
            Ok(stat) => stat,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ESRCH) =>
            {
                // A missing /proc entry is not proof on its own (/proc may be
                // absent or hidden). Require the kernel to deny the PID too.
                // SAFETY: signal 0 performs only existence/permission checks.
                if unsafe { libc::kill(raw, 0) } == -1
                    && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                return Err(io::Error::other(format!(
                    "process identity is unreadable while the process exists: {error}"
                )));
            }
            Err(error) => return Err(error),
        };
        parse(pid, &stat, clock_ticks()?)
    }

    fn clock_ticks() -> io::Result<u64> {
        // SAFETY: sysconf has no memory-safety preconditions.
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        u64::try_from(ticks)
            .ok()
            .filter(|ticks| (1..=1_000_000).contains(ticks))
            .ok_or_else(|| io::Error::other("invalid clock tick rate"))
    }

    pub(in crate::process) fn parse(
        pid: u32,
        stat: &[u8],
        ticks_per_second: u64,
    ) -> io::Result<(ProcessIdentity, u8)> {
        let invalid = || io::Error::other("invalid process startup identity");
        // comm may contain spaces, ')' and invalid UTF-8; only fields after
        // its LAST ')' have a fixed text format.
        let open = stat
            .iter()
            .position(|byte| *byte == b'(')
            .ok_or_else(invalid)?;
        let close = stat
            .iter()
            .rposition(|byte| *byte == b')')
            .ok_or_else(invalid)?;
        if close < open || open == 0 || stat[open - 1] != b' ' {
            return Err(invalid());
        }
        let number = |field: &[u8]| {
            std::str::from_utf8(field)
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(invalid)
        };
        if number(&stat[..open - 1])? != u64::from(pid) {
            return Err(io::Error::other(
                "process identity changed during inspection",
            ));
        }
        let tail = &stat[close + 1..];
        if !tail.starts_with(b" ") {
            return Err(invalid());
        }
        let fields: Vec<&[u8]> = tail
            .split(u8::is_ascii_whitespace)
            .filter(|field| !field.is_empty())
            .collect();
        // Fields 3.. of proc_pid_stat(5): state, ppid, pgrp, ..., starttime (22).
        let state = fields.first().ok_or_else(invalid)?;
        if state.len() != 1 || !state[0].is_ascii_alphabetic() {
            return Err(invalid());
        }
        let parent_pid =
            u32::try_from(number(fields.get(1).ok_or_else(invalid)?)?).map_err(|_| invalid())?;
        let process_group =
            u32::try_from(number(fields.get(2).ok_or_else(invalid)?)?).map_err(|_| invalid())?;
        let started = number(fields.get(19).ok_or_else(invalid)?)?;
        if ticks_per_second == 0 || ticks_per_second > 1_000_000 {
            return Err(invalid());
        }
        let started_seconds = started / ticks_per_second;
        // ticks_per_second <= 10^6, so distinct tick remainders stay distinct.
        let started_microseconds = u32::try_from(
            u128::from(started % ticks_per_second) * 1_000_000 / u128::from(ticks_per_second),
        )
        .map_err(|_| invalid())?;
        if process_group == 0 || started_seconds == 0 {
            return Err(invalid());
        }
        Ok((
            ProcessIdentity {
                pid,
                parent_pid,
                process_group,
                started_seconds,
                started_microseconds,
            },
            state[0],
        ))
    }
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

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::{ProcessIdentity, linux};
    use std::time::{Duration, Instant};

    fn stat(pid: u32, comm: &str, state: char, ppid: u32, pgrp: u32, start: u64) -> Vec<u8> {
        // proc_pid_stat(5): fields 6..21 are irrelevant here; starttime is 22.
        let middle = "0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        format!("{pid} ({comm}) {state} {ppid} {pgrp} {middle} {start} 1 2 3\n").into_bytes()
    }

    #[test]
    fn parses_identity_after_the_last_parenthesis_of_a_hostile_comm() {
        let raw = stat(4242, "a) Z 1 1 (b", 'S', 17, 4242, 123_456);
        let (identity, state) = linux::parse(4242, &raw, 100).unwrap();
        assert_eq!(state, b'S');
        assert_eq!(
            identity,
            ProcessIdentity {
                pid: 4242,
                parent_pid: 17,
                process_group: 4242,
                started_seconds: 1234,
                started_microseconds: 560_000,
            }
        );
    }

    #[test]
    fn distinct_start_ticks_never_collapse_to_one_identity() {
        let one = linux::parse(7, &stat(7, "x", 'R', 1, 7, 1001), 100)
            .unwrap()
            .0;
        let two = linux::parse(7, &stat(7, "x", 'R', 1, 7, 1002), 100)
            .unwrap()
            .0;
        assert!(!one.same_process(two));
        // Group and parent changes are topology, not a new incarnation.
        let moved = linux::parse(7, &stat(7, "x", 'R', 99, 99, 1001), 100)
            .unwrap()
            .0;
        assert!(one.same_process(moved));
    }

    #[test]
    fn malformed_or_mismatched_records_are_errors_not_absence() {
        for raw in [
            stat(8, "x", 'S', 1, 8, 500).replace_pid(9),
            b"8 (x) S 1 8".to_vec(),
            stat(8, "x", '?', 1, 8, 500),
            stat(8, "x", 'S', 1, 0, 500),
            // Started within the first second after boot: no valid identity.
            stat(8, "x", 'S', 1, 8, 50),
        ] {
            let error = linux::parse(8, &raw, 100).unwrap_err();
            assert_ne!(error.raw_os_error(), Some(libc::ESRCH), "{error}");
        }
        assert!(linux::parse(8, &stat(8, "x", 'S', 1, 8, 500), 0).is_err());
    }

    trait ReplacePid {
        fn replace_pid(self, pid: u32) -> Vec<u8>;
    }
    impl ReplacePid for Vec<u8> {
        fn replace_pid(self, pid: u32) -> Vec<u8> {
            let open = self.iter().position(|byte| *byte == b'(').unwrap();
            let mut out = format!("{pid} ").into_bytes();
            out.extend_from_slice(&self[open..]);
            out
        }
    }

    #[test]
    fn a_live_zombie_and_reaped_child_are_three_different_answers() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let original = ProcessIdentity::observe(pid).unwrap();
        assert_eq!(original.pid, pid);
        assert_eq!(original.parent_pid, std::process::id());
        assert!(original.is_running().unwrap());
        // The same PID with another start time is a different incarnation.
        let reused = ProcessIdentity {
            started_microseconds: (original.started_microseconds + 10_000) % 1_000_000,
            started_seconds: original.started_seconds + 1,
            ..original
        };
        assert!(!reused.is_running().unwrap());

        child.kill().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let (current, state) = linux::inspect(pid).unwrap();
            assert!(current.same_process(original));
            if state == b'Z' {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the killed child must become a zombie"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // An unreaped zombie still has its identity but is not running.
        assert!(!original.is_running().unwrap());
        child.wait().unwrap();
        let error = ProcessIdentity::observe(pid).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ESRCH));
        assert!(!original.is_running().unwrap());
    }

    #[test]
    fn the_current_process_is_observed_consistently() {
        let first = ProcessIdentity::observe(std::process::id()).unwrap();
        let second = ProcessIdentity::observe(std::process::id()).unwrap();
        assert_eq!(first, second);
        assert!(first.started_microseconds < 1_000_000);
        assert!(first.is_running().unwrap());
        assert!(ProcessIdentity::observe(0).is_err());
    }
}
