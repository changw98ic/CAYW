//! Process-shared coordination for Runtime lifecycle transactions spanning Core
//! and queue connections. This lock is held only by synchronous state changes,
//! never across model execution, capability execution, or an await.
use crate::RuntimeError;
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

pub(crate) struct Coordination {
    path: PathBuf,
}
impl Coordination {
    pub(crate) fn new(database: &Path) -> Result<Self, RuntimeError> {
        let path = database
            .canonicalize()
            .map_err(|e| RuntimeError::Workspace(e.to_string()))?;
        let mut name = path.into_os_string();
        name.push(".runtime-lock");
        let coordinator = Self { path: name.into() };
        let _guard = coordinator.enter()?;
        Ok(coordinator)
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
