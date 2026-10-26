use crate::{Host, HostConfig, ProfileKind, check, failure, filesystem, io_error};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, DirBuilder, OpenOptions};
use cayw_ai::CancellationToken;
use cayw_codemode::CapabilityError;
use cayw_device::{
    CleanupResource, DeviceError, DirectoryProcessSpec, ProcessCleanup, ProcessRunner,
    TerminationReason,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, Read},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}
/// A test filter is one literal argv item: it is never split or passed to a
/// shell. Only the ambiguity that a leading `-` would become an option is
/// rejected, plus control characters and the catalog's length bound. Ordinary
/// test names may use any Unicode, spaces, parentheses or a leading underscore.
pub(crate) fn valid_filter(filter: &str) -> bool {
    !filter.is_empty()
        && filter.chars().count() <= 256
        && !filter.starts_with('-')
        && !filter.chars().any(char::is_control)
}
pub(crate) fn validate_program(program: &Path) -> Result<(), CapabilityError> {
    if !program.is_absolute() || !program.is_file() {
        return Err(failure(
            "profile executable must be an existing absolute file",
        ));
    }
    Ok(())
}
pub(crate) fn runner(config: &HostConfig) -> Result<ProcessRunner, CapabilityError> {
    let programs = config
        .git_program
        .iter()
        .cloned()
        .chain(
            config
                .terminal_handoffs
                .iter()
                .flat_map(|terminal| terminal.profiles.values())
                .map(|profile| profile.program.clone()),
        )
        .chain(
            config
                .profiles
                .values()
                .map(|profile| profile.program.clone()),
        );
    ProcessRunner::new(programs).map_err(|error| failure(error.to_string()))
}

#[cfg(windows)]
fn null_device() -> &'static str {
    "NUL"
}
#[cfg(not(windows))]
fn null_device() -> &'static str {
    "/dev/null"
}

pub(crate) fn git_environment() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_CONFIG_SYSTEM".into(), null_device().into()),
        ("GIT_CONFIG_GLOBAL".into(), null_device().into()),
        ("GIT_CONFIG_COUNT".into(), "0".into()),
        ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
        ("GIT_NO_REPLACE_OBJECTS".into(), "1".into()),
        ("GIT_NO_LAZY_FETCH".into(), "1".into()),
        ("GIT_ATTR_NOSYSTEM".into(), "1".into()),
        ("GIT_LITERAL_PATHSPECS".into(), "1".into()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ("LC_ALL".into(), "C".into()),
    ])
}
/// Fixed global flags and command-scope overrides shared by metadata-view reads and
/// real-repository writes. The caller appends the subcommand and its arguments.
pub(crate) fn git_base_args() -> Vec<String> {
    git_base_args_with_excludes(null_device())
}
/// Reads use a private excludes file that names only real Host state; writes
/// keep the system null device. No ambient exclude file is consulted.
fn git_base_args_with_excludes(excludes_file: &str) -> Vec<String> {
    let mut args = vec![
        "--no-pager".into(),
        "--no-optional-locks".into(),
        "--no-replace-objects".into(),
    ];
    for (key, value) in [
        ("core.hooksPath", null_device()),
        ("core.fsmonitor", "false"),
        ("core.attributesFile", null_device()),
        ("core.excludesFile", excludes_file),
        ("core.pager", ""),
        ("core.askPass", ""),
        ("diff.external", ""),
        ("diff.trustExitCode", "false"),
        ("diff.ignoreSubmodules", "all"),
        ("submodule.recurse", "false"),
        ("log.showSignature", "false"),
        ("gc.auto", "0"),
        ("maintenance.auto", "false"),
        ("protocol.allow", "never"),
        ("credential.helper", ""),
        ("core.alternateRefsCommand", ""),
        ("color.ui", "false"),
        ("safe.directory", ""),
    ] {
        args.push("-c".into());
        args.push(format!("{key}={value}"));
    }
    args
}
fn git_args(name: &str, arguments: &Value, excludes_file: &str) -> Vec<String> {
    let mut args = git_base_args_with_excludes(excludes_file);
    match name {
        "git.status" => args.extend(
            [
                "status",
                "--porcelain=v1",
                "--untracked-files=normal",
                "--ignore-submodules=all",
            ]
            .map(str::to_string),
        ),
        "git.diff" => {
            args.extend(
                [
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--ignore-submodules=all",
                    "--no-renames",
                    "--no-relative",
                ]
                .map(str::to_string),
            );
            if arguments.get("staged").and_then(Value::as_bool) == Some(true) {
                args.push("--cached".into());
            }
            args.push("--".into());
            if let Some(path) = arguments.get("path").and_then(Value::as_str) {
                args.push(path.into());
            }
        }
        "git.log" => {
            args.extend(
                [
                    "log",
                    "--no-show-signature",
                    "--no-decorate",
                    "--no-use-mailmap",
                    "--format=%H%x00%s",
                    "--no-color",
                ]
                .map(str::to_string),
            );
            args.push(format!(
                "--max-count={}",
                arguments.get("limit").and_then(Value::as_u64).unwrap_or(20)
            ));
            args.push("--".into());
        }
        "git.show" => {
            args.extend(
                [
                    "show",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-show-signature",
                    "--no-decorate",
                    "--format=fuller",
                    "--no-color",
                    "--ignore-submodules=all",
                    "--no-renames",
                ]
                .map(str::to_string),
            );
            let commit = arguments["commit"].as_str().expect("validated commit");
            if let Some(path) = arguments.get("path").and_then(Value::as_str) {
                args.push(format!("{commit}:{path}"));
            } else {
                args.push(commit.into());
            }
            args.push("--".into());
        }
        "git.blame" => {
            args.extend(["blame", "--line-porcelain", "--no-progress"].map(str::to_string));
            if let Some(commit) = arguments.get("commit").and_then(Value::as_str) {
                args.push(commit.into());
            }
            args.push("--".into());
            args.push(
                arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .expect("validated blame path")
                    .into(),
            );
        }
        _ => unreachable!("catalog contains only fixed git commands"),
    }
    args
}
static NEXT_VIEW: AtomicU64 = AtomicU64::new(1);
struct GitView {
    parent: Dir,
    name: String,
    dir: Dir,
    quarantine: Option<Dir>,
}

#[cfg(unix)]
fn same_directory(one: &Dir, two: &Dir) -> io::Result<bool> {
    use cap_std::fs::MetadataExt;
    let one = one.dir_metadata()?;
    let two = two.dir_metadata()?;
    Ok(one.dev() == two.dev() && one.ino() == two.ino())
}

