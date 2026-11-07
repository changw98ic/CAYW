//! Bounded JSON-line companion process for React and Vue projects.
//!
//! The companion is a Node process that owns the project's framework tooling.
//! It never writes to the project: it maps and edits source in memory, returns
//! source patches to this crate, and serves two Vite preview origins. The host
//! process keeps the authoritative source bytes and the durable audit.
//!
//! Framing is one JSON object per line in both directions. Reads and writes are
//! bounded by [`MAX_LINE_BYTES`]; a pipe failure, an over-limit frame or a
//! timeout marks the companion unavailable and tears the process group down.
//! There is no automatic restart. Only known method errors leave it running.

use crate::project::ProjectError;
use serde_json::{Value, json};
use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

/// One request or response line may not exceed this many bytes.
pub(crate) const MAX_LINE_BYTES: usize = 32 * 1024 * 1024;

const READ_CHUNK_BYTES: usize = 64 * 1024;
const LOG_TAIL_BYTES: u64 = 2 * 1024;
const NORMAL_CALL_TIMEOUT: Duration = Duration::from_secs(30);
const PREVIEW_CALL_TIMEOUT: Duration = Duration::from_secs(60);
const SHUTDOWN_CALL_TIMEOUT: Duration = Duration::from_secs(10);
const KILL_TIMEOUT: Duration = Duration::from_secs(2);

/// Environment variables the companion may inherit. Everything else, in
/// particular provider credentials and the editor token, is removed.
const ALLOWED_ENVIRONMENT: &[&str] = &["PATH", "HOME", "TMPDIR"];

/// A started companion process and its request serialization.
pub(crate) struct Companion {
    child: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    stdout: Mutex<Option<ChildStdout>>,
    /// Bytes read from stdout that belong to the next line.
    pending: Mutex<Vec<u8>>,
    /// One request at a time. The protocol has no request pipelining.
    request: Mutex<()>,
    next_id: AtomicU64,
    available: AtomicBool,
    pid: u32,
    log_path: PathBuf,
}

impl std::fmt::Debug for Companion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Companion")
            .field("pid", &self.pid)
            .field("available", &self.available())
            .finish_non_exhaustive()
    }
}

