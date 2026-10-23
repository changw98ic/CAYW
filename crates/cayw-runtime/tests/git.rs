//! Git capabilities through the real Runtime: the model-visible `execute_code`
//! cell calls `cayw.call`, Runtime performs Host permission/start/settle
//! bookkeeping in Core, and the synthetic repository is inspected directly
//! afterwards. No test touches the development repository.

use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, ToolCall, Usage, UsageStatus,
};
use cayw_core::{CapabilityCallRecord, CapabilityOutcome, CoreEvent, EventRecord};
use cayw_host::{GitWritePolicy, HostConfig};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission, TaskState};
use futures::stream;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

const IDENTITY_NAME: &str = "CAYW Runtime Host";
const IDENTITY_EMAIL: &str = "runtime-host@example.invalid";

/// One scripted model. The first turn calls the single model-visible tool with
/// the test's Luau script; after the tool result it stops with plain text.
struct ScriptedProvider {
    prompt: String,
    script: String,
}

impl Provider for ScriptedProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let mut message = AssistantMessage::new(&request.model);
        // Synthetic counters; Git effects still go through the real Host.
        message.usage = Usage {
            input: 4,
            output: 4,
            total_tokens: 8,
            ..Default::default()
        };
        message.usage_status = UsageStatus::Reported;
        if matches!(request.messages.last(), Some(Message::ToolResult(_))) {
            message.content.push(Content::text("git step complete"));
            message.stop_reason = StopReason::Stop;
        } else {
            let seen = request
                .messages
                .iter()
                .rev()
                .find_map(|message| match message {
                    Message::User { content, .. } => content.iter().find_map(|part| match part {
                        Content::Text { text } => Some(text.clone()),
                        _ => None,
                    }),
                    _ => None,
                })
                .expect("the runtime supplied a user prompt");
            assert_eq!(seen, self.prompt, "unexpected runtime prompt");
            message.content.push(Content::ToolCall(ToolCall::new(
                format!("git-call-{}", self.prompt),
                "execute_code",
                json!({
                    "action":"exec",
                    "script":self.script,
                    "yield_time_ms":20000,
                    "max_output_tokens":8192
                }),
            )));
            message.stop_reason = StopReason::ToolUse;
        }
        Box::pin(stream::iter(vec![AssistantEvent::Done { message }]))
    }
}

fn policy() -> GitWritePolicy {
    GitWritePolicy {
        identity_name: IDENTITY_NAME.into(),
        identity_email: IDENTITY_EMAIL.into(),
        allow_stage: true,
        allow_commit: true,
        allow_branch: true,
        allow_worktree: true,
    }
}

fn git_environment() -> Vec<(&'static str, &'static str)> {
    vec![
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_OPTIONAL_LOCKS", "0"),
        ("LC_ALL", "C"),
        ("GIT_AUTHOR_NAME", "Fixture"),
        ("GIT_AUTHOR_EMAIL", "fixture@example.invalid"),
        ("GIT_COMMITTER_NAME", "Fixture"),
        ("GIT_COMMITTER_EMAIL", "fixture@example.invalid"),
    ]
}

fn git_with_index(root: &Path, args: &[&str], index: Option<&Path>) -> String {
    let mut command = std::process::Command::new("/usr/bin/git");
    command.args(args).current_dir(root).env_clear();
    for (name, value) in git_environment() {
        command.env(name, value);
    }
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}

fn git(root: &Path, args: &[&str]) -> String {
    git_with_index(root, args, None)
}

fn head(root: &Path) -> String {
    git(root, &["rev-parse", "HEAD"]).trim().to_string()
}

fn index_tree(root: &Path) -> String {
    git(root, &["write-tree"]).trim().to_string()
}

/// Compute the index tree the real stage call will produce without touching the
/// real index: a private index is seeded from HEAD, the same file is added, and
/// only that private index is read back.
fn staged_tree(root: &Path, index: &Path) -> String {
    let _ = std::fs::remove_file(index);
    git_with_index(root, &["read-tree", "HEAD"], Some(index));
    git_with_index(root, &["add", "--", "note"], Some(index));
    git_with_index(root, &["write-tree"], Some(index))
        .trim()
        .to_string()
}

