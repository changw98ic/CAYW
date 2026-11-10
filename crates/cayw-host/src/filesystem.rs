use crate::{Host, check, failure, io_error};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, File, Metadata, OpenOptions, Permissions};
use cayw_ai::CancellationToken;
use cayw_codemode::CapabilityError;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::TryLockError,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

const INTERNAL: &str = ".cayw-host-";
/// An ordinary un-ranged read returns the file directly up to this size.
const INLINE_READ_LIMIT: u64 = 8 * 1024 * 1024;
/// Default preview size for an un-ranged read above `INLINE_READ_LIMIT`.
const DEFAULT_PREVIEW_BYTES: usize = 256 * 1024;
/// Default number of lines for a line-ranged read.
const DEFAULT_READ_LINES: u64 = 2000;
/// Upper bound on one streamed read window. Explicit host limits may lower it.
const READ_WINDOW_BYTES: usize = 8 * 1024 * 1024;
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
fn read_up_to(
    file: &mut File,
    limit: usize,
    detect_extra: bool,
    cancel: &CancellationToken,
    deadline: Option<Instant>,
) -> Result<(Vec<u8>, bool), CapabilityError> {
    let target = if detect_extra {
        limit.saturating_add(1)
    } else {
        limit
    };
    let mut bytes = Vec::with_capacity(target.min(64 * 1024));
    let mut chunk = [0; 8192];
    while bytes.len() < target {
        check(cancel, deadline)?;
        let want = (target - bytes.len()).min(chunk.len());
        let count = file.read(&mut chunk[..want]).map_err(io_error)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let truncated = detect_extra && bytes.len() > limit;
    if truncated {
        bytes.truncate(limit);
    }
    Ok((bytes, truncated))
}

fn trim_incomplete_utf8(bytes: &mut Vec<u8>) -> Result<(), CapabilityError> {
    match std::str::from_utf8(bytes) {
        Ok(_) => Ok(()),
        Err(error) if error.error_len().is_none() => {
            bytes.truncate(error.valid_up_to());
            Ok(())
        }
        Err(_) => Err(failure("file is not UTF-8")),
    }
}

fn text(bytes: &[u8]) -> Result<String, CapabilityError> {
    String::from_utf8(bytes.to_vec()).map_err(|_| failure("file is not UTF-8"))
}

/// Trims an incomplete trailing character and, when possible, cuts the preview
/// at the last complete line so the next line range starts cleanly.
fn preview(mut bytes: Vec<u8>) -> Result<(String, usize), CapabilityError> {
    trim_incomplete_utf8(&mut bytes)?;
    if let Some(at) = bytes.iter().rposition(|byte| *byte == b'\n') {
        bytes.truncate(at + 1);
    }
    let content = text(&bytes)?;
    let consumed = content.len();
    Ok((content, consumed))
}

fn line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.bytes().filter(|byte| *byte == b'\n').count() + usize::from(!text.ends_with('\n'))
    }
}