impl ProcessCleanup for GitView {
    fn cleanup(&mut self) -> io::Result<()> {
        #[cfg(not(unix))]
        return Err(io::Error::other(
            "Git metadata view cleanup is unsupported on this platform",
        ));
        #[cfg(unix)]
        {
            let changed = || {
                io::Error::other(format!(
                    "Git metadata view ownership changed; retained opened directory for {}",
                    self.name
                ))
            };
            // Never recursively delete through an old name. A precheck alone is
            // insufficient: move the candidate into a fresh private quarantine,
            // then compare its opened identity again before touching contents.
            let candidate = self
                .parent
                .open_dir_nofollow(&self.name)
                .map_err(|_| changed())?;
            if !same_directory(&candidate, &self.dir)? {
                return Err(changed());
            }
            let (quarantine_name, quarantine) = private_directory(&self.parent, "cleanup")?;
            self.quarantine = Some(quarantine);
            let quarantine = self.quarantine.as_ref().expect("owned quarantine");
            #[cfg(all(test, unix))]
            tests::interfere_with_cleanup(
                tests::CleanupCheckpoint::Rename,
                &self.parent,
                &self.name,
            );
            self.parent.rename(&self.name, quarantine, "owned")?;
            let candidate = quarantine
                .open_dir_nofollow("owned")
                .map_err(|_| changed())?;
            if !same_directory(&candidate, &self.dir)? {
                // A source-name swap raced the rename. Keep the moved candidate
                // untouched and report its location; do not guess a restoration.
                return Err(io::Error::other(format!(
                    "Git metadata view ownership changed during cleanup; retained candidate in {quarantine_name}/owned"
                )));
            }
            // Recursive deletion is rooted in the opened, checked handle inside
            // this private directory, not the publicly replaceable original name.
            #[cfg(all(test, unix))]
            tests::interfere_with_cleanup(tests::CleanupCheckpoint::Traverse, quarantine, "owned");
            clear_open_directory(&candidate)?;
            // remove_open_dir_all may resolve a renamed directory's parent/name
            // again. Never let that final lookup recursively touch a replacement:
            // traversal above is rooted in the checked fd, final unlink is empty
            // directory only and relative to our already-opened quarantine.
            let entry = quarantine
                .open_dir_nofollow("owned")
                .map_err(|_| changed())?;
            if !same_directory(&entry, &self.dir)? {
                return Err(changed());
            }
            #[cfg(all(test, unix))]
            tests::interfere_with_cleanup(tests::CleanupCheckpoint::Unlink, quarantine, "owned");
            quarantine.remove_dir("owned")?;
            confirm_directory_removed(&self.dir, quarantine, "owned")?;
            // Removing a quarantine is also nonrecursive. A renamed private
            // parent must remain an unconfirmed cleanup rather than delete data.
            let entry = self.parent.open_dir_nofollow(&quarantine_name)?;
            if !same_directory(&entry, quarantine)? {
                return Err(changed());
            }
            self.parent.remove_dir(&quarantine_name)?;
            confirm_directory_removed(quarantine, &self.parent, &quarantine_name)?;
            self.quarantine.take();
            Ok(())
        }
    }
}

#[cfg(unix)]
fn confirm_directory_removed(dir: &Dir, parent: &Dir, name: impl AsRef<Path>) -> io::Result<()> {
    use cap_std::fs::MetadataExt;
    if dir.dir_metadata()?.nlink() == 0 {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        // Darwin filesystems can retain nlink=2 on an unlinked open directory. F_GETPATH is a
        // handle query and tracks renames while preserving the final name after
        // unlink. Require that final name to match our held parent plus entry,
        // and prove that entry is absent. A moved original remains unconfirmed.
        fn handle_path(dir: &Dir) -> io::Result<std::path::PathBuf> {
            use std::os::{fd::AsRawFd, unix::ffi::OsStringExt};
            unsafe extern "C" {
                fn fcntl(fd: i32, command: i32, ...) -> i32;
            }
            let mut bytes = [0u8; 1024];
            // SAFETY: dir holds this fd; F_GETPATH writes at most MAXPATHLEN
            // bytes into this live output buffer and does not reopen any path.
            if unsafe { fcntl(dir.as_raw_fd(), 50, bytes.as_mut_ptr()) } == -1 {
                return Err(io::Error::last_os_error());
            }
            let end = bytes
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(|| io::Error::other("invalid directory handle path"))?;
            Ok(std::ffi::OsString::from_vec(bytes[..end].to_vec()).into())
        }
        if handle_path(dir)? == handle_path(parent)?.join(name.as_ref()) {
            match parent.symlink_metadata(name) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
                Ok(_) => {}
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (parent, name);
    Err(io::Error::other(
        "Git metadata view directory removal is unconfirmed",
    ))
}

// Traverse only already-opened directories. A final name lookup may at worst
// fail/unlink an empty entry; it can never recurse through a replacement name.
// Identity and unlink evidence are checked at each level so a moved original
// stays an error even if a replacement happened to be empty.
#[cfg(unix)]
fn clear_open_directory(dir: &Dir) -> io::Result<()> {
    use cap_std::fs::MetadataExt;
    for entry in dir.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let metadata = dir.symlink_metadata(&name)?;
        if metadata.is_dir() {
            let child = dir.open_dir_nofollow(&name)?;
            let opened = child.dir_metadata()?;
            if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
                return Err(io::Error::other(
                    "Git metadata view child changed during cleanup",
                ));
            }
            clear_open_directory(&child)?;
            let current = dir.open_dir_nofollow(&name)?;
            if !same_directory(&current, &child)? {
                return Err(io::Error::other(
                    "Git metadata view child moved during cleanup",
                ));
            }
            dir.remove_dir(&name)?;
            confirm_directory_removed(&child, dir, &name)?;
        } else {
            // Unlink never follows symbolic links or recursively removes a
            // directory, even if an entry changes between stat and unlink.
            dir.remove_file(&name)?;
        }
    }
    Ok(())
}

