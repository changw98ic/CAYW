#![cfg(feature = "strongflow")]
use cayw_modules::{strongflow::*, *};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

fn plan() -> WorkflowPlan {
    WorkflowPlan {
        id: "build".into(),
        steps: vec![
            PlanStep {
                id: "compile".into(),
                depends_on: Default::default(),
                acceptance_checks: ["macos".into(), "linux".into()].into(),
            },
            PlanStep {
                id: "ship".into(),
                depends_on: ["compile".into()].into(),
                acceptance_checks: ["artifact".into()].into(),
            },
        ],
    }
}
fn evidence(check: &str, passed: bool) -> ExecutionEvidence {
    ExecutionEvidence {
        source_hash: "a".repeat(64),
        build_id: "local-fixture-build".into(),
        configuration: json!({"features":[]}),
        platform: check.into(),
        input: json!({"test":"offline-fixture"}),
        command: vec!["cargo".into(), "test".into()],
        actual_result: json!({"exit_code":if passed { 0 } else { 1 }}),
        checks: [check.into()].into(),
        passed,
    }
}
#[test]
fn dependency_cycles_missing_dependencies_and_checks_are_rejected() {
    let mut invalid = plan();
    invalid.steps[0].depends_on.insert("ship".into());
    assert!(invalid.validate().is_err());
    let mut invalid = plan();
    invalid.steps[0].depends_on.insert("unknown".into());
    assert!(invalid.validate().is_err());
    let mut invalid = plan();
    invalid.steps[0].acceptance_checks.clear();
    assert!(invalid.validate().is_err());
    let mut invalid = plan();
    invalid.steps[1].id = "compile".into();
    assert!(invalid.validate().is_err());
    assert!(plan().validate().is_ok());
}
#[test]
fn host_execution_evidence_advances_only_ready_steps() {
    let flow = StrongFlow::new();
    flow.create_plan(plan()).unwrap();
    assert_eq!(flow.ready("build").unwrap(), vec!["compile"]);
    assert!(flow.start_step("build", "ship").is_err());
    assert!(
        flow.submit_evidence("build", "compile", evidence("macos", true))
            .is_err()
    );
    flow.start_step("build", "compile").unwrap();
    let mut incomplete = evidence("macos", true);
    incomplete.source_hash.clear();
    assert!(
        flow.submit_evidence("build", "compile", incomplete)
            .is_err()
    );
    assert_eq!(
        flow.submit_evidence("build", "compile", evidence("macos", true))
            .unwrap(),
        StepStatus::Running
    );
    assert!(flow.ready("build").unwrap().is_empty());
    let mut changed_build = evidence("linux", true);
    changed_build.build_id = "other".into();
    assert!(
        flow.submit_evidence("build", "compile", changed_build)
            .is_err()
    );
    assert_eq!(
        flow.submit_evidence("build", "compile", evidence("linux", true))
            .unwrap(),
        StepStatus::Passed
    );
    assert_eq!(flow.ready("build").unwrap(), vec!["ship"]);
    flow.start_step("build", "ship").unwrap();
    assert_eq!(
        flow.submit_evidence("build", "ship", evidence("artifact", false))
            .unwrap(),
        StepStatus::Failed
    );
    assert!(
        flow.submit_evidence("build", "ship", evidence("artifact", true))
            .is_err()
    );
    let snapshot = flow.describe("build").unwrap();
    assert_eq!(snapshot.evidence["compile"].len(), 2);
    assert_eq!(
        serde_json::from_value::<WorkflowSnapshot>(serde_json::to_value(&snapshot).unwrap())
            .unwrap(),
        snapshot
    );
}
#[tokio::test]
async fn module_calls_cannot_submit_model_claims_as_acceptance() {
    let registry = ModuleRegistry::new();
    let flow = Arc::new(StrongFlow::new());
    flow.create_plan(plan()).unwrap();
    registry
        .load_native(
            flow,
            ModuleContext::new(
                [
                    "strongflow.ready".into(),
                    "strongflow.describe".into(),
                    "strongflow.accept".into(),
                ],
                [],
            ),
        )
        .unwrap();
    assert_eq!(
        registry
            .call(
                "strongflow",
                "strongflow.ready",
                json!({"workflow_id":"build"}),
                CancellationToken::new(),
                Duration::from_secs(1)
            )
            .await
            .unwrap(),
        json!(["compile"])
    );
    assert!(matches!(
        registry
            .call(
                "strongflow",
                "strongflow.accept",
                json!({"model_says_passed":true}),
                CancellationToken::new(),
                Duration::from_secs(1)
            )
            .await,
        Err(ModuleError::Denied(_))
    ));
}
#[test]
fn session_unload_preserves_binding_and_only_confirmed_fork_changes_mode() {
    let registry = ModuleRegistry::new();
    let description = registry
        .load_native(Arc::new(StrongFlow::new()), ModuleContext::default())
        .unwrap();
    let mut binding = SessionBinding::strongflow("original".into(), &description).unwrap();
    binding.check_available(&registry).unwrap();
    registry.unload("strongflow").unwrap();
    assert!(binding.check_available(&registry).is_err());
    assert_eq!(binding.mode(), SessionMode::StrongFlow);
    assert_eq!(
        binding.bound_module().unwrap().version,
        description.manifest.version
    );
    assert!(binding.frozen_reason().is_some());
    registry
        .load_native(Arc::new(StrongFlow::new()), ModuleContext::default())
        .unwrap();
    assert!(binding.check_available(&registry).is_err());
    assert!(
        binding
            .request_ordinary_fork("forked".into(), false)
            .is_err()
    );
    assert!(
        binding
            .request_ordinary_fork("original".into(), true)
            .is_err()
    );
    let old = binding.clone();
    let decision = binding
        .request_ordinary_fork("forked".into(), true)
        .unwrap();
    assert_eq!(binding, old);
    assert_eq!(decision.target.mode(), SessionMode::Ordinary);
    assert_eq!(decision.target.source_session_id(), Some("original"));
    assert!(decision.target.bound_module().is_none());
    assert!(decision.user_confirmed);
    assert_eq!(
        serde_json::from_value::<SessionBinding>(serde_json::to_value(&binding).unwrap()).unwrap(),
        binding
    );
}
#[test]
fn fresh_registry_cannot_silently_resume_an_old_binding_with_same_generation() {
    let old_registry = ModuleRegistry::new();
    let old = old_registry
        .load_native(Arc::new(StrongFlow::new()), ModuleContext::default())
        .unwrap();
    let mut binding = SessionBinding::strongflow("session".into(), &old).unwrap();
    let fresh_registry = ModuleRegistry::new();
    let fresh = fresh_registry
        .load_native(Arc::new(StrongFlow::new()), ModuleContext::default())
        .unwrap();
    assert_eq!(old.generation, fresh.generation);
    assert_ne!(old.registry_id, fresh.registry_id);
    assert!(binding.check_available(&fresh_registry).is_err());
    assert_eq!(binding.mode(), SessionMode::StrongFlow);
    assert!(binding.frozen_reason().is_some());
}

