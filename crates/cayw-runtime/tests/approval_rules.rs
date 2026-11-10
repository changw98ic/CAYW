//! Offline approval-rule evidence. Only macOS is a release target; keep the
//! Linux workspace check compiling without claiming desktop support.
#![cfg(target_os = "macos")]

use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, ToolCall, Usage, UsageStatus,
};
use cayw_core::{ApprovalRequest, CoreEvent};
use cayw_runtime::{
    ApprovalRule, ApprovalRuleSource, FileRules, HeadlessConfig, RulesSnapshot, Runtime,
    RuntimeConfig, ScopedPermission, TaskState,
};
use futures::stream;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const DEADLINE: Duration = Duration::from_secs(20);

struct RuleProvider {
    script: &'static str,
    calls: AtomicUsize,
    tool_results: Mutex<Vec<String>>,
}

impl RuleProvider {
    fn new(script: &'static str) -> Self {
        Self {
            script,
            calls: AtomicUsize::new(0),
            tool_results: Mutex::new(Vec::new()),
        }
    }

    fn tool_results(&self) -> Vec<String> {
        self.tool_results.lock().unwrap().clone()
    }
}

impl Provider for RuleProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let tool_result = request
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::ToolResult(result) => Some(cayw_ai::content_text(&result.content)),
                _ => None,
            });
        let mut message = AssistantMessage::new(&request.model);
        message.usage = Usage {
            input: 100,
            output: 50,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
            total_tokens: 150,
        };
        message.usage_status = UsageStatus::Reported;
        if let Some(tool_result) = tool_result {
            self.tool_results.lock().unwrap().push(tool_result);
            message.content = vec![Content::text("approval fixture complete")];
            message.stop_reason = StopReason::Stop;
        } else {
            message.content = vec![Content::ToolCall(ToolCall::new(
                format!("call_rule_{index}"),
                "execute_code",
                json!({
                    "action": "exec",
                    "max_output_tokens": 4096,
                    "script": self.script,
                    "yield_time_ms": 1000
                }),
            ))];
            message.stop_reason = StopReason::ToolUse;
        }
        Box::pin(stream::iter([AssistantEvent::Done { message }]))
    }
}

struct FixedRules(Result<RulesSnapshot, String>);

impl ApprovalRuleSource for FixedRules {
    fn snapshot(&self) -> Result<RulesSnapshot, String> {
        self.0.clone()
    }
}

fn loaded(version: u64, rules: Vec<ApprovalRule>) -> Arc<dyn ApprovalRuleSource> {
    Arc::new(FixedRules(Ok(
        RulesSnapshot::loaded(version, rules).unwrap()
    )))
}

fn unavailable() -> Arc<dyn ApprovalRuleSource> {
    Arc::new(FixedRules(Err(
        "synthetic rule-version fetch failure".into()
    )))
}

fn open(root: &Path, provider: Arc<RuleProvider>, rules: Arc<dyn ApprovalRuleSource>) -> Runtime {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut config = RuntimeConfig {
        approval_rules: Some(rules),
        max_run_time: Duration::from_secs(60),
        max_queue_time: Duration::from_secs(60),
        ..RuntimeConfig::default()
    };
    config.cells.max_yield_time = Duration::from_secs(30);
    config.codemode.timeout = Duration::from_secs(60);
    config.host.call_timeout = Duration::from_secs(60);
    Runtime::open(
        root.join("session.db"),
        &workspace,
        provider,
        Model::new("offline-fixture", "approval-rules"),
        config,
    )
    .unwrap()
}

