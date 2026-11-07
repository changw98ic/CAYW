//! Host-listed, immutable Markdown Skill files for the injected Bridge boundary.
//!
//! The host explicitly lists every path. `load` scans each file once to compute
//! the SHA-256 published in the manifest. That host scan is not a model read.
//! `skills.list` uses only manifest metadata and never opens a file. A model
//! `skills.read` performs the bounded on-demand scan counted by
//! `on_demand_reads`.
//!
//! A file is one canonical format: UTF-8 Markdown with a `.md` extension. The
//! body is returned verbatim and is never parsed for permissions or metadata.
//! Symlinks are not followed. The loader resolves only a root-level system alias
//! (for example macOS `/var` -> `/private/var`) and pins every component after
//! it. Every later read rechecks the pinned root, parent directories and file
//! device/inode before checking size, UTF-8 and digest.

use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_bridge::{
    Limits, Manifest, SkillSpec, SourceHandle, SourceOperation, SourceReply, SourceRequest,
    content_sha256,
};
use cayw_codemode::CapabilityError;
use libc::{O_CLOEXEC, O_DIRECTORY, O_NOFOLLOW, O_NONBLOCK, O_RDONLY, openat};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    ffi::{CString, OsStr, OsString},
    fs::{File, Metadata},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::task;
use uuid::Uuid;

const MAX_DESCRIPTION_BYTES: usize = 8192;

/// One explicit host-supplied Skill file. `name` is a logical name, never a path.
#[derive(Debug, Clone)]
pub struct FileSkill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// Cloned read-only observation handle for one loader instance.
#[derive(Clone)]
pub struct FileSkillStats {
    inner: Arc<StatsInner>,
}

/// Serializable point-in-time control observation.
#[derive(Debug, Clone, Serialize)]
pub struct FileSkillSnapshot {
    pub instance_id: String,
    pub load_scans: u64,
    pub on_demand_reads: u64,
    pub rejections: u64,
    pub closed: bool,
}

struct StatsInner {
    instance_id: String,
    load_scans: AtomicU64,
    on_demand_reads: AtomicU64,
    rejections: AtomicU64,
    closed: AtomicBool,
}

impl FileSkillStats {
    pub fn snapshot(&self) -> FileSkillSnapshot {
        FileSkillSnapshot {
            instance_id: self.inner.instance_id.clone(),
            load_scans: self.inner.load_scans.load(Ordering::Acquire),
            on_demand_reads: self.inner.on_demand_reads.load(Ordering::Acquire),
            rejections: self.inner.rejections.load(Ordering::Acquire),
            closed: self.inner.closed.load(Ordering::Acquire),
        }
    }

    fn scan(&self) {
        let _ = self.inner.on_demand_reads.fetch_add(1, Ordering::AcqRel);
    }

    fn reject(&self) {
        let _ = self.inner.rejections.fetch_add(1, Ordering::AcqRel);
    }

    fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
    }

    fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }
}

/// Loaded manifest plus the pinned identities used for on-demand reads.
pub struct FileSkills {
    manifest: Manifest,
    entries: BTreeMap<String, Arc<Entry>>,
    stats: FileSkillStats,
    max_skill_bytes: usize,
}

impl FileSkills {
    /// Scans every explicitly listed file once for its manifest digest. The
    /// whole load fails on the first unsafe, unbounded or non-UTF-8 input.
    pub fn load(entries: Vec<FileSkill>, limits: &Limits) -> Result<Self, CapabilityError> {
        if entries.len() > limits.max_skills_per_source {
            return Err(failure("too many explicit Skill files"));
        }
        let stats = FileSkillStats {
            inner: Arc::new(StatsInner {
                instance_id: Uuid::new_v4().to_string(),
                load_scans: AtomicU64::new(0),
                on_demand_reads: AtomicU64::new(0),
                rejections: AtomicU64::new(0),
                closed: AtomicBool::new(false),
            }),
        };
        let mut specs = Vec::with_capacity(entries.len());
        let mut loaded = BTreeMap::new();
        for skill in entries {
            if !valid_name(&skill.name) || skill.description.len() > MAX_DESCRIPTION_BYTES {
                return Err(failure("invalid Skill name or description"));
            }
            if loaded.contains_key(&skill.name) {
                return Err(failure("duplicate Skill name"));
            }
            let entry = Arc::new(scan_entry(&skill, limits.max_skill_bytes)?);
            let _ = stats.inner.load_scans.fetch_add(1, Ordering::AcqRel);
            specs.push(SkillSpec {
                name: entry.name.clone(),
                description: entry.description.clone(),
                content_sha256: entry.digest.clone(),
            });
            loaded.insert(entry.name.clone(), entry);
        }
        Ok(Self {
            manifest: Manifest {
                tools: Vec::new(),
                skills: specs,
                resources: Vec::new(),
                prompts: Vec::new(),
                search: false,
            },
            entries: loaded,
            stats,
            max_skill_bytes: limits.max_skill_bytes,
        })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn stats(&self) -> FileSkillStats {
        self.stats.clone()
    }

    pub fn into_handle(self) -> Box<dyn SourceHandle> {
        let ownership_key = self.stats.inner.instance_id.clone();
        Box::new(FileHandle {
            entries: self.entries,
            stats: self.stats,
            max_skill_bytes: self.max_skill_bytes,
            ownership_key,
        })
    }
}

struct FileHandle {
    entries: BTreeMap<String, Arc<Entry>>,
    stats: FileSkillStats,
    max_skill_bytes: usize,
    ownership_key: String,
}

#[async_trait]
impl SourceHandle for FileHandle {
    fn ownership_key(&self) -> &str {
        &self.ownership_key
    }