#[test]
fn explicit_compatible_reload_keeps_plan_evidence_and_original_fork_source() {
    let registry = ModuleRegistry::new();
    let flow = Arc::new(StrongFlow::new());
    flow.create_plan(plan()).unwrap();
    flow.start_step("build", "compile").unwrap();
    flow.submit_evidence("build", "compile", evidence("macos", true))
        .unwrap();
    let original = registry
        .load_native(flow.clone(), ModuleContext::default())
        .unwrap();
    let mut binding = SessionBinding::strongflow("resume-source".into(), &original).unwrap();
    let saved = binding.checkpoint_strongflow(&registry).unwrap();
    registry.unload("strongflow").unwrap();
    assert!(binding.check_available(&registry).is_err());
    let frozen = binding.clone();
    let fork = binding
        .request_ordinary_fork("ordinary-copy".into(), true)
        .unwrap();
    assert_eq!(binding, frozen);
    assert_eq!(fork.source, frozen);
    assert_eq!(fork.target.mode(), SessionMode::Ordinary);
    let reloaded = registry
        .load_native(flow.clone(), ModuleContext::default())
        .unwrap();
    assert!(
        binding.check_available(&registry).is_err(),
        "reload must not automatically thaw"
    );
    binding.resume_strongflow(&registry, &reloaded).unwrap();
    binding.check_available(&registry).unwrap();
    assert_eq!(binding.mode(), SessionMode::StrongFlow);
    assert_eq!(
        binding.bound_module().unwrap().generation,
        reloaded.generation
    );
    assert_eq!(binding.workflow_state(), Some(&saved));
    assert_eq!(flow.describe("build").unwrap().evidence["compile"].len(), 1);
    assert_eq!(
        flow.describe("build").unwrap().statuses["compile"],
        StepStatus::Running
    );
    assert!(flow.ready("build").unwrap().is_empty());
    flow.submit_evidence("build", "compile", evidence("linux", true))
        .unwrap();
    assert_eq!(flow.ready("build").unwrap(), ["ship"]);
    assert!(fork.source.frozen_reason().is_some());
}

