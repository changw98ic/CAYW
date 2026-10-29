//! Dedicated review regression tests for RT-02 and RT-06.
//!
//! RT-02: Incremental Core event cursor hydration so peer runtimes refresh
//! newly recorded feedback across connections without restart.
//! RT-06: Check prompt byte budgets before decoding or re-encoding base64 images
//! to prevent unbounded allocations on oversized inputs.

use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
};
use cayw_ipc::RpcHandler;
use cayw_plannotator::{FEEDBACK_SECTION_KEY, FeedbackDecision};
use cayw_runtime::{
    ImageInput, PlanFeedbackInput, Runtime, RuntimeConfig, RuntimeError, ScopedPermission,
    TaskInput, TaskState,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

const PLAN_SCRIPT: &str = r#"return cayw.call('plan.update',{expected_version=0,steps={{id='inspect',text='Inspect workspace',status='in_progress'},{id='edit',text='Apply edit',status='pending'}}})"#;

struct Script(&'static str);
impl Provider for Script {
    fn stream(&self, r: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &r,
            self.0,
            &format!("tool-{}", r.messages.len()),
        );
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message,
        }]))
    }
}

struct Capture {
    seen: Mutex<Vec<Vec<Message>>>,
    script: &'static str,
}
impl Capture {
    fn new(script: &'static str) -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
            script,
        }
    }
}
impl Provider for Capture {
    fn stream(&self, r: ModelRequest, _: CancellationToken) -> AssistantStream {
        self.seen.lock().unwrap().push(r.messages.clone());
        let message = cayw_runtime::fixture::script_response(
            &r,
            self.script,
            &format!("tool-{}", r.messages.len()),
        );
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message,
        }]))
    }
}

fn plan_scope() -> serde_json::Value {
    json!({
        "expected_version": 0,
        "steps": [
            {"id":"inspect","text":"Inspect workspace","status":"in_progress"},
            {"id":"edit","text":"Apply edit","status":"pending"}
        ]
    })
}

#[tokio::test]
async fn rt_02_peer_runtime_incremental_hydration_and_prompt_injection() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();

    let rt1 = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(Script(PLAN_SCRIPT)),
        Model::new("fixture", "plannotator"),
        RuntimeConfig {
            permissions: vec![ScopedPermission::exact("plan.update", plan_scope())],
            ..Default::default()
        },
    )
    .unwrap();
    let id = rt1.create(None).unwrap().id;

    // Run initial prompt to create plan v1.
    rt1.prompt(&id, "create plan", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(rt1.tasks(&id).unwrap()[0].state, TaskState::Completed);
    let plan = rt1.plan(&id).unwrap().expect("plan v1");

    // Peer Runtime rt2 opens concurrently against the same sqlite database.
    let second_capture = Arc::new(Capture::new("return 1"));
    let rt2 = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        second_capture.clone(),
        Model::new("fixture", "plannotator"),
        RuntimeConfig::default(),
    )
    .unwrap();

    // rt2 queries feedback: initially empty.
    let initial_fb = rt2.plannotator_feedback(&id).unwrap();
    assert!(initial_fb.is_empty(), "rt2 sees empty initial feedback");

    // rt1 submits feedback on plan v1.
    let submitted = rt1
        .submit_plan_feedback(PlanFeedbackInput {
            session_id: id.clone(),
            expected_task_id: plan.task_id.clone(),
            expected_version: plan.version,
            expected_content_sha256: plan.content_sha256.clone(),
            decision: FeedbackDecision::DenyWithFeedback,
            comments: vec![cayw_plannotator::AnnotationComment {
                comment_id: "c-rt1".into(),
                body: "incremental hydration test comment".into(),
                anchor: None,
                created_at_ms: 1,
            }],
            summary: Some("rt1-incremental-review".into()),
            actor: "reviewer-1".into(),
        })
        .unwrap();
    assert_eq!(submitted.decision, FeedbackDecision::DenyWithFeedback);

    // rt2 without restarting MUST query and see the newly recorded feedback.
    let rt2_fb = rt2.plannotator_feedback(&id).unwrap();
    assert_eq!(rt2_fb.len(), 1, "rt2 must see feedback without restart");
    assert_eq!(rt2_fb[0].decision, FeedbackDecision::DenyWithFeedback);
    assert_eq!(rt2_fb[0].summary.as_deref(), Some("rt1-incremental-review"));

    // rt2 now prompts the next attempt; the feedback must be injected into the system prompt.
    rt2.prompt(&id, "attempt 2", CancellationToken::new())
        .await
        .unwrap();

    let seen = second_capture.seen.lock().unwrap().clone();
    assert!(!seen.is_empty(), "rt2 provider saw attempt 2 messages");
    let has_feedback = seen.iter().any(|messages| {
        messages.iter().any(|msg| match msg {
            Message::System(system) => system
                .sections
                .get(FEEDBACK_SECTION_KEY)
                .and_then(|v| v.as_ref())
                .is_some_and(|body| {
                    body.contains("deny_with_feedback")
                        && body.contains("incremental hydration test comment")
                }),
            _ => false,
        })
    });
    assert!(
        has_feedback,
        "rt2 attempt 2 must inject feedback into {FEEDBACK_SECTION_KEY}"
    );
}