    async fn request(
        &self,
        request: SourceRequest,
        cancel: CancellationToken,
    ) -> Result<SourceReply, CapabilityError> {
        let SourceRequest {
            key,
            operation,
            max_result_bytes,
        } = request;
        let name = match operation {
            SourceOperation::ReadSkill { name } => name,
            _ => {
                self.stats.reject();
                return Err(failure("file Skill source only supports Skill reads"));
            }
        };
        if self.stats.is_closed() {
            self.stats.reject();
            return Err(failure("file Skill source is closed"));
        }
        if cancel.is_cancelled() {
            self.stats.reject();
            return Err(failure("file Skill read cancelled before start"));
        }
        let entry = self.entries.get(&name).cloned().ok_or_else(|| {
            self.stats.reject();
            failure("Skill is unavailable")
        })?;
        let limit = self.max_skill_bytes.min(max_result_bytes);
        if entry.size > limit as u64 {
            self.stats.reject();
            return Err(failure("Skill exceeds the read budget"));
        }

        let stats = self.stats.clone();
        let task_cancel = cancel.clone();
        let task_entry = entry;
        // Normal polling awaits this bounded scan. Dropping the request cannot
        // interrupt an in-progress blocking read; its late result is discarded.
        let result = task::spawn_blocking(move || {
            if task_cancel.is_cancelled() {
                return Err(failure("file Skill read cancelled before scan"));
            }
            stats.scan();
            read_verified(&task_entry, limit)
        })
        .await;

        if self.stats.is_closed() {
            self.stats.reject();
            return Err(failure("file Skill source closed during the read"));
        }
        if cancel.is_cancelled() {
            self.stats.reject();
            return Err(failure("file Skill read cancelled after start"));
        }
        match result {
            Ok(Ok(content)) => {
                if serde_json::to_vec(&content).map_or(true, |bytes| bytes.len() > max_result_bytes)
                {
                    self.stats.reject();
                    return Err(failure("Skill exceeds the result byte budget"));
                }
                Ok(SourceReply {
                    key,
                    value: Value::String(content),
                })
            }
            Ok(Err(error)) => {
                self.stats.reject();
                Err(error)
            }
            Err(_) => {
                self.stats.reject();
                Err(failure("file Skill read task failed"))
            }
        }
    }

