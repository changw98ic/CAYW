use crate::{RequestError, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions, TryLockError},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

// Keep failed authorities stopped for this process's lifetime, even if all store
// connections are dropped while both durable failure-record writes fail. This
// cannot promise survival of process loss when neither record can persist.
static STOPPED: OnceLock<Mutex<BTreeMap<PathBuf, Arc<AtomicBool>>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Ready,
    Pending,
    Stopped,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    pub id: String,
    pub last_now_ms: u64,
    pub phase: Phase,
}

/// A separate, synchronously flushed record fences SQLite failures. Its OS lock
/// serializes every participating connection/process, including store reopen.
/// Pending is flushed BEFORE observing the clock or changing permission state.
/// A crash, partial record, or failed SQLite commit therefore cannot authorize
/// work using an older clock. No automatic recovery clears Pending or Stopped.
pub(crate) struct AuthorityFence {
    file: Mutex<File>,
    stopped: Arc<AtomicBool>,
}
impl AuthorityFence {
    pub fn open(database: &Path) -> Result<Self> {
        let stopped = STOPPED
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| RequestError::Poisoned)?
            .entry(database.to_owned())
            .or_default()
            .clone();
        if stopped.load(Ordering::Acquire) {
            return Err(RequestError::AuthorityStopped);
        }
        let mut path = database.as_os_str().to_os_string();
        path.push(".authority");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(PathBuf::from(path))
            .inspect_err(|_| stopped.store(true, Ordering::Release))?;
        // Retain the sidecar directory entry across a system crash as well as a
        // process crash. Other platforms are separately subject to validation.
        #[cfg(unix)]
        if let Some(parent) = database.parent() {
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .inspect_err(|_| stopped.store(true, Ordering::Release))?;
        }
        Ok(Self {
            file: Mutex::new(file),
            stopped,
        })
    }
    pub fn lock(&self) -> Result<FenceGuard<'_>> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(RequestError::AuthorityStopped);
        }
        let file = self.file.lock().map_err(|_| {
            self.stop();
            RequestError::Poisoned
        })?;
        match file.try_lock() {
            Ok(()) => {
                let guard = FenceGuard { file, fence: self };
                if self.stopped.load(Ordering::Acquire) {
                    return Err(RequestError::AuthorityStopped);
                }
                Ok(guard)
            }
            // No clock has been read or persistence attempted. Contention may
            // be retried; it cannot resurrect authority or lose an observation.
            Err(TryLockError::WouldBlock) => {
                Err(RequestError::Database(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                    Some("request authority is busy".into()),
                )))
            }
            Err(TryLockError::Error(error)) => {
                self.stop();
                Err(error.into())
            }
        }
    }
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }
}
pub(crate) struct FenceGuard<'a> {
    file: MutexGuard<'a, File>,
    fence: &'a AuthorityFence,
}
impl FenceGuard<'_> {
    pub fn read(&mut self) -> Result<Option<Record>> {
        self.file.seek(SeekFrom::Start(0))?;
        let size = self.file.metadata()?.len();
        if size == 0 {
            return Ok(None);
        }
        if size > 1024 {
            return Err(RequestError::AuthorityStopped);
        }
        let mut bytes = Vec::with_capacity(size as usize);
        std::io::Read::by_ref(&mut *self.file)
            .take(1025)
            .read_to_end(&mut bytes)?;
        let record: Record =
            serde_json::from_slice(&bytes).map_err(|_| RequestError::AuthorityStopped)?;
        if record.id.len() > 128 || record.last_now_ms > i64::MAX as u64 {
            return Err(RequestError::AuthorityStopped);
        }
        Ok(Some(record))
    }
    pub fn write(&mut self, id: &str, last_now_ms: u64, phase: Phase) -> Result<()> {
        let bytes = serde_json::to_vec(&Record {
            id: id.into(),
            last_now_ms,
            phase,
        })?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.set_len(0)?;
        self.file.write_all(&bytes)?;
        self.file.sync_all()?;
        Ok(())
    }
    pub fn stop(&mut self, id: &str, last_now_ms: u64) -> Result<()> {
        self.fence.stop();
        self.write(id, last_now_ms, Phase::Stopped)
    }
}
impl Drop for FenceGuard<'_> {
    fn drop(&mut self) {
        if File::unlock(&self.file).is_err() {
            self.fence.stop();
        }
    }
}
