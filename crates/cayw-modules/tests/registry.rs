use cayw_modules::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;

fn manifest(name: &str, capability: &str) -> ModuleManifest {
    ModuleManifest {
        name: name.into(),
        version: "1.0.0".into(),
        kind: ModuleKind::Native,
        protocol_version: MODULE_PROTOCOL_VERSION,
        capabilities: vec![ToolDefinition {
            name: capability.into(),
            description: "fixture".into(),
            parameters: json!({"type":"object","properties":{"n":{"type":"integer"}},"additionalProperties":false}),
        }],
        permissions: BTreeMap::new(),
    }
}
fn context(capability: &str) -> ModuleContext {
    ModuleContext::new([capability.into()], [])
}

struct Native {
    manifest: ModuleManifest,
    calls: AtomicUsize,
    started: Notify,
    cancelled: CancellationToken,
    hang: bool,
}
impl Native {
    fn new(name: &str, capability: &str, hang: bool) -> Self {
        Self {
            manifest: manifest(name, capability),
            calls: AtomicUsize::new(0),
            started: Notify::new(),
            cancelled: CancellationToken::new(),
            hang,
        }
    }
}
#[async_trait]
impl Module for Native {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }
    async fn call(
        &self,
        _method: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ModuleError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.started.notify_one();
        if self.hang {
            let observed = self.cancelled.clone();
            tokio::spawn(async move {
                cancel.cancelled().await;
                observed.cancel();
            });
            futures::future::pending().await
        } else {
            Ok(arguments)
        }
    }
}

#[tokio::test]
async fn registry_denies_missing_grants_and_permissions_before_invocation() {
    for (capabilities, permissions) in [(vec![], vec![]), (vec!["fixture.echo".into()], vec![])] {
        let registry = ModuleRegistry::new();
        let mut native = Native::new("fixture", "fixture.echo", false);
        native
            .manifest
            .permissions
            .insert("fixture.echo".into(), ["read".into()].into());
        let native = Arc::new(native);
        registry
            .load_native(
                native.clone(),
                ModuleContext::new(capabilities, permissions),
            )
            .unwrap();
        assert!(matches!(
            registry
                .call(
                    "fixture",
                    "fixture.echo",
                    json!({}),
                    CancellationToken::new(),
                    Duration::from_secs(1)
                )
                .await,
            Err(ModuleError::Denied(_))
        ));
        assert_eq!(native.calls.load(Ordering::Relaxed), 0);
    }
    let registry = ModuleRegistry::new();
    let mut native = Native::new("fixture", "fixture.echo", false);
    native
        .manifest
        .permissions
        .insert("fixture.echo".into(), ["read".into()].into());
    registry
        .load_native(
            Arc::new(native),
            ModuleContext::new(["fixture.echo".into()], ["read".into()]),
        )
        .unwrap();
    assert_eq!(
        registry
            .call(
                "fixture",
                "fixture.echo",
                json!({"n":2}),
                CancellationToken::new(),
                Duration::from_secs(1)
            )
            .await
            .unwrap(),
        json!({"n":2})
    );
}

#[tokio::test]
async fn registry_strictly_validates_arguments_and_never_converts_response_into_permission() {
    let registry = ModuleRegistry::new();
    let native = Arc::new(Native::new("fixture", "fixture.echo", false));
    registry
        .load_native(native.clone(), context("fixture.echo"))
        .unwrap();
    for arguments in [json!([]), json!({"n":"2"}), json!({"approved":true})] {
        assert!(matches!(
            registry
                .call(
                    "fixture",
                    "fixture.echo",
                    arguments,
                    CancellationToken::new(),
                    Duration::from_secs(1)
                )
                .await,
            Err(ModuleError::InvalidArguments(_))
        ));
    }
    assert!(matches!(
        registry
            .call(
                "fixture",
                "other",
                json!({}),
                CancellationToken::new(),
                Duration::from_secs(1)
            )
            .await,
        Err(ModuleError::Denied(_))
    ));
    assert_eq!(native.calls.load(Ordering::Relaxed), 0);
}

