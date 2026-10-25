#![cfg(unix)]

use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{AuditSink, Host, HostConfig, PermissionGate, Profile, ProfileKind};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cayw-process-contracts-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Allow;
#[async_trait]
impl PermissionGate for Allow {
    async fn authorize(
        &self,
        _: &str,
        _: &Value,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
}
#[derive(Default)]
struct Audit(Mutex<Vec<Value>>);
#[async_trait]
impl AuditSink for Audit {
    async fn started(&self, name: &str, _: &Value) -> Result<String, CapabilityError> {
        let mut records = self.0.lock().unwrap();
        let id = records.len().to_string();
        records.push(json!({"id":id,"phase":"running","name":name}));
        Ok(id)
    }
    async fn settled(
        &self,
        id: &str,
        result: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        self.0.lock().unwrap().push(match result {
            Ok(value) => json!({"id":id,"phase":"succeeded","value":value}),
            Err(error) => json!({"id":id,"phase":if error.outcome_unknown {"unknown"} else {"failed"},"message":error.message}),
        });
        Ok(())
    }
    fn dropped(&self, id: &str) -> Result<(), CapabilityError> {
        self.0
            .lock()
            .unwrap()
            .push(json!({"id":id,"phase":"unknown","dropped":true}));
        Ok(())
    }
}
fn fixture() -> PathBuf {
    env!("CARGO_BIN_EXE_cayw-host-test-process").into()
}
fn profile(program: PathBuf, mode: &str) -> Profile {
    Profile {
        kind: ProfileKind::Check,
        program,
        args: vec![mode.into()],
        env: BTreeMap::new(),
        timeout: Duration::from_secs(20),
        max_output_bytes: 4096,
        allow_filter: false,
        io: cayw_device::ProcessIo::Pipes,
    }
}
fn host(root: &Path, config: HostConfig) -> (Arc<Host>, Arc<Audit>) {
    let audit = Arc::new(Audit::default());
    (
        Arc::new(Host::new(root, Arc::new(Allow), audit.clone(), config).unwrap()),
        audit,
    )
}
async fn call(host: &Host, name: &str, args: Value) -> Result<Value, CapabilityError> {
    host.call(name, args, CancellationToken::new()).await
}
async fn ready(path: &Path) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(bytes) = fs::read(path)
                && let Ok(value) = serde_json::from_slice(&bytes)
            {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fixture did not publish readiness")
}
async fn ready_bytes(path: &Path, expected: &[u8]) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(bytes) = fs::read(path)
                && bytes == expected
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fixture did not publish expected bytes")
}
fn assert_gone(pid: u32) {
    let observed = std::process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .output()
        .unwrap();
    assert!(!observed.status.success(), "PID {pid} remained alive");
}
fn repository(root: &Path) {
    fs::create_dir_all(root.join(".git/objects")).unwrap();
    fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
    fs::write(root.join(".git/HEAD"), "ref: refs/heads/fixture\n").unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn configured_pty_profile_runs_through_permission_audit_and_the_same_device() {
    let root = Root::new();
    let mut terminal = profile(fixture(), "terminal");
    terminal.io = cayw_device::ProcessIo::Pty {
        columns: 80,
        rows: 24,
    };
    let (host, audit) = host(
        &root.0,
        HostConfig {
            profiles: BTreeMap::from([("terminal".into(), terminal)]),
            ..Default::default()
        },
    );
    let result = call(&host, "check.run", json!({"profile":"terminal"}))
        .await
        .unwrap();
    host.drain_processes().await.unwrap();
    let output = result["stdout"].as_str().unwrap();
    let observation: Value = serde_json::from_str(output.lines().next().unwrap()).unwrap();
    assert_eq!(
        observation,
        json!({"stdin":true,"stdout":true,"stderr":true,"controlling_terminal":true})
    );
    assert!(output.contains("terminal-stderr"));
    assert_eq!(result["stderr"], "");
    assert_eq!(result["output_complete"], true);
    let records = audit.0.lock().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["phase"], "running");
    assert_eq!(records[1]["phase"], "succeeded");
    assert_eq!(records[0]["id"], records[1]["id"]);
}

