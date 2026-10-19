#[path = "../examples/support/mod.rs"]
mod support;
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_bridge::*;
use cayw_codemode::CapabilityError;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Policy {
    cancel: bool,
    slow: bool,
}
#[async_trait]
impl RegistryPolicy for Policy {
    fn visible(&self, _: Visibility, _: &Provenance, _: &str) -> bool {
        if self.slow {
            std::thread::sleep(Duration::from_millis(12));
        }
        true
    }
    async fn authorize(
        &self,
        _: &AccessRequest,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError> {
        if self.cancel {
            cancel.cancel();
        }
        Ok(())
    }
}
struct Handle {
    calls: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
    barrier: Option<Arc<Barrier>>,
    delay: bool,
    deep: bool,
    mismatch: bool,
}
#[async_trait]
impl SourceHandle for Handle {
    fn ownership_key(&self) -> &str {
        if let Some(barrier) = &self.barrier {
            barrier.wait();
        }
        if self.delay {
            std::thread::sleep(Duration::from_millis(2));
        }
        "one-physical-connection"
    }
    async fn request(
        &self,
        request: SourceRequest,
        _: CancellationToken,
    ) -> Result<SourceReply, CapabilityError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut value = json!({"ok":true});
        if self.deep {
            for _ in 0..60000 {
                value = Value::Array(vec![value]);
            }
        }
        let mut key = request.key;
        if self.mismatch {
            key.id += 1;
        }
        Ok(SourceReply { key, value })
    }
    fn disconnect(&self) {
        self.closed.fetch_add(1, Ordering::SeqCst);
    }
}
fn identity() -> SourceIdentity {
    SourceIdentity {
        id: "source".into(),
        version: "1".into(),
        origin: "fixture:regression".into(),
    }
}
fn manifest() -> Manifest {
    Manifest {
        tools: vec![ToolSpec {
            name: "echo".into(),
            description: "echo".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            output_schema: None,
        }],
        ..Manifest::default()
    }
}
fn handle(calls: Arc<AtomicUsize>, closed: Arc<AtomicUsize>) -> Handle {
    Handle {
        calls,
        closed,
        barrier: None,
        delay: false,
        deep: false,
        mismatch: false,
    }
}
fn registry(cancel: bool, slow: bool) -> Registry {
    Registry::new(
        Limits {
            operation_timeout: if slow {
                Duration::from_millis(5)
            } else {
                Duration::from_secs(5)
            },
            ..Limits::default()
        },
        Arc::new(Policy { cancel, slow }),
    )
    .unwrap()
}
async fn invoke(registry: &Registry) -> Result<Value, CapabilityError> {
    support::consumer(registry)
        .call(
            "mcp.call",
            json!({"server":"source","tool":"echo","arguments":{}}),
            CancellationToken::new(),
        )
        .await
}

