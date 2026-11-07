#![cfg(unix)]

use cayw_device::{
    DeviceError, DirectoryProcessSpec, InteractiveLimits, InteractiveProcess, ProcessRunner,
    ResourceLimits, ResourceRequest, ResourceScheduler, SessionId, TerminationReason,
};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cayw-interactive-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn fixture() -> PathBuf {
    env!("CARGO_BIN_EXE_cayw-device-test-process").into()
}
fn runner() -> ProcessRunner {
    ProcessRunner::new([fixture()]).unwrap()
}
fn scheduler() -> ResourceScheduler {
    ResourceScheduler::new(ResourceLimits {
        max_concurrent_tasks: 1,
        max_declared_memory_bytes: 1,
    })
    .unwrap()
}
fn spec(directory: &Directory, mode: &str) -> DirectoryProcessSpec {
    DirectoryProcessSpec {
        program: fixture(),
        args: vec![mode.into(), directory.0.display().to_string()],
        cwd: fs::File::open(&directory.0).unwrap(),
        env: BTreeMap::new(),
        timeout: Duration::from_secs(15),
        max_output_bytes: 1024 * 1024,
        io: cayw_device::ProcessIo::Pipes,
    }
}
fn limits() -> InteractiveLimits {
    InteractiveLimits {
        max_write_bytes: 1024 * 1024,
        max_buffered_stdout_bytes: 64,
    }
}
async fn start(
    runner: &ProcessRunner,
    scheduler: &ResourceScheduler,
    spec: DirectoryProcessSpec,
    limits: InteractiveLimits,
    cancel: CancellationToken,
) -> InteractiveProcess {
    let permit = scheduler
        .try_acquire(
            SessionId::new("interactive").unwrap(),
            ResourceRequest::default(),
        )
        .unwrap();
    runner
        .start_interactive_in_directory_with_permit(spec, limits, cancel, permit)
        .await
        .unwrap()
}
async fn marker(directory: &Directory, name: &str) -> i32 {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(value) = fs::read_to_string(directory.0.join(name))
                && let Ok(pid) = value.parse()
            {
                return pid;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fixture marker absent")
}
async fn receive(process: &mut InteractiveProcess, count: usize) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut bytes = Vec::new();
        while bytes.len() < count {
            match process.stdout.recv().await.unwrap() {
                Some(chunk) => bytes.extend(chunk),
                None => {
                    let exit = tokio::time::timeout(Duration::from_secs(2), process.control.wait())
                        .await
                        .ok()
                        .and_then(Result::ok);
                    panic!(
                        "early stdout EOF after {} of {} bytes; termination={exit:?}",
                        bytes.len(),
                        count
                    );
                }
            }
        }
        assert_eq!(bytes.len(), count);
        bytes
    })
    .await
    .unwrap()
}
fn reaped(pid: i32) {
    let mut status = 0;
    // SAFETY: this only checks our fixture child after confirmed cleanup.
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

// Closing stdin, releasing the startup guard too early, or resetting the output
// budget for each write breaks this real multi-exchange protocol.
#[tokio::test(flavor = "current_thread")]
async fn multiple_literal_exchanges_keep_the_process_alive_until_quit() {
    let directory = Directory::new();
    let runner = runner();
    let scheduler = scheduler();
    let mut process = start(
        &runner,
        &scheduler,
        spec(&directory, "interactive-echo"),
        limits(),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(receive(&mut process, 6).await, b"ready\n");
    assert!(process.control.is_running().unwrap());
    for message in [b"first ; $(literal)\n".as_slice(), b"second\n", b"third\n"] {
        process.stdin.write_all(message).await.unwrap();
        assert_eq!(receive(&mut process, message.len()).await, message);
    }
    process.stdin.write_all(b"quit\n").await.unwrap();
    let one = process.control.clone();
    let two = process.control.clone();
    let (first, second) = tokio::join!(one.wait(), two.wait());
    let first = first.unwrap();
    let second = second.unwrap();
    assert!(first.status.success());
    assert_eq!(first.termination, TerminationReason::Exited);
    assert!(first.output_complete);
    assert_eq!(second.status, first.status);
    assert_eq!(process.control.wait().await.unwrap().status, first.status);
    assert!(!process.control.is_running().unwrap());
    assert!(process.stdout.recv().await.unwrap().is_none());
    reaped(marker(&directory, "root.pid").await);
    assert_eq!(scheduler.usage().active_tasks, 0);
    runner.drain().await.unwrap();
}

#[tokio::test]
async fn open_cwd_argv_and_environment_survive_rename_and_replacement() {
    for symlink in [false, true] {
        let directory = Directory::new();
        let original = directory.0.join("opened");
        let moved = directory.0.join("moved");
        let replacement = directory.0.join("other");
        fs::create_dir(&original).unwrap();
        fs::create_dir(&replacement).unwrap();
        fs::write(original.join("marker"), "original").unwrap();
        fs::write(replacement.join("marker"), "replacement").unwrap();
        let mut command = spec(&directory, "interactive-inspect");
        command.cwd = fs::File::open(&original).unwrap();
        command.args.push("literal ; $(argv) spaces".into());
        command
            .env
            .insert("CAYW_EXPLICIT".into(), "value = literal".into());
        fs::rename(&original, &moved).unwrap();
        if symlink {
            std::os::unix::fs::symlink(&replacement, &original).unwrap();
        } else {
            fs::rename(&replacement, &original).unwrap();
        }
        let runner = runner();
        let scheduler = scheduler();
        let mut capacity = limits();
        capacity.max_buffered_stdout_bytes = 1024;
        let mut process = start(
            &runner,
            &scheduler,
            command,
            capacity,
            CancellationToken::new(),
        )
        .await;
        let mut output = vec![];
        while let Some(bytes) = process.stdout.recv().await.unwrap() {
            output.extend(bytes);
        }
        assert_eq!(output, b"arg=literal ; $(argv) spaces\nmarker=original\nexplicit=value = literal\npath=<absent>\n");
        assert!(process.control.wait().await.unwrap().status.success());
        runner.drain().await.unwrap();
    }
}

#[tokio::test]
async fn oversize_write_is_rejected_before_queueing_and_next_valid_frame_works() {
    let directory = Directory::new();
    let runner = runner();
    let scheduler = scheduler();
    let mut capacity = limits();
    capacity.max_write_bytes = 5;
    let mut process = start(
        &runner,
        &scheduler,
        spec(&directory, "interactive-echo"),
        capacity,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(receive(&mut process, 6).await, b"ready\n");
    assert!(matches!(
        process.stdin.write_all(b"too-long\n").await,
        Err(DeviceError::InvalidSpec(_))
    ));
    process.stdin.write_all(b"ok\n").await.unwrap();
    assert_eq!(receive(&mut process, 3).await, b"ok\n");
    process.stdin.write_all(b"quit\n").await.unwrap();
    assert!(process.control.wait().await.unwrap().status.success());
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_a_partially_awaited_write_stops_the_original_process() {
    let directory = Directory::new();
    let runner = runner();
    let scheduler = scheduler();
    let mut process = start(
        &runner,
        &scheduler,
        spec(&directory, "sleep"),
        limits(),
        CancellationToken::new(),
    )
    .await;
    let pid = marker(&directory, "root.pid").await;
    let bytes = vec![b'x'; 1024 * 1024];
    let mut write = Box::pin(process.stdin.write_all(&bytes));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(write.as_mut().poll(cx).is_pending()))
            .await
    );
    drop(write);
    assert_eq!(
        process.control.wait().await.unwrap().termination,
        TerminationReason::Cancelled
    );
    reaped(pid);
    assert_eq!(scheduler.usage().active_tasks, 0);
    assert!(process.stdin.write_all(b"next\n").await.is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn full_stdout_keeps_stderr_lifetime_budget_and_control_responsive() {
    let directory = Directory::new();
    let runner = runner();
    let scheduler = scheduler();
    let mut command = spec(&directory, "interactive-stderr");
    command.max_output_bytes = 16384;
    let process = start(
        &runner,
        &scheduler,
        command,
        limits(),
        CancellationToken::new(),
    )
    .await;
    marker(&directory, "stdout-sent.pid").await;
    fs::write(directory.0.join("flood"), b"go").unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), process.control.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.termination, TerminationReason::OutputLimit);
    assert!(!result.output_complete);
    assert_eq!(scheduler.usage().active_tasks, 0);
}

#[tokio::test]
async fn raw_lifetime_budget_includes_ready_delimiters_and_stderr_across_exchanges() {
    // ready(6) + ab/echo+stderr(5) + cd/echo+stderr(5) = 16 raw bytes across pipes.
    // Budget 16 must survive both exchanges; 15 must trip OutputLimit on the second.
    // Never require a full second echo under budget 15: stderr can consume the last
    // bytes mid-chunk and stage stdout EOF before "cd\n" is fully pushed.
    for (budget, reason) in [
        (16, TerminationReason::Exited),
        (15, TerminationReason::OutputLimit),
    ] {
        let directory = Directory::new();
        let runner = runner();
        let scheduler = scheduler();
        let mut command = spec(&directory, "interactive-budget");
        command.max_output_bytes = budget;
        let mut process = start(
            &runner,
            &scheduler,
            command,
            limits(),
            CancellationToken::new(),
        )
        .await;
        marker(&directory, "root.pid").await;
        assert_eq!(receive(&mut process, 6).await, b"ready\n");
        process.stdin.write_all(b"ab\n").await.unwrap();
        assert_eq!(receive(&mut process, 3).await, b"ab\n");
        process.stdin.write_all(b"cd\n").await.unwrap();
        if budget == 16 {
            assert_eq!(receive(&mut process, 3).await, b"cd\n");
            process.stdin.write_all(b"quit\n").await.unwrap();
        }
        let result = tokio::time::timeout(Duration::from_secs(10), process.control.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.termination, reason);
        assert_eq!(result.output_complete, budget == 16);
    }
}

#[tokio::test]
async fn cancel_close_endpoint_drop_timeout_and_runner_close_stop_without_stdout_consumer() {
    for action in ["cancel", "control", "stdin", "stdout", "timeout", "runner"] {
        let directory = Directory::new();
        let runner = runner();
        let scheduler = scheduler();
        let token = CancellationToken::new();
        let mut command = spec(&directory, "interactive-gated-output");
        if action == "timeout" {
            command.timeout = Duration::from_secs(1);
        }
        let mut process = start(&runner, &scheduler, command, limits(), token.clone()).await;
        let pid = marker(&directory, "root.pid").await;
        let control = process.control.clone();
        fs::write(directory.0.join("emit"), b"go").unwrap();
        marker(&directory, "emitted.pid").await;
        assert_eq!(receive(&mut process, 64).await, vec![b'x'; 64]);
        match action {
            "cancel" => token.cancel(),
            "control" => control.close(),
            "stdin" => drop(process.stdin),
            "stdout" => drop(process.stdout),
            "runner" => runner.close(),
            _ => {}
        }
        let exit = tokio::time::timeout(Duration::from_secs(10), control.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            exit.termination,
            if action == "timeout" {
                TerminationReason::TimedOut
            } else {
                TerminationReason::Cancelled
            },
            "{action}"
        );
        reaped(pid);
        assert_eq!(scheduler.usage().active_tasks, 0);
        runner.drain().await.unwrap();
    }
}

#[tokio::test]
async fn leader_exit_cleans_descendants_with_and_without_inherited_pipes() {
    for mode in ["orphan", "orphan-no-pipes"] {
        let directory = Directory::new();
        let runner = runner();
        let scheduler = scheduler();
        let process = start(
            &runner,
            &scheduler,
            spec(&directory, mode),
            limits(),
            CancellationToken::new(),
        )
        .await;
        let result = tokio::time::timeout(Duration::from_secs(10), process.control.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(result.status.success());
        assert_eq!(result.termination, TerminationReason::Exited);
        assert!(result.output_complete);
        reaped(marker(&directory, "root.pid").await);
        for name in ["middle.pid", "leaf.pid"] {
            let pid = marker(&directory, name).await;
            // Group teardown can lag the leader wait under load; poll until gone.
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    // SAFETY: pid came from our fixture marker after leader cleanup.
                    if unsafe { libc::getpgid(pid) } == -1 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{name} pid {pid} still has a process group"));
        }
        assert_eq!(scheduler.usage().active_tasks, 0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_wait_and_recv_preserves_future_waits_and_the_original_process() {
    let directory = Directory::new();
    let runner = runner();
    let scheduler = scheduler();
    let mut process = start(
        &runner,
        &scheduler,
        spec(&directory, "interactive-echo"),
        limits(),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(receive(&mut process, 6).await, b"ready\n");
    let mut wait = Box::pin(process.control.wait());
    let mut recv = Box::pin(process.stdout.recv());
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(wait.as_mut().poll(cx).is_pending()))
            .await
    );
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(recv.as_mut().poll(cx).is_pending()))
            .await
    );
    drop(wait);
    drop(recv);
    assert!(process.control.is_running().unwrap());
    process.stdin.write_all(b"again\n").await.unwrap();
    assert_eq!(receive(&mut process, 6).await, b"again\n");
    process.control.close();
    let (one, two) = tokio::join!(process.control.wait(), process.control.wait());
    assert_eq!(one.unwrap().status, two.unwrap().status);
}

// A full stdout buffer must still let the quit command reach the child and
// cleanup finish while every output byte is retained for the consumer.
#[tokio::test(flavor = "current_thread")]
async fn stdout_capacity_pauses_reads_without_blocking_a_new_stdin_write() {
    let directory = Directory::new();
    let runner = runner();
    let scheduler = scheduler();
    let mut capacity = limits();
    capacity.max_buffered_stdout_bytes = 6;
    let mut command = spec(&directory, "interactive-await-quit");
    command.max_output_bytes = 6;
    let mut process = start(
        &runner,
        &scheduler,
        command,
        capacity,
        CancellationToken::new(),
    )
    .await;
    marker(&directory, "root.pid").await;
    process.stdin.write_all(b"quit\n").await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), process.control.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.termination, TerminationReason::Exited);
    assert!(result.output_complete);
    assert_eq!(receive(&mut process, 6).await, b"ready\n");
    assert!(process.stdout.recv().await.unwrap().is_none());
}

#[tokio::test]
async fn idle_prefetched_output_uses_the_same_raw_budget() {
    for (budget, reason) in [
        (6, TerminationReason::Exited),
        (5, TerminationReason::OutputLimit),
    ] {
        let directory = Directory::new();
        let runner = runner();
        let scheduler = scheduler();
        let mut command = spec(&directory, "interactive-await-quit");
        command.max_output_bytes = budget;
        let mut process = start(
            &runner,
            &scheduler,
            command,
            limits(),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(receive(&mut process, budget).await, &b"ready\n"[..budget]);
        if budget == 6 {
            process.stdin.write_all(b"quit\n").await.unwrap();
        }
        let result = tokio::time::timeout(Duration::from_secs(10), process.control.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.termination, reason);
    }
}

#[tokio::test]
async fn last_runner_drop_and_tokio_shutdown_still_leave_cleanup_owned() {
    let directory = Directory::new();
    let runner = runner();
    let scheduler = scheduler();
    let command = spec(&directory, "sleep");
    let cloned = runner.clone();
    let ledger = scheduler.clone();
    let process = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(start(
            &cloned,
            &ledger,
            command,
            limits(),
            CancellationToken::new(),
        ))
    })
    .join()
    .unwrap();
    let pid = marker(&directory, "root.pid").await;
    assert!(process.control.is_running().unwrap());
    drop(runner);
    let result = tokio::time::timeout(Duration::from_secs(10), process.control.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.termination, TerminationReason::Cancelled);
    assert_eq!(scheduler.usage().active_tasks, 0);
    reaped(pid);
}

