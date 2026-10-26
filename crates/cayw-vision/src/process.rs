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
use std::thread;
use std::time::Duration;

/// Poll interval while waiting for a short-lived vision child (tesseract/curl).
const POLL: Duration = Duration::from_millis(20);

#[derive(Debug)]
pub(crate) enum RunError {
    Cancelled,
    Io(io::Error),
}

impl RunError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Cancelled => "cancelled".into(),
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

/// Spawn `command`, wait for exit, honor `cancel` by killing the child.
///
/// Stdout/stderr are collected on side threads so a chatty child cannot fill
/// the pipe and deadlock while we poll cancellation.
pub(crate) fn run_command_cancellable(
    mut command: Command,
    cancel: &CancellationToken,
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

    let stdout_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stdout {
            let _ = io::copy(&mut pipe, &mut buf);
        }
        buf
    });
    let stderr_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stderr {
            let _ = io::copy(&mut pipe, &mut buf);
        }
        buf
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
    Ok(Output {
        status,
        stdout,
        stderr,
    })
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
}
