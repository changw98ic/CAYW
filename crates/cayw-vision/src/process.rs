//! Cancellable OS process helper for OCR / HTTP describe backends.
//!
//! Host cancel cancels the tool [`CancellationToken`]. Blocking
//! `Command::output()` ignores that token and can leave `tesseract` / `curl`
//! orphans after the Host returns. This helper:
//! - spawns with piped stdio,
//! - holds a [`KillOnDrop`] guard so Drop / cancel always kills the child,
//! - polls `cancel` while waiting so a cancelled tool call reaps the child.

use cayw_ai::CancellationToken;
use std::io;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Poll interval while waiting for a short-lived vision child (tesseract/curl).
const POLL: Duration = Duration::from_millis(20);

/// Default byte limit on stdout/stderr when no tighter budget is specified (16 MiB).
pub(crate) const DEFAULT_MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub(crate) enum RunError {
    Cancelled,
    OutputLimitExceeded(String),
    Io(io::Error),
}

impl RunError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Cancelled => "cancelled".into(),
            Self::OutputLimitExceeded(msg) => msg.clone(),
            Self::Io(err) => err.to_string(),
        }
    }
}

/// Kills and reaps the child on Drop (cancel path, panic, or early return).
struct KillOnDrop(Option<Child>);

impl KillOnDrop {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        match self.0.as_mut() {
            Some(child) => child.try_wait(),
            None => Ok(None),
        }
    }

    fn take(mut self) -> Option<Child> {
        self.0.take()
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn read_bounded_stream(
    pipe: Option<impl io::Read>,
    max_bytes: usize,
    stream_name: &'static str,
    limit_exceeded: Arc<AtomicBool>,
    limit_error: Arc<Mutex<Option<String>>>,
) -> Vec<u8> {
    let mut buf = Vec::new();
    let Some(mut pipe) = pipe else {
        return buf;
    };
    let mut chunk = [0u8; 8192];
    loop {
        if limit_exceeded.load(Ordering::SeqCst) {
            break;
        }
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let current_len = buf.len();
                if current_len.saturating_add(n) > max_bytes {
                    let allowed = max_bytes.saturating_sub(current_len);
                    buf.extend_from_slice(&chunk[..allowed]);
                    let mut lock = limit_error.lock().unwrap();
                    if lock.is_none() {
                        *lock = Some(format!("{stream_name} exceeded limit of {max_bytes} bytes"));
                    }
                    limit_exceeded.store(true, Ordering::SeqCst);
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    buf
}

/// Spawn `command`, wait for exit, honor `cancel` and output byte limits by killing the child.
///
/// Stdout and stderr are collected on side threads up to their specified limits.
/// Exceeding either limit triggers immediate termination and reaping of the child
/// via [`KillOnDrop`], avoiding unbounded memory growth before JSON parsing or caching.
pub(crate) fn run_command_bounded(
    mut command: Command,
    cancel: &CancellationToken,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
) -> Result<Output, RunError> {
    if cancel.is_cancelled() {
        return Err(RunError::Cancelled);
    }

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().map_err(RunError::Io)?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let limit_exceeded = Arc::new(AtomicBool::new(false));
    let limit_error = Arc::new(Mutex::new(None));

    let stdout_flag = limit_exceeded.clone();
    let stdout_err = limit_error.clone();
    let stdout_handle = thread::spawn(move || {
        read_bounded_stream(stdout, max_stdout_bytes, "stdout", stdout_flag, stdout_err)
    });

    let stderr_flag = limit_exceeded.clone();
    let stderr_err = limit_error.clone();
    let stderr_handle = thread::spawn(move || {
        read_bounded_stream(stderr, max_stderr_bytes, "stderr", stderr_flag, stderr_err)
    });

    let mut guard = KillOnDrop::new(child);
    let status = loop {
        if cancel.is_cancelled() {
            // Drop kills + reaps; reader threads see EOF.
            drop(guard);
            let _ = stdout_handle.join();
            let _ = stderr_handle.join();
            return Err(RunError::Cancelled);
        }
        if limit_exceeded.load(Ordering::SeqCst) {
            drop(guard);
            let _ = stdout_handle.join();
            let _ = stderr_handle.join();
            let msg = limit_error
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "output limit exceeded".into());
            return Err(RunError::OutputLimitExceeded(msg));
        }
        match guard.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(POLL),
            Err(err) => {
                drop(guard);
                let _ = stdout_handle.join();
                let _ = stderr_handle.join();
                return Err(RunError::Io(err));
            }
        }
    };

    // Child already exited; disarm kill-on-drop and collect pipes.
    let _ = guard.take();
    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();

    if limit_exceeded.load(Ordering::SeqCst) {
        let msg = limit_error
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "output limit exceeded".into());
        return Err(RunError::OutputLimitExceeded(msg));
    }

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Spawn `command`, wait for exit, honor `cancel` by killing the child.
///
/// Uses [`DEFAULT_MAX_OUTPUT_BYTES`] for stdout and stderr bounds.
pub(crate) fn run_command_cancellable(
    command: Command,
    cancel: &CancellationToken,
) -> Result<Output, RunError> {
    run_command_bounded(
        command,
        cancel,
        DEFAULT_MAX_OUTPUT_BYTES,
        DEFAULT_MAX_OUTPUT_BYTES,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    fn scratch_script(dir: &Path) -> (PathBuf, PathBuf) {
        let pid_path = dir.join("child.pid");
        let script = dir.join("slow.sh");
        let body = format!(
            "#!/bin/sh\nset -e\necho $$ > '{}'\nexec sleep 60\n",
            pid_path.display()
        );
        fs::write(&script, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script, perms).unwrap();
        }
        (script, pid_path)
    }

    fn pid_still_alive(pid: u32) -> bool {
        // Prefer /proc (Linux, no unsafe). macOS CI has no /proc — fall back to
        // `kill -0`, which succeeds iff the process still exists.
        let proc = PathBuf::from(format!("/proc/{pid}"));
        if proc.exists() {
            // Zombies still have a /proc entry; after KillOnDrop::wait the entry
            // is gone. Treat any remaining entry as alive for this cancel test.
            return true;
        }
        #[cfg(unix)]
        {
            Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            false
        }
    }

    #[test]
    fn cancel_kills_child_process() {
        let dir = tempfile::tempdir().unwrap();
        let (script, pid_path) = scratch_script(dir.path());
        let cancel = CancellationToken::new();
        let cancel_worker = cancel.clone();

        let worker = thread::spawn(move || {
            let cmd = Command::new(&script);
            run_command_cancellable(cmd, &cancel_worker)
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if pid_path.exists()
                && fs::read_to_string(&pid_path)
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .is_some()
            {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let pid: u32 = fs::read_to_string(&pid_path)
            .expect("pid file")
            .trim()
            .parse()
            .expect("pid");
        assert!(
            pid_still_alive(pid),
            "child pid {pid} should be alive before cancel"
        );

        cancel.cancel();
        let result = worker.join().expect("worker");
        assert!(matches!(result, Err(RunError::Cancelled)));

        let reap_deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < reap_deadline && pid_still_alive(pid) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !pid_still_alive(pid),
            "child pid {pid} must be gone after cancel (no orphan tesseract/curl)"
        );
    }

    #[test]
    fn already_cancelled_does_not_spawn() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        assert!(matches!(
            run_command_cancellable(cmd, &cancel),
            Err(RunError::Cancelled)
        ));
    }

    #[test]
    fn successful_command_returns_output() {
        let cancel = CancellationToken::new();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo hello-vision");
        let output = run_command_cancellable(cmd, &cancel).unwrap();
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("hello-vision"),
            "stdout={:?}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn bounded_stdout_exact_boundary() {
        let cancel = CancellationToken::new();
        let exact_bytes = b"0123456789"; // 10 bytes

        // Exactly at limit: succeeds
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf '0123456789'");
        let output = run_command_bounded(cmd, &cancel, 10, 1024).unwrap();
        assert!(output.status.success());
        assert_eq!(&output.stdout, exact_bytes);

        // One byte below output length: rejected with OutputLimitExceeded
        let mut cmd2 = Command::new("sh");
        cmd2.arg("-c").arg("printf '0123456789'");
        let err = run_command_bounded(cmd2, &cancel, 9, 1024).unwrap_err();
        assert!(
            matches!(err, RunError::OutputLimitExceeded(ref msg) if msg.contains("stdout exceeded limit of 9 bytes")),
            "expected limit exceeded error, got {err:?}"
        );
    }

    #[test]
    fn bounded_streaming_child_without_eof_is_killed_on_limit() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("streaming.pid");
        let script = dir.path().join("infinite_stream.sh");
        let body = format!(
            "#!/bin/sh\necho $$ > '{}'\nwhile true; do\n  printf 'AAAAAAAAAAAAAAAA'\n  sleep 0.05\ndone\n",
            pid_path.display()
        );
        fs::write(&script, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script, perms).unwrap();
        }

        let cancel = CancellationToken::new();
        let cmd = Command::new(&script);
        // Limit stdout to 32 bytes. Child sends infinite stream without EOF.
        let result = run_command_bounded(cmd, &cancel, 32, 1024);
        assert!(
            matches!(result, Err(RunError::OutputLimitExceeded(ref msg)) if msg.contains("stdout exceeded limit of 32 bytes")),
            "expected OutputLimitExceeded, got {result:?}"
        );

        // Child process must have been killed and reaped (no lingering process).
        if let Ok(content) = fs::read_to_string(&pid_path)
            && let Ok(pid) = content.trim().parse::<u32>()
        {
            let reap_deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < reap_deadline && pid_still_alive(pid) {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(
                !pid_still_alive(pid),
                "streaming child pid {pid} must be killed when output limit exceeded"
            );
        }
    }

    #[test]
    fn bounded_stderr_kills_child_when_exceeded() {
        let cancel = CancellationToken::new();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf 'error-message-overflow' >&2");
        let err = run_command_bounded(cmd, &cancel, 1024, 8).unwrap_err();
        assert!(
            matches!(err, RunError::OutputLimitExceeded(ref msg) if msg.contains("stderr exceeded limit of 8 bytes")),
            "expected stderr limit exceeded error, got {err:?}"
        );
    }

    #[test]
    fn bounded_streaming_stderr_without_eof_is_killed_on_limit() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("streaming_stderr.pid");
        let script = dir.path().join("infinite_stderr_stream.sh");
        let body = format!(
            "#!/bin/sh\necho $$ > '{}'\nwhile true; do\n  printf 'BBBBBBBBBBBBBBBB' >&2\n  sleep 0.05\ndone\n",
            pid_path.display()
        );
        fs::write(&script, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script, perms).unwrap();
        }

        let cancel = CancellationToken::new();
        let cmd = Command::new(&script);
        // Limit stderr to 32 bytes. Child sends infinite stream on stderr without EOF.
        let result = run_command_bounded(cmd, &cancel, 1024, 32);
        assert!(
            matches!(result, Err(RunError::OutputLimitExceeded(ref msg)) if msg.contains("stderr exceeded limit of 32 bytes")),
            "expected stderr OutputLimitExceeded, got {result:?}"
        );

        // Child process must have been killed and reaped (no lingering process).
        if let Ok(content) = fs::read_to_string(&pid_path)
            && let Ok(pid) = content.trim().parse::<u32>()
        {
            let reap_deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < reap_deadline && pid_still_alive(pid) {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(
                !pid_still_alive(pid),
                "streaming child pid {pid} must be killed when stderr limit exceeded"
            );
        }
    }
}
