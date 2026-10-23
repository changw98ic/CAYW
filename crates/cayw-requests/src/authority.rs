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
    time::{Duration, Instant},
};

// Keep failed authorities stopped for this process's lifetime, even if all store
// connections are dropped while both durable failure-record writes fail. This
// cannot promise survival of process loss when neither record can persist.
struct LocalGeneration {
    id: Option<String>,
    stopped: Arc<AtomicBool>,
}
static STOPPED: OnceLock<Mutex<BTreeMap<PathBuf, LocalGeneration>>> = OnceLock::new();

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
    database: PathBuf,
}
impl AuthorityFence {
    pub fn open(database: &Path) -> Result<Self> {
        Self::open_inner(database, false)
    }
    /// Administrative access still takes the original sidecar lock. It never
    /// creates a missing marker and never clears the old generation's stop.
    pub fn inspect(database: &Path) -> Result<Self> {
        Self::open_inner(database, true)
    }
    fn open_inner(database: &Path, administrative: bool) -> Result<Self> {
        let stopped = STOPPED
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| RequestError::Poisoned)?
            .entry(database.to_owned())
            .or_insert_with(|| LocalGeneration {
                id: None,
                stopped: Arc::new(AtomicBool::new(false)),
            })
            .stopped
            .clone();
        if !administrative && stopped.load(Ordering::Acquire) {
            return Err(RequestError::AuthorityStopped);
        }
        let mut path = database.as_os_str().to_os_string();
        path.push(".authority");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(!administrative)
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
            database: database.to_owned(),
        })
    }
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
    pub fn bind_identity(&self, id: &str) -> Result<()> {
        let mut generations = STOPPED
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| RequestError::Poisoned)?;
        if let Some(generation) = generations.get_mut(&self.database)
            && Arc::ptr_eq(&generation.stopped, &self.stopped)
        {
            generation.id = Some(id.into());
        }
        Ok(())
    }
    pub fn stopped_for(&self, id: &str) -> bool {
        let Ok(generations) = STOPPED.get_or_init(Mutex::default).lock() else {
            return true;
        };
        generations.get(&self.database).is_some_and(|generation| {
            generation.stopped.load(Ordering::Acquire)
                && generation.id.as_deref().is_none_or(|known| known == id)
        })
    }
    /// Installs a distinct process-local generation. Old Arc holders stay
    /// stopped forever; clearing their AtomicBool would revive old store handles.
    pub fn publish_repaired(&self, id: &str) -> Result<()> {
        let mut generations = STOPPED
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| RequestError::Poisoned)?;
        self.stop();
        generations.insert(
            self.database.clone(),
            LocalGeneration {
                id: Some(id.into()),
                stopped: Arc::new(AtomicBool::new(false)),
            },
        );
        Ok(())
    }
    pub fn lock(&self, timeout: Duration) -> Result<FenceGuard<'_>> {
        self.lock_inner(timeout, false)
    }
    pub fn inspect_lock(&self, timeout: Duration) -> Result<FenceGuard<'_>> {
        self.lock_inner(timeout, true)
    }
    fn lock_inner(&self, timeout: Duration, administrative: bool) -> Result<FenceGuard<'_>> {
        if !administrative && self.stopped.load(Ordering::Acquire) {
            return Err(RequestError::AuthorityStopped);
        }
        let file = self.file.lock().map_err(|_| {
            self.stop();
            RequestError::Poisoned
        })?;
        let started = Instant::now();
        loop {
            if !administrative && self.stopped.load(Ordering::Acquire) {
                return Err(RequestError::AuthorityStopped);
            }
            match file.try_lock() {
                Ok(()) => {
                    let guard = FenceGuard { file, fence: self };
                    if !administrative && self.stopped.load(Ordering::Acquire) {
                        return Err(RequestError::AuthorityStopped);
                    }
                    return Ok(guard);
                }
                // Only acquisition is retried. No record, clock, transaction,
                // or permission is observed before the existing owner releases.
                Err(TryLockError::WouldBlock) => {
                    let remaining = timeout.saturating_sub(started.elapsed());
                    if remaining.is_zero() {
                        return Err(RequestError::Database(rusqlite::Error::SqliteFailure(
                            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                            Some("request authority is busy".into()),
                        )));
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(1)));
                }
                Err(TryLockError::Error(error)) => {
                    self.stop();
                    return Err(error.into());
                }
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
    pub fn metadata(&self) -> Result<std::fs::Metadata> {
        Ok(self.file.metadata()?)
    }
    pub fn bytes(&mut self) -> Result<Vec<u8>> {
        self.file.seek(SeekFrom::Start(0))?;
        let size = self.file.metadata()?.len();
        if size > 1024 {
            return Err(RequestError::AuthorityStopped);
        }
        let mut bytes = Vec::with_capacity(size as usize);
        std::io::Read::by_ref(&mut *self.file)
            .take(1025)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    pub fn read(&mut self) -> Result<Option<Record>> {
        let bytes = self.bytes()?;
        if bytes.is_empty() {
            return Ok(None);
        }
        let record: Record =
            serde_json::from_slice(&bytes).map_err(|_| RequestError::AuthorityStopped)?;
        if record.id.is_empty() || record.id.len() > 128 || record.last_now_ms > i64::MAX as u64 {
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