#[tokio::test]
async fn authorization_cancellation_before_dispatch_is_known_and_does_not_freeze_source() {
    let registry = registry(true, false);
    let calls = Arc::new(AtomicUsize::new(0));
    registry
        .register(
            identity(),
            manifest(),
            Box::new(handle(calls.clone(), Arc::default())),
        )
        .unwrap();
    assert!(!invoke(&registry).await.unwrap_err().outcome_unknown);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let snapshot = registry.snapshot("source").unwrap();
    assert!(!snapshot.outcome_unknown);
    assert!(snapshot.unresolved.is_empty());
}
#[tokio::test]
async fn synchronous_policy_time_counts_before_source_dispatch() {
    let registry = registry(false, true);
    let calls = Arc::new(AtomicUsize::new(0));
    registry
        .register(
            identity(),
            manifest(),
            Box::new(handle(calls.clone(), Arc::default())),
        )
        .unwrap();
    assert!(!invoke(&registry).await.unwrap_err().outcome_unknown);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!registry.snapshot("source").unwrap().outcome_unknown);
}
#[test]
fn skill_registration_rejects_names_host_cannot_read() {
    let registry = registry(false, false);
    let mut identity = identity();
    identity.id = "s".repeat(128);
    let mut manifest = Manifest::default();
    manifest.skills.push(SkillSpec {
        name: "x".repeat(128),
        description: "fixed".into(),
        content_sha256: content_sha256("fixed"),
    });
    assert!(
        registry
            .register(
                identity,
                manifest,
                Box::new(handle(Arc::default(), Arc::default()))
            )
            .is_err()
    );
}
#[tokio::test]
async fn maximum_host_readable_skill_name_is_listed_and_read_through_host() {
    struct SkillHandle;
    #[async_trait]
    impl SourceHandle for SkillHandle {
        fn ownership_key(&self) -> &str {
            "boundary-skill"
        }
        async fn request(
            &self,
            request: SourceRequest,
            _: CancellationToken,
        ) -> Result<SourceReply, CapabilityError> {
            Ok(SourceReply {
                key: request.key,
                value: json!("fixed"),
            })
        }
        fn disconnect(&self) {}
    }
    let registry = registry(false, false);
    let workspace = support::Workspace::new();
    let mut id = identity();
    id.id = "s".repeat(128);
    let name = format!("{}/{}", id.id, "x".repeat(127));
    let manifest = Manifest {
        skills: vec![SkillSpec {
            name: "x".repeat(127),
            description: "fixed".into(),
            content_sha256: content_sha256("fixed"),
        }],
        ..Manifest::default()
    };
    registry
        .register(id, manifest, Box::new(SkillHandle))
        .unwrap();
    let host = support::host(
        &workspace.0,
        support::consumer(&registry),
        Arc::default(),
        Arc::default(),
    );
    assert_eq!(
        support::host_call(&host, "skills.list", json!({}))
            .await
            .unwrap()["skills"][0]["name"],
        name
    );
    assert_eq!(
        support::host_call(&host, "skills.read", json!({"name":name}))
            .await
            .unwrap()["value"],
        "fixed"
    );
}

#[test]
fn rejected_owner_disconnect_callback_can_reenter_the_registry() {
    struct Reenter(Registry, Arc<AtomicUsize>);
    #[async_trait]
    impl SourceHandle for Reenter {
        fn ownership_key(&self) -> &str {
            "new-rejected-owner"
        }
        async fn request(
            &self,
            _: SourceRequest,
            _: CancellationToken,
        ) -> Result<SourceReply, CapabilityError> {
            unreachable!()
        }
        fn disconnect(&self) {
            assert!(self.0.snapshot("source").is_some());
            self.1.fetch_add(1, Ordering::SeqCst);
        }
    }
    let registry = registry(false, false);
    registry
        .register(
            identity(),
            manifest(),
            Box::new(handle(Arc::default(), Arc::default())),
        )
        .unwrap();
    let closed = Arc::new(AtomicUsize::new(0));
    assert!(
        registry
            .register(
                identity(),
                manifest(),
                Box::new(Reenter(registry.clone(), closed.clone()))
            )
            .is_err()
    );
    assert_eq!(closed.load(Ordering::SeqCst), 1);
}