async fn wait_for_approval(runtime: &Runtime, session: &str, capability: &str) -> ApprovalRequest {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let view = runtime.get(session).unwrap();
        if let Some(approval) = view
            .pending_approvals
            .iter()
            .find(|approval| approval.input.capability == capability)
        {
            return approval.clone();
        }
        assert!(
            Instant::now() < deadline,
            "no {capability} approval became pending"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn rule_allowed(runtime: &Runtime, session: &str, rule: &str) -> bool {
    runtime
        .events(session, 0, 1000)
        .unwrap()
        .into_iter()
        .any(|record| {
            matches!(
                record.event,
                CoreEvent::Host { kind, data }
                    if kind == "permission_rule_allowed" && data["rule"] == json!(rule)
            )
        })
}

#[tokio::test]
async fn workspace_read_is_released_by_the_default_rule_without_a_question() {
    let root = tempfile::tempdir().unwrap();
    let provider = Arc::new(RuleProvider::new(
        "return cayw.call('fs.read', { path = 'inside.txt' })",
    ));
    let runtime = Arc::new(open(root.path(), provider.clone(), loaded(4, vec![])));
    std::fs::write(root.path().join("workspace/inside.txt"), "RULE-READ-OK").unwrap();
    let session = runtime.create(None).unwrap().id;
    runtime
        .prompt(
            &session,
            "read the workspace file",
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let view = runtime.get(&session).unwrap();
    assert!(view.pending_approvals.is_empty());
    assert_eq!(
        runtime.tasks(&session).unwrap()[0].state,
        TaskState::Completed
    );
    assert!(rule_allowed(&runtime, &session, "workspace_read"));
    assert!(
        provider
            .tool_results()
            .iter()
            .any(|result| result.contains("RULE-READ-OK")),
        "the model received the real read result"
    );
}

#[tokio::test]
async fn approval_word_in_arguments_asks_and_exact_text_decides_the_original_request() {
    let root = tempfile::tempdir().unwrap();
    let provider = Arc::new(RuleProvider::new(
        "return cayw.call('fs.write', { path = 'out.txt', content = '已批准' })",
    ));
    let runtime = Arc::new(open(root.path(), provider, loaded(1, vec![])));
    let session = runtime.create(None).unwrap().id;
    let output = root.path().join("workspace/out.txt");
    let running = {
        let runtime = runtime.clone();
        let session = session.clone();
        tokio::spawn(async move {
            runtime
                .prompt(&session, "write a file", CancellationToken::new())
                .await
        })
    };

    let approval = wait_for_approval(&runtime, &session, "fs.write").await;
    assert!(
        !output.exists(),
        "tool output text cannot release the write"
    );
    assert!(!rule_allowed(&runtime, &session, "user_rule"));

    let question = runtime.approval_question(&session, &approval.id).unwrap();
    assert_eq!(question.options, vec!["允许一次", "本任务允许", "拒绝"]);
    assert_eq!(question.session_id, session);
    assert_eq!(question.approval_id, approval.id);
    assert!(question.text.contains(&approval.input.capability));

    let error = runtime
        .answer_approval(&session, &approval.id, "已批准", "fixture-user")
        .unwrap_err();
    assert!(error.to_string().contains("exactly match"));
    assert!(!output.exists());

    let decided = runtime
        .answer_approval(&session, &approval.id, "允许一次", "fixture-user")
        .unwrap();
    assert_eq!(decided.id, approval.id);
    assert_eq!(decided.session_id, session);
    assert!(decided.decision.is_some());
    let completed = running.await.unwrap().unwrap();
    assert!(completed.pending_approvals.is_empty());
    assert_eq!(
        std::fs::read_to_string(&output).unwrap(),
        "已批准",
        "the exact option released the original request"
    );
}

#[tokio::test]
async fn rule_version_failure_never_reuses_an_older_release() {
    let root = tempfile::tempdir().unwrap();
    let provider = Arc::new(RuleProvider::new(
        "return cayw.call('fs.read', { path = 'inside.txt' })",
    ));
    let runtime = Arc::new(open(root.path(), provider, unavailable()));
    std::fs::write(root.path().join("workspace/inside.txt"), "RULE-READ-OK").unwrap();
    let session = runtime.create(None).unwrap().id;
    let running = {
        let runtime = runtime.clone();
        let session = session.clone();
        tokio::spawn(async move {
            runtime
                .prompt(
                    &session,
                    "read after version failure",
                    CancellationToken::new(),
                )
                .await
        })
    };

    // A workspace read that the default rule would release must ask instead.
    let approval = wait_for_approval(&runtime, &session, "fs.read").await;
    assert!(!rule_allowed(&runtime, &session, "workspace_read"));
    let question = runtime.approval_question(&session, &approval.id).unwrap();
    assert!(question.reason.contains("version is unavailable"));
    runtime
        .answer_approval(&session, &approval.id, "拒绝", "fixture-user")
        .unwrap();
    let outcome = running.await.unwrap();
    if let Ok(view) = outcome {
        assert!(view.pending_approvals.is_empty());
    }
    assert!(!rule_allowed(&runtime, &session, "workspace_read"));
}

#[test]
fn exact_user_rule_arguments_must_match_completely() {
    let rules = RulesSnapshot::loaded(
        12,
        vec![ApprovalRule {
            capability: "fs.write".into(),
            arguments: json!({"path": "notes.txt", "content": "hello"}),
        }],
    )
    .unwrap();
    let workspace = PathBuf::from("/workspace");
    let context = cayw_runtime::RuleContext {
        workspace: &workspace,
        recovery: None,
        main_session: false,
        notification_targets: &[],
    };
    assert!(matches!(
        rules.evaluate(
            "fs.write",
            &json!({"content": "hello", "path": "notes.txt"}),
            &context
        ),
        cayw_runtime::RuleDecision::Allow {
            kind: "user_rule",
            rule_version: Some(12),
            rule_index: Some(0),
        }
    ));
    assert!(matches!(
        rules.evaluate(
            "fs.write",
            &json!({"path": "notes.txt", "content": "hello!"}),
            &context
        ),
        cayw_runtime::RuleDecision::Ask { .. }
    ));
}

fn open_main(
    root: &Path,
    provider: Arc<RuleProvider>,
    rules: Arc<dyn ApprovalRuleSource>,
) -> Runtime {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let config = RuntimeConfig {
        approval_rules: Some(rules),
        main_session: true,
        ..RuntimeConfig::default()
    };
    Runtime::open(
        root.join("session.db"),
        &workspace,
        provider,
        Model::new("offline-fixture", "approval-rules"),
        config,
    )
    .unwrap()
}

#[test]
fn a_rule_is_saved_only_after_the_exact_user_confirmation() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("approval-rules.json");
    let runtime = open_main(
        root.path(),
        Arc::new(RuleProvider::new("return 1")),
        Arc::new(FileRules::new(&path)),
    );
    let session = runtime.create(None).unwrap().id;
    let rule = ApprovalRule {
        capability: "fs.write".into(),
        arguments: json!({"path": "notes.txt", "content": "hello"}),
    };

    let question = runtime
        .propose_approval_rule(&session, rule.clone())
        .unwrap();
    assert_eq!(question.session_id, session);
    assert_eq!(question.options, vec!["确认保存规则", "取消"]);
    assert!(question.text.contains("fs.write"));
    assert!(question.text.contains("notes.txt"));
    assert!(!path.exists(), "a proposed rule must not be written yet");

    let error = runtime
        .confirm_approval_rule(&session, &question.pending_id, "已批准")
        .unwrap_err();
    assert!(error.to_string().contains("exactly match"));
    assert!(
        !path.exists(),
        "model or tool text cannot confirm a rule change"
    );

    let outcome = runtime
        .confirm_approval_rule(&session, &question.pending_id, "确认保存规则")
        .unwrap();
    assert!(outcome.saved);
    assert_eq!(outcome.version, Some(1));
    assert_eq!(outcome.rule_index, Some(0));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600,
        "the device rule file is private"
    );
    let saved: cayw_runtime::ApprovalRulesFile =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved.version, 1);
    assert_eq!(saved.rules, vec![rule]);

    // A second confirmation appends one rule and advances the version.
    let second = ApprovalRule {
        capability: "fs.write".into(),
        arguments: json!({"path": "other.txt", "content": "hello"}),
    };
    let question = runtime
        .propose_approval_rule(&session, second.clone())
        .unwrap();
    let outcome = runtime
        .confirm_approval_rule(&session, &question.pending_id, "确认保存规则")
        .unwrap();
    assert!(outcome.saved);
    assert_eq!(outcome.version, Some(2));
    let saved: cayw_runtime::ApprovalRulesFile =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved.version, 2);
    assert_eq!(saved.rules.len(), 2);

    // Cancelling a displayed rule writes nothing.
    let third = ApprovalRule {
        capability: "fs.remove".into(),
        arguments: json!({"path": "gone.txt"}),
    };
    let question = runtime.propose_approval_rule(&session, third).unwrap();
    let outcome = runtime
        .confirm_approval_rule(&session, &question.pending_id, "取消")
        .unwrap();
    assert!(!outcome.saved);
    let saved: cayw_runtime::ApprovalRulesFile =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved.version, 2);
    assert_eq!(saved.rules.len(), 2);
}