#[tokio::test]
async fn rt_02_interleaved_submissions_across_runtimes_retains_all_records() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();

    let rt1 = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(Script(PLAN_SCRIPT)),
        Model::new("fixture", "plannotator"),
        RuntimeConfig {
            permissions: vec![ScopedPermission::exact("plan.update", plan_scope())],
            ..Default::default()
        },
    )
    .unwrap();
    let id = rt1.create(None).unwrap().id;
    rt1.prompt(&id, "setup plan", CancellationToken::new())
        .await
        .unwrap();
    let plan = rt1.plan(&id).unwrap().expect("plan");

    let rt2 = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(Script(PLAN_SCRIPT)),
        Model::new("fixture", "plannotator"),
        RuntimeConfig::default(),
    )
    .unwrap();

    // Initial query on both runtimes.
    assert!(rt1.plannotator_feedback(&id).unwrap().is_empty());
    assert!(rt2.plannotator_feedback(&id).unwrap().is_empty());

    // Submit from rt1.
    rt1.submit_plan_feedback(PlanFeedbackInput {
        session_id: id.clone(),
        expected_task_id: plan.task_id.clone(),
        expected_version: plan.version,
        expected_content_sha256: plan.content_sha256.clone(),
        decision: FeedbackDecision::ApproveWithNotes,
        comments: vec![],
        summary: Some("turn-1-rt1".into()),
        actor: "actor-1".into(),
    })
    .unwrap();

    // Submit from rt2 without restarting.
    rt2.submit_plan_feedback(PlanFeedbackInput {
        session_id: id.clone(),
        expected_task_id: plan.task_id.clone(),
        expected_version: plan.version,
        expected_content_sha256: plan.content_sha256.clone(),
        decision: FeedbackDecision::DenyWithFeedback,
        comments: vec![],
        summary: Some("turn-2-rt2".into()),
        actor: "actor-2".into(),
    })
    .unwrap();

    // Both runtimes see both feedback items.
    let fb1 = rt1.plannotator_feedback(&id).unwrap();
    assert_eq!(fb1.len(), 2);
    assert_eq!(fb1[0].summary.as_deref(), Some("turn-1-rt1"));
    assert_eq!(fb1[1].summary.as_deref(), Some("turn-2-rt2"));

    let fb2 = rt2.plannotator_feedback(&id).unwrap();
    assert_eq!(fb2.len(), 2);
    assert_eq!(fb2[0].summary.as_deref(), Some("turn-1-rt1"));
    assert_eq!(fb2[1].summary.as_deref(), Some("turn-2-rt2"));
}