#[test]
fn same_version_empty_changed_plan_and_lost_evidence_cannot_resume() {
    for replacement in [
        "empty",
        "weaker_plan",
        "lost_evidence",
        "rewritten_evidence",
    ] {
        let registry = ModuleRegistry::new();
        let flow = Arc::new(StrongFlow::new());
        flow.create_plan(plan()).unwrap();
        flow.start_step("build", "compile").unwrap();
        flow.submit_evidence("build", "compile", evidence("macos", true))
            .unwrap();
        let loaded = registry
            .load_native(flow, ModuleContext::default())
            .unwrap();
        let mut binding = SessionBinding::strongflow("preserve".into(), &loaded).unwrap();
        binding.checkpoint_strongflow(&registry).unwrap();
        registry.unload("strongflow").unwrap();
        assert!(binding.check_available(&registry).is_err());
        let frozen = binding.clone();
        let other = Arc::new(StrongFlow::new());
        if replacement != "empty" {
            let mut new_plan = plan();
            if replacement == "weaker_plan" {
                new_plan.steps.remove(1);
            }
            other.create_plan(new_plan).unwrap();
            if replacement == "rewritten_evidence" {
                other.start_step("build", "compile").unwrap();
                let mut changed = evidence("macos", true);
                changed.actual_result = json!({"exit_code":0,"different_run":true});
                other.submit_evidence("build", "compile", changed).unwrap();
            }
        }
        let replacement = registry
            .load_native(other, ModuleContext::default())
            .unwrap();
        assert_eq!(replacement.manifest.version, loaded.manifest.version);
        assert!(binding.resume_strongflow(&registry, &replacement).is_err());
        assert_eq!(
            binding, frozen,
            "failed recovery must retain all original obligations"
        );
    }
}

#[test]
fn durable_state_restores_running_step_without_replaying_it_and_explicitly_resumes_new_registry() {
    let old_registry = ModuleRegistry::new();
    let flow = Arc::new(StrongFlow::new());
    flow.create_plan(plan()).unwrap();
    flow.start_step("build", "compile").unwrap();
    flow.submit_evidence("build", "compile", evidence("macos", true))
        .unwrap();
    let loaded = old_registry
        .load_native(flow, ModuleContext::default())
        .unwrap();
    let mut binding = SessionBinding::strongflow("restart".into(), &loaded).unwrap();
    let state = binding.checkpoint_strongflow(&old_registry).unwrap();
    let state_json = serde_json::to_vec(&state).unwrap();
    let binding_json = serde_json::to_vec(&binding).unwrap();
    drop(old_registry);
    let state: StrongFlowState = serde_json::from_slice(&state_json).unwrap();
    let restored = Arc::new(StrongFlow::restore(state).unwrap());
    assert!(restored.ready("build").unwrap().is_empty());
    assert!(restored.start_step("build", "compile").is_err());
    assert_eq!(
        restored.describe("build").unwrap().statuses["compile"],
        StepStatus::Running
    );
    let registry = ModuleRegistry::new();
    let loaded = registry
        .load_native(restored.clone(), ModuleContext::default())
        .unwrap();
    let mut binding: SessionBinding = serde_json::from_slice(&binding_json).unwrap();
    assert!(binding.check_available(&registry).is_err());
    binding.resume_strongflow(&registry, &loaded).unwrap();
    restored
        .submit_evidence("build", "compile", evidence("linux", true))
        .unwrap();
    assert_eq!(restored.ready("build").unwrap(), ["ship"]);
    let checkpoint = binding.checkpoint_strongflow(&registry).unwrap();
    assert_eq!(checkpoint.workflows["build"].evidence["compile"].len(), 2);
    assert_eq!(
        checkpoint.workflows["build"].statuses["compile"],
        StepStatus::Passed
    );
}