#[test]
fn duplicate_modules_capabilities_protocols_and_invalid_schemas_are_rejected() {
    let registry = ModuleRegistry::new();
    registry
        .load_native(
            Arc::new(Native::new("one", "first", false)),
            context("first"),
        )
        .unwrap();
    assert!(matches!(
        registry.load_native(
            Arc::new(Native::new("one", "second", false)),
            context("second")
        ),
        Err(ModuleError::Conflict(_))
    ));
    assert!(matches!(
        registry.load_native(
            Arc::new(Native::new("two", "first", false)),
            context("first")
        ),
        Err(ModuleError::Conflict(_))
    ));
    let mut native = Native::new("version", "version", false);
    native.manifest.protocol_version += 1;
    assert!(matches!(
        registry.load_native(Arc::new(native), context("version")),
        Err(ModuleError::Protocol(_))
    ));
    let mut native = Native::new("schema", "schema", false);
    native.manifest.capabilities[0].parameters = json!({"$ref":"https://invalid.example/schema"});
    assert!(matches!(
        registry.load_native(Arc::new(native), context("schema")),
        Err(ModuleError::InvalidManifest(_))
    ));
    assert_eq!(registry.list().len(), 1);
}

#[tokio::test]
async fn queued_name_call_selects_the_replacement_at_first_poll() {
    let registry = ModuleRegistry::new();
    let old = Arc::new(Native::new("fixture", "fixture.echo", false));
    registry
        .load_native(old.clone(), context("fixture.echo"))
        .unwrap();
    let queued = registry.call(
        "fixture",
        "fixture.echo",
        json!({"n": 7}),
        CancellationToken::new(),
        Duration::from_secs(1),
    );
    registry.unload("fixture").unwrap();
    let replacement = Arc::new(Native::new("fixture", "fixture.echo", false));
    registry
        .load_native(replacement.clone(), context("fixture.echo"))
        .unwrap();

    assert_eq!(queued.await.unwrap(), json!({"n": 7}));
    assert_eq!(old.calls.load(Ordering::Relaxed), 0);
    assert_eq!(replacement.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn queued_exact_call_rejects_an_identical_replacement_before_execution() {
    let registry = ModuleRegistry::new();
    let old = Arc::new(Native::new("fixture", "fixture.echo", false));
    let expected = registry
        .load_native(old.clone(), context("fixture.echo"))
        .unwrap();
    let queued = registry.call_exact(
        &expected,
        "fixture.echo",
        json!({"n": 7}),
        CancellationToken::new(),
        Duration::from_secs(1),
    );
    registry.unload("fixture").unwrap();
    let replacement = Arc::new(Native::new("fixture", "fixture.echo", false));
    let current = registry
        .load_native(replacement.clone(), context("fixture.echo"))
        .unwrap();
    assert_eq!(expected.manifest, current.manifest);
    assert_eq!(expected.registry_id, current.registry_id);
    assert_ne!(expected.generation, current.generation);

    let result = queued.await;
    assert_eq!(old.calls.load(Ordering::Relaxed), 0);
    assert_eq!(replacement.calls.load(Ordering::Relaxed), 0);
    let error = result.unwrap_err();
    assert!(matches!(error, ModuleError::Unavailable(_)));
    assert!(!error.outcome_unknown());
}

#[tokio::test]
async fn queued_exact_call_rejects_a_changed_manifest_without_refreshing_the_pin() {
    let registry = ModuleRegistry::new();
    let old = Arc::new(Native::new("fixture", "fixture.echo", false));
    let expected = registry
        .load_native(old.clone(), context("fixture.echo"))
        .unwrap();
    let queued = registry.call_exact(
        &expected,
        "fixture.echo",
        json!({}),
        CancellationToken::new(),
        Duration::from_secs(1),
    );
    registry.unload("fixture").unwrap();
    let mut replacement = Native::new("fixture", "fixture.echo", false);
    replacement.manifest.version = "2.0.0".into();
    let replacement = Arc::new(replacement);
    let current = registry
        .load_native(replacement.clone(), context("fixture.echo"))
        .unwrap();

    let error = queued.await.unwrap_err();
    assert!(matches!(error, ModuleError::Unavailable(_)));
    assert!(!error.outcome_unknown());
    assert_eq!(old.calls.load(Ordering::Relaxed), 0);
    assert_eq!(replacement.calls.load(Ordering::Relaxed), 0);
    assert_eq!(expected.manifest.version, "1.0.0");
    assert_eq!(current.manifest.version, "2.0.0");
    assert!(matches!(
        registry
            .call_exact(
                &expected,
                "fixture.echo",
                json!({}),
                CancellationToken::new(),
                Duration::from_secs(1),
            )
            .await,
        Err(ModuleError::Unavailable(_))
    ));
    assert_eq!(replacement.calls.load(Ordering::Relaxed), 0);
    assert_eq!(
        registry
            .call_exact(
                &current,
                "fixture.echo",
                json!({"n": 8}),
                CancellationToken::new(),
                Duration::from_secs(1),
            )
            .await
            .unwrap(),
        json!({"n": 8})
    );
    assert_eq!(replacement.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn exact_call_rejects_a_foreign_registry_with_matching_generation_and_manifest() {
    let original = ModuleRegistry::new();
    let original_module = Arc::new(Native::new("fixture", "fixture.echo", false));
    let expected = original
        .load_native(original_module.clone(), context("fixture.echo"))
        .unwrap();
    let registry = ModuleRegistry::new();
    let replacement = Arc::new(Native::new("fixture", "fixture.echo", false));
    let current = registry
        .load_native(replacement.clone(), context("fixture.echo"))
        .unwrap();
    assert_eq!(expected.manifest, current.manifest);
    assert_eq!(expected.generation, current.generation);
    assert_ne!(expected.registry_id, current.registry_id);
    let queued = registry.call_exact(
        &expected,
        "fixture.echo",
        json!({}),
        CancellationToken::new(),
        Duration::from_secs(1),
    );

    let error = queued.await.unwrap_err();
    assert!(matches!(error, ModuleError::Unavailable(_)));
    assert!(!error.outcome_unknown());
    assert_eq!(original_module.calls.load(Ordering::Relaxed), 0);
    assert_eq!(replacement.calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn exact_call_compares_the_full_manifest_even_with_matching_registry_and_generation() {
    let registry = ModuleRegistry::new();
    let native = Arc::new(Native::new("fixture", "fixture.echo", false));
    let current = registry
        .load_native(native.clone(), context("fixture.echo"))
        .unwrap();
    let mut versions = Vec::new();
    let mut changed = current.clone();
    changed.manifest.version = "2.0.0".into();
    versions.push(changed);
    let mut changed = current.clone();
    changed.manifest.protocol_version += 1;
    versions.push(changed);
    let mut changed = current.clone();
    changed.manifest.kind = ModuleKind::External;
    versions.push(changed);
    let mut changed = current.clone();
    changed.manifest.capabilities[0].description = "changed".into();
    versions.push(changed);
    let mut changed = current.clone();
    changed.manifest.capabilities[0].parameters = json!({"type": "object"});
    versions.push(changed);
    let mut changed = current.clone();
    changed
        .manifest
        .permissions
        .insert("fixture.echo".into(), ["write".into()].into());
    versions.push(changed);
    for expected in versions {
        let error = registry
            .call_exact(
                &expected,
                "fixture.echo",
                json!({}),
                CancellationToken::new(),
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ModuleError::Unavailable(_)));
        assert!(!error.outcome_unknown());
        assert_eq!(native.calls.load(Ordering::Relaxed), 0);
    }
}

#[tokio::test]
async fn queued_exact_call_to_an_unloaded_module_never_starts() {
    let registry = ModuleRegistry::new();
    let native = Arc::new(Native::new("fixture", "fixture.echo", false));
    let expected = registry
        .load_native(native.clone(), context("fixture.echo"))
        .unwrap();
    let queued = registry.call_exact(
        &expected,
        "fixture.echo",
        json!({}),
        CancellationToken::new(),
        Duration::from_secs(1),
    );
    registry.unload("fixture").unwrap();

    let error = queued.await.unwrap_err();
    assert!(matches!(error, ModuleError::NotFound(_)));
    assert!(!error.outcome_unknown());
    assert_eq!(native.calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn exact_call_enforces_grants_permissions_and_schema_before_invocation() {
    for context in [
        ModuleContext::default(),
        ModuleContext::new(["fixture.echo".into()], []),
    ] {
        let registry = ModuleRegistry::new();
        let mut native = Native::new("fixture", "fixture.echo", false);
        native
            .manifest
            .permissions
            .insert("fixture.echo".into(), ["read".into()].into());
        let native = Arc::new(native);
        let expected = registry.load_native(native.clone(), context).unwrap();
        let error = registry
            .call_exact(
                &expected,
                "fixture.echo",
                json!({}),
                CancellationToken::new(),
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ModuleError::Denied(_)));
        assert!(!error.outcome_unknown());
        assert_eq!(native.calls.load(Ordering::Relaxed), 0);
    }
    let registry = ModuleRegistry::new();
    let mut native = Native::new("fixture", "fixture.echo", false);
    native
        .manifest
        .permissions
        .insert("fixture.echo".into(), ["read".into()].into());
    let native = Arc::new(native);
    let expected = registry
        .load_native(
            native.clone(),
            ModuleContext::new(["fixture.echo".into()], ["read".into()]),
        )
        .unwrap();
    for arguments in [json!([]), json!({"n":"2"}), json!({"approved":true})] {
        let error = registry
            .call_exact(
                &expected,
                "fixture.echo",
                arguments,
                CancellationToken::new(),
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ModuleError::InvalidArguments(_)));
        assert!(!error.outcome_unknown());
    }
    assert!(matches!(
        registry
            .call_exact(
                &expected,
                "other",
                json!({}),
                CancellationToken::new(),
                Duration::from_secs(1),
            )
            .await,
        Err(ModuleError::Denied(_))
    ));
    assert_eq!(native.calls.load(Ordering::Relaxed), 0);
    assert_eq!(
        registry
            .call_exact(
                &expected,
                "fixture.echo",
                json!({"n": 9}),
                CancellationToken::new(),
                Duration::from_secs(1),
            )
            .await
            .unwrap(),
        json!({"n": 9})
    );
    assert_eq!(native.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn exact_call_keeps_running_effects_unknown_when_unloaded_and_replaced() {
    let registry = ModuleRegistry::new();
    let native = Arc::new(Native::new("fixture", "fixture.echo", true));
    let expected = registry
        .load_native(native.clone(), context("fixture.echo"))
        .unwrap();
    let call = registry.call_exact(
        &expected,
        "fixture.echo",
        json!({}),
        CancellationToken::new(),
        Duration::from_secs(5),
    );
    tokio::pin!(call);
    assert!(futures::poll!(call.as_mut()).is_pending());
    assert_eq!(native.calls.load(Ordering::Relaxed), 1);
    registry.unload("fixture").unwrap();
    let replacement = Arc::new(Native::new("fixture", "fixture.echo", false));
    let current = registry
        .load_native(replacement.clone(), context("fixture.echo"))
        .unwrap();

    let error = call.await.unwrap_err();
    assert_eq!(error, ModuleError::Unloaded);
    assert!(error.outcome_unknown());
    tokio::time::timeout(Duration::from_secs(2), native.cancelled.cancelled())
        .await
        .unwrap();
    assert_eq!(replacement.calls.load(Ordering::Relaxed), 0);
    assert_eq!(registry.describe("fixture").unwrap(), current);
    assert!(matches!(
        registry
            .call_exact(
                &expected,
                "fixture.echo",
                json!({}),
                CancellationToken::new(),
                Duration::from_secs(1),
            )
            .await,
        Err(ModuleError::Unavailable(_))
    ));
    assert_eq!(replacement.calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn exact_call_cancel_timeout_and_drop_preserve_the_cancellation_contract() {
    for stop in ["cancel", "timeout", "drop"] {
        let registry = ModuleRegistry::new();
        let native = Arc::new(Native::new("fixture", "fixture.echo", true));
        let expected = registry
            .load_native(native.clone(), context("fixture.echo"))
            .unwrap();
        let cancel = CancellationToken::new();
        let timeout = if stop == "timeout" {
            Duration::from_millis(20)
        } else {
            Duration::from_secs(5)
        };
        let mut call = Box::pin(registry.call_exact(
            &expected,
            "fixture.echo",
            json!({}),
            cancel.clone(),
            timeout,
        ));
        assert!(futures::poll!(call.as_mut()).is_pending());
        assert_eq!(native.calls.load(Ordering::Relaxed), 1);
        match stop {
            "drop" => drop(call),
            "cancel" => {
                cancel.cancel();
                let error = call.await.unwrap_err();
                assert_eq!(error, ModuleError::Cancelled);
                assert!(error.outcome_unknown());
            }
            "timeout" => {
                let error = call.await.unwrap_err();
                assert_eq!(error, ModuleError::Timeout);
                assert!(error.outcome_unknown());
            }
            _ => unreachable!(),
        }
        tokio::time::timeout(Duration::from_secs(2), native.cancelled.cancelled())
            .await
            .unwrap();
    }
}

struct Reentrant {
    manifest: ModuleManifest,
    registry: Weak<ModuleRegistry>,
    replacement: Arc<Native>,
    calls: AtomicUsize,
    shutdowns: AtomicUsize,
    cancellation_seen_before_reload: Mutex<Option<bool>>,
}

#[async_trait]
impl Module for Reentrant {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }

    async fn call(
        &self,
        _method: &str,
        _arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ModuleError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let registry = self.registry.upgrade().unwrap();
        let expected = registry.describe(&self.manifest.name)?;
        assert_eq!(registry.check_available(&self.manifest.name)?, expected);
        assert_eq!(registry.list(), vec![expected.clone()]);
        assert_eq!(registry.unload(&self.manifest.name)?, expected);
        *self.cancellation_seen_before_reload.lock().unwrap() = Some(cancel.is_cancelled());
        registry.load_native(self.replacement.clone(), context("fixture.echo"))?;
        Ok(json!({"effect_performed": true}))
    }

    fn shutdown(&self) {
        self.shutdowns.fetch_add(1, Ordering::Relaxed);
        assert!(self.registry.upgrade().unwrap().list().is_empty());
    }
}

#[tokio::test]
async fn reentrant_exact_call_checks_the_captured_entry_at_settlement_without_deadlock() {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let registry = Arc::new(ModuleRegistry::new());
            let replacement = Arc::new(Native::new("fixture", "fixture.echo", false));
            let native = Arc::new(Reentrant {
                manifest: manifest("fixture", "fixture.echo"),
                registry: Arc::downgrade(&registry),
                replacement: replacement.clone(),
                calls: AtomicUsize::new(0),
                shutdowns: AtomicUsize::new(0),
                cancellation_seen_before_reload: Mutex::new(None),
            });
            let expected = registry
                .load_native(native.clone(), context("fixture.echo"))
                .unwrap();
            let error = registry
                .call_exact(
                    &expected,
                    "fixture.echo",
                    json!({}),
                    CancellationToken::new(),
                    Duration::from_secs(2),
                )
                .await
                .unwrap_err();
            assert_eq!(error, ModuleError::Unloaded);
            assert!(error.outcome_unknown());
            assert_eq!(native.calls.load(Ordering::Relaxed), 1);
            assert_eq!(native.shutdowns.load(Ordering::Relaxed), 1);
            assert_eq!(
                *native.cancellation_seen_before_reload.lock().unwrap(),
                Some(true)
            );
            assert_eq!(replacement.calls.load(Ordering::Relaxed), 0);
            let current = registry.describe("fixture").unwrap();
            assert_ne!(current.generation, expected.generation);
            assert_eq!(
                registry
                    .call_exact(
                        &current,
                        "fixture.echo",
                        json!({"n": 10}),
                        CancellationToken::new(),
                        Duration::from_secs(1),
                    )
                    .await
                    .unwrap(),
                json!({"n": 10})
            );
            assert_eq!(replacement.calls.load(Ordering::Relaxed), 1);
        });
        let _ = sender.send(());
    });
    // A separate OS thread lets this watchdog detect a synchronous mutex deadlock.
    tokio::time::timeout(Duration::from_secs(5), receiver)
        .await
        .expect("reentrant registry lifecycle deadlocked")
        .unwrap();
    thread.join().unwrap();
}

#[tokio::test]
async fn unload_cancels_running_native_and_rejects_old_generation() {
    let registry = Arc::new(ModuleRegistry::new());
    let native = Arc::new(Native::new("fixture", "fixture.echo", true));
    let old = registry
        .load_native(native.clone(), context("fixture.echo"))
        .unwrap();
    let task = {
        let registry = registry.clone();
        tokio::spawn(async move {
            registry
                .call(
                    "fixture",
                    "fixture.echo",
                    json!({}),
                    CancellationToken::new(),
                    Duration::from_secs(10),
                )
                .await
        })
    };
    native.started.notified().await;
    registry.unload("fixture").unwrap();
    let new = registry
        .load_native(
            Arc::new(Native::new("fixture", "fixture.echo", false)),
            context("fixture.echo"),
        )
        .unwrap();
    assert_ne!(old.generation, new.generation);
    assert_eq!(task.await.unwrap(), Err(ModuleError::Unloaded));
    tokio::time::timeout(Duration::from_secs(2), native.cancelled.cancelled())
        .await
        .unwrap();
    assert_eq!(
        registry
            .call(
                "fixture",
                "fixture.echo",
                json!({}),
                CancellationToken::new(),
                Duration::from_secs(1)
            )
            .await
            .unwrap(),
        json!({})
    );
    registry.unload("fixture").unwrap();
    assert!(matches!(
        registry
            .call(
                "fixture",
                "fixture.echo",
                json!({}),
                CancellationToken::new(),
                Duration::from_secs(1)
            )
            .await,
        Err(ModuleError::NotFound(_))
    ));
}

#[tokio::test]
async fn cancelled_and_dropped_native_calls_propagate_token() {
    for drop_future in [false, true] {
        let registry = Arc::new(ModuleRegistry::new());
        let native = Arc::new(Native::new("fixture", "fixture.echo", true));
        registry
            .load_native(native.clone(), context("fixture.echo"))
            .unwrap();
        let cancel = CancellationToken::new();
        let task = {
            let registry = registry.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                registry
                    .call(
                        "fixture",
                        "fixture.echo",
                        json!({}),
                        cancel,
                        Duration::from_secs(5),
                    )
                    .await
            })
        };
        native.started.notified().await;
        if drop_future {
            task.abort();
            let _ = task.await;
        } else {
            cancel.cancel();
            assert_eq!(task.await.unwrap(), Err(ModuleError::Cancelled));
        }
        tokio::time::timeout(Duration::from_secs(2), native.cancelled.cancelled())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn independent_modules_run_without_global_await_lock() {
    let registry = Arc::new(ModuleRegistry::new());
    let hanging = Arc::new(Native::new("hanging", "wait", true));
    registry
        .load_native(hanging.clone(), context("wait"))
        .unwrap();
    registry
        .load_native(
            Arc::new(Native::new("fast", "echo", false)),
            context("echo"),
        )
        .unwrap();
    let task = {
        let registry = registry.clone();
        tokio::spawn(async move {
            registry
                .call(
                    "hanging",
                    "wait",
                    json!({}),
                    CancellationToken::new(),
                    Duration::from_secs(10),
                )
                .await
        })
    };
    hanging.started.notified().await;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        registry.call(
            "fast",
            "echo",
            json!({}),
            CancellationToken::new(),
            Duration::from_secs(1),
        ),
    )
    .await
    .unwrap();
    assert!(result.is_ok());
    registry.unload("hanging").unwrap();
    assert_eq!(task.await.unwrap(), Err(ModuleError::Unloaded));
}
