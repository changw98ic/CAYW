use crate::{Failure, Result, emit};
use cayw_core::EventRecord;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_CURSOR_BYTES: u64 = 64 * 1024;

pub(crate) fn read_bounded_cursor_file(path: &Path, max_bytes: u64) -> Result<Option<Vec<u8>>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Failure::invalid("cursor file must be a regular file"));
    }
    if metadata.len() > max_bytes {
        return Err(Failure::invalid("cursor file exceeds 64 KiB"));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(Failure::invalid("cursor file exceeds 64 KiB"));
    }
    Ok(Some(bytes))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    session_id: String,
    after: u64,
}
pub struct Cursor {
    pub session: String,
    pub after: u64,
    file: Option<PathBuf>,
}
impl Cursor {
    pub fn load(session: &str, after: Option<u64>, file: Option<PathBuf>) -> Result<Self> {
        let saved =
            match file.as_deref() {
                Some(path) => match read_bounded_cursor_file(path, MAX_CURSOR_BYTES)? {
                    Some(bytes) => Some(serde_json::from_slice::<Saved>(&bytes).map_err(|_| {
                        Failure::invalid("cursor file is not a valid Session cursor")
                    })?),
                    None => None,
                },
                None => None,
            };
        if let Some(saved) = &saved {
            if saved.session_id != session {
                return Err(Failure::invalid("saved cursor belongs to another Session"));
            }
            if after.is_some_and(|after| after != saved.after) {
                return Err(Failure::invalid("after conflicts with the saved cursor"));
            }
        }
        Ok(Self {
            session: session.into(),
            after: saved.map(|saved| saved.after).or(after).unwrap_or(0),
            file,
        })
    }
    pub fn process(&mut self, page: &[EventRecord]) -> Result<()> {
        let mut next = self.after;
        for event in page {
            next = next
                .checked_add(1)
                .ok_or_else(|| Failure::invalid("event cursor overflow"))?;
            if event.session_id != self.session || event.cursor != next {
                return Err(Failure::invalid(
                    "event page has a gap, repeat or different Session",
                ));
            }
        }
        for event in page {
            emit(
                &json!({"type":"event","session_id":self.session,"cursor":event.cursor,"event":event}),
            )?;
        }
        if let Some(path) = &self.file {
            save(
                path,
                &Saved {
                    session_id: self.session.clone(),
                    after: next,
                },
            )?;
        }
        self.after = next;
        Ok(())
    }
}
fn save(path: &Path, value: &Saved) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| Failure::invalid("cursor file needs a filename"))?;
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        cayw_ai::timestamp_ms()
    ));
    let mut created = false;
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        created = true;
        file.write_all(&serde_json::to_vec(value)?)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if created && result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_load_none_file() {
        let cursor = Cursor::load("test-sess", Some(5), None)
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(cursor.after, 5);
        assert_eq!(cursor.session, "test-sess");
    }

    #[test]
    fn cursor_load_missing_file_uses_fallback_or_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing-cursor.json");
        let cursor = Cursor::load("test-sess", Some(3), Some(path))
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(cursor.after, 3);
        assert_eq!(cursor.session, "test-sess");
    }

    #[test]
    fn cursor_load_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cursor.json");
        std::fs::write(&path, b"{\"session_id\":\"test-sess\",\"after\":42}\n").unwrap();
        let cursor = Cursor::load("test-sess", None, Some(path))
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(cursor.after, 42);
        assert_eq!(cursor.session, "test-sess");
    }

    #[test]
    fn cursor_load_rejects_foreign_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cursor.json");
        std::fs::write(&path, b"{\"session_id\":\"other-sess\",\"after\":42}\n").unwrap();
        let err = Cursor::load("test-sess", None, Some(path)).err().unwrap();
        assert_eq!(err.message, "saved cursor belongs to another Session");
    }

    #[test]
    fn cursor_load_rejects_after_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cursor.json");
        std::fs::write(&path, b"{\"session_id\":\"test-sess\",\"after\":42}\n").unwrap();
        let err = Cursor::load("test-sess", Some(10), Some(path))
            .err()
            .unwrap();
        assert_eq!(err.message, "after conflicts with the saved cursor");
    }

    #[test]
    fn cursor_load_rejects_non_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = Cursor::load("test-sess", None, Some(dir.path().to_path_buf()))
            .err()
            .unwrap();
        assert_eq!(err.message, "cursor file must be a regular file");
    }

    #[test]
    fn cursor_load_rejects_oversized_file_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large-cursor.json");
        std::fs::write(&path, vec![b' '; 65 * 1024]).unwrap();
        let err = Cursor::load("test-sess", None, Some(path)).err().unwrap();
        assert_eq!(err.message, "cursor file exceeds 64 KiB");
    }

    #[test]
    fn read_bounded_cursor_file_small_budget_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        std::fs::write(&path, b"0123456789extra").unwrap();
        let err = read_bounded_cursor_file(&path, 10).unwrap_err();
        assert_eq!(err.message, "cursor file exceeds 64 KiB");
    }
}