fn read_bytes(
    dir: &Dir,
    name: &str,
    limit: Option<usize>,
    cancel: &CancellationToken,
    deadline: Option<Instant>,
) -> Result<Vec<u8>, CapabilityError> {
    let mut file = regular(dir, name)?;
    if let Some(limit) = limit {
        if file.metadata().map_err(io_error)?.len() > limit as u64 {
            return Err(failure("file exceeds byte limit"));
        }
        return read_up_to(&mut file, limit, false, cancel, deadline).map(|(bytes, _)| bytes);
    }
    let mut bytes = vec![];
    let mut chunk = [0; 8192];
    loop {
        check(cancel, deadline)?;
        let count = file.read(&mut chunk).map_err(io_error)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    Ok(bytes)
}

fn read_line_range(
    dir: &Dir,
    name: &str,
    start_line: u64,
    max_lines: u64,
    byte_cap: usize,
    cancel: &CancellationToken,
    deadline: Option<Instant>,
) -> Result<(String, usize, u64, u64, bool), CapabilityError> {
    let file = regular(dir, name)?;
    let total_bytes = file.metadata().map_err(io_error)?.len();
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut offset = 0u64;
    for _ in 1..start_line {
        check(cancel, deadline)?;
        line.clear();
        let count = reader.read_until(b'\n', &mut line).map_err(io_error)?;
        if count == 0 {
            break;
        }
        offset += count as u64;
    }
    let mut collected = Vec::new();
    let mut lines_returned = 0usize;
    let mut truncated = false;
    while (lines_returned as u64) < max_lines {
        check(cancel, deadline)?;
        line.clear();
        let count = reader.read_until(b'\n', &mut line).map_err(io_error)?;
        if count == 0 {
            break;
        }
        if collected.len().saturating_add(count) > byte_cap {
            if lines_returned == 0 {
                return Err(failure("requested line exceeds the read window"));
            }
            truncated = true;
            break;
        }
        collected.extend_from_slice(&line);
        lines_returned += 1;
    }
    if !truncated && (lines_returned as u64) == max_lines {
        truncated = !reader.fill_buf().map_err(io_error)?.is_empty();
    }
    trim_incomplete_utf8(&mut collected)?;
    let content = text(&collected)?;
    Ok((content, lines_returned, offset, total_bytes, truncated))
}

fn read_byte_range(
    dir: &Dir,
    name: &str,
    offset: u64,
    max_bytes: usize,
    byte_cap: usize,
    cancel: &CancellationToken,
    deadline: Option<Instant>,
) -> Result<(String, u64, u64, bool), CapabilityError> {
    let mut file = regular(dir, name)?;
    let total_bytes = file.metadata().map_err(io_error)?.len();
    let start = offset.min(total_bytes);
    if start > 0 {
        file.seek(SeekFrom::Start(start)).map_err(io_error)?;
    }
    let window = max_bytes.min(byte_cap).max(1);
    let (mut bytes, truncated) = read_up_to(&mut file, window, true, cancel, deadline)?;
    if truncated {
        trim_incomplete_utf8(&mut bytes)?;
    }
    let content = text(&bytes)?;
    Ok((content, start, total_bytes, truncated))
}

fn read_window(host: &Host) -> usize {
    host.config
        .max_file_bytes
        .map_or(READ_WINDOW_BYTES, |limit| READ_WINDOW_BYTES.min(limit))
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
    deadline: Option<Instant>,
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
                    _ = crate::sleep_deadline(deadline) => return Err(failure("workspace write lock deadline reached")),
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
    deadline: Option<Instant>,
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
    deadline: Option<Instant>,
) -> Result<Value, CapabilityError> {
    check(cancel, deadline)?;
    let path = args["path"].as_str().expect("validated path");
    let components = components(path, matches!(name, "fs.list" | "fs.stat"))?;
    match name {
        "fs.read" => {
            let (dir, name) = parent(&host.root, &components)?;
            let line_mode = args.get("start_line").is_some() || args.get("max_lines").is_some();
            let byte_mode = args.get("offset_bytes").is_some() || args.get("max_bytes").is_some();
            if line_mode && byte_mode {
                return Err(failure(
                    "line range and byte range arguments cannot be combined",
                ));
            }
            if let Some(limit) = host.config.max_file_bytes
                && !line_mode
                && !byte_mode
            {
                let bytes = read_bytes(&dir, name, Some(limit), cancel, deadline)?;
                let sha256 = hash(&bytes);
                let content = text(&bytes)?;
                return Ok(
                    json!({"path":path,"content":content,"bytes":content.len(),"sha256":sha256}),
                );
            }
            if byte_mode {
                let offset = args
                    .get("offset_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let max_bytes = args
                    .get("max_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_PREVIEW_BYTES as u64)
                    .min(usize::MAX as u64) as usize;
                let (content, offset, total, truncated) = read_byte_range(
                    &dir,
                    name,
                    offset,
                    max_bytes,
                    read_window(host),
                    cancel,
                    deadline,
                )?;
                let mut result = json!({
                    "path":path,
                    "content":content,
                    "bytes":content.len(),
                    "total_bytes":total,
                    "offset_bytes":offset,
                    "truncated":truncated,
                });
                if truncated {
                    result["next_offset_bytes"] = json!(offset + content.len() as u64);
                } else if offset == 0 {
                    result["sha256"] = json!(hash(content.as_bytes()));
                }
                return Ok(result);
            }
            if line_mode {
                let start_line = args.get("start_line").and_then(Value::as_u64).unwrap_or(1);
                let max_lines = args
                    .get("max_lines")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_READ_LINES);
                let (content, lines_returned, offset, total, truncated) = read_line_range(
                    &dir,
                    name,
                    start_line,
                    max_lines,
                    read_window(host),
                    cancel,
                    deadline,
                )?;
                let mut result = json!({
                    "path":path,
                    "content":content,
                    "bytes":content.len(),
                    "total_bytes":total,
                    "start_line":start_line,
                    "lines_returned":lines_returned,
                    "offset_bytes":offset,
                    "truncated":truncated,
                });
                if truncated {
                    result["next_start_line"] = json!(start_line + lines_returned as u64);
                    result["next_offset_bytes"] = json!(offset + content.len() as u64);
                } else if start_line == 1 {
                    result["sha256"] = json!(hash(content.as_bytes()));
                }
                return Ok(result);
            }
            let mut file = regular(&dir, name)?;
            let total = file.metadata().map_err(io_error)?.len();
            if total <= INLINE_READ_LIMIT {
                let (bytes, _) = read_up_to(
                    &mut file,
                    INLINE_READ_LIMIT as usize,
                    false,
                    cancel,
                    deadline,
                )?;
                let sha256 = hash(&bytes);
                let content = text(&bytes)?;
                return Ok(
                    json!({"path":path,"content":content,"bytes":content.len(),"sha256":sha256}),
                );
            }
            let (bytes, _) = read_up_to(&mut file, DEFAULT_PREVIEW_BYTES, false, cancel, deadline)?;
            let (content, consumed) = preview(bytes)?;
            let returned = line_count(&content);
            Ok(json!({
                "path":path,
                "content":content,
                "bytes":consumed,
                "total_bytes":total,
                "start_line":1,
                "lines_returned":returned,
                "next_start_line":1 + returned as u64,
                "offset_bytes":0,
                "next_offset_bytes":consumed,
                "truncated":true,
            }))
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
                if host
                    .config
                    .max_list_entries
                    .is_some_and(|limit| entries.len() >= limit)
                {
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
                    if host.config.max_file_bytes.is_some_and(|limit| size > limit) {
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
            if host
                .config
                .max_file_bytes
                .is_some_and(|limit| content.len() > limit)
            {
                return Err(failure("written file exceeds byte limit"));
            }
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