impl Companion {
    /// Starts the companion with the given Node executable and script.
    ///
    /// The child runs in `state_dir`, inherits only [`ALLOWED_ENVIRONMENT`],
    /// owns its own process group on Unix, and appends diagnostics to
    /// `state_dir/companion.log`. Nothing is written to the project.
    pub(crate) async fn spawn(
        node: &Path,
        script: &Path,
        state_dir: &Path,
    ) -> Result<Self, ProjectError> {
        let script = resolve_script(script)?;
        let node = resolve_program(node)?;
        std::fs::create_dir_all(state_dir).map_err(|error| {
            unavailable(format!(
                "could not create the companion state directory: {error}"
            ))
        })?;
        let log_path = state_dir.join("companion.log");
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|error| {
                unavailable(format!(
                    "could not open the companion log {}: {error}",
                    log_path.display()
                ))
            })?;
        let mut command = Command::new(&node);
        command
            .arg(&script)
            .current_dir(state_dir)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log))
            .kill_on_drop(true);
        for name in ALLOWED_ENVIRONMENT {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // The companion starts Node tooling that can spawn helpers such as
            // esbuild. One process group lets shutdown clean all of them.
            command.as_std_mut().process_group(0);
        }
        let mut child = command.spawn().map_err(|error| {
            unavailable(format!(
                "could not start the companion with {}: {error}",
                node.display()
            ))
        })?;
        let Some(pid) = child.id() else {
            let _ = child.start_kill();
            return Err(unavailable("the companion exited before it was ready"));
        };
        let Some(stdin) = child.stdin.take() else {
            let _ = child.start_kill();
            return Err(unavailable("the companion has no request pipe"));
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.start_kill();
            return Err(unavailable("the companion has no response pipe"));
        };
        Ok(Self {
            child: Mutex::new(Some(child)),
            stdin: Mutex::new(Some(stdin)),
            stdout: Mutex::new(Some(stdout)),
            pending: Mutex::new(Vec::new()),
            request: Mutex::new(()),
            next_id: AtomicU64::new(1),
            available: AtomicBool::new(true),
            pid,
            log_path,
        })
    }

    /// True while the companion can answer. A transport failure, timeout or
    /// shutdown clears it; there is no restart. An exit that happens between
    /// requests is observed here, so a status request never reports a stopped
    /// companion as available.
    pub(crate) fn available(&self) -> bool {
        if !self.available.load(Ordering::Acquire) {
            return false;
        }
        // Holding the child slot prevents an orderly terminate from reaping the
        // leader between the observation and the group signal below.
        let Ok(child) = self.child.try_lock() else {
            return self.available.load(Ordering::Acquire);
        };
        if child.is_none() {
            return false;
        }
        if self.leader_exited() {
            // Signal the group while the leader is still unreaped: its pid is
            // the group id and must not be freed before the signal.
            self.kill_group();
            self.available.store(false, Ordering::Release);
            return false;
        }
        true
    }

    /// Reports an exited leader without reaping it. `waitpid` would free the
    /// pid, which is also the group id; a later group signal could then reach an
    /// unrelated process. `waitid` with `WNOWAIT` observes the status and
    /// leaves the child in place for the normal terminate/drop path to reap.
    #[cfg(unix)]
    fn leader_exited(&self) -> bool {
        // SAFETY: `waitid` reads the child status into `info`; WNOHANG only
        // polls and WNOWAIT leaves the child unreaped. The pid is our child.
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let result = libc::waitid(
                libc::P_PID,
                self.pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            );
            // On Linux `si_pid` is a method; `si_signo != 0` is the portable
            // waitid(WNOHANG|WNOWAIT) signal that this P_PID child has exited,
            // matching cayw-device's observe_identity pattern.
            result == 0 && info.si_signo != 0
        }
    }

    #[cfg(not(unix))]
    fn leader_exited(&self) -> bool {
        false
    }

    /// Sends one method call and returns its `result`.
    ///
    /// Method errors reported by the companion (an invalid operation, a source
    /// that does not parse) are ordinary known failures and leave the process
    /// running. Pipe failures, framing errors and timeouts mark it unavailable.
    pub(crate) async fn call(&self, method: &str, params: Value) -> Result<Value, ProjectError> {
        let timeout = call_timeout(method);
        let _gate = self.request.lock().await;
        if !self.available.load(Ordering::Acquire) {
            return Err(unavailable("the framework companion is not running"));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({ "id": id, "method": method, "params": params });
        let mut line = serde_json::to_vec(&request).map_err(|error| {
            known_error(
                "companion_request_invalid",
                format!("could not encode the companion request: {error}"),
            )
        })?;
        if line.len() > MAX_LINE_BYTES {
            return Err(known_error(
                "companion_request_too_large",
                format!("the companion request exceeds the {MAX_LINE_BYTES}-byte line limit"),
            ));
        }
        line.push(b'\n');
        match tokio::time::timeout(timeout, self.exchange(id, &line)).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(failure)) => {
                if transport_failure(&failure) {
                    self.mark_unavailable(&failure.message).await;
                }
                Err(failure)
            }
            Err(_) => {
                let failure = known_error(
                    "companion_timeout",
                    format!(
                        "the companion did not answer {method} within {} seconds",
                        timeout.as_secs()
                    ),
                );
                self.mark_unavailable(&failure.message).await;
                Err(failure)
            }
        }
    }

    /// Sends the shutdown request, then waits for the process group to exit.
    /// Best effort: this method never reports failure.
    pub(crate) async fn shutdown(&self) {
        let _gate = self.request.lock().await;
        let was_available = self.available.swap(false, Ordering::AcqRel);
        if was_available {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let line = format!("{{\"id\":{id},\"method\":\"shutdown\",\"params\":{{}}}}\n");
            let _ = tokio::time::timeout(SHUTDOWN_CALL_TIMEOUT, self.exchange(id, line.as_bytes()))
                .await;
        }
        self.terminate().await;
    }

    /// Writes one request and reads exactly one matching response.
    async fn exchange(&self, id: u64, line: &[u8]) -> Result<Value, ProjectError> {
        {
            let mut stdin = self.stdin.lock().await;
            let Some(stdin) = stdin.as_mut() else {
                return Err(known_error(
                    "companion_io",
                    "the companion request pipe is closed",
                ));
            };
            stdin.write_all(line).await.map_err(|failure| {
                known_error(
                    "companion_io",
                    format!("could not write to the companion: {failure}"),
                )
            })?;
            stdin.flush().await.map_err(|failure| {
                known_error(
                    "companion_io",
                    format!("could not flush the companion request: {failure}"),
                )
            })?;
        }
        let response = self.read_line().await?;
        self.parse_response(id, &response)
    }

    /// Reads one newline-terminated response without ever buffering more than
    /// the protocol limit. EOF is a transport failure, not an empty response.
    async fn read_line(&self) -> Result<Vec<u8>, ProjectError> {
        let mut stdout = self.stdout.lock().await;
        let Some(stdout) = stdout.as_mut() else {
            return Err(known_error(
                "companion_io",
                "the companion response pipe is closed",
            ));
        };
        let mut pending = self.pending.lock().await;
        loop {
            if let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                if end > MAX_LINE_BYTES {
                    return Err(known_error(
                        "companion_protocol",
                        format!(
                            "the companion response exceeds the {MAX_LINE_BYTES}-byte line limit"
                        ),
                    ));
                }
                let mut line: Vec<u8> = pending.drain(..=end).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(line);
            }
            if pending.len() > MAX_LINE_BYTES {
                return Err(known_error(
                    "companion_protocol",
                    format!("the companion response exceeds the {MAX_LINE_BYTES}-byte line limit"),
                ));
            }
            let mut chunk = [0_u8; READ_CHUNK_BYTES];
            let read = stdout.read(&mut chunk).await.map_err(|failure| {
                known_error(
                    "companion_io",
                    format!("could not read from the companion: {failure}"),
                )
            })?;
            if read == 0 {
                let tail = log_tail(&self.log_path);
                return Err(known_error(
                    "companion_io",
                    format!(
                        "the companion closed its output; inspect {}: {tail}",
                        self.log_path.display()
                    ),
                ));
            }
            pending.extend_from_slice(&chunk[..read]);
        }
    }

    fn parse_response(&self, id: u64, line: &[u8]) -> Result<Value, ProjectError> {
        let value: Value = serde_json::from_slice(line).map_err(|failure| {
            known_error(
                "companion_protocol",
                format!("the companion response is not JSON: {failure}"),
            )
        })?;
        let object = value.as_object().ok_or_else(|| {
            known_error(
                "companion_protocol",
                "the companion response is not an object",
            )
        })?;
        if object.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(known_error(
                "companion_protocol",
                "the companion response id does not match the request",
            ));
        }
        match object.get("ok").and_then(Value::as_bool) {
            Some(true) => Ok(object.get("result").cloned().unwrap_or(Value::Null)),
            Some(false) => {
                let failure = object.get("error");
                let code = failure
                    .and_then(|value| value.get("code"))
                    .and_then(Value::as_str)
                    .unwrap_or("companion_error");
                let message = failure
                    .and_then(|value| value.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("the companion reported an error");
                Err(known_error(code, message))
            }
            None => Err(known_error(
                "companion_protocol",
                "the companion response has no ok flag",
            )),
        }
    }

    async fn mark_unavailable(&self, reason: &str) {
        self.available.store(false, Ordering::Release);
        eprintln!("cayw-visual-editor: companion unavailable: {reason}");
        self.terminate().await;
    }

    /// Closes stdin, then kills the process group before reaping the leader.
    ///
    /// The leader is also the group id. `wait` reaps it and frees the pid, and
    /// a freed pid can be assigned to an unrelated new group before a later
    /// group signal. Therefore the group is killed while the unreaped child
    /// still reserves its identity; no signal is sent after the reap. The
    /// shutdown request has already had its bounded chance to stop cleanly, so
    /// descendants that outlive the leader are killed rather than waited for.
    async fn terminate(&self) {
        self.available.store(false, Ordering::Release);
        self.stdin.lock().await.take();
        let mut child = self.child.lock().await.take();
        if let Some(child) = child.as_mut() {
            self.kill_group();
            let _ = child.start_kill();
            let _ = tokio::time::timeout(KILL_TIMEOUT, child.wait()).await;
        }
    }

    fn kill_group(&self) {
        #[cfg(unix)]
        if self.pid > 0 {
            // SAFETY: the child was started as the leader of its own process
            // group, so a negative pid targets exactly that group.
            unsafe {
                libc::kill(-(self.pid as libc::pid_t), libc::SIGKILL);
            }
        }
    }
}

