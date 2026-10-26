//! Disk-backed recovery for artifact + action snapshots (F09/F19).
//!
//! Product path (relative to a device/session root the host already owns):
//! `{root}/artifacts/recovery.json`
//!
//! This is intentionally **not** ContentStore: F01 result handles stay opaque
//! references; artifact catalog recovery uses a clear JSON file under the
//! product tree so restart restores Session-owned cards/actions without pulling
//! cayw-content into this crate.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::RecoveryBundle;
use crate::types::ArtifactError;

/// Directory under the product root that holds artifact recovery files.
pub const ARTIFACTS_DIR_NAME: &str = "artifacts";

/// Recovery bundle filename (protocol v1 JSON).
pub const RECOVERY_FILE_NAME: &str = "recovery.json";

/// `{root}/artifacts/recovery.json`
pub fn recovery_path(root: &Path) -> PathBuf {
    root.join(ARTIFACTS_DIR_NAME).join(RECOVERY_FILE_NAME)
}

/// Atomically write a recovery bundle under `{root}/artifacts/recovery.json`.
pub fn save_recovery(root: &Path, bundle: &RecoveryBundle) -> Result<PathBuf, ArtifactError> {
    let dir = root.join(ARTIFACTS_DIR_NAME);
    fs::create_dir_all(&dir).map_err(io_err)?;
    let path = dir.join(RECOVERY_FILE_NAME);
    let tmp = dir.join(format!(".{RECOVERY_FILE_NAME}.tmp"));
    let bytes = serde_json::to_vec_pretty(bundle)
        .map_err(|e| ArtifactError::Invalid(format!("serialize recovery: {e}")))?;
    {
        let mut file = fs::File::create(&tmp).map_err(io_err)?;
        file.write_all(&bytes).map_err(io_err)?;
        file.sync_all().map_err(io_err)?;
    }
    fs::rename(&tmp, &path).map_err(io_err)?;
    // Best-effort directory sync so the rename is durable on crash.
    if let Ok(dir_file) = fs::File::open(&dir) {
        let _ = dir_file.sync_all();
    }
    Ok(path)
}

/// Load a recovery bundle from `{root}/artifacts/recovery.json`.
pub fn load_recovery(root: &Path) -> Result<RecoveryBundle, ArtifactError> {
    let path = recovery_path(root);
    let bytes = fs::read(&path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            ArtifactError::NotFound(path.display().to_string())
        } else {
            io_err(e)
        }
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|e| ArtifactError::Invalid(format!("parse recovery {}: {e}", path.display())))
}

fn io_err(e: io::Error) -> ArtifactError {
    ArtifactError::Io(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ArtifactKind, ArtifactRecord};
    use crate::{ActionSnapshot, ArtifactSnapshot, RecoveryBundle};
    use std::collections::BTreeMap;

    #[test]
    fn round_trip_recovery_file() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = RecoveryBundle {
            artifacts: ArtifactSnapshot {
                artifacts: vec![ArtifactRecord {
                    artifact_id: "card-1".into(),
                    session_id: "sess-a".into(),
                    kind: ArtifactKind::HtmlCard,
                    title: "T".into(),
                    body: "<p>x</p>".into(),
                    content_version: 1,
                    created_at_ms: 1,
                    updated_at_ms: 1,
                    result_handle: None,
                }],
            },
            actions: ActionSnapshot {
                actions: vec![],
                state: BTreeMap::new(),
            },
        };
        let path = save_recovery(dir.path(), &bundle).unwrap();
        assert_eq!(path, recovery_path(dir.path()));
        let loaded = load_recovery(dir.path()).unwrap();
        assert_eq!(loaded, bundle);
    }
}