fn init_repo(root: &Path) -> (String, String) {
    std::fs::create_dir(root).unwrap();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("note"), "alpha\nbeta\n").unwrap();
    git(root, &["add", "note"]);
    git(root, &["commit", "-qm", "fixture"]);
    (head(root), index_tree(root))
}

async fn run_step(
    temp: &tempfile::TempDir,
    repo: &Path,
    session: Option<&str>,
    permissions: Vec<ScopedPermission>,
    prompt: &str,
    script: String,
) -> (Runtime, String) {
    let provider = Arc::new(ScriptedProvider {
        prompt: prompt.into(),
        script,
    });
    let config = RuntimeConfig {
        host: HostConfig {
            git_program: Some(PathBuf::from("/usr/bin/git")),
            git_writes: Some(policy()),
            ..Default::default()
        },
        permissions,
        ..Default::default()
    };
    let runtime = Runtime::open(
        temp.path().join("runtime.sqlite"),
        repo,
        provider,
        Model::new("fixture", "runtime-git"),
        config,
    )
    .unwrap();
    let id = match session {
        Some(id) => id.to_owned(),
        None => runtime.create(None).unwrap().id,
    };
    runtime
        .prompt(&id, prompt, CancellationToken::new())
        .await
        .unwrap();
    (runtime, id)
}

fn completed_task(runtime: &Runtime, session: &str) -> String {
    let tasks = runtime.tasks(session).unwrap();
    let task = tasks.last().expect("a durable task");
    assert_eq!(task.state, TaskState::Completed, "{task:?}");
    task.id.clone()
}

fn matching<'a>(
    events: &'a [EventRecord],
    task_id: &str,
    name: &str,
    arguments: &Value,
    select: impl Fn(&'a CoreEvent) -> Option<&'a CapabilityCallRecord>,
) -> Vec<&'a CapabilityCallRecord> {
    events
        .iter()
        .filter_map(|record| select(&record.event))
        .filter(|call| call.name == name && &call.arguments == arguments && call.task_id == task_id)
        .collect()
}

/// Verify Core retained the exact original capability call, its start, and one
/// completed settlement for that same key, then return the settled result.
fn settled_result(events: &[EventRecord], task_id: &str, name: &str, arguments: &Value) -> Value {
    let planned = matching(events, task_id, name, arguments, |event| match event {
        CoreEvent::CapabilityPlanned { call } => Some(call),
        _ => None,
    });
    let started = matching(events, task_id, name, arguments, |event| match event {
        CoreEvent::CapabilityStarted { call } => Some(call),
        _ => None,
    });
    let settled = matching(events, task_id, name, arguments, |event| match event {
        CoreEvent::CapabilitySettled { call } => Some(call),
        _ => None,
    });
    assert_eq!(planned.len(), 1, "one planned {name}");
    assert_eq!(started.len(), 1, "one started {name}");
    assert_eq!(settled.len(), 1, "one settled {name}");
    assert_eq!(started[0].key, planned[0].key);
    assert_eq!(settled[0].key, planned[0].key);
    assert_eq!(settled[0].lease, planned[0].lease);
    assert_eq!(settled[0].source_hash, planned[0].source_hash);
    match settled[0].outcome.as_ref().expect("settled outcome") {
        CapabilityOutcome::Completed { result } => result.clone(),
        other => panic!("{name} did not complete: {other:?}"),
    }
}

fn script(capability: &str, arguments: &Value) -> String {
    format!(
        "local result = cayw.call({}, {})\nreturn result",
        serde_json::to_string(capability).unwrap(),
        luau(arguments)
    )
}