#[tokio::test]
async fn invalid_representability_and_exec_failure_do_not_leave_admission_owned() {
    use std::os::unix::fs::PermissionsExt;
    let directory = Directory::new();
    let program = directory.0.join("not-an-image");
    fs::write(&program, b"printf forbidden > ran-shell\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    let runner = ProcessRunner::new([program.clone()]).unwrap();
    let scheduler = scheduler();
    for case in ["exec", "timeout", "write", "stdout"] {
        let mut command = spec(&directory, "unused");
        command.program = program.clone();
        let mut capacity = limits();
        match case {
            "timeout" => command.timeout = Duration::MAX,
            "write" => capacity.max_write_bytes = usize::MAX,
            "stdout" => capacity.max_buffered_stdout_bytes = usize::MAX,
            _ => {}
        }
        let permit = scheduler
            .try_acquire(
                SessionId::new("invalid").unwrap(),
                ResourceRequest::default(),
            )
            .unwrap();
        let result = runner
            .start_interactive_in_directory_with_permit(
                command,
                capacity,
                CancellationToken::new(),
                permit,
            )
            .await;
        assert!(matches!(
            result,
            Err(DeviceError::NotStarted { .. } | DeviceError::InvalidSpec(_))
        ));
        assert_eq!(scheduler.usage().active_tasks, 0);
        assert_eq!(runner.active_count(), 0);
    }
    assert!(!directory.0.join("ran-shell").exists());
    runner.drain().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn a_write_cancelled_after_the_child_received_a_prefix_stops_before_next_frame() {
    let directory = Directory::new();
    let runner = runner();
    let scheduler = scheduler();
    let mut process = start(
        &runner,
        &scheduler,
        spec(&directory, "interactive-read-prefix"),
        limits(),
        CancellationToken::new(),
    )
    .await;
    let bytes = vec![b'p'; 1024 * 1024];
    let mut write = Box::pin(process.stdin.write_all(&bytes));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(write.as_mut().poll(cx).is_pending()))
            .await
    );
    let pid = marker(&directory, "prefix.pid").await;
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(write.as_mut().poll(cx).is_pending()))
            .await
    );
    drop(write);
    assert!(process.stdin.write_all(b"must-not-send\n").await.is_err());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), process.control.wait())
            .await
            .unwrap()
            .unwrap()
            .termination,
        TerminationReason::Cancelled
    );
    reaped(pid);
    assert_eq!(scheduler.usage().active_tasks, 0);
}