fn private_directory(parent: &Dir, label: &str) -> io::Result<(String, Dir)> {
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    {
        use cap_std::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    for _ in 0..128 {
        let name = format!(
            "cayw-git-{label}-{}-{}",
            std::process::id(),
            NEXT_VIEW.fetch_add(1, Ordering::Relaxed)
        );
        match parent.create_dir_with(&name, &builder) {
            Ok(()) => return Ok((name.clone(), parent.open_dir_nofollow(&name)?)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other("unable to create private Git directory"))
}
impl GitView {
    fn new(runner: &ProcessRunner) -> Result<CleanupResource<Self>, CapabilityError> {
        let parent_path = std::env::temp_dir().canonicalize().map_err(io_error)?;
        let parent =
            Dir::open_ambient_dir(&parent_path, cap_std::ambient_authority()).map_err(io_error)?;
        let (name, dir) = private_directory(&parent, "view").map_err(io_error)?;
        Ok(runner.own_cleanup(Self {
            parent,
            name,
            dir,
            quarantine: None,
        }))
    }
}
struct MetadataBudget {
    bytes: usize,
    entries: usize,
}
/// Returns the current pathname of an already-open directory or file handle.
/// The query tracks renames without reopening the path; the caller still treats
/// the handle, not this string, as the authority.
#[cfg(target_os = "macos")]
fn handle_path(descriptor: std::os::fd::RawFd) -> io::Result<std::path::PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    unsafe extern "C" {
        fn fcntl(fd: i32, command: i32, ...) -> i32;
    }
    let mut bytes = [0u8; 1024];
    // SAFETY: the caller owns this live descriptor; F_GETPATH writes at most
    // MAXPATHLEN bytes into this output buffer and does not reopen any path.
    if unsafe { fcntl(descriptor, 50, bytes.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| io::Error::other("invalid handle path"))?;
    Ok(std::ffi::OsString::from_vec(bytes[..end].to_vec()).into())
}
#[cfg(target_os = "macos")]
fn directory_handle_path(dir: &Dir) -> Result<std::path::PathBuf, CapabilityError> {
    use std::os::fd::AsRawFd;
    handle_path(dir.as_raw_fd()).map_err(|_| failure("Git directory handle path is unavailable"))
}
#[cfg(not(target_os = "macos"))]
fn directory_handle_path(_: &Dir) -> Result<std::path::PathBuf, CapabilityError> {
    Err(failure(
        "Git read capabilities require a macOS directory handle path",
    ))
}
#[cfg(unix)]
fn directory_identity(dir: &Dir) -> Result<(u64, u64), CapabilityError> {
    use cap_std::fs::MetadataExt;
    let metadata = dir.dir_metadata().map_err(io_error)?;
    Ok((metadata.dev(), metadata.ino()))
}
#[cfg(not(unix))]
fn directory_identity(_: &Dir) -> Result<(u64, u64), CapabilityError> {
    Err(failure(
        "Git read capabilities require Unix directory identity",
    ))
}
/// Object stores linked from elsewhere would let a read consult ambient paths.
/// The real object store is used read-only, so refuse any alternate list
/// instead of following it.
fn check_object_alternates(objects: &Dir) -> Result<(), CapabilityError> {
    let info = match objects.open_dir_nofollow("info") {
        Ok(info) => info,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error(error)),
    };
    for name in ["alternates", "http-alternates"] {
        match info.symlink_metadata(name) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                let mut options = OpenOptions::new();
                options.read(true).follow(FollowSymlinks::No).nonblock(true);
                let file = info.open_with(name, &options).map_err(io_error)?;
                let mut bytes = vec![];
                file.take(4096).read_to_end(&mut bytes).map_err(io_error)?;
                if bytes.iter().any(|byte| !byte.is_ascii_whitespace()) {
                    return Err(failure(
                        "Git object alternates are not supported by read capabilities",
                    ));
                }
            }
            Ok(_) => {
                return Err(failure(
                    "Git objects/info must contain only regular files for read capabilities",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok(())
}
fn read_environment(
    objects: &Path,
    index: Option<&Path>,
) -> Result<BTreeMap<String, String>, CapabilityError> {
    let as_text = |path: &Path| {
        path.to_str()
            .map(str::to_string)
            .ok_or_else(|| failure("Git repository path is not UTF-8"))
    };
    let mut env = git_environment();
    env.insert("GIT_OBJECT_DIRECTORY".into(), as_text(objects)?);
    if let Some(index) = index {
        env.insert("GIT_INDEX_FILE".into(), as_text(index)?);
    }
    Ok(env)
}
fn copy_metadata(
    source: &Dir,
    target: &Dir,
    name: &str,
    required: bool,
    budget: &mut MetadataBudget,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(), CapabilityError> {
    check(cancel, deadline)?;
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let mut file = match source.open_with(name, &options) {
        Ok(file) => file,
        Err(error) if !required && error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error(error)),
    };
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err(failure("Git metadata must be regular files"));
    }
    let mut bytes = vec![];
    let mut chunk = [0; 8192];
    loop {
        check(cancel, deadline)?;
        let count = file.read(&mut chunk).map_err(io_error)?;
        if count == 0 {
            break;
        }
        if count > budget.bytes {
            return Err(failure("Git metadata byte budget exceeded"));
        }
        budget.bytes -= count;
        bytes.extend_from_slice(&chunk[..count]);
    }
    target.write(name, bytes).map_err(io_error)
}
fn copy_refs(
    source: &Dir,
    target: &Dir,
    budget: &mut MetadataBudget,
    cancel: &CancellationToken,
    deadline: Instant,
    depth: usize,
) -> Result<(), CapabilityError> {
    if depth > 32 {
        return Err(failure("Git ref directory depth exceeded"));
    }
    for entry in source.entries().map_err(io_error)? {
        check(cancel, deadline)?;
        if budget.entries == 0 {
            return Err(failure("Git ref entry limit exceeded"));
        }
        budget.entries -= 1;
        let entry = entry.map_err(io_error)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| failure("Git ref must have a UTF-8 name"))?;
        filesystem::components(&name, false)?;
        let metadata = source.symlink_metadata(&name).map_err(io_error)?;
        if metadata.is_dir() {
            let child = source.open_dir_nofollow(&name).map_err(io_error)?;
            target.create_dir(&name).map_err(io_error)?;
            let target = target.open_dir_nofollow(&name).map_err(io_error)?;
            copy_refs(&child, &target, budget, cancel, deadline, depth + 1)?;
        } else if metadata.is_file() {
            copy_metadata(source, target, &name, true, budget, cancel, deadline)?;
        } else {
            return Err(failure(
                "Git refs cannot contain symbolic links or special files",
            ));
        }
    }
    Ok(())
}
// Only these local settings affect a read without executing anything. The real
// config is parsed by Git itself from the private copy; every other key,
// including includes, filters, hooks and remotes, is never carried over.
#[derive(Default)]
struct SafeGitConfig {
    filemode: Option<bool>,
    autocrlf: Option<&'static str>,
    ignorecase: Option<bool>,
    precomposeunicode: Option<bool>,
    quotepath: Option<bool>,
    eol: Option<&'static str>,
}
/// Git's own boolean semantics: a key without `=` is implicit true, while an
/// explicit empty value (`key =`) is false. `git config --list -z` preserves the
/// difference as a missing versus present `\n` separator.
fn parse_git_bool(value: Option<&str>) -> Result<bool, CapabilityError> {
    match value {
        None => Ok(true),
        Some(value) => match value.trim().to_ascii_lowercase().as_str() {
            "" => Ok(false),
            "true" | "yes" | "on" | "1" => Ok(true),
            "false" | "no" | "off" | "0" => Ok(false),
            other => Err(failure(format!("unsupported Git boolean value {other}"))),
        },
    }
}
fn parse_git_autocrlf(value: Option<&str>) -> Result<&'static str, CapabilityError> {
    if value.is_some_and(|value| value.trim().eq_ignore_ascii_case("input")) {
        return Ok("input");
    }
    Ok(if parse_git_bool(value)? {
        "true"
    } else {
        "false"
    })
}
fn parse_git_eol(value: Option<&str>) -> Result<&'static str, CapabilityError> {
    match value.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "lf" => Ok("lf"),
        "crlf" => Ok("crlf"),
        "native" => Ok("native"),
        other => Err(failure(format!("unsupported Git core.eol value {other}"))),
    }
}
/// Parses `git config --file <private> --no-includes --list -z` output. Formats
/// and semantics unsupported by this SHA-1 metadata view are refused by name
/// instead of being silently treated as defaults.
fn parse_private_git_config(bytes: &[u8]) -> Result<SafeGitConfig, CapabilityError> {
    let mut config = SafeGitConfig::default();
    for record in bytes.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        let (key, value) = match record.iter().position(|byte| *byte == b'\n') {
            Some(position) => (&record[..position], Some(&record[position + 1..])),
            None => (record, None),
        };
        let key = std::str::from_utf8(key)
            .map_err(|_| failure("Git config keys must be UTF-8"))?
            .to_ascii_lowercase();
        let value = value
            .map(|value| std::str::from_utf8(value))
            .transpose()
            .map_err(|_| failure(format!("Git config value for {key} must be UTF-8")))?;
        if key == "core.repositoryformatversion" && value.unwrap_or("").trim() != "0" {
            return Err(failure(format!(
                "Git repository format version {} is not supported; only SHA-1 repositories at version 0 are supported",
                value.unwrap_or("").trim()
            )));
        }
        if key == "extensions.objectformat"
            && !value.unwrap_or("").trim().eq_ignore_ascii_case("sha1")
        {
            return Err(failure(format!(
                "Git object format {} is not supported; only SHA-1 repositories are supported",
                value.unwrap_or("").trim()
            )));
        }
        if key == "extensions.refstorage" {
            return Err(failure("Git reftable reference storage is not supported"));
        }
        if key == "extensions.worktreeconfig" {
            return Err(failure("Git per-worktree config is not supported"));
        }
        if matches!(
            key.as_str(),
            "core.sparsecheckout" | "core.sparsecheckoutcone"
        ) && parse_git_bool(value)?
        {
            return Err(failure("Git sparse checkout is not supported"));
        }
        match key.as_str() {
            "core.filemode" => config.filemode = Some(parse_git_bool(value)?),
            "core.autocrlf" => config.autocrlf = Some(parse_git_autocrlf(value)?),
            "core.ignorecase" => config.ignorecase = Some(parse_git_bool(value)?),
            "core.precomposeunicode" => {
                config.precomposeunicode = Some(parse_git_bool(value)?);
            }
            "core.quotepath" => config.quotepath = Some(parse_git_bool(value)?),
            "core.eol" => config.eol = Some(parse_git_eol(value)?),
            _ => {}
        }
    }
    Ok(config)
}
fn sanitized_git_config(config: &SafeGitConfig) -> String {
    let mut text = String::from("[core]\n\trepositoryformatversion = 0\n\tbare = false\n");
    for (key, value) in [
        ("filemode", config.filemode.map(|value| value.to_string())),
        ("autocrlf", config.autocrlf.map(str::to_string)),
        (
            "ignorecase",
            config.ignorecase.map(|value| value.to_string()),
        ),
        (
            "precomposeunicode",
            config.precomposeunicode.map(|value| value.to_string()),
        ),
        ("quotepath", config.quotepath.map(|value| value.to_string())),
        ("eol", config.eol.map(str::to_string)),
    ] {
        if let Some(value) = value {
            text.push_str(&format!("\t{key} = {value}\n"));
        }
    }
    text
}
/// Runs one fixed Git inspection command inside the private view through the
/// original Device runner. The caller supplies a literal argv; no ambient path,
/// shell or repository is consulted.
async fn run_git_probe(
    host: &Host,
    view: &GitView,
    args: Vec<String>,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Vec<u8>, CapabilityError> {
    check(cancel, deadline)?;
    let timeout = host
        .config
        .process_timeout
        .min(deadline.saturating_duration_since(Instant::now()));
    if timeout.is_zero() {
        return Err(failure(
            "capability deadline reached before Git metadata view inspection",
        ));
    }
    let spec = DirectoryProcessSpec {
        program: host
            .config
            .git_program
            .clone()
            .expect("git catalog configured"),
        args,
        cwd: view.dir.try_clone().map_err(io_error)?.into_std_file(),
        env: git_environment(),
        timeout,
        max_output_bytes: host.config.max_process_output_bytes,
        io: cayw_device::ProcessIo::Pipes,
    };
    let result = host
        .runner
        .run_in_directory(spec, cancel.clone())
        .await
        .map_err(device_error)?;
    if result.termination != TerminationReason::Exited || !result.output_complete {
        return Err(failure(
            "Git metadata view inspection stopped before a complete result",
        ));
    }
    if !result.status.success() {
        return Err(failure(format!(
            "Git metadata view inspection failed with exit {:?}",
            result.status.code()
        )));
    }
    Ok(result.stdout)
}
/// Read capabilities use a private, sanitized Git directory over the held root.
/// Only bounded passive metadata (config, HEAD, refs, packed refs, shallow
/// state and shared index segments) is copied. Object data and ordinary
/// worktree files are read in place relative to the held cwd, so repository
/// size and ignored build data do not gate a read. Original repository, object
/// and index identities stay open and are checked after the process returns.
/// A sanitized config prevents hooks, filters, credentials, external diff and
/// fsmonitor from running. Replaced identities discard the result.
struct GitReadIdentity {
    git: Dir,
    objects: Dir,
    index: Option<cap_std::fs::File>,
}

impl GitReadIdentity {
    fn verify(&self, host: &Host) -> Result<(), CapabilityError> {
        let git = host
            .root
            .open_dir_nofollow(".git")
            .map_err(|_| failure("Git repository disappeared during a read; result discarded"))?;
        if !same_directory(&git, &self.git).map_err(io_error)? {
            return Err(failure(
                "Git repository identity changed during a read; result discarded",
            ));
        }
        let objects = git
            .open_dir_nofollow("objects")
            .map_err(|_| failure("Git object store disappeared during a read; result discarded"))?;
        if directory_identity(&objects)? != directory_identity(&self.objects)? {
            return Err(failure(
                "Git object store identity changed during a read; result discarded",
            ));
        }
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No).nonblock(true);
        let current = match git.open_with("index", &options) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(_) => return Err(failure("Git index changed during a read; result discarded")),
        };
        match (&self.index, current) {
            (None, None) => Ok(()),
            (Some(original), Some(current)) => {
                use cap_std::fs::MetadataExt;
                let original = original.metadata().map_err(io_error)?;
                let current = current.metadata().map_err(io_error)?;
                if current.is_file()
                    && original.dev() == current.dev()
                    && original.ino() == current.ino()
                {
                    Ok(())
                } else {
                    Err(failure(
                        "Git index identity changed during a read; result discarded",
                    ))
                }
            }
            _ => Err(failure(
                "Git index presence changed during a read; result discarded",
            )),
        }
    }
}

