//! Private per-workspace Git recovery points and bounded local backups.
//!
//! A workspace keeps its snapshot repository under
//! `<CAYW_HOME>/workspaces/<directory digest>/recovery/repository.git`. The
//! workspace itself is the work tree. Git is never run on the user's own
//! repository: every command receives an explicit `--git-dir` and
//! `--work-tree`, ignores system/global configuration, disables hooks and
//! prompts, and never deletes a recovery point.
//!
//! Recovery points are internal evidence. They are not exposed as model
//! capabilities and no generic `git` command is added. Restoring only accepts
//! commits reachable from this workspace's own recovery branch.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _},
    path::{Component, Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

pub const RECOVERY_SCHEMA: &str = "cayw.recovery.v1";
const RECOVERY_DIR: &str = "recovery";
const REPOSITORY_DIR: &str = "repository.git";
const RECOVERY_REF: &str = "refs/heads/recovery";
const RECOVERY_LOCK: &str = "recovery.lock";
const STATE_FILE: &str = "state.json";
const CONFIG_FILE: &str = "config.json";
const RESTORE_INTENT_FILE: &str = "restore-intent.json";
const BACKUP_DIR: &str = "backups";
const EMPTY_HOME_DIR: &str = "empty-home";
const EMPTY_HOOKS_DIR: &str = "empty-hooks";

/// Names that never enter a snapshot and can never be restored over.
pub const PROTECTED_NAMES: &[&str] = &[
    ".git",
    ".cayw-workspace",
    ".cayw-worktrees",
    ".cayw-host-lock",
    ".cayw-host-read-fence",
    ".cayw-host-read-state",
    ".cayw-host-read-fault",
    ".cayw-host-read-faults",
];
pub const PROTECTED_PREFIXES: &[&str] = &[".cayw-host-tmp-", ".cayw-host-read-state-"];

#[derive(Clone, Copy, Debug)]
pub struct RecoveryLimits {
    pub max_files: usize,
    pub max_bytes: u64,
    pub max_file_bytes: u64,
    pub max_backup_bytes: u64,
}

impl Default for RecoveryLimits {
    fn default() -> Self {
        Self {
            max_files: 200_000,
            max_bytes: 2 * 1024 * 1024 * 1024,
            max_file_bytes: 64 * 1024 * 1024,
            max_backup_bytes: 256 * 1024 * 1024,
        }
    }
}

impl RecoveryLimits {
    fn validate(self) -> Result<(), String> {
        if self.max_files == 0
            || self.max_files > 1_000_000
            || self.max_bytes == 0
            || self.max_file_bytes == 0
            || self.max_file_bytes > self.max_bytes
            || self.max_backup_bytes == 0
        {
            return Err("invalid recovery limits".into());
        }
        Ok(())
    }
}

/// Per-workspace recovery settings. The local Client enables this by default;
/// a fully explicit Headless/SDK configuration leaves it absent.
#[derive(Clone, Debug)]
pub struct RecoveryConfig {
    /// Explicit Git executable. `None` uses the safe platform detection.
    pub git_program: Option<PathBuf>,
    /// The device state root, used only to exclude it when it lives inside the
    /// workspace. The snapshot repository is always excluded as well.
    pub data_dir: Option<PathBuf>,
    /// Set to false to keep the opt-in but disable this workspace. The private
    /// `recovery/config.json` may also set `"enabled": false`.
    pub enabled: bool,
    pub limits: RecoveryLimits,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            git_program: None,
            data_dir: None,
            enabled: true,
            limits: RecoveryLimits::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Enabled {
    Yes,
    No,
    Unreadable,
}

#[derive(Debug, Clone)]
pub enum SnapshotOutcome {
    Disabled,
    Unavailable {
        warning: String,
    },
    Ready {
        id: String,
        created: bool,
        complete: bool,
        out_of_scope: usize,
        out_of_scope_sample: Vec<String>,
        warning: Option<String>,
    },
}

impl SnapshotOutcome {
    pub fn warning(&self) -> Option<&str> {
        match self {
            Self::Disabled | Self::Ready { warning: None, .. } => None,
            Self::Unavailable { warning } => Some(warning),
            Self::Ready {
                warning: Some(warning),
                ..
            } => Some(warning),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecoveryError {
    pub message: String,
    pub outcome_unknown: bool,
}

impl RecoveryError {
    fn known(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            outcome_unknown: false,
        }
    }
    fn unknown(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            outcome_unknown: true,
        }
    }
}

impl std::fmt::Display for RecoveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// One explicit restore. Paths are workspace-relative and must name regular
/// files that existed in the selected CAYW recovery point. A current file that
/// differs from the point is reported in `needs_confirmation`; it is not
/// overwritten unless that exact path is repeated in `confirm_overwrite`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreRequest {
    pub snapshot: String,
    pub paths: Vec<String>,
    #[serde(default)]
    pub confirm_overwrite: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestoreReport {
    pub snapshot: String,
    pub restored: Vec<String>,
    pub unchanged: Vec<String>,
    pub needs_confirmation: Vec<String>,
    pub not_in_snapshot: Vec<String>,
    pub undo_snapshot: Option<String>,
    pub complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Current {
    Same,
    Absent,
    Different,
}

pub struct RecoveryPoints {
    workspace: PathBuf,
    state_dir: PathBuf,
    config: RecoveryConfig,
    git: Option<PathBuf>,
    enabled: Mutex<Enabled>,
    warned: AtomicBool,
    attempted_tasks: Mutex<BTreeSet<String>>,
}

impl RecoveryPoints {
    /// Opens no directories and creates nothing. Git detection is read-only.
    pub fn open(
        workspace: impl AsRef<Path>,
        state_dir: impl AsRef<Path>,
        config: RecoveryConfig,
    ) -> Self {
        let workspace = workspace.as_ref().to_path_buf();
        let state_dir = state_dir.as_ref().to_path_buf();
        let enabled = read_enabled(&state_dir.join(RECOVERY_DIR).join(CONFIG_FILE));
        // Keep any configured explicit program even when it does not exist so
        // callers can observe the warning instead of silently switching to
        // another Git installation.
        let git = match &config.git_program {
            Some(program) => Some(program.clone()),
            None => detect_git_program(),
        };
        Self {
            workspace,
            state_dir,
            config,
            git,
            enabled: Mutex::new(enabled),
            warned: AtomicBool::new(false),
            attempted_tasks: Mutex::new(BTreeSet::new()),
        }
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn git_program(&self) -> Option<&Path> {
        self.git.as_deref()
    }

    fn enabled_state(&self) -> Enabled {
        *self
            .enabled
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled && self.enabled_state() != Enabled::No
    }

    /// Disable Git recovery for this workspace. The private config is replaced
    /// atomically and the running instance observes the new value immediately.
    /// Existing points and local backups are retained; nothing is deleted.
    pub fn disable(&self) -> Result<(), RecoveryError> {
        let _recovery_lock = self.acquire_lock(&self.recovery_dir().join(RECOVERY_LOCK))?;
        self.write_config(&json!({
            "schema":RECOVERY_SCHEMA,
            "enabled":false,
        }))?;
        *self
            .enabled
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Enabled::No;
        Ok(())
    }

    /// Recovery coverage for approval decisions. `None` means the workspace
    /// recovery is disabled, so every rewrite or deletion asks. When enabled,
    /// the snapshot state's out-of-scope sample becomes explicit exclusions and
    /// never counts as covered.
    pub fn approval_scope(
        &self,
        base: &crate::approval_rules::RecoveryScope,
    ) -> Option<crate::approval_rules::RecoveryScope> {
        if !self.enabled() {
            return None;
        }
        // Without a readable point state there is no evidence that the current
        // workspace content is covered. Fail closed: file changes ask.
        let state = self.read_state().ok().flatten()?;
        let mut scope = base.clone();
        let mut excluded = BTreeSet::new();
        for path in state.out_of_scope_sample {
            if validate_relative(&path).is_ok() {
                excluded.insert(PathBuf::from(path));
            }
        }
        for path in excluded {
            scope.exclusions.push(path);
        }
        Some(scope)
    }

    fn recovery_dir(&self) -> PathBuf {
        self.state_dir.join(RECOVERY_DIR)
    }

    fn repository(&self) -> PathBuf {
        self.recovery_dir().join(REPOSITORY_DIR)
    }

    /// Read-only status for `cayw doctor` and host diagnostics.
    pub fn status(&self) -> Value {
        let state_result = self.read_state();
        let state_error = state_result.as_ref().err().cloned();
        let state = state_result.ok().flatten();
        let points_result = if self.git.is_some() {
            self.point_ids()
        } else {
            Ok(vec![])
        };
        let points_error = points_result.as_ref().err().cloned();
        let snapshot_count = points_result.as_ref().ok().map(Vec::len);
        let restore_intent = fs::read(self.recovery_dir().join(RESTORE_INTENT_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let reconciliation = self.reconciliation();
        let restore_outcome_unknown = reconciliation["outcome_unknown"] == true;
        let mut warnings = Vec::new();
        if let Some(warning) = self.warning_for_availability() {
            warnings.push(warning);
        }
        if let Some(error) = &state_error {
            warnings.push(error.clone());
        }
        if let Some(error) = &points_error {
            warnings.push(error.clone());
        }
        if restore_outcome_unknown && let Some(message) = reconciliation["message"].as_str() {
            warnings.push(message.to_owned());
        }
        let warning = (!warnings.is_empty()).then(|| warnings.join("；"));
        json!({
            "schema":RECOVERY_SCHEMA,
            "enabled":self.enabled(),
            "repository":self.repository(),
            "git":self.git,
            "snapshot_count":snapshot_count,
            "state_error":state_error,
            "points_error":points_error,
            "last_snapshot":state.as_ref().and_then(|state| state.last_snapshot.clone()),
            "last_complete":state.as_ref().map(|state| state.complete),
            "out_of_scope":state.as_ref().map(|state| state.out_of_scope),
            "out_of_scope_sample":state.as_ref().map_or_else(Vec::new, |state| state.out_of_scope_sample.clone()),
            "restore_intent":restore_intent,
            "restore_outcome_unknown":restore_outcome_unknown,
            "restore_reconciliation":reconciliation,
            "warning":warning,
        })
    }

    /// Read-only inspection of an interrupted restore. An intent that did not
    /// reach `completed` is reported as `outcome_unknown`; this method never
    /// writes, never changes the intent to success and never replays a write.
    pub fn reconciliation(&self) -> Value {
        let path = self.recovery_dir().join(RESTORE_INTENT_FILE);
        let intent = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<RestoreIntent>(&bytes) {
                Ok(intent) => Some(intent),
                Err(_) => {
                    return restore_record_unknown("还原记录损坏，无法确认上次还原是否完成。");
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(_) => {
                return restore_record_unknown("还原记录无法读取，无法确认上次还原是否完成。");
            }
        };
        match intent {
            Some(intent) => {
                let unfinished = intent.outcome_unknown || intent.status != "completed";
                let installed: BTreeSet<&str> =
                    intent.installed.iter().map(String::as_str).collect();
                let pending = intent
                    .paths
                    .iter()
                    .filter(|path| !installed.contains(path.as_str()))
                    .cloned()
                    .collect::<Vec<_>>();
                json!({
                    "schema":RECOVERY_SCHEMA,
                    "read_only":true,
                    "unfinished":unfinished,
                    "outcome_unknown":unfinished,
                    "status":intent.status,
                    "snapshot":intent.snapshot,
                    "installed":intent.installed,
                    "pending":pending,
                    "failed":intent.failed,
                    "created_unix_ms":intent.created_unix_ms,
                    "message":if unfinished {
                        "上次还原未完成；状态保持 outcome_unknown。本入口只读，不重放，也不会改成成功。"
                    } else {
                        "没有未完成的还原。"
                    },
                })
            }
            None => json!({
                "schema":RECOVERY_SCHEMA,
                "read_only":true,
                "unfinished":false,
                "outcome_unknown":false,
                "status":Value::Null,
                "snapshot":Value::Null,
                "installed":[],
                "pending":[],
                "failed":Value::Null,
                "created_unix_ms":Value::Null,
                "message":"没有还原记录。",
            }),
        }
    }

    fn warning_for_availability(&self) -> Option<String> {
        if !self.enabled() {
            return None;
        }
        if self.enabled_state() == Enabled::Unreadable {
            return Some(
                "工作区恢复点配置无法读取；恢复点保持开启，文件改动前会尝试本地备份".into(),
            );
        }
        let Some(git) = &self.git else {
            return Some(git_unavailable_warning());
        };
        if let Some(program) = &self.config.git_program
            && !program.is_file()
        {
            return Some(format!(
                "配置的 Git 程序无法使用：{}；已建立的恢复点保留，修改前改用本地备份",
                program.display()
            ));
        }
        if let Err(error) = git_version(git) {
            return Some(format!(
                "检测到的 Git 程序无法使用：{error}；已建立的恢复点保留，修改前改用本地备份"
            ));
        }
        None
    }

    fn warn_once(&self, message: &str) -> bool {
        if !self.mark_warned() || self.warned.swap(true, Ordering::AcqRel) {
            return false;
        }
        eprintln!("{message}");
        true
    }

    fn mark_warned(&self) -> bool {
        static WARNED: std::sync::OnceLock<Mutex<BTreeSet<PathBuf>>> = std::sync::OnceLock::new();
        WARNED
            .get_or_init(|| Mutex::new(BTreeSet::new()))
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(self.workspace.clone())
    }

    /// Establish a point if the current snapshot content differs from the last
    /// one. The caller must hold both the workspace and recovery locks.
    fn ensure_point_locked(
        &self,
        git: &Path,
        reason: &str,
    ) -> Result<SnapshotOutcome, RecoveryError> {
        self.initialize_repository(git)?;
        self.write_excludes()?;
        let (tree, out_of_scope, out_of_scope_sample, complete) = self.stage_tree(git)?;
        let previous = self.ref_commit(git)?;
        if let Some(previous) = &previous {
            let previous_tree =
                self.run_text(git, &["rev-parse", &format!("{previous}^{{tree}}")])?;
            if previous_tree.trim() == tree {
                return Ok(SnapshotOutcome::Ready {
                    id: previous.clone(),
                    created: false,
                    complete,
                    out_of_scope,
                    out_of_scope_sample,
                    warning: incomplete_warning(complete, out_of_scope),
                });
            }
        }
        let message = commit_message(reason);
        let mut arguments = vec!["commit-tree".to_string(), tree.clone()];
        if let Some(previous) = &previous {
            arguments.push("-p".into());
            arguments.push(previous.clone());
        }
        arguments.push("-m".into());
        arguments.push(message);
        let commit = self.run_text_owned(git, &arguments)?;
        let commit = commit.trim().to_owned();
        validate_object_id(&commit)?;
        // Compare-and-swap against the commit we compared the tree with. A
        // competing writer that produced the same tree is reused; otherwise the
        // new commit remains reachable through the branch it wrote.
        let update = self.run_status(
            git,
            &[
                "update-ref",
                RECOVERY_REF,
                &commit,
                previous.as_deref().unwrap_or(""),
            ],
        )?;
        if !update {
            if let Some(current) = self.ref_commit(git)? {
                let current_tree =
                    self.run_text(git, &["rev-parse", &format!("{current}^{{tree}}")])?;
                if current_tree.trim() == tree {
                    return Ok(SnapshotOutcome::Ready {
                        id: current,
                        created: false,
                        complete,
                        out_of_scope,
                        out_of_scope_sample,
                        warning: incomplete_warning(complete, out_of_scope),
                    });
                }
            }
            return Err(RecoveryError::known(
                "recovery branch changed while the point was created",
            ));
        }
        let state = RecoveryState {
            schema: RECOVERY_SCHEMA.into(),
            last_snapshot: Some(commit.clone()),
            complete,
            out_of_scope,
            out_of_scope_sample: out_of_scope_sample.clone(),
            created_unix_ms: unix_ms(),
        };
        let warning = self
            .write_state(&state)
            .err()
            .map(|error| format!("恢复点已建立，但状态记录写入失败：{error}"));
        let warning = match (warning, incomplete_warning(complete, out_of_scope)) {
            (Some(existing), Some(incomplete)) => Some(format!("{existing}；{incomplete}")),
            (existing, incomplete) => existing.or(incomplete),
        };
        Ok(SnapshotOutcome::Ready {
            id: commit,
            created: true,
            complete,
            out_of_scope,
            out_of_scope_sample,
            warning,
        })
    }

    /// Establish a recovery point now. Content identical to the previous point
    /// is not committed again. Git unavailability is a warning, not a failure,
    /// so ordinary work continues.
    pub fn ensure_point(&self, reason: &str) -> Result<SnapshotOutcome, RecoveryError> {
        if !self.enabled() {
            return Ok(SnapshotOutcome::Disabled);
        }
        self.config
            .limits
            .validate()
            .map_err(RecoveryError::known)?;
        if let Some(warning) = self.warning_for_availability() {
            self.warn_once(&warning);
            return Ok(SnapshotOutcome::Unavailable { warning });
        }
        let git = self
            .git
            .as_deref()
            .ok_or_else(|| RecoveryError::known("Git is unavailable"))?;
        let _recovery_lock = self.acquire_lock(&self.recovery_dir().join(RECOVERY_LOCK))?;
        let _workspace_lock = self.acquire_workspace_lock()?;
        self.ensure_point_locked(git, reason)
    }

    /// Returns the newest point, creating one only when recovery is enabled and
    /// Git is available. Unlike [`Self::ensure_point`], unavailable Git is an
    /// error because restoration cannot proceed without it.
    pub fn require_point(&self, reason: &str) -> Result<String, RecoveryError> {
        if !self.enabled() {
            return Err(RecoveryError::known(
                "Git recovery is disabled for this workspace",
            ));
        }
        if let Some(warning) = self.warning_for_availability() {
            return Err(RecoveryError::known(warning));
        }
        let git = self
            .git
            .as_deref()
            .ok_or_else(|| RecoveryError::known("Git is unavailable"))?;
        let _recovery_lock = self.acquire_lock(&self.recovery_dir().join(RECOVERY_LOCK))?;
        let _workspace_lock = self.acquire_workspace_lock()?;
        match self.ensure_point_locked(git, reason)? {
            SnapshotOutcome::Ready { id, .. } => Ok(id),
            SnapshotOutcome::Disabled => Err(RecoveryError::known(
                "Git recovery is disabled for this workspace",
            )),
            SnapshotOutcome::Unavailable { warning } => Err(RecoveryError::known(warning)),
        }
    }

    /// Point ids, newest first. Read-only. No point is ever deleted.
    pub fn point_ids(&self) -> Result<Vec<String>, String> {
        if !self.repository().is_dir() {
            return Ok(vec![]);
        }
        let git = self
            .git
            .as_ref()
            .ok_or_else(|| "Git is unavailable".to_string())?;
        let verify = self
            .run_status_output(git, &["show-ref", "--verify", "--quiet", RECOVERY_REF])
            .map_err(|error| error.message)?;
        if !verify.status.success() {
            let stderr = String::from_utf8_lossy(&verify.stderr).trim().to_owned();
            if verify.status.code() == Some(1) && stderr.is_empty() {
                // Valid repository initialized without commits on refs/heads/recovery yet.
                return Ok(vec![]);
            }
            return Err(if stderr.is_empty() {
                "Git recovery repository reference check failed".to_string()
            } else {
                format!("Git recovery repository is unreadable: {stderr}")
            });
        }
        let output = self
            .run_status_output(git, &["rev-list", RECOVERY_REF])
            .map_err(|error| error.message)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(if stderr.is_empty() {
                "Git recovery point listing failed".to_string()
            } else {
                format!("Git recovery point listing failed: {stderr}")
            });
        }
        let text = String::from_utf8(output.stdout)
            .map_err(|_| "Git returned a non-UTF8 point list".to_string())?;
        let mut ids = vec![];
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            validate_object_id(line).map_err(|error| error.message)?;
            ids.push(line.to_owned());
        }
        Ok(ids)
    }

    /// Establish a point before a capability that changes the workspace.
    /// Failures never block the capability; they produce warnings.
    pub fn before_capability(&self, task_id: &str, name: &str, arguments: &Value) -> Vec<String> {
        if !self.enabled() || !mutates_workspace(name) {
            return vec![];
        }
        let mut warnings = vec![];
        let first = {
            let mut tasks = self
                .attempted_tasks
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            tasks.insert(task_id.to_owned())
        };
        if first || multi_file_change(name) {
            match self.ensure_point(&format!("before {name}")) {
                Ok(outcome) => {
                    if let Some(warning) = outcome.warning()
                        && self.warn_once(warning)
                    {
                        warnings.push(warning.to_owned());
                    }
                }
                Err(error) => {
                    let warning = format!(
                        "无法建立恢复点：{}；继续执行，并根据本地备份核对",
                        error.message
                    );
                    if self.warn_once(&warning) {
                        warnings.push(warning);
                    }
                }
            }
        }
        // The batch journal already stores every old and candidate byte under a
        // bounded limit. Direct single-path operations need an explicit copy for
        // content that the snapshot intentionally excludes. This warning is
        // reported for each affected call: it names a real backup destination,
        // and the one-time snapshot warning must not hide it.
        for path in direct_backup_paths(name, arguments) {
            if let Some(warning) = self.backup_out_of_scope(&path) {
                warnings.push(warning);
            }
        }
        warnings
    }

    fn backup_out_of_scope(&self, path: &str) -> Option<String> {
        if !self.enabled() || validate_relative(path).is_err() {
            return None;
        }
        let full = self.workspace.join(path);
        let metadata = fs::symlink_metadata(&full).ok()?;
        if self.snapshot_contains(path).unwrap_or(false) {
            return None;
        }
        match self.backup_path(path, &full, &metadata) {
            Ok(Some(destination)) => Some(format!(
                "文件不在恢复点范围内，已在修改前保存本地备份：{} -> {}",
                path,
                destination.display()
            )),
            Ok(None) => None,
            Err(error) => Some(format!(
                "文件不在恢复点范围内，本地备份失败：{path}：{error}；按不在恢复范围处理"
            )),
        }
    }

    fn snapshot_contains(&self, path: &str) -> Result<bool, String> {
        let Some(git) = self.git.as_deref() else {
            return Ok(false);
        };
        let Some(commit) = self.ref_commit(git).map_err(|error| error.message)? else {
            return Ok(false);
        };
        let output = self
            .run_status_output(git, &["ls-tree", "-z", &commit, "--", path])
            .map_err(|error| error.message)?;
        Ok(output.status.success() && !output.stdout.is_empty())
    }

    fn backup_path(
        &self,
        relative: &str,
        full: &Path,
        metadata: &fs::Metadata,
    ) -> Result<Option<PathBuf>, String> {
        let root = self.recovery_dir().join(BACKUP_DIR);
        let id = format!(
            "{}-{:x}",
            unix_ms(),
            Sha256::digest(relative.as_bytes())[..8]
                .iter()
                .fold(0u64, |acc, byte| { (acc << 8) | u64::from(*byte) })
        );
        let destination = root.join(&id);
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(full).map_err(|error| error.to_string())?;
            create_private_dir(&destination.join("root")).map_err(|error| error.message)?;
            write_private(
                &destination.join("root").join("link"),
                target.as_os_str().as_encoded_bytes(),
            )?;
            write_private(
                &destination.join("manifest.json"),
                &serde_json::to_vec(&json!({
                    "schema":"cayw.recovery-backup.v1",
                    "path":relative,
                    "kind":"symlink",
                    "target":target,
                }))
                .map_err(|error| error.to_string())?,
            )?;
            return Ok(Some(destination));
        }
        if metadata.is_file() {
            if metadata.len() > self.config.limits.max_backup_bytes {
                return Err("file exceeds the local backup byte limit".into());
            }
            create_private_dir(&destination.join("root")).map_err(|error| error.message)?;
            let bytes = read_bounded(full, self.config.limits.max_backup_bytes)?;
            write_private(&destination.join("root").join(file_name(full)), &bytes)?;
            write_private(
                &destination.join("manifest.json"),
                &serde_json::to_vec(&json!({
                    "schema":"cayw.recovery-backup.v1",
                    "path":relative,
                    "kind":"file",
                    "bytes":bytes.len(),
                }))
                .map_err(|error| error.to_string())?,
            )?;
            return Ok(Some(destination));
        }
        if metadata.is_dir() {
            create_private_dir(&destination.join("root")).map_err(|error| error.message)?;
            let mut used = 0u64;
            let mut entries = 0usize;
            copy_tree_bounded(
                full,
                &destination.join("root"),
                &mut used,
                &mut entries,
                self.config.limits.max_backup_bytes,
            )?;
            write_private(
                &destination.join("manifest.json"),
                &serde_json::to_vec(&json!({
                    "schema":"cayw.recovery-backup.v1",
                    "path":relative,
                    "kind":"directory",
                    "entries":entries,
                    "bytes":used,
                }))
                .map_err(|error| error.to_string())?,
            )?;
            return Ok(Some(destination));
        }
        Ok(None)
    }

    /// Restore exact files from one CAYW point. Every target and conflict is
    /// checked before the first write. The pre-restore state is itself a point,
    /// and an interrupted restore stays marked `outcome_unknown`.
    pub fn restore(&self, request: RestoreRequest) -> Result<RestoreReport, RecoveryError> {
        if !self.enabled() {
            return Err(RecoveryError::known(
                "Git recovery is disabled for this workspace",
            ));
        }
        self.config
            .limits
            .validate()
            .map_err(RecoveryError::known)?;
        if let Some(warning) = self.warning_for_availability() {
            return Err(RecoveryError::known(warning));
        }
        if request.paths.is_empty() {
            return Err(RecoveryError::known(
                "restore requires at least one workspace-relative file path",
            ));
        }
        validate_object_id(&request.snapshot)?;
        let mut unique = BTreeSet::new();
        for path in &request.paths {
            validate_relative(path)?;
            if !unique.insert(path.clone()) {
                return Err(RecoveryError::known("restore paths must be unique"));
            }
        }
        let confirm: BTreeSet<&str> = request
            .confirm_overwrite
            .iter()
            .map(String::as_str)
            .collect();
        for path in &confirm {
            validate_relative(path)?;
        }
        let git = self
            .git
            .as_deref()
            .ok_or_else(|| RecoveryError::known("Git is unavailable"))?;
        let _recovery_lock = self.acquire_lock(&self.recovery_dir().join(RECOVERY_LOCK))?;
        let _workspace_lock = self.acquire_workspace_lock()?;
        if self.pending_change_batch()? {
            return Err(RecoveryError::known(
                "a change batch is still in progress; recovery is mutually exclusive",
            ));
        }
        if !self.is_our_point(git, &request.snapshot)? {
            return Err(RecoveryError::known(
                "the requested snapshot is not a recovery point of this workspace",
            ));
        }
        let undo = match self.ensure_point_locked(git, "before restore")? {
            SnapshotOutcome::Ready { id, .. } => Some(id),
            _ => None,
        };
        struct Desired {
            path: String,
            mode: u32,
            bytes: Vec<u8>,
        }
        let mut desired = vec![];
        let mut not_in_snapshot = vec![];
        let mut total = 0u64;
        for path in &request.paths {
            match self.tree_blob(git, &request.snapshot, path)? {
                Some((mode, bytes)) => {
                    total = total
                        .checked_add(bytes.len() as u64)
                        .ok_or_else(|| RecoveryError::known("restore byte count overflow"))?;
                    if total > self.config.limits.max_bytes {
                        return Err(RecoveryError::known(
                            "restore content exceeds the configured byte limit",
                        ));
                    }
                    desired.push(Desired {
                        path: path.clone(),
                        mode,
                        bytes,
                    });
                }
                None => not_in_snapshot.push(path.clone()),
            }
        }
        let mut needs_confirmation = vec![];
        let mut unchanged = vec![];
        for entry in &desired {
            match self.current_state(&entry.path, entry.mode, &entry.bytes) {
                Ok(Current::Same) => unchanged.push(entry.path.clone()),
                Ok(Current::Absent) => {
                    // Restoring a file that is now missing is the primary
                    // recovery case; there is no existing content to lose.
                }
                Ok(Current::Different) => {
                    if !confirm.contains(entry.path.as_str()) {
                        needs_confirmation.push(entry.path.clone());
                    }
                }
                Err(error) => {
                    return Err(RecoveryError::known(format!(
                        "cannot read current path {}: {error}",
                        entry.path
                    )));
                }
            }
        }
        if !needs_confirmation.is_empty() {
            return Ok(RestoreReport {
                snapshot: request.snapshot,
                restored: vec![],
                unchanged,
                needs_confirmation,
                not_in_snapshot,
                undo_snapshot: undo,
                complete: false,
            });
        }
        let mut intent = RestoreIntent {
            schema: RECOVERY_SCHEMA.into(),
            snapshot: request.snapshot.clone(),
            status: "intent".into(),
            paths: request.paths.clone(),
            installed: vec![],
            failed: None,
            outcome_unknown: false,
            created_unix_ms: unix_ms(),
        };
        self.write_restore_intent(&intent)?;
        for entry in &desired {
            if unchanged.iter().any(|path| path == &entry.path) {
                continue;
            }
            if let Err(error) = self.install_restored(&entry.path, entry.mode, &entry.bytes) {
                intent.status = "outcome_unknown".into();
                intent.outcome_unknown = true;
                intent.failed = Some(entry.path.clone());
                let _ = self.write_restore_intent(&intent);
                return Err(RecoveryError::unknown(format!(
                    "restore was interrupted after {} path(s): {error}",
                    intent.installed.len()
                )));
            }
            intent.installed.push(entry.path.clone());
            if let Err(error) = self.write_restore_intent(&intent) {
                intent.status = "outcome_unknown".into();
                intent.outcome_unknown = true;
                return Err(RecoveryError::unknown(format!(
                    "restore install record was not durable: {error}"
                )));
            }
        }
        intent.status = "completed".into();
        self.write_restore_intent(&intent)?;
        Ok(RestoreReport {
            snapshot: request.snapshot,
            restored: desired
                .iter()
                .map(|entry| entry.path.clone())
                .filter(|path| !unchanged.contains(path))
                .collect(),
            unchanged,
            needs_confirmation: vec![],
            not_in_snapshot,
            undo_snapshot: undo,
            complete: true,
        })
    }

    fn is_our_point(&self, git: &Path, snapshot: &str) -> Result<bool, RecoveryError> {
        let output = self.run_status_output(
            git,
            &["merge-base", "--is-ancestor", snapshot, RECOVERY_REF],
        )?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(RecoveryError::known(format!(
                "Git could not verify the recovery point: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))),
        }
    }

    fn tree_blob(
        &self,
        git: &Path,
        snapshot: &str,
        path: &str,
    ) -> Result<Option<(u32, Vec<u8>)>, RecoveryError> {
        let spec = format!("{snapshot}:{path}");
        let output = self.run_status_output(git, &["ls-tree", "-z", snapshot, "--", path])?;
        if !output.status.success() || output.stdout.is_empty() {
            return Ok(None);
        }
        let record = output.stdout.strip_suffix(&[0]).unwrap_or(&output.stdout);
        let at = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| RecoveryError::known("Git tree record is malformed"))?;
        let (metadata, returned) = (&record[..at], &record[at + 1..]);
        let returned = std::str::from_utf8(returned)
            .map_err(|_| RecoveryError::known("Git tree path is not UTF-8"))?;
        if returned != path {
            return Ok(None);
        }
        let metadata = std::str::from_utf8(metadata)
            .map_err(|_| RecoveryError::known("Git tree metadata is not UTF-8"))?;
        let mut fields = metadata.split(' ');
        let mode = fields.next().unwrap_or_default();
        let kind = fields.next().unwrap_or_default();
        let oid = fields.next().unwrap_or_default();
        validate_object_id(oid)?;
        let mode = match mode {
            "100644" => 0o644,
            "100755" => 0o755,
            "120000" => 0o777,
            _ => {
                return Err(RecoveryError::known(format!(
                    "{path} is not a regular file or symbolic link in the recovery point"
                )));
            }
        };
        if kind != "blob" {
            return Err(RecoveryError::known(format!(
                "{path} is not a file in the recovery point"
            )));
        }
        let size = self.run_text(git, &["cat-file", "-s", oid])?;
        let size = size
            .trim()
            .parse::<u64>()
            .map_err(|_| RecoveryError::known("Git returned an invalid blob size"))?;
        if size > self.config.limits.max_file_bytes {
            return Err(RecoveryError::known(format!(
                "{path} exceeds the recovery file byte limit"
            )));
        }
        let bytes = self.run_output(git, &["cat-file", "blob", oid])?.stdout;
        if bytes.len() as u64 != size {
            return Err(RecoveryError::known("Git blob size changed"));
        }
        let _ = spec;
        Ok(Some((mode, bytes)))
    }

    fn current_state(&self, path: &str, mode: u32, desired: &[u8]) -> Result<Current, String> {
        let full = self.workspace.join(path);
        let metadata = match fs::symlink_metadata(&full) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Current::Absent),
            Err(error) => return Err(error.to_string()),
        };
        if mode == 0o777 {
            if !metadata.file_type().is_symlink() {
                return Ok(Current::Different);
            }
            let target = fs::read_link(&full).map_err(|error| error.to_string())?;
            return Ok(if target.as_os_str().as_encoded_bytes() == desired {
                Current::Same
            } else {
                Current::Different
            });
        }
        if !metadata.is_file() || metadata.len() > self.config.limits.max_file_bytes {
            return Ok(Current::Different);
        }
        let current = read_bounded(&full, self.config.limits.max_file_bytes)?;
        Ok(if current == desired {
            Current::Same
        } else {
            Current::Different
        })
    }

    fn install_restored(&self, path: &str, mode: u32, desired: &[u8]) -> Result<(), String> {
        let relative = validate_relative(path).map_err(|error| error.message)?;
        let parent = ensure_parent_dirs(&self.workspace, &relative)?;
        let full = self.workspace.join(path);
        let existing = fs::symlink_metadata(&full);
        if existing.as_ref().is_ok_and(|metadata| metadata.is_dir()) {
            return Err("a directory occupies the restore target".into());
        }
        if mode == 0o777 {
            if existing.is_ok() {
                fs::remove_file(&full).map_err(|error| error.to_string())?;
            }
            let target = std::str::from_utf8(desired)
                .map_err(|_| "symlink target is not UTF-8".to_string())?;
            std::os::unix::fs::symlink(target, &full).map_err(|error| error.to_string())?;
            File::open(&parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| error.to_string())?;
            return Ok(());
        }
        let name = format!(
            ".cayw-host-tmp-recovery-{}-{}",
            std::process::id(),
            unique_suffix()
        );
        let temporary = parent.join(name);
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true).mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW);
            let mut file = options
                .open(&temporary)
                .map_err(|error| error.to_string())?;
            file.write_all(desired).map_err(|error| error.to_string())?;
            file.set_permissions(fs::Permissions::from_mode(mode))
                .map_err(|error| error.to_string())?;
            file.sync_all().map_err(|error| error.to_string())?;
            fs::rename(&temporary, &full).map_err(|error| error.to_string())?;
            File::open(&parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| error.to_string())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn pending_change_batch(&self) -> Result<bool, RecoveryError> {
        let store = self.workspace.join(".cayw-workspace");
        let entries = match fs::read_dir(&store) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(RecoveryError::known(format!(
                    "cannot inspect change batches: {error}"
                )));
            }
        };
        for entry in entries {
            let entry = entry.map_err(|error| RecoveryError::known(error.to_string()))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("b-") || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let state = entry.path().join("state.json");
            let bytes = match read_bounded(&state, 4096) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            if matches!(
                value.get("phase").and_then(Value::as_str),
                Some(
                    "intent"
                        | "committing"
                        | "recovering_commit"
                        | "recovering_rollback"
                        | "outcome_unknown"
                )
            ) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn initialize_repository(&self, git: &Path) -> Result<(), RecoveryError> {
        let recovery = self.recovery_dir();
        create_private_dir(&recovery)?;
        create_private_dir(&recovery.join(EMPTY_HOME_DIR))?;
        create_private_dir(&recovery.join(EMPTY_HOOKS_DIR))?;
        let repository = self.repository();
        if !repository.is_dir() {
            let mut command = self.raw_command(git);
            command
                .arg("init")
                .arg("--bare")
                .arg("--quiet")
                .arg(&repository);
            let output = command
                .output()
                .map_err(|error| RecoveryError::known(format!("cannot start Git: {error}")))?;
            if !output.status.success() {
                return Err(RecoveryError::known(format!(
                    "cannot initialize the recovery repository: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
        }
        let config = recovery.join(CONFIG_FILE);
        if !config.exists() {
            self.write_config(&json!({
                "schema":RECOVERY_SCHEMA,
                "enabled":true,
            }))?;
        }
        Ok(())
    }

    fn write_config(&self, value: &Value) -> Result<(), RecoveryError> {
        write_atomic(
            &self.recovery_dir().join(CONFIG_FILE),
            &serde_json::to_vec_pretty(value)
                .map_err(|error| RecoveryError::known(error.to_string()))?,
            0o600,
        )
        .map_err(|error| RecoveryError::known(format!("cannot write recovery config: {error}")))
    }

    fn write_state(&self, state: &RecoveryState) -> Result<(), String> {
        write_atomic(
            &self.recovery_dir().join(STATE_FILE),
            &serde_json::to_vec(state).map_err(|error| error.to_string())?,
            0o600,
        )
        .map_err(|error| error.to_string())
    }

    fn read_state(&self) -> Result<Option<RecoveryState>, String> {
        let path = self.recovery_dir().join(STATE_FILE);
        match fs::metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("恢复点状态文件无法读取：{error}")),
            Ok(_) => {}
        }
        let bytes = read_bounded(&path, 64 * 1024)
            .map_err(|error| format!("恢复点状态文件无法读取：{error}"))?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| format!("恢复点状态文件损坏：{error}"))
    }

    fn write_restore_intent(&self, intent: &RestoreIntent) -> Result<(), RecoveryError> {
        write_atomic(
            &self.recovery_dir().join(RESTORE_INTENT_FILE),
            &serde_json::to_vec(intent).map_err(|error| RecoveryError::known(error.to_string()))?,
            0o600,
        )
        .map_err(|error| RecoveryError::known(format!("cannot write restore intent: {error}")))
    }

    fn write_excludes(&self) -> Result<(), RecoveryError> {
        let repository = self.repository();
        let info = repository.join("info");
        create_private_dir(&info)?;
        let mut patterns = String::from("# Managed by CAYW; do not edit.\n");
        for name in PROTECTED_NAMES {
            patterns.push_str(name);
            patterns.push('\n');
            patterns.push_str(name);
            patterns.push_str("/\n");
        }
        for prefix in PROTECTED_PREFIXES {
            patterns.push_str(prefix);
            patterns.push_str("*\n");
        }
        for excluded in [Some(self.repository()), self.config.data_dir.clone()]
            .into_iter()
            .flatten()
        {
            if let Some(relative) = relative_inside(&self.workspace, &excluded) {
                patterns.push('/');
                patterns.push_str(&relative);
                patterns.push_str("/\n");
            }
        }
        write_atomic(&info.join("exclude"), patterns.as_bytes(), 0o600)
            .map_err(|error| RecoveryError::known(format!("cannot write Git excludes: {error}")))
    }

    fn stage_tree(&self, git: &Path) -> Result<(String, usize, Vec<String>, bool), RecoveryError> {
        self.run_status(git, &["add", "-A", "--", "."])?;
        let listing = self.run_output(git, &["ls-files", "-s", "-z"])?.stdout;
        let mut records = vec![];
        for record in listing.split(|byte| *byte == 0) {
            if record.is_empty() {
                continue;
            }
            let at = record
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or_else(|| RecoveryError::known("Git index record is malformed"))?;
            let (metadata, path) = (&record[..at], &record[at + 1..]);
            let metadata = std::str::from_utf8(metadata)
                .map_err(|_| RecoveryError::known("Git index metadata is not UTF-8"))?;
            let path = std::str::from_utf8(path)
                .map_err(|_| RecoveryError::known("workspace path is not UTF-8"))?;
            let mut fields = metadata.split(' ');
            let mode = fields.next().unwrap_or_default();
            let oid = fields.next().unwrap_or_default();
            let stage = fields.next().unwrap_or_default();
            validate_object_id(oid)?;
            if stage != "0" {
                return Err(RecoveryError::known("Git index has an unmerged entry"));
            }
            records.push((mode.to_owned(), path.to_owned()));
        }
        records.sort_by(|left, right| left.1.cmp(&right.1));
        let mut excluded = BTreeSet::new();
        let mut files = 0usize;
        let mut bytes = 0u64;
        for (mode, path) in records {
            let valid_mode = matches!(mode.as_str(), "100644" | "100755" | "120000");
            let size = fs::symlink_metadata(self.workspace.join(&path))
                .ok()
                .map(|metadata| metadata.len());
            // A nested repository (160000), an unsupported mode, a protected
            // name, an unreadable path or a file over the snapshot limits stays
            // outside the snapshot and is reported as out of scope.
            let drop = mode == "160000"
                || !valid_mode
                || path.split('/').any(is_protected)
                || size.is_none()
                || size.is_some_and(|size| {
                    size > self.config.limits.max_file_bytes
                        || files + 1 > self.config.limits.max_files
                        || bytes.saturating_add(size) > self.config.limits.max_bytes
                });
            if drop {
                excluded.insert(path);
            } else if let Some(size) = size {
                files += 1;
                bytes += size;
            }
        }
        // Git-ignored entries are absent from the snapshot for the same reason
        // as nested repositories and over-limit files: recovery cannot restore
        // them, so they must not count as recovery coverage. `--directory` keeps
        // whole ignored trees as one compact sample entry.
        let ignored = self.run_output(
            git,
            &[
                "ls-files",
                "-z",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
            ],
        )?;
        for record in ignored.stdout.split(|byte| *byte == 0) {
            if record.is_empty() {
                continue;
            }
            let path = std::str::from_utf8(record)
                .map_err(|_| RecoveryError::known("ignored workspace path is not UTF-8"))?
                .trim_end_matches('/');
            if path.is_empty() || path.split('/').any(is_protected) || self.is_internal_path(path) {
                continue;
            }
            excluded.insert(path.to_owned());
        }
        let excluded = excluded.into_iter().collect::<Vec<_>>();
        for chunk in excluded.chunks(128) {
            // `update-index --force-remove` drops the index entry without
            // touching the working tree and also handles gitlinks (nested
            // repositories), which `git rm --cached` refuses when their staged
            // content differs from HEAD.
            let mut arguments = vec!["--force-remove".to_string(), "--".to_string()];
            arguments.extend(chunk.iter().cloned());
            self.run_status_owned(git, "update-index", &arguments)?;
        }
        let tree = self.run_text(git, &["write-tree"])?;
        let tree = tree.trim().to_owned();
        validate_object_id(&tree)?;
        let count = excluded.len();
        let sample = excluded.into_iter().take(64).collect();
        Ok((tree, count, sample, count == 0))
    }

    /// True when a workspace-relative path is the private recovery repository,
    /// its state directory or the device data directory written inside the
    /// workspace. Those are managed by the program, not user content.
    fn is_internal_path(&self, path: &str) -> bool {
        [Some(self.repository()), self.config.data_dir.clone()]
            .into_iter()
            .flatten()
            .filter_map(|candidate| relative_inside(&self.workspace, &candidate))
            .any(|relative| {
                path == relative
                    || path
                        .strip_prefix(&relative)
                        .is_some_and(|rest| rest.starts_with('/'))
            })
    }

    fn ref_commit(&self, git: &Path) -> Result<Option<String>, RecoveryError> {
        let output =
            self.run_status_output(git, &["rev-parse", "--verify", "--quiet", RECOVERY_REF])?;
        if !output.status.success() {
            return Ok(None);
        }
        let value = String::from_utf8(output.stdout)
            .map_err(|_| RecoveryError::known("Git returned a non-UTF8 commit"))?;
        let value = value.trim();
        if value.is_empty() {
            return Ok(None);
        }
        validate_object_id(value)?;
        Ok(Some(value.to_owned()))
    }

    fn run_status(&self, git: &Path, arguments: &[&str]) -> Result<bool, RecoveryError> {
        let output = self.run_output(git, arguments)?;
        if output.status.success() {
            Ok(true)
        } else {
            Err(RecoveryError::known(format!(
                "Git {} failed: {}",
                arguments.first().copied().unwrap_or("command"),
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    fn run_status_owned(
        &self,
        git: &Path,
        subcommand: &str,
        arguments: &[String],
    ) -> Result<(), RecoveryError> {
        let mut command = self.command(git);
        command.arg(subcommand);
        for argument in arguments {
            command.arg(argument);
        }
        let output = command
            .output()
            .map_err(|error| RecoveryError::known(format!("cannot start Git: {error}")))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(RecoveryError::known(format!(
                "Git {subcommand} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    fn run_status_output(
        &self,
        git: &Path,
        arguments: &[&str],
    ) -> Result<std::process::Output, RecoveryError> {
        self.run_output(git, arguments)
    }

    fn run_output(
        &self,
        git: &Path,
        arguments: &[&str],
    ) -> Result<std::process::Output, RecoveryError> {
        let mut command = self.command(git);
        for argument in arguments {
            command.arg(argument);
        }
        let output = command
            .output()
            .map_err(|error| RecoveryError::known(format!("cannot start Git: {error}")))?;
        let total = output.stdout.len().saturating_add(output.stderr.len());
        if total > 64 * 1024 * 1024 {
            return Err(RecoveryError::known("Git output exceeds its byte limit"));
        }
        Ok(output)
    }

    fn run_text(&self, git: &Path, arguments: &[&str]) -> Result<String, RecoveryError> {
        let output = self.run_output(git, arguments)?;
        if !output.status.success() {
            return Err(RecoveryError::known(format!(
                "Git {} failed: {}",
                arguments.first().copied().unwrap_or("command"),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        String::from_utf8(output.stdout)
            .map_err(|_| RecoveryError::known("Git returned non-UTF8 output"))
    }

    fn run_text_owned(&self, git: &Path, arguments: &[String]) -> Result<String, RecoveryError> {
        let mut command = self.command(git);
        for argument in arguments {
            command.arg(argument);
        }
        let output = command
            .output()
            .map_err(|error| RecoveryError::known(format!("cannot start Git: {error}")))?;
        if !output.status.success() {
            return Err(RecoveryError::known(format!(
                "Git {} failed: {}",
                arguments.first().map(String::as_str).unwrap_or("command"),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        String::from_utf8(output.stdout)
            .map_err(|_| RecoveryError::known("Git returned non-UTF8 output"))
    }

    fn command(&self, git: &Path) -> std::process::Command {
        let mut command = self.raw_command(git);
        let repository = self.repository();
        command
            .arg("--git-dir")
            .arg(&repository)
            .arg("--work-tree")
            .arg(&self.workspace)
            .env("GIT_DIR", &repository)
            .env("GIT_WORK_TREE", &self.workspace)
            .env("GIT_INDEX_FILE", repository.join("index"));
        command
    }

    fn raw_command(&self, git: &Path) -> std::process::Command {
        let mut command = std::process::Command::new(git);
        command
            .current_dir(&self.workspace)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/bin/false")
            .env("SSH_ASKPASS", "/bin/false")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env("LC_ALL", "C")
            .env("HOME", self.recovery_dir().join(EMPTY_HOME_DIR))
            .env("GIT_AUTHOR_NAME", "CAYW")
            .env("GIT_AUTHOR_EMAIL", "cayw@localhost")
            .env("GIT_COMMITTER_NAME", "CAYW")
            .env("GIT_COMMITTER_EMAIL", "cayw@localhost");
        for variable in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_AUTHOR_DATE",
            "GIT_COMMITTER_DATE",
        ] {
            command.env_remove(variable);
        }
        command
            .arg("-c")
            .arg(format!(
                "core.hooksPath={}",
                self.recovery_dir().join(EMPTY_HOOKS_DIR).display()
            ))
            .arg("-c")
            .arg("core.fsmonitor=false")
            .arg("-c")
            .arg("core.autocrlf=false")
            .arg("-c")
            .arg("core.safecrlf=false")
            .arg("-c")
            .arg("core.excludesFile=/dev/null")
            .arg("-c")
            .arg("gc.auto=0")
            .arg("-c")
            .arg("maintenance.auto=false")
            .arg("-c")
            .arg("commit.gpgsign=false")
            .arg("-c")
            .arg("user.name=CAYW")
            .arg("-c")
            .arg("user.email=cayw@localhost");
        command
    }

    fn acquire_lock(&self, path: &Path) -> Result<File, RecoveryError> {
        if let Some(parent) = path.parent() {
            create_private_dir(parent)?;
        }
        let file = open_lock_file(path)?;
        acquire_flock(&file)?;
        Ok(file)
    }

    fn acquire_workspace_lock(&self) -> Result<File, RecoveryError> {
        let path = self.workspace.join(".cayw-host-lock");
        let file = open_lock_file(&path)?;
        acquire_flock(&file)?;
        Ok(file)
    }
}

fn open_lock_file(path: &Path) -> Result<File, RecoveryError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).mode(0o600);
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .map_err(|error| RecoveryError::known(format!("cannot open lock file: {error}")))?;
    if !file
        .metadata()
        .map_err(|error| RecoveryError::known(error.to_string()))?
        .is_file()
    {
        return Err(RecoveryError::known("lock path must be a regular file"));
    }
    Ok(file)
}

fn acquire_flock(file: &File) -> Result<(), RecoveryError> {
    use std::os::unix::io::AsRawFd as _;
    loop {
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(RecoveryError::known(format!(
            "cannot lock the workspace for recovery: {error}"
        )));
    }
}

fn ensure_parent_dirs(workspace: &Path, relative: &[String]) -> Result<PathBuf, String> {
    let mut current = workspace.to_path_buf();
    for component in &relative[..relative.len().saturating_sub(1)] {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err("restore parent is not a real directory".into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|error| error.to_string())?;
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(current)
}

fn copy_tree_bounded(
    source: &Path,
    destination: &Path,
    used: &mut u64,
    entries: &mut usize,
    limit: u64,
) -> Result<(), String> {
    for entry in fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        *entries = entries.saturating_add(1);
        if *entries > 100_000 {
            return Err("local backup entry limit exceeded".into());
        }
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| error.to_string())?;
        let target = destination.join(entry.file_name());
        if metadata.file_type().is_symlink() {
            let link = fs::read_link(entry.path()).map_err(|error| error.to_string())?;
            std::os::unix::fs::symlink(link, &target).map_err(|error| error.to_string())?;
        } else if metadata.is_dir() {
            fs::create_dir(&target).map_err(|error| error.to_string())?;
            copy_tree_bounded(&entry.path(), &target, used, entries, limit)?;
        } else if metadata.is_file() {
            let bytes = metadata.len();
            *used = used
                .checked_add(bytes)
                .ok_or_else(|| "local backup byte count overflow".to_string())?;
            if *used > limit {
                return Err("local backup byte limit exceeded".into());
            }
            fs::copy(entry.path(), &target).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    if file.metadata().map_err(|error| error.to_string())?.len() > limit {
        return Err("file exceeds the byte limit".into());
    }
    let mut bytes = vec![];
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("file exceeds the byte limit".into());
    }
    Ok(bytes)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent).map_err(|error| error.message)?;
    }
    write_atomic(path, bytes, 0o600).map_err(|error| error.to_string())
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    create_private_dir(parent).map_err(|error| io::Error::other(error.message))?;
    let temporary = parent.join(format!(
        ".cayw-host-tmp-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(mode);
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_private_dir(path: &Path) -> Result<(), RecoveryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(RecoveryError::known(format!(
            "recovery path must be a real directory: {}",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                create_private_dir(parent)?;
            }
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(path).map_err(|error| {
                RecoveryError::known(format!("cannot create recovery directory: {error}"))
            })
        }
        Err(error) => Err(RecoveryError::known(error.to_string())),
    }
}

fn relative_inside(workspace: &Path, candidate: &Path) -> Option<String> {
    let workspace = fs::canonicalize(workspace).ok()?;
    let candidate = if candidate.exists() {
        fs::canonicalize(candidate).ok()?
    } else {
        let parent = candidate.parent()?;
        let name = candidate.file_name()?;
        fs::canonicalize(parent).ok()?.join(name)
    };
    let relative = candidate.strip_prefix(&workspace).ok()?;
    let text = relative.to_str()?;
    (!text.is_empty()).then(|| text.to_owned())
}

fn read_enabled(path: &Path) -> Enabled {
    match read_bounded(path, 16 * 1024) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => {
                if value.get("enabled").and_then(Value::as_bool) == Some(false) {
                    Enabled::No
                } else {
                    Enabled::Yes
                }
            }
            Err(_) => Enabled::Unreadable,
        },
        Err(_) if !path.exists() => Enabled::Yes,
        Err(_) => Enabled::Unreadable,
    }
}

fn validate_object_id(value: &str) -> Result<(), RecoveryError> {
    if (value.len() == 40 || value.len() == 64)
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        Ok(())
    } else {
        Err(RecoveryError::known("expected a full Git object ID"))
    }
}

fn validate_relative(path: &str) -> Result<Vec<String>, RecoveryError> {
    if path.is_empty() || path.len() > 4096 || path.chars().any(char::is_control) {
        return Err(RecoveryError::known("expected a workspace-relative path"));
    }
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return Err(RecoveryError::known("expected a workspace-relative path"));
    }
    let mut components = vec![];
    for component in candidate.components() {
        match component {
            Component::Normal(name) => {
                let name = name
                    .to_str()
                    .ok_or_else(|| RecoveryError::known("path is not UTF-8"))?;
                if name.is_empty() || name.len() > 255 || is_protected(name) {
                    return Err(RecoveryError::known(
                        "path contains a forbidden or protected component",
                    ));
                }
                components.push(name.to_owned());
            }
            _ => {
                return Err(RecoveryError::known(
                    "path must contain only ordinary components",
                ));
            }
        }
    }
    if components.is_empty() {
        return Err(RecoveryError::known("path must name an entry"));
    }
    Ok(components)
}

fn is_protected(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    PROTECTED_NAMES.contains(&lower.as_str())
        || PROTECTED_PREFIXES
            .iter()
            .any(|prefix| lower.starts_with(prefix))
}

fn mutates_workspace(name: &str) -> bool {
    matches!(
        name,
        "fs.write"
            | "fs.patch"
            | "fs.mkdir"
            | "fs.move"
            | "fs.remove"
            | "fs.prepare_change_batch"
            | "fs.submit_change_batch"
            | "fs.recover_change_batch"
            | "build.run"
            | "test.run"
            | "check.run"
            | "lint.run"
            | "format.run"
            | "git.stage"
            | "git.commit"
            | "git.branch"
            | "git.worktree"
            | "terminal.start"
    )
}

fn multi_file_change(name: &str) -> bool {
    matches!(
        name,
        "fs.prepare_change_batch"
            | "fs.submit_change_batch"
            | "fs.recover_change_batch"
            | "build.run"
            | "test.run"
            | "check.run"
            | "lint.run"
            | "format.run"
            | "git.stage"
            | "git.commit"
            | "git.branch"
            | "git.worktree"
            | "terminal.start"
    )
}

fn direct_backup_paths(name: &str, arguments: &Value) -> Vec<String> {
    match name {
        "fs.write" | "fs.patch" | "fs.remove" => arguments
            .get("path")
            .and_then(Value::as_str)
            .map(|path| vec![path.to_owned()])
            .unwrap_or_default(),
        "fs.move" => arguments
            .get("source")
            .and_then(Value::as_str)
            .map(|path| vec![path.to_owned()])
            .unwrap_or_default(),
        _ => vec![],
    }
}

fn commit_message(reason: &str) -> String {
    let mut reason: String = reason
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(160)
        .collect();
    if reason.trim().is_empty() {
        reason = "workspace change".into();
    }
    format!("CAYW recovery point: {}", reason.trim())
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:x}")
}

fn restore_record_unknown(message: &str) -> Value {
    json!({
        "schema":RECOVERY_SCHEMA,
        "read_only":true,
        "unfinished":true,
        "outcome_unknown":true,
        "status":"unknown",
        "snapshot":Value::Null,
        "installed":[],
        "pending":[],
        "failed":Value::Null,
        "created_unix_ms":Value::Null,
        "message":message,
    })
}

fn incomplete_warning(complete: bool, out_of_scope: usize) -> Option<String> {
    if complete {
        return None;
    }
    Some(format!(
        "恢复点未包含 {out_of_scope} 个被 Git 忽略、超出上限、不可读或位于嵌套仓库的文件；这些文件不在恢复范围内"
    ))
}

fn file_name(path: &Path) -> &Path {
    path.file_name()
        .map(Path::new)
        .unwrap_or(Path::new("entry"))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryState {
    schema: String,
    last_snapshot: Option<String>,
    complete: bool,
    out_of_scope: usize,
    #[serde(default)]
    out_of_scope_sample: Vec<String>,
    created_unix_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreIntent {
    schema: String,
    snapshot: String,
    status: String,
    paths: Vec<String>,
    installed: Vec<String>,
    failed: Option<String>,
    outcome_unknown: bool,
    created_unix_ms: u64,
}

/// Safe Git detection. macOS `/usr/bin/git` is a shim that can display a
/// Command Line Tools installation dialog, so it is never executed unless the
/// selected developer directory already contains Git. PATH candidates other
/// than that shim remain allowed (for example Homebrew Git).
pub fn detect_git_program() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = std::process::Command::new("/usr/bin/xcode-select")
            .arg("-p")
            .output()
            && output.status.success()
        {
            let developer = String::from_utf8_lossy(&output.stdout);
            let candidate = Path::new(developer.trim()).join("usr/bin/git");
            if candidate.is_file() && git_version(&candidate).is_ok() {
                return Some(candidate);
            }
        }
        let developer = std::env::var_os("DEVELOPER_DIR").map(PathBuf::from);
        if let Some(developer) = developer {
            let candidate = developer.join("usr/bin/git");
            if candidate.is_file() && git_version(&candidate).is_ok() {
                return Some(candidate);
            }
        }
        let path = std::env::var_os("PATH")?;
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join("git");
            if candidate == Path::new("/usr/bin/git") || !candidate.is_file() {
                continue;
            }
            // A user symlink can also point at the Command Line Tools shim.
            let resolved = fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
            if resolved == Path::new("/usr/bin/git") {
                continue;
            }
            if git_version(&candidate).is_ok() {
                return Some(candidate);
            }
        }
        None
    }
    #[cfg(not(target_os = "macos"))]
    {
        let path = std::env::var_os("PATH")?;
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join("git");
            if candidate.is_file() && git_version(&candidate).is_ok() {
                return Some(candidate);
            }
        }
        None
    }
}

fn git_version(program: &Path) -> Result<(), String> {
    let output = std::process::Command::new(program)
        .arg("--version")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

pub fn git_unavailable_warning() -> String {
    "检测不到可用的 Git；无法建立工作区恢复点，修改或删除文件前会尝试本地备份，工作继续".into()
}

/// The warning shown once when a local workspace opens.
pub fn availability_warning(workspace: &Path, state_dir: &Path) -> Option<String> {
    let points = RecoveryPoints::open(workspace, state_dir, RecoveryConfig::default());
    let warning = points.status()["warning"].as_str().map(str::to_owned)?;
    // Opening the workspace is the one place the product shows this warning.
    // Later snapshot attempts reuse the same process-wide record.
    points.mark_warned();
    Some(warning)
}

/// Whether this workspace's private configuration still enables recovery.
pub fn workspace_enabled(workspace: &Path, state_dir: &Path) -> bool {
    RecoveryPoints::open(workspace, state_dir, RecoveryConfig::default()).enabled()
}

/// Read-only detection used by `cayw doctor`; never creates workspace state.
pub fn doctor_check(workspace: &Path, state_dir: &Path) -> Value {
    let points = RecoveryPoints::open(workspace, state_dir, RecoveryConfig::default());
    let status = points.status();
    let warning = status["warning"].as_str();
    json!({
        "name":"git_recovery",
        "status":if warning.is_some() { "warning" } else { "ok" },
        "message":warning.unwrap_or("Git 恢复点可用；程序不会自动删除已建立的恢复点"),
        "details":status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_names_and_paths_are_rejected() {
        assert!(is_protected(".git"));
        assert!(is_protected(".CAYW-HOST-LOCK"));
        assert!(is_protected(".cayw-host-tmp-123"));
        assert!(validate_relative("../x").is_err());
        assert!(validate_relative("/tmp/x").is_err());
        assert!(validate_relative(".cayw-workspace/b-123").is_err());
        assert!(validate_relative("src/lib.rs").is_ok());
    }

    #[test]
    fn direct_backups_name_the_source_path_only() {
        assert_eq!(
            direct_backup_paths("fs.move", &json!({"source":"a","destination":"b"})),
            vec!["a".to_owned()]
        );
        assert!(direct_backup_paths("fs.submit_change_batch", &json!({})).is_empty());
    }
}
