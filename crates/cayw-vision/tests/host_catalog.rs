//! Vision tools appear in definitions and execute through VisionService.

use cayw_ai::CancellationToken;
use cayw_vision::*;
use serde_json::json;
use std::sync::Arc;

fn sample() -> ImageBuffer {
    let mut rgba = vec![0u8; 8 * 8 * 4];
    for y in 0..4 {
        for x in 0..4 {
            let idx = (y * 8 + x) * 4;
            rgba[idx..idx + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
    }
    ImageBuffer::from_rgba8(8, 8, rgba).unwrap()
}

fn image_arg(img: &ImageBuffer) -> serde_json::Value {
    use base64::Engine;
    json!({
        "width": img.width(),
        "height": img.height(),
        "rgba_base64": base64::engine::general_purpose::STANDARD.encode(img.rgba()),
    })
}

#[test]
fn catalog_registers_vision_tools() {
    let names: Vec<_> = definitions().into_iter().map(|d| d.name).collect();
    for expected in [
        "vision.crop",
        "vision.pixel_diff",
        "vision.colors",
        "vision.ocr",
        "vision.describe",
    ] {
        assert!(names.iter().any(|n| n == expected), "missing {expected}");
    }
}

#[test]
fn service_crop_and_colors_are_real_pixels() {
    let img = sample();
    let mut ocr = FixtureOcrEngine::new();
    ocr.insert(img.sha256(), "CAYW");
    let describe = Arc::new(CountingDescribeBackend::new());
    describe.seed(img.sha256(), "what?", "glyph");
    let router = VisionRouter::new(Arc::new(ocr), describe);
    let service = VisionService::with_router(router, Limits::default()).unwrap();

    let cropped = service
        .execute_value(
            "vision.crop",
            &json!({
                "image": image_arg(&img),
                "x": 0, "y": 0, "width": 4, "height": 4
            }),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(cropped["width"], 4);
    assert_eq!(cropped["height"], 4);
    assert!(!cropped["rgba_base64"].as_str().unwrap().is_empty());

    let colors = service
        .execute_value(
            "vision.colors",
            &json!({ "image": image_arg(&img), "top": 2 }),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(colors[0]["hex"], "#ffffff");
}

#[test]
fn service_ocr_uses_injected_fixture_backend() {
    let img = sample();
    let mut ocr = FixtureOcrEngine::new();
    ocr.insert(img.sha256(), "CAYW");
    let router = VisionRouter::new(Arc::new(ocr), Arc::new(CountingDescribeBackend::new()));
    let service = VisionService::with_router(router, Limits::default()).unwrap();
    let result = service
        .execute_value(
            "vision.ocr",
            &json!({ "image": image_arg(&img) }),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(result["text"], "CAYW");
    assert_eq!(result["engine"], "fixture");
}

#[test]
fn tesseract_backend_runs_when_installed() {
    if !TesseractOcrEngine::available() {
        return;
    }
    let img = ImageBuffer::solid(48, 24, [255, 255, 255, 255]).unwrap();
    let engine = TesseractOcrEngine::detect().unwrap();
    assert_eq!(engine.name(), "tesseract");
    assert!(engine.recognize(&img, &CancellationToken::new()).is_ok());
}

/// Slow OCR that polls cancel — mirrors subprocess backends without needing tesseract.
struct PollingSlowOcr;

impl OcrEngine for PollingSlowOcr {
    fn name(&self) -> &str {
        "slow-poll"
    }

    fn recognize(
        &self,
        _image: &ImageBuffer,
        cancel: &CancellationToken,
    ) -> Result<String, VisionError> {
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(30) {
            if cancel.is_cancelled() {
                return Err(VisionError::Cancelled);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Ok("should-not-finish".into())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_cancel_aborts_ocr_before_completion() {
    let img = sample();
    let router = VisionRouter::new(
        Arc::new(PollingSlowOcr),
        Arc::new(CountingDescribeBackend::new()),
    );
    let service = Arc::new(VisionService::with_router(router, Limits::default()).unwrap());
    let cancel = CancellationToken::new();
    let cancel_trigger = cancel.clone();
    let args = json!({ "image": image_arg(&img) });

    let join = tokio::spawn(async move { execute(service, "vision.ocr", args, cancel).await });

    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    cancel_trigger.cancel();

    let err = join
        .await
        .expect("join")
        .expect_err("ocr must fail with cancelled");
    let msg = err.to_string();
    assert!(
        msg.contains("cancelled"),
        "expected cancelled error, got {msg}"
    );
}
