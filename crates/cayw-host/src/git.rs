//! Host-owned Git capabilities.
//!
//! Read commands use a bounded private metadata view built by `process`. Write
//! commands operate on the real repository held by this host's root after a
//! stricter structural check: a direct `.git` directory (never a symlink or a
//! `gitdir:` file), no alternates or common directory, no dangerous local config,
//! and hooks, filters, external diff/pager/credential programs disabled. Every
//! command is a fixed argv launched through the shared Device runner. No command
//! pushes or opens a network protocol.

use crate::{Host, HostConfig, check, failure, filesystem, io_error};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, OpenOptions};
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::CapabilityError;
use cayw_device::{DirectoryProcessSpec, ProcessIo, ProcessResult, TerminationReason};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_STAGE_PATHS: usize = 256;
const WORKTREE_ROOT: &str = ".cayw-worktrees";
const MAX_OUTPUT_DETAIL: usize = 8192;

/// Host-selected policy for real-repository writes. Without this value the
/// catalog contains no Git write capability. The identity is applied by the
/// host; no model argument can override author or committer.
#[derive(Clone, Debug)]
pub struct GitWritePolicy {
    pub identity_name: String,
    pub identity_email: String,
    pub allow_stage: bool,
    pub allow_commit: bool,
    pub allow_branch: bool,
    pub allow_worktree: bool,
}

#[derive(Clone, Copy)]
enum WriteOp {
    Stage,
    Commit,
    Branch,
    Worktree,
}

impl WriteOp {
    fn enabled(self, policy: &GitWritePolicy) -> bool {
        match self {
            Self::Stage => policy.allow_stage,
            Self::Commit => policy.allow_commit,
            Self::Branch => policy.allow_branch,
            Self::Worktree => policy.allow_worktree,
        }
    }
}

fn definition(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        parameters: json!({
            "type":"object", "properties":properties, "required":required, "additionalProperties":false
        }),
    }
}
fn string(max: usize) -> Value {
    json!({"type":"string", "minLength":1, "maxLength":max})
}
fn path_value() -> Value {
    string(4096)
}
fn object_id() -> Value {
    json!({"type":"string", "pattern":"^([0-9a-fA-F]{40}|[0-9a-fA-F]{64})$"})
}
fn branch_value() -> Value {
    json!({"type":"string", "minLength":1, "maxLength":255})
}
fn worktree_name_value() -> Value {
    string(255)
}

pub(crate) fn validate_config(config: &HostConfig) -> Result<(), CapabilityError> {
    let Some(policy) = &config.git_writes else {
        return Ok(());
    };
    if config.git_program.is_none() {
        return Err(failure(
            "Git write policy requires a configured git program",
        ));
    }
    validate_identity(&policy.identity_name, &policy.identity_email)
}

fn validate_identity(name: &str, email: &str) -> Result<(), CapabilityError> {
    if name.is_empty()
        || name.len() > 256
        || email.is_empty()
        || email.len() > 320
        || name.contains(['<', '>'])
        || email.contains(['<', '>'])
        || name.chars().any(char::is_control)
        || email.chars().any(char::is_control)
    {
        return Err(failure("invalid host Git identity"));
    }
    Ok(())
}

pub(crate) fn definitions(config: &HostConfig) -> Vec<ToolDefinition> {
    if config.git_program.is_none() {
        return vec![];
    }
    let mut definitions = vec![
        definition(
            "git.status",
            "Read repository porcelain status with optional locks and hooks disabled.",
            json!({}),
            &[],
        ),
        definition(
            "git.diff",
            "Read bounded diff with external diff and text conversion disabled.",
            json!({"path":path_value(), "staged":{"type":"boolean"}}),
            &[],
        ),
        definition(
            "git.log",
            "Read a bounded commit log without decorations or signature commands.",
            json!({"limit":{"type":"integer", "minimum":1, "maximum":100}}),
            &[],
        ),
        definition(
            "git.show",
            "Read one commit or file from a full commit object ID.",
            json!({"commit":object_id(), "path":path_value()}),
            &["commit"],
        ),
        definition(
            "git.blame",
            "Read line attribution through the private metadata view for one workspace file, optionally from a full commit object ID.",
            json!({"path":path_value(), "commit":object_id()}),
            &["path"],
        ),
    ];
    let Some(policy) = &config.git_writes else {
        return definitions;
    };
    if policy.allow_stage {
        definitions.push(definition(
            "git.stage",
            "Stage exact workspace file paths after verifying HEAD and the current index tree. No implicit pathspec expansion.",
            json!({
                "paths":{"type":"array", "items":path_value(), "minItems":1, "maxItems":MAX_STAGE_PATHS, "uniqueItems":true},
                "expected_head":object_id(),
                "expected_index_tree":object_id()
            }),
            &["paths", "expected_head", "expected_index_tree"],
        ));
    }
    if policy.allow_commit {
        definitions.push(definition(
            "git.commit",
            "Commit the verified index with host-owned author and committer identity. Hooks and signing are disabled.",
            json!({
                "message":{"type":"string", "minLength":1, "maxLength":MAX_MESSAGE_BYTES},
                "expected_head":object_id(),
                "expected_index_tree":object_id()
            }),
            &["message", "expected_head", "expected_index_tree"],
        ));
    }
    if policy.allow_branch {
        definitions.push(definition(
            "git.branch",
            "Create or safely delete one local branch at an exact commit while verifying the current HEAD.",
            json!({
                "action":{"type":"string", "enum":["create","delete"]},
                "name":branch_value(),
                "commit":object_id(),
                "expected_head":object_id()
            }),
            &["action", "name", "commit", "expected_head"],
        ));
    }
    if policy.allow_worktree {
        definitions.push(definition(
            "git.worktree",
            "Create or remove one linked worktree under the fixed in-root .cayw-worktrees directory. No force and no external path.",
            json!({
                "action":{"type":"string", "enum":["create","remove"]},
                "name":worktree_name_value(),
                "branch":branch_value(),
                "commit":object_id(),
                "expected_head":object_id()
            }),
            &["action", "name", "expected_head"],
        ));
    }
    definitions
}

fn policy_for(config: &HostConfig, op: WriteOp) -> Result<&GitWritePolicy, CapabilityError> {
    let policy = config
        .git_writes
        .as_ref()
        .ok_or_else(|| failure("Git write capabilities are not configured"))?;
    if !op.enabled(policy) {
        return Err(failure("Git write operation is not enabled by host policy"));
    }
    Ok(policy)
}

