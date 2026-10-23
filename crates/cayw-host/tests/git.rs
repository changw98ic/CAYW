use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{AuditSink, GitWritePolicy, Host, HostConfig, PermissionGate};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Tree(PathBuf);
impl Tree {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cayw-host-git-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Gate {
    allow: bool,
    calls: Mutex<Vec<String>>,
}
#[async_trait]
impl PermissionGate for Gate {
    async fn authorize(
        &self,
        capability: &str,
        _: &Value,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError> {
        self.calls.lock().unwrap().push(capability.into());
        if self.allow && !cancel.is_cancelled() {
            Ok(())
        } else {
            Err(CapabilityError::new("denied"))
        }
    }
}

#[derive(Default)]
struct Audit {
    records: Mutex<Vec<String>>,
}
#[async_trait]
impl AuditSink for Audit {
    async fn started(&self, capability: &str, _: &Value) -> Result<String, CapabilityError> {
        let mut records = self.records.lock().unwrap();
        let id = records.len().to_string();
        records.push(format!("start:{capability}"));
        Ok(id)
    }
    async fn settled(
        &self,
        _: &str,
        result: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        self.records.lock().unwrap().push(
            if result.is_ok() {
                "success"
            } else if result.as_ref().unwrap_err().outcome_unknown {
                "unknown"
            } else {
                "failure"
            }
            .into(),
        );
        Ok(())
    }
    fn dropped(&self, _: &str) -> Result<(), CapabilityError> {
        self.records.lock().unwrap().push("dropped".into());
        Ok(())
    }
}

fn policy() -> GitWritePolicy {
    GitWritePolicy {
        identity_name: "CAYW Host".into(),
        identity_email: "host@example.invalid".into(),
        allow_stage: true,
        allow_commit: true,
        allow_branch: true,
        allow_worktree: true,
    }
}

fn host(root: &Path, policy: Option<GitWritePolicy>) -> (Arc<Host>, Arc<Gate>, Arc<Audit>) {
    let gate = Arc::new(Gate {
        allow: true,
        calls: Mutex::new(vec![]),
    });
    let audit = Arc::new(Audit::default());
    let host = Host::new(
        root,
        gate.clone(),
        audit.clone(),
        HostConfig {
            git_program: Some("/usr/bin/git".into()),
            git_writes: policy,
            ..Default::default()
        },
    )
    .unwrap();
    (Arc::new(host), gate, audit)
}

async fn call(host: &Host, name: &str, arguments: Value) -> Result<Value, CapabilityError> {
    host.call(name, arguments, CancellationToken::new()).await
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
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}

fn git_bytes(root: &Path, args: &[&str]) -> Vec<u8> {
    let result = std::process::Command::new("/usr/bin/git")
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(result.status.success());
    result.stdout
}

fn init_repo(root: &Path) -> String {
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("note"), "one\n").unwrap();
    git(root, &["add", "note"]);
    git(root, &["commit", "-qm", "fixture"]);
    head(root)
}

fn head(root: &Path) -> String {
    git(root, &["rev-parse", "HEAD"]).trim().to_string()
}

fn index_tree(root: &Path) -> String {
    git(root, &["write-tree"]).trim().to_string()
}

fn cached_names(root: &Path) -> String {
    String::from_utf8(git_bytes(root, &["diff", "--cached", "--name-only", "--"])).unwrap()
}

fn capability_names(host: &Host) -> Vec<String> {
    host.capabilities()
        .into_iter()
        .map(|definition| definition.name)
        .collect()
}

fn stage_args(root: &Path, path: &str) -> Value {
    json!({
        "paths":[path],
        "expected_head":head(root),
        "expected_index_tree":index_tree(root),
    })
}

#[tokio::test]
async fn git_catalog_is_policy_gated_and_calls_use_permission_and_audit() {
    let tree = Tree::new();
    init_repo(tree.path());
    let (plain, gate, audit) = host(tree.path(), None);
    let names = capability_names(&plain);
    assert!(names.iter().any(|name| name == "git.blame"));
    for name in ["git.stage", "git.commit", "git.branch", "git.worktree"] {
        assert!(!names.iter().any(|candidate| candidate == name), "{name}");
    }
    assert!(
        call(&plain, "git.stage", json!({}))
            .await
            .unwrap_err()
            .message
            .contains("not configured")
    );
    assert!(gate.calls.lock().unwrap().is_empty());
    assert!(audit.records.lock().unwrap().is_empty());

    let (host, gate, audit) = host(tree.path(), Some(policy()));
    let names = capability_names(&host);
    for name in [
        "git.status",
        "git.diff",
        "git.log",
        "git.show",
        "git.blame",
        "git.stage",
        "git.commit",
        "git.branch",
        "git.worktree",
    ] {
        assert!(names.iter().any(|candidate| candidate == name), "{name}");
    }
    let value = call(&host, "git.blame", json!({"path":"note"}))
        .await
        .unwrap();
    assert_eq!(value["success"], true);
    assert_eq!(
        gate.calls.lock().unwrap().as_slice(),
        ["git.blame".to_string()]
    );
    assert_eq!(
        audit.records.lock().unwrap().as_slice(),
        ["start:git.blame".to_string(), "success".to_string()]
    );
}

#[tokio::test]
async fn blame_reads_a_snapshot_and_rejects_non_full_commit_ids_before_permission() {
    let tree = Tree::new();
    init_repo(tree.path());
    std::fs::write(tree.path().join("note"), "one\nchanged\n").unwrap();
    let (host, gate, _) = host(tree.path(), Some(policy()));
    let value = call(&host, "git.blame", json!({"path":"note"}))
        .await
        .unwrap();
    assert_eq!(value["success"], true);
    let stdout = value["stdout"].as_str().unwrap();
    assert!(stdout.contains("author Fixture"), "{stdout}");
    assert!(stdout.contains("changed"), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(tree.path().join("note")).unwrap(),
        "one\nchanged\n"
    );

    let before = gate.calls.lock().unwrap().len();
    assert!(
        call(&host, "git.blame", json!({"path":"note","commit":"abc"}))
            .await
            .is_err()
    );
    assert_eq!(gate.calls.lock().unwrap().len(), before);
    assert!(
        call(&host, "git.blame", json!({"path":"../note"}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn stage_and_commit_require_exact_head_index_tree_and_record_host_identity() {
    let tree = Tree::new();
    let original = init_repo(tree.path());
    let original_tree = index_tree(tree.path());
    std::fs::write(tree.path().join("note"), "two\n").unwrap();
    let (host, _, _) = host(tree.path(), Some(policy()));

    let zero = "0".repeat(40);
    assert!(
        call(
            &host,
            "git.stage",
            json!({"paths":["note"],"expected_head":zero,"expected_index_tree":original_tree})
        )
        .await
        .is_err()
    );
    assert_eq!(cached_names(tree.path()), "");

    let staged = call(
        &host,
        "git.stage",
        json!({"paths":["note"],"expected_head":original,"expected_index_tree":original_tree}),
    )
    .await
    .unwrap();
    assert_eq!(staged["head"], original);
    let staged_tree = staged["index_tree"].as_str().unwrap().to_string();
    assert_ne!(staged_tree, original_tree);
    assert_eq!(cached_names(tree.path()), "note\n");

    // A second stage using the pre-stage index tree must not silently accept a
    // concurrent index change.
    assert!(
        call(
            &host,
            "git.stage",
            json!({"paths":["note"],"expected_head":original,"expected_index_tree":original_tree})
        )
        .await
        .is_err()
    );
    assert_eq!(index_tree(tree.path()), staged_tree);

    assert!(
        call(
            &host,
            "git.commit",
            json!({"message":"host message","expected_head":original,"expected_index_tree":original_tree})
        )
        .await
        .is_err()
    );
    assert_eq!(head(tree.path()), original);

    let committed = call(
        &host,
        "git.commit",
        json!({"message":"host message","expected_head":original,"expected_index_tree":staged_tree}),
    )
    .await
    .unwrap();
    let commit = committed["commit"].as_str().unwrap();
    assert_eq!(head(tree.path()), commit);
    assert_eq!(committed["parent"], original);
    assert_eq!(committed["tree"], staged_tree);
    let identity = git(
        tree.path(),
        &["show", "-s", "--format=%an|%ae|%cn|%ce", "HEAD"],
    );
    assert_eq!(
        identity.trim(),
        "CAYW Host|host@example.invalid|CAYW Host|host@example.invalid"
    );
    assert_eq!(
        git(tree.path(), &["log", "-1", "--format=%B"]).trim(),
        "host message"
    );

    // A model-supplied identity is not part of the schema and cannot reach the
    // repository.
    assert!(
        call(
            &host,
            "git.commit",
            json!({
                "message":"other",
                "author_name":"Model",
                "expected_head":commit,
                "expected_index_tree":staged_tree
            })
        )
        .await
        .is_err()
    );
    assert_eq!(head(tree.path()), commit);
}

#[tokio::test]
async fn stage_rejects_unsafe_paths_and_directories_without_changing_the_index() {
    let tree = Tree::new();
    let head_oid = init_repo(tree.path());
    std::fs::create_dir(tree.path().join("sub")).unwrap();
    std::fs::write(tree.path().join("sub/inside"), "inside\n").unwrap();
    let outside = Tree::new();
    std::fs::write(outside.path().join("outside"), "outside\n").unwrap();
    std::os::unix::fs::symlink(outside.path().join("outside"), tree.path().join("link")).unwrap();
    let tree_oid = index_tree(tree.path());
    let (host, gate, _) = host(tree.path(), Some(policy()));

    for path in ["../escape", ".git/config", "link", "sub"] {
        let before = gate.calls.lock().unwrap().len();
        assert!(
            call(
                &host,
                "git.stage",
                json!({
                    "paths":[path],
                    "expected_head":head_oid,
                    "expected_index_tree":tree_oid
                })
            )
            .await
            .is_err(),
            "{path}"
        );
        if matches!(path, "../escape" | ".git/config") {
            assert_eq!(gate.calls.lock().unwrap().len(), before);
        }
        assert_eq!(index_tree(tree.path()), tree_oid, "{path}");
        assert_eq!(head(tree.path()), head_oid, "{path}");
    }

    assert!(
        call(
            &host,
            "git.stage",
            json!({
                "paths":["note","note"],
                "expected_head":head_oid,
                "expected_index_tree":tree_oid
            })
        )
        .await
        .is_err()
    );
    assert_eq!(index_tree(tree.path()), tree_oid);
}

#[tokio::test]
async fn branch_writes_check_head_and_controlled_names_and_safe_delete() {
    let tree = Tree::new();
    let base = init_repo(tree.path());
    let (host, _, _) = host(tree.path(), Some(policy()));

    let before = call(
        &host,
        "git.branch",
        json!({
            "action":"create","name":"-bad","commit":base,"expected_head":base
        }),
    )
    .await;
    assert!(before.is_err());

    assert!(
        call(
            &host,
            "git.branch",
            json!({
                "action":"create","name":"topic","commit":base,"expected_head":"0".repeat(40)
            })
        )
        .await
        .is_err()
    );
    assert!(
        std::process::Command::new("/usr/bin/git")
            .args(["rev-parse", "--verify", "--quiet", "refs/heads/topic"])
            .current_dir(tree.path())
            .status()
            .unwrap()
            .code()
            != Some(0)
    );

    call(
        &host,
        "git.branch",
        json!({
            "action":"create","name":"topic","commit":base,"expected_head":base
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        git(tree.path(), &["rev-parse", "refs/heads/topic"]).trim(),
        base
    );
    assert!(
        call(
            &host,
            "git.branch",
            json!({
                "action":"create","name":"topic","commit":base,"expected_head":base
            })
        )
        .await
        .is_err()
    );

    call(
        &host,
        "git.branch",
        json!({
            "action":"delete","name":"topic","commit":base,"expected_head":base
        }),
    )
    .await
    .unwrap();
    assert!(
        std::process::Command::new("/usr/bin/git")
            .args(["rev-parse", "--verify", "--quiet", "refs/heads/topic"])
            .current_dir(tree.path())
            .status()
            .unwrap()
            .code()
            != Some(0)
    );

    git(tree.path(), &["checkout", "-q", "-b", "unmerged"]);
    std::fs::write(tree.path().join("other"), "other\n").unwrap();
    git(tree.path(), &["add", "other"]);
    git(tree.path(), &["commit", "-qm", "unmerged"]);
    let unmerged = head(tree.path());
    git(tree.path(), &["checkout", "-q", "main"]);
    assert!(
        call(
            &host,
            "git.branch",
            json!({
                "action":"delete","name":"unmerged","commit":unmerged,"expected_head":base
            })
        )
        .await
        .is_err()
    );
    assert_eq!(
        git(tree.path(), &["rev-parse", "refs/heads/unmerged"]).trim(),
        unmerged
    );
}

#[tokio::test]
async fn worktree_operations_stay_under_the_fixed_root_and_verify_registration() {
    let tree = Tree::new();
    let base = init_repo(tree.path());
    git(tree.path(), &["branch", "wt", &base]);
    let (host, gate, _) = host(tree.path(), Some(policy()));

    let created = call(
        &host,
        "git.worktree",
        json!({
            "action":"create","name":"one","branch":"wt","commit":base,"expected_head":base
        }),
    )
    .await
    .unwrap();
    assert_eq!(created["path"], ".cayw-worktrees/one");
    let worktree = tree.path().join(".cayw-worktrees/one");
    assert!(worktree.join("note").is_file());
    assert_eq!(
        std::fs::read_to_string(worktree.join("note")).unwrap(),
        "one\n"
    );
    let registration = git(tree.path(), &["worktree", "list", "--porcelain"]);
    assert!(
        registration.contains(".cayw-worktrees/one"),
        "{registration}"
    );
    let control = std::fs::read_to_string(worktree.join(".git")).unwrap();
    assert!(control.starts_with("gitdir: "), "{control}");
    assert!(control.contains(".git/worktrees/"), "{control}");

    let before = gate.calls.lock().unwrap().len();
    assert!(
        call(&host, "git.worktree", json!({
            "action":"create","name":"../escape","branch":"wt","commit":base,"expected_head":base
        }))
        .await
        .is_err()
    );
    assert_eq!(gate.calls.lock().unwrap().len(), before);
    assert!(
        call(&host, "git.worktree", json!({
            "action":"create","name":"two","path":"/tmp/escape","branch":"wt","commit":base,"expected_head":base
        }))
        .await
        .is_err()
    );
    assert!(
        call(
            &host,
            "git.worktree",
            json!({
                "action":"create","name":"one","branch":"wt","commit":base,"expected_head":base
            })
        )
        .await
        .is_err()
    );
    assert!(
        call(&host, "git.worktree", json!({
            "action":"create","name":"missing","branch":"no-such-branch","commit":base,"expected_head":base
        }))
        .await
        .is_err()
    );
    assert!(!tree.path().join(".cayw-worktrees/missing").exists());

    call(
        &host,
        "git.worktree",
        json!({
            "action":"remove","name":"one","expected_head":base
        }),
    )
    .await
    .unwrap();
    assert!(!worktree.exists());
    assert!(
        !git(tree.path(), &["worktree", "list", "--porcelain"]).contains(".cayw-worktrees/one")
    );
    assert!(
        call(
            &host,
            "git.worktree",
            json!({
                "action":"remove","name":"one","expected_head":base
            })
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn worktree_remove_refuses_an_external_gitdir_pointer() {
    let tree = Tree::new();
    let base = init_repo(tree.path());
    git(tree.path(), &["branch", "wt", &base]);
    let outside = Tree::new();
    git(outside.path(), &["init", "-q"]);
    std::fs::create_dir_all(outside.path().join(".git/worktrees/evil")).unwrap();
    let (host, _, _) = host(tree.path(), Some(policy()));
    call(
        &host,
        "git.worktree",
        json!({
            "action":"create","name":"one","branch":"wt","commit":base,"expected_head":base
        }),
    )
    .await
    .unwrap();
    let worktree = tree.path().join(".cayw-worktrees/one");
    std::fs::write(
        worktree.join(".git"),
        format!(
            "gitdir: {}\n",
            outside.path().join(".git/worktrees/evil").display()
        ),
    )
    .unwrap();

    assert!(
        call(
            &host,
            "git.worktree",
            json!({
                "action":"remove","name":"one","expected_head":base
            })
        )
        .await
        .is_err()
    );
    assert!(worktree.exists());
    assert!(git(tree.path(), &["worktree", "list", "--porcelain"]).contains(".cayw-worktrees/one"));
}

#[tokio::test]
async fn worktree_remove_never_forces_a_dirty_linked_worktree() {
    let tree = Tree::new();
    let base = init_repo(tree.path());
    git(tree.path(), &["branch", "wt", &base]);
    let (host, _, _) = host(tree.path(), Some(policy()));
    call(
        &host,
        "git.worktree",
        json!({
            "action":"create","name":"one","branch":"wt","commit":base,"expected_head":base
        }),
    )
    .await
    .unwrap();
    let worktree = tree.path().join(".cayw-worktrees/one");
    std::fs::write(worktree.join("untracked"), "keep\n").unwrap();
    assert!(
        call(
            &host,
            "git.worktree",
            json!({
                "action":"remove","name":"one","expected_head":base
            })
        )
        .await
        .is_err()
    );
    assert!(worktree.join("untracked").is_file());
    std::fs::remove_file(worktree.join("untracked")).unwrap();
    call(
        &host,
        "git.worktree",
        json!({
            "action":"remove","name":"one","expected_head":base
        }),
    )
    .await
    .unwrap();
    assert!(!worktree.exists());
}

#[tokio::test]
async fn writes_refuse_a_symlinked_or_external_git_directory() {
    let tree = Tree::new();
    let base = init_repo(tree.path());
    let outside = Tree::new();
    let outside_head = init_repo(outside.path());
    let tree_oid = index_tree(tree.path());
    let (host, gate, _) = host(tree.path(), Some(policy()));

    std::fs::rename(tree.path().join(".git"), tree.path().join("git-store")).unwrap();
    std::os::unix::fs::symlink("git-store", tree.path().join(".git")).unwrap();
    assert!(
        call(
            &host,
            "git.stage",
            json!({
                "paths":["note"],"expected_head":base,"expected_index_tree":tree_oid
            })
        )
        .await
        .is_err()
    );
    assert_eq!(gate.calls.lock().unwrap().len(), 1);
    assert_eq!(head(outside.path()), outside_head);
    assert_eq!(head(tree.path()), base);

    std::fs::remove_file(tree.path().join(".git")).unwrap();
    std::fs::write(
        tree.path().join(".git"),
        format!("gitdir: {}\n", outside.path().join(".git").display()),
    )
    .unwrap();
    assert!(
        call(
            &host,
            "git.stage",
            json!({
                "paths":["note"],"expected_head":base,"expected_index_tree":tree_oid
            })
        )
        .await
        .is_err()
    );
    assert_eq!(head(outside.path()), outside_head);
    // The ambient fixture Git follows the gitdir file, but the Host write path
    // never did; the outside repository is unchanged.
    assert_eq!(head(tree.path()), outside_head);
}

#[tokio::test]
async fn writes_refuse_alternates_and_dangerous_local_config() {
    let tree = Tree::new();
    let base = init_repo(tree.path());
    let tree_oid = index_tree(tree.path());
    let outside = Tree::new();
    git(outside.path(), &["init", "-q"]);
    let (host, _, _) = host(tree.path(), Some(policy()));

    std::fs::create_dir_all(tree.path().join(".git/objects/info")).unwrap();
    std::fs::write(
        tree.path().join(".git/objects/info/alternates"),
        format!("{}\n", outside.path().join(".git/objects").display()),
    )
    .unwrap();
    assert!(
        call(
            &host,
            "git.stage",
            json!({
                "paths":["note"],"expected_head":base,"expected_index_tree":tree_oid
            })
        )
        .await
        .is_err()
    );
    assert_eq!(index_tree(tree.path()), tree_oid);
    std::fs::remove_file(tree.path().join(".git/objects/info/alternates")).unwrap();

    let marker = outside.path().join("filter-marker");
    git(
        tree.path(),
        &[
            "config",
            "filter.evil.clean",
            &format!("/bin/sh -c 'touch {}'", marker.display()),
        ],
    );
    assert!(
        call(
            &host,
            "git.stage",
            json!({
                "paths":["note"],"expected_head":base,"expected_index_tree":tree_oid
            })
        )
        .await
        .is_err()
    );
    assert!(!marker.exists());
    assert_eq!(index_tree(tree.path()), tree_oid);

    git(tree.path(), &["config", "--unset", "filter.evil.clean"]);
    git(
        tree.path(),
        &[
            "config",
            "include.path",
            outside.path().join("config").to_str().unwrap(),
        ],
    );
    assert!(
        call(
            &host,
            "git.stage",
            json!({
                "paths":["note"],"expected_head":base,"expected_index_tree":tree_oid
            })
        )
        .await
        .is_err()
    );
    assert_eq!(index_tree(tree.path()), tree_oid);
}

#[tokio::test]
async fn commit_disables_hooks_and_signing_even_if_repository_config_asks_for_them() {
    let tree = Tree::new();
    let base = init_repo(tree.path());
    let marker = tree.path().join("hook-marker");
    std::fs::create_dir_all(tree.path().join(".git/hooks")).unwrap();
    let hook = tree.path().join(".git/hooks/pre-commit");
    std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(tree.path(), &["config", "commit.gpgSign", "true"]);
    std::fs::write(tree.path().join("note"), "two\n").unwrap();
    let (host, _, _) = host(tree.path(), Some(policy()));
    let staged = call(&host, "git.stage", stage_args(tree.path(), "note"))
        .await
        .unwrap();
    let tree_oid = staged["index_tree"].as_str().unwrap().to_string();
    call(
        &host,
        "git.commit",
        json!({
            "message":"hook free","expected_head":base,"expected_index_tree":tree_oid
        }),
    )
    .await
    .unwrap();
    assert!(!marker.exists());
    assert_ne!(head(tree.path()), base);
}

struct Script(PathBuf);
impl Script {
    fn new(contents: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "cayw-git-wrapper-{}-{}.sh",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self(path)
    }
}
impl Drop for Script {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[tokio::test]
async fn interrupted_device_write_remains_unknown_and_is_not_retried() {
    let tree = Tree::new();
    let base = init_repo(tree.path());
    let tree_oid = index_tree(tree.path());
    std::fs::write(tree.path().join("note"), "two\n").unwrap();
    let ready = tree.path().join("wrapper-ready");
    let script = Script::new(&format!(
        "#!/bin/sh\ncase \" $* \" in\n  *\" add \"*)\n    /usr/bin/git \"$@\"\n    code=$?\n    : > '{}'\n    sleep 30\n    exit $code\n    ;;\nesac\nexec /usr/bin/git \"$@\"\n",
        ready.display()
    ));
    let gate = Arc::new(Gate {
        allow: true,
        calls: Mutex::new(vec![]),
    });
    let audit = Arc::new(Audit::default());
    let host = Arc::new(
        Host::new(
            tree.path(),
            gate.clone(),
            audit.clone(),
            HostConfig {
                git_program: Some(script.0.clone()),
                git_writes: Some(policy()),
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let cancel = CancellationToken::new();
    let task = {
        let host = host.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            host.call(
                "git.stage",
                json!({
                    "paths":["note"],
                    "expected_head":base,
                    "expected_index_tree":tree_oid
                }),
                cancel,
            )
            .await
        })
    };
    for _ in 0..500 {
        if ready.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ready.exists(), "git add did not run");
    let staged_tree = index_tree(tree.path());
    cancel.cancel();
    let error = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.outcome_unknown, "{error}");
    assert_eq!(index_tree(tree.path()), staged_tree);
    assert_eq!(cached_names(tree.path()), "note\n");
    assert!(host.outcome_unknown());
    assert!(audit.records.lock().unwrap().contains(&"unknown".into()));

    let permission_calls = gate.calls.lock().unwrap().len();
    assert!(
        host.call("git.status", json!({}), CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(gate.calls.lock().unwrap().len(), permission_calls);
    assert_eq!(index_tree(tree.path()), staged_tree);
}
