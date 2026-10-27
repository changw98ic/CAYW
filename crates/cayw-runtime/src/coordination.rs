//! Process-shared coordination for Runtime lifecycle transactions spanning Core
//! and queue connections. This lock is held only by synchronous state changes,
//! never across model execution, capability execution, or an await.
//!
//! A second, lifetime-scoped policy lock makes trusted configuration changes
//! safe: every open Runtime holds it shared, and a change temporarily takes it
//! exclusive. A change therefore succeeds only when no other Runtime (in this or
//! another process) can still admit work with the previous policy. Amendments
//! also hold a separate exclusive lock until shared ownership is restored, so
//! two upgraders cannot replace each other's policy during that transition.
use crate::RuntimeError;
use std::{
    cell::RefCell,
    fs::{File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
    sync::{Arc, Weak},
};

thread_local! {
    /// The policy lock owned by the Runtime currently being opened on this
    /// thread. Holding only a Weak means a dropped Runtime never keeps the
    /// shared lifetime lock alive.
    static OPEN_POLICY: RefCell<Option<(PathBuf, Weak<File>)>> = const { RefCell::new(None) };
}

pub(crate) struct BackgroundLock {
    file: File,
}

impl Drop for BackgroundLock {
    fn drop(&mut self) {
        // A concurrent fork can keep the open file description alive until exec.
        // The last owner must release its own lock before closing the descriptor.
        let _ = self.file.unlock();
    }
}

#[derive(Clone)]
pub(crate) struct Coordination {
    path: PathBuf,
    amendment_path: PathBuf,
    /// Shared for the Runtime lifetime. Only `amend_policy` upgrades it.
    policy: Arc<File>,
}
impl Coordination {
    pub(crate) fn new(database: &Path) -> Result<Self, RuntimeError> {
        let path = database
            .canonicalize()
            .map_err(|e| RuntimeError::Workspace(e.to_string()))?;
        let policy_path = policy_path(&path);
        let policy_file = open_policy(&policy_path)?;
        policy_file
            .lock_shared()
            .map_err(|e| RuntimeError::Workspace(format!("runtime policy lock: {e}")))?;
        let policy = Arc::new(policy_file);
        let amendment_path = amendment_path(&path);
        let mut name = path.into_os_string();
        name.push(".runtime-lock");
        let coordinator = Self {
            path: name.into(),
            amendment_path,
            policy,
        };
        let _guard = coordinator.enter()?;
        Ok(coordinator)
    }
    /// Only the opening Runtime may temporarily upgrade its own shared lock.
    /// Finished opens must never leave a handle available for an offline change.
    pub(crate) fn opening_policy(&self, database: &Path) -> Result<PolicyScope, RuntimeError> {
        let path = database
            .canonicalize()
            .map_err(|e| RuntimeError::Workspace(e.to_string()))?;
        let previous =
            OPEN_POLICY.with_borrow_mut(|slot| slot.replace((path, Arc::downgrade(&self.policy))));
        Ok(PolicyScope { previous })
    }
    /// The caller holds `enter()` while changing this Runtime's configuration.
    /// A peer's lifetime shared lock prevents the change before any durable write.
    pub(crate) fn amend_policy<T>(
        &self,
        attempt: impl FnOnce() -> Result<Option<T>, RuntimeError>,
    ) -> Result<Option<T>, RuntimeError> {
        amend_with(&self.policy, &self.amendment_path, true, attempt)
    }
    /// Held by the owned worker through VM/process shutdown and queue settlement.
    /// Recovery must acquire the same lock before changing that task's records.
    pub(crate) fn task(&self, task_id: &str) -> Result<File, RuntimeError> {
        let id = uuid::Uuid::parse_str(task_id)
            .map_err(|_| RuntimeError::Worker("persistent task ID is invalid".into()))?;
        let mut path = self.path.as_os_str().to_owned();
        path.push(format!(".task-{id}"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(PathBuf::from(path))
            .map_err(|e| RuntimeError::Workspace(format!("runtime task lock: {e}")))?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => Err(RuntimeError::Busy),
            Err(error) => Err(RuntimeError::Workspace(format!(
                "runtime task lock: {error}"
            ))),
        }
    }
    /// Held by the background task worker while active.
    /// Other instances check this lock before assuming an orphan state.
    pub(crate) fn background(&self, id: &str) -> Result<BackgroundLock, RuntimeError> {
        let mut path = self.path.as_os_str().to_owned();
        path.push(format!(".background-{id}"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(PathBuf::from(path))
            .map_err(|e| RuntimeError::Workspace(format!("runtime background lock: {e}")))?;
        match file.try_lock() {
            Ok(()) => Ok(BackgroundLock { file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(RuntimeError::Busy),
            Err(error) => Err(RuntimeError::Workspace(format!(
                "runtime background lock: {error}"
            ))),
        }
    }
    pub(crate) fn try_background(&self, id: &str) -> Result<Option<BackgroundLock>, RuntimeError> {
        match self.background(id) {
            Ok(file) => Ok(Some(file)),
            Err(RuntimeError::Busy) => Ok(None),
            Err(error) => Err(error),
        }
    }
    pub(crate) fn enter(&self) -> Result<File, RuntimeError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)
            .map_err(|e| RuntimeError::Workspace(format!("runtime lifecycle lock: {e}")))?;
        file.lock()
            .map_err(|e| RuntimeError::Workspace(format!("runtime lifecycle lock: {e}")))?;
        Ok(file)
    }
}

fn policy_path(database: &Path) -> PathBuf {
    let mut name = database.as_os_str().to_owned();
    name.push(".runtime-policy-lock");
    PathBuf::from(name)
}
fn amendment_path(database: &Path) -> PathBuf {
    let mut name = database.as_os_str().to_owned();
    name.push(".runtime-policy-amend-lock");
    PathBuf::from(name)
}
fn open_policy(path: &Path) -> Result<File, RuntimeError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| RuntimeError::Workspace(format!("runtime policy lock: {e}")))
}
fn policy_in_use() -> RuntimeError {
    RuntimeError::InvalidInput(
        "runtime policy cannot change while another runtime connection is open; stop all runtime connections first".into(),
    )
}

pub(crate) struct PolicyScope {
    previous: Option<(PathBuf, Weak<File>)>,
}
impl Drop for PolicyScope {
    fn drop(&mut self) {
        OPEN_POLICY.with_borrow_mut(|slot| *slot = self.previous.take());
    }
}

/// Offline administration must not borrow an opening or live Runtime's lock.
pub(crate) fn amend_stopped_policy<T>(
    database: &Path,
    attempt: impl FnOnce() -> Result<Option<T>, RuntimeError>,
) -> Result<Option<T>, RuntimeError> {
    let canonical = database
        .canonicalize()
        .map_err(|e| RuntimeError::Workspace(e.to_string()))?;
    let file = open_policy(&policy_path(&canonical))?;
    amend_with(&file, &amendment_path(&canonical), false, attempt)
}

/// Runs a trusted configuration amendment while no other Runtime can admit
/// work with the previous policy. `attempt` re-reads the durable policy under
/// the exclusive lock and returns `None` when another connection already made
/// the requested policy effective.
///
/// `attempt` must not hold a SQLite write transaction while waiting: the
/// upgrade happens before it is called, so its own transaction cannot deadlock
/// against an opener that still holds the shared lock.
pub(crate) fn amend_policy<T>(
    database: &Path,
    attempt: impl FnOnce() -> Result<Option<T>, RuntimeError>,
) -> Result<Option<T>, RuntimeError> {
    let canonical = database
        .canonicalize()
        .map_err(|e| RuntimeError::Workspace(e.to_string()))?;
    let live = OPEN_POLICY.with_borrow(|slot| {
        slot.as_ref()
            .filter(|(path, _)| path == &canonical)
            .and_then(|(_, weak)| weak.upgrade())
    });
    match live {
        Some(file) => amend_with(&file, &amendment_path(&canonical), true, attempt),
        None => {
            // No Runtime in this process owns the lock. Another process may
            // still hold it shared, in which case the exclusive attempt fails
            // without waiting.
            let file = open_policy(&policy_path(&canonical))?;
            amend_with(&file, &amendment_path(&canonical), false, attempt)
        }
    }
}

fn amend_with<T>(
    file: &File,
    amendment_path: &Path,
    keep_shared: bool,
    attempt: impl FnOnce() -> Result<Option<T>, RuntimeError>,
) -> Result<Option<T>, RuntimeError> {
    // A separate fd per attempt serializes every upgrader through restoration
    // of its shared lock. Contention must fail before releasing that lock: two
    // upgraders retaining shared ownership must never wait on each other.
    let _amendment = open_policy(amendment_path)?;
    match _amendment.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Err(policy_in_use()),
        Err(TryLockError::Error(error)) => {
            return Err(RuntimeError::Workspace(format!(
                "runtime policy amendment lock: {error}"
            )));
        }
    }
    file.unlock()
        .map_err(|e| RuntimeError::Workspace(format!("runtime policy lock: {e}")))?;
    let mut guard = PolicyGuard {
        file,
        keep_shared,
        exclusive: false,
    };
    match file.try_lock() {
        Ok(()) => guard.exclusive = true,
        Err(TryLockError::WouldBlock) => return Err(policy_in_use()),
        Err(TryLockError::Error(error)) => {
            return Err(RuntimeError::Workspace(format!(
                "runtime policy lock: {error}"
            )));
        }
    }
    let result = attempt();
    // Restoring the caller's lifetime shared lock is part of the amendment
    // contract: after it succeeds no other connection can replace the policy
    // again while this Runtime lives. A failed restore therefore fails the
    // open even when `attempt` succeeded, instead of running unguarded.
    guard.release()?;
    result
}

struct PolicyGuard<'a> {
    file: &'a File,
    keep_shared: bool,
    exclusive: bool,
}
impl PolicyGuard<'_> {
    /// Releases the exclusive lock and restores the lifetime shared lock. The
    /// normal path calls this explicitly so a failure is reported; `Drop` is
    /// only a best-effort safety net for panics and early returns.
    fn release(&mut self) -> Result<(), RuntimeError> {
        if self.exclusive {
            self.exclusive = false;
            self.file
                .unlock()
                .map_err(|e| RuntimeError::Workspace(format!("runtime policy lock: {e}")))?;
        }
        if self.keep_shared {
            self.keep_shared = false;
            self.file
                .lock_shared()
                .map_err(|e| RuntimeError::Workspace(format!("runtime policy lock: {e}")))?;
        }
        Ok(())
    }
}
impl Drop for PolicyGuard<'_> {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::unix::{net::UnixStream, process::CommandExt},
        process::Command,
        thread,
        time::Duration,
    };

    #[test]
    fn released_background_lock_is_not_retained_by_fork_before_exec() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("sessions.db");
        File::create(&database).unwrap();
        let coordination = Coordination::new(&database).unwrap();
        let id = "inherited-lock";
        let owner = Arc::new(coordination.background(id).unwrap());
        let last_owner = owner.clone();
        let (mut parent, mut child) = UnixStream::pair().unwrap();
        parent
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let spawn = thread::spawn(move || {
            let mut command = Command::new("/usr/bin/true");
            // Only socket I/O on pre-opened descriptors runs after fork. The
            // handshake holds the child before exec while the parent drops its
            // last owner; no timing or child exit is used to release the lock.
            unsafe {
                command.pre_exec(move || {
                    child.write_all(&[1])?;
                    child.read_exact(&mut [0])?;
                    Ok(())
                });
            }
            command.spawn().and_then(|mut child| child.wait())
        });
        parent.read_exact(&mut [0]).unwrap();
        drop(owner);
        let held = coordination.try_background(id);
        drop(last_owner);
        let released = coordination.try_background(id);
        // Always release and reap the original child before asserting results.
        let signal = parent.write_all(&[1]);
        let exit = spawn.join().unwrap();
        signal.unwrap();
        assert!(exit.unwrap().success());
        assert!(held.unwrap().is_none(), "a live owner lost its lock");
        assert!(
            released.unwrap().is_some(),
            "the child retained a finished owner's lock before exec"
        );
    }
}
