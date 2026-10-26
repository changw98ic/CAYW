//! Durable Session/Task → device ownership for the WebUI gateway (F13 / SLICE P).
//!
//! The feat/tui-webui `DeviceChannelHub` kept `session_devices` and `task_devices`
//! in memory; a gateway restart lost affinity. This store persists the same maps
//! with atomic replace so ownership survives process restart. Results of
//! background commands remain keyed by Session in Runtime; this store only
//! restores which device should resume a Session/Task after the gateway returns.
//! Pair with [`crate::DeviceRouter`]: store supplies the device id, router supplies
//! the IPC endpoint; metadata restore alone does not route RPCs.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
struct Snapshot {
    session_devices: BTreeMap<String, String>,
    task_devices: BTreeMap<String, String>,
}

/// Process-wide durable ownership maps. Safe to share across gateway tasks.
pub struct OwnershipStore {
    path: PathBuf,
    state: Mutex<Snapshot>,
}

impl OwnershipStore {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let state = if path.exists() {
            let bytes = fs::read(&path)?;
            if bytes.is_empty() {
                Snapshot::default()
            } else {
                serde_json::from_slice(&bytes).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("ownership store: {e}"))
                })?
            }
        } else {
            Snapshot::default()
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    fn persist(path: &Path, state: &Snapshot) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(state)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("tmp");
        {
            let mut file = File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        if let Some(parent) = path.parent() {
            let dir = File::open(parent)?;
            let _ = dir.sync_all();
        }
        Ok(())
    }

    pub fn bind_session(&self, session_id: &str, device_id: &str) -> io::Result<()> {
        validate_id(session_id, "session")?;
        validate_id(device_id, "device")?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .session_devices
            .insert(session_id.to_owned(), device_id.to_owned());
        Self::persist(&self.path, &state)
    }

    pub fn bind_task(&self, task_id: &str, device_id: &str) -> io::Result<()> {
        validate_id(task_id, "task")?;
        validate_id(device_id, "device")?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .task_devices
            .insert(task_id.to_owned(), device_id.to_owned());
        Self::persist(&self.path, &state)
    }

    pub fn session_device(&self, session_id: &str) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session_devices
            .get(session_id)
            .cloned()
    }

    pub fn task_device(&self, task_id: &str) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .task_devices
            .get(task_id)
            .cloned()
    }

    pub fn clear_session(&self, session_id: &str) -> io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.session_devices.remove(session_id);
        Self::persist(&self.path, &state)
    }

    pub fn session_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session_devices
            .len()
    }

    pub fn task_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .task_devices
            .len()
    }
}

fn validate_id(value: &str, label: &str) -> io::Result<()> {
    if value.is_empty()
        || value.len() > 256
        || value.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {label} id"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn ownership_survives_gateway_restart() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ownership.json");
        {
            let store = OwnershipStore::open(&path).unwrap();
            store.bind_session("sess-a", "device-1").unwrap();
            store.bind_task("task-9", "device-1").unwrap();
            assert_eq!(store.session_device("sess-a").as_deref(), Some("device-1"));
            assert_eq!(store.task_device("task-9").as_deref(), Some("device-1"));
        }
        let store = OwnershipStore::open(&path).unwrap();
        assert_eq!(store.session_device("sess-a").as_deref(), Some("device-1"));
        assert_eq!(store.task_device("task-9").as_deref(), Some("device-1"));
        assert!(store.session_device("missing").is_none());
    }

    /// Same bind points the Gateway RPC path uses (session.create / task.enqueue);
    /// reopen must restore the original Session ownership for route/resolve.
    #[test]
    fn reopen_restores_original_session_ownership_for_route() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ownership.json");
        {
            let store = OwnershipStore::open(&path).unwrap();
            // session.create → bind_session; task.enqueue inherits session device
            store.bind_session("sess-original", "device-alpha").unwrap();
            store.bind_task("task-child", "device-alpha").unwrap();
            assert_eq!(
                store.session_device("sess-original").as_deref(),
                Some("device-alpha")
            );
        }
        // Gateway process restart: new OwnershipStore::open is the restore path.
        let restored = OwnershipStore::open(&path).unwrap();
        assert_eq!(
            restored.session_device("sess-original").as_deref(),
            Some("device-alpha"),
            "route/resolve must see the original Session ownership after reopen"
        );
        assert_eq!(
            restored.task_device("task-child").as_deref(),
            Some("device-alpha")
        );
    }
}
