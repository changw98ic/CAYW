//! Device-level approval rules. The file is user-owned and model-unwritable.
//!
//! A task captures one immutable snapshot at dispatch. If the current version
//! cannot be read, the snapshot is `Unavailable` and no rule may release a
//! capability; an older rule set is never reused. Exact user rules match the
//! capability name and the complete canonical argument object. The built-in
//! default categories below are deterministic host policy, not model output.

use crate::permissions::canonical_scope;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

pub const APPROVAL_RULES_FILE: &str = "approval-rules.json";
pub const MAX_RULES_BYTES: u64 = 1024 * 1024;
pub const MAX_RULES: usize = 4096;
/// The only text a user may send to persist a displayed pending rule.
pub const RULE_CONFIRM_REPLY: &str = "确认保存规则";
pub const RULE_CANCEL_REPLY: &str = "取消";

/// A rule modification shown to the user before anything is saved. The text is
/// assembled by the program; model output is only the proposed rule data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleChangeQuestion {
    pub session_id: String,
    pub pending_id: String,
    pub rule: ApprovalRule,
    pub text: String,
    pub options: Vec<&'static str>,
}

/// The result of answering a pending rule modification. `version` identifies
/// the exact on-disk rule set that was read (or written) for this answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleChangeOutcome {
    pub saved: bool,
    pub version: Option<u64>,
    pub rule_index: Option<usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingRuleChange {
    pub(crate) pending_id: String,
    pub(crate) session_id: String,
    pub(crate) rule: ApprovalRule,
}

/// One exact user permission rule. `arguments` must be a JSON object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRule {
    pub capability: String,
    pub arguments: Value,
}

/// The on-disk device rule list. The Client reads this version and passes the
/// exact snapshot with each dispatched task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRulesFile {
    pub version: u64,
    #[serde(default)]
    pub rules: Vec<ApprovalRule>,
}

impl ApprovalRulesFile {
    pub fn validate(&self) -> Result<(), String> {
        if self.rules.len() > MAX_RULES {
            return Err(format!(
                "approval rules file has more than {MAX_RULES} rules"
            ));
        }
        for rule in &self.rules {
            if rule.capability.trim().is_empty()
                || rule.capability.len() > 256
                || rule.capability.chars().any(char::is_control)
            {
                return Err("approval rule capability must contain 1..256 ordinary bytes".into());
            }
            if !rule.arguments.is_object() {
                return Err("approval rule arguments must be an object".into());
            }
        }
        Ok(())
    }
}

/// One immutable rule view captured when a task is dispatched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RulesSnapshot {
    /// No local approval-rule policy is configured. This is the explicit
    /// Headless/SDK behavior: only exact `RuntimeConfig.permissions`, durable
    /// grants and `approval.decide` answers release a capability.
    ExplicitOnly,
    /// A local policy exists, but its current version could not be fetched.
    /// Fail closed: an older rule set must not release anything.
    Unavailable,
    Loaded {
        version: u64,
        rules: Vec<ApprovalRule>,
    },
}

impl RulesSnapshot {
    pub fn loaded(version: u64, rules: Vec<ApprovalRule>) -> Result<Self, String> {
        let file = ApprovalRulesFile { version, rules };
        file.validate()?;
        Ok(Self::Loaded {
            version: file.version,
            rules: file.rules,
        })
    }

    pub fn version(&self) -> Option<u64> {
        match self {
            Self::Loaded { version, .. } => Some(*version),
            Self::ExplicitOnly | Self::Unavailable => None,
        }
    }

    fn exact_rule(&self, capability: &str, arguments: &Value) -> Option<usize> {
        let Self::Loaded { rules, .. } = self else {
            return None;
        };
        let scope = canonical_scope(arguments);
        rules.iter().position(|rule| {
            rule.capability == capability && canonical_scope(&rule.arguments) == scope
        })
    }