#[test]
fn rt_06_prompt_budget_checked_before_image_decoding_or_mime_validation() {
    let mut model = Model::new("fixture", "typed-image-fixture");
    model.supports_images = true;

    // Oversized invalid data with invalid MIME.
    let malformed_image = ImageInput {
        mime_type: "application/not-a-supported-image".into(),
        data: "!@#$%^&*()_+~`{}|[]\\:\";'<>?,./".repeat(100),
    };
    let input = TaskInput {
        text: "test prompt".into(),
        images: vec![malformed_image.clone()],
    };
    let total_bytes =
        "test prompt".len() + malformed_image.mime_type.len() + malformed_image.data.len();

    // Budget lower than total: budget error MUST be returned first.
    let err_budget = input.validate(&model, total_bytes - 1).unwrap_err();
    assert!(
        matches!(err_budget, RuntimeError::InvalidInput(ref msg) if msg.contains("exceeds the byte budget")),
        "expected byte budget error, got: {err_budget}"
    );

    // Budget sufficient: validation proceeds to MIME type check.
    let err_mime = input.validate(&model, total_bytes).unwrap_err();
    assert!(
        matches!(err_mime, RuntimeError::InvalidInput(ref msg) if msg.contains("MIME type is not supported")),
        "expected MIME error once within budget, got: {err_mime}"
    );

    // Valid MIME but invalid base64:
    let malformed_b64 = ImageInput {
        mime_type: "image/png".into(),
        data: "not-base-64-content!!!!".into(),
    };
    let input_b64 = TaskInput {
        text: "test prompt".into(),
        images: vec![malformed_b64.clone()],
    };
    let total_b64 = "test prompt".len() + malformed_b64.mime_type.len() + malformed_b64.data.len();

    let err_budget_b64 = input_b64.validate(&model, total_b64 - 1).unwrap_err();
    assert!(
        matches!(err_budget_b64, RuntimeError::InvalidInput(ref msg) if msg.contains("exceeds the byte budget")),
        "expected byte budget error before decoding, got: {err_budget_b64}"
    );

    let err_decode = input_b64.validate(&model, total_b64).unwrap_err();
    assert!(
        matches!(err_decode, RuntimeError::InvalidInput(ref msg) if msg.contains("not valid base64")),
        "expected base64 error once within budget, got: {err_decode}"
    );
}

#[tokio::test]
async fn rt_06_task_enqueue_rejects_oversized_before_base64_decode() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let mut model = Model::new("fixture", "typed-image-fixture");
    model.supports_images = true;
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(Script("return 1")),
        model,
        RuntimeConfig::default(),
    )
    .unwrap();

    let session = rt.create(None).unwrap().id;

    // 500KB of non-base64 characters (> default 256KB max_prompt_bytes).
    let oversized_invalid_data = "!@#$%^&*()".repeat(50_000);
    let error = RpcHandler::call(
        &rt,
        "task.enqueue",
        json!({
            "session_id": session,
            "text": "oversized input test",
            "images": [{
                "mime_type": "image/png",
                "data": oversized_invalid_data
            }]
        }),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert_eq!(error.code, "invalid_params");
    assert!(
        error.message.contains("byte budget"),
        "expected byte budget error, got: {}",
        error.message
    );
    assert!(rt.tasks(&session).unwrap().is_empty());
}