fn object_id_value(value: Option<&Value>, field: &str) -> Result<String, CapabilityError> {
    let value = value
        .and_then(Value::as_str)
        .ok_or_else(|| failure(format!("{field} must be a full object ID")))?;
    if !valid_object_id(value) {
        return Err(failure(format!(
            "{field} must be a full 40 or 64 character hexadecimal object ID"
        )));
    }
    // Git prints object IDs in lowercase. Normalize validated model input so a
    // valid uppercase spelling cannot be mistaken for a different object.
    Ok(value.to_ascii_lowercase())
}

fn valid_object_id(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Accept exactly the names Git itself accepts as a local branch short name,
/// plus the containment and option-injection rules this Host needs. Characters
/// outside ASCII are valid; Git's own ref-format exclusions are not.
fn valid_branch_name(name: &str) -> bool {
    if name.is_empty()
        || name.chars().count() > 255
        || name == "HEAD"
        || name.starts_with('-')
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.contains("..")
        || name.contains("@{")
        || name.contains("//")
    {
        return false;
    }
    if name.chars().any(|character| {
        character.is_ascii_control()
            || matches!(character, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\')
    }) {
        return false;
    }
    for component in name.split('/') {
        if component.is_empty()
            || component.starts_with('.')
            || component.ends_with(".lock")
            || component.len() > 255
        {
            return false;
        }
    }
    true
}

/// A worktree name is one directory component under the fixed in-root
/// `.cayw-worktrees` directory. This is a directory name, not a Git ref. The
/// fixed prefix and `--` keep leading punctuation from becoming an option.
fn valid_worktree_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.chars().any(char::is_control)
}

fn stage_paths<'a>(
    arguments: &'a Value,
    config: &HostConfig,
) -> Result<Vec<&'a str>, CapabilityError> {
    let values = arguments
        .get("paths")
        .and_then(Value::as_array)
        .ok_or_else(|| failure("paths must be a nonempty array"))?;
    if values.is_empty() || values.len() > MAX_STAGE_PATHS {
        return Err(failure("paths must contain between 1 and 256 entries"));
    }
    let mut paths = Vec::with_capacity(values.len());
    let mut seen = BTreeSet::new();
    let mut bytes = 0usize;
    for value in values {
        let path = value
            .as_str()
            .ok_or_else(|| failure("each staged path must be a string"))?;
        filesystem::components(path, false)?;
        if !seen.insert(path) {
            return Err(failure("staged paths must be unique"));
        }
        bytes = bytes.saturating_add(path.len());
        if bytes > config.max_argument_bytes {
            return Err(failure("staged paths exceed the argument byte limit"));
        }
        paths.push(path);
    }
    Ok(paths)
}

pub(crate) fn validate_arguments(
    name: &str,
    arguments: &Value,
    config: &HostConfig,
) -> Result<(), CapabilityError> {
    match name {
        "git.status" | "git.diff" | "git.log" | "git.show" => Ok(()),
        "git.blame" => {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| failure("blame requires a workspace path"))?;
            filesystem::components(path, false)?;
            if let Some(commit) = arguments.get("commit").and_then(Value::as_str)
                && !valid_object_id(commit)
            {
                return Err(failure("blame commit must be a full object ID"));
            }
            Ok(())
        }
        "git.stage" => {
            policy_for(config, WriteOp::Stage)?;
            stage_paths(arguments, config)?;
            object_id_value(arguments.get("expected_head"), "expected_head")?;
            object_id_value(arguments.get("expected_index_tree"), "expected_index_tree")?;
            Ok(())
        }
        "git.commit" => {
            policy_for(config, WriteOp::Commit)?;
            let message = arguments
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(|| failure("commit message must be a string"))?;
            if message.trim().is_empty()
                || message.len() > MAX_MESSAGE_BYTES
                || message.contains('\0')
            {
                return Err(failure(
                    "commit message must be nonempty, NUL-free, and within the byte limit",
                ));
            }
            object_id_value(arguments.get("expected_head"), "expected_head")?;
            object_id_value(arguments.get("expected_index_tree"), "expected_index_tree")?;
            Ok(())
        }
        "git.branch" => {
            policy_for(config, WriteOp::Branch)?;
            let action = arguments
                .get("action")
                .and_then(Value::as_str)
                .ok_or_else(|| failure("branch action must be create or delete"))?;
            if action != "create" && action != "delete" {
                return Err(failure("branch action must be create or delete"));
            }
            let name = arguments
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| failure("branch name must be a string"))?;
            if !valid_branch_name(name) {
                return Err(failure("branch name is not a controlled local branch name"));
            }
            object_id_value(arguments.get("commit"), "commit")?;
            object_id_value(arguments.get("expected_head"), "expected_head")?;
            Ok(())
        }
        "git.worktree" => {
            policy_for(config, WriteOp::Worktree)?;
            let action = arguments
                .get("action")
                .and_then(Value::as_str)
                .ok_or_else(|| failure("worktree action must be create or remove"))?;
            let name = arguments
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| failure("worktree name must be a string"))?;
            if !valid_worktree_name(name) {
                return Err(failure("worktree name is not a controlled in-root name"));
            }
            object_id_value(arguments.get("expected_head"), "expected_head")?;
            match action {
                "create" => {
                    let branch = arguments
                        .get("branch")
                        .and_then(Value::as_str)
                        .ok_or_else(|| failure("worktree create requires a branch"))?;
                    if !valid_branch_name(branch) {
                        return Err(failure(
                            "worktree branch is not a controlled local branch name",
                        ));
                    }
                    object_id_value(arguments.get("commit"), "commit")?;
                }
                "remove" => {
                    if arguments.get("branch").is_some() || arguments.get("commit").is_some() {
                        return Err(failure(
                            "worktree remove does not accept a branch or commit",
                        ));
                    }
                }
                _ => return Err(failure("worktree action must be create or remove")),
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

pub(crate) fn may_mutate(name: &str) -> bool {
    matches!(
        name,
        "git.stage" | "git.commit" | "git.branch" | "git.worktree"
    )
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
            crate::process::execute_git_read(host, name, arguments, cancel, deadline).await
        }
        "git.stage" => {
            let policy = policy_for(&host.config, WriteOp::Stage)?;
            stage(host, policy, arguments, cancel, deadline).await
        }
        "git.commit" => {
            let policy = policy_for(&host.config, WriteOp::Commit)?;
            commit(host, policy, arguments, cancel, deadline).await
        }
        "git.branch" => {
            let policy = policy_for(&host.config, WriteOp::Branch)?;
            branch(host, policy, arguments, cancel, deadline).await
        }
        "git.worktree" => {
            let policy = policy_for(&host.config, WriteOp::Worktree)?;
            worktree(host, policy, arguments, cancel, deadline).await
        }
        _ => Err(failure("unknown Git capability")),
    }
}