    /// Evaluate the captured snapshot. `ExplicitOnly` never auto-allows.
    pub fn evaluate(
        &self,
        capability: &str,
        arguments: &Value,
        context: &RuleContext<'_>,
    ) -> RuleDecision {
        // Capability applications are always a user decision. Even an exact
        // rule cannot pre-approve them; asking repeatedly never grants one.
        if capability == "capability.request" {
            return RuleDecision::Ask {
                reason: "capability requests always require your answer",
            };
        }
        match self {
            Self::ExplicitOnly => {
                return RuleDecision::Ask {
                    reason: "no local approval-rule policy is configured",
                };
            }
            Self::Unavailable => {
                return RuleDecision::Ask {
                    reason: "the current approval-rule version is unavailable; an older rule set cannot release this call",
                };
            }
            Self::Loaded { version, .. } => {
                if let Some(index) = self.exact_rule(capability, arguments) {
                    return RuleDecision::Allow {
                        kind: "user_rule",
                        rule_version: Some(*version),
                        rule_index: Some(index),
                    };
                }
            }
        }
        if is_workspace_read(capability) {
            return RuleDecision::Allow {
                kind: "workspace_read",
                rule_version: None,
                rule_index: None,
            };
        }
        if is_configured_build(capability) {
            return RuleDecision::Allow {
                kind: "configured_build",
                rule_version: None,
                rule_index: None,
            };
        }
        if context.main_session && is_main_session_local(capability) {
            return RuleDecision::Allow {
                kind: "main_session_local",
                rule_version: None,
                rule_index: None,
            };
        }
        if capability == "notify.send" && notification_target(arguments, context) {
            return RuleDecision::Allow {
                kind: "notification_target",
                rule_version: None,
                rule_index: None,
            };
        }
        if is_workspace_mutation(capability) {
            if mutation_within_recovery(capability, arguments, context) {
                return RuleDecision::Allow {
                    kind: "recovery_scope",
                    rule_version: None,
                    rule_index: None,
                };
            }
            return RuleDecision::Ask {
                reason: mutation_reason(capability, arguments, context),
            };
        }
        if is_network(capability) {
            return RuleDecision::Ask {
                reason: "network access requires your answer",
            };
        }
        if is_git_write(capability) {
            return RuleDecision::Ask {
                reason: "git writes require your answer",
            };
        }
        RuleDecision::Ask {
            reason: "this operation is not in an automatic rule and requires your answer",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleDecision {
    Allow {
        kind: &'static str,
        rule_version: Option<u64>,
        rule_index: Option<usize>,
    },
    Ask {
        reason: &'static str,
    },
}

/// The host context used by the built-in default rules.
pub struct RuleContext<'a> {
    pub workspace: &'a Path,
    pub recovery: Option<&'a RecoveryScope>,
    pub main_session: bool,
    pub notification_targets: &'a [String],
}

/// Directories covered by the current recovery points. No recovery scope means
/// every rewrite/delete asks, including changes inside the workspace.
///
/// `exclusions` names paths that a recovery point did not contain: Git-ignored
/// content, files over the snapshot limits, unreadable entries and nested
/// repositories. An excluded path and everything below it stays outside the
/// scope even when the workspace root is listed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryScope {
    #[serde(default)]
    pub roots: Vec<PathBuf>,
    #[serde(default)]
    pub exclusions: Vec<PathBuf>,
}

impl RecoveryScope {
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            roots: roots.into_iter().collect(),
            exclusions: Vec::new(),
        }
    }

    pub fn exclude(mut self, path: impl Into<PathBuf>) -> Self {
        self.exclusions.push(path.into());
        self
    }

    fn resolve(&self, path: &Path, workspace: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            workspace.join(path)
        }
    }

    /// True only when `path` is below a covered root and is not one of the
    /// paths that the recovery point explicitly left out.
    pub fn allows(&self, path: &str, workspace: &Path) -> bool {
        let candidate = Path::new(path);
        if candidate.as_os_str().is_empty()
            || candidate.is_absolute()
            || candidate
                .components()
                .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
        {
            return false;
        }
        let joined = workspace.join(candidate);
        let covered = self
            .roots
            .iter()
            .any(|root| joined.starts_with(self.resolve(root, workspace)));
        if !covered {
            return false;
        }
        !self
            .exclusions
            .iter()
            .any(|excluded| joined.starts_with(self.resolve(excluded, workspace)))
    }
}

/// A trusted, Client-owned source of the device rule version. Implementations
/// must not read model, plugin, web or tool output.
pub trait ApprovalRuleSource: Send + Sync {
    fn snapshot(&self) -> Result<RulesSnapshot, String>;