impl Drop for Companion {
    fn drop(&mut self) {
        self.available.store(false, Ordering::Release);
        // Drop cannot await. Only signal the group when the child is still
        // owned here; after an orderly shutdown the pid may already be reused.
        if let Ok(mut child) = self.child.try_lock()
            && let Some(mut child) = child.take()
        {
            self.kill_group();
            let _ = child.start_kill();
            let _ = child.try_wait();
        }
    }
}

fn call_timeout(method: &str) -> Duration {
    if method == "preview.start" {
        PREVIEW_CALL_TIMEOUT
    } else {
        NORMAL_CALL_TIMEOUT
    }
}

fn transport_failure(failure: &ProjectError) -> bool {
    matches!(failure.code.as_str(), "companion_io" | "companion_protocol")
}

fn resolve_script(script: &Path) -> Result<PathBuf, ProjectError> {
    let script = script.canonicalize().map_err(|error| {
        known_error(
            "companion_script_missing",
            format!(
                "the framework companion script {} is not available: {error}",
                script.display()
            ),
        )
    })?;
    if !script.is_file() {
        return Err(known_error(
            "companion_script_missing",
            format!("{} is not a file", script.display()),
        ));
    }
    Ok(script)
}

fn resolve_program(node: &Path) -> Result<PathBuf, ProjectError> {
    if node.components().count() > 1 {
        return node.canonicalize().map_err(|error| {
            unavailable(format!(
                "the Node executable {} is not available: {error}",
                node.display()
            ))
        });
    }
    Ok(node.to_path_buf())
}

