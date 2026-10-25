use crate::{Context, Workspace, failure, io_error, parse, private_options, unknown};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, File, Metadata, OpenOptions};
use cayw_codemode::CapabilityError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::TryLockError,
    io::{self, Read, Write},
    path::{Component, Path},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

pub(crate) const STORE: &str = ".cayw-workspace";
static NEXT: AtomicU64 = AtomicU64::new(1);

/// Names this workspace or a cooperating Host owns. Only actual internal
/// objects are reserved; other `.cayw-*` names are ordinary user paths.
pub(crate) fn reserved(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == ".git"
        || lower == STORE
        || lower == ".cayw-worktrees"
        || lower == ".cayw-host-lock"
        || lower == ".cayw-host-read-fence"
        || lower == ".cayw-host-read-state"
        || lower == ".cayw-host-read-fault"
        || lower == ".cayw-host-read-faults"
        || lower.starts_with(".cayw-host-tmp-")
        || lower.starts_with(".cayw-host-read-state-")
}
pub(crate) fn components(path: &str, root: bool) -> Result<Vec<&str>, CapabilityError> {
    if root && path == "." {
        return Ok(vec![]);
    }
    // macOS names may be UTF-8 and may contain characters that Windows forbids
    // (colon, backslash, trailing space/dot, reserved device stems). The fixed
    // directory handle, not a name whitelist, keeps access inside the root.
    if path.is_empty() || path.len() > 4096 || path.chars().any(char::is_control) {
        return Err(failure("expected a relative workspace path"));
    }
    let mut result = vec![];
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.len() > 255 || reserved(part) {
            return Err(failure("forbidden or reserved path component"));
        }
        result.push(part);
    }
    Ok(result)
}
pub(crate) fn directory(root: &Dir, parts: &[&str]) -> Result<Dir, CapabilityError> {
    let mut dir = root.try_clone().map_err(io_error)?;
    for part in parts {
        dir = dir.open_dir_nofollow(part).map_err(io_error)?;
    }
    Ok(dir)
}
pub(crate) fn parent<'a>(root: &Dir, parts: &[&'a str]) -> Result<(Dir, &'a str), CapabilityError> {
    let (name, parents) = parts
        .split_last()
        .ok_or_else(|| failure("path must name an entry"))?;
    Ok((directory(root, parents)?, name))
}
pub(crate) fn open_root(path: &Path) -> Result<Dir, CapabilityError> {
    if !path.is_absolute() {
        return Err(failure("application root must be absolute"));
    }
    let mut dir = Dir::open_ambient_dir("/", cap_std::ambient_authority()).map_err(io_error)?;
    for part in path.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(name) => dir = dir.open_dir_nofollow(name).map_err(io_error)?,
            _ => {
                return Err(failure(
                    "root must have no relative or normalizing components",
                ));
            }
        }
    }
    Ok(dir)
}
pub(crate) fn sync(dir: &Dir) -> Result<(), CapabilityError> {
    dir.try_clone()
        .map_err(io_error)?
        .into_std_file()
        .sync_all()
        .map_err(io_error)
}
pub(crate) fn open_store(root: &Dir, context: &Context) -> Result<Dir, CapabilityError> {
    context.check()?;
    match root.create_dir(STORE) {
        Ok(()) => sync(root)?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(io_error(error)),
    }
    let store = root.open_dir_nofollow(STORE).map_err(io_error)?;
    #[cfg(unix)]
    {
        use cap_std::fs::PermissionsExt;
        let mode = store.dir_metadata().map_err(io_error)?.permissions().mode();
        if mode & 0o077 != 0 {
            context.check()?;
            store
                .set_permissions(".", cap_std::fs::Permissions::from_mode(0o700))
                .map_err(io_error)?;
            sync(&store)?;
        }
    }
    Ok(store)
}
pub(crate) fn lock_file(root: &Dir) -> Result<std::fs::File, CapabilityError> {
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
        return Err(failure("lock must be a regular file"));
    }
    Ok(file.into_std())
}
pub(crate) async fn write_lock(
    root: &Dir,
    context: &Context,
) -> Result<std::fs::File, CapabilityError> {
    context.check()?;
    let file = lock_file(root)?;
    loop {
        context.check()?;
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::Error(error)) => return Err(io_error(error)),
            Err(TryLockError::WouldBlock) => {
                tokio::select! {
                    biased;
                    _ = context.cancel.cancelled() => return Err(failure("cancelled waiting for workspace lock")),
                    _ = tokio::time::sleep_until(context.deadline.into()) => return Err(failure("workspace lock deadline reached")),
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {},
                }
            }
        }
    }
}
pub(crate) fn regular(dir: &Dir, name: &str) -> Result<File, CapabilityError> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let file = dir.open_with(name, &options).map_err(io_error)?;
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err(failure("entry must be a regular file"));
    }
    Ok(file)
}
pub(crate) fn read(
    dir: &Dir,
    name: &str,
    limit: usize,
    context: &Context,
) -> Result<Vec<u8>, CapabilityError> {
    let mut file = regular(dir, name)?;
    if file.metadata().map_err(io_error)?.len() > limit as u64 {
        return Err(failure("file byte limit exceeded"));
    }
    read_bounded(&mut file, limit, context)
}
pub(crate) fn read_bounded(
    reader: &mut impl Read,
    limit: usize,
    context: &Context,
) -> Result<Vec<u8>, CapabilityError> {
    let mut bytes = vec![];
    let mut chunk = [0; 8192];
    loop {
        context.check()?;
        let count = reader.read(&mut chunk).map_err(io_error)?;
        if count == 0 {
            return Ok(bytes);
        }
        if count > limit.saturating_sub(bytes.len()) {
            return Err(failure("read byte limit exceeded"));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
}
pub(crate) fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub(crate) fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}
pub(crate) fn validate_hash(hash: &str) -> Result<(), CapabilityError> {
    if valid_hash(hash) {
        Ok(())
    } else {
        Err(failure("expected a SHA-256 hex digest"))
    }
}
pub(crate) fn metadata(dir: &Dir, name: &str) -> Result<Option<Metadata>, CapabilityError> {
    match dir.symlink_metadata(name) {
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(error)),
    }
}
pub(crate) fn mode(meta: &Metadata) -> u32 {
    #[cfg(unix)]
    {
        use cap_std::fs::PermissionsExt;
        meta.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0
    }
}
pub(crate) fn set_mode(file: &File, mode: u32) -> Result<(), CapabilityError> {
    #[cfg(unix)]
    {
        use cap_std::fs::PermissionsExt;
        file.set_permissions(cap_std::fs::Permissions::from_mode(mode))
            .map_err(io_error)?;
    }
    let _ = (file, mode);
    Ok(())
}
pub(crate) fn write_exclusive(
    dir: &Dir,
    name: &str,
    bytes: &[u8],
    mode: u32,
    context: &Context,
) -> Result<(), CapabilityError> {
    context.check()?;
    let mut file = dir.open_with(name, &private_options()).map_err(io_error)?;
    for chunk in bytes.chunks(8192) {
        context.check()?;
        file.write_all(chunk).map_err(io_error)?;
    }
    context.check()?;
    set_mode(&file, mode)?;
    file.sync_all().map_err(io_error)?;
    sync(dir)
}
pub(crate) fn unique() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{:x}-{:x}-{:x}",
        std::process::id(),
        nanos,
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Node {
    pub path: String,
    pub kind: String,
    pub bytes: u64,
    pub sha256: Option<String>,
    pub mode: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TreeArgs {
    path: String,
    max_entries: usize,
}
struct TreeScan<'a> {
    workspace: &'a Workspace,
    context: &'a Context,
    entries: Vec<Node>,
    max: usize,
    discovered: usize,
    bytes: usize,
    deleting: bool,
}
impl TreeScan<'_> {
    fn walk(&mut self, dir: &Dir, path: &str, depth: usize) -> Result<(), CapabilityError> {
        self.context.check()?;
        let mut names = vec![];
        for entry in dir.entries().map_err(io_error)? {
            self.context.check()?;
            let name = entry
                .map_err(io_error)?
                .file_name()
                .into_string()
                .map_err(|_| failure("entry name is not UTF-8"))?;
            if reserved(&name) {
                if self.deleting {
                    return Err(failure("recursive removal includes a protected entry"));
                }
                continue;
            }
            components(&name, false)?;
            if self.discovered >= self.max {
                return Err(failure("tree entry limit exceeded"));
            }
            self.discovered += 1;
            names.push(name);
        }
        names.sort();
        for name in names {
            self.context.check()?;
            if depth > self.workspace.limits.max_depth || self.entries.len() >= self.max {
                return Err(failure("tree depth or entry limit exceeded"));
            }
            let full = if path == "." {
                name.clone()
            } else {
                format!("{path}/{name}")
            };
            components(&full, false)?;
            let meta = dir.symlink_metadata(&name).map_err(io_error)?;
            if meta.is_file() {
                let bytes = read(
                    dir,
                    &name,
                    self.workspace.limits.max_file_bytes.min(
                        self.workspace
                            .limits
                            .max_batch_bytes
                            .saturating_sub(self.bytes),
                    ),
                    self.context,
                )?;
                self.bytes += bytes.len();
                self.entries.push(Node {
                    path: full,
                    kind: "file".into(),
                    bytes: bytes.len() as u64,
                    sha256: Some(hash(&bytes)),
                    mode: mode(&meta),
                });
            } else if meta.is_dir() {
                self.entries.push(Node {
                    path: full.clone(),
                    kind: "directory".into(),
                    bytes: 0,
                    sha256: None,
                    mode: mode(&meta),
                });
                let child = dir.open_dir_nofollow(&name).map_err(io_error)?;
                self.walk(&child, &full, depth + 1)?;
            } else {
                return Err(failure("tree contains a symbolic link or special file"));
            }
        }
        Ok(())
    }
}
fn scan(
    workspace: &Workspace,
    path: &str,
    max: usize,
    deleting: bool,
    context: &Context,
) -> Result<(Vec<Node>, String), CapabilityError> {
    if max == 0 || max > workspace.limits.max_tree_entries {
        return Err(failure("invalid tree entry bound"));
    }
    let parts = components(path, !deleting)?;
    let dir = directory(&workspace.root, &parts)?;
    let meta = dir.dir_metadata().map_err(io_error)?;
    let mut scan = TreeScan {
        workspace,
        context,
        entries: vec![Node {
            path: path.into(),
            kind: "directory".into(),
            bytes: 0,
            sha256: None,
            mode: mode(&meta),
        }],
        max,
        discovered: 1,
        bytes: 0,
        deleting,
    };
    scan.walk(&dir, path, 1)?;
    let digest =
        hash(&serde_json::to_vec(&scan.entries).map_err(|error| failure(error.to_string()))?);
    Ok((scan.entries, digest))
}
pub(crate) fn tree(
    workspace: &Workspace,
    args: &Value,
    context: &Context,
) -> Result<Value, CapabilityError> {
    let args: TreeArgs = parse(args)?;
    let (entries, root_sha256) = scan(workspace, &args.path, args.max_entries, false, context)?;
    Ok(
        json!({"path":args.path,"root_sha256":root_sha256,"entries":entries,"protected_entries_omitted":true}),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MkdirArgs {
    path: String,
    expected_absent: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MoveArgs {
    source: String,
    destination: String,
    expected_sha256: String,
    expected_destination_absent: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveArgs {
    path: String,
    #[serde(default)]
    recursive: bool,
    expected_sha256: Option<String>,
    expected_root_sha256: Option<String>,
    max_entries: Option<usize>,
}

pub(crate) fn mutate(
    workspace: &Workspace,
    name: &str,
    args: &Value,
    context: &Context,
) -> Result<Value, CapabilityError> {
    context.check()?;
    match name {
        "fs.mkdir" => {
            let args: MkdirArgs = parse(args)?;
            if !args.expected_absent {
                return Err(failure("mkdir requires expected_absent=true"));
            }
            let parts = components(&args.path, false)?;
            let (dir, name) = parent(&workspace.root, &parts)?;
            context.check()?;
            dir.create_dir(name).map_err(io_error)?;
            sync(&dir).map_err(unknown)?;
            Ok(json!({"path":args.path,"created":true}))
        }
        "fs.move" => {
            let args: MoveArgs = parse(args)?;
            validate_hash(&args.expected_sha256)?;
            if !args.expected_destination_absent {
                return Err(failure("move requires expected_destination_absent=true"));
            }
            let source = components(&args.source, false)?;
            let destination = components(&args.destination, false)?;
            let (from, filename) = parent(&workspace.root, &source)?;
            let (to, target) = parent(&workspace.root, &destination)?;
            let bytes = read(&from, filename, workspace.limits.max_file_bytes, context)?;
            if !hash(&bytes).eq_ignore_ascii_case(&args.expected_sha256) {
                return Err(failure("move source compare-and-swap conflict"));
            }
            if metadata(&to, target)?.is_some() {
                return Err(failure("move destination must be absent"));
            }
            context.check()?;
            // Link is no-clobber. Removal is a separate visible step and can fail.
            from.hard_link(filename, &to, target).map_err(io_error)?;
            sync(&to).map_err(unknown)?;
            let linked =
                read(&to, target, workspace.limits.max_file_bytes, context).map_err(unknown)?;
            let current =
                read(&from, filename, workspace.limits.max_file_bytes, context).map_err(unknown)?;
            if hash(&linked) != hash(&bytes) || hash(&current) != hash(&bytes) {
                return Err(CapabilityError::unknown(
                    "move raced with a source writer; both paths retained for reconciliation",
                ));
            }
            context.check().map_err(unknown)?;
            from.remove_file(filename)
                .map_err(io_error)
                .map_err(unknown)?;
            sync(&from).map_err(unknown)?;
            Ok(
                json!({"source":args.source,"destination":args.destination,"sha256":hash(&bytes),"moved":true}),
            )
        }
        "fs.remove" => {
            let args: RemoveArgs = parse(args)?;
            let parts = components(&args.path, false)?;
            let (dir, filename) = parent(&workspace.root, &parts)?;
            let meta = dir.symlink_metadata(filename).map_err(io_error)?;
            if meta.is_file() {
                if args.recursive
                    || args.expected_root_sha256.is_some()
                    || args.max_entries.is_some()
                {
                    return Err(failure("file removal accepts only expected_sha256"));
                }
                let expected = args
                    .expected_sha256
                    .ok_or_else(|| failure("file removal requires expected_sha256"))?;
                validate_hash(&expected)?;
                let bytes = read(&dir, filename, workspace.limits.max_file_bytes, context)?;
                if !hash(&bytes).eq_ignore_ascii_case(&expected) {
                    return Err(failure("remove compare-and-swap conflict"));
                }
                context.check()?;
                dir.remove_file(filename).map_err(io_error)?;
                sync(&dir).map_err(unknown)?;
                Ok(json!({"path":args.path,"removed_entries":1}))
            } else if meta.is_dir() {
                if args.expected_sha256.is_some() {
                    return Err(failure("directory removal does not accept file hash"));
                }
                if !args.recursive {
                    if args.expected_root_sha256.is_some() || args.max_entries.is_some() {
                        return Err(failure(
                            "empty directory removal does not accept recursive options",
                        ));
                    }
                    context.check()?;
                    dir.remove_dir(filename).map_err(io_error)?;
                    sync(&dir).map_err(unknown)?;
                    return Ok(json!({"path":args.path,"removed_entries":1}));
                }
                let expected = args
                    .expected_root_sha256
                    .ok_or_else(|| failure("recursive removal requires expected_root_sha256"))?;
                validate_hash(&expected)?;
                let max = args
                    .max_entries
                    .ok_or_else(|| failure("recursive removal requires max_entries"))?;
                let (entries, digest) = scan(workspace, &args.path, max, true, context)?;
                if !digest.eq_ignore_ascii_case(&expected) {
                    return Err(failure("recursive removal root hash conflict"));
                }
                let mut removed = 0;
                for (index, node) in entries.iter().rev().enumerate() {
                    let result = (|| {
                        context.check()?;
                        workspace.fault("before_remove", index, context)?;
                        let parts = components(&node.path, false)?;
                        let (dir, name) = parent(&workspace.root, &parts)?;
                        if node.kind == "file" {
                            let bytes = read(&dir, name, workspace.limits.max_file_bytes, context)?;
                            if Some(hash(&bytes)) != node.sha256 {
                                return Err(failure("recursive removal file changed"));
                            }
                            dir.remove_file(name).map_err(io_error)?;
                        } else {
                            dir.remove_dir(name).map_err(io_error)?;
                        }
                        removed += 1;
                        workspace.fault("after_remove", index, context)?;
                        sync(&dir)
                    })();
                    if let Err(error) = result {
                        return Err(if removed > 0 { unknown(error) } else { error });
                    }
                }
                Ok(json!({"path":args.path,"removed_entries":removed,"root_sha256":digest}))
            } else {
                Err(failure("cannot remove symbolic links or special files"))
            }
        }
        _ => Err(failure("unknown mutation")),
    }
}