    fn disconnect(&self) {
        self.stats.close();
    }
}

struct Entry {
    name: String,
    description: String,
    digest: String,
    root: FileId,
    dirs: Vec<(OsString, FileId)>,
    leaf: OsString,
    file: FileId,
    size: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileId {
    device: u64,
    inode: u64,
}

impl FileId {
    fn of(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

fn scan_entry(skill: &FileSkill, limit: usize) -> Result<Entry, CapabilityError> {
    if skill.path.extension().and_then(OsStr::to_str) != Some("md") {
        return Err(failure("Skill file must have the .md extension"));
    }
    let mut parts = normalize_parts(&skill.path)?;
    let leaf = parts
        .pop()
        .ok_or_else(|| failure("Skill path must name a file"))?;

    let root = open_root()?;
    let root_metadata = root
        .metadata()
        .map_err(|error| io_failure("Skill root", error))?;
    let root_id = FileId::of(&root_metadata);
    let mut current = root;
    let mut dirs = Vec::with_capacity(parts.len());
    for name in parts {
        let dir = open_directory(&current, &name)?;
        let metadata = dir
            .metadata()
            .map_err(|error| io_failure("Skill directory", error))?;
        if !metadata.is_dir() {
            return Err(failure("Skill path component is not a directory"));
        }
        dirs.push((name, FileId::of(&metadata)));
        current = dir;
    }

    let file = open_leaf(&current, &leaf)?;
    let metadata = file
        .metadata()
        .map_err(|error| io_failure("Skill file", error))?;
    if !metadata.file_type().is_file() {
        return Err(failure("Skill path is not a regular file"));
    }
    if metadata.len() > limit as u64 {
        return Err(failure("Skill file exceeds max_skill_bytes"));
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| io_failure("Skill read", error))?;
    if bytes.len() as u64 != metadata.len() {
        return Err(failure("Skill file changed while it was scanned"));
    }
    let content = String::from_utf8(bytes).map_err(|_| failure("Skill file is not valid UTF-8"))?;
    Ok(Entry {
        name: skill.name.clone(),
        description: skill.description.clone(),
        digest: content_sha256(&content),
        root: root_id,
        dirs,
        leaf,
        file: FileId::of(&metadata),
        size: metadata.len(),
    })
}

fn read_verified(entry: &Entry, limit: usize) -> Result<String, CapabilityError> {
    let root = open_root()?;
    let root_metadata = root
        .metadata()
        .map_err(|error| io_failure("Skill root", error))?;
    if !root_metadata.is_dir() || FileId::of(&root_metadata) != entry.root {
        return Err(failure("Skill root changed after load"));
    }
    let mut current = root;
    for (name, expected) in &entry.dirs {
        let dir = open_directory(&current, name.as_os_str())?;
        let metadata = dir
            .metadata()
            .map_err(|error| io_failure("Skill directory", error))?;
        if !metadata.is_dir() || FileId::of(&metadata) != *expected {
            return Err(failure("Skill parent directory changed after load"));
        }
        current = dir;
    }

    let file = open_leaf(&current, entry.leaf.as_os_str())?;
    let metadata = file
        .metadata()
        .map_err(|error| io_failure("Skill file", error))?;
    if !metadata.file_type().is_file()
        || FileId::of(&metadata) != entry.file
        || metadata.len() != entry.size
    {
        return Err(failure("Skill file changed after load"));
    }
    if metadata.len() > limit as u64 {
        return Err(failure("Skill file exceeds the read budget"));
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| io_failure("Skill read", error))?;
    if bytes.len() as u64 != entry.size {
        return Err(failure("Skill file size changed while it was read"));
    }
    let content = String::from_utf8(bytes).map_err(|_| failure("Skill file is not valid UTF-8"))?;
    if content_sha256(&content) != entry.digest {
        return Err(failure("Skill file content changed after load"));
    }
    Ok(content)
}

/// Resolves only a root-level system alias. Any symlink deeper in the explicit
/// path is left in place so the per-component `openat` rejects it.
fn normalize_parts(path: &Path) -> Result<Vec<OsString>, CapabilityError> {
    if !path.is_absolute() {
        return Err(failure("Skill path must be absolute"));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(part) => parts.push(part.to_os_string()),
            _ => return Err(failure("Skill path must not contain . or .. components")),
        }
    }
    if parts.is_empty() {
        return Err(failure("Skill path must name a file"));
    }
    if parts.len() >= 2 {
        let first = Path::new("/").join(parts[0].as_os_str());
        let is_symlink = std::fs::symlink_metadata(&first)
            .is_ok_and(|metadata| metadata.file_type().is_symlink());
        if is_symlink {
            let base = std::fs::canonicalize(&first)
                .map_err(|error| io_failure("Skill path base", error))?;
            let mut resolved = Vec::new();
            for component in base.components() {
                if let Component::Normal(part) = component {
                    resolved.push(part.to_os_string());
                }
            }
            resolved.extend(parts.into_iter().skip(1));
            return Ok(resolved);
        }
    }
    Ok(parts)
}

fn open_root() -> Result<File, CapabilityError> {
    File::open("/").map_err(|error| io_failure("Skill root", error))
}

fn open_directory(parent: &File, name: &OsStr) -> Result<File, CapabilityError> {
    open_at(
        parent,
        name,
        O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC,
    )
    .map_err(|error| io_failure("Skill directory", error))
}

fn open_leaf(parent: &File, name: &OsStr) -> Result<File, CapabilityError> {
    open_at(parent, name, O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK)
        .map_err(|error| io_failure("Skill file", error))
}

fn open_at(parent: &File, name: &OsStr, flags: libc::c_int) -> io::Result<File> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    let descriptor = unsafe { openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn failure(message: &str) -> CapabilityError {
    CapabilityError::new(message)
}

fn io_failure(label: &str, error: io::Error) -> CapabilityError {
    CapabilityError::new(format!("{label}: {:?}", error.kind()))
}
