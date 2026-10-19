#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use cayw_device::{
    DeviceError, ProcessCleanup, ProcessRunner, ProcessSpec, ResourceLimits, ResourceRequest,
    ResourceScheduler, SessionId, TerminationReason,
};
use tokio_util::sync::CancellationToken;

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cayw-device-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        // Also clean up a fixture tree if an assertion fails before normal cleanup.
        if let Ok(pid) = fs::read_to_string(self.0.join("root.pid"))
            && let Ok(pid) = pid.parse::<libc::pid_t>()
        {
            // SAFETY: the PID was written by this test's child, which has its own group.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        if let Ok(pid) = fs::read_to_string(self.0.join("escaped.pid"))
            && let Ok(pid) = pid.parse::<libc::pid_t>()
        {
            // SAFETY: this fixture explicitly created its own separate session.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cayw-device-test-process"))
}

fn runner() -> ProcessRunner {
    ProcessRunner::new([fixture()]).unwrap()
}

fn spec(directory: &Path, mode: &str) -> ProcessSpec {
    ProcessSpec {
        program: fixture(),
        args: vec![mode.into(), directory.to_string_lossy().into_owned()],
        cwd: directory.to_owned(),
        env: BTreeMap::new(),
        timeout: Duration::from_secs(20),
        max_output_bytes: 1024 * 1024,
    }
}

async fn pid(directory: &Path, name: &str) -> libc::pid_t {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(text) = fs::read_to_string(directory.join(name))
                && let Ok(pid) = text.parse()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture did not become ready")
}

async fn assert_gone(pid: libc::pid_t) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            // SAFETY: signal zero only checks whether this fixture PID exists.
            if unsafe { libc::kill(pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return;
            }
            // Linux containers may keep orphan zombies until their init reaps them.
            #[cfg(target_os = "linux")]
            if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat"))
                && stat
                    .rsplit_once(')')
                    .is_some_and(|(_, tail)| tail.starts_with(" Z"))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture process was left running");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn unrelated_non_utf8_process_name_does_not_retain_a_resource_permit() {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};

    // Own and reap the unrelated child even if cleanup fails its deadline.
    struct UnrelatedChild(Child);
    impl Drop for UnrelatedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let directory = TestDirectory::new();
    let mut unrelated = UnrelatedChild(
        Command::new(fixture())
            .arg("non-utf8-name")
            .arg(&directory.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap(),
    );
    let named = pid(&directory.0, "named.pid").await;
    assert_eq!(named as u32, unrelated.0.id());
    let stat = fs::read(format!("/proc/{named}/stat")).unwrap();
    assert!(std::str::from_utf8(&stat).is_err());
    assert!(stat.windows(5).any(|bytes| bytes == b") (\xff)"));

    let scheduler = ResourceScheduler::new(ResourceLimits {
        max_concurrent_tasks: 1,
        max_declared_memory_bytes: 100,
    })
    .unwrap();
    let permit = scheduler
        .try_acquire(SessionId::new("quick").unwrap(), ResourceRequest::default())
        .unwrap();
    let program = PathBuf::from("/usr/bin/true");
    let runner = ProcessRunner::new([program.clone()]).unwrap();
    let process_spec = ProcessSpec {
        program,
        args: Vec::new(),
        cwd: directory.0.clone(),
        env: BTreeMap::new(),
        timeout: Duration::from_secs(5),
        max_output_bytes: 1024,
    };
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        runner.run_with_permit(process_spec, CancellationToken::new(), permit),
    )
    .await;
    assert!(
        result.is_ok(),
        "unrelated process name blocked cleanup; active_tasks={}",
        scheduler.usage().active_tasks
    );
    let result = result.unwrap().unwrap();
    assert!(result.status.success());
    assert_eq!(result.termination, TerminationReason::Exited);
    assert_eq!(scheduler.usage().active_tasks, 0);
    assert!(unrelated.0.try_wait().unwrap().is_none());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancelling_a_non_utf8_named_process_confirms_cleanup_and_releases_its_permit() {
    let directory = TestDirectory::new();
    let scheduler = ResourceScheduler::new(ResourceLimits {
        max_concurrent_tasks: 1,
        max_declared_memory_bytes: 100,
    })
    .unwrap();
    let permit = scheduler
        .try_acquire(SessionId::new("named").unwrap(), ResourceRequest::default())
        .unwrap();
    let cancellation = CancellationToken::new();
    let child_cancellation = cancellation.clone();
    let process_spec = spec(&directory.0, "non-utf8-name");
    let task = tokio::spawn(async move {
        runner()
            .run_with_permit(process_spec, child_cancellation, permit)
            .await
    });
    let named = pid(&directory.0, "named.pid").await;
    assert!(std::str::from_utf8(&fs::read(format!("/proc/{named}/stat")).unwrap()).is_err());
    assert_eq!(scheduler.usage().active_tasks, 1);
    cancellation.cancel();
    let result = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("non-UTF-8 comm blocked cleanup of its own group")
        .unwrap()
        .unwrap();
    assert_eq!(result.termination, TerminationReason::Cancelled);
    assert_eq!(scheduler.usage().active_tasks, 0);
    // A successful return includes waiting for the direct child, not just killing it.
    let mut status = 0;
    // SAFETY: waitpid only inspects this test's direct child.
    assert_eq!(
        unsafe { libc::waitpid(named, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[tokio::test]
async fn explicit_arguments_cwd_environment_and_closed_stdin_reach_the_process() {
    let directory = TestDirectory::new();
    let mut spec = spec(&directory.0, "inspect");
    spec.args[1] = "literal ; $(not-a-command) spaces".into();
    spec.env.insert("CAYW_EXPLICIT".into(), "a value".into());
    let result = runner().run(spec, CancellationToken::new()).await.unwrap();
    assert!(result.status.success());
    assert_eq!(result.termination, TerminationReason::Exited);
    assert_eq!(
        String::from_utf8(result.stdout).unwrap(),
        format!(
            "arg=literal ; $(not-a-command) spaces\ncwd={}\nexplicit=a value\npath=<absent>\nstdin=0\n",
            directory.0.display()
        )
    );
    assert_eq!(result.stderr, b"fixture stderr\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_closes_admission_and_reaps_a_dropped_run() {
    let directory = TestDirectory::new();
    let runner = runner();
    let cloned = runner.clone();
    let process_spec = spec(&directory.0, "tree-no-pipes");
    let task =
        tokio::spawn(async move { cloned.run(process_spec, CancellationToken::new()).await });
    let root = pid(&directory.0, "root.pid").await;
    let middle = pid(&directory.0, "middle.pid").await;
    let leaf = pid(&directory.0, "leaf.pid").await;
    assert_eq!(runner.active_count(), 1);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(10), runner.drain())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(runner.active_count(), 0);
    let mut status = 0;
    // SAFETY: only inspect our direct child, which drain must have reaped.
    assert_eq!(
        unsafe { libc::waitpid(root, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    for pid in [root, middle, leaf] {
        assert_gone(pid).await;
    }
    assert!(matches!(
        runner
            .run(spec(&directory.0, "exit"), CancellationToken::new())
            .await,
        Err(DeviceError::RunnerClosed)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_close_and_admission_cannot_leave_an_untracked_supervisor() {
    use std::sync::Arc;
    let directory = TestDirectory::new();
    let runner = runner();
    let barrier = Arc::new(tokio::sync::Barrier::new(17));
    let mut tasks = vec![];
    for _ in 0..16 {
        let cloned = runner.clone();
        let barrier = barrier.clone();
        let process_spec = spec(&directory.0, "exit");
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            cloned.run(process_spec, CancellationToken::new()).await
        }));
    }
    barrier.wait().await;
    runner.drain().await.unwrap();
    for task in tasks {
        match task.await.unwrap() {
            Ok(result) => assert!(matches!(
                result.termination,
                TerminationReason::Exited | TerminationReason::Cancelled
            )),
            Err(error) => assert!(matches!(
                error,
                DeviceError::RunnerClosed | DeviceError::Cancelled
            )),
        }
    }
    assert_eq!(runner.active_count(), 0);
    runner.drain().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_runner_cancels_an_active_call_and_does_not_cancel_its_input_token() {
    let directory = TestDirectory::new();
    let runner = runner();
    let cloned = runner.clone();
    let cancellation = CancellationToken::new();
    let input = cancellation.clone();
    let process_spec = spec(&directory.0, "tree");
    let run = tokio::spawn(async move { cloned.run(process_spec, input).await });
    let root = pid(&directory.0, "root.pid").await;
    let leaf = pid(&directory.0, "leaf.pid").await;
    runner.drain().await.unwrap();
    assert!(!cancellation.is_cancelled());
    assert_eq!(
        run.await.unwrap().unwrap().termination,
        TerminationReason::Cancelled
    );
    assert_eq!(runner.active_count(), 0);
    for pid in [root, leaf] {
        assert_gone(pid).await;
    }
}

#[tokio::test]
async fn directory_launch_failure_and_invalid_file_handle_release_reservations() {
    use cayw_device::DirectoryProcessSpec;
    use std::os::unix::fs::PermissionsExt;
    let directory = TestDirectory::new();
    let program = directory.0.join("invalid-executable");
    fs::write(&program, [0u8, 1, 2, 3]).unwrap();
    // A regular file without execute permission must fail at OS spawn, before
    // there can be a child to supervise. Avoid ENOEXEC's platform shell fallback.
    fs::set_permissions(&program, fs::Permissions::from_mode(0o600)).unwrap();
    let runner = ProcessRunner::new([program.clone()]).unwrap();
    let scheduler = ResourceScheduler::new(ResourceLimits {
        max_concurrent_tasks: 1,
        max_declared_memory_bytes: 100,
    })
    .unwrap();
    for invalid_handle in [false, true] {
        let permit = scheduler
            .try_acquire(
                SessionId::new("failed").unwrap(),
                ResourceRequest::default(),
            )
            .unwrap();
        let process_spec = DirectoryProcessSpec {
            program: program.clone(),
            args: vec![],
            cwd: fs::File::open(if invalid_handle {
                &program
            } else {
                &directory.0
            })
            .unwrap(),
            env: BTreeMap::new(),
            timeout: Duration::from_secs(10),
            max_output_bytes: 1024,
        };
        let error = runner
            .run_in_directory_with_permit(process_spec, CancellationToken::new(), permit)
            .await
            .unwrap_err();
        assert!(if invalid_handle {
            matches!(error, DeviceError::InvalidSpec(_))
        } else {
            matches!(
                error,
                DeviceError::NotStarted {
                    operation: "launch process",
                    ref source,
                } if source.raw_os_error() == Some(libc::EACCES)
            )
        });
        assert_eq!(scheduler.usage().active_tasks, 0);
        assert_eq!(runner.active_count(), 0);
    }
    runner.drain().await.unwrap();
}

#[tokio::test]
async fn executable_text_without_shebang_never_implicitly_runs_a_shell() {
    use std::os::unix::fs::PermissionsExt;
    let directory = TestDirectory::new();
    let program = directory.0.join("plain-text");
    fs::write(&program, b"printf forbidden > implicit-shell-ran\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    let runner = ProcessRunner::new([program.clone()]).unwrap();
    let result = runner
        .run(
            ProcessSpec {
                program,
                args: vec![],
                cwd: directory.0.clone(),
                env: BTreeMap::new(),
                timeout: Duration::from_secs(10),
                max_output_bytes: 1024,
            },
            CancellationToken::new(),
        )
        .await;
    assert!(!directory.0.join("implicit-shell-ran").exists());
    assert!(
        matches!(result, Err(DeviceError::NotStarted { operation: "launch process", source }) if source.raw_os_error() == Some(libc::ENOEXEC))
    );
    runner.drain().await.unwrap();
}

#[tokio::test]
async fn both_full_pipes_are_drained_without_deadlock() {
    let directory = TestDirectory::new();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        runner().run(spec(&directory.0, "both"), CancellationToken::new()),
    )
    .await
    .expect("full output pipes deadlocked")
    .unwrap();
    assert!(result.status.success());
    assert_eq!(result.stdout, vec![b'o'; 128 * 1024]);
    assert_eq!(result.stderr, vec![b'e'; 128 * 1024]);
}

#[tokio::test]
async fn combined_output_budget_terminates_the_process_and_bounds_retained_bytes() {
    let directory = TestDirectory::new();
    let mut spec = spec(&directory.0, "flood");
    spec.max_output_bytes = 10_000;
    let result = runner().run(spec, CancellationToken::new()).await.unwrap();
    assert_eq!(result.termination, TerminationReason::OutputLimit);
    assert_eq!(result.stdout.len() + result.stderr.len(), 10_000);
    assert!(!result.status.success());
}

#[tokio::test]
async fn nonzero_exit_is_reported_as_a_process_result() {
    let directory = TestDirectory::new();
    let result = runner()
        .run(spec(&directory.0, "exit"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.status.code(), Some(7));
    assert_eq!(result.termination, TerminationReason::Exited);
}

#[tokio::test]
async fn timeout_kills_a_running_process() {
    let directory = TestDirectory::new();
    let mut spec = spec(&directory.0, "sleep");
    spec.timeout = Duration::from_secs(1);
    let result = runner().run(spec, CancellationToken::new()).await.unwrap();
    assert_eq!(result.termination, TerminationReason::TimedOut);
    assert_gone(pid(&directory.0, "root.pid").await).await;
}

#[tokio::test]
async fn cancellation_kills_children_and_grandchildren_and_reaps_the_direct_child() {
    let directory = TestDirectory::new();
    let token = CancellationToken::new();
    let child_token = token.clone();
    let spec = spec(&directory.0, "tree");
    let task = tokio::spawn(async move { runner().run(spec, child_token).await });
    let root = pid(&directory.0, "root.pid").await;
    let middle = pid(&directory.0, "middle.pid").await;
    let leaf = pid(&directory.0, "leaf.pid").await;
    token.cancel();
    let result = task.await.unwrap().unwrap();
    assert_eq!(result.termination, TerminationReason::Cancelled);
    for pid in [root, middle, leaf] {
        assert_gone(pid).await;
    }
    let mut status = 0;
    // SAFETY: waitpid only inspects this test's child PID; the runner must have reaped it.
    assert_eq!(
        unsafe { libc::waitpid(root, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[tokio::test]
async fn dropping_the_run_future_kills_the_whole_tree_without_cancelling_shared_token() {
    let directory = TestDirectory::new();
    let token = CancellationToken::new();
    let child_token = token.clone();
    let spec = spec(&directory.0, "tree");
    let task = tokio::spawn(async move { runner().run(spec, child_token).await });
    let root = pid(&directory.0, "root.pid").await;
    let middle = pid(&directory.0, "middle.pid").await;
    let leaf = pid(&directory.0, "leaf.pid").await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(!token.is_cancelled());
    for pid in [root, middle, leaf] {
        assert_gone(pid).await;
    }
}

#[tokio::test]
async fn normal_exit_stops_remaining_descendants_even_if_they_hold_output_pipes() {
    let directory = TestDirectory::new();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        runner().run(spec(&directory.0, "orphan"), CancellationToken::new()),
    )
    .await
    .expect("descendant-held output pipes hung the run")
    .unwrap();
    assert!(result.status.success());
    assert_eq!(result.termination, TerminationReason::Exited);
    for name in ["root.pid", "middle.pid", "leaf.pid"] {
        assert_gone(pid(&directory.0, name).await).await;
    }
}

#[tokio::test]
async fn normal_exit_stops_descendants_that_closed_output_pipes() {
    let directory = TestDirectory::new();
    let result = runner()
        .run(
            spec(&directory.0, "orphan-no-pipes"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(result.status.success());
    assert_gone(pid(&directory.0, "middle.pid").await).await;
    assert_gone(pid(&directory.0, "leaf.pid").await).await;
}

#[tokio::test]
async fn cancelling_one_run_does_not_stop_another_runs_process_group() {
    let first = TestDirectory::new();
    let second = TestDirectory::new();
    let first_token = CancellationToken::new();
    let second_token = CancellationToken::new();
    let first_spec = spec(&first.0, "sleep");
    let second_spec = spec(&second.0, "sleep");
    let first_cancel = first_token.clone();
    let second_cancel = second_token.clone();
    let first_task = tokio::spawn(async move { runner().run(first_spec, first_cancel).await });
    let second_task = tokio::spawn(async move { runner().run(second_spec, second_cancel).await });
    let first_pid = pid(&first.0, "root.pid").await;
    let second_pid = pid(&second.0, "root.pid").await;
    first_token.cancel();
    assert_eq!(
        first_task.await.unwrap().unwrap().termination,
        TerminationReason::Cancelled
    );
    assert_gone(first_pid).await;
    // SAFETY: signal zero does not change the other run's process.
    assert_eq!(unsafe { libc::kill(second_pid, 0) }, 0);
    assert!(!second_task.is_finished());
    second_token.cancel();
    assert_eq!(
        second_task.await.unwrap().unwrap().termination,
        TerminationReason::Cancelled
    );
    assert_gone(second_pid).await;
}

#[tokio::test]
async fn programs_outside_the_host_allowlist_and_relative_paths_are_rejected() {
    let directory = TestDirectory::new();
    let runner = ProcessRunner::new(std::iter::empty::<PathBuf>()).unwrap();
    assert!(matches!(
        runner
            .run(spec(&directory.0, "exit"), CancellationToken::new())
            .await,
        Err(DeviceError::ProgramNotAllowed(_))
    ));
    let mut relative = spec(&directory.0, "exit");
    relative.program = "cayw-device-test-process".into();
    assert!(matches!(
        runner.run(relative, CancellationToken::new()).await,
        Err(DeviceError::InvalidSpec(_))
    ));
}

#[tokio::test]
async fn cancelling_a_run_returns_only_after_its_resource_permit_is_released() {
    let directory = TestDirectory::new();
    let scheduler = ResourceScheduler::new(ResourceLimits {
        max_concurrent_tasks: 1,
        max_declared_memory_bytes: 100,
    })
    .unwrap();
    let permit = scheduler
        .try_acquire(
            SessionId::new("process").unwrap(),
            ResourceRequest {
                declared_memory_bytes: 100,
                ..ResourceRequest::default()
            },
        )
        .unwrap();
    let cancellation = CancellationToken::new();
    let child_cancellation = cancellation.clone();
    let process_spec = spec(&directory.0, "tree");
    let task = tokio::spawn(async move {
        runner()
            .run_with_permit(process_spec, child_cancellation, permit)
            .await
    });
    let root = pid(&directory.0, "root.pid").await;
    let middle = pid(&directory.0, "middle.pid").await;
    let leaf = pid(&directory.0, "leaf.pid").await;
    assert_eq!(scheduler.usage().active_tasks, 1);
    cancellation.cancel();
    let result = task.await.unwrap().unwrap();
    assert_eq!(result.termination, TerminationReason::Cancelled);
    assert_eq!(scheduler.usage().active_tasks, 0);
    for pid in [root, middle, leaf] {
        assert_gone(pid).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_run_does_not_release_its_permit_before_the_process_has_stopped() {
    let directory = TestDirectory::new();
    let scheduler = ResourceScheduler::new(ResourceLimits {
        max_concurrent_tasks: 1,
        max_declared_memory_bytes: 100,
    })
    .unwrap();
    let request = ResourceRequest {
        declared_memory_bytes: 100,
        ..ResourceRequest::default()
    };
    let permit = scheduler
        .try_acquire(SessionId::new("running").unwrap(), request)
        .unwrap();
    let process_spec = spec(&directory.0, "sleep");
    let task = tokio::spawn(async move {
        runner()
            .run_with_permit(process_spec, CancellationToken::new(), permit)
            .await
    });
    let root = pid(&directory.0, "root.pid").await;
    assert_eq!(scheduler.usage().active_tasks, 1);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let next = tokio::time::timeout(
        Duration::from_secs(10),
        scheduler.acquire(
            SessionId::new("next").unwrap(),
            request,
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let mut status = 0;
    // SAFETY: this checks that the previous direct child has already been reaped.
    assert_eq!(
        unsafe { libc::waitpid(root, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    assert_gone(root).await;
    drop(next);
    assert_eq!(scheduler.usage().active_tasks, 0);
}

#[tokio::test]
async fn releasing_a_permit_waits_for_descendants_that_closed_their_output_pipes() {
    let directory = TestDirectory::new();
    let scheduler = ResourceScheduler::new(ResourceLimits {
        max_concurrent_tasks: 1,
        max_declared_memory_bytes: 100,
    })
    .unwrap();
    let request = ResourceRequest {
        declared_memory_bytes: 100,
        ..ResourceRequest::default()
    };
    let permit = scheduler
        .try_acquire(SessionId::new("first").unwrap(), request)
        .unwrap();
    let cancellation = CancellationToken::new();
    let child_cancellation = cancellation.clone();
    let process_spec = spec(&directory.0, "tree-no-pipes");
    let task = tokio::spawn(async move {
        runner()
            .run_with_permit(process_spec, child_cancellation, permit)
            .await
    });
    let root = pid(&directory.0, "root.pid").await;
    let middle = pid(&directory.0, "middle.pid").await;
    let leaf = pid(&directory.0, "leaf.pid").await;
    cancellation.cancel();
    let _next = tokio::time::timeout(
        Duration::from_secs(10),
        scheduler.acquire(
            SessionId::new("next").unwrap(),
            request,
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    for pid in [root, middle, leaf] {
        #[cfg(target_os = "macos")]
        {
            // SAFETY: getpgid inspects only this fixture's PID; Darwin excludes zombies.
            assert_eq!(
                unsafe { libc::getpgid(pid) },
                -1,
                "previous task is still running"
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
        #[cfg(target_os = "linux")]
        if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
            assert!(
                stat.rsplit_once(')')
                    .is_some_and(|(_, tail)| tail.starts_with(" Z")),
                "previous task is still running"
            );
        }
    }
    assert_eq!(
        task.await.unwrap().unwrap().termination,
        TerminationReason::Cancelled
    );
}

#[tokio::test]
async fn exact_output_budget_is_complete_and_finite_overflow_is_reported() {
    let directory = TestDirectory::new();
    let mut exact = spec(&directory.0, "both");
    exact.max_output_bytes = 256 * 1024;
    let result = runner().run(exact, CancellationToken::new()).await.unwrap();
    assert_eq!(result.termination, TerminationReason::Exited);
    assert!(result.output_complete);
    let mut overflow = spec(&directory.0, "both");
    overflow.max_output_bytes = 256 * 1024 - 1;
    let result = runner()
        .run(overflow, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.termination, TerminationReason::OutputLimit);
    assert!(!result.output_complete);
    assert_eq!(result.stdout.len() + result.stderr.len(), 256 * 1024 - 1);
}

#[tokio::test]
async fn escaped_descendant_holding_pipes_returns_incomplete_output_without_hanging() {
    let directory = TestDirectory::new();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        runner().run(spec(&directory.0, "escape"), CancellationToken::new()),
    )
    .await
    .expect("an escaped pipe owner hung the supervisor")
    .unwrap();
    assert!(result.status.success());
    assert_eq!(result.termination, TerminationReason::Exited);
    assert!(!result.output_complete);
    let escaped = pid(&directory.0, "escaped.pid").await;
    // This confirms the documented boundary: setsid moved it out of our group.
    // SAFETY: signal zero only checks this test fixture process.
    assert_eq!(unsafe { libc::kill(escaped, 0) }, 0);
    // SAFETY: the fixture PID also identifies its separately created group.
    unsafe { libc::kill(-escaped, libc::SIGKILL) };
    assert_gone(escaped).await;
}

struct FallibleResource {
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::mpsc::Receiver<()>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    fail: bool,
}
impl ProcessCleanup for FallibleResource {
    fn cleanup(&mut self) -> std::io::Result<()> {
        self.entered.send(()).unwrap();
        self.release.recv_timeout(Duration::from_secs(15)).unwrap();
        if self.fail {
            Err(std::io::Error::other("injected cleanup refusal"))
        } else {
            Ok(())
        }
    }
}
impl Drop for FallibleResource {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_failure_waits_for_supervisor_join_and_retains_resource_after_last_owner_drop() {
    use std::{
        sync::{Arc, atomic::AtomicBool, mpsc::sync_channel},
        task::Poll,
    };
    let directory = TestDirectory::new();
    let runner = runner();
    let (entered, started) = sync_channel(1);
    let (release, resume) = sync_channel(1);
    let dropped = Arc::new(AtomicBool::new(false));
    let resource = runner.own_cleanup(FallibleResource {
        entered,
        release: resume,
        dropped: dropped.clone(),
        fail: true,
    });
    let cloned = runner.clone();
    let process_spec = spec(&directory.0, "exit");
    let task = tokio::spawn(async move {
        let spec = cayw_device::DirectoryProcessSpec {
            program: process_spec.program,
            args: process_spec.args,
            cwd: fs::File::open(&process_spec.cwd).unwrap(),
            env: process_spec.env,
            timeout: process_spec.timeout,
            max_output_bytes: process_spec.max_output_bytes,
        };
        cloned
            .run_in_directory_with_resources(spec, CancellationToken::new(), resource)
            .await
    });
    tokio::task::spawn_blocking(move || started.recv_timeout(Duration::from_secs(15)).unwrap())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let mut one = Box::pin(runner.drain());
    let mut two = Box::pin(runner.drain());
    assert!(std::future::poll_fn(|cx| Poll::Ready(one.as_mut().poll(cx).is_pending())).await);
    assert!(std::future::poll_fn(|cx| Poll::Ready(two.as_mut().poll(cx).is_pending())).await);
    assert_eq!(runner.active_count(), 1);
    assert!(!dropped.load(Ordering::SeqCst));
    drop(one);
    release.send(()).unwrap();
    assert!(matches!(two.await, Err(DeviceError::CleanupUnconfirmed(_))));
    assert_eq!(runner.active_count(), 0);
    assert!(matches!(
        runner.drain().await,
        Err(DeviceError::CleanupUnconfirmed(_))
    ));
    assert!(runner.is_closed());
    assert!(
        runner
            .cleanup_failure()
            .unwrap()
            .contains("injected cleanup refusal")
    );
    drop(runner);
    assert!(
        !dropped.load(Ordering::SeqCst),
        "last owner released an unreconciled resource"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_runner_drop_cancels_supervisor_and_successful_cleanup_releases_resource() {
    use std::sync::{Arc, atomic::AtomicBool, mpsc::sync_channel};
    let directory = TestDirectory::new();
    let runner = runner();
    let (entered, started) = sync_channel(1);
    let (release, resume) = sync_channel(1);
    let dropped = Arc::new(AtomicBool::new(false));
    let resource = runner.own_cleanup(FallibleResource {
        entered,
        release: resume,
        dropped: dropped.clone(),
        fail: false,
    });
    let mut run = Box::pin(runner.run_in_directory_with_resources(
        cayw_device::DirectoryProcessSpec {
            program: fixture(),
            args: vec!["exit".into(), directory.0.display().to_string()],
            cwd: fs::File::open(&directory.0).unwrap(),
            env: BTreeMap::new(),
            timeout: Duration::from_secs(10),
            max_output_bytes: 1024,
        },
        CancellationToken::new(),
        resource,
    ));
    std::future::poll_fn(|cx| {
        assert!(run.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    tokio::task::spawn_blocking(move || started.recv_timeout(Duration::from_secs(15)).unwrap())
        .await
        .unwrap();
    drop(run);
    drop(runner);
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn early_cleanup_failure_closes_admission_and_cannot_be_lost_by_dropping_runner() {
    use std::sync::{Arc, atomic::AtomicBool};
    let runner = runner();
    let dropped = Arc::new(AtomicBool::new(false));
    struct Refused(Arc<AtomicBool>);
    impl ProcessCleanup for Refused {
        fn cleanup(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("early cleanup failure"))
        }
    }
    impl Drop for Refused {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    drop(runner.own_cleanup(Refused(dropped.clone())));
    assert!(runner.is_closed());
    assert_eq!(runner.active_count(), 0);
    assert!(matches!(
        runner.drain().await,
        Err(DeviceError::CleanupUnconfirmed(_))
    ));
    drop(runner);
    assert!(!dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn invalid_limits_environment_and_pre_cancelled_runs_do_not_spawn() {
    let directory = TestDirectory::new();
    for invalid in 0..5 {
        let mut spec = spec(&directory.0, "sleep");
        match invalid {
            0 => spec.timeout = Duration::ZERO,
            1 => spec.max_output_bytes = 0,
            2 => spec.cwd = PathBuf::from("relative"),
            3 => {
                spec.env.insert("bad=name".into(), "value".into());
            }
            _ => spec.args.push("nul\0argument".into()),
        }
        assert!(matches!(
            runner().run(spec, CancellationToken::new()).await,
            Err(DeviceError::InvalidSpec(_))
        ));
    }
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        runner().run(spec(&directory.0, "sleep"), cancelled).await,
        Err(DeviceError::Cancelled)
    ));
    assert!(!directory.0.join("root.pid").exists());
}