#[tokio::test]
async fn rt_02_peer_runtime_incremental_hydration_for_file_target_and_prompt_injection() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();

    let rt1 = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(Script("return 1")),
        Model::new("fixture", "plannotator"),
        RuntimeConfig::default(),
    )
    .unwrap();
    let id = rt1.create(None).unwrap().id;

    // Peer Runtime rt2 opens concurrently against the same sqlite database.
    let second_capture = Arc::new(Capture::new("return 1"));
    let rt2 = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        second_capture.clone(),
        Model::new("fixture", "plannotator"),
        RuntimeConfig::default(),
    )
    .unwrap();

    // rt2 queries feedback: initially empty.
    let initial_fb = rt2.plannotator_feedback(&id).unwrap();
    assert!(initial_fb.is_empty(), "rt2 sees empty initial feedback");

    // rt1 submits feedback on a non-plan file target (e.g. "src/main.rs").
    let file_path = "src/main.rs";
    let file_sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let draft = cayw_plannotator::FeedbackDraft {
        session_id: id.clone(),
        target: cayw_plannotator::AnnotationTarget {
            target_id: "target-file-main".into(),
            kind: cayw_plannotator::TargetKind::Markdown,
            file: cayw_plannotator::FileVersionBinding {
                path: file_path.into(),
                version: 1,
                content_sha256: file_sha.into(),
            },
            label: Some("main.rs review".into()),
        },
        decision: FeedbackDecision::DenyWithFeedback,
        comments: vec![cayw_plannotator::AnnotationComment {
            comment_id: "c-file-1".into(),
            body: "syntax error on line 10".into(),
            anchor: None,
            created_at_ms: 10,
        }],
        summary: Some("file-review-summary".into()),
        created_at_ms: 10,
        current_attempt: 0,
    };
    let submitted = rt1
        .submit_plannotator_feedback(draft, "reviewer-code")
        .unwrap();
    assert_eq!(submitted.decision, FeedbackDecision::DenyWithFeedback);

    // rt2 without restart queries plannotator_feedback: must see the newly recorded file feedback.
    let rt2_fb = rt2.plannotator_feedback(&id).unwrap();
    assert_eq!(
        rt2_fb.len(),
        1,
        "rt2 must see file feedback without restart"
    );
    assert_eq!(rt2_fb[0].decision, FeedbackDecision::DenyWithFeedback);
    assert_eq!(rt2_fb[0].target.file.path, file_path);
    assert_eq!(rt2_fb[0].summary.as_deref(), Some("file-review-summary"));

    // rt2 sets live file version matching the target file.
    let mut live_files = std::collections::BTreeMap::new();
    live_files.insert(
        file_path.into(),
        cayw_plannotator::LiveFileVersion {
            version: 1,
            content_sha256: file_sha.into(),
        },
    );
    rt2.set_plannotator_live_files(&id, live_files).unwrap();

    // rt2 prompts the next attempt; the file feedback must be injected into the system prompt.
    rt2.prompt(&id, "attempt 1", CancellationToken::new())
        .await
        .unwrap();

    let seen = second_capture.seen.lock().unwrap().clone();
    assert!(!seen.is_empty(), "rt2 provider saw attempt messages");
    let has_feedback = seen.iter().any(|messages| {
        messages.iter().any(|msg| match msg {
            Message::System(system) => system
                .sections
                .get(FEEDBACK_SECTION_KEY)
                .and_then(|v| v.as_ref())
                .is_some_and(|body| {
                    body.contains("deny_with_feedback")
                        && body.contains("syntax error on line 10")
                        && body.contains("src/main.rs")
                }),
            _ => false,
        })
    });
    assert!(
        has_feedback,
        "rt2 attempt 1 must inject file feedback into {FEEDBACK_SECTION_KEY}"
    );
}