async fn git_spec(
    host: &Host,
    name: &str,
    arguments: &Value,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<
    (
        DirectoryProcessSpec,
        CleanupResource<GitView>,
        GitReadIdentity,
    ),
    CapabilityError,
> {
    // Git's local config can run clean filters even for status/diff. Copy it into
    // the private view, parse it with Git itself from that fixed private file
    // without includes, and carry over only explicitly passive keys. The real
    // repository config, hooks and filters never run or enter the view.
    let source = host.root.open_dir_nofollow(".git").map_err(|_| {
        failure("git capabilities require a direct .git directory in the workspace")
    })?;
    let objects = source.open_dir_nofollow("objects").map_err(io_error)?;
    check_object_alternates(&objects)?;
    if let Some(path) = arguments.get("path").and_then(Value::as_str) {
        filesystem::validate_git_path(&host.root, path)?;
    }
    // Relative paths resolve from Device's held cwd even if its ambient name
    // is replaced. Keep opened identities until after Git returns; optional
    // locks prevent this read from refreshing the real index.
    let index = {
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No).nonblock(true);
        match source.open_with("index", &options) {
            Ok(file) => {
                if !file.metadata().map_err(io_error)?.is_file() {
                    return Err(failure("Git index must be a regular file"));
                }
                Some(file)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(io_error(error)),
        }
    };
    let identity = GitReadIdentity {
        git: source.try_clone().map_err(io_error)?,
        objects,
        index,
    };
    let view = GitView::new(&host.runner)?;
    view.dir.create_dir(".git").map_err(io_error)?;
    let git_dir = view.dir.open_dir_nofollow(".git").map_err(io_error)?;
    git_dir.create_dir("refs").map_err(io_error)?;
    // Git checks that its private git-dir contains an objects entry before
    // consulting GIT_OBJECT_DIRECTORY, even though the actual objects live in
    // the held repository.
    git_dir.create_dir("objects").map_err(io_error)?;
    let mut budget = MetadataBudget {
        bytes: host.config.max_file_bytes,
        entries: host.config.max_list_entries,
    };
    copy_metadata(
        &source,
        &git_dir,
        "config",
        false,
        &mut budget,
        cancel,
        deadline,
    )?;
    let config = match git_dir.symlink_metadata("config") {
        Ok(metadata) if metadata.is_file() => {
            let parsed = run_git_probe(
                host,
                &view,
                vec![
                    "config".into(),
                    "--file".into(),
                    ".git/config".into(),
                    "--no-includes".into(),
                    "--list".into(),
                    "-z".into(),
                ],
                cancel,
                deadline,
            )
            .await?;
            parse_private_git_config(&parsed)?
        }
        Ok(_) => return Err(failure("Git config must be a regular file")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => SafeGitConfig::default(),
        Err(error) => return Err(io_error(error)),
    };
    git_dir
        .write("config", sanitized_git_config(&config).into_bytes())
        .map_err(io_error)?;
    copy_metadata(
        &source,
        &git_dir,
        "HEAD",
        true,
        &mut budget,
        cancel,
        deadline,
    )?;
    for name in ["packed-refs", "shallow"] {
        copy_metadata(
            &source,
            &git_dir,
            name,
            false,
            &mut budget,
            cancel,
            deadline,
        )?;
    }
    // Shared index files and local exclude patterns are passive data. Config,
    // hooks, info/attributes and alternate commands never enter this view.
    for entry in source.entries().map_err(io_error)? {
        check(cancel, deadline)?;
        if budget.entries == 0 {
            return Err(failure("Git metadata entry limit exceeded"));
        }
        budget.entries -= 1;
        let entry = entry.map_err(io_error)?;
        let filename = entry.file_name();
        let Some(filename) = filename.to_str() else {
            continue;
        };
        if let Some(digest) = filename.strip_prefix("sharedindex.")
            && digest.len() == 40
            && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            copy_metadata(
                &source,
                &git_dir,
                filename,
                true,
                &mut budget,
                cancel,
                deadline,
            )?;
        }
    }
    // Only real Host state is hidden from Git's view of the worktree. Ordinary
    // user names that merely start with `.cayw-` (such as `.cayw-note.md`) are
    // ordinary tracked or untracked files and never block a read.
    git_dir
        .write(
            "cayw-excludes",
            filesystem::git_internal_excludes().as_bytes(),
        )
        .map_err(io_error)?;
    let excludes_path = directory_handle_path(&git_dir)?.join("cayw-excludes");
    let excludes = excludes_path
        .to_str()
        .ok_or_else(|| failure("Git excludes path is not UTF-8"))?
        .to_string();
    match source.open_dir_nofollow("info") {
        Ok(info) => {
            git_dir.create_dir("info").map_err(io_error)?;
            let target = git_dir.open_dir_nofollow("info").map_err(io_error)?;
            copy_metadata(
                &info,
                &target,
                "exclude",
                false,
                &mut budget,
                cancel,
                deadline,
            )?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error(error)),
    }
    let refs = source.open_dir_nofollow("refs").map_err(io_error)?;
    let target_refs = git_dir.open_dir_nofollow("refs").map_err(io_error)?;
    copy_refs(&refs, &target_refs, &mut budget, cancel, deadline, 0)?;
    let git_dir_path = directory_handle_path(&git_dir)?;
    let mut args = git_args(name, arguments, &excludes);
    args.insert(0, "--work-tree=.".into());
    args.insert(
        0,
        format!(
            "--git-dir={}",
            git_dir_path
                .to_str()
                .ok_or_else(|| failure("Git view path is not UTF-8"))?
        ),
    );
    Ok((
        DirectoryProcessSpec {
            program: host
                .config
                .git_program
                .clone()
                .expect("git catalog configured"),
            args,
            cwd: host.root.try_clone().map_err(io_error)?.into_std_file(),
            env: read_environment(Path::new(".git/objects"), Some(Path::new(".git/index")))?,
            timeout: host.config.process_timeout,
            max_output_bytes: host.config.max_process_output_bytes,
            io: cayw_device::ProcessIo::Pipes,
        },
        view,
        identity,
    ))
}
fn profile_spec(
    host: &Host,
    name: &str,
    arguments: &Value,
) -> Result<DirectoryProcessSpec, CapabilityError> {
    let profile = host
        .config
        .profiles
        .get(arguments["profile"].as_str().expect("validated profile"))
        .ok_or_else(|| failure("profile is not configured"))?;
    let expected_kind = match name {
        "build.run" => ProfileKind::Build,
        "test.run" => ProfileKind::Test,
        "check.run" => ProfileKind::Check,
        "lint.run" => ProfileKind::Lint,
        "format.run" => ProfileKind::Format,
        _ => return Err(failure("unknown profile capability")),
    };
    if profile.kind != expected_kind {
        return Err(failure("profile does not match capability"));
    }
    let mut args = profile.args.clone();
    if let Some(filter) = arguments.get("filter").and_then(Value::as_str) {
        if !profile.allow_filter || !valid_filter(filter) {
            return Err(failure("test filter is not allowed"));
        }
        args.push(filter.into());
    }
    Ok(DirectoryProcessSpec {
        program: profile.program.clone(),
        args,
        env: profile.env.clone(),
        cwd: host.root.try_clone().map_err(io_error)?.into_std_file(),
        timeout: profile.timeout.min(host.config.process_timeout),
        max_output_bytes: profile
            .max_output_bytes
            .min(host.config.max_process_output_bytes),
        io: profile.io,
    })
}
pub(crate) fn device_error(error: DeviceError) -> CapabilityError {
    match error {
        DeviceError::NotStarted { .. }
        | DeviceError::InvalidSpec(_)
        | DeviceError::ProgramNotAllowed(_)
        | DeviceError::Cancelled
        | DeviceError::RunnerClosed
        | DeviceError::ResourcesBusy
        | DeviceError::ResourceRequestTooLarge
        | DeviceError::UnsupportedResource(_)
        | DeviceError::InvalidLimits(_)
        | DeviceError::InvalidSessionId
        | DeviceError::UnsupportedPlatform => failure(error.to_string()),
        // A supervisor or I/O failure may happen after a process was launched.
        // Without a terminal ProcessResult its effects must be reconciled.
        _ => CapabilityError::unknown(error.to_string()),
    }
}

pub(crate) async fn execute(
    host: &Host,
    name: &str,
    arguments: &Value,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Value, CapabilityError> {
    match name {
        "git.status" | "git.diff" | "git.log" | "git.show" | "git.blame" => {
            execute_git_read(host, name, arguments, cancel, deadline).await
        }
        "build.run" | "test.run" | "check.run" | "lint.run" | "format.run" => {
            check(cancel, deadline)?;
            if host.runner.is_closed() {
                return Err(failure("process runner is closed"));
            }
            let mut spec = profile_spec(host, name, arguments)?;
            check(cancel, deadline)?;
            spec.timeout = spec
                .timeout
                .min(deadline.saturating_duration_since(Instant::now()));
            if spec.timeout.is_zero() {
                return Err(failure("capability deadline reached before process launch"));
            }
            let result = host
                .runner
                .run_in_directory_with_resources(spec, cancel.clone(), None::<()>)
                .await
                .map_err(device_error)?;
            process_value(result, output_filter_options(host, name))
        }
        _ => Err(failure("unknown process capability")),
    }
}

/// Read-only Git uses a bounded private metadata view and runs in the held root.
/// Objects and the index are read in place relative to the held cwd; opened
/// identities are retained and rechecked. Local config, hooks, filters and
/// object alternates cannot run or redirect a read.
pub(crate) async fn execute_git_read(
    host: &Host,
    name: &str,
    arguments: &Value,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Value, CapabilityError> {
    check(cancel, deadline)?;
    if host.runner.is_closed() {
        return Err(failure("process runner is closed"));
    }
    let (mut spec, git_view, identity) = git_spec(host, name, arguments, cancel, deadline).await?;
    check(cancel, deadline)?;
    spec.timeout = spec
        .timeout
        .min(deadline.saturating_duration_since(Instant::now()));
    if spec.timeout.is_zero() {
        return Err(failure("capability deadline reached before process launch"));
    }
    let result = host
        .runner
        .run_in_directory_with_resources(spec, cancel.clone(), git_view)
        .await
        .map_err(device_error)?;
    // Preserve an uncertain process terminal before reporting a read conflict.
    // A completed read with a replaced repository or index discards its data.
    let result = process_value(result, output_filter_options(host, name))?;
    identity.verify(host)?;
    Ok(result)
}

/// Resolve host filter options for one capability. Profile auto-selects for
/// known process/git shapes; other fields come from `HostConfig::output_filter`.
///
/// Structured git records (`blame` / `log` / `show`) use Passthrough with no
/// line cap so Generic head/tail selection cannot split porcelain records.
fn output_filter_options(
    host: &Host,
    capability: &str,
) -> Option<cayw_output_filter::FilterOptions> {
    let mut options = host.config.output_filter.clone()?;
    if options.enabled {
        match capability {
            "build.run" | "test.run" | "check.run" | "lint.run" | "format.run" => {
                options.profile = cayw_output_filter::FilterProfile::CargoLike;
            }
            "git.status" => {
                options.profile = cayw_output_filter::FilterProfile::GitStatusLike;
            }
            // Line-porcelain blame and commit-oriented log/show must stay
            // parseable; destructive Generic selection drops mid-record lines
            // (e.g. first blamed content line) and breaks Runtime git tests.
            "git.blame" | "git.log" | "git.show" => {
                options.profile = cayw_output_filter::FilterProfile::Passthrough;
                options.max_lines = 0;
            }
            _ => {}
        }
    }
    Some(options)
}

fn stream_filter_fields(
    original: &str,
    options: &cayw_output_filter::FilterOptions,
) -> Result<(String, Value), CapabilityError> {
    let filtered = cayw_output_filter::apply_filter(original, options.clone())
        .map_err(|error| failure(format!("output filter failed: {error}")))?;
    let dual = cayw_output_filter::dual_entry(&filtered);
    let meta = json!({
        "original_digest": dual.original_digest,
        "filter_enabled": dual.filter_enabled,
        "applied_profile": dual.applied_profile,
        "original_inline": false,
        "reduction": {
            "original_bytes": filtered.reduction.original_bytes,
            "filtered_bytes": filtered.reduction.filtered_bytes,
            "original_lines": filtered.reduction.original_lines,
            "filtered_lines": filtered.reduction.filtered_lines,
            "changed": filtered.reduction.changed,
        },
    });
    // Agent-facing payload is the summary only. Full original stays in the
    // filter crate / dual_entry for offline compare; Host must not re-embed it
    // inline (that would cancel context reduction). Oversized bodies may still
    // be externalized by Runtime ContentStore auto-externalize (F01).
    Ok((dual.summary, meta))
}

fn process_value(
    result: cayw_device::ProcessResult,
    filter: Option<cayw_output_filter::FilterOptions>,
) -> Result<Value, CapabilityError> {
    if result.termination != TerminationReason::Exited || !result.output_complete {
        return Err(CapabilityError::unknown(format!(
            "process stopped with {:?}; exit={:?}, stdout_bytes={}, stderr_bytes={}, complete={}",
            result.termination,
            result.status.code(),
            result.stdout.len(),
            result.stderr.len(),
            result.output_complete
        )));
    }
    // Text is lossy only at this JSON presentation boundary. Device byte budgets
    // are checked before allocation; the JSON result budget is checked afterward.
    let stdout_raw = String::from_utf8_lossy(&result.stdout).into_owned();
    let stderr_raw = String::from_utf8_lossy(&result.stderr).into_owned();
    let mut value = json!({
        "success": result.status.success(),
        "exit_code": result.status.code(),
        "stdout": stdout_raw.clone(),
        "stderr": stderr_raw.clone(),
        "termination": "exited",
        "output_complete": result.output_complete,
    });
    if let Some(options) = filter.filter(|options| options.enabled) {
        let (stdout, stdout_meta) = stream_filter_fields(&stdout_raw, &options)?;
        let (stderr, stderr_meta) = stream_filter_fields(&stderr_raw, &options)?;
        value["stdout"] = json!(stdout);
        value["stderr"] = json!(stderr);
        // Deliberately omit stdout_original/stderr_original from the capability
        // result so the model context receives only the filtered summary (+ meta).
        value["output_filter"] = json!({
            "protocol_version": cayw_output_filter::OUTPUT_FILTER_PROTOCOL_VERSION,
            "stdout": stdout_meta,
            "stderr": stderr_meta,
        });
    }
    // A normal nonzero exit is a confirmed terminal result, not an unknown
    // effect and not an assertion that a build has rolled back its outputs.
    Ok(value)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{AuditSink, PermissionGate};
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::time::Duration;

    #[derive(Clone, Copy, PartialEq)]
    pub(super) enum CleanupCheckpoint {
        Rename,
        Traverse,
        Unlink,
    }
    struct CleanupInterference {
        checkpoint: CleanupCheckpoint,
        outside: Dir,
        replace: bool,
        replacement: Arc<std::sync::Mutex<Option<Dir>>>,
    }
    thread_local! { static CLEANUP_INTERFERENCE: std::cell::RefCell<Option<CleanupInterference>> = const { std::cell::RefCell::new(None) }; }
    pub(super) fn interfere_with_cleanup(checkpoint: CleanupCheckpoint, parent: &Dir, name: &str) {
        CLEANUP_INTERFERENCE.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot
                .as_ref()
                .is_some_and(|state| state.checkpoint == checkpoint)
            {
                let state = slot.take().unwrap();
                parent
                    .rename(name, &state.outside, "moved-original")
                    .unwrap();
                if state.replace {
                    parent.create_dir(name).unwrap();
                    let replacement = parent.open_dir_nofollow(name).unwrap();
                    replacement
                        .write("third-party-marker", b"preserve")
                        .unwrap();
                    *state.replacement.lock().unwrap() = Some(replacement);
                }
            }
        });
    }

    #[tokio::test]
    async fn cleanup_swaps_after_identity_checks_never_delete_replacements_or_confirm_moved_originals()
     {
        for checkpoint in [
            CleanupCheckpoint::Rename,
            CleanupCheckpoint::Traverse,
            CleanupCheckpoint::Unlink,
        ] {
            for replace in [false, true] {
                let outside = Tree::new();
                let outside_dir =
                    Dir::open_ambient_dir(&outside.0, cap_std::ambient_authority()).unwrap();
                let runner = ProcessRunner::new([]).unwrap();
                let view = GitView::new(&runner).unwrap();
                let parent = view.parent.try_clone().unwrap();
                let name = view.name.clone();
                let replacement = Arc::new(std::sync::Mutex::new(None));
                CLEANUP_INTERFERENCE.with(|slot| {
                    *slot.borrow_mut() = Some(CleanupInterference {
                        checkpoint,
                        outside: outside_dir.try_clone().unwrap(),
                        replace,
                        replacement: replacement.clone(),
                    })
                });
                drop(view);
                CLEANUP_INTERFERENCE.with(|slot| assert!(slot.borrow().is_none()));
                assert!(runner.cleanup_failure().is_some());
                assert!(runner.is_closed());
                assert!(runner.drain().await.is_err());
                assert_eq!(runner.active_count(), 0);
                assert!(outside_dir.open_dir_nofollow("moved-original").is_ok());
                if replace {
                    let marker = replacement
                        .lock()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .read("third-party-marker")
                        .unwrap();
                    assert_eq!(marker, b"preserve");
                }
                if parent.symlink_metadata(&name).is_ok() {
                    parent.remove_dir_all(&name).unwrap();
                }
            }
        }
    }

    struct Boundary;
    #[async_trait]
    impl PermissionGate for Boundary {
        async fn authorize(
            &self,
            _: &str,
            _: &Value,
            _: CancellationToken,
        ) -> Result<(), CapabilityError> {
            Ok(())
        }
    }
    #[async_trait]
    impl AuditSink for Boundary {
        async fn started(&self, _: &str, _: &Value) -> Result<String, CapabilityError> {
            Ok("fixture".into())
        }
        async fn settled(
            &self,
            _: &str,
            _: &Result<Value, CapabilityError>,
        ) -> Result<(), CapabilityError> {
            Ok(())
        }
        fn dropped(&self, _: &str) -> Result<(), CapabilityError> {
            Ok(())
        }
    }
    struct Tree(std::path::PathBuf);
    impl Tree {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "cayw-host-metadata-view-{}-{}",
                std::process::id(),
                NEXT_VIEW.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn git(root: &Path, args: &[&str]) -> String {
        let result = std::process::Command::new("/usr/bin/git")
            .args(args)
            .current_dir(root)
            .env_clear()
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap()
    }
    fn host(root: &Path) -> Host {
        Host::new(
            root,
            Arc::new(Boundary),
            Arc::new(Boundary),
            HostConfig {
                git_program: Some("/usr/bin/git".into()),
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn prepared_profile_survives_root_move_and_replacement_without_path_lookup() {
        for link in [false, true] {
            let container = Tree::new();
            let root = container.0.join("root");
            let moved = container.0.join("moved");
            let replacement = container.0.join("replacement");
            std::fs::create_dir(&root).unwrap();
            std::fs::create_dir(&replacement).unwrap();
            std::fs::write(root.join("marker"), "opened").unwrap();
            std::fs::write(replacement.join("marker"), "replacement").unwrap();
            let host = Host::new(
                &root,
                Arc::new(Boundary),
                Arc::new(Boundary),
                HostConfig {
                    profiles: BTreeMap::from([(
                        "read".into(),
                        crate::Profile {
                            kind: ProfileKind::Check,
                            program: "/bin/cat".into(),
                            args: vec!["marker".into()],
                            env: BTreeMap::new(),
                            timeout: Duration::from_secs(10),
                            max_output_bytes: 1024,
                            allow_filter: false,
                            io: cayw_device::ProcessIo::Pipes,
                        },
                    )]),
                    ..Default::default()
                },
            )
            .unwrap();
            let spec = profile_spec(&host, "check.run", &json!({"profile":"read"})).unwrap();
            std::fs::rename(&root, &moved).unwrap();
            if link {
                std::os::unix::fs::symlink(&replacement, &root).unwrap();
            } else {
                std::fs::rename(&replacement, &root).unwrap();
            }
            let result = host
                .runner
                .run_in_directory(spec, CancellationToken::new())
                .await
                .unwrap();
            assert!(result.status.success());
            assert_eq!(result.stdout, b"opened");
        }
    }

    #[tokio::test]
    async fn renamed_private_metadata_view_is_retained_and_cleanup_stays_unconfirmed() {
        let container = Tree::new();
        git(&container.0, &["init", "--quiet"]);
        let host = host(&container.0);
        let cancel = CancellationToken::new();
        let (spec, view, _) = git_spec(
            &host,
            "git.status",
            &json!({}),
            &cancel,
            Instant::now() + Duration::from_secs(10),
        )
        .await
        .unwrap();
        let moved = format!("{}-moved", view.name);
        let owner = view.parent.try_clone().unwrap();
        owner.rename(&view.name, &owner, &moved).unwrap();
        let result = host
            .runner
            .run_in_directory_with_resources(spec, cancel, view)
            .await;
        assert!(matches!(result, Err(DeviceError::CleanupUnconfirmed(_))));
        assert!(host.drain_processes().await.is_err());
        assert!(host.outcome_unknown());
        assert!(
            owner
                .open_dir_nofollow(&moved)
                .unwrap()
                .open_dir_nofollow(".git")
                .is_ok()
        );
        // Explicit test reconciliation; production retains the opened metadata view.
        owner.remove_dir_all(&moved).unwrap();
    }

    #[tokio::test]
    async fn prepared_git_read_never_redirects_to_replacement_root_subtree_or_object_store() {
        for replacement in ["root", "subtree", "objects"] {
            let container = Tree::new();
            let root = container.0.join("root");
            let moved = container.0.join("moved");
            std::fs::create_dir(&root).unwrap();
            git(&root, &["init", "--quiet"]);
            std::fs::create_dir(root.join("sub")).unwrap();
            std::fs::write(root.join("sub/note"), "before\n").unwrap();
            git(&root, &["add", "sub/note"]);
            git(&root, &["commit", "--quiet", "-m", "fixture"]);
            let commit = git(&root, &["rev-parse", "HEAD"]).trim().to_string();
            std::fs::write(root.join("sub/note"), "opened\n").unwrap();
            let host = host(&root);
            let cancel = CancellationToken::new();
            let deadline = Instant::now() + Duration::from_secs(10);
            let (diff_spec, diff_view, identity) = git_spec(
                &host,
                "git.diff",
                &json!({"path":"sub/note"}),
                &cancel,
                deadline,
            )
            .await
            .unwrap();
            let (show_spec, show_view, _) = git_spec(
                &host,
                "git.show",
                &json!({"commit":commit,"path":"sub/note"}),
                &cancel,
                deadline,
            )
            .await
            .unwrap();
            match replacement {
                "root" => {
                    std::fs::rename(&root, &moved).unwrap();
                    std::fs::create_dir(&root).unwrap();
                    std::fs::write(root.join("wrong-root"), "replacement").unwrap();
                }
                "subtree" => {
                    std::fs::rename(root.join("sub"), &moved).unwrap();
                    let outside = container.0.join("outside");
                    std::fs::create_dir(&outside).unwrap();
                    std::fs::write(outside.join("note"), "replacement\n").unwrap();
                    std::os::unix::fs::symlink(&outside, root.join("sub")).unwrap();
                }
                "objects" => {
                    std::fs::rename(root.join(".git/objects"), &moved).unwrap();
                    let outside = container.0.join("outside");
                    std::fs::create_dir(&outside).unwrap();
                    std::os::unix::fs::symlink(&outside, root.join(".git/objects")).unwrap();
                }
                _ => unreachable!(),
            }
            let diff = host
                .runner
                .run_in_directory_with_resources(diff_spec, cancel.clone(), diff_view)
                .await;
            if let Ok(result) = &diff {
                let text = format!(
                    "{}{}",
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr)
                );
                assert!(
                    !text.contains("replacement") && !text.contains("wrong-root"),
                    "{replacement}: read crossed into replacement content: {text}"
                );
            }
            let shown = host
                .runner
                .run_in_directory_with_resources(show_spec, cancel, show_view)
                .await;
            if let Ok(result) = &shown {
                let text = format!(
                    "{}{}",
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr)
                );
                assert!(
                    !text.contains("replacement") && !text.contains("wrong-root"),
                    "{replacement}: history crossed into replacement content: {text}"
                );
            }
            if replacement == "objects" {
                // The object path was replaced after preparation; the result must
                // be discarded rather than reported as this repository's history.
                assert!(identity.verify(&host).is_err());
            }
        }
    }
}