#[test]
fn recovery_rejects_stale_or_foreign_descriptions_and_manifest_changes() {
    let registry = ModuleRegistry::new();
    let flow = Arc::new(StrongFlow::new());
    flow.create_plan(plan()).unwrap();
    let loaded = registry
        .load_native(flow.clone(), ModuleContext::default())
        .unwrap();
    let mut binding = SessionBinding::strongflow("pinned".into(), &loaded).unwrap();
    binding.checkpoint_strongflow(&registry).unwrap();
    assert!(
        binding.resume_strongflow(&registry, &loaded).is_err(),
        "live binding is not a recovery request"
    );
    registry.unload("strongflow").unwrap();
    assert!(binding.check_available(&registry).is_err());
    let reloaded = registry
        .load_native(flow, ModuleContext::default())
        .unwrap();
    let frozen = binding.clone();
    let mut foreign = reloaded.clone();
    foreign.registry_id = loaded.registry_id.clone();
    foreign.generation = loaded.generation;
    assert!(binding.resume_strongflow(&registry, &foreign).is_err());
    let mut changed = reloaded.clone();
    changed.manifest.capabilities[0].description = "changed declaration".into();
    assert!(binding.resume_strongflow(&registry, &changed).is_err());
    let other_registry = ModuleRegistry::new();
    let other = other_registry
        .load_native(Arc::new(StrongFlow::new()), ModuleContext::default())
        .unwrap();
    assert!(binding.resume_strongflow(&registry, &other).is_err());
    assert_eq!(binding, frozen);
    binding.resume_strongflow(&registry, &reloaded).unwrap();
}

#[test]
fn uncheckpointed_binding_cannot_recover_by_discarding_original_workflows() {
    let registry = ModuleRegistry::new();
    let flow = Arc::new(StrongFlow::new());
    flow.create_plan(plan()).unwrap();
    let original = registry
        .load_native(flow.clone(), ModuleContext::default())
        .unwrap();
    let mut binding = SessionBinding::strongflow("no-checkpoint".into(), &original).unwrap();
    registry.unload("strongflow").unwrap();
    assert!(binding.check_available(&registry).is_err());
    let frozen = binding.clone();
    let reloaded = registry
        .load_native(flow, ModuleContext::default())
        .unwrap();
    assert!(binding.checkpoint_strongflow(&registry).is_err());
    assert!(binding.resume_strongflow(&registry, &reloaded).is_err());
    assert_eq!(binding, frozen);
}

#[test]
fn checkpoint_accepts_only_monotonic_progress_and_keeps_terminal_failures_on_restore() {
    let registry = ModuleRegistry::new();
    let flow = Arc::new(StrongFlow::new());
    flow.create_plan(plan()).unwrap();
    let loaded = registry
        .load_native(flow.clone(), ModuleContext::default())
        .unwrap();
    let mut binding = SessionBinding::strongflow("progress".into(), &loaded).unwrap();
    binding.checkpoint_strongflow(&registry).unwrap();
    flow.start_step("build", "compile").unwrap();
    binding.checkpoint_strongflow(&registry).unwrap();
    flow.submit_evidence("build", "compile", evidence("macos", true))
        .unwrap();
    binding.checkpoint_strongflow(&registry).unwrap();
    flow.submit_evidence("build", "compile", evidence("linux", true))
        .unwrap();
    binding.checkpoint_strongflow(&registry).unwrap();
    flow.start_step("build", "ship").unwrap();
    flow.submit_evidence("build", "ship", evidence("artifact", false))
        .unwrap();
    let terminal = binding.checkpoint_strongflow(&registry).unwrap();
    let restored = StrongFlow::restore(terminal).unwrap();
    assert!(restored.ready("build").unwrap().is_empty());
    assert_eq!(
        restored.describe("build").unwrap().statuses["ship"],
        StepStatus::Failed
    );
    assert!(restored.start_step("build", "ship").is_err());
    assert!(
        restored
            .submit_evidence("build", "ship", evidence("artifact", true))
            .is_err()
    );
}