    /// Persist one complete rule file. Read-only sources keep the default
    /// refusal, so no capability can turn a synthetic source into a writer.
    fn commit(&self, _rules: &ApprovalRulesFile) -> Result<(), String> {
        Err("approval rules source is read-only".into())
    }

    /// Path that capabilities may never write. The model-facing Host is rooted
    /// at the workspace; this is a second boundary if those directories overlap.
    fn protected_path(&self) -> Option<&Path> {
        None
    }
}

/// Reads `<CAYW_HOME>/approval-rules.json`. The file must be a private regular
/// file (0600). A missing file is an empty version-0 rule set; any other
/// unreadable or invalid file makes this snapshot `Unavailable`.
#[derive(Debug, Clone)]
pub struct FileRules {
    path: PathBuf,
}

impl FileRules {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn device(data_dir: impl AsRef<Path>) -> Self {
        Self::new(data_dir.as_ref().join(APPROVAL_RULES_FILE))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl ApprovalRuleSource for FileRules {
    fn snapshot(&self) -> Result<RulesSnapshot, String> {
        match crate::local_cli::read_private(&self.path, MAX_RULES_BYTES) {
            Ok(bytes) => {
                let file: ApprovalRulesFile = serde_json::from_slice(&bytes)
                    .map_err(|_| "approval rules file is not valid JSON".to_string())?;
                file.validate()?;
                Ok(RulesSnapshot::Loaded {
                    version: file.version,
                    rules: file.rules,
                })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(RulesSnapshot::Loaded {
                version: 0,
                rules: Vec::new(),
            }),
            Err(_) => Err("approval rules file cannot be read".into()),
        }
    }

    /// Write the complete rule file through a private temporary file. The
    /// version is chosen by the caller under the Runtime's edit lock.
    fn commit(&self, rules: &ApprovalRulesFile) -> Result<(), String> {
        rules.validate()?;
        let bytes =
            serde_json::to_vec_pretty(rules).map_err(|_| "approval rules cannot be encoded")?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| "approval rules path has no parent directory".to_string())?;
        let temporary = parent.join(format!(
            ".approval-rules-{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        let result = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            let metadata = file.metadata()?;
            if !metadata.is_file()
                || metadata.permissions().mode() & 0o077 != 0
                || metadata.nlink() != 1
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "approval rules temporary file is not private",
                ));
            }
            fs::rename(&temporary, &self.path)?;
            File::open(parent)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(|_| "approval rules file cannot be written".to_string())
    }

    fn protected_path(&self) -> Option<&Path> {
        Some(&self.path)
    }
}

impl crate::Inner {
    /// Capture the current device rule version at dispatch. No fallback to an
    /// older loaded set is permitted when the source fails.
    pub(crate) fn approval_rules_snapshot(&self) -> RulesSnapshot {
        match &self.config.approval_rules {
            None => RulesSnapshot::ExplicitOnly,
            Some(source) => source.snapshot().unwrap_or(RulesSnapshot::Unavailable),
        }
    }
}

impl crate::Runtime {
    /// Show one proposed user rule without saving anything. The pending change
    /// is memory-only; only an exact confirmation from the user-facing surface
    /// can commit it. Tools and model output never reach this method.
    pub fn propose_approval_rule(
        &self,
        session_id: &str,
        rule: ApprovalRule,
    ) -> Result<RuleChangeQuestion, crate::RuntimeError> {
        self.require_main_session()?;
        if self.inner.config.approval_rules.is_none() {
            return Err(crate::RuntimeError::InvalidInput(
                "fully explicit Headless/SDK has no device approval-rule file".into(),
            ));
        }
        self.get(session_id)?;
        ApprovalRulesFile {
            version: 0,
            rules: vec![rule.clone()],
        }
        .validate()
        .map_err(crate::RuntimeError::InvalidInput)?;
        let pending_id = uuid::Uuid::new_v4().simple().to_string();
        let text = format!(
            "待确认的放行规则：能力 {}，完整参数 {}。确认后保存到设备规则文件并提升版本。请只回复以下选项之一：{} / {}",
            rule.capability,
            crate::permissions::canonical_scope(&rule.arguments),
            RULE_CONFIRM_REPLY,
            RULE_CANCEL_REPLY,
        );
        let question = RuleChangeQuestion {
            session_id: session_id.to_string(),
            pending_id: pending_id.clone(),
            rule: rule.clone(),
            text,
            options: vec![RULE_CONFIRM_REPLY, RULE_CANCEL_REPLY],
        };
        crate::lock(&self.inner.pending_rule_changes).insert(
            session_id.to_string(),
            PendingRuleChange {
                pending_id,
                session_id: session_id.to_string(),
                rule,
            },
        );
        Ok(question)
    }

