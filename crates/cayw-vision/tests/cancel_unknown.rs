use cayw_ai::CancellationToken;
use cayw_codemode::CapabilityError;
use cayw_vision::{
    CountingDescribeBackend, FixtureOcrEngine, ImageSink, Limits, VisionRouter, VisionService,
    execute,
};
use serde_json::{Value, json};
use std::sync::Arc;

struct UncertainSink(CancellationToken);
impl ImageSink for UncertainSink {
    fn put(&self, _: &str, _: &[u8], _: Option<&str>) -> Result<Value, CapabilityError> {
        self.0.cancel();
        Err(CapabilityError::unknown("sink commit uncertain"))
    }
    fn get(&self, _: &str) -> Result<Option<Vec<u8>>, CapabilityError> {
        unreachable!("inline image")
    }
}

#[tokio::test]
async fn cancellation_during_sink_commit_preserves_unknown_effect() {
    let cancel = CancellationToken::new();
    let service = VisionService::with_router(
        VisionRouter::new(
            Arc::new(FixtureOcrEngine::new()),
            Arc::new(CountingDescribeBackend::new()),
        ),
        Limits::default(),
    )
    .unwrap()
    .with_sink(Arc::new(UncertainSink(cancel.clone())));
    let error = execute(
        Arc::new(service),
        "vision.crop",
        json!({
            "image": {"width": 1, "height": 1, "rgba_base64": "AAAA/w=="},
            "x": 0, "y": 0, "width": 1, "height": 1
        }),
        cancel,
    )
    .await
    .unwrap_err();
    assert!(error.outcome_unknown);
    assert!(error.message.contains("sink commit uncertain"));
}

struct CommittedSink(CancellationToken);
impl ImageSink for CommittedSink {
    fn put(&self, _: &str, _: &[u8], _: Option<&str>) -> Result<Value, CapabilityError> {
        self.0.cancel();
        Ok(json!({"handle":"committed-image"}))
    }
    fn get(&self, _: &str) -> Result<Option<Vec<u8>>, CapabilityError> {
        unreachable!("inline image")
    }
}
#[tokio::test]
async fn cancellation_during_successful_sink_commit_preserves_success() {
    let cancel = CancellationToken::new();
    let service = VisionService::with_router(
        VisionRouter::new(
            Arc::new(FixtureOcrEngine::new()),
            Arc::new(CountingDescribeBackend::new()),
        ),
        Limits::default(),
    )
    .unwrap()
    .with_sink(Arc::new(CommittedSink(cancel.clone())));
    let result = execute(
        Arc::new(service),
        "vision.crop",
        json!({
            "image": {"width": 1, "height": 1, "rgba_base64": "AAAA/w=="},
            "x": 0, "y": 0, "width": 1, "height": 1
        }),
        cancel,
    )
    .await
    .unwrap();
    assert_eq!(result["handle"], "committed-image");
}
