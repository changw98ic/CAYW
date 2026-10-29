//! Typed prompt input keeps text and images in one durable record. These tests
//! exercise the real RPC adapter and Runtime queue; no model is called by
//! `task.enqueue`.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason,
};
use cayw_ipc::RpcHandler;
use cayw_runtime::{ImageInput, Runtime, RuntimeConfig, RuntimeError, TaskInput};
use futures::stream;
use serde_json::{Value, json};
use std::sync::Arc;

struct FixtureProvider {
    seen: std::sync::Mutex<Vec<Message>>,
}
impl Provider for FixtureProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        self.seen
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .extend(request.messages.clone());
        let mut message = AssistantMessage::new(&request.model);
        message.content.push(Content::text("fixture reply"));
        message.stop_reason = StopReason::Stop;
        Box::pin(stream::iter(vec![AssistantEvent::Done { message }]))
    }
}

fn runtime(temp: &tempfile::TempDir, supports_images: bool) -> (Runtime, Arc<FixtureProvider>) {
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let mut model = Model::new("fixture", "typed-image-fixture");
    model.supports_images = supports_images;
    let provider = Arc::new(FixtureProvider {
        seen: std::sync::Mutex::new(vec![]),
    });
    let runtime = Runtime::open(
        temp.path().join("sessions.db"),
        &workspace,
        provider.clone(),
        model,
        RuntimeConfig::default(),
    )
    .unwrap();
    (runtime, provider)
}

async fn rpc(runtime: &Runtime, method: &str, params: Value) -> Result<Value, cayw_ipc::RpcError> {
    runtime.call(method, params, CancellationToken::new()).await
}