    /// Answer the displayed rule modification. The reply must exactly equal one
    /// of the displayed options. Only confirmation writes the private file; the
    /// new version is the version that was read plus one.
    pub fn confirm_approval_rule(
        &self,
        session_id: &str,
        pending_id: &str,
        reply: &str,
    ) -> Result<RuleChangeOutcome, crate::RuntimeError> {
        self.require_main_session()?;
        let source = self.inner.config.approval_rules.as_ref().ok_or_else(|| {
            crate::RuntimeError::InvalidInput(
                "fully explicit Headless/SDK has no device approval-rule file".into(),
            )
        })?;
        // One edit at a time: read the current version, append, write, and only
        // then drop the pending display.
        let _edit = crate::lock(&self.inner.rule_edit);
        let pending = crate::lock(&self.inner.pending_rule_changes)
            .get(session_id)
            .filter(|pending| pending.session_id == session_id && pending.pending_id == pending_id)
            .cloned()
            .ok_or_else(|| {
                crate::RuntimeError::InvalidInput(
                    "no matching pending rule modification is displayed".into(),
                )
            })?;
        match reply {
            RULE_CANCEL_REPLY => {
                crate::lock(&self.inner.pending_rule_changes).remove(session_id);
                Ok(RuleChangeOutcome {
                    saved: false,
                    version: None,
                    rule_index: None,
                })
            }
            RULE_CONFIRM_REPLY => {
                let (version, mut rules) = match source.snapshot() {
                    Ok(RulesSnapshot::Loaded { version, rules }) => (version, rules),
                    Ok(_) => {
                        return Err(crate::RuntimeError::InvalidInput(
                            "approval rules source has no readable version".into(),
                        ));
                    }
                    Err(_) => {
                        return Err(crate::RuntimeError::InvalidInput(
                            "the current approval-rule version is unavailable; refusing to save"
                                .into(),
                        ));
                    }
                };
                if let Some(index) = rules.iter().position(|existing| {
                    existing.capability == pending.rule.capability
                        && canonical_scope(&existing.arguments)
                            == canonical_scope(&pending.rule.arguments)
                }) {
                    crate::lock(&self.inner.pending_rule_changes).remove(session_id);
                    return Ok(RuleChangeOutcome {
                        saved: false,
                        version: Some(version),
                        rule_index: Some(index),
                    });
                }
                if rules.len() >= MAX_RULES {
                    return Err(crate::RuntimeError::InvalidInput(format!(
                        "approval rules file already has {MAX_RULES} rules"
                    )));
                }
                let index = rules.len();
                rules.push(pending.rule);
                let next = ApprovalRulesFile {
                    version: version.checked_add(1).ok_or_else(|| {
                        crate::RuntimeError::InvalidInput(
                            "approval rules version cannot advance".into(),
                        )
                    })?,
                    rules,
                };
                source
                    .commit(&next)
                    .map_err(crate::RuntimeError::InvalidInput)?;
                crate::lock(&self.inner.pending_rule_changes).remove(session_id);
                Ok(RuleChangeOutcome {
                    saved: true,
                    version: Some(next.version),
                    rule_index: Some(index),
                })
            }
            _ => Err(crate::RuntimeError::InvalidInput(
                "rule confirmation must exactly match one of the displayed options".into(),
            )),
        }
    }
}

/// A convenience source for the local Client and tests. The Runtime never
/// mutates this value; a user-facing editor replaces the file and bumps the
/// version.
pub fn device_rules(data_dir: impl AsRef<Path>) -> Arc<dyn ApprovalRuleSource> {
    Arc::new(FileRules::device(data_dir))
}

fn is_workspace_read(name: &str) -> bool {
    matches!(
        name,
        "fs.read"
            | "fs.list"
            | "fs.stat"
            | "fs.tree"
            | "fs.grep"
            | "fs.glob"
            | "fs.batch_status"
            | "archive.list"
            | "archive.read"
            | "code.outline"
            | "code.diff_hunks"
            | "codeintel.refresh"
            | "codeintel.definitions"
            | "codeintel.file_version"
            | "codeintel.symbols"
            | "git.status"
            | "git.diff"
            | "git.log"
            | "git.show"
            | "git.blame"
    )
}

fn is_configured_build(name: &str) -> bool {
    // These capabilities exist only when the Host has a configured profile.
    matches!(
        name,
        "build.run" | "test.run" | "check.run" | "lint.run" | "format.run"
    )
}

fn is_main_session_local(name: &str) -> bool {
    matches!(
        name,
        "agent.run"
            | "agent.fleet"
            | "agent.stop"
            | "agent.steer"
            | "result.store"
            | "result.get"
            | "result.batch_get"
            | "result.search"
            | "result.release"
            | "content.read"
            | "cells.store"
            | "cells.load"
            | "user.ask"
            | "plan.update"
    ) || name.starts_with("work.")
}

pub(crate) fn is_workspace_mutation(name: &str) -> bool {
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
    )
}