/// Last bytes of the companion log, used to make an early exit actionable.
fn log_tail(path: &Path) -> String {
    let Ok(mut file) = std::fs::File::open(path) else {
        return "the companion log could not be read".to_owned();
    };
    let length = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    if length > LOG_TAIL_BYTES && file.seek(SeekFrom::End(-(LOG_TAIL_BYTES as i64))).is_err() {
        return "the companion log could not be read".to_owned();
    }
    let mut tail = Vec::new();
    if file.take(LOG_TAIL_BYTES).read_to_end(&mut tail).is_err() {
        return "the companion log could not be read".to_owned();
    }
    let text = String::from_utf8_lossy(&tail);
    let text = text.trim();
    if text.is_empty() {
        "no diagnostics were written".to_owned()
    } else {
        text.to_owned()
    }
}

fn known_error(code: &str, message: impl Into<String>) -> ProjectError {
    ProjectError {
        code: code.to_owned(),
        message: message.into(),
        outcome_unknown: false,
    }
}

fn unavailable(message: impl Into<String>) -> ProjectError {
    known_error("companion_unavailable", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn node_is_installed() -> bool {
        std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_ok()
    }

    async fn start(script: &str) -> Option<(Companion, TempDir)> {
        if !node_is_installed() {
            eprintln!("skipping companion test: node is not installed");
            return None;
        }
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("companion.mjs");
        std::fs::write(&path, script).expect("test script");
        let companion = Companion::spawn(Path::new("node"), &path, directory.path())
            .await
            .expect("spawn companion");
        Some((companion, directory))
    }

    const ECHO: &str = r#"
import { createInterface } from 'node:readline';
const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
lines.on('line', (line) => {
  const request = JSON.parse(line);
  let response;
  if (request.method === 'shutdown') {
    process.stdout.write(JSON.stringify({ id: request.id, ok: true, result: {} }) + '\n', () => process.exit(0));
    return;
  }
  if (request.method === 'boom') {
    response = { id: request.id, ok: false, error: { code: 'boom', message: 'bad request' } };
  } else {
    response = { id: request.id, ok: true, result: {
      method: request.method,
      params: request.params,
      secret: process.env.CAYW_TEST_COMPANION_SECRET ?? null
    } };
  }
  process.stdout.write(JSON.stringify(response) + '\n');
});
"#;

    #[tokio::test]
    async fn round_trip_and_known_method_errors_keep_the_companion_running() {
        let Some((companion, _directory)) = start(ECHO).await else {
            return;
        };
        let result = companion
            .call("echo", json!({ "value": 3 }))
            .await
            .expect("echo result");
        assert_eq!(result["method"], "echo");
        assert_eq!(result["params"]["value"], 3);
        assert!(companion.available());

        let failure = companion.call("boom", json!({})).await.expect_err("boom");
        assert_eq!(failure.code, "boom");
        assert_eq!(failure.message, "bad request");
        assert!(!failure.outcome_unknown);
        assert!(companion.available());

        let result = companion
            .call("again", json!({}))
            .await
            .expect("still available");
        assert_eq!(result["method"], "again");
        companion.shutdown().await;
        assert!(!companion.available());
    }

    #[tokio::test]
    async fn environment_is_cleared_except_the_allowlist() {
        // The unique variable is read only by this test script. Set it before
        // spawn so an inherited environment would be visible if it leaked.
        unsafe {
            std::env::set_var("CAYW_TEST_COMPANION_SECRET", "must-not-leak");
        }
        let started = start(ECHO).await;
        let Some((companion, _directory)) = started else {
            unsafe {
                std::env::remove_var("CAYW_TEST_COMPANION_SECRET");
            }
            return;
        };
        let result = companion.call("env", json!({})).await.expect("env result");
        assert_eq!(result["secret"], Value::Null);
        unsafe {
            std::env::remove_var("CAYW_TEST_COMPANION_SECRET");
        }
        companion.shutdown().await;
    }

    #[tokio::test]
    async fn oversized_requests_are_rejected_without_killing_the_process() {
        let Some((companion, _directory)) = start(ECHO).await else {
            return;
        };
        let oversized = "x".repeat(MAX_LINE_BYTES + 1);
        let failure = companion
            .call("echo", json!({ "value": oversized }))
            .await
            .expect_err("oversized request");
        assert_eq!(failure.code, "companion_request_too_large");
        assert!(!failure.outcome_unknown);
        assert!(companion.available());
        companion
            .call("echo", json!({}))
            .await
            .expect("still usable");
        companion.shutdown().await;
    }

    #[tokio::test]
    async fn oversized_responses_mark_the_companion_unavailable() {
        let script = r#"
import { createInterface } from 'node:readline';
const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
lines.on('line', (line) => {
  const request = JSON.parse(line);
  const payload = 'x'.repeat(34 * 1024 * 1024);
  process.stdout.write(JSON.stringify({ id: request.id, ok: true, result: payload }) + '\n');
});
"#;
        let Some((companion, _directory)) = start(script).await else {
            return;
        };
        let failure = companion
            .call("echo", json!({}))
            .await
            .expect_err("oversized response");
        assert_eq!(failure.code, "companion_protocol");
        assert!(!failure.outcome_unknown);
        assert!(!companion.available());
        let failure = companion
            .call("echo", json!({}))
            .await
            .expect_err("unavailable");
        assert_eq!(failure.code, "companion_unavailable");
    }

    #[tokio::test]
    async fn eof_marks_the_companion_unavailable_without_a_restart() {
        let script = "process.exit(0);\n";
        let Some((companion, _directory)) = start(script).await else {
            return;
        };
        let failure = companion.call("echo", json!({})).await.expect_err("eof");
        assert!(matches!(
            failure.code.as_str(),
            "companion_io" | "companion_protocol"
        ));
        assert!(!failure.outcome_unknown);
        assert!(!companion.available());
        let failure = companion
            .call("echo", json!({}))
            .await
            .expect_err("no restart");
        assert_eq!(failure.code, "companion_unavailable");
    }

    #[tokio::test]
    async fn shutdown_reaps_the_process_group() {
        let script = r#"
import { writeFileSync } from 'node:fs';
import { createInterface } from 'node:readline';
writeFileSync('companion.pid', String(process.pid));
const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
lines.on('line', (line) => {
  const request = JSON.parse(line);
  if (request.method === 'shutdown') {
    process.stdout.write(JSON.stringify({ id: request.id, ok: true, result: {} }) + '\n', () => process.exit(0));
    return;
  }
  process.stdout.write(JSON.stringify({ id: request.id, ok: true, result: {} }) + '\n');
});
setInterval(() => {}, 1000);
"#;
        let Some((companion, directory)) = start(script).await else {
            return;
        };
        companion.call("echo", json!({})).await.expect("echo");
        let pid: i32 = std::fs::read_to_string(directory.path().join("companion.pid"))
            .expect("pid file")
            .trim()
            .parse()
            .expect("pid");
        companion.shutdown().await;
        assert!(!companion.available());
        #[cfg(unix)]
        {
            // SAFETY: signal 0 only checks whether the reaped process remains.
            let alive = unsafe { libc::kill(pid, 0) };
            assert_eq!(alive, -1, "the companion process must be reaped");
        }
    }

    #[cfg(unix)]
    async fn wait_for_pid_file(path: &Path) -> i32 {
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(path)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("pid file {} was not written", path.display());
    }

    #[cfg(unix)]
    fn process_exists(pid: i32) -> bool {
        // SAFETY: signal 0 only checks whether the process can be signalled.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[cfg(unix)]
    async fn wait_until_gone(pid: i32) -> bool {
        for _ in 0..150 {
            if !process_exists(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[cfg(unix)]
    fn kill_if_alive(pid: i32) {
        // SAFETY: best-effort cleanup of a helper started by this test.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_kills_descendants_after_the_leader_exits() {
        let script = r#"
import { spawn } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import { createInterface } from 'node:readline';
const helper = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio: 'ignore' });
writeFileSync('leader.pid', String(process.pid));
writeFileSync('helper.pid', String(helper.pid));
const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
lines.on('line', (line) => {
  const request = JSON.parse(line);
  if (request.method === 'shutdown') {
    process.stdout.write(JSON.stringify({ id: request.id, ok: true, result: {} }) + '\n', () => process.exit(0));
    return;
  }
  process.stdout.write(JSON.stringify({ id: request.id, ok: true, result: {} }) + '\n');
});
setInterval(() => {}, 1000);
"#;
        let Some((companion, directory)) = start(script).await else {
            return;
        };
        companion.call("echo", json!({})).await.expect("echo");
        let helper = wait_for_pid_file(&directory.path().join("helper.pid")).await;
        companion.shutdown().await;
        let gone = wait_until_gone(helper).await;
        if !gone {
            kill_if_alive(helper);
        }
        assert!(
            gone,
            "a helper in the companion process group must not survive shutdown"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_descendant_of_an_immediately_exiting_companion_is_killed() {
        let script = r#"
import { spawn } from 'node:child_process';
import { writeFileSync } from 'node:fs';
const helper = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio: 'ignore' });
writeFileSync('leader.pid', String(process.pid));
writeFileSync('helper.pid', String(helper.pid));
process.exit(0);
"#;
        let Some((companion, directory)) = start(script).await else {
            return;
        };
        let helper = wait_for_pid_file(&directory.path().join("helper.pid")).await;
        let failure = companion.call("echo", json!({})).await.expect_err("eof");
        assert!(matches!(
            failure.code.as_str(),
            "companion_io" | "companion_protocol"
        ));
        let gone = wait_until_gone(helper).await;
        if !gone {
            kill_if_alive(helper);
        }
        assert!(
            gone,
            "a helper must not survive after its companion leader exits"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn availability_reports_an_exited_companion_before_the_next_request() {
        let script = r#"
import { spawn } from 'node:child_process';
import { writeFileSync } from 'node:fs';
const helper = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio: 'ignore' });
writeFileSync('helper.pid', String(helper.pid));
process.exit(0);
"#;
        let Some((companion, directory)) = start(script).await else {
            return;
        };
        let helper = wait_for_pid_file(&directory.path().join("helper.pid")).await;
        let mut stopped = false;
        for _ in 0..100 {
            if !companion.available() {
                stopped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            stopped,
            "an exited companion must be reported as unavailable without a request"
        );
        let failure = companion
            .call("echo", json!({}))
            .await
            .expect_err("unavailable");
        assert_eq!(failure.code, "companion_unavailable");
        let gone = wait_until_gone(helper).await;
        if !gone {
            kill_if_alive(helper);
        }
        assert!(
            gone,
            "observing the exit must also clean the companion process group"
        );
    }

    #[tokio::test]
    async fn a_missing_script_is_reported_before_start() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let missing = directory.path().join("missing.mjs");
        let failure = Companion::spawn(Path::new("node"), &missing, directory.path())
            .await
            .expect_err("missing script");
        assert_eq!(failure.code, "companion_script_missing");
        assert!(!failure.outcome_unknown);
    }

    #[tokio::test]
    async fn the_shipped_companion_answers_scan_and_shutdown() {
        if !node_is_installed() {
            eprintln!("skipping companion test: node is not installed");
            return;
        }
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("framework/companion.mjs");
        let directory = tempfile::tempdir().expect("temporary directory");
        let project = directory.path().join("project");
        std::fs::create_dir_all(project.join("src")).expect("project");
        std::fs::create_dir_all(project.join("node_modules")).expect("ignored directory");
        std::fs::write(
            project.join("index.html"),
            "<html><head></head><body><div id=app></div></body></html>",
        )
        .expect("page");
        std::fs::write(project.join("src/main.jsx"), "export default 1;\n").expect("source");
        std::fs::write(project.join("node_modules/ignored.js"), "ignored\n").expect("ignored");
        let state = directory.path().join("state");
        std::fs::create_dir_all(&state).expect("state");
        let companion = Companion::spawn(Path::new("node"), &script, &state)
            .await
            .expect("shipped companion");
        let result = companion
            .call(
                "scan",
                json!({
                    "root": project,
                    "page": "index.html",
                    "framework": "react",
                }),
            )
            .await
            .expect("scan result");
        assert_eq!(result["paths"], json!(["index.html", "src/main.jsx"]));
        companion.shutdown().await;
        assert!(!companion.available());
    }
}
