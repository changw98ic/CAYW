//! HostIdentity must cover every execution-relevant HostConfig field, not only
//! the catalog's selected operations. Otherwise an old receipt could authorize
//! a different terminal program, limit set, Git writer identity, or read mode.
use cayw_ai::CancellationToken;
use cayw_codemode::CapabilityError;
use cayw_host::{
    AuditSink, GitWritePolicy, Host, HostConfig, PermissionGate, ProcessIo, Profile, ProfileKind,
    TerminalConfig, TerminalLimits,
};
use cayw_requests::HostIdentity;
use serde_json::Value;
use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

struct Allow;
#[async_trait::async_trait]
impl PermissionGate for Allow {
    async fn authorize(
        &self,
        _: &str,
        _: &Value,
        _: CancellationToken,
    ) -> std::result::Result<(), CapabilityError> {
        Ok(())
    }
}
#[derive(Default)]
struct Audit;
#[async_trait::async_trait]
impl AuditSink for Audit {
    async fn started(&self, _: &str, _: &Value) -> std::result::Result<String, CapabilityError> {
        Ok("audit".into())
    }
    async fn settled(
        &self,
        _: &str,
        _: &std::result::Result<Value, CapabilityError>,
    ) -> std::result::Result<(), CapabilityError> {
        Ok(())
    }
    fn dropped(&self, _: &str) -> std::result::Result<(), CapabilityError> {
        Ok(())
    }
}
fn profile(program: &str, args: &[&str]) -> Profile {
    Profile {
        kind: ProfileKind::Check,
        program: program.into(),
        args: args.iter().map(|value| (*value).into()).collect(),
        env: BTreeMap::new(),
        timeout: Duration::from_secs(10),
        max_output_bytes: 4096,
        allow_filter: false,
        io: ProcessIo::Pipes,
    }
}
fn limits() -> TerminalLimits {
    TerminalLimits {
        max_sessions: 2,
        max_sessions_total: 8,
        max_input_bytes_per_write: 64,
        max_input_bytes_per_session: 256,
        max_output_bytes_per_session: 4096,
        max_read_bytes: 64,
        max_wait: Duration::from_secs(10),
    }
}
fn terminal(profiles: BTreeMap<String, Profile>, limits: TerminalLimits) -> TerminalConfig {
    TerminalConfig { profiles, limits }
}
fn config() -> HostConfig {
    HostConfig {
        frozen_reads: false,
        git_program: Some("/usr/bin/git".into()),
        git_writes: Some(GitWritePolicy {
            identity_name: "Fixture".into(),
            identity_email: "fixture@example.invalid".into(),
            allow_stage: true,
            allow_commit: false,
            allow_branch: false,
            allow_worktree: false,
        }),
        terminal_handoffs: Some(terminal(
            BTreeMap::from([("terminal".into(), profile("/bin/cat", &[]))]),
            limits(),
        )),
        ..HostConfig::default()
    }
}
fn fingerprint(root: &Path, config: &HostConfig) -> String {
    let host = Host::new(root, Arc::new(Allow), Arc::new(Audit), config.clone()).unwrap();
    HostIdentity::from_host(&host, config, "same-permission-policy")
        .unwrap()
        .fingerprint()
}
fn changed(root: &Path, label: &str, mutate: impl FnOnce(&mut HostConfig)) {
    let base = config();
    let mut next = base.clone();
    mutate(&mut next);
    assert_ne!(
        fingerprint(root, &base),
        fingerprint(root, &next),
        "HostIdentity ignored {label}; an old receipt could authorize the changed execution configuration"
    );
}

