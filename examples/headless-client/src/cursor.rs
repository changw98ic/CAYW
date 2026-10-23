use crate::{Failure, Result, emit};
use cayw_core::EventRecord;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

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
            match file.as_ref().map(std::fs::read) {
                Some(Ok(bytes)) => {
                    if bytes.len() > 64 * 1024 {
                        return Err(Failure::invalid("cursor file exceeds 64 KiB"));
                    }
                    Some(serde_json::from_slice::<Saved>(&bytes).map_err(|_| {
                        Failure::invalid("cursor file is not a valid Session cursor")
                    })?)
                }
                Some(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
                Some(Err(error)) => return Err(error.into()),
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