fn is_git_write(name: &str) -> bool {
    matches!(
        name,
        "git.stage" | "git.commit" | "git.branch" | "git.worktree"
    )
}

fn is_network(name: &str) -> bool {
    name.starts_with("web.")
        || name.starts_with("mcp.")
        || matches!(name, "search.query" | "notify.send")
}

fn notification_target(arguments: &Value, context: &RuleContext<'_>) -> bool {
    let Some(target) = ["target", "recipient", "target_id"]
        .into_iter()
        .find_map(|field| arguments.get(field).and_then(Value::as_str))
    else {
        return false;
    };
    context
        .notification_targets
        .iter()
        .any(|configured| configured == target)
}

fn mutation_reason(name: &str, arguments: &Value, context: &RuleContext<'_>) -> &'static str {
    if mutation_paths(name, arguments).is_none() {
        return "this file change has no single recovery-scope path and requires your answer";
    }
    if context.recovery.is_none() {
        return "no recovery scope is established; file changes require your answer";
    }
    "this file change is outside the recovery scope and requires your answer"
}

fn mutation_within_recovery(name: &str, arguments: &Value, context: &RuleContext<'_>) -> bool {
    let Some(paths) = mutation_paths(name, arguments) else {
        return false;
    };
    let Some(recovery) = context.recovery else {
        return false;
    };
    paths
        .iter()
        .all(|path| recovery.allows(path, context.workspace))
}

pub(crate) fn mutation_paths<'a>(name: &str, arguments: &'a Value) -> Option<Vec<&'a str>> {
    match name {
        "fs.write" | "fs.patch" | "fs.mkdir" | "fs.remove" => {
            Some(vec![arguments.get("path")?.as_str()?])
        }
        "fs.move" => Some(vec![
            arguments.get("source")?.as_str()?,
            arguments.get("destination")?.as_str()?,
        ]),
        "fs.prepare_change_batch" => {
            let operations = arguments.get("operations")?.as_array()?;
            let paths = operations
                .iter()
                .map(|operation| operation.get("path")?.as_str())
                .collect::<Option<Vec<_>>>()?;
            (!paths.is_empty()).then_some(paths)
        }
        _ => None,
    }
}