struct Repo {
    git: Dir,
    stamp: (u64, u64),
}

impl Repo {
    fn open(host: &Host) -> Result<Self, CapabilityError> {
        let metadata = host
            .root
            .symlink_metadata(".git")
            .map_err(|_| failure("Git writes require a direct .git directory in the workspace"))?;
        if metadata.file_type().is_symlink() {
            return Err(failure("Git writes refuse a symbolic-link .git directory"));
        }
        if !metadata.is_dir() {
            return Err(failure(
                "Git writes require a direct .git directory; gitdir files and bare repositories are refused",
            ));
        }
        let git = host
            .root
            .open_dir_nofollow(".git")
            .map_err(|_| failure("Git writes could not open .git without following links"))?;
        let stamp = directory_stamp(&git)?;
        for name in ["commondir", "config.worktree"] {
            if entry_exists(&git, name)? {
                return Err(failure(format!(
                    "Git writes refuse repository control entry {name}"
                )));
            }
        }
        regular_if_present(&git, "config")?;
        regular_if_present(&git, "index")?;
        regular_if_present(&git, "packed-refs")?;
        regular_if_present(&git, "shallow")?;
        regular_if_present(&git, "HEAD")?;
        if !entry_exists(&git, "HEAD")? {
            return Err(failure("Git writes require a repository HEAD file"));
        }
        directory_if_present(&git, "refs")?;
        directory_if_present(&git, "logs")?;
        directory_if_present(&git, "worktrees")?;
        directory_if_present(&git, "objects")?;
        if !entry_exists(&git, "objects")? {
            return Err(failure("Git writes require an objects directory"));
        }
        let objects = git
            .open_dir_nofollow("objects")
            .map_err(|_| failure("Git writes require a direct objects directory"))?;
        directory_if_present(&objects, "info")?;
        if entry_exists(&objects, "info")? {
            let info = objects
                .open_dir_nofollow("info")
                .map_err(|_| failure("Git objects/info must be a direct directory"))?;
            for name in ["alternates", "http-alternates"] {
                if entry_exists(&info, name)? {
                    return Err(failure(format!(
                        "Git writes refuse object alternate file {name}"
                    )));
                }
            }
        }
        reject_symlink_entries(&objects)?;
        if entry_exists(&git, "refs")? {
            let refs = git
                .open_dir_nofollow("refs")
                .map_err(|_| failure("Git refs must be a direct directory"))?;
            reject_ref_symlinks(&refs, 0)?;
        }
        Ok(Self { git, stamp })
    }

    fn verify(&self, host: &Host) -> Result<(), CapabilityError> {
        let stamp = Repo::open_stamp(host)?;
        if stamp != self.stamp {
            return Err(CapabilityError::unknown(
                "repository identity changed during a Git write; inspect before and after state",
            ));
        }
        let _ = &self.git;
        Ok(())
    }

    fn open_stamp(host: &Host) -> Result<(u64, u64), CapabilityError> {
        let git = host
            .root
            .open_dir_nofollow(".git")
            .map_err(|_| failure("Git writes could not reopen .git without following links"))?;
        directory_stamp(&git)
    }
}

#[cfg(unix)]
fn directory_stamp(dir: &Dir) -> Result<(u64, u64), CapabilityError> {
    use cap_std::fs::MetadataExt;
    let metadata = dir.dir_metadata().map_err(io_error)?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn directory_stamp(_: &Dir) -> Result<(u64, u64), CapabilityError> {
    Err(failure("Git writes require Unix directory identity"))
}

fn entry_exists(dir: &Dir, name: &str) -> Result<bool, CapabilityError> {
    match dir.symlink_metadata(name) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(error)),
    }
}

fn regular_if_present(dir: &Dir, name: &str) -> Result<(), CapabilityError> {
    match dir.symlink_metadata(name) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(failure(format!(
                    "Git writes require {name} to be a regular file"
                )));
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(error)),
    }
}

fn directory_if_present(dir: &Dir, name: &str) -> Result<(), CapabilityError> {
    match dir.symlink_metadata(name) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(failure(format!(
                    "Git writes require {name} to be a direct directory"
                )));
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(error)),
    }
}

fn reject_symlink_entries(dir: &Dir) -> Result<(), CapabilityError> {
    for entry in dir.entries().map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| failure("Git object entry names must be UTF-8"))?;
        let metadata = dir.symlink_metadata(name).map_err(io_error)?;
        if metadata.file_type().is_symlink() {
            return Err(failure(
                "Git writes refuse symbolic links in the object directory",
            ));
        }
    }
    Ok(())
}

fn reject_ref_symlinks(dir: &Dir, depth: usize) -> Result<(), CapabilityError> {
    if depth > 32 {
        return Err(failure("Git ref directory depth exceeded"));
    }
    for entry in dir.entries().map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| failure("Git ref names must be UTF-8"))?;
        filesystem::components(name, false)?;
        let metadata = dir.symlink_metadata(name).map_err(io_error)?;
        if metadata.file_type().is_symlink() {
            return Err(failure("Git writes refuse symbolic links in refs"));
        }
        if metadata.is_dir() {
            let child = dir.open_dir_nofollow(name).map_err(io_error)?;
            reject_ref_symlinks(&child, depth + 1)?;
        } else if !metadata.is_file() {
            return Err(failure("Git writes refuse special files in refs"));
        }
    }
    Ok(())
}

/// Returns the real path of the opened host root without reopening it as the
/// process working directory. The handle remains authoritative if the pathname
/// was renamed.
#[cfg(target_os = "macos")]
fn root_path(host: &Host) -> Result<PathBuf, CapabilityError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStringExt;
    unsafe extern "C" {
        fn fcntl(fd: i32, command: i32, ...) -> i32;
    }
    let mut bytes = [0u8; 1024];
    // SAFETY: host.root owns this fd; F_GETPATH writes at most MAXPATHLEN bytes
    // into this live output buffer and does not reopen any path.
    if unsafe { fcntl(host.root.as_raw_fd(), 50, bytes.as_mut_ptr()) } == -1 {
        return Err(io_error(io::Error::last_os_error()));
    }
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| failure("invalid workspace handle path"))?;
    Ok(std::ffi::OsString::from_vec(bytes[..end].to_vec()).into())
}

#[cfg(not(target_os = "macos"))]
fn root_path(_: &Host) -> Result<PathBuf, CapabilityError> {
    Err(failure("Git writes require a macOS workspace handle path"))
}