// A Git read now runs in the held root rather than a copied worktree. Renaming
// that pathname while the process runs must not redirect it, and a third-party
// replacement must be preserved. Private-view cleanup itself is covered by the
// process unit tests, which can address the view directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_read_holds_its_working_directory_across_path_replacement() {
    for case in ["control", "directory", "symlink"] {
        let replacement = case != "control";
        let root = Root::new();
        repository(&root.0);
        let ready_path = root.0.join("ready");
        let release = root.0.with_extension("release");
        fs::write(
            root.0.join("probe-control.json"),
            serde_json::to_vec(&json!({"ready":ready_path,"release":release})).unwrap(),
        )
        .unwrap();
        let (host, audit) = host(
            &root.0,
            HostConfig {
                git_program: Some(fixture()),
                ..Default::default()
            },
        );
        let cloned = host.clone();
        let task = tokio::spawn(async move { call(&cloned, "git.status", json!({})).await });
        let observed = ready(&ready_path).await;
        let pid = observed["pid"].as_u64().unwrap() as u32;
        let cwd = PathBuf::from(observed["cwd"].as_str().unwrap());
        assert_eq!(
            fs::canonicalize(&cwd).unwrap(),
            fs::canonicalize(&root.0).unwrap(),
            "a Git read runs in the held root, not a worktree copy"
        );
        let moved = root.0.with_extension("moved");
        let identity = fs::metadata(&cwd).unwrap();
        let outside = root.0.with_extension("third-party");
        if replacement {
            fs::rename(&cwd, &moved).unwrap();
            if case == "symlink" {
                fs::create_dir(&outside).unwrap();
                std::os::unix::fs::symlink(&outside, &cwd).unwrap();
            } else {
                fs::create_dir(&cwd).unwrap();
            }
            fs::write(cwd.join("third-party-marker"), "preserve").unwrap();
        }
        fs::write(&release, "release").unwrap();
        let result = task.await.unwrap();
        let drained = host.drain_processes().await;
        let marker = fs::read(cwd.join("third-party-marker")).ok();
        let moved_identity = fs::metadata(&moved).ok().map(|m| (m.dev(), m.ino()));
        let records = audit.0.lock().unwrap().clone();
        println!(
            "{}",
            json!({"case":case,"pid":pid,"result":format!("{result:?}"),"drain":format!("{drained:?}"),"marker":marker,"owned_identity":moved_identity,"audit":records})
        );
        assert_gone(pid);
        assert_eq!(result.unwrap()["success"], true, "{case}");
        drained.unwrap();
        assert!(!host.outcome_unknown());
        assert_eq!(records.last().unwrap()["phase"], "succeeded");
        if replacement {
            // The held descriptor keeps the original directory; the renamed or
            // linked replacement is third-party data and must stay intact.
            assert_eq!(marker.as_deref(), Some(b"preserve".as_slice()));
            assert_eq!(moved_identity, Some((identity.dev(), identity.ino())));
            assert!(moved.join("probe-control.json").is_file());
            if case == "symlink" {
                fs::remove_file(&cwd).unwrap();
            } else {
                fs::remove_dir_all(&cwd).unwrap();
            }
            fs::remove_dir_all(&moved).unwrap();
            let _ = fs::remove_dir_all(&outside);
        }
        let _ = fs::remove_file(&release);
        assert_eq!(host.active_processes(), 0);
        assert_eq!(records[0]["id"], records[1]["id"]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_git_read_after_root_rename_uses_the_held_repository() {
    let root = Root::new();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("/usr/bin/git")
            .args(args)
            .current_dir(&root.0)
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
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    fs::write(root.0.join("note"), "one\n").unwrap();
    git(&["add", "note"]);
    git(&["commit", "-qm", "original-held-repository"]);
    let ready_path = root.0.join("ready");
    let release = root.0.with_extension("release");
    fs::write(
        root.0.join("probe-control.json"),
        serde_json::to_vec(&json!({"ready":ready_path,"release":release,"run_git":true})).unwrap(),
    )
    .unwrap();
    let (host, _) = host(
        &root.0,
        HostConfig {
            git_program: Some(fixture()),
            ..Default::default()
        },
    );
    let cloned = host.clone();
    let task = tokio::spawn(async move { call(&cloned, "git.log", json!({"limit":1})).await });
    ready(&ready_path).await;
    let moved = root.0.with_extension("held");
    fs::rename(&root.0, &moved).unwrap();
    fs::create_dir(&root.0).unwrap();
    fs::write(root.0.join("replacement-marker"), "preserve").unwrap();
    fs::write(&release, "release").unwrap();
    let result = task.await.unwrap();
    let drained = host.drain_processes().await;
    fs::remove_file(&release).unwrap();
    assert_eq!(
        fs::read(root.0.join("replacement-marker")).unwrap(),
        b"preserve"
    );
    fs::remove_dir_all(&moved).unwrap();
    let result = result.unwrap();
    assert_eq!(result["success"], true, "{result}");
    assert!(
        result["stdout"]
            .as_str()
            .unwrap()
            .contains("original-held-repository"),
        "{result}"
    );
    drained.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replaced_index_is_discarded_instead_of_returning_another_view() {
    let root = Root::new();
    repository(&root.0);
    fs::write(root.0.join(".git/index"), b"original").unwrap();
    let ready_path = root.0.join("ready");
    let release = root.0.with_extension("release");
    fs::write(
        root.0.join("probe-control.json"),
        serde_json::to_vec(&json!({"ready":ready_path,"release":release})).unwrap(),
    )
    .unwrap();
    let (host, audit) = host(
        &root.0,
        HostConfig {
            git_program: Some(fixture()),
            ..Default::default()
        },
    );
    let cloned = host.clone();
    let task = tokio::spawn(async move { call(&cloned, "git.status", json!({})).await });
    ready(&ready_path).await;
    fs::rename(root.0.join(".git/index"), root.0.join(".git/old-index")).unwrap();
    fs::write(root.0.join(".git/index"), b"replacement").unwrap();
    fs::write(&release, "release").unwrap();
    let result = task.await.unwrap();
    let drained = host.drain_processes().await;
    fs::remove_file(&release).unwrap();
    let error = result.expect_err("a replacement index must not be returned as the held view");
    assert!(!error.outcome_unknown, "{error}");
    assert!(error.message.contains("index"), "{error}");
    assert_eq!(fs::read(root.0.join(".git/index")).unwrap(), b"replacement");
    assert_eq!(audit.0.lock().unwrap().last().unwrap()["phase"], "failed");
    drained.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_git_read_keeps_unknown_when_its_index_was_replaced() {
    let root = Root::new();
    repository(&root.0);
    fs::write(root.0.join(".git/index"), b"original").unwrap();
    let ready_path = root.0.join("ready");
    let release = root.0.with_extension("release");
    fs::write(
        root.0.join("probe-control.json"),
        serde_json::to_vec(&json!({"ready":ready_path,"release":release})).unwrap(),
    )
    .unwrap();
    let (host, audit) = host(
        &root.0,
        HostConfig {
            git_program: Some(fixture()),
            ..Default::default()
        },
    );
    let cancel = CancellationToken::new();
    let cloned_cancel = cancel.clone();
    let cloned = host.clone();
    let task =
        tokio::spawn(async move { cloned.call("git.status", json!({}), cloned_cancel).await });
    ready(&ready_path).await;
    fs::rename(root.0.join(".git/index"), root.0.join(".git/old-index")).unwrap();
    fs::write(root.0.join(".git/index"), b"replacement").unwrap();
    cancel.cancel();
    let error = task
        .await
        .unwrap()
        .expect_err("cancelled launched process retains uncertainty");
    host.drain_processes().await.unwrap();
    assert!(error.outcome_unknown, "{error}");
    assert!(host.outcome_unknown());
    assert_eq!(audit.0.lock().unwrap().last().unwrap()["phase"], "unknown");
    assert_eq!(fs::read(root.0.join(".git/index")).unwrap(), b"replacement");
}

// EACCES is observed at real exec, rather than predicted from mode bits. The
// same Host must settle a known failure and admit a following real process.
#[tokio::test]
async fn exec_permission_refusal_is_known_and_does_not_fence_the_host() {
    let root = Root::new();
    let program = root.0.join("denied");
    fs::copy(fixture(), &program).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o600)).unwrap();
    let (host, audit) = host(
        &root.0,
        HostConfig {
            profiles: BTreeMap::from([("probe".into(), profile(program.clone(), "effect"))]),
            ..Default::default()
        },
    );
    let error = call(&host, "check.run", json!({"profile":"probe"}))
        .await
        .unwrap_err();
    let records = audit.0.lock().unwrap().clone();
    println!(
        "{}",
        json!({"case":"exec-denied","unknown":error.outcome_unknown,"message":error.message,"host_unknown":host.outcome_unknown(),"active":host.active_processes(),"audit":records})
    );
    assert!(!root.0.join("effect").exists());
    assert!(!root.0.join("effect.pid").exists());
    assert_eq!(host.active_processes(), 0);
    assert!(!error.outcome_unknown, "{error}");
    assert!(!host.outcome_unknown());
    assert_eq!(records[1]["phase"], "failed");
    assert_eq!(records[0]["id"], records[1]["id"]);
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        call(&host, "check.run", json!({"profile":"probe"}))
            .await
            .unwrap()["success"],
        true
    );
    assert_eq!(fs::read(root.0.join("effect")).unwrap(), b"started");
    let pid: u32 = fs::read_to_string(root.0.join("effect.pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert_gone(pid);
    host.drain_processes().await.unwrap();
    assert!(!host.outcome_unknown());
}

#[tokio::test]
async fn started_process_cancellation_retains_unknown_and_pairs_audit() {
    let root = Root::new();
    let (host, audit) = host(
        &root.0,
        HostConfig {
            profiles: BTreeMap::from([("probe".into(), profile(fixture(), "wait"))]),
            ..Default::default()
        },
    );
    let cancel = CancellationToken::new();
    let cloned = host.clone();
    let child = cancel.clone();
    let task = tokio::spawn(async move {
        cloned
            .call("check.run", json!({"profile":"probe"}), child)
            .await
    });
    // Wait for both markers: effect.pid is published last by the fixture, but
    // still poll effect so a scheduling race cannot unwrap NotFound.
    let pid: u32 = ready(&root.0.join("effect.pid")).await.as_u64().unwrap() as u32;
    ready_bytes(&root.0.join("effect"), b"started").await;
    cancel.cancel();
    let result = task.await.unwrap().unwrap_err();
    host.drain_processes().await.unwrap();
    assert_gone(pid);
    assert!(result.outcome_unknown);
    assert!(host.outcome_unknown());
    let records = audit.0.lock().unwrap();
    assert_eq!(records[1]["phase"], "unknown");
    assert_eq!(records[0]["id"], records[1]["id"]);
}