#[test]
fn host_identity_rotates_for_every_execution_relevant_configuration() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let root = root.join("workspace");

    // Same root and same permission policy ID produce a stable identity.
    assert_eq!(fingerprint(&root, &config()), fingerprint(&root, &config()));

    changed(&root, "terminal profile program", |config| {
        config
            .terminal_handoffs
            .as_mut()
            .unwrap()
            .profiles
            .insert("terminal".into(), profile("/usr/bin/true", &[]));
    });
    changed(&root, "terminal profile argv", |config| {
        config
            .terminal_handoffs
            .as_mut()
            .unwrap()
            .profiles
            .insert("terminal".into(), profile("/bin/cat", &["-"]));
    });
    changed(&root, "terminal profile environment", |config| {
        config
            .terminal_handoffs
            .as_mut()
            .unwrap()
            .profiles
            .get_mut("terminal")
            .unwrap()
            .env
            .insert("CAYW_TERMINAL".into(), "1".into());
    });
    changed(&root, "terminal profile timeout", |config| {
        config
            .terminal_handoffs
            .as_mut()
            .unwrap()
            .profiles
            .get_mut("terminal")
            .unwrap()
            .timeout = Duration::from_secs(11);
    });
    changed(&root, "terminal profile output budget", |config| {
        config
            .terminal_handoffs
            .as_mut()
            .unwrap()
            .profiles
            .get_mut("terminal")
            .unwrap()
            .max_output_bytes = 8192;
    });
    changed(&root, "terminal profile I/O mode", |config| {
        config
            .terminal_handoffs
            .as_mut()
            .unwrap()
            .profiles
            .get_mut("terminal")
            .unwrap()
            .io = ProcessIo::Pty {
            columns: 80,
            rows: 24,
        };
    });
    changed(&root, "the complete terminal profile set", |config| {
        config
            .terminal_handoffs
            .as_mut()
            .unwrap()
            .profiles
            .insert("second".into(), profile("/usr/bin/true", &[]));
    });
    changed(&root, "terminal session limits", |config| {
        config
            .terminal_handoffs
            .as_mut()
            .unwrap()
            .limits
            .max_sessions = 1;
    });
    changed(&root, "terminal lifetime session limits", |config| {
        config
            .terminal_handoffs
            .as_mut()
            .unwrap()
            .limits
            .max_sessions_total = 9;
    });
    changed(&root, "terminal input write limits", |config| {
        config
            .terminal_handoffs
            .as_mut()
            .unwrap()
            .limits
            .max_input_bytes_per_write = 32;
    });
    changed(&root, "terminal per-session input limits", |config| {
        config
            .terminal_handoffs
            .as_mut()
            .unwrap()
            .limits
            .max_input_bytes_per_session = 512;
    });
    changed(&root, "terminal output limits", |config| {
        config
            .terminal_handoffs
            .as_mut()
            .unwrap()
            .limits
            .max_output_bytes_per_session = 8192;
    });
    changed(&root, "terminal read limits", |config| {
        config
            .terminal_handoffs
            .as_mut()
            .unwrap()
            .limits
            .max_read_bytes = 32;
    });
    changed(&root, "terminal wait limits", |config| {
        config.terminal_handoffs.as_mut().unwrap().limits.max_wait = Duration::from_secs(11);
    });
    changed(&root, "terminal availability", |config| {
        config.terminal_handoffs = None;
    });

    changed(&root, "Git writer identity name", |config| {
        config.git_writes.as_mut().unwrap().identity_name = "Other".into();
    });
    changed(&root, "Git writer identity email", |config| {
        config.git_writes.as_mut().unwrap().identity_email = "other@example.invalid".into();
    });
    changed(&root, "Git stage permission", |config| {
        config.git_writes.as_mut().unwrap().allow_stage = false;
    });
    changed(&root, "Git commit permission", |config| {
        config.git_writes.as_mut().unwrap().allow_commit = true;
    });
    changed(&root, "Git branch permission", |config| {
        config.git_writes.as_mut().unwrap().allow_branch = true;
    });
    changed(&root, "Git worktree permission", |config| {
        config.git_writes.as_mut().unwrap().allow_worktree = true;
    });
    changed(&root, "Git write availability", |config| {
        config.git_writes = None;
    });

    changed(&root, "frozen read mode", |config| {
        config.frozen_reads = true;
    });
}