#[tokio::test]
async fn task_enqueue_accepts_text_and_canonical_images_as_one_input() {
    let temp = tempfile::tempdir().unwrap();
    let (runtime, _) = runtime(&temp, true);
    let session = runtime.create(None).unwrap().id;

    let record = rpc(
        &runtime,
        "task.enqueue",
        json!({
            "session_id":session,
            "text":"describe this image",
            "images":[{"mime_type":"image/png","data":"aGVsbG8="}]
        }),
    )
    .await
    .unwrap();

    assert_eq!(record["input"]["text"], "describe this image");
    assert_eq!(record["input"]["images"][0]["mime_type"], "image/png");
    assert_eq!(record["input"]["images"][0]["data"], "aGVsbG8=");
    assert!(record.get("text").is_none(), "{record}");
    let stored = serde_json::to_value(
        runtime
            .task(record["id"].as_str().expect("record id"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(stored["input"], record["input"]);
}

#[tokio::test]
async fn task_enqueue_without_images_uses_the_same_input_shape() {
    let temp = tempfile::tempdir().unwrap();
    let (runtime, _) = runtime(&temp, false);
    let session = runtime.create(None).unwrap().id;
    let record = rpc(
        &runtime,
        "task.enqueue",
        json!({"session_id":session,"text":"text only"}),
    )
    .await
    .unwrap();
    assert_eq!(
        record["input"],
        json!({"text":"text only","images":[]}),
        "{record}"
    );
    assert!(record.get("text").is_none(), "{record}");
}

#[tokio::test]
async fn task_enqueue_rejects_invalid_images_before_insert() {
    let temp = tempfile::tempdir().unwrap();
    let (runtime, _) = runtime(&temp, true);

    let session = runtime.create(None).unwrap().id;
    for images in [
        json!([{"mime_type":"image/png","data":"aGVsbG8"}]),
        json!([{"mime_type":"image/png","data":"aGVsbG8_"}]),
        json!([{"mime_type":"image/png","data":"aGVs bG8="}]),
        json!([{"mime_type":"text/plain","data":"aGVsbG8="}]),
        json!([{"mime_type":"image/png","data":""}]),
        json!([{"mime_type":"image/png","data":"aGVsbG8=","filename":"x.png"}]),
    ] {
        let error = rpc(
            &runtime,
            "task.enqueue",
            json!({"session_id":session,"text":"describe","images":images}),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "invalid_params", "{images}");
        assert!(runtime.tasks(&session).unwrap().is_empty(), "{images}");
    }

    let session = runtime.create(None).unwrap().id;
    let error = rpc(
        &runtime,
        "task.enqueue",
        json!({
            "session_id":session,"text":"text",
            "images":[{"mime_type":"image/png","data":"aGVsbG8="}],
            "extra":true
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "invalid_params");
    assert!(runtime.tasks(&session).unwrap().is_empty());
}

#[tokio::test]
async fn task_enqueue_rejects_images_for_a_text_only_model_before_insert() {
    let temp = tempfile::tempdir().unwrap();
    let (runtime, _) = runtime(&temp, false);
    let session = runtime.create(None).unwrap().id;
    let error = rpc(
        &runtime,
        "task.enqueue",
        json!({
            "session_id":session,"text":"describe",
            "images":[{"mime_type":"image/png","data":"aGVsbG8="}]
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "invalid_params");
    assert!(runtime.tasks(&session).unwrap().is_empty());
}

#[tokio::test]
async fn session_prompt_delivers_the_image_content_to_the_provider() {
    let temp = tempfile::tempdir().unwrap();
    let (runtime, provider) = runtime(&temp, true);
    let session = runtime.create(None).unwrap().id;
    rpc(
        &runtime,
        "session.prompt",
        json!({
            "session_id":session,
            "text":"what is in this image",
            "images":[{"mime_type":"image/png","data":"aGVsbG8="}]
        }),
    )
    .await
    .unwrap();

    let seen = provider
        .seen
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let user = seen
        .iter()
        .find(|message| matches!(message, Message::User { .. }))
        .expect("provider saw a user message");
    let Message::User { content, .. } = user else {
        unreachable!()
    };
    assert!(content.iter().any(|part| matches!(
        part,
        Content::Image { data, mime_type }
            if data == "aGVsbG8=" && mime_type == "image/png"
    )));
}

#[test]
fn task_input_validation_is_local_and_strict() {
    let mut model = Model::new("fixture", "typed-image-fixture");
    model.supports_images = true;
    let text_only = TaskInput {
        text: "hello".into(),
        images: vec![],
    };
    assert!(text_only.validate(&model, 5).is_ok());
    assert!(text_only.validate(&model, 4).is_err());
    assert!(
        TaskInput::default().validate(&model, 1024).is_err(),
        "empty text with no images is not a prompt"
    );

    let image = ImageInput {
        mime_type: "image/png".into(),
        data: "aGVsbG8=".into(),
    };
    let with_image = TaskInput {
        text: "hello".into(),
        images: vec![image.clone()],
    };
    let exact = "hello".len() + image.mime_type.len() + image.data.len();
    assert!(with_image.validate(&model, exact).is_ok());
    assert!(with_image.validate(&model, exact - 1).is_err());

    model.supports_images = false;
    assert!(
        with_image.validate(&model, usize::MAX).is_err(),
        "a text-only model refuses images before enqueue"
    );
}

#[test]
fn task_input_message_keeps_text_and_images_in_order() {
    let input = TaskInput {
        text: "first".into(),
        images: vec![
            ImageInput {
                mime_type: "image/png".into(),
                data: "aGVsbG8=".into(),
            },
            ImageInput {
                mime_type: "image/jpeg".into(),
                data: "d29ybGQ=".into(),
            },
        ],
    };
    let Message::User { content, .. } = input.message() else {
        panic!("task input becomes a user message");
    };
    assert_eq!(
        content,
        vec![
            Content::Text {
                text: "first".into()
            },
            Content::Image {
                data: "aGVsbG8=".into(),
                mime_type: "image/png".into()
            },
            Content::Image {
                data: "d29ybGQ=".into(),
                mime_type: "image/jpeg".into()
            },
        ]
    );
}

#[test]
fn task_input_budget_checked_before_decoding_or_mime_validation() {
    let mut model = Model::new("fixture", "typed-image-fixture");
    model.supports_images = true;

    // Construct image with invalid MIME and non-base64 characters.
    let invalid_image = ImageInput {
        mime_type: "invalid/unsupported-mime".into(),
        data: "!@#$%^&*()_+~`{}|[]\\:\";'<>?,./".into(),
    };
    let input = TaskInput {
        text: "prompt text".into(),
        images: vec![invalid_image.clone()],
    };
    let total_bytes =
        "prompt text".len() + invalid_image.mime_type.len() + invalid_image.data.len();

    // With budget strictly smaller than total_bytes, validation fails with byte budget error,
    // not MIME type or base64 decoding error.
    let err_under_budget = input.validate(&model, total_bytes - 1).unwrap_err();
    assert!(
        matches!(err_under_budget, RuntimeError::InvalidInput(ref msg) if msg.contains("exceeds the byte budget")),
        "expected byte budget error before mime/decode validation, got: {err_under_budget}"
    );

    // With budget sufficient for total_bytes, validation proceeds past budget check
    // and catches the invalid MIME type.
    let err_sufficient_budget = input.validate(&model, total_bytes).unwrap_err();
    assert!(
        matches!(err_sufficient_budget, RuntimeError::InvalidInput(ref msg) if msg.contains("MIME type is not supported")),
        "expected mime type error when budget is sufficient, got: {err_sufficient_budget}"
    );

    // Construct image with valid MIME but non-base64 data.
    let invalid_b64_image = ImageInput {
        mime_type: "image/png".into(),
        data: "not valid base64!!!".into(),
    };
    let input_b64 = TaskInput {
        text: "prompt text".into(),
        images: vec![invalid_b64_image.clone()],
    };
    let total_b64_bytes =
        "prompt text".len() + invalid_b64_image.mime_type.len() + invalid_b64_image.data.len();

    let err_b64_under_budget = input_b64.validate(&model, total_b64_bytes - 1).unwrap_err();
    assert!(
        matches!(err_b64_under_budget, RuntimeError::InvalidInput(ref msg) if msg.contains("exceeds the byte budget")),
        "expected byte budget error before base64 decode, got: {err_b64_under_budget}"
    );

    let err_b64_sufficient = input_b64.validate(&model, total_b64_bytes).unwrap_err();
    assert!(
        matches!(err_b64_sufficient, RuntimeError::InvalidInput(ref msg) if msg.contains("not valid base64")),
        "expected base64 error when budget is sufficient, got: {err_b64_sufficient}"
    );
}

#[tokio::test]
async fn task_enqueue_rejects_oversized_invalid_image_on_budget_first() {
    let temp = tempfile::tempdir().unwrap();
    let (runtime, _) = runtime(&temp, true);
    let session = runtime.create(None).unwrap().id;

    // Construct oversized input (> default 256KiB prompt budget) with invalid base64 data.
    let oversized_data = "x".repeat(300 * 1024);
    let error = rpc(
        &runtime,
        "task.enqueue",
        json!({
            "session_id": session,
            "text": "oversized payload",
            "images": [{"mime_type": "image/png", "data": oversized_data}]
        }),
    )
    .await
    .unwrap_err();

    assert_eq!(error.code, "invalid_params");
    assert!(
        error.message.contains("byte budget"),
        "expected prompt budget error, got: {}",
        error.message
    );
    assert!(runtime.tasks(&session).unwrap().is_empty());
}

#[test]
fn task_input_multi_image_cumulative_budget_checked_before_decoding() {
    let mut model = Model::new("fixture", "typed-image-fixture");
    model.supports_images = true;

    let img1 = ImageInput {
        mime_type: "image/png".into(),
        data: "!@#$%^&*()_+~`{}|[]\\:\";'<>?,./".into(),
    };
    let img2 = ImageInput {
        mime_type: "image/jpeg".into(),
        data: "!@#$%^&*()_+~`{}|[]\\:\";'<>?,./".into(),
    };
    let input = TaskInput {
        text: "prompt".into(),
        images: vec![img1, img2],
    };
    let total_bytes = input.text.len()
        + input
            .images
            .iter()
            .map(|image| image.mime_type.len() + image.data.len())
            .sum::<usize>();
    let individual_budget = input.text.len()
        + input
            .images
            .iter()
            .map(|image| image.mime_type.len() + image.data.len())
            .max()
            .unwrap();
    assert!(individual_budget < total_bytes);

    // Either image fits alone, but both exceed the cumulative budget.
    let err_individual = input.validate(&model, individual_budget).unwrap_err();
    assert!(
        matches!(err_individual, RuntimeError::InvalidInput(ref msg) if msg.contains("exceeds the byte budget")),
        "multi-image cumulative budget must reject before decoding: {err_individual}"
    );

    let err_under = input.validate(&model, total_bytes - 1).unwrap_err();
    assert!(
        matches!(err_under, RuntimeError::InvalidInput(ref msg) if msg.contains("exceeds the byte budget")),
        "expected byte budget error at total - 1: {err_under}"
    );

    let err_exact = input.validate(&model, total_bytes).unwrap_err();
    assert!(
        matches!(err_exact, RuntimeError::InvalidInput(ref msg) if msg.contains("not valid base64")),
        "expected base64 error once exact budget is met: {err_exact}"
    );

    let valid_img1 = ImageInput {
        mime_type: "image/png".into(),
        data: STANDARD.encode(b"img1"),
    };
    let valid_img2 = ImageInput {
        mime_type: "image/jpeg".into(),
        data: STANDARD.encode(b"img2"),
    };
    let valid_total = "prompt".len()
        + (valid_img1.mime_type.len() + valid_img1.data.len())
        + (valid_img2.mime_type.len() + valid_img2.data.len());
    let valid_input = TaskInput {
        text: "prompt".into(),
        images: vec![valid_img1, valid_img2],
    };
    assert!(valid_input.validate(&model, valid_total).is_ok());
    let err_valid_under = valid_input.validate(&model, valid_total - 1).unwrap_err();
    assert!(
        matches!(err_valid_under, RuntimeError::InvalidInput(ref msg) if msg.contains("exceeds the byte budget")),
        "expected byte budget error for valid images at total - 1: {err_valid_under}"
    );
}

#[tokio::test]
async fn task_enqueue_rejects_multi_image_cumulative_oversized_on_budget_first() {
    let temp = tempfile::tempdir().unwrap();
    let (runtime, _) = runtime(&temp, true);
    let session = runtime.create(None).unwrap().id;

    // Two images, each 150KiB (< 256KiB default prompt budget), cumulatively 300KiB (> 256KiB).
    let oversized_part = "x".repeat(150 * 1024);
    let error = rpc(
        &runtime,
        "task.enqueue",
        json!({
            "session_id": session,
            "text": "multi-image payload",
            "images": [
                {"mime_type": "image/png", "data": oversized_part},
                {"mime_type": "image/jpeg", "data": oversized_part}
            ]
        }),
    )
    .await
    .unwrap_err();

    assert_eq!(error.code, "invalid_params");
    assert!(
        error.message.contains("byte budget"),
        "expected prompt budget error for multi-image cumulative overflow, got: {}",
        error.message
    );
    assert!(runtime.tasks(&session).unwrap().is_empty());
}