#[tokio::test]
async fn broken_stdin_error_reaches_writer_stdout_and_repeatable_wait() {
    let directory = Directory::new();
    let runner = runner();
    let scheduler = scheduler();
    let mut process = start(
        &runner,
        &scheduler,
        spec(&directory, "interactive-close-stdin"),
        limits(),
        CancellationToken::new(),
    )
    .await;
    let pid = marker(&directory, "stdin-closed.pid").await;
    let error = process.stdin.write_all(b"frame\n").await.unwrap_err();
    assert!(
        matches!(error, DeviceError::Io { operation: "write stdin", source } if source.kind() == std::io::ErrorKind::BrokenPipe)
    );
    let first = process.control.wait().await.unwrap_err();
    let second = process.control.wait().await.unwrap_err();
    assert!(std::sync::Arc::ptr_eq(&first, &second));
    assert!(matches!(
        &*first,
        DeviceError::Io {
            operation: "write stdin",
            ..
        }
    ));
    assert!(matches!(
        process.stdout.recv().await,
        Err(DeviceError::Io {
            operation: "write stdin",
            ..
        })
    ));
    reaped(pid);
    assert_eq!(scheduler.usage().active_tasks, 0);
    runner.drain().await.unwrap();
}

#[tokio::test]
async fn failed_or_dropped_start_remains_in_drain_and_releases_its_permit() {
    let directory = Directory::new();
    for action in ["drop", "cancel", "closed", "limits"] {
        let runner = runner();
        let scheduler = scheduler();
        let token = CancellationToken::new();
        let mut capacity = limits();
        match action {
            "cancel" => token.cancel(),
            "closed" => runner.close(),
            "limits" => capacity.max_write_bytes = 0,
            _ => {}
        }
        let permit = scheduler
            .try_acquire(SessionId::new("start").unwrap(), ResourceRequest::default())
            .unwrap();
        let mut start = Box::pin(runner.start_interactive_in_directory_with_permit(
            spec(&directory, "sleep"),
            capacity,
            token,
            permit,
        ));
        if action == "drop" {
            std::future::poll_fn(|cx| {
                let result = start.as_mut().poll(cx);
                if let std::task::Poll::Ready(Ok(process)) = result {
                    drop(process);
                }
                std::task::Poll::Ready(())
            })
            .await;
            drop(start);
        } else {
            assert!(start.await.is_err());
        }
        tokio::time::timeout(Duration::from_secs(10), runner.drain())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(scheduler.usage().active_tasks, 0);
        assert_eq!(runner.active_count(), 0);
    }
}
