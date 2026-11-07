use crate::{ContentHash, Error, ExpectedRevision, Id, Limits, Record, Result, SnapshotOrigin};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

const STATE_LIMIT: u64 = 32 * 1024 * 1024;

#[cfg(feature = "fault-injection")]
thread_local! {
    static STATE_SYNC_FAILURE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Fault tests may fail only the current thread's state-directory sync, from
/// inside its `content_state_renamed` checkpoint. No later write is affected.
#[cfg(feature = "fault-injection")]
pub fn fail_current_state_directory_sync() -> bool {
    STATE_SYNC_FAILURE.with(|failure| {
        if failure.get().is_none() {
            return false;
        }
        failure.set(Some(true));
        true
    })
}

fn state_directory_sync(root: &Dir, changed: Option<&Record>) -> Result<()> {
    #[cfg(feature = "fault-injection")]
    {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                STATE_SYNC_FAILURE.with(|failure| failure.set(None));
            }
        }
        let _reset = Reset;
        STATE_SYNC_FAILURE.with(|failure| failure.set(Some(false)));
        content_checkpoint("content_state_renamed", changed);
        if STATE_SYNC_FAILURE.with(|failure| failure.get() == Some(true)) {
            return Err(Error::Io(std::io::Error::other(
                "injected state directory sync failure after replacement",
            )));
        }
    }
    #[cfg(not(feature = "fault-injection"))]
    content_checkpoint("content_state_renamed", changed);
    sync_dir(root)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum KeyState {
    Active,
    Revoked,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct State {
    pub protocol: u32,
    pub device_id: Id,
    pub limits: Limits,
    pub records: BTreeMap<Id, Record>,
    pub keys: BTreeMap<Id, KeyState>,
    /// Key id + nonce is hashed; no secret key material is persisted.
    pub exported: BTreeMap<String, ContentHash>,
    pub imported: BTreeSet<ContentHash>,
}
impl State {
    #[cfg(feature = "encryption")]
    pub(crate) fn security_capacity(&self) -> Result<()> {
        if self.keys.len() + self.exported.len() + self.imported.len()
            >= self.limits.max_security_entries as usize
        {
            return Err(Error::QuotaExceeded);
        }
        Ok(())
    }
    pub(crate) fn check_expected(&self, name: &Id, expected: ExpectedRevision) -> Result<()> {
        let actual = self
            .records
            .get(name)
            .map_or(ExpectedRevision::Absent, Record::expected);
        if expected != actual {
            return Err(Error::Conflict { expected, actual });
        }
        Ok(())
    }
}

struct TransactionLock {
    file: File,
}

impl Drop for TransactionLock {
    fn drop(&mut self) {
        // A concurrent fork can inherit this open file description until exec.
        // Closing only our descriptor would leave the completed transaction's
        // lock held by that child. Each guard unlocks only its own acquired lock.
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

pub(crate) struct LockedDirs<'a> {
    pub root: &'a Dir,
    pub objects: Dir,
    _lock: TransactionLock,
}

/// All cooperating processes serialize reads and writes through a separate lock handle.
/// The caller owns and secures this private directory; hostile same-user writers are
/// outside the lock's advisory protection. Key bytes are never stored here.
pub struct ContentStore {
    root: Dir,
    device_id: Id,
    limits: Limits,
}

impl ContentStore {
    /// Opens or creates a store at an explicit absolute directory. Each path component
    /// is opened without following symlinks, and all subsequent IO uses a directory capability.
    pub fn open(path: impl AsRef<Path>, device_id: Id, limits: Limits) -> Result<Self> {
        limits.validate()?;
        let root = open_root(path.as_ref())?;
        Self {
            root,
            device_id,
            limits,
        }
        .initialize()
    }

    /// Checks the held root's mode and optional original device/inode before
    /// creating any store entries. A bound root must already exist.
    #[cfg(unix)]
    pub fn open_private(
        path: impl AsRef<Path>,
        device_id: Id,
        limits: Limits,
        expected_root: Option<(u64, u64)>,
    ) -> Result<Self> {
        limits.validate()?;
        let root = open_root_inner(path.as_ref(), expected_root.is_none())?;
        let store = Self {
            root,
            device_id,
            limits,
        };
        let actual = store.root_metadata()?;
        if actual.mode & 0o022 != 0 {
            return Err(Error::UnsafePermissions);
        }
        if expected_root.is_some_and(|expected| expected != (actual.device, actual.inode)) {
            return Err(Error::RootIdentityMismatch);
        }
        store.initialize()
    }

    /// Opens one already-bound private store and adopts a trusted quota policy.
    /// The device and original root identity are still checked; records, keys
    /// and objects are preserved. The new limits are accepted only when every
    /// retained record and object still fits them.
    #[cfg(unix)]
    pub fn open_private_amending_limits(
        path: impl AsRef<Path>,
        device_id: Id,
        limits: Limits,
        expected_root: (u64, u64),
    ) -> Result<Self> {
        limits.validate()?;
        let root = open_root_inner(path.as_ref(), false)?;
        let store = Self {
            root,
            device_id,
            limits,
        };
        let actual = store.root_metadata()?;
        if actual.mode & 0o022 != 0 {
            return Err(Error::UnsafePermissions);
        }
        if expected_root != (actual.device, actual.inode) {
            return Err(Error::RootIdentityMismatch);
        }
        store.initialize_with(true)
    }

    fn initialize(self) -> Result<Self> {
        self.initialize_with(false)
    }

    fn initialize_with(self, amend_limits: bool) -> Result<Self> {
        let store = self;
        let limits = store.limits;
        let mut lock = store.lock()?;
        let file = &mut lock.file;
        match store.root.symlink_metadata("objects") {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
            Ok(_) => return Err(Error::UnsafePath),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                create_private_dir(&store.root, std::ffi::OsStr::new("objects"))?;
                sync_dir(&store.root)?;
            }
            Err(e) => return Err(e.into()),
        }
        let mut initialized = Vec::new();
        Read::by_ref(file).take(64).read_to_end(&mut initialized)?;
        if !initialized.is_empty() && initialized != b"cayw-content-initialized-v1\n" {
            return Err(Error::Integrity);
        }
        match read_file(&store.root, "state.json", STATE_LIMIT) {
            Ok(bytes) => {
                let mut state = store.parse_state_with(&bytes, amend_limits)?;
                if amend_limits && state.limits != limits {
                    store.check_object_total()?;
                    state.limits = limits;
                    store.save_state(&state)?;
                }
            }
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                let objects = store.root.open_dir_nofollow("objects")?;
                if !initialized.is_empty() || objects.entries()?.next().is_some() {
                    return Err(Error::Integrity);
                }
                let state = State {
                    protocol: 1,
                    device_id: store.device_id.clone(),
                    limits,
                    records: BTreeMap::new(),
                    keys: BTreeMap::new(),
                    exported: BTreeMap::new(),
                    imported: BTreeSet::new(),
                };
                store.save_state(&state)?;
            }
            Err(e) => return Err(e),
        }
        if initialized.is_empty() {
            file.write_all(b"cayw-content-initialized-v1\n")?;
            file.sync_all()?;
            sync_dir(&store.root)?;
        }
        drop(lock);
        Ok(store)
    }

    pub fn read(&self, name: &Id) -> Result<Option<(Record, Vec<u8>)>> {
        self.read_inner(name, false)
    }

    /// Returns `Busy` instead of waiting for the current transaction's lock.
    /// Once acquired, the same bounded synchronous read and validation run to completion.
    pub fn try_read(&self, name: &Id) -> Result<Option<(Record, Vec<u8>)>> {
        self.read_inner(name, true)
    }

    fn read_inner(&self, name: &Id, nonblocking: bool) -> Result<Option<(Record, Vec<u8>)>> {
        self.transaction_inner(nonblocking, |state, dirs| {
            let Some(record) = state.records.get(name) else {
                return Ok(None);
            };
            let bytes = self.read_content(dirs, record)?;
            Ok(Some((record.clone(), bytes)))
        })
    }

    /// Compares the full observed revision and increments the local version once.
    /// No implicit conversion or last-writer-wins update is performed.
    pub fn put(&self, name: &Id, expected: ExpectedRevision, bytes: &[u8]) -> Result<Record> {
        self.put_inner(name, expected, bytes, false)
    }

    /// Returns `Busy` before changing content when another transaction holds the lock.
    /// The host may wait with its own cancellation token and retry the same request.
    pub fn try_put(&self, name: &Id, expected: ExpectedRevision, bytes: &[u8]) -> Result<Record> {
        self.put_inner(name, expected, bytes, true)
    }

    fn put_inner(
        &self,
        name: &Id,
        expected: ExpectedRevision,
        bytes: &[u8],
        nonblocking: bool,
    ) -> Result<Record> {
        self.check_length(bytes)?;
        self.transaction_inner(nonblocking, |state, dirs| {
            state.check_expected(name, expected)?;
            self.put_locked(state, dirs, name, bytes, None)
        })
    }

    /// Confirm exact inspected records (including absence) under the transaction
    /// lock and sync the current authority directory. This never puts content or
    /// changes a revision; the host first inspects the actual stored state.
    pub fn confirm_observed(&self, expected: &[(Id, Option<Record>)]) -> Result<()> {
        self.transaction_inner(true, |state, dirs| {
            for (name, expected) in expected {
                if state.records.get(name) != expected.as_ref() {
                    return Err(Error::Conflict {
                        expected: expected
                            .as_ref()
                            .map_or(ExpectedRevision::Absent, Record::expected),
                        actual: state
                            .records
                            .get(name)
                            .map_or(ExpectedRevision::Absent, Record::expected),
                    });
                }
                if let Some(record) = expected {
                    self.read_content(dirs, record)?;
                }
            }
            sync_dir(dirs.root)
        })
    }

    /// Reads the held directory's identity and current mode without resolving its path.
    #[cfg(unix)]
    pub fn root_metadata(&self) -> Result<crate::RootMetadata> {
        use cap_std::fs::{MetadataExt, PermissionsExt};
        let metadata = self.root.dir_metadata()?;
        Ok(crate::RootMetadata {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.permissions().mode(),
        })
    }

    /// Checks the persisted device and quotas through the held root without
    /// changing policy or resolving the configured directory again.
    pub fn validate_configuration(&self) -> Result<()> {
        self.transaction_inner(false, |_, _| Ok(()))
    }

    pub fn device_id(&self) -> &Id {
        &self.device_id
    }
    pub fn limits(&self) -> Limits {
        self.limits
    }

    pub(crate) fn check_length(&self, bytes: &[u8]) -> Result<()> {
        if bytes.len() as u64 > self.limits.max_content_bytes {
            return Err(Error::ContentTooLarge);
        }
        Ok(())
    }
    #[cfg(feature = "encryption")]
    pub(crate) fn transaction<T>(
        &self,
        f: impl FnOnce(&mut State, &LockedDirs<'_>) -> Result<T>,
    ) -> Result<T> {
        self.transaction_inner(false, f)
    }
    fn transaction_inner<T>(
        &self,
        nonblocking: bool,
        f: impl FnOnce(&mut State, &LockedDirs<'_>) -> Result<T>,
    ) -> Result<T> {
        let lock = self.lock_inner(nonblocking)?;
        let objects = self.root.open_dir_nofollow("objects")?;
        let dirs = LockedDirs {
            root: &self.root,
            objects,
            _lock: lock,
        };
        let mut state = self.parse_state(&read_file(&self.root, "state.json", STATE_LIMIT)?)?;
        f(&mut state, &dirs)
    }
    fn lock(&self) -> Result<TransactionLock> {
        self.lock_inner(false)
    }
    fn lock_inner(&self, nonblocking: bool) -> Result<TransactionLock> {
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).follow(FollowSymlinks::No);
        private_file_mode(&mut opts);
        let mut retries = 0;
        let lock = loop {
            // Exclusive creation avoids the create-if-missing path's internal
            // open/create race. Every fallback still refuses symlinks.
            opts.create_new(true);
            let opened = match self.root.open_with(".lock", &opts) {
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    #[cfg(all(test, unix))]
                    tests::before_path_retry(&self.root, std::ffi::OsStr::new(".lock"));
                    opts.create_new(false);
                    self.root.open_with(".lock", &opts)
                }
                other => other,
            };
            match opened {
                // A name can disappear between creation and reopening. Bound
                // retries so a writer removing it cannot keep open() spinning.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && retries < 32 => {
                    retries += 1;
                }
                other => break other?.into_std(),
            }
        };
        if !lock.metadata()?.is_file() {
            return Err(Error::UnsafePath);
        }
        if nonblocking {
            match fs2::FileExt::try_lock_exclusive(&lock) {
                Ok(()) => {}
                Err(error)
                    if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
                {
                    return Err(Error::Busy);
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            fs2::FileExt::lock_exclusive(&lock)?;
        }
        Ok(TransactionLock { file: lock })
    }
    fn parse_state(&self, bytes: &[u8]) -> Result<State> {
        self.parse_state_with(bytes, false)
    }
    /// `amend_limits` accepts a persisted policy that differs from the opener's
    /// while still validating every retained record against the new limits.
    fn parse_state_with(&self, bytes: &[u8], amend_limits: bool) -> Result<State> {
        let state: State = serde_json::from_slice(bytes)?;
        if state.protocol != 1 {
            return Err(Error::UnsupportedVersion);
        }
        if state.device_id != self.device_id {
            return Err(Error::StoreConfigurationMismatch);
        }
        let changed = state.limits != self.limits;
        if changed && !amend_limits {
            return Err(Error::StoreConfigurationMismatch);
        }
        let over_limit = |changed: bool| {
            if changed {
                Error::QuotaExceeded
            } else {
                Error::Integrity
            }
        };
        if state.records.len() > self.limits.max_records as usize
            || state.keys.len() + state.exported.len() + state.imported.len()
                > self.limits.max_security_entries as usize
        {
            return Err(over_limit(changed));
        }
        for nonce_id in state.exported.keys() {
            ContentHash::parse_filename(nonce_id)?;
        }
        for (name, record) in &state.records {
            record.validate()?;
            if name != &record.name
                || record.device_id != state.device_id
                || record.content_len > self.limits.max_content_bytes
            {
                return Err(over_limit(changed));
            }
        }
        Ok(state)
    }
    /// Charges all retained objects, including orphans from interrupted writes,
    /// before a lowered total-object quota becomes authoritative.
    fn check_object_total(&self) -> Result<()> {
        let objects = self.root.open_dir_nofollow("objects")?;
        let mut total = 0u64;
        for entry in objects.entries()? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| Error::Integrity)?;
            ContentHash::parse_filename(&name)?;
            let meta = objects.symlink_metadata(&name)?;
            if !meta.is_file() || meta.file_type().is_symlink() {
                return Err(Error::UnsafePath);
            }
            total = total.checked_add(meta.len()).ok_or(Error::QuotaExceeded)?;
        }
        if total > self.limits.max_total_bytes {
            return Err(Error::QuotaExceeded);
        }
        Ok(())
    }
    pub(crate) fn save_state(&self, state: &State) -> Result<()> {
        self.save_state_inner(state, None)
    }
    fn save_state_inner(&self, state: &State, changed: Option<&Record>) -> Result<()> {
        let bytes = serde_json::to_vec(state)?;
        if bytes.len() as u64 > STATE_LIMIT {
            return Err(Error::QuotaExceeded);
        }
        write_stage(&self.root, ".state-stage", &bytes)?;
        content_checkpoint("content_state_staged", changed);
        self.root.rename(".state-stage", &self.root, "state.json")?;
        state_directory_sync(&self.root, changed).map_err(|e| match e {
            Error::Io(io) => Error::CommitOutcomeUnknown(io),
            other => other,
        })?;
        content_checkpoint("content_state_synced", changed);
        Ok(())
    }
    pub(crate) fn read_content(&self, dirs: &LockedDirs<'_>, record: &Record) -> Result<Vec<u8>> {
        let bytes = read_file(
            &dirs.objects,
            &record.content_hash.to_string(),
            self.limits.max_content_bytes,
        )?;
        if bytes.len() as u64 != record.content_len
            || ContentHash::of(&bytes) != record.content_hash
        {
            return Err(Error::Integrity);
        }
        Ok(bytes)
    }
    pub(crate) fn put_locked(
        &self,
        state: &mut State,
        dirs: &LockedDirs<'_>,
        name: &Id,
        bytes: &[u8],
        origin: Option<SnapshotOrigin>,
    ) -> Result<Record> {
        let previous = state.records.get(name);
        if previous.is_none() && state.records.len() >= self.limits.max_records as usize {
            return Err(Error::QuotaExceeded);
        }
        let mut record = Record {
            name: name.clone(),
            version: match previous {
                Some(r) => r.version.checked_add(1).ok_or(Error::QuotaExceeded)?,
                None => 1,
            },
            content_hash: ContentHash::of(bytes),
            content_len: bytes.len() as u64,
            device_id: self.device_id.clone(),
            lineage: match previous {
                Some(r) => r.lineage,
                None => ContentHash::random()?,
            },
            parent_revision: previous.map(|r| r.revision),
            restored_from: origin,
            revision: ContentHash::of(b""),
        };
        record.revision = record.calculate_revision()?;
        self.ensure_object(dirs, &record.content_hash, bytes)?;
        state.records.insert(name.clone(), record.clone());
        self.save_state_inner(state, Some(&record))?;
        Ok(record)
    }
    fn ensure_object(&self, dirs: &LockedDirs<'_>, hash: &ContentHash, bytes: &[u8]) -> Result<()> {
        let filename = hash.to_string();
        let mut total = 0u64;
        let mut exists = false;
        for entry in dirs.objects.entries()? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| Error::Integrity)?;
            ContentHash::parse_filename(&name)?;
            let meta = dirs.objects.symlink_metadata(&name)?;
            if !meta.is_file() || meta.file_type().is_symlink() {
                return Err(Error::UnsafePath);
            }
            total = total.checked_add(meta.len()).ok_or(Error::QuotaExceeded)?;
            exists |= name == filename;
        }
        if total > self.limits.max_total_bytes {
            return Err(Error::QuotaExceeded);
        }
        if exists {
            let mut opts = OpenOptions::new();
            // Windows also requires write access to flush an existing file.
            // Neither creation nor truncation is enabled for this handle.
            opts.read(true).write(true).follow(FollowSymlinks::No);
            let mut file = dirs.objects.open_with(&filename, &opts)?.into_std();
            let existing = read_open_file(&mut file, self.limits.max_content_bytes)?;
            if existing != bytes || ContentHash::of(&existing) != *hash {
                return Err(Error::Integrity);
            }
            // An orphan from an interrupted or failed write has no durability
            // receipt. Validate and sync the same handle before committing it.
            sync_file(&file)?;
        } else {
            if total
                .checked_add(bytes.len() as u64)
                .is_none_or(|n| n > self.limits.max_total_bytes)
            {
                return Err(Error::QuotaExceeded);
            }
            write_stage(dirs.root, ".object-stage", bytes)?;
            dirs.root
                .rename(".object-stage", &dirs.objects, &filename)?;
        }
        // A previous rename may have failed to sync either directory. Reused
        // objects need these barriers too, before a reference is authoritative.
        sync_dir(&dirs.objects)?;
        sync_dir(dirs.root)?;
        Ok(())
    }
}