#[test]
fn rt_06_multi_image_cumulative_budget_boundary_and_rejection_before_decode() {
    let mut model = Model::new("fixture", "typed-image-fixture");
    model.supports_images = true;

    // Both images individually fit within a moderate budget (e.g. 60 bytes),
    // but together with text they exceed it. Both images contain invalid base64 data.
    let img1 = ImageInput {
        mime_type: "image/png".into(),                           // 9 bytes
        data: "!@#$%^&*()_+~`{}|[]\\:\";'<>?,./12345678".into(), // 40 bytes. Total img1 = 49 bytes.
    };
    let img2 = ImageInput {
        mime_type: "image/jpeg".into(),                          // 10 bytes
        data: "!@#$%^&*()_+~`{}|[]\\:\";'<>?,./87654321".into(), // 40 bytes. Total img2 = 50 bytes.
    };
    let text = "multi-img"; // 9 bytes
    let total_bytes = text.len()
        + (img1.mime_type.len() + img1.data.len())
        + (img2.mime_type.len() + img2.data.len());
    // total_bytes = 9 + 49 + 50 = 108 bytes.

    let input = TaskInput {
        text: text.into(),
        images: vec![img1, img2],
    };

    // Single image 1 + text = 58 bytes <= 60 bytes.
    // Single image 2 + text = 59 bytes <= 60 bytes.
    // Budget 60 would fit either image alone, but cumulative exceeds 60 on image 2.
    let err_individual_fit = input.validate(&model, 60).unwrap_err();
    assert!(
        matches!(err_individual_fit, RuntimeError::InvalidInput(ref msg) if msg.contains("exceeds the byte budget")),
        "cumulative budget must reject before decoding when second image pushes over budget: {err_individual_fit}"
    );

    // Exact boundary: 107 bytes (total_bytes - 1) -> byte budget error before decoding.
    let err_boundary_under = input.validate(&model, total_bytes - 1).unwrap_err();
    assert!(
        matches!(err_boundary_under, RuntimeError::InvalidInput(ref msg) if msg.contains("exceeds the byte budget")),
        "expected byte budget error at total - 1, got: {err_boundary_under}"
    );

    // Exact boundary: 108 bytes (total_bytes) -> passes budget check, then fails on base64 decoding.
    let err_boundary_exact = input.validate(&model, total_bytes).unwrap_err();
    assert!(
        matches!(err_boundary_exact, RuntimeError::InvalidInput(ref msg) if msg.contains("not valid base64")),
        "expected base64 error once exact budget is met, got: {err_boundary_exact}"
    );

    // Valid canonical images:
    let valid_img1 = ImageInput {
        mime_type: "image/png".into(),
        data: "aGVsbG8=".into(), // 8 bytes. Total = 9 + 8 = 17 bytes.
    };
    let valid_img2 = ImageInput {
        mime_type: "image/jpeg".into(),
        data: "d29ybGQ=".into(), // 8 bytes. Total = 10 + 8 = 18 bytes.
    };
    let valid_input = TaskInput {
        text: text.into(),
        images: vec![valid_img1, valid_img2],
    };
    let valid_total = text.len() + (9 + 8) + (10 + 8); // 9 + 17 + 18 = 44 bytes.
    assert!(valid_input.validate(&model, valid_total).is_ok());
    let err_valid_under = valid_input.validate(&model, valid_total - 1).unwrap_err();
    assert!(
        matches!(err_valid_under, RuntimeError::InvalidInput(ref msg) if msg.contains("exceeds the byte budget")),
        "expected budget error at valid_total - 1, got: {err_valid_under}"
    );
}

#[tokio::test]
async fn rt_06_task_enqueue_rejects_multi_image_cumulative_oversized_before_decode() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let mut model = Model::new("fixture", "typed-image-fixture");
    model.supports_images = true;
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(Script("return 1")),
        model,
        RuntimeConfig::default(),
    )
    .unwrap();

    let session = rt.create(None).unwrap().id;

    // Two images, each ~150KB of non-base64 characters. Individually each is below
    // the default 256KiB max_prompt_bytes, but cumulatively ~300KB exceeds 256KiB.
    let img1_data = "!@#$%^&*()".repeat(15_000);
    let img2_data = "!@#$%^&*()".repeat(15_000);
    let error = RpcHandler::call(
        &rt,
        "task.enqueue",
        json!({
            "session_id": session,
            "text": "multi-image cumulative budget test",
            "images": [
                {
                    "mime_type": "image/png",
                    "data": img1_data
                },
                {
                    "mime_type": "image/jpeg",
                    "data": img2_data
                }
            ]
        }),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert_eq!(error.code, "invalid_params");
    assert!(
        error.message.contains("byte budget"),
        "expected cumulative byte budget error before decoding, got: {}",
        error.message
    );
    assert!(rt.tasks(&session).unwrap().is_empty());
}