#[test]
fn concurrent_duplicate_owner_never_disconnects_the_accepted_connection() {
    let registry = registry(false, false);
    let calls = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));
    let mut slow = handle(calls.clone(), closed.clone());
    slow.barrier = Some(barrier.clone());
    let mut fast = handle(calls, closed.clone());
    fast.barrier = Some(barrier);
    fast.delay = true;
    let mut heavy = manifest();
    let properties: serde_json::Map<String, Value> = (0..20)
        .map(|i| (format!("f{i}"), json!({"type":"integer","minimum":0})))
        .collect();
    heavy.tools=(0..128).map(|i|ToolSpec { name:format!("tool{i}"),description:"compile before acceptance".into(),input_schema:json!({"type":"object","properties":properties,"additionalProperties":false}),output_schema:None }).collect();
    let first = registry.clone();
    let first = std::thread::spawn(move || first.register(identity(), heavy, Box::new(slow)));
    let second = registry.clone();
    let second =
        std::thread::spawn(move || second.register(identity(), manifest(), Box::new(fast)));
    let a = first.join().unwrap();
    let b = second.join().unwrap();
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(
        closed.load(Ordering::SeqCst),
        0,
        "rejected wrapper closed the accepted physical owner"
    );
}
fn deep() -> Value {
    let mut value = Value::Null;
    for _ in 0..60000 {
        value = Value::Array(vec![value]);
    }
    value
}
#[tokio::test]
async fn hostile_owned_json_child() {
    let Some(case) = std::env::var_os("CAYW_BRIDGE_JSON_CHILD") else {
        return;
    };
    let registry = registry(false, false);
    match case.to_str().unwrap() {
        "manifest" | "output-schema" | "identity-reject" | "duplicate-schema" | "count-reject" => {
            let mut spec = manifest();
            let mut id = identity();
            if case == "output-schema" {
                spec.tools[0].output_schema = Some(deep());
            } else {
                spec.tools[0].input_schema = deep();
            }
            if case == "identity-reject" {
                id.id = "invalid/identity".into();
            }
            if case == "count-reject" {
                for _ in 0..129 {
                    spec.tools.push(manifest().tools.remove(0));
                }
            }
            if case == "duplicate-schema" {
                registry
                    .register(
                        identity(),
                        manifest(),
                        Box::new(handle(Arc::default(), Arc::default())),
                    )
                    .unwrap();
            }
            assert!(
                registry
                    .register(id, spec, Box::new(handle(Arc::default(), Arc::default())))
                    .is_err()
            );
        }
        "reply" | "mismatch" => {
            let mut handle = handle(Arc::default(), Arc::default());
            handle.deep = true;
            handle.mismatch = case == "mismatch";
            registry
                .register(identity(), manifest(), Box::new(handle))
                .unwrap();
            assert!(invoke(&registry).await.is_err());
        }
        "unpolled" => {
            let scope = registry
                .consumer()
                .scoped(
                    "mcp.call",
                    support::invocation(1),
                    Arc::new(support::FixtureJournal::default()),
                )
                .unwrap();
            drop(scope.call("mcp.call", deep(), CancellationToken::new()));
        }
        "arguments" => {
            let scope = registry
                .consumer()
                .scoped(
                    "mcp.call",
                    support::invocation(1),
                    Arc::new(support::FixtureJournal::default()),
                )
                .unwrap();
            assert!(
                scope
                    .call("mcp.call", deep(), CancellationToken::new())
                    .await
                    .is_err()
            );
        }
        "capability-reject" | "reused-scope" => {
            let scope = registry
                .consumer()
                .scoped(
                    "mcp.call",
                    support::invocation(1),
                    Arc::new(support::FixtureJournal::default()),
                )
                .unwrap();
            if case == "reused-scope" {
                assert!(
                    scope
                        .call("mcp.call", json!({}), CancellationToken::new())
                        .await
                        .is_err()
                );
            }
            assert!(
                scope
                    .call(
                        if case == "capability-reject" {
                            "skills.list"
                        } else {
                            "mcp.call"
                        },
                        deep(),
                        CancellationToken::new()
                    )
                    .await
                    .is_err()
            );
        }
        "cancel-arguments" => {
            let scope = registry
                .consumer()
                .scoped(
                    "mcp.call",
                    support::invocation(1),
                    Arc::new(support::FixtureJournal::default()),
                )
                .unwrap();
            let cancel = CancellationToken::new();
            cancel.cancel();
            assert!(scope.call("mcp.call", deep(), cancel).await.is_err());
        }
        _ => panic!("unknown child case"),
    }
}
#[test]
fn hostile_owned_json_is_rejected_without_aborting_a_real_child() {
    for case in [
        "reply",
        "mismatch",
        "arguments",
        "unpolled",
        "manifest",
        "output-schema",
        "identity-reject",
        "duplicate-schema",
        "count-reject",
        "capability-reject",
        "reused-scope",
        "cancel-arguments",
    ] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "hostile_owned_json_child", "--nocapture"])
            .env("CAYW_BRIDGE_JSON_CHILD", case)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{case}: {:?}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