fn content_checkpoint(point: &'static str, record: Option<&Record>) {
    #[cfg(feature = "fault-injection")]
    if let Some(record) = record {
        cayw_ai::faults::hit(
            point,
            serde_json::json!({
                "name": record.name,
                "version": record.version,
                "revision": record.revision.to_string(),
                "content_hash": record.content_hash.to_string(),
            }),
        );
    }
    #[cfg(not(feature = "fault-injection"))]
    let _ = (point, record);
}

pub(crate) fn read_file(dir: &Dir, name: &str, limit: u64) -> Result<Vec<u8>> {
    let mut opts = OpenOptions::new();
    opts.read(true).follow(FollowSymlinks::No);
    let mut file = dir.open_with(name, &opts)?.into_std();
    read_open_file(&mut file, limit)
}
fn read_open_file(file: &mut File, limit: u64) -> Result<Vec<u8>> {
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(Error::UnsafePath);
    }
    if meta.len() > limit {
        return Err(Error::ContentTooLarge);
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::ContentTooLarge);
    }
    Ok(bytes)
}
fn write_stage(dir: &Dir, name: &str, bytes: &[u8]) -> Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true)
        .create(true)
        .truncate(true)
        .follow(FollowSymlinks::No);
    private_file_mode(&mut opts);
    let mut file = dir.open_with(name, &opts)?.into_std();
    if !file.metadata()?.is_file() {
        return Err(Error::UnsafePath);
    }
    file.write_all(bytes)?;
    sync_file(&file)?;
    Ok(())
}
fn private_file_mode(opts: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = opts;
}
fn create_private_dir(dir: &Dir, name: &std::ffi::OsStr) -> Result<()> {
    let mut builder = cap_std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use cap_std::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    dir.create_dir_with(name, &builder)?;
    Ok(())
}
fn sync_file(file: &File) -> Result<()> {
    #[cfg(all(test, unix))]
    tests::before_sync(file)?;
    file.sync_all()?;
    Ok(())
}
fn sync_dir(dir: &Dir) -> Result<()> {
    sync_file(&dir.try_clone()?.into_std_file())
}
fn open_root(path: &Path) -> Result<Dir> {
    open_root_inner(path, true)
}
fn open_root_inner(path: &Path, create_missing: bool) -> Result<Dir> {
    if !path.is_absolute() {
        return Err(Error::UnsafePath);
    }
    let mut anchor = PathBuf::new();
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) if names.is_empty() => anchor.push(prefix.as_os_str()),
            Component::RootDir if names.is_empty() => anchor.push(component.as_os_str()),
            Component::Normal(name) => names.push(name),
            _ => return Err(Error::UnsafePath),
        }
    }
    let mut dir = Dir::open_ambient_dir(anchor, cap_std::ambient_authority())?;
    for name in names {
        let next = match dir.open_dir_nofollow(name) {
            Ok(next) => next,
            Err(e) if create_missing && e.kind() == std::io::ErrorKind::NotFound => {
                #[cfg(all(test, unix))]
                tests::before_path_retry(&dir, name);
                match create_private_dir(&dir, name) {
                    Ok(()) => {}
                    Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                }
                // Another opener may have created this component after our
                // lookup. Reopen it without following links before using it.
                dir.open_dir_nofollow(name)?
            }
            Err(e) => return Err(e.into()),
        };
        // An existing entry may come from a failed or interrupted parent sync,
        // including another opener's creation. Persist it before descending so
        // no store operation can bypass a root or ancestor durability barrier.
        sync_dir(&dir)?;
        dir = next;
    }
    Ok(dir)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{cell::RefCell, os::unix::fs::MetadataExt};
    use tempfile::TempDir;

    type PathRetry = Box<dyn FnOnce(&Dir, &std::ffi::OsStr)>;
    thread_local! {
        static FAILED_SYNC: RefCell<Option<(u64, u64, usize)>> = const { RefCell::new(None) };
        static PATH_RETRY: RefCell<Option<PathRetry>> = const { RefCell::new(None) };
    }

    pub(super) fn before_path_retry(dir: &Dir, name: &std::ffi::OsStr) {
        let action = PATH_RETRY.with_borrow_mut(Option::take);
        if let Some(action) = action {
            action(dir, name);
        }
    }

    struct PathRace;
    impl PathRace {
        fn on_retry(action: impl FnOnce(&Dir, &std::ffi::OsStr) + 'static) -> Self {
            PATH_RETRY.with_borrow_mut(|target| {
                assert!(target.is_none());
                *target = Some(Box::new(action));
            });
            Self
        }
    }
    impl Drop for PathRace {
        fn drop(&mut self) {
            PATH_RETRY.with_borrow_mut(|target| *target = None);
        }
    }

    // Only the selected OS sync fails. Writes, rename, locking, reads and all
    // other syncs still use real files; each test thread has its own failure.
    pub(super) fn before_sync(file: &File) -> std::io::Result<()> {
        let meta = file.metadata()?;
        FAILED_SYNC.with_borrow_mut(|target| {
            if let Some((device, inode, remaining)) = target.as_mut()
                && *device == meta.dev()
                && *inode == meta.ino()
            {
                if *remaining == 0 {
                    *target = None;
                    return Err(std::io::Error::from_raw_os_error(5)); // EIO on Unix.
                }
                *remaining -= 1;
            }
            Ok(())
        })
    }

    struct FailedSync;
    impl FailedSync {
        fn at(path: &Path) -> Self {
            Self::after(path, 0)
        }
        fn after(path: &Path, successful_matches: usize) -> Self {
            let meta = std::fs::metadata(path).unwrap();
            FAILED_SYNC.with_borrow_mut(|target| {
                assert!(target.is_none());
                *target = Some((meta.dev(), meta.ino(), successful_matches));
            });
            Self
        }
    }
    impl Drop for FailedSync {
        fn drop(&mut self) {
            FAILED_SYNC.with_borrow_mut(|target| *target = None);
        }
    }

    fn id(value: &str) -> Id {
        Id::new(value).unwrap()
    }

    fn retry_after_object_sync_failure(retry_barrier: &str) {
        let root = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let db = ContentStore::open(root.path(), id("device"), Limits::default()).unwrap();
        let state = std::fs::read(root.path().join("state.json")).unwrap();
        let bytes = b"renamed but not yet committed";
        let object = root
            .path()
            .join("objects")
            .join(ContentHash::of(bytes).to_string());
        let first_barrier = if retry_barrier == "root" {
            root.path().to_path_buf()
        } else {
            root.path().join("objects")
        };
        let failure = FailedSync::at(&first_barrier);
        assert!(matches!(
            db.put(&id("record"), ExpectedRevision::Absent, bytes),
            Err(Error::Io(e)) if e.raw_os_error() == Some(5)
        ));
        drop(failure);
        assert_eq!(std::fs::read(&object).unwrap(), bytes);
        assert!(!root.path().join(".object-stage").exists());
        assert_eq!(
            std::fs::read(root.path().join("state.json")).unwrap(),
            state
        );
        assert!(db.read(&id("record")).unwrap().is_none());
        drop(db);

        // Reopen after the failed transaction: the orphan must not make a
        // later write skip any barrier, including syncing the reused file.
        let db = ContentStore::open(root.path(), id("device"), Limits::default()).unwrap();
        let retry_target = match retry_barrier {
            "file" => object,
            "objects" => root.path().join("objects"),
            "root" => root.path().to_path_buf(),
            _ => unreachable!(),
        };
        let failure = FailedSync::at(&retry_target);
        let result = db.put(&id("record"), ExpectedRevision::Absent, bytes);
        assert!(
            matches!(result, Err(Error::Io(ref e)) if e.raw_os_error() == Some(5)),
            "reused object must fail before authority changes when {retry_barrier} sync fails: {result:?}"
        );
        drop(failure);
        assert_eq!(
            std::fs::read(root.path().join("state.json")).unwrap(),
            state
        );
        assert!(db.read(&id("record")).unwrap().is_none());
        let record = db
            .put(&id("record"), ExpectedRevision::Absent, bytes)
            .unwrap();
        assert_eq!(record.version, 1);
        drop(db);
        let db = ContentStore::open(root.path(), id("device"), Limits::default()).unwrap();
        assert_eq!(
            db.read(&id("record")).unwrap().unwrap(),
            (record, bytes.to_vec())
        );
    }

    fn retry_after_root_path_sync_failure(suffix: &str, created_component: &str) {
        let outer = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let root = outer.path().join(suffix);
        let created = outer.path().join(created_component);

        // The first open creates this directory but fails to persist its entry
        // in the parent. Later opens see it, but that is not a sync receipt.
        for attempt in 1..=3 {
            let failure = FailedSync::at(outer.path());
            let result = ContentStore::open(&root, id("device"), Limits::default());
            assert!(
                matches!(&result, Err(Error::Io(e)) if e.raw_os_error() == Some(5)),
                "attempt {attempt} must retry the failed parent barrier: {:?}",
                result.as_ref().err()
            );
            drop(failure);
            assert!(created.is_dir());
            assert!(std::fs::read_dir(&created).unwrap().next().is_none());
            assert!(!root.join("state.json").exists());
        }

        let name = id("note");
        let db = ContentStore::open(&root, id("device"), Limits::default()).unwrap();
        let first = db
            .put(&name, ExpectedRevision::Absent, b"first durable version")
            .unwrap();
        assert_eq!(first.version, 1);
        let state = std::fs::read(root.join("state.json")).unwrap();
        let object = root.join("objects").join(first.content_hash.to_string());
        drop(db);

        // Failure must also gate reopening an initialized store, leaving its
        // exact authority bytes and its existing record/version untouched.
        for attempt in 1..=3 {
            let failure = FailedSync::at(outer.path());
            let result = ContentStore::open(&root, id("device"), Limits::default());
            assert!(
                matches!(&result, Err(Error::Io(e)) if e.raw_os_error() == Some(5)),
                "initialized attempt {attempt} must fail before using the store: {:?}",
                result.as_ref().err()
            );
            drop(failure);
            assert_eq!(std::fs::read(root.join("state.json")).unwrap(), state);
            assert_eq!(std::fs::read(&object).unwrap(), b"first durable version");
        }

        let db = ContentStore::open(&root, id("device"), Limits::default()).unwrap();
        assert_eq!(
            db.read(&name).unwrap().unwrap(),
            (first.clone(), b"first durable version".to_vec())
        );
        let second = db
            .put(&name, first.expected(), b"next durable version")
            .unwrap();
        assert_eq!(second.version, 2);
        drop(db);
        let db = ContentStore::open(&root, id("device"), Limits::default()).unwrap();
        assert_eq!(
            db.read(&name).unwrap().unwrap(),
            (second, b"next durable version".to_vec())
        );
    }

    #[test]
    fn post_rename_sync_failure_is_unknown_and_exact_record_survives_reopen() {
        let root = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let db = ContentStore::open(root.path(), id("device"), Limits::default()).unwrap();
        let first = db
            .put(&id("note"), ExpectedRevision::Absent, b"first")
            .unwrap();
        let old_authority = std::fs::read(root.path().join("state.json")).unwrap();
        // Object installation syncs the root once. Fail the later sync after
        // state.json has been replaced, with all other real filesystem IO intact.
        let failure = FailedSync::after(root.path(), 1);
        assert!(matches!(
            db.try_put(&id("note"), first.expected(), b"second"),
            Err(Error::CommitOutcomeUnknown(error)) if error.raw_os_error() == Some(5)
        ));
        drop(failure);
        assert_ne!(
            std::fs::read(root.path().join("state.json")).unwrap(),
            old_authority
        );
        let (second, bytes) = db.try_read(&id("note")).unwrap().unwrap();
        assert_eq!(second.version, 2);
        assert_eq!(second.parent_revision, Some(first.revision));
        assert_eq!(bytes, b"second");
        assert!(matches!(
            db.try_put(&id("note"), first.expected(), b"second"),
            Err(Error::Conflict { actual, .. }) if actual == second.expected()
        ));
        drop(db);
        let db = ContentStore::open(root.path(), id("device"), Limits::default()).unwrap();
        assert_eq!(
            db.try_read(&id("note")).unwrap().unwrap(),
            (second, b"second".to_vec())
        );
    }

    #[test]
    fn retry_after_failed_store_parent_sync_cannot_bypass_it() {
        retry_after_root_path_sync_failure("store", "store");
    }

    #[test]
    fn retry_after_failed_ancestor_parent_sync_cannot_bypass_it() {
        retry_after_root_path_sync_failure("fresh/nested/store", "fresh");
    }

    #[test]
    fn retry_after_failed_object_sync_resyncs_the_reused_file() {
        retry_after_object_sync_failure("file");
    }

    #[test]
    fn retry_after_failed_object_directory_sync_cannot_bypass_it() {
        retry_after_object_sync_failure("objects");
    }

    #[test]
    fn retry_after_failed_root_directory_sync_cannot_commit_state_first() {
        retry_after_object_sync_failure("root");
    }

    #[test]
    fn root_create_race_reopens_existing_directories_without_following_links() {
        use std::os::unix::fs::symlink;
        let outer = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let external = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let sentinel = external.path().join("sentinel");
        std::fs::write(&sentinel, b"outside remains unchanged").unwrap();
        let in_scope = outer.path().join("in-scope-target");
        std::fs::create_dir(&in_scope).unwrap();
        for target in [
            external.path().to_path_buf(),
            PathBuf::from("in-scope-target"),
        ] {
            for suffix in ["raced", "raced/nested/store"] {
                let root = outer.path().join(suffix);
                let raced_component = outer.path().join("raced");
                let target = target.clone();
                let link = raced_component.clone();
                let race = PathRace::on_retry(move |_, name| {
                    assert_eq!(name, "raced");
                    symlink(target, link).unwrap();
                });
                assert!(ContentStore::open(&root, id("device"), Limits::default()).is_err());
                drop(race);
                assert_eq!(
                    std::fs::read(&sentinel).unwrap(),
                    b"outside remains unchanged"
                );
                assert_eq!(std::fs::read_dir(external.path()).unwrap().count(), 1);
                assert_eq!(std::fs::read_dir(&in_scope).unwrap().count(), 0);
                std::fs::remove_file(raced_component).unwrap();
            }
        }

        // The same AlreadyExists race with a real directory must succeed.
        let root = outer.path().join("raced/nested/store");
        let winner = outer.path().join("raced");
        let race = PathRace::on_retry(move |_, _| std::fs::create_dir(winner).unwrap());
        let db = ContentStore::open(&root, id("device"), Limits::default()).unwrap();
        drop(race);
        db.put(&id("note"), ExpectedRevision::Absent, b"local")
            .unwrap();
        assert_eq!(db.read(&id("note")).unwrap().unwrap().1, b"local");
    }

    #[test]
    fn lock_reopen_race_rejects_a_replacement_symlink() {
        use std::os::unix::fs::symlink;
        let root = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let external = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let sentinel = external.path().join("sentinel");
        std::fs::write(&sentinel, b"outside lock target").unwrap();
        let in_scope = root.path().join("in-scope-lock");
        std::fs::write(&in_scope, b"").unwrap();
        for target in [sentinel.clone(), PathBuf::from("in-scope-lock")] {
            let lock_path = root.path().join(".lock");
            std::fs::write(&lock_path, b"").unwrap();
            let race = PathRace::on_retry(move |dir, name| {
                assert_eq!(name, ".lock");
                dir.remove_file(name).unwrap();
                symlink(target, lock_path).unwrap();
            });
            assert!(ContentStore::open(root.path(), id("device"), Limits::default()).is_err());
            drop(race);
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"outside lock target");
            assert_eq!(std::fs::read(&in_scope).unwrap(), b"");
            assert!(!root.path().join("state.json").exists());
            std::fs::remove_file(root.path().join(".lock")).unwrap();
        }
    }

    #[test]
    fn lock_reopen_race_retries_a_disappearing_name() {
        let root = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        std::fs::write(root.path().join(".lock"), b"").unwrap();
        let race = PathRace::on_retry(|dir, name| dir.remove_file(name).unwrap());
        let db = ContentStore::open(root.path(), id("device"), Limits::default()).unwrap();
        drop(race);
        let first = db
            .put(&id("note"), ExpectedRevision::Absent, b"local")
            .unwrap();
        assert_eq!(first.version, 1);
        assert_eq!(db.read(&id("note")).unwrap().unwrap().1, b"local");
    }
}