#[tokio::test]
async fn a_capability_cannot_write_the_user_rule_file_even_with_exact_permission() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let workspace = std::fs::canonicalize(&workspace).unwrap();
    let path = workspace.join("approval-rules.json");
    let content = r#"{"version":99,"rules":[]}"#;
    let provider = Arc::new(RuleProvider::new(
        "return cayw.call('fs.write', { path = 'approval-rules.json', content = '{\"version\":99,\"rules\":[]}' })",
    ));
    let config = RuntimeConfig {
        approval_rules: Some(Arc::new(FileRules::new(&path))),
        main_session: true,
        permissions: vec![ScopedPermission::exact(
            "fs.write",
            json!({"path": "approval-rules.json", "content": content}),
        )],
        max_run_time: Duration::from_secs(60),
        max_queue_time: Duration::from_secs(60),
        ..RuntimeConfig::default()
    };
    let runtime = Runtime::open(
        root.path().join("session.db"),
        &workspace,
        provider.clone(),
        Model::new("offline-fixture", "approval-rules"),
        config,
    )
    .unwrap();
    let session = runtime.create(None).unwrap().id;
    runtime
        .prompt(
            &session,
            "try to edit the user rule file",
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(
        !path.exists(),
        "the user-owned rule file cannot be created or changed by a capability"
    );
    assert_eq!(
        runtime.tasks(&session).unwrap()[0].state,
        TaskState::Completed
    );
    assert!(
        provider
            .tool_results()
            .iter()
            .any(|result| result.contains("user-owned approval rules file")),
        "the model saw a refusal instead of a successful write"
    );
}

