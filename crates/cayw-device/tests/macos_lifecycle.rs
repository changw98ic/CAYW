#![cfg(target_os = "macos")]

use cayw_device::{
    DirectoryProcessSpec, InteractiveLimits, ProcessRunner, ResourceLimits, ResourceRequest,
    ResourceScheduler, SessionId, TerminationReason,
};
use std::{collections::BTreeMap, fs, path::PathBuf, time::Duration};
use tokio_util::sync::CancellationToken;

fn fixture() -> PathBuf {
    env!("CARGO_BIN_EXE_cayw-device-test-process").into()
}

#[derive(Debug, Default)]
struct Observations(std::sync::Mutex<Vec<(bool, cayw_device::ProcessIdentity)>>);
impl cayw_device::ProcessObserver for Observations {
    fn started(&self, identity: &cayw_device::ProcessIdentity) -> std::io::Result<()> {
        let actual = cayw_device::ProcessIdentity::observe(identity.pid)?;
        assert_eq!(actual, *identity);
        self.0.lock().unwrap().push((true, *identity));
        Ok(())
    }
    fn observed(&self, identities: &[cayw_device::ProcessIdentity]) -> std::io::Result<()> {
        self.0
            .lock()
            .unwrap()
            .extend(identities.iter().map(|identity| (true, *identity)));
        Ok(())
    }
    fn stopped(&self, identity: &cayw_device::ProcessIdentity) -> std::io::Result<()> {
        self.0.lock().unwrap().push((false, *identity));
        Ok(())
    }
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

/// Wait until a fixture marker has parseable content.
///
/// `fs::write` truncates/creates before writing bytes, so existence alone can
/// observe an empty file. Same readiness rule as `process::pid` / interactive
/// `marker`: the sync point is a successful parse, not path existence.
async fn parsed_marker<T>(directory: &std::path::Path, name: &str) -> T
where
    T: std::str::FromStr,
    T::Err: std::fmt::Debug,
{
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(text) = fs::read_to_string(directory.join(name))
                && let Ok(value) = text.trim().parse::<T>()
            {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fixture marker {name} never became parseable"))
}

// A group-only cleanup check would falsely mark a previously observed child
// stopped after that child creates a new session. It must keep the reservation
// and omit that stop record until the same OS incarnation really stops.
#[tokio::test]
async fn an_observed_descendant_that_leaves_the_group_is_never_falsely_recorded_stopped() {
    for (name, io) in [
        ("pipes", cayw_device::ProcessIo::Pipes),
        (
            "pty",
            cayw_device::ProcessIo::Pty {
                columns: 80,
                rows: 24,
            },
        ),
    ] {
        let directory = std::env::temp_dir().join(format!(
            "cayw-observed-escape-{}-{name}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let observations = std::sync::Arc::new(Observations::default());
        let runner = ProcessRunner::new([fixture()])
            .unwrap()
            .observed(observations.clone());
        let scheduler = ResourceScheduler::new(ResourceLimits {
            max_concurrent_tasks: 1,
            max_declared_memory_bytes: 1,
        })
        .unwrap();
        let permit = scheduler
            .try_acquire(
                SessionId::new("escaped-task").unwrap(),
                ResourceRequest::default(),
            )
            .unwrap();
        let cancel = CancellationToken::new();
        let running = {
            let runner = runner.clone();
            let root = directory.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                runner
                    .run_in_directory_with_permit(
                        DirectoryProcessSpec {
                            program: fixture(),
                            args: vec!["observed-escape".into(), root.display().to_string()],
                            cwd: fs::File::open(&root).unwrap(),
                            env: BTreeMap::new(),
                            timeout: Duration::from_secs(30),
                            max_output_bytes: 4096,
                            io,
                        },
                        cancel,
                        permit,
                    )
                    .await
            })
        };
        let pid: u32 = parsed_marker(&directory, "waiting.pid").await;
        wait_until(|| {
            observations
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|(started, identity)| *started && identity.pid == pid)
        })
        .await;
        fs::write(directory.join("escape-now"), "leave group").unwrap();
        wait_until(|| directory.join("escaped.pid").exists()).await;
        let escaped = cayw_device::ProcessIdentity::observe(pid).unwrap();
        cancel.cancel();
        let leader = parsed_marker::<u32>(&directory, "root.pid")
            .await
            .to_string();
        wait_until(|| {
            let status = std::process::Command::new("/bin/ps")
                .args(["-o", "stat=", "-p", leader.as_str()])
                .output()
                .unwrap();
            !status.status.success()
                || String::from_utf8_lossy(&status.stdout)
                    .split_whitespace()
                    .all(|state| state.starts_with('Z'))
        })
        .await;
        let mut draining = Box::pin(runner.drain());
        let first_drain = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::future::Future::poll(draining.as_mut(), cx))
        })
        .await;
        let prematurely_drained = first_drain.is_ready();
        let reserved = scheduler.usage().active_tasks;
        let false_stop = observations
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|(started, identity)| !started && identity.pid == pid);
        let still_running = cayw_device::ProcessIdentity::observe(pid).unwrap();
        fs::write(directory.join("escape-release"), "exit naturally").unwrap();
        match first_drain {
            std::task::Poll::Ready(result) => result.unwrap(),
            std::task::Poll::Pending => {
                tokio::time::timeout(Duration::from_secs(10), &mut draining)
                    .await
                    .unwrap()
                    .unwrap()
            }
        }
        let _ = running.await.unwrap();
        let records = observations.0.lock().unwrap().clone();
        fs::remove_dir_all(&directory).unwrap();
        assert_eq!(escaped.process_group, pid, "{name}: fixture did not escape");
        assert_eq!(escaped.started_seconds, still_running.started_seconds);
        assert_eq!(
            escaped.started_microseconds,
            still_running.started_microseconds
        );
        assert!(
            !prematurely_drained,
            "{name}: live observed descendant was omitted from drain"
        );
        assert_eq!(
            reserved, 1,
            "{name}: live observed descendant lost its slot"
        );
        assert!(
            !false_stop,
            "{name}: live observed descendant was recorded stopped"
        );
        assert!(
            records
                .iter()
                .any(|(started, identity)| !started && identity.pid == pid)
        );
        assert_eq!(scheduler.usage().active_tasks, 0);
    }
}

