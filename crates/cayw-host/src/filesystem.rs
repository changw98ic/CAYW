use crate::{Host, check, failure, io_error};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, File, Metadata, OpenOptions, Permissions};
use cayw_ai::CancellationToken;
use cayw_codemode::CapabilityError;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::TryLockError,
    io::{self, Read, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

const INTERNAL: &str = ".cayw-host-";
static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

/// Actual objects owned by this Host or by Git. Other `.cayw-*` names are
/// ordinary user files and must remain readable, listable and writable.
const INTERNAL_NAMES: &[&str] = &[
    ".git",
    ".cayw-host-lock",
    ".cayw-workspace",
    ".cayw-worktrees",
    ".cayw-host-read-fence",
    ".cayw-host-read-state",
    ".cayw-host-read-fault",
    ".cayw-host-read-faults",
];
const INTERNAL_PREFIXES: &[&str] = &[".cayw-host-tmp-", ".cayw-host-read-state-"];
pub(crate) fn internal(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    INTERNAL_NAMES.contains(&lower.as_str())
        || INTERNAL_PREFIXES
            .iter()
            .any(|prefix| lower.starts_with(prefix))
}
pub(crate) fn git_internal_excludes() -> String {
    let mut patterns = String::new();
    for (name, prefix) in INTERNAL_NAMES
        .iter()
        .map(|name| (*name, false))
        .chain(INTERNAL_PREFIXES.iter().map(|name| (*name, true)))
    {
        for character in name.chars() {
            if character.is_ascii_alphabetic() {
                patterns.push('[');
                patterns.push(character);
                patterns.push(character.to_ascii_uppercase());
                patterns.push(']');
            } else {
                patterns.push(character);
            }
        }
        if prefix {
            patterns.push('*');
        }
        patterns.push('\n');
    }
    patterns
}
/// A relative path with no normalizing segments or actual internal objects.
/// Validation is lexical; directory handles enforce the access boundary.
/// macOS accepts UTF-8 names and characters Windows forbids; those are kept.
pub(crate) fn components(path: &str, allow_root: bool) -> Result<Vec<&str>, CapabilityError> {
    if allow_root && path == "." {
        return Ok(vec![]);
    }
    if path.is_empty() || path.len() > 4096 || path.chars().any(char::is_control) {
        return Err(failure("expected a relative workspace path"));
    }
    let mut components = vec![];
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.len() > 255 || internal(part) {
            return Err(failure("path contains a forbidden or reserved component"));
        }
        components.push(part);
    }
    Ok(components)
}
fn directory(root: &Dir, components: &[&str]) -> Result<Dir, CapabilityError> {
    let mut dir = root.try_clone().map_err(io_error)?;
    for component in components {
        dir = dir.open_dir_nofollow(component).map_err(io_error)?;
    }
    Ok(dir)
}
fn parent<'a>(root: &Dir, components: &[&'a str]) -> Result<(Dir, &'a str), CapabilityError> {
    let (name, parents) = components
        .split_last()
        .ok_or_else(|| failure("file path must name an entry"))?;
    Ok((directory(root, parents)?, name))
}
fn regular(dir: &Dir, name: &str) -> Result<File, CapabilityError> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let file = dir.open_with(name, &options).map_err(io_error)?;
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err(failure("entry must be a regular file"));
    }
    Ok(file)
}
fn read_bytes(
    dir: &Dir,
    name: &str,
    limit: usize,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Vec<u8>, CapabilityError> {
    let mut file = regular(dir, name)?;
    if file.metadata().map_err(io_error)?.len() > limit as u64 {
        return Err(failure("file exceeds byte limit"));
    }
    let mut bytes = vec![];
    let mut chunk = [0; 8192];
    loop {
        check(cancel, deadline)?;
        let count = file.read(&mut chunk).map_err(io_error)?;
        if count == 0 {
            break;
        }
        if count > limit.saturating_sub(bytes.len()) {
            return Err(failure("file exceeds byte limit"));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    Ok(bytes)
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn kind(metadata: &Metadata) -> &'static str {
    if metadata.is_file() {
        "file"
    } else if metadata.is_dir() {
        "directory"
    } else if metadata.file_type().is_symlink() {
        "symlink"
    } else {
        "other"
    }
}

/// One advisory OS file lock shared by all cooperating hosts using this root.
/// The handle is held through CAS verification, replacement, and directory sync.
pub(crate) async fn write_lock(
    root: &Dir,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<std::fs::File, CapabilityError> {
    check(cancel, deadline)?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .follow(FollowSymlinks::No)
        .nonblock(true);
    let file = root
        .open_with(".cayw-host-lock", &options)
        .map_err(io_error)?;
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err(failure("workspace lock must be a regular file"));
    }
    let file = file.into_std();
    loop {
        check(cancel, deadline)?;
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::Error(error)) => return Err(io_error(error)),
            Err(TryLockError::WouldBlock) => {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(failure("cancelled while waiting for workspace write lock")),
                    _ = tokio::time::sleep_until(deadline.into()) => return Err(failure("workspace write lock deadline reached")),
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {},
                }
            }
        }
    }
}