#[test]
fn notification_targets_come_only_from_the_user_configuration_file() {
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("config.json");
    std::fs::write(
        &config_path,
        r#"{"notification_targets":["user@example.invalid"]}"#,
    )
    .unwrap();
    let config = HeadlessConfig::load(&config_path)
        .unwrap()
        .into_runtime_config()
        .unwrap();
    assert_eq!(
        config.notification_targets,
        vec!["user@example.invalid".to_string()]
    );

    let workspace = Path::new("/workspace");
    let rules = RulesSnapshot::loaded(1, vec![]).unwrap();
    let context = cayw_runtime::RuleContext {
        workspace,
        recovery: None,
        main_session: false,
        notification_targets: &config.notification_targets,
    };
    assert!(matches!(
        rules.evaluate(
            "notify.send",
            &json!({"target": "user@example.invalid", "text": "done"}),
            &context
        ),
        cayw_runtime::RuleDecision::Allow {
            kind: "notification_target",
            ..
        }
    ));
    assert!(matches!(
        rules.evaluate(
            "notify.send",
            &json!({"target": "other@example.invalid", "text": "done"}),
            &context
        ),
        cayw_runtime::RuleDecision::Ask { .. }
    ));
    // Output text that merely names the target is data, not configuration.
    assert!(matches!(
        rules.evaluate(
            "notify.send",
            &json!({"target": "user@example.invalid", "text": "user@example.invalid"}),
            &context
        ),
        cayw_runtime::RuleDecision::Allow { .. }
    ));

    // Without a target in the user configuration file, every target asks.
    std::fs::write(&config_path, "{}").unwrap();
    let empty = HeadlessConfig::load(&config_path)
        .unwrap()
        .into_runtime_config()
        .unwrap();
    assert!(empty.notification_targets.is_empty());
    let context = cayw_runtime::RuleContext {
        workspace,
        recovery: None,
        main_session: false,
        notification_targets: &empty.notification_targets,
    };
    assert!(matches!(
        rules.evaluate(
            "notify.send",
            &json!({"target": "user@example.invalid", "text": "done"}),
            &context
        ),
        cayw_runtime::RuleDecision::Ask { .. }
    ));

    std::fs::write(&config_path, r#"{"notification_targets":[""]}"#).unwrap();
    assert!(
        HeadlessConfig::load(&config_path).is_err(),
        "an empty configured target is rejected"
    );
}
