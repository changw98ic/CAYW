//! Owns fixture children through wait confirmation, including async Future Drop.
//! A dedicated thread drains nonblocking pipes and retains the original Child.
use crate::sdk::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read, Write},
    os::fd::AsRawFd,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

static CLEANUP_UNKNOWN: AtomicBool = AtomicBool::new(false);
pub fn cleanup_unknown() -> bool {
    CLEANUP_UNKNOWN.load(Ordering::Acquire)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildOutput {
    pub process_id: u32,
    pub exit_code: Option<i32>,
    pub success: bool,
    pub timed_out: bool,
    pub stopped: bool,
    pub cleanup_confirmed: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub stdout_sha256: String,
    pub stderr_sha256: String,
    pub error: Option<String>,
}
struct Drain<R> {
    reader: R,
    prefix: Vec<u8>,
    total: u64,
    hash: Sha256,
    eof: bool,
    limit: usize,
}
impl<R: Read + AsRawFd> Drain<R> {
    fn new(reader: R, limit: usize) -> io::Result<Self> {
        // SAFETY: fcntl operates on this owned pipe's live descriptor.
        let flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            reader,
            prefix: vec![],
            total: 0,
            hash: Sha256::new(),
            eof: false,
            limit,
        })
    }
    fn drain(&mut self) -> io::Result<()> {
        let mut buffer = [0; 8192];
        // A continuously writing child must not starve cancellation or wait.
        for _ in 0..16 {
            match self.reader.read(&mut buffer) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(n) => {
                    self.hash.update(&buffer[..n]);
                    self.total = self.total.saturating_add(n as u64);
                    let keep = n.min(self.limit.saturating_sub(self.prefix.len()));
                    self.prefix.extend_from_slice(&buffer[..keep]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}
pub struct ManagedChild {
    pub id: u32,
    stop: Arc<AtomicBool>,
    output: Arc<Mutex<Option<ChildOutput>>>,
    owner: Option<JoinHandle<()>>,
}
impl ManagedChild {
    pub fn spawn(
        mut command: Command,
        timeout: Duration,
        limit: usize,
        receipt: PathBuf,
    ) -> Result<Self> {
        if cleanup_unknown() {
            return Err("previous child cleanup unconfirmed; sampling stopped".into());
        }
        if timeout.is_zero() || limit == 0 || limit > 16 * 1024 * 1024 {
            return Err("invalid child limits".into());
        }
        // This owner is for the tool's own known fixtures, never an arbitrary model command.
        if PathBuf::from(command.get_program()).canonicalize()?
            != std::env::current_exe()?.canonicalize()?
        {
            return Err("child owner accepts this fixture executable only".into());
        }
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        let mut child = command.spawn()?;
        let id = child.id();
        let prepared = (|| -> io::Result<_> {
            Ok((
                Drain::new(
                    child
                        .stdout
                        .take()
                        .ok_or_else(|| io::Error::other("stdout absent"))?,
                    limit,
                )?,
                Drain::new(
                    child
                        .stderr
                        .take()
                        .ok_or_else(|| io::Error::other("stderr absent"))?,
                    limit,
                )?,
            ))
        })();
        let (mut stdout, mut stderr) = match prepared {
            Ok(v) => v,
            Err(error) => {
                let _ = child.kill();
                if child.wait().is_err() {
                    CLEANUP_UNKNOWN.store(true, Ordering::Release);
                }
                return Err(error.into());
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let request = stop.clone();
        let output = Arc::new(Mutex::new(None));
        let destination = output.clone();
        let owner = thread::spawn(move || {
            let start = Instant::now();
            let mut status = None;
            let mut error = None;
            let mut killed = false;
            let mut timed_out = false;
            let mut kill_time = None;
            loop {
                if let Err(e) = stdout.drain().and_then(|()| stderr.drain()) {
                    error = Some(e.to_string());
                }
                match child.try_wait() {
                    Ok(Some(s)) => status = Some(s),
                    Ok(None) => {}
                    Err(e) => {
                        error = Some(e.to_string());
                        break;
                    }
                }
                if status.is_some() && stdout.eof && stderr.eof {
                    break;
                }
                timed_out |= start.elapsed() >= timeout;
                if status.is_none()
                    && !killed
                    && (request.load(Ordering::Acquire) || timed_out || error.is_some())
                {
                    if let Err(e) = child.kill() {
                        error = Some(e.to_string());
                    }
                    killed = true;
                    kill_time = Some(Instant::now());
                }
                if kill_time.is_some_and(|t| t.elapsed() >= Duration::from_secs(5))
                    || (status.is_some() && start.elapsed() >= timeout + Duration::from_secs(5))
                {
                    error = Some("original child or pipe cleanup not confirmed".into());
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
            let confirmed = status.is_some() && stdout.eof && stderr.eof && error.is_none();
            if !confirmed {
                CLEANUP_UNKNOWN.store(true, Ordering::Release);
            }
            let mut value = ChildOutput {
                process_id: id,
                exit_code: status.and_then(|s| s.code()),
                success: status.is_some_and(|s| s.success()),
                timed_out,
                stopped: killed,
                cleanup_confirmed: confirmed,
                stdout: stdout.prefix,
                stderr: stderr.prefix,
                stdout_bytes: stdout.total,
                stderr_bytes: stderr.total,
                stdout_sha256: format!("{:x}", stdout.hash.finalize()),
                stderr_sha256: format!("{:x}", stderr.hash.finalize()),
                error,
            };
            let saved = (|| -> io::Result<()> {
                let mut f = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(receipt)?;
                serde_json::to_writer(&mut f, &value)?;
                f.write_all(b"\n")?;
                f.sync_all()
            })();
            if let Err(e) = saved {
                value.error = Some(format!("child receipt persistence failed: {e}"));
                CLEANUP_UNKNOWN.store(true, Ordering::Release);
            }
            *destination.lock().unwrap_or_else(|e| e.into_inner()) = Some(value);
        });
        Ok(Self {
            id,
            stop,
            output,
            owner: Some(owner),
        })
    }
    pub fn finished(&self) -> bool {
        self.output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
    pub async fn wait(&mut self) -> Result<ChildOutput> {
        while !self.finished() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        if let Some(owner) = self.owner.take()
            && owner.join().is_err()
        {
            CLEANUP_UNKNOWN.store(true, Ordering::Release);
            return Err("child owner panicked".into());
        }
        let output = self
            .output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or("child receipt absent")?;
        if !output.cleanup_confirmed || output.error.is_some() {
            return Err(
                format!("child cleanup/persistence unconfirmed: {:?}", output.error).into(),
            );
        }
        Ok(output)
    }
}
impl Drop for ManagedChild {
    fn drop(&mut self) {
        self.stop();
        // This thread owns a std Child and nonblocking pipes; it needs no async
        // runtime progress. Joining here also covers a dropped enclosing Future.
        if self.owner.take().is_some_and(|t| t.join().is_err()) {
            CLEANUP_UNKNOWN.store(true, Ordering::Release);
        }
    }
}