struct Temporary<'a> {
    dir: &'a Dir,
    name: String,
    present: bool,
}
impl Drop for Temporary<'_> {
    fn drop(&mut self) {
        if self.present {
            let _ = self.dir.remove_file(&self.name);
        }
    }
}
fn temporary(dir: &Dir) -> Result<(Temporary<'_>, File), CapabilityError> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    for _ in 0..128 {
        let name = format!(
            "{INTERNAL}tmp-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        );
        match dir.open_with(&name, &options) {
            Ok(file) => {
                return Ok((
                    Temporary {
                        dir,
                        name,
                        present: true,
                    },
                    file,
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(error)),
        }
    }
    Err(failure("unable to create a unique temporary file"))
}


/// Durably records directory entry updates after a rename or hard-link commit.
///
/// `cap_std::Dir` is opened with `O_PATH` on Linux. Those descriptors reject
/// `fsync` with `EBADF`, so reopening `"."` yields a normal directory file
/// descriptor that can be synchronized. Other Unix platforms keep the previous
/// `Dir::into_std_file` path.
pub(crate) fn sync_directory(dir: &Dir) -> Result<(), CapabilityError> {
    #[cfg(target_os = "linux")]
    {
        dir.open(".")
            .map_err(|error| {
                CapabilityError::unknown(format!(
                    "file committed but directory synchronization failed: {error}"
                ))
            })?
            .into_std()
            .sync_all()
            .map_err(|error| {
                CapabilityError::unknown(format!(
                    "file committed but directory synchronization failed: {error}"
                ))
            })
    }
    #[cfg(not(target_os = "linux"))]
    {
        dir.try_clone()
            .map_err(|error| CapabilityError::unknown(error.to_string()))?
            .into_std_file()
            .sync_all()
            .map_err(|error| {
                CapabilityError::unknown(format!(
                    "file committed but directory synchronization failed: {error}"
                ))
            })
    }
}

/// Once rename/link may have committed, every uncertain durability result is
/// unknown. No await point occurs between checking cancellation and committing.
fn commit(
    dir: &Dir,
    name: &str,
    bytes: &[u8],
    permissions: Option<Permissions>,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Value, CapabilityError> {
    let overwrite = permissions.is_some();
    let (mut temporary, mut file) = temporary(dir)?;
    if let Some(permissions) = permissions {
        file.set_permissions(permissions).map_err(io_error)?;
    }
    for chunk in bytes.chunks(8192) {
        check(cancel, deadline)?;
        file.write_all(chunk).map_err(io_error)?;
    }
    file.sync_all().map_err(io_error)?;
    check(cancel, deadline)?;
    if overwrite {
        // Renaming only the entry cannot follow a destination symlink. The
        // directory handle remains pinned if an ancestor is renamed meanwhile.
        dir.rename(&temporary.name, dir, name).map_err(io_error)?;
        temporary.present = false;
    } else {
        // Creating a hard link is atomic and fails if *any* destination entry
        // exists, including a link created after the absence check.
        dir.hard_link(&temporary.name, dir, name)
            .map_err(io_error)?;
        dir.remove_file(&temporary.name).map_err(|error| {
            CapabilityError::unknown(format!(
                "file committed but temporary unlink failed: {error}"
            ))
        })?;
        temporary.present = false;
    }
    sync_directory(dir)?;
    Ok(json!({"bytes":bytes.len(), "sha256":hash(bytes)}))
}

pub(crate) async fn execute(
    host: &Host,
    name: &str,
    args: &Value,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Value, CapabilityError> {
    check(cancel, deadline)?;
    let path = args["path"].as_str().expect("validated path");
    let components = components(path, matches!(name, "fs.list" | "fs.stat"))?;
    match name {
        "fs.read" => {
            let (dir, name) = parent(&host.root, &components)?;
            let bytes = read_bytes(&dir, name, host.config.max_file_bytes, cancel, deadline)?;
            let sha256 = hash(&bytes);
            let content = String::from_utf8(bytes).map_err(|_| failure("file is not UTF-8"))?;
            Ok(json!({"path":path,"content":content,"bytes":content.len(),"sha256":sha256}))
        }
        "fs.stat" => {
            let metadata = if components.is_empty() {
                host.root.dir_metadata().map_err(io_error)?
            } else {
                let (dir, name) = parent(&host.root, &components)?;
                dir.symlink_metadata(name).map_err(io_error)?
            };
            Ok(
                json!({"path":path,"kind":kind(&metadata),"size":metadata.len(),"readonly":metadata.permissions().readonly()}),
            )
        }
        "fs.list" => {
            let dir = directory(&host.root, &components)?;
            let mut entries = vec![];
            for entry in dir.entries().map_err(io_error)? {
                check(cancel, deadline)?;
                let entry = entry.map_err(io_error)?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| failure("entry name is not UTF-8"))?;
                if internal(&name) {
                    continue;
                }
                if entries.len() >= host.config.max_list_entries {
                    return Err(failure("directory entry limit reached"));
                }
                let metadata = dir.symlink_metadata(&name).map_err(io_error)?;
                entries.push(json!({"name":name,"kind":kind(&metadata),"size":metadata.len()}));
            }
            entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            Ok(json!({"path":path,"entries":entries}))
        }
        "fs.write" | "fs.patch" => {
            // Host holds the shared lock from journal readiness through commit.
            let (dir, filename) = parent(&host.root, &components)?;
            let expected = args.get("expected_sha256").and_then(Value::as_str);
            let permissions = match dir.symlink_metadata(filename) {
                Ok(metadata) => {
                    if !metadata.is_file() {
                        return Err(failure("destination must be a regular file"));
                    }
                    Some(metadata.permissions())
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(io_error(error)),
            };
            let exists = permissions.is_some();
            if expected.is_some() && !exists {
                return Err(failure("compare-and-swap conflict: destination is absent"));
            }
            if name == "fs.patch" || exists {
                let expected = expected.ok_or_else(|| {
                    failure("compare-and-swap conflict: existing file requires expected_sha256")
                })?;
                let before =
                    read_bytes(&dir, filename, host.config.max_file_bytes, cancel, deadline)?;
                if !hash(&before).eq_ignore_ascii_case(expected) {
                    return Err(failure("compare-and-swap conflict: file changed"));
                }
                if name == "fs.patch" {
                    let text = String::from_utf8(before)
                        .map_err(|_| failure("patch target is not UTF-8"))?;
                    let old = args["old"].as_str().expect("validated old text");
                    let new = args["new"].as_str().expect("validated new text");
                    let at = text
                        .find(old)
                        .ok_or_else(|| failure("patch text was not found"))?;
                    let next_character = text[at..]
                        .chars()
                        .next()
                        .expect("nonempty match")
                        .len_utf8();
                    if text[at + next_character..].contains(old) {
                        return Err(failure("patch text must match exactly once"));
                    }
                    let size = text
                        .len()
                        .checked_sub(old.len())
                        .and_then(|size| size.checked_add(new.len()))
                        .ok_or_else(|| failure("patch size overflow"))?;
                    if size > host.config.max_file_bytes {
                        return Err(failure("patched file exceeds byte limit"));
                    }
                    let mut patched = String::with_capacity(size);
                    patched.push_str(&text[..at]);
                    patched.push_str(new);
                    patched.push_str(&text[at + old.len()..]);
                    return commit(
                        &dir,
                        filename,
                        patched.as_bytes(),
                        permissions,
                        cancel,
                        deadline,
                    );
                }
            }
            let content = args["content"].as_str().expect("validated content");
            commit(
                &dir,
                filename,
                content.as_bytes(),
                permissions,
                cancel,
                deadline,
            )
        }
        _ => Err(failure("unknown filesystem capability")),
    }
}

pub(crate) fn validate_git_path(root: &Dir, path: &str) -> Result<(), CapabilityError> {
    let parts = components(path, false)?;
    let mut dir = root.try_clone().map_err(io_error)?;
    for (index, part) in parts.iter().enumerate() {
        let metadata = match dir.symlink_metadata(Path::new(part)) {
            Ok(metadata) => metadata,
            // Historical and deleted paths may have no current parent subtree.
            // The complete path has already passed lexical validation; check
            // every existing prefix without resolving any later ambient path.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_error(error)),
        };
        if metadata.file_type().is_symlink() {
            return Err(failure("git path may not be a symbolic link"));
        }
        if index + 1 < parts.len() {
            dir = dir.open_dir_nofollow(part).map_err(io_error)?;
        }
    }
    Ok(())
}