async fn write_lock(
    host: &Host,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(std::fs::File, cayw_workspace::Workspace), CapabilityError> {
    let (workspace, workspace_deadline) = host.open_workspace(cancel, deadline).await?;
    let lock = filesystem::write_lock(&host.root, cancel, deadline).await?;
    host.ensure_workspace_settled(&workspace, cancel, workspace_deadline)
        .await?;
    Ok((lock, workspace))
}

fn write_base_args() -> Vec<String> {
    let mut args = crate::process::git_base_args();
    // Pin the repository to the held working directory. Host callers cannot
    // supply a git-dir or work-tree; these flags override local core.worktree.
    args.push("--git-dir=.git".into());
    args.push("--work-tree=.".into());
    for (key, value) in [
        ("commit.gpgSign", "false"),
        ("tag.gpgSign", "false"),
        ("core.bare", "false"),
        ("core.sshCommand", ""),
        ("commit.verbose", "false"),
    ] {
        args.push("-c".into());
        args.push(format!("{key}={value}"));
    }
    args
}

fn write_environment(policy: &GitWritePolicy) -> std::collections::BTreeMap<String, String> {
    let mut env = crate::process::git_environment();
    for (name, value) in [
        ("GIT_AUTHOR_NAME", policy.identity_name.as_str()),
        ("GIT_AUTHOR_EMAIL", policy.identity_email.as_str()),
        ("GIT_COMMITTER_NAME", policy.identity_name.as_str()),
        ("GIT_COMMITTER_EMAIL", policy.identity_email.as_str()),
    ] {
        env.insert(name.into(), value.into());
    }
    env
}

async fn run(
    host: &Host,
    policy: &GitWritePolicy,
    args: Vec<String>,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<ProcessResult, CapabilityError> {
    check(cancel, deadline)?;
    let program = host
        .config
        .git_program
        .clone()
        .ok_or_else(|| failure("Git is not configured"))?;
    let mut spec = DirectoryProcessSpec {
        program,
        args,
        cwd: host.root.try_clone().map_err(io_error)?.into_std_file(),
        env: write_environment(policy),
        timeout: host.config.process_timeout,
        max_output_bytes: host.config.max_process_output_bytes,
        io: ProcessIo::Pipes,
    };
    check(cancel, deadline)?;
    spec.timeout = spec
        .timeout
        .min(deadline.saturating_duration_since(Instant::now()));
    if spec.timeout.is_zero() {
        return Err(failure("capability deadline reached before process launch"));
    }
    host.runner
        .run_in_directory(spec, cancel.clone())
        .await
        .map_err(crate::process::device_error)
}

fn output_detail(result: &ProcessResult) -> String {
    let stderr = String::from_utf8_lossy(&result.stderr);
    let stdout = String::from_utf8_lossy(&result.stdout);
    let mut detail = format!(
        "exit={:?}, termination={:?}, complete={}, stderr={}, stdout={}",
        result.status.code(),
        result.termination,
        result.output_complete,
        stderr.trim(),
        stdout.trim()
    );
    if detail.len() > MAX_OUTPUT_DETAIL {
        let mut end = MAX_OUTPUT_DETAIL;
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
    }
    detail
}

fn known_failure(label: &str, result: &ProcessResult) -> CapabilityError {
    failure(format!("git {label} failed: {}", output_detail(result)))
}

fn interrupted(label: &str, result: &ProcessResult) -> CapabilityError {
    CapabilityError::unknown(format!(
        "git {label} outcome is unknown: {}",
        output_detail(result)
    ))
}

fn require_terminal(label: &str, result: &ProcessResult) -> Result<(), CapabilityError> {
    if result.termination == TerminationReason::Exited && result.output_complete {
        Ok(())
    } else {
        Err(interrupted(label, result))
    }
}

fn parse_oid(result: &ProcessResult, label: &str) -> Result<String, CapabilityError> {
    let text = std::str::from_utf8(&result.stdout)
        .map_err(|_| failure(format!("git {label} returned non-UTF-8 output")))?;
    let value = text.trim();
    if !valid_object_id(value) {
        return Err(failure(format!(
            "git {label} did not return a full object ID"
        )));
    }
    Ok(value.into())
}

async fn query_oid(
    host: &Host,
    policy: &GitWritePolicy,
    mut args: Vec<String>,
    label: &str,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<String, CapabilityError> {
    args = {
        let mut base = write_base_args();
        base.append(&mut args);
        base
    };
    let result = run(host, policy, args, cancel, deadline).await?;
    if result.termination != TerminationReason::Exited || !result.output_complete {
        return Err(failure(format!(
            "git {label} did not complete before a write: {}",
            output_detail(&result)
        )));
    }
    if !result.status.success() {
        return Err(known_failure(label, &result));
    }
    parse_oid(&result, label)
}

async fn head_oid(
    host: &Host,
    policy: &GitWritePolicy,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<String, CapabilityError> {
    query_oid(
        host,
        policy,
        vec!["rev-parse".into(), "--verify".into(), "HEAD".into()],
        "rev-parse HEAD",
        cancel,
        deadline,
    )
    .await
}

async fn index_tree(
    host: &Host,
    policy: &GitWritePolicy,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<String, CapabilityError> {
    query_oid(
        host,
        policy,
        vec!["write-tree".into()],
        "write-tree",
        cancel,
        deadline,
    )
    .await
}

async fn branch_oid(
    host: &Host,
    policy: &GitWritePolicy,
    name: &str,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Option<String>, CapabilityError> {
    let mut args = write_base_args();
    args.extend([
        "rev-parse".into(),
        "--verify".into(),
        "--quiet".into(),
        format!("refs/heads/{name}"),
    ]);
    let result = run(host, policy, args, cancel, deadline).await?;
    if result.termination != TerminationReason::Exited || !result.output_complete {
        return Err(failure(format!(
            "git rev-parse branch did not complete: {}",
            output_detail(&result)
        )));
    }
    if !result.status.success() {
        return Ok(None);
    }
    Ok(Some(parse_oid(&result, "rev-parse branch")?))
}

async fn parent_oid(
    host: &Host,
    policy: &GitWritePolicy,
    commit: &str,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Option<String>, CapabilityError> {
    let mut args = write_base_args();
    args.extend([
        "rev-list".into(),
        "--parents".into(),
        "-n".into(),
        "1".into(),
        commit.into(),
    ]);
    let result = run(host, policy, args, cancel, deadline).await?;
    require_terminal("rev-list", &result)?;
    if !result.status.success() {
        return Err(known_failure("rev-list", &result));
    }
    let text = std::str::from_utf8(&result.stdout)
        .map_err(|_| failure("git rev-list returned non-UTF-8 output"))?;
    let mut fields = text.split_whitespace();
    let Some(first) = fields.next() else {
        return Err(failure("git rev-list returned no commit"));
    };
    if !valid_object_id(first) {
        return Err(failure("git rev-list returned an invalid commit ID"));
    }
    let parent = fields.next();
    if let Some(parent) = parent
        && !valid_object_id(parent)
    {
        return Err(failure("git rev-list returned an invalid parent ID"));
    }
    Ok(parent.map(str::to_string))
}

async fn inspect_local_config(
    host: &Host,
    policy: &GitWritePolicy,
    repo: &Repo,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(), CapabilityError> {
    let args = vec![
        "--no-pager".into(),
        "--git-dir=.git".into(),
        "config".into(),
        "--local".into(),
        "--null".into(),
        "--list".into(),
        "--no-includes".into(),
    ];
    let result = run(host, policy, args, cancel, deadline).await?;
    require_terminal("config inspection", &result)?;
    if !result.status.success() {
        return Err(known_failure("config inspection", &result));
    }
    for entry in result.stdout.split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }
        let (key, value) = match entry.iter().position(|byte| *byte == b'\n') {
            Some(position) => (&entry[..position], Some(&entry[position + 1..])),
            None => (entry, None),
        };
        let key =
            std::str::from_utf8(key).map_err(|_| failure("repository config key is not UTF-8"))?;
        let value = value
            .map(|value| std::str::from_utf8(value))
            .transpose()
            .map_err(|_| failure("repository config value is not UTF-8"))?;
        if dangerous_config_value(&key.to_ascii_lowercase(), value) {
            return Err(failure(format!(
                "repository config key is refused for host Git writes: {key}"
            )));
        }
    }
    repo.verify(host)
}

fn boolean_disabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "false" | "no" | "off" | "0"
        )
    })
}

/// Values have the semantics of their own key. For a command or include path,
/// `false` is a program or filename, not a boolean disable switch. Command-line
/// overrides neutralize fixed hooks, monitors, pagers and signing settings.
fn dangerous_config_value(key: &str, value: Option<&str>) -> bool {
    if key.starts_with("include.") || key.starts_with("includeif.") {
        return !matches!(value, Some("" | "/dev/null"));
    }
    if key.starts_with("filter.") {
        return (key.ends_with(".clean") || key.ends_with(".smudge") || key.ends_with(".process"))
            && value != Some("");
    }
    if key == "credential.helper" || (key.starts_with("credential.") && key.ends_with(".helper")) {
        return value != Some("");
    }
    if key.starts_with("gpg.") || key.starts_with("sequence.") || key == "core.editor" {
        return value != Some("");
    }
    if key.starts_with("pager.") {
        return !boolean_disabled(value) && value != Some("cat");
    }
    if key == "protocol.allow" || (key.starts_with("protocol.") && key.ends_with(".allow")) {
        return !value.is_some_and(|value| value.eq_ignore_ascii_case("never"));
    }
    match key {
        // Every write pins these exact values after repository config. No local
        // value can select a helper, alternate root or ambient attributes file.
        "core.hookspath"
        | "core.fsmonitor"
        | "core.sshcommand"
        | "core.askpass"
        | "core.pager"
        | "core.worktree"
        | "core.alternaterefscommand"
        | "core.attributesfile"
        | "core.excludesfile"
        | "diff.external" => false,
        "core.gitproxy" => value != Some("none") && value != Some(""),
        "extensions.worktreeconfig" => !boolean_disabled(value),
        "extensions.partialclone" => value != Some(""),
        "commit.template" => !matches!(value, Some("" | "/dev/null")),
        _ => {
            key.starts_with("diff.")
                && (key.ends_with(".command") || key.ends_with(".textconv"))
                && value != Some("")
        }
    }
}

fn validate_stage_paths(host: &Host, paths: &[&str]) -> Result<(), CapabilityError> {
    for path in paths {
        filesystem::validate_git_path(&host.root, path)?;
        let components = filesystem::components(path, false)?;
        let Some((name, parents)) = components.split_last() else {
            return Err(failure("staged path must name a file"));
        };
        let mut dir = host.root.try_clone().map_err(io_error)?;
        let mut missing = false;
        for component in parents {
            match dir.open_dir_nofollow(component) {
                Ok(next) => dir = next,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    // A tracked path under a deleted directory is a valid staging
                    // deletion; there is no current entry to inspect.
                    missing = true;
                    break;
                }
                Err(error) => return Err(io_error(error)),
            }
        }
        if missing {
            continue;
        }
        match dir.symlink_metadata(name) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(failure("staged paths may not be symbolic links"));
                }
                if !metadata.is_file() {
                    return Err(failure("staged paths must be regular files"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok(())
}

fn verify_repo_after(host: &Host, repo: &Repo) -> Result<(), CapabilityError> {
    repo.verify(host).map_err(|error| {
        CapabilityError::unknown(format!(
            "repository identity could not be verified after a Git write: {}",
            error.message
        ))
    })
}

async fn stage(
    host: &Host,
    policy: &GitWritePolicy,
    arguments: &Value,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Value, CapabilityError> {
    validate_arguments("git.stage", arguments, &host.config)?;
    let paths = stage_paths(arguments, &host.config)?;
    let expected_head = object_id_value(arguments.get("expected_head"), "expected_head")?;
    let expected_tree =
        object_id_value(arguments.get("expected_index_tree"), "expected_index_tree")?;
    let (_lock, _workspace) = write_lock(host, cancel, deadline).await?;
    let repo = Repo::open(host)?;
    inspect_local_config(host, policy, &repo, cancel, deadline).await?;
    validate_stage_paths(host, &paths)?;

    let head = head_oid(host, policy, cancel, deadline).await?;
    if head != expected_head {
        return Err(failure(
            "staging precondition failed: HEAD does not match expected_head",
        ));
    }
    let before = index_tree(host, policy, cancel, deadline).await?;
    if before != expected_tree {
        return Err(failure(
            "staging precondition failed: index tree does not match expected_index_tree",
        ));
    }
    repo.verify(host)?;

    let mut args = write_base_args();
    args.extend(["add".into(), "--".into()]);
    args.extend(paths.iter().map(|path| (*path).to_string()));
    let result = match run(host, policy, args, cancel, deadline).await {
        Ok(result) => result,
        Err(error) => {
            verify_repo_after(host, &repo)?;
            return Err(error);
        }
    };
    verify_repo_after(host, &repo)?;
    if result.termination != TerminationReason::Exited || !result.output_complete {
        return Err(interrupted("add", &result));
    }
    if !result.status.success() {
        let after_head = head_oid(host, policy, cancel, deadline).await.ok();
        let after_tree = index_tree(host, policy, cancel, deadline).await.ok();
        verify_repo_after(host, &repo)?;
        if after_head.as_deref() == Some(head.as_str())
            && after_tree.as_deref() == Some(before.as_str())
        {
            return Err(known_failure("add", &result));
        }
        return Err(CapabilityError::unknown(format!(
            "git add failed after changing repository state; inspect the index: {}",
            output_detail(&result)
        )));
    }
    let index_tree = index_tree(host, policy, cancel, deadline)
        .await
        .map_err(|error| {
            CapabilityError::unknown(format!(
                "git add succeeded but index verification failed: {}",
                error.message
            ))
        })?;
    verify_repo_after(host, &repo)?;
    Ok(json!({
        "head":head,
        "index_tree":index_tree,
        "paths":paths,
    }))
}

async fn commit(
    host: &Host,
    policy: &GitWritePolicy,
    arguments: &Value,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Value, CapabilityError> {
    validate_arguments("git.commit", arguments, &host.config)?;
    let message = arguments["message"]
        .as_str()
        .expect("validated commit message");
    let expected_head = object_id_value(arguments.get("expected_head"), "expected_head")?;
    let expected_tree =
        object_id_value(arguments.get("expected_index_tree"), "expected_index_tree")?;
    let (_lock, _workspace) = write_lock(host, cancel, deadline).await?;
    let repo = Repo::open(host)?;
    inspect_local_config(host, policy, &repo, cancel, deadline).await?;

    let head = head_oid(host, policy, cancel, deadline).await?;
    if head != expected_head {
        return Err(failure(
            "commit precondition failed: HEAD does not match expected_head",
        ));
    }
    let tree = index_tree(host, policy, cancel, deadline).await?;
    if tree != expected_tree {
        return Err(failure(
            "commit precondition failed: index tree does not match expected_index_tree",
        ));
    }
    repo.verify(host)?;

    let mut args = write_base_args();
    args.extend([
        "commit".into(),
        "--no-verify".into(),
        "--no-gpg-sign".into(),
        "--cleanup=verbatim".into(),
        "-m".into(),
        message.into(),
    ]);
    let result = match run(host, policy, args, cancel, deadline).await {
        Ok(result) => result,
        Err(error) => {
            verify_repo_after(host, &repo)?;
            return Err(error);
        }
    };
    verify_repo_after(host, &repo)?;
    if result.termination != TerminationReason::Exited || !result.output_complete {
        return Err(interrupted("commit", &result));
    }
    if !result.status.success() {
        let after_head = head_oid(host, policy, cancel, deadline).await.ok();
        let after_tree = index_tree(host, policy, cancel, deadline).await.ok();
        verify_repo_after(host, &repo)?;
        if after_head.as_deref() == Some(head.as_str())
            && after_tree.as_deref() == Some(tree.as_str())
        {
            return Err(known_failure("commit", &result));
        }
        return Err(CapabilityError::unknown(format!(
            "git commit failed after changing repository state; inspect HEAD and the index: {}",
            output_detail(&result)
        )));
    }
    let commit = head_oid(host, policy, cancel, deadline)
        .await
        .map_err(|error| {
            CapabilityError::unknown(format!(
                "git commit succeeded but HEAD verification failed: {}",
                error.message
            ))
        })?;
    if commit == head {
        return Err(CapabilityError::unknown(
            "git commit reported success but HEAD did not advance",
        ));
    }
    let parent = parent_oid(host, policy, &commit, cancel, deadline)
        .await
        .map_err(|error| {
            CapabilityError::unknown(format!(
                "git commit succeeded but parent verification failed: {}",
                error.message
            ))
        })?;
    if parent.as_deref() != Some(head.as_str()) {
        return Err(CapabilityError::unknown(
            "git commit parent does not match the verified HEAD",
        ));
    }
    let committed_tree = query_oid(
        host,
        policy,
        vec![
            "rev-parse".into(),
            "--verify".into(),
            format!("{commit}^{{tree}}"),
        ],
        "rev-parse commit tree",
        cancel,
        deadline,
    )
    .await
    .map_err(|error| {
        CapabilityError::unknown(format!(
            "git commit succeeded but tree verification failed: {}",
            error.message
        ))
    })?;
    if committed_tree != tree {
        return Err(CapabilityError::unknown(
            "committed tree differs from the verified index tree",
        ));
    }
    verify_repo_after(host, &repo)?;
    Ok(json!({
        "commit":commit,
        "parent":head,
        "tree":committed_tree,
    }))
}

async fn branch(
    host: &Host,
    policy: &GitWritePolicy,
    arguments: &Value,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Value, CapabilityError> {
    validate_arguments("git.branch", arguments, &host.config)?;
    let action = arguments["action"].as_str().expect("validated action");
    let name = arguments["name"].as_str().expect("validated name");
    let target = object_id_value(arguments.get("commit"), "commit")?;
    let expected_head = object_id_value(arguments.get("expected_head"), "expected_head")?;
    let (_lock, _workspace) = write_lock(host, cancel, deadline).await?;
    let repo = Repo::open(host)?;
    inspect_local_config(host, policy, &repo, cancel, deadline).await?;

    let head = head_oid(host, policy, cancel, deadline).await?;
    if head != expected_head {
        return Err(failure(
            "branch precondition failed: HEAD does not match expected_head",
        ));
    }
    let existing = branch_oid(host, policy, name, cancel, deadline).await?;
    if action == "create" {
        if existing.is_some() {
            return Err(failure("branch already exists"));
        }
    } else if existing.as_deref() != Some(target.as_str()) {
        return Err(failure(
            "branch precondition failed: branch does not point at the expected commit",
        ));
    }
    repo.verify(host)?;

    let mut args = write_base_args();
    if action == "create" {
        args.extend(["branch".into(), "--".into(), name.into(), target.clone()]);
    } else {
        args.extend(["branch".into(), "-d".into(), "--".into(), name.into()]);
    }
    let result = match run(host, policy, args, cancel, deadline).await {
        Ok(result) => result,
        Err(error) => {
            verify_repo_after(host, &repo)?;
            return Err(error);
        }
    };
    verify_repo_after(host, &repo)?;
    if result.termination != TerminationReason::Exited || !result.output_complete {
        return Err(interrupted("branch", &result));
    }
    let after = branch_oid(host, policy, name, cancel, deadline).await;
    match (action, result.status.success(), after) {
        ("create", true, Ok(Some(oid))) if oid == target => Ok(json!({
            "action":"create",
            "name":name,
            "commit":target,
        })),
        ("delete", true, Ok(None)) => Ok(json!({
            "action":"delete",
            "name":name,
            "commit":target,
        })),
        (_, false, Ok(after)) if after.as_deref() == existing.as_deref() => {
            Err(known_failure("branch", &result))
        }
        _ => Err(CapabilityError::unknown(format!(
            "git branch outcome is unknown; inspect the branch: {}",
            output_detail(&result)
        ))),
    }
}

fn worktree_parent_existing(host: &Host) -> Result<Option<Dir>, CapabilityError> {
    match host.root.symlink_metadata(WORKTREE_ROOT) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(failure(
                    "Git worktree root must be a direct .cayw-worktrees directory",
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    }
    host.root
        .open_dir_nofollow(WORKTREE_ROOT)
        .map(Some)
        .map_err(|_| failure("Git worktree root could not be opened without links"))
}

fn worktree_dir(host: &Host, name: &str) -> Result<Dir, CapabilityError> {
    let parent =
        worktree_parent_existing(host)?.ok_or_else(|| failure("Git worktree root is absent"))?;
    let metadata = parent
        .symlink_metadata(name)
        .map_err(|_| failure("Git worktree path is not a registered in-root worktree"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(failure("Git worktree path must be a direct directory"));
    }
    parent
        .open_dir_nofollow(name)
        .map_err(|_| failure("Git worktree path could not be opened without links"))
}

fn absolute_worktree_path(host: &Host, name: &str) -> Result<PathBuf, CapabilityError> {
    Ok(root_path(host)?.join(WORKTREE_ROOT).join(name))
}

async fn git_worktree_entries(
    host: &Host,
    policy: &GitWritePolicy,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Vec<(PathBuf, Option<String>, Option<String>)>, CapabilityError> {
    let mut args = write_base_args();
    args.extend(["worktree".into(), "list".into(), "--porcelain".into()]);
    let result = run(host, policy, args, cancel, deadline).await?;
    require_terminal("worktree list", &result)?;
    if !result.status.success() {
        return Err(known_failure("worktree list", &result));
    }
    let text = std::str::from_utf8(&result.stdout)
        .map_err(|_| failure("git worktree list returned non-UTF-8 output"))?;
    let mut entries = Vec::new();
    for block in text.split("\n\n") {
        let mut path = None;
        let mut head = None;
        let mut branch = None;
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(value));
            } else if let Some(value) = line.strip_prefix("HEAD ") {
                head = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("branch ") {
                branch = Some(value.to_string());
            }
        }
        if let Some(path) = path {
            entries.push((path, head, branch));
        }
    }
    Ok(entries)
}

fn worktree_admin_names(host: &Host) -> Result<BTreeSet<String>, CapabilityError> {
    let mut names = BTreeSet::new();
    let git = host.root.open_dir_nofollow(".git").map_err(io_error)?;
    if !entry_exists(&git, "worktrees")? {
        return Ok(names);
    }
    let worktrees = git.open_dir_nofollow("worktrees").map_err(io_error)?;
    for entry in worktrees.entries().map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        if let Some(name) = entry.file_name().to_str() {
            names.insert(name.to_string());
        }
    }
    Ok(names)
}

fn worktree_gitdir_stays_inside(host: &Host, name: &str) -> Result<(), CapabilityError> {
    let dir = worktree_dir(host, name)?;
    let metadata = dir
        .symlink_metadata(".git")
        .map_err(|_| failure("linked worktree has no .git control file"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(failure(
            "linked worktree .git control entry must be a regular file",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let mut file = dir
        .open_with(".git", &options)
        .map_err(|_| failure("linked worktree .git control file could not be opened"))?;
    if file.metadata().map_err(io_error)?.len() > 4096 {
        return Err(failure("linked worktree .git control file is too large"));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(io_error)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| failure("linked worktree .git control file is not UTF-8"))?;
    let target = text
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .ok_or_else(|| failure("linked worktree .git control file has no gitdir"))?;
    if target.is_empty() {
        return Err(failure(
            "linked worktree .git control file has an empty gitdir",
        ));
    }
    let target = PathBuf::from(target);
    let root = root_path(host)?;
    let resolved = if target.is_absolute() {
        target
    } else {
        root.join(WORKTREE_ROOT).join(name).join(target)
    };
    let normalized = normalize_path(&resolved);
    let admin = normalize_path(&root.join(".git").join("worktrees"));
    if !normalized.starts_with(&admin) || normalized == admin {
        return Err(failure(
            "linked worktree .git points outside this repository's worktree administration",
        ));
    }
    // Resolve the final target without letting a replaced pathname claim another
    // repository: the canonical directory must still sit under our admin root.
    let canonical_target = std::fs::canonicalize(&normalized)
        .map_err(|_| failure("linked worktree .git target cannot be verified"))?;
    let canonical_admin = std::fs::canonicalize(&admin)
        .map_err(|_| failure("repository worktree administration cannot be verified"))?;
    if !canonical_target.starts_with(&canonical_admin) || canonical_target == canonical_admin {
        return Err(failure(
            "linked worktree .git resolves outside this repository's worktree administration",
        ));
    }
    Ok(())
}

async fn worktree_has_changes(
    host: &Host,
    policy: &GitWritePolicy,
    name: &str,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<bool, CapabilityError> {
    // Ask Git whether removal without force would refuse before attempting any
    // write. This is a read through the verified linked-worktree control file:
    // it does not refresh the index (optional locks are disabled) and cannot run
    // repository hooks, filters, diff programs, or credentials.
    let mut args = crate::process::git_base_args();
    args.extend([
        "-C".into(),
        format!("{WORKTREE_ROOT}/{name}"),
        "status".into(),
        "--porcelain=v1".into(),
        "--untracked-files=all".into(),
        "--ignore-submodules=none".into(),
    ]);
    let result = run(host, policy, args, cancel, deadline).await?;
    if result.termination != TerminationReason::Exited || !result.output_complete {
        // This preflight is read-only and runs before any removal. A timeout or
        // lost output here cannot have mutated the worktree or its registration.
        return Err(failure(format!(
            "git worktree status did not complete before removal: {}",
            output_detail(&result)
        )));
    }
    if !result.status.success() {
        return Err(known_failure("worktree status", &result));
    }
    Ok(result.stdout.iter().any(|byte| !byte.is_ascii_whitespace()))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

async fn worktree(
    host: &Host,
    policy: &GitWritePolicy,
    arguments: &Value,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Value, CapabilityError> {
    validate_arguments("git.worktree", arguments, &host.config)?;
    let action = arguments["action"].as_str().expect("validated action");
    let name = arguments["name"].as_str().expect("validated name");
    let expected_head = object_id_value(arguments.get("expected_head"), "expected_head")?;
    let (_lock, _workspace) = write_lock(host, cancel, deadline).await?;
    let repo = Repo::open(host)?;
    inspect_local_config(host, policy, &repo, cancel, deadline).await?;

    let head = head_oid(host, policy, cancel, deadline).await?;
    if head != expected_head {
        return Err(failure(
            "worktree precondition failed: HEAD does not match expected_head",
        ));
    }
    let absolute = absolute_worktree_path(host, name)?;
    let before_entries = git_worktree_entries(host, policy, cancel, deadline).await?;
    let before_admin = worktree_admin_names(host)?;
    let path = format!("{WORKTREE_ROOT}/{name}");
    if action == "create" {
        let branch = arguments["branch"].as_str().expect("validated branch");
        let target = object_id_value(arguments.get("commit"), "commit")?;
        if before_entries
            .iter()
            .any(|(entry, _, _)| entry == &absolute)
        {
            return Err(failure(
                "worktree is already registered at the controlled path",
            ));
        }
        if branch_oid(host, policy, branch, cancel, deadline)
            .await?
            .as_deref()
            != Some(target.as_str())
        {
            return Err(failure(
                "worktree precondition failed: branch does not point at the expected commit",
            ));
        }
        if let Some(parent) = worktree_parent_existing(host)?
            && entry_exists(&parent, name)?
        {
            return Err(failure("worktree path already exists"));
        }
        repo.verify(host)?;
        let mut args = write_base_args();
        args.extend([
            "worktree".into(),
            "add".into(),
            "--".into(),
            path.clone(),
            branch.into(),
        ]);
        let result = match run(host, policy, args, cancel, deadline).await {
            Ok(result) => result,
            Err(error) => {
                verify_repo_after(host, &repo)?;
                return Err(error);
            }
        };
        verify_repo_after(host, &repo)?;
        if result.termination != TerminationReason::Exited || !result.output_complete {
            return Err(interrupted("worktree add", &result));
        }
        if !result.status.success() {
            let after_admin = worktree_admin_names(host).ok();
            // A verification error after launch cannot prove that add changed
            // nothing, so only an explicit absent path plus unchanged admin set
            // permits a known failure.
            let absent = match worktree_parent_existing(host) {
                Ok(Some(parent)) => matches!(entry_exists(&parent, name), Ok(false)),
                Ok(None) => true,
                Err(_) => false,
            };
            if absent && after_admin.as_ref() == Some(&before_admin) {
                return Err(known_failure("worktree add", &result));
            }
            return Err(CapabilityError::unknown(format!(
                "git worktree add failed after changing administration or path state: {}",
                output_detail(&result)
            )));
        }
        let after_entries = git_worktree_entries(host, policy, cancel, deadline)
            .await
            .map_err(|error| {
                CapabilityError::unknown(format!(
                    "git worktree add succeeded but registration verification failed: {}",
                    error.message
                ))
            })?;
        let Some((_, entry_head, entry_branch)) = after_entries
            .iter()
            .find(|(entry, _, _)| entry == &absolute)
        else {
            return Err(CapabilityError::unknown(
                "git worktree add succeeded without an in-root registration",
            ));
        };
        let expected_branch = format!("refs/heads/{branch}");
        if entry_branch.as_deref() != Some(expected_branch.as_str())
            || entry_head.as_deref() != Some(target.as_str())
        {
            return Err(CapabilityError::unknown(
                "git worktree registration does not match the requested branch and commit",
            ));
        }
        worktree_gitdir_stays_inside(host, name).map_err(|error| {
            CapabilityError::unknown(format!(
                "git worktree add succeeded but the linked .git pointer could not be verified: {}",
                error.message
            ))
        })?;
        verify_repo_after(host, &repo)?;
        return Ok(json!({
            "action":"create",
            "name":name,
            "path":path,
            "branch":branch,
            "commit":target,
        }));
    }

    // Removal is accepted only for the fixed controlled subdirectory and only
    // when both Git's own registration and the worktree .git pointer identify
    // this repository. This never relaxes the external-gitdir refusal above.
    if !before_entries
        .iter()
        .any(|(entry, _, _)| entry == &absolute)
    {
        return Err(failure(
            "worktree removal requires an in-root registration at the controlled path",
        ));
    }
    worktree_gitdir_stays_inside(host, name)?;
    if worktree_has_changes(host, policy, name, cancel, deadline).await? {
        return Err(failure(
            "linked worktree has staged, modified, or untracked changes; removal never forces",
        ));
    }
    repo.verify(host)?;
    let mut args = write_base_args();
    args.extend([
        "worktree".into(),
        "remove".into(),
        "--".into(),
        path.clone(),
    ]);
    let result = match run(host, policy, args, cancel, deadline).await {
        Ok(result) => result,
        Err(error) => {
            verify_repo_after(host, &repo)?;
            return Err(error);
        }
    };
    verify_repo_after(host, &repo)?;
    if result.termination != TerminationReason::Exited || !result.output_complete {
        return Err(interrupted("worktree remove", &result));
    }
    let after_entries = git_worktree_entries(host, policy, cancel, deadline)
        .await
        .map_err(|error| {
            CapabilityError::unknown(format!(
                "git worktree remove could not be verified: {}",
                error.message
            ))
        })?;
    let listed = after_entries.iter().any(|(entry, _, _)| entry == &absolute);
    let exists = match worktree_parent_existing(host) {
        Ok(Some(parent)) => entry_exists(&parent, name),
        Ok(None) => Ok(false),
        Err(error) => Err(error),
    };
    let exists = match exists {
        Ok(exists) => exists,
        Err(error) => {
            return Err(CapabilityError::unknown(format!(
                "git worktree remove ran but its path state could not be verified: {}",
                error.message
            )));
        }
    };
    if result.status.success() && !listed && !exists {
        return Ok(json!({
            "action":"remove",
            "name":name,
            "path":path,
        }));
    }
    Err(CapabilityError::unknown(format!(
        "git worktree remove left registration or path state uncertain (listed={listed}, path_exists={exists}): {}",
        output_detail(&result)
    )))
}