#[test]
fn restore_rejects_inconsistent_versions_dependencies_statuses_and_evidence() {
    let flow = StrongFlow::new();
    flow.create_plan(plan()).unwrap();
    flow.start_step("build", "compile").unwrap();
    flow.submit_evidence("build", "compile", evidence("macos", true))
        .unwrap();
    let valid = serde_json::to_value(flow.export_state()).unwrap();
    let cases = [
        ("format_version", json!(999)),
        ("module_manifest/version", json!("other")),
        ("module_manifest/protocol_version", json!(999)),
        (
            "module_manifest/capabilities/0/description",
            json!("different"),
        ),
        ("workflows/build/plan/id", json!("other")),
        ("workflows/build/plan/steps/0/depends_on", json!(["ship"])),
        ("workflows/build/plan/steps/0/acceptance_checks", json!([])),
        ("workflows/build/statuses", json!({"compile":"running"})),
        ("workflows/build/statuses/ship", json!("running")),
        ("workflows/build/statuses/compile", json!("passed")),
        (
            "workflows/build/evidence/compile/0/source_hash",
            json!("missing"),
        ),
        (
            "workflows/build/evidence/compile/0/checks",
            json!(["undeclared"]),
        ),
        ("workflows/build/evidence/compile/0/passed", json!(false)),
    ];
    for (pointer, value) in cases {
        let mut invalid = valid.clone();
        *invalid.pointer_mut(&format!("/{pointer}")).unwrap() = value;
        let state: StrongFlowState = serde_json::from_value(invalid).unwrap();
        assert!(StrongFlow::restore(state).is_err(), "must reject {pointer}");
    }
    let mut mixed = flow.export_state();
    let mut next = evidence("linux", true);
    next.build_id = "different-build".into();
    mixed
        .workflows
        .get_mut("build")
        .unwrap()
        .evidence
        .get_mut("compile")
        .unwrap()
        .push(next);
    mixed
        .workflows
        .get_mut("build")
        .unwrap()
        .statuses
        .insert("compile".into(), StepStatus::Passed);
    assert!(StrongFlow::restore(mixed).is_err());
    let mut extra = flow.export_state();
    extra
        .workflows
        .get_mut("build")
        .unwrap()
        .evidence
        .insert("unknown".into(), vec![evidence("macos", true)]);
    assert!(StrongFlow::restore(extra).is_err());
    let mut terminal_records = flow.export_state();
    let workflow = terminal_records.workflows.get_mut("build").unwrap();
    workflow
        .evidence
        .get_mut("compile")
        .unwrap()
        .extend([evidence("linux", true), evidence("macos", true)]);
    workflow
        .statuses
        .insert("compile".into(), StepStatus::Passed);
    assert!(
        StrongFlow::restore(terminal_records).is_err(),
        "evidence after terminal pass must be rejected"
    );
    let mut empty_records = flow.export_state();
    empty_records
        .workflows
        .get_mut("build")
        .unwrap()
        .evidence
        .insert("compile".into(), vec![]);
    assert!(StrongFlow::restore(empty_records).is_err());
    let mut unknown_fields = valid.clone();
    unknown_fields["credentials_file"] = json!("ignored-by-test");
    assert!(serde_json::from_value::<StrongFlowState>(unknown_fields).is_err());
    assert!(StrongFlow::restore(serde_json::from_value(valid).unwrap()).is_ok());
}

#[test]
fn restored_binding_must_be_validated_and_unavailable_version_freezes() {
    let registry = ModuleRegistry::new();
    let description = registry
        .load_native(Arc::new(StrongFlow::new()), ModuleContext::default())
        .unwrap();
    let binding = SessionBinding::strongflow("session".into(), &description).unwrap();
    let mut value = serde_json::to_value(binding).unwrap();
    value["bound_module_version"]["generation"] = json!(100);
    let mut restored: SessionBinding = serde_json::from_value(value.clone()).unwrap();
    assert!(restored.check_available(&registry).is_err());
    value["mode"] = json!("ordinary");
    let mut invalid: SessionBinding = serde_json::from_value(value).unwrap();
    assert!(invalid.check_available(&registry).is_err());
}