/// Convert the exact JSON argument object into a Luau table literal. Arrays and
/// objects recurse; strings are JSON-escaped, which is also valid Luau.
fn luau(value: &Value) -> String {
    match value {
        Value::Null => "nil".into(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => serde_json::to_string(value).unwrap(),
        Value::Array(values) => format!(
            "{{{}}}",
            values.iter().map(luau).collect::<Vec<_>>().join(",")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!("{key} = {}", luau(value)))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

#[tokio::test]
async fn runtime_git_capabilities_record_core_calls_and_change_a_synthetic_repo() {
    let temp = tempfile::tempdir().unwrap();
    let repo_path = temp.path().join("repo");
    let (h0, t0) = init_repo(&repo_path);
    // Runtime and Git report canonical paths; compare against the same form.
    let repo = repo_path.canonicalize().unwrap();

    // Blame runs through the real Runtime and the private snapshot. It must not
    // change HEAD or the index.
    let blame_arguments = json!({"path":"note"});
    let (runtime, session) = run_step(
        &temp,
        &repo,
        None,
        vec![ScopedPermission::exact(
            "git.blame",
            blame_arguments.clone(),
        )],
        "blame",
        script("git.blame", &blame_arguments),
    )
    .await;
    let task = completed_task(&runtime, &session);
    let events = runtime.events(&session, 0, 1000).unwrap();
    let blame = settled_result(&events, &task, "git.blame", &blame_arguments);
    let stdout = blame["stdout"].as_str().expect("blame stdout");
    assert!(
        stdout.contains("alpha") && stdout.contains("Fixture"),
        "{stdout}"
    );
    assert_eq!(head(&repo), h0);
    assert_eq!(index_tree(&repo), t0);

    // Stage the actual modified file. The capability, not the test, changes the
    // real index; the test only computes the expected tree through a private
    // index so the exact precondition can be supplied.
    std::fs::write(repo.join("note"), "gamma\n").unwrap();
    let t1 = staged_tree(&repo, &temp.path().join("scratch-index"));
    let stage_arguments = json!({
        "paths":["note"],
        "expected_head":h0,
        "expected_index_tree":t0,
    });
    let (runtime, _) = run_step(
        &temp,
        &repo,
        Some(&session),
        vec![ScopedPermission::exact(
            "git.stage",
            stage_arguments.clone(),
        )],
        "stage",
        script("git.stage", &stage_arguments),
    )
    .await;
    let task = completed_task(&runtime, &session);
    let events = runtime.events(&session, 0, 1000).unwrap();
    let staged = settled_result(&events, &task, "git.stage", &stage_arguments);
    assert_eq!(staged["head"], json!(h0));
    assert_eq!(staged["index_tree"], json!(t1));
    assert_eq!(head(&repo), h0, "staging never commits");
    assert_eq!(index_tree(&repo), t1);
    assert_eq!(git(&repo, &["diff", "--cached", "--name-only"]), "note\n");

    // Commit the verified index. Host identity must appear as both author and
    // committer, and the parent/tree must match the verified preconditions.
    let commit_arguments = json!({
        "message":"runtime commit",
        "expected_head":h0,
        "expected_index_tree":t1,
    });
    let (runtime, _) = run_step(
        &temp,
        &repo,
        Some(&session),
        vec![ScopedPermission::exact(
            "git.commit",
            commit_arguments.clone(),
        )],
        "commit",
        script("git.commit", &commit_arguments),
    )
    .await;
    let task = completed_task(&runtime, &session);
    let events = runtime.events(&session, 0, 1000).unwrap();
    let committed = settled_result(&events, &task, "git.commit", &commit_arguments);
    let h1 = head(&repo);
    assert_eq!(committed["commit"], json!(h1));
    assert_eq!(committed["parent"], json!(h0));
    assert_eq!(committed["tree"], json!(t1));
    let record = git(
        &repo,
        &[
            "log",
            "-1",
            "--format=%an%x00%ae%x00%cn%x00%ce%x00%T%x00%P%x00%s",
        ],
    );
    let fields: Vec<&str> = record.trim_end().split('\0').collect();
    assert_eq!(fields[0], IDENTITY_NAME);
    assert_eq!(fields[1], IDENTITY_EMAIL);
    assert_eq!(fields[2], IDENTITY_NAME);
    assert_eq!(fields[3], IDENTITY_EMAIL);
    assert_eq!(fields[4], t1);
    assert_eq!(fields[5], h0);
    assert_eq!(fields[6], "runtime commit");

    // Branch creation and safe deletion at the exact verified commit.
    let create_arguments = json!({
        "action":"create",
        "name":"feature",
        "commit":h1,
        "expected_head":h1,
    });
    let (runtime, _) = run_step(
        &temp,
        &repo,
        Some(&session),
        vec![ScopedPermission::exact(
            "git.branch",
            create_arguments.clone(),
        )],
        "branch-create",
        script("git.branch", &create_arguments),
    )
    .await;
    let task = completed_task(&runtime, &session);
    let events = runtime.events(&session, 0, 1000).unwrap();
    settled_result(&events, &task, "git.branch", &create_arguments);
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/feature"]).trim(), h1);

    let delete_arguments = json!({
        "action":"delete",
        "name":"feature",
        "commit":h1,
        "expected_head":h1,
    });
    let (runtime, _) = run_step(
        &temp,
        &repo,
        Some(&session),
        vec![ScopedPermission::exact(
            "git.branch",
            delete_arguments.clone(),
        )],
        "branch-delete",
        script("git.branch", &delete_arguments),
    )
    .await;
    let task = completed_task(&runtime, &session);
    let events = runtime.events(&session, 0, 1000).unwrap();
    settled_result(&events, &task, "git.branch", &delete_arguments);
    assert!(
        std::process::Command::new("/usr/bin/git")
            .args(["rev-parse", "--verify", "refs/heads/feature"])
            .current_dir(&repo)
            .env_clear()
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap()
            .status
            .code()
            != Some(0)
    );

    // A linked worktree is accepted only under the fixed in-root directory and
    // only for an existing branch at the verified commit.
    let branch_arguments = json!({
        "action":"create",
        "name":"wt-branch",
        "commit":h1,
        "expected_head":h1,
    });
    let (runtime, _) = run_step(
        &temp,
        &repo,
        Some(&session),
        vec![ScopedPermission::exact(
            "git.branch",
            branch_arguments.clone(),
        )],
        "worktree-branch",
        script("git.branch", &branch_arguments),
    )
    .await;
    let task = completed_task(&runtime, &session);
    let events = runtime.events(&session, 0, 1000).unwrap();
    settled_result(&events, &task, "git.branch", &branch_arguments);

    let add_arguments = json!({
        "action":"create",
        "name":"linked",
        "branch":"wt-branch",
        "commit":h1,
        "expected_head":h1,
    });
    let (runtime, _) = run_step(
        &temp,
        &repo,
        Some(&session),
        vec![ScopedPermission::exact(
            "git.worktree",
            add_arguments.clone(),
        )],
        "worktree-add",
        script("git.worktree", &add_arguments),
    )
    .await;
    let task = completed_task(&runtime, &session);
    let events = runtime.events(&session, 0, 1000).unwrap();
    let added = settled_result(&events, &task, "git.worktree", &add_arguments);
    let linked = repo.join(".cayw-worktrees/linked");
    assert_eq!(added["path"], json!(".cayw-worktrees/linked"));
    assert_eq!(
        std::fs::read_to_string(linked.join("note")).unwrap(),
        "gamma\n"
    );
    let control = std::fs::read_to_string(linked.join(".git")).unwrap();
    let target = control
        .trim()
        .strip_prefix("gitdir:")
        .expect("linked worktree control file")
        .trim();
    assert!(
        Path::new(target).starts_with(repo.join(".git/worktrees")),
        "{target}"
    );
    assert!(
        git(&repo, &["worktree", "list", "--porcelain"]).contains(&linked.display().to_string())
    );
    assert_eq!(git(&linked, &["rev-parse", "HEAD"]).trim(), h1);

    let remove_arguments = json!({"action":"remove","name":"linked","expected_head":h1});
    let (runtime, _) = run_step(
        &temp,
        &repo,
        Some(&session),
        vec![ScopedPermission::exact(
            "git.worktree",
            remove_arguments.clone(),
        )],
        "worktree-remove",
        script("git.worktree", &remove_arguments),
    )
    .await;
    let task = completed_task(&runtime, &session);
    let events = runtime.events(&session, 0, 1000).unwrap();
    settled_result(&events, &task, "git.worktree", &remove_arguments);
    assert!(!linked.exists());
    assert!(
        !git(&repo, &["worktree", "list", "--porcelain"]).contains(&linked.display().to_string())
    );
    assert_eq!(head(&repo), h1);
    assert_eq!(index_tree(&repo), t1);
}