/// True when one file-changing capability would target the user-owned rules
/// file. The Host is rooted at the workspace, so this matters only when the
/// workspace and CAYW_HOME overlap; it is a second boundary, not a substitute.
pub(crate) fn mutation_targets_protected(
    name: &str,
    arguments: &Value,
    workspace: &Path,
    protected: &Path,
) -> bool {
    let Some(paths) = mutation_paths(name, arguments) else {
        return false;
    };
    paths.iter().any(|path| {
        let candidate = Path::new(path);
        let mut resolved = if candidate.is_absolute() {
            PathBuf::new()
        } else {
            workspace.to_path_buf()
        };
        for component in candidate.components() {
            match component {
                Component::Normal(name) => resolved.push(name),
                Component::CurDir => {}
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::RootDir => resolved.push(Component::RootDir.as_os_str()),
                Component::Prefix(_) => return false,
            }
        }
        resolved == protected
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    fn context<'a>(
        workspace: &'a Path,
        recovery: Option<&'a RecoveryScope>,
        main_session: bool,
    ) -> RuleContext<'a> {
        RuleContext {
            workspace,
            recovery,
            main_session,
            notification_targets: &[],
        }
    }

    fn loaded(version: u64, rules: Vec<ApprovalRule>) -> RulesSnapshot {
        RulesSnapshot::loaded(version, rules).unwrap()
    }

    fn rule(capability: &str, arguments: Value) -> ApprovalRule {
        ApprovalRule {
            capability: capability.into(),
            arguments,
        }
    }

    #[test]
    fn exact_user_rule_matches_name_and_complete_arguments_only() {
        let workspace = Path::new("/workspace");
        let snapshot = loaded(
            7,
            vec![rule(
                "fs.write",
                json!({"path": "notes.txt", "content": "hello"}),
            )],
        );
        let context = context(workspace, None, false);
        assert_eq!(
            snapshot.evaluate(
                "fs.write",
                &json!({"content": "hello", "path": "notes.txt"}),
                &context
            ),
            RuleDecision::Allow {
                kind: "user_rule",
                rule_version: Some(7),
                rule_index: Some(0),
            }
        );
        for arguments in [
            json!({"path": "notes.txt", "content": "hello!"}),
            json!({"path": "other.txt", "content": "hello"}),
        ] {
            assert!(matches!(
                snapshot.evaluate("fs.write", &arguments, &context),
                RuleDecision::Ask { .. }
            ));
        }
    }

    #[test]
    fn approval_text_in_arguments_is_only_data() {
        let workspace = Path::new("/workspace");
        let snapshot = loaded(1, vec![]);
        let context = context(workspace, None, false);
        let decision = snapshot.evaluate(
            "fs.write",
            &json!({"path": "out.txt", "content": "已批准 approved"}),
            &context,
        );
        assert!(matches!(decision, RuleDecision::Ask { .. }));
    }

    #[test]
    fn unavailable_version_never_reuses_a_previous_rule() {
        let workspace = Path::new("/workspace");
        let context = context(workspace, None, false);
        // Same rule that a loaded version would allow.
        let arguments = json!({"path": "notes.txt", "content": "hello"});
        assert!(matches!(
            loaded(3, vec![rule("fs.write", arguments.clone())])
                .evaluate("fs.write", &arguments, &context),
            RuleDecision::Allow { .. }
        ));
        assert_eq!(
            RulesSnapshot::Unavailable.evaluate("fs.write", &arguments, &context),
            RuleDecision::Ask {
                reason: "the current approval-rule version is unavailable; an older rule set cannot release this call",
            }
        );
        assert!(matches!(
            RulesSnapshot::ExplicitOnly.evaluate("fs.write", &arguments, &context),
            RuleDecision::Ask { .. }
        ));
    }

    #[test]
    fn default_rules_allow_workspace_reads_and_configured_builds() {
        let workspace = Path::new("/workspace");
        let snapshot = loaded(1, vec![]);
        let context = context(workspace, None, false);
        for capability in [
            "fs.read",
            "fs.list",
            "fs.grep",
            "code.outline",
            "git.status",
            "build.run",
            "test.run",
            "check.run",
            "format.run",
        ] {
            assert!(
                matches!(
                    snapshot.evaluate(capability, &json!({"path": "src"}), &context),
                    RuleDecision::Allow { .. }
                ),
                "{capability} should be allowed by default"
            );
        }
    }

    #[test]
    fn mutations_ask_until_the_exact_path_is_in_the_recovery_scope() {
        let workspace = Path::new("/workspace");
        let snapshot = loaded(1, vec![]);
        let argument = json!({"path": "src/lib.rs", "content": "x"});
        let without = context(workspace, None, false);
        assert!(matches!(
            snapshot.evaluate("fs.write", &argument, &without),
            RuleDecision::Ask { .. }
        ));
        let outside = RecoveryScope::new([PathBuf::from("/workspace/tests")]);
        let outside = context(workspace, Some(&outside), false);
        assert!(matches!(
            snapshot.evaluate("fs.write", &argument, &outside),
            RuleDecision::Ask { .. }
        ));
        let inside = RecoveryScope::new([PathBuf::from("/workspace/src")]);
        let inside = context(workspace, Some(&inside), false);
        assert_eq!(
            snapshot.evaluate("fs.write", &argument, &inside),
            RuleDecision::Allow {
                kind: "recovery_scope",
                rule_version: None,
                rule_index: None,
            }
        );
    }

    #[test]
    fn excluded_paths_never_count_as_recovery_coverage() {
        let workspace = Path::new("/workspace");
        let snapshot = loaded(1, vec![]);
        let scope = RecoveryScope::new([PathBuf::from("/workspace")])
            .exclude("ignored.log")
            .exclude("large.bin")
            .exclude("vendor/nested");
        let context = context(workspace, Some(&scope), false);
        for (path, allowed) in [
            ("src/lib.rs", true),
            ("ignored.log", false),
            ("ignored.log/child", false),
            ("large.bin", false),
            ("vendor/nested/inside.txt", false),
            ("vendor/nested-sibling.txt", true),
        ] {
            assert_eq!(
                scope.allows(path, workspace),
                allowed,
                "scope decision for {path}"
            );
            let decision =
                snapshot.evaluate("fs.write", &json!({"path": path, "content": "x"}), &context);
            assert_eq!(
                matches!(
                    decision,
                    RuleDecision::Allow {
                        kind: "recovery_scope",
                        ..
                    }
                ),
                allowed,
                "approval decision for {path}: {decision:?}"
            );
        }
    }

    #[test]
    fn issues_outside_and_unclassified_capabilities_ask() {
        let workspace = Path::new("/workspace");
        let snapshot = loaded(1, vec![]);
        let context = context(workspace, None, false);
        for capability in [
            "web.search",
            "mcp.call",
            "search.query",
            "git.commit",
            "capability.request",
            "mystery.action",
        ] {
            assert!(
                matches!(
                    snapshot.evaluate(capability, &json!({}), &context),
                    RuleDecision::Ask { .. }
                ),
                "{capability} should ask"
            );
        }
    }

    #[test]
    fn capability_request_cannot_be_released_even_by_an_exact_rule() {
        let workspace = Path::new("/workspace");
        let snapshot = loaded(
            9,
            vec![rule("capability.request", json!({"capability": "web.add"}))],
        );
        let context = context(workspace, None, false);
        assert!(matches!(
            snapshot.evaluate(
                "capability.request",
                &json!({"capability": "web.add"}),
                &context
            ),
            RuleDecision::Ask { .. }
        ));
    }

    #[test]
    fn main_session_and_notification_targets_are_explicit_categories() {
        let workspace = Path::new("/workspace");
        let snapshot = loaded(1, vec![]);
        let ordinary = context(workspace, None, false);
        assert!(matches!(
            snapshot.evaluate("agent.run", &json!({"prompt": "work"}), &ordinary),
            RuleDecision::Ask { .. }
        ));
        let main = context(workspace, None, true);
        assert!(matches!(
            snapshot.evaluate("agent.run", &json!({"prompt": "work"}), &main),
            RuleDecision::Allow {
                kind: "main_session_local",
                ..
            }
        ));
        let targets = vec!["user@example.invalid".to_string()];
        let with_target = RuleContext {
            workspace,
            recovery: None,
            main_session: false,
            notification_targets: &targets,
        };
        assert!(matches!(
            snapshot.evaluate(
                "notify.send",
                &json!({"target": "user@example.invalid"}),
                &with_target
            ),
            RuleDecision::Allow {
                kind: "notification_target",
                ..
            }
        ));
        assert!(matches!(
            snapshot.evaluate(
                "notify.send",
                &json!({"target": "other@example.invalid"}),
                &with_target
            ),
            RuleDecision::Ask { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn file_rules_read_the_device_version_and_fail_closed_when_damaged() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(APPROVAL_RULES_FILE);
        std::fs::write(&path, r#"{"version":5,"rules":[]}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let source = FileRules::new(&path);
        assert_eq!(source.snapshot().unwrap().version(), Some(5));

        std::fs::write(&path, "not json").unwrap();
        assert!(source.snapshot().is_err(), "a damaged file is unavailable");

        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            source.snapshot().unwrap(),
            RulesSnapshot::Loaded {
                version: 0,
                rules: vec![]
            }
        );
    }
}