fn incarnation_is_running(original: cayw_device::ProcessIdentity) -> std::io::Result<bool> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as libc::c_int;
    // SAFETY: this is a read-only query into a buffer of exactly the given size.
    let count = unsafe {
        libc::proc_pidinfo(
            original.pid as libc::pid_t,
            libc::PROC_PIDTBSDINFO,
            1,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if count != size {
        let error = std::io::Error::last_os_error();
        return if count == 0 && error.raw_os_error() == Some(libc::ESRCH) {
            Ok(false)
        } else if count == 0 {
            Err(error)
        } else {
            Err(std::io::Error::other(
                "incomplete process identity observation",
            ))
        };
    }
    if info.pbi_pid != original.pid {
        return Err(std::io::Error::other(
            "process identity changed during inspection",
        ));
    }
    Ok(info.pbi_start_tvsec == original.started_seconds
        && info.pbi_start_tvusec == original.started_microseconds as u64
        && info.pbi_status != libc::SZOMB)
}

struct RegroupRelease(PathBuf);
impl Drop for RegroupRelease {
    fn drop(&mut self) {
        for gate in ["regroup-now", "exit-now", "helper-release"] {
            let _ = fs::write(self.0.join(gate), "fixture cleanup");
        }
    }
}

// A group change preserves the owned incarnation. Wait for its start record
// before allowing the change, and prove cleanup leaves the other group alive.
#[tokio::test]
async fn a_leader_that_changes_its_own_process_group_is_still_reaped() {
    for (name, io) in [
        ("pipes", cayw_device::ProcessIo::Pipes),
        (
            "pty",
            cayw_device::ProcessIo::Pty {
                columns: 80,
                rows: 24,
            },
        ),
    ] {
        let directory =
            std::env::temp_dir().join(format!("cayw-regroup-{}-{name}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let release = RegroupRelease(directory.clone());
        let observations = std::sync::Arc::new(Observations::default());
        let runner = ProcessRunner::new([fixture()])
            .unwrap()
            .observed(observations.clone());
        let scheduler = ResourceScheduler::new(ResourceLimits {
            max_concurrent_tasks: 1,
            max_declared_memory_bytes: 1,
        })
        .unwrap();
        let permit = scheduler
            .try_acquire(
                SessionId::new("regroup").unwrap(),
                ResourceRequest::default(),
            )
            .unwrap();
        let running = {
            let runner = runner.clone();
            let root = directory.clone();
            tokio::spawn(async move {
                runner
                    .run_in_directory_with_permit(
                        DirectoryProcessSpec {
                            program: fixture(),
                            args: vec!["regroup".into(), root.display().to_string()],
                            cwd: fs::File::open(&root).unwrap(),
                            env: BTreeMap::new(),
                            timeout: Duration::from_secs(30),
                            max_output_bytes: 4096,
                            io,
                        },
                        CancellationToken::new(),
                        permit,
                    )
                    .await
            })
        };
        let leader: u32 = parsed_marker(&directory, "root.pid").await;
        wait_until(|| {
            observations
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|(started, identity)| *started && identity.pid == leader)
        })
        .await;
        let original = observations.0.lock().unwrap()[0].1;
        assert_eq!(original.pid, leader);
        assert_eq!(original.process_group, leader);
        fs::write(directory.join("regroup-now"), "start record confirmed").unwrap();
        // regroup.rc is written after setpgid; wait for its integer, not mere
        // existence (truncate-before-write can briefly leave an empty file).
        let helper: u32 = parsed_marker(&directory, "helper.pid").await;
        let rc: i32 = parsed_marker(&directory, "regroup.rc").await;
        let helper_identity = cayw_device::ProcessIdentity::observe(helper).unwrap();
        assert_eq!(helper_identity.process_group, helper);
        assert!(incarnation_is_running(helper_identity).unwrap());
        let regrouped = rc == 0 && directory.join("regrouped.pid").exists();
        let changed = cayw_device::ProcessIdentity::observe(leader).unwrap();
        assert_eq!(changed.started_seconds, original.started_seconds);
        assert_eq!(changed.started_microseconds, original.started_microseconds);
        let left_group = regrouped && changed.process_group == helper;
        // Let the original incarnation stop naturally, then require the owner to
        // join within a bounded time.
        fs::write(directory.join("exit-now"), "exit naturally").unwrap();
        let joined = tokio::time::timeout(Duration::from_secs(15), running).await;
        let outcome = joined.unwrap_or_else(|_| {
            panic!("{name}: supervisor never joined after the leader left its managed group")
        });
        let result = outcome
            .expect("supervisor task panicked")
            .expect("run returned an error");
        runner.drain().await.unwrap();
        let records = observations.0.lock().unwrap().clone();
        let helper_survived = incarnation_is_running(helper_identity).unwrap();
        assert!(helper_survived, "{name}: cleanup signalled the other group");
        assert_eq!(records, vec![(true, original), (false, original)]);
        assert!(!incarnation_is_running(original).unwrap());
        let mut status = 0;
        // SAFETY: this read-only wait must find that the original owner reaped it.
        assert_eq!(
            unsafe { libc::waitpid(leader as libc::pid_t, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        fs::write(
            directory.join("helper-release"),
            "test releases its own helper",
        )
        .unwrap();
        wait_until(|| !incarnation_is_running(helper_identity).unwrap()).await;
        println!(
            "REGROUP io={io:?} original={original:?} helper={helper_identity:?} rc={rc} regrouped={regrouped} left_group={left_group} helper_survived={helper_survived} termination={:?} success={} records={records:?} reserved={}",
            result.termination,
            result.status.success(),
            scheduler.usage().active_tasks
        );
        drop(release);
        fs::remove_dir_all(&directory).unwrap();
        match io {
            cayw_device::ProcessIo::Pipes => assert!(
                left_group,
                "pipes: the OS refused a group leader leaving its managed group"
            ),
            cayw_device::ProcessIo::Pty { .. } => assert!(
                !regrouped,
                "pty: a session leader changed its own process group"
            ),
        }
        assert_eq!(result.termination, TerminationReason::Exited, "{name}");
        assert!(result.status.success(), "{name}");
        assert_eq!(scheduler.usage().active_tasks, 0, "{name}");
        assert_eq!(runner.active_count(), 0, "{name}");
    }
}

#[derive(Debug)]
struct FailedRecords {
    phase: &'static str,
    panics: bool,
    identities: std::sync::Mutex<Vec<cayw_device::ProcessIdentity>>,
}
impl FailedRecords {
    fn record(
        &self,
        phase: &str,
        identities: &[cayw_device::ProcessIdentity],
    ) -> std::io::Result<()> {
        self.identities
            .lock()
            .unwrap()
            .extend_from_slice(identities);
        if self.phase == phase {
            assert!(!self.panics, "injected {phase} process record panic");
            return Err(std::io::Error::other(format!(
                "injected {phase} process record refusal"
            )));
        }
        Ok(())
    }
}
impl cayw_device::ProcessObserver for FailedRecords {
    fn started(&self, identity: &cayw_device::ProcessIdentity) -> std::io::Result<()> {
        self.record("started", &[*identity])
    }
    fn observed(&self, identities: &[cayw_device::ProcessIdentity]) -> std::io::Result<()> {
        self.record("observed", identities)
    }
    fn stopped(&self, identity: &cayw_device::ProcessIdentity) -> std::io::Result<()> {
        self.record("stopped", &[*identity])
    }
}

// Losing any process record must quarantine its original reservation, including
// a panicking host callback. A successful OS cleanup cannot invent that record.
#[tokio::test]
async fn failed_process_records_keep_the_original_permit_after_real_child_cleanup() {
    let mut missing = Vec::new();
    for phase in ["started", "observed", "stopped"] {
        for panics in [false, true] {
            for (label, io) in [
                ("pipes", cayw_device::ProcessIo::Pipes),
                (
                    "pty",
                    cayw_device::ProcessIo::Pty {
                        columns: 80,
                        rows: 24,
                    },
                ),
            ] {
                let directory = std::env::temp_dir().join(format!(
                    "cayw-record-failure-{}-{phase}-{panics}-{label}",
                    std::process::id()
                ));
                fs::create_dir(&directory).unwrap();
                let program = if phase == "observed" {
                    fixture()
                } else {
                    PathBuf::from("/usr/bin/true")
                };
                let records = std::sync::Arc::new(FailedRecords {
                    phase,
                    panics,
                    identities: Default::default(),
                });
                let runner = ProcessRunner::new([program.clone()])
                    .unwrap()
                    .observed(records.clone());
                let scheduler = ResourceScheduler::new(ResourceLimits {
                    max_concurrent_tasks: 1,
                    max_declared_memory_bytes: 1,
                })
                .unwrap();
                let permit = scheduler
                    .try_acquire(
                        SessionId::new("failed-record").unwrap(),
                        ResourceRequest::default(),
                    )
                    .unwrap();
                let result = tokio::time::timeout(
                    Duration::from_secs(15),
                    runner.run_in_directory_with_permit(
                        DirectoryProcessSpec {
                            program,
                            args: if phase == "observed" {
                                vec!["tree".into(), directory.display().to_string()]
                            } else {
                                vec![]
                            },
                            cwd: fs::File::open(&directory).unwrap(),
                            env: BTreeMap::new(),
                            timeout: Duration::from_secs(10),
                            max_output_bytes: 4096,
                            io,
                        },
                        CancellationToken::new(),
                        permit,
                    ),
                )
                .await
                .expect("the original supervisor did not finish real OS cleanup");
                let identities = records.identities.lock().unwrap().clone();
                let original = identities
                    .first()
                    .expect("no actual OS identity reached the observer");
                for identity in &identities {
                    let status = std::process::Command::new("/bin/ps")
                        .args(["-o", "stat=", "-p", &identity.pid.to_string()])
                        .output()
                        .unwrap();
                    assert!(
                        !status.status.success()
                            || String::from_utf8_lossy(&status.stdout)
                                .split_whitespace()
                                .all(|state| state.starts_with('Z')),
                        "recorded child remains running: {identity:?}"
                    );
                }
                let mut status = 0;
                // SAFETY: checks the direct child whose original owner has finished.
                assert_eq!(
                    unsafe {
                        libc::waitpid(original.pid as libc::pid_t, &mut status, libc::WNOHANG)
                    },
                    -1
                );
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ECHILD)
                );
                let drain = runner.drain().await;
                let retained = scheduler.usage().active_tasks;
                let failure = runner.cleanup_failure();
                println!(
                    "RECORD_FAILURE phase={phase} panics={panics} io={io:?} identities={identities:?} result={result:?} drain={drain:?} reserved={retained} failure={failure:?}"
                );
                if !matches!(result, Err(cayw_device::DeviceError::CleanupUnconfirmed(_)))
                    || !matches!(drain, Err(cayw_device::DeviceError::CleanupUnconfirmed(_)))
                    || retained != 1
                    || failure.is_none()
                    || !runner.is_closed()
                {
                    missing.push(format!("{phase}/{panics}/{label}"));
                }
                assert_eq!(runner.active_count(), 0);
                drop(runner);
                if scheduler.usage().active_tasks != 1 {
                    missing.push(format!("{phase}/{panics}/{label}/last-owner"));
                }
                fs::remove_dir_all(directory).unwrap();
            }
        }
    }
    assert!(
        missing.is_empty(),
        "failed process records released or confirmed the original scope: {missing:?}"
    );
}

// This exercises existing output accounting through both actual transports.
// Per-stream limits, PTY-only accounting, or publishing before cwd cleanup would
// break the exact/overflow boundary or retain the original directory descriptor.
#[tokio::test]
async fn pipe_and_pty_output_limits_release_the_original_directory_handle_after_join() {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    for (name, io) in [
        ("pipes", cayw_device::ProcessIo::Pipes),
        (
            "pty",
            cayw_device::ProcessIo::Pty {
                columns: 80,
                rows: 24,
            },
        ),
    ] {
        for limit in [256 * 1024, 256 * 1024 - 1, 8192] {
            let directory = std::env::temp_dir().join(format!(
                "cayw-output-handles-{}-{name}-{limit}",
                std::process::id()
            ));
            fs::create_dir(&directory).unwrap();
            let cwd = fs::File::open(&directory).unwrap();
            let original_fd = cwd.as_raw_fd();
            let original_metadata = cwd.metadata().unwrap();
            let observations = std::sync::Arc::new(Observations::default());
            let runner = ProcessRunner::new([fixture()])
                .unwrap()
                .observed(observations.clone());
            let scheduler = ResourceScheduler::new(ResourceLimits {
                max_concurrent_tasks: 1,
                max_declared_memory_bytes: 1,
            })
            .unwrap();
            let permit = scheduler
                .try_acquire(
                    SessionId::new("output-handles").unwrap(),
                    ResourceRequest::default(),
                )
                .unwrap();
            let result = tokio::time::timeout(
                Duration::from_secs(15),
                runner.run_in_directory_with_permit(
                    DirectoryProcessSpec {
                        program: fixture(),
                        args: vec!["both".into(), directory.display().to_string()],
                        cwd,
                        env: BTreeMap::new(),
                        timeout: Duration::from_secs(10),
                        max_output_bytes: limit,
                        io,
                    },
                    CancellationToken::new(),
                    permit,
                ),
            )
            .await
            .expect("bounded dual output did not finish")
            .unwrap();
            runner.drain().await.unwrap();
            let mut status: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: fstat only observes the original descriptor number. If a
            // concurrent thread reused it, compare the unique directory identity.
            let fd_open = unsafe { libc::fstat(original_fd, &mut status) } == 0;
            let original_still_open = fd_open
                && status.st_dev as u64 == original_metadata.dev()
                && status.st_ino == original_metadata.ino();
            let records = observations.0.lock().unwrap().clone();
            let identity = records
                .first()
                .expect("actual original identity was not recorded")
                .1;
            let mut child_status = 0;
            // SAFETY: this observes the actual direct child after its owner joined.
            assert_eq!(
                unsafe {
                    libc::waitpid(
                        identity.pid as libc::pid_t,
                        &mut child_status,
                        libc::WNOHANG,
                    )
                },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
            let exact = limit == 256 * 1024;
            let reason = if exact {
                TerminationReason::Exited
            } else {
                TerminationReason::OutputLimit
            };
            let stdout_o = result.stdout.iter().filter(|byte| **byte == b'o').count();
            let stdout_e = result.stdout.iter().filter(|byte| **byte == b'e').count();
            println!(
                "OUTPUT_HANDLES io={io:?} limit={limit} identity={identity:?} stdout={} stderr={} stdout_o={stdout_o} stdout_e={stdout_e} termination={:?} complete={} original_cwd_open={original_still_open} active={} reserved={}",
                result.stdout.len(),
                result.stderr.len(),
                result.termination,
                result.output_complete,
                runner.active_count(),
                scheduler.usage().active_tasks
            );
            fs::remove_dir_all(&directory).unwrap();
            assert_eq!(result.termination, reason);
            assert_eq!(result.output_complete, exact);
            assert_eq!(result.stdout.len() + result.stderr.len(), limit);
            assert!(
                !original_still_open,
                "{name}: joined owner retained its cwd handle"
            );
            assert_eq!(runner.active_count(), 0);
            assert_eq!(scheduler.usage().active_tasks, 0);
            assert!(records.contains(&(false, identity)));
            match io {
                cayw_device::ProcessIo::Pipes => {
                    assert_eq!(stdout_o, result.stdout.len());
                    assert!(result.stderr.iter().all(|byte| *byte == b'e'));
                    if exact {
                        assert_eq!(result.stdout.len(), 128 * 1024);
                        assert_eq!(result.stderr.len(), 128 * 1024);
                    }
                }
                cayw_device::ProcessIo::Pty { .. } => {
                    assert!(result.stderr.is_empty());
                    assert_eq!(stdout_o + stdout_e, result.stdout.len());
                    if exact {
                        assert_eq!(stdout_o, 128 * 1024);
                        assert_eq!(stdout_e, 128 * 1024);
                    }
                }
            }
        }
    }
}

// A cancelled/exited leader must settle exactly once through the original owner.
// The two ordered endpoints and eight competing releases per transport exercise
// both causes without requiring a timing-dependent winner for the race itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn natural_exit_and_public_cancel_share_one_cleanup_and_reservation() {
    for (name, io) in [
        ("pipes", cayw_device::ProcessIo::Pipes),
        (
            "pty",
            cayw_device::ProcessIo::Pty {
                columns: 80,
                rows: 24,
            },
        ),
    ] {
        for order in 0..10 {
            let directory = std::env::temp_dir().join(format!(
                "cayw-exit-cancel-{}-{name}-{order}",
                std::process::id()
            ));
            fs::create_dir(&directory).unwrap();
            let observations = std::sync::Arc::new(Observations::default());
            let runner = ProcessRunner::new([fixture()])
                .unwrap()
                .observed(observations.clone());
            let scheduler = ResourceScheduler::new(ResourceLimits {
                max_concurrent_tasks: 1,
                max_declared_memory_bytes: 1,
            })
            .unwrap();
            let permit = scheduler
                .try_acquire(
                    SessionId::new("exit-cancel").unwrap(),
                    ResourceRequest::default(),
                )
                .unwrap();
            let cancel = CancellationToken::new();
            let run = {
                let runner = runner.clone();
                let cancel = cancel.clone();
                let directory = directory.clone();
                tokio::spawn(async move {
                    runner
                        .run_in_directory_with_permit(
                            DirectoryProcessSpec {
                                program: fixture(),
                                args: vec!["gated-exit".into(), directory.display().to_string()],
                                cwd: fs::File::open(&directory).unwrap(),
                                env: BTreeMap::new(),
                                timeout: Duration::from_secs(10),
                                max_output_bytes: 1024,
                                io,
                            },
                            cancel,
                            permit,
                        )
                        .await
                })
            };
            wait_until(|| directory.join("root.pid").exists()).await;
            wait_until(|| !observations.0.lock().unwrap().is_empty()).await;
            let identity = observations.0.lock().unwrap()[0].1;
            assert_eq!(scheduler.usage().active_tasks, 1);
            let result = if order == 0 {
                // Cancellation finishes before the natural-exit gate opens.
                cancel.cancel();
                let result = tokio::time::timeout(Duration::from_secs(10), run)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                fs::write(directory.join("exit-now"), "too late").unwrap();
                assert_eq!(result.termination, TerminationReason::Cancelled);
                result
            } else if order == 1 {
                // A completed result stays complete when cancellation arrives later.
                fs::write(directory.join("exit-now"), "exit naturally").unwrap();
                let result = tokio::time::timeout(Duration::from_secs(10), run)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                cancel.cancel();
                assert_eq!(result.termination, TerminationReason::Exited);
                assert!(result.status.success());
                assert!(result.output_complete);
                result
            } else {
                let ready = std::sync::Arc::new(tokio::sync::Barrier::new(3));
                let exiting = {
                    let ready = ready.clone();
                    let directory = directory.clone();
                    tokio::spawn(async move {
                        ready.wait().await;
                        fs::write(directory.join("exit-now"), "competing exit").unwrap();
                    })
                };
                let cancelling = {
                    let ready = ready.clone();
                    let cancel = cancel.clone();
                    tokio::spawn(async move {
                        ready.wait().await;
                        cancel.cancel();
                    })
                };
                ready.wait().await;
                exiting.await.unwrap();
                cancelling.await.unwrap();
                tokio::time::timeout(Duration::from_secs(10), run)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
            };
            runner.drain().await.unwrap();
            let records = observations.0.lock().unwrap().clone();
            println!(
                "EXIT_CANCEL io={io:?} order={order} identity={identity:?} termination={:?} complete={} stdout={} stderr={} starts={} stops={} active={} reserved={}",
                result.termination,
                result.output_complete,
                result.stdout.len(),
                result.stderr.len(),
                records.iter().filter(|(start, _)| *start).count(),
                records.iter().filter(|(start, _)| !*start).count(),
                runner.active_count(),
                scheduler.usage().active_tasks
            );
            assert!(matches!(
                result.termination,
                TerminationReason::Exited | TerminationReason::Cancelled
            ));
            assert_eq!(records, vec![(true, identity), (false, identity)]);
            assert_eq!(scheduler.usage().active_tasks, 0);
            assert_eq!(runner.active_count(), 0);
            let mut status = 0;
            // SAFETY: the original owner must already have reaped this direct child.
            assert_eq!(
                unsafe { libc::waitpid(identity.pid as libc::pid_t, &mut status, libc::WNOHANG) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
            fs::remove_dir_all(directory).unwrap();
        }
    }
}

// Ignoring terminal echo or CRLF expansion would admit the 23-byte run as a
// complete success. Raw bytes are hand-counted: ready=7, echo/reply/stderr=11,
// and the echoed quit frame=6, for one lifetime allowance of 24 bytes.
#[tokio::test]
async fn pty_echo_and_line_endings_share_the_original_lifetime_output_budget() {
    for (budget, reason) in [
        (24, TerminationReason::Exited),
        (23, TerminationReason::OutputLimit),
    ] {
        let directory = std::env::temp_dir().join(format!(
            "cayw-pty-byte-budget-{}-{budget}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let runner = ProcessRunner::new([fixture()]).unwrap();
        let scheduler = ResourceScheduler::new(ResourceLimits {
            max_concurrent_tasks: 1,
            max_declared_memory_bytes: 1,
        })
        .unwrap();
        let permit = scheduler
            .try_acquire(
                SessionId::new("pty-byte-budget").unwrap(),
                ResourceRequest::default(),
            )
            .unwrap();
        let mut process = runner
            .start_interactive_in_directory_with_permit(
                DirectoryProcessSpec {
                    program: fixture(),
                    args: vec!["interactive-budget".into(), directory.display().to_string()],
                    cwd: fs::File::open(&directory).unwrap(),
                    env: BTreeMap::new(),
                    timeout: Duration::from_secs(10),
                    max_output_bytes: budget,
                    io: cayw_device::ProcessIo::Pty {
                        columns: 80,
                        rows: 24,
                    },
                },
                InteractiveLimits {
                    max_write_bytes: 16,
                    max_buffered_stdout_bytes: 64,
                },
                CancellationToken::new(),
                permit,
            )
            .await
            .unwrap();
        let mut output = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), async {
            while output.len() < 7 {
                output.extend(
                    process
                        .stdout
                        .recv()
                        .await
                        .unwrap()
                        .expect("ready output absent"),
                );
            }
            assert_eq!(output, b"ready\r\n");
            process.stdin.write_all(b"ab\n").await.unwrap();
            while output.len() < 18 {
                output.extend(
                    process
                        .stdout
                        .recv()
                        .await
                        .unwrap()
                        .expect("exchange output absent"),
                );
            }
            assert_eq!(output.len(), 18);
            process.stdin.write_all(b"quit\n").await.unwrap();
            while let Some(bytes) = process.stdout.recv().await.unwrap() {
                output.extend(bytes);
            }
        })
        .await
        .expect("terminal byte accounting did not finish");
        let exit = process.control.wait().await.unwrap();
        runner.drain().await.unwrap();
        println!(
            "PTY_BYTES budget={budget} retained={} output={output:?} termination={:?} complete={} active={} reserved={}",
            output.len(),
            exit.termination,
            exit.output_complete,
            runner.active_count(),
            scheduler.usage().active_tasks
        );
        fs::remove_dir_all(directory).unwrap();
        assert_eq!(output.len(), budget);
        assert_eq!(exit.termination, reason);
        assert_eq!(exit.output_complete, budget == 24);
        assert_eq!(scheduler.usage().active_tasks, 0);
        assert_eq!(runner.active_count(), 0);
    }
}

#[tokio::test]
async fn pty_mode_uses_a_controlling_terminal_and_the_same_managed_cleanup() {
    let directory = std::env::temp_dir().join(format!("cayw-macos-pty-{}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let observations = std::sync::Arc::new(Observations::default());
    let runner = ProcessRunner::new([fixture()])
        .unwrap()
        .observed(observations.clone());
    let scheduler = ResourceScheduler::new(ResourceLimits {
        max_concurrent_tasks: 1,
        max_declared_memory_bytes: 1,
    })
    .unwrap();
    let permit = scheduler
        .try_acquire(
            SessionId::new("pty-task").unwrap(),
            ResourceRequest::default(),
        )
        .unwrap();
    // Pipe execution cannot satisfy this: all three descriptors must be attached
    // to one controlling terminal with this child's own foreground group.
    let mut process = runner
        .start_interactive_in_directory_with_permit(
            DirectoryProcessSpec {
                program: fixture(),
                args: vec!["terminal-inspect".into(), directory.display().to_string()],
                cwd: fs::File::open(&directory).unwrap(),
                env: BTreeMap::new(),
                timeout: Duration::from_secs(10),
                max_output_bytes: 4096,
                io: cayw_device::ProcessIo::Pty {
                    columns: 80,
                    rows: 24,
                },
            },
            InteractiveLimits {
                max_write_bytes: 128,
                max_buffered_stdout_bytes: 4096,
            },
            CancellationToken::new(),
            permit,
        )
        .await
        .unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !bytes.contains(&b'\n') {
            bytes.extend(
                process
                    .stdout
                    .recv()
                    .await
                    .unwrap()
                    .expect("terminal greeting"),
            );
        }
    })
    .await
    .unwrap();
    process.stdin.write_all(b"quit\n").await.unwrap();
    while let Some(output) = process.stdout.recv().await.unwrap() {
        bytes.extend(output);
    }
    let exit = process.control.wait().await.unwrap();
    runner.drain().await.unwrap();
    let pid: i32 = parsed_marker(&directory, "root.pid").await;
    fs::remove_dir_all(&directory).unwrap();
    let output = String::from_utf8(bytes).unwrap();
    assert!(
        output.contains(&format!("terminal=(1, 1, 1, {pid}, {pid})")),
        "{output}"
    );
    assert!(output.contains("received=quit"), "{output}");
    assert!(output.contains("terminal-stderr"), "{output}");
    assert!(exit.status.success());
    assert_eq!(exit.termination, TerminationReason::Exited);
    assert!(exit.output_complete);
    assert_eq!(scheduler.usage().active_tasks, 0);
    assert_eq!(runner.active_count(), 0);
    let records = observations.0.lock().unwrap();
    assert_eq!(
        records.len(),
        2,
        "start and cleanup need paired original OS identities"
    );
    assert!(records[0].0 && !records[1].0);
    assert_eq!(records[0].1, records[1].1);
    assert_eq!(records[0].1.pid, pid as u32);
    assert_eq!(records[0].1.process_group, pid as u32);
    assert!(records[0].1.started_seconds > 0);
}
