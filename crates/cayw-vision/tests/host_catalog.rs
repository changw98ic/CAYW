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

#[test]
fn unconfigured_describe_fails_honestly_without_masquerading_as_qa() {
    let img = sample();
    let router = VisionRouter::new(
        Arc::new(FixtureOcrEngine::new()),
        Arc::new(UnconfiguredDescribeBackend),
    );
    let service = VisionService::with_router(router, Limits::default()).unwrap();
    let result = service.execute_value(
        "vision.describe",
        &json!({
            "image": image_arg(&img),
            "question": "What is in this image?"
        }),
        &CancellationToken::new(),
    );
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("not configured"),
        "expected unconfigured error, got {err_msg}"
    );
}

#[test]
fn invalid_image_length_still_rejected_by_image_buffer() {
    use base64::Engine;
    let service = VisionService::with_router(
        VisionRouter::new(
            Arc::new(FixtureOcrEngine::new()),
            Arc::new(CountingDescribeBackend::new()),
        ),
        Limits::default(),
    )
    .unwrap();
    // width=8, height=8 requires 8*8*4=256 bytes, but we only supply 16 bytes.
    let truncated_rgba = vec![0u8; 16];
    let bad_img = json!({
        "width": 8,
        "height": 8,
        "rgba_base64": base64::engine::general_purpose::STANDARD.encode(&truncated_rgba),
    });
    let result = service.execute_value(
        "vision.crop",
        &json!({
            "image": bad_img,
            "x": 0, "y": 0, "width": 4, "height": 4
        }),
        &CancellationToken::new(),
    );
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("does not match"),
        "expected mismatch error from ImageBuffer, got {err_msg}"
    );
}

struct MockImageSink(std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>);
impl ImageSink for MockImageSink {
    fn put(
        &self,
        key: &str,
        bytes: &[u8],
        _mime_type: Option<&str>,
    ) -> Result<serde_json::Value, cayw_codemode::CapabilityError> {
        self.0
            .lock()
            .unwrap()
            .insert(key.to_string(), bytes.to_vec());
        Ok(json!({"handle": key}))
    }
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, cayw_codemode::CapabilityError> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
}

struct FailingImageSink(bool);
impl ImageSink for FailingImageSink {
    fn put(
        &self,
        _: &str,
        _: &[u8],
        _: Option<&str>,
    ) -> Result<serde_json::Value, cayw_codemode::CapabilityError> {
        if self.0 {
            Err(cayw_codemode::CapabilityError::unknown(
                "injected image commit failure",
            ))
        } else {
            Err(cayw_codemode::CapabilityError::new(
                "injected image store refusal",
            ))
        }
    }
    fn get(&self, _: &str) -> Result<Option<Vec<u8>>, cayw_codemode::CapabilityError> {
        Ok(None)
    }
}

#[test]
fn image_sink_failures_preserve_unknown_and_never_fall_back_to_inline_pixels() {
    for unknown in [false, true] {
        let service = VisionService::with_router(
            VisionRouter::new(
                Arc::new(FixtureOcrEngine::new()),
                Arc::new(CountingDescribeBackend::new()),
            ),
            Limits::default(),
        )
        .unwrap()
        .with_sink(Arc::new(FailingImageSink(unknown)));
        let error = service
            .execute_value(
                "vision.crop",
                &json!({"image":image_arg(&sample()),"x":0,"y":0,"width":4,"height":4}),
                &CancellationToken::new(),
            )
            .unwrap_err();
        assert_eq!(error.outcome_unknown, unknown);
        assert!(error.message.contains("injected image"));
    }
}

#[test]
fn image_sink_receives_cropped_output() {
    let img = sample();
    let sink = Arc::new(MockImageSink(std::sync::Mutex::new(
        std::collections::HashMap::new(),
    )));
    let router = VisionRouter::new(
        Arc::new(FixtureOcrEngine::new()),
        Arc::new(CountingDescribeBackend::new()),
    );
    let service = VisionService::with_router(router, Limits::default())
        .unwrap()
        .with_sink(sink.clone());

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

    let sha = cropped["image_sha256"].as_str().unwrap();
    assert!(sink.0.lock().unwrap().contains_key(sha));
    assert_eq!(sink.0.lock().unwrap().get(sha).unwrap().len(), 4 * 4 * 4);
}

#[test]
fn image_schema_accepts_valid_handle_and_rejects_both_or_neither() {
    let defs = definitions();
    for tool_name in [
        "vision.crop",
        "vision.colors",
        "vision.ocr",
        "vision.describe",
    ] {
        let def = defs.iter().find(|d| d.name == tool_name).unwrap();
        let validator = cayw_ai::ArgumentValidator::new(&def.parameters).unwrap();

        // 1. Valid with rgba_base64
        let valid_b64 = match tool_name {
            "vision.crop" => json!({
                "image": {"width": 8, "height": 8, "rgba_base64": "AAAA"},
                "x": 0, "y": 0, "width": 4, "height": 4
            }),
            "vision.colors" => json!({
                "image": {"width": 8, "height": 8, "rgba_base64": "AAAA"},
                "top": 2
            }),
            "vision.ocr" => json!({
                "image": {"width": 8, "height": 8, "rgba_base64": "AAAA"}
            }),
            "vision.describe" => json!({
                "image": {"width": 8, "height": 8, "rgba_base64": "AAAA"},
                "question": "what is this?"
            }),
            _ => unreachable!(),
        };
        assert!(
            validator.validate(&valid_b64).is_ok(),
            "{tool_name} must accept rgba_base64"
        );

        // 2. Valid with handle
        let valid_handle = match tool_name {
            "vision.crop" => json!({
                "image": {"width": 8, "height": 8, "handle": "content-handle-123"},
                "x": 0, "y": 0, "width": 4, "height": 4
            }),
            "vision.colors" => json!({
                "image": {"width": 8, "height": 8, "handle": "content-handle-123"},
                "top": 2
            }),
            "vision.ocr" => json!({
                "image": {"width": 8, "height": 8, "handle": "content-handle-123"}
            }),
            "vision.describe" => json!({
                "image": {"width": 8, "height": 8, "handle": "content-handle-123"},
                "question": "what is this?"
            }),
            _ => unreachable!(),
        };
        assert!(
            validator.validate(&valid_handle).is_ok(),
            "{tool_name} must accept handle"
        );

        // 3. Invalid: both rgba_base64 and handle provided
        let mut both = valid_b64.clone();
        both["image"]["handle"] = json!("content-handle-123");
        assert!(
            validator.validate(&both).is_err(),
            "{tool_name} must reject both rgba_base64 and handle"
        );

        // 4. Invalid: neither rgba_base64 nor handle provided
        let mut neither = valid_b64.clone();
        neither["image"]
            .as_object_mut()
            .unwrap()
            .remove("rgba_base64");
        assert!(
            validator.validate(&neither).is_err(),
            "{tool_name} must reject image with neither source"
        );

        // 5. Invalid: unknown extra properties
        let mut extra = valid_handle.clone();
        extra["image"]["extra_field"] = json!("forbidden");
        assert!(
            validator.validate(&extra).is_err(),
            "{tool_name} must reject additional properties"
        );
    }
}

#[test]
fn image_handle_service_execution_and_mutual_exclusion() {
    let img = sample();
    let sink = Arc::new(MockImageSink(std::sync::Mutex::new(
        std::collections::HashMap::new(),
    )));
    sink.put("sample-handle", img.rgba(), Some("image/x-rgba8"))
        .unwrap();

    let service = VisionService::with_router(
        VisionRouter::new(
            Arc::new(FixtureOcrEngine::new()),
            Arc::new(CountingDescribeBackend::new()),
        ),
        Limits::default(),
    )
    .unwrap()
    .with_sink(sink.clone());

    // Valid handle execution via service
    let res = service
        .execute_value(
            "vision.colors",
            &json!({
                "image": {"width": 8, "height": 8, "handle": "sample-handle"},
                "top": 2
            }),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(res[0]["hex"], "#ffffff");

    // Both rgba_base64 and handle provided to service: rejected
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(img.rgba());
    let err_both = service
        .execute_value(
            "vision.colors",
            &json!({
                "image": {"width": 8, "height": 8, "rgba_base64": b64, "handle": "sample-handle"},
                "top": 2
            }),
            &CancellationToken::new(),
        )
        .unwrap_err();
    assert!(
        err_both.to_string().contains("not both"),
        "expected mutual exclusion error, got {err_both}"
    );

    // Missing handle: rejected
    let err_missing = service
        .execute_value(
            "vision.colors",
            &json!({
                "image": {"width": 8, "height": 8, "handle": "non-existent-handle"},
                "top": 2
            }),
            &CancellationToken::new(),
        )
        .unwrap_err();
    assert!(
        err_missing.to_string().contains("handle not found"),
        "expected handle not found error, got {err_missing}"
    );

    // Service without sink when handle is given: rejected
    let no_sink_service = VisionService::with_router(
        VisionRouter::new(
            Arc::new(FixtureOcrEngine::new()),
            Arc::new(CountingDescribeBackend::new()),
        ),
        Limits::default(),
    )
    .unwrap();
    let err_no_sink = no_sink_service
        .execute_value(
            "vision.colors",
            &json!({
                "image": {"width": 8, "height": 8, "handle": "sample-handle"},
                "top": 2
            }),
            &CancellationToken::new(),
        )
        .unwrap_err();
    assert!(
        err_no_sink.to_string().contains("sink is not configured"),
        "expected unconfigured sink error, got {err_no_sink}"
    );
}

fn spawn_mock_http_fixture<F>(
    handler: F,
) -> (
    String,
    std::sync::mpsc::Sender<()>,
    std::thread::JoinHandle<()>,
)
where
    F: Fn(std::net::TcpStream) + Send + 'static,
{
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        loop {
            if rx.try_recv().is_ok() {
                break;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    handler(stream);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    (format!("http://127.0.0.1:{port}"), tx, handle)
}

#[test]
fn http_describe_bounded_output_kills_curl_on_infinite_stream() {
    let (url, stop, handle) = spawn_mock_http_fixture(|mut stream| {
        use std::io::{Read, Write};
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let header = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n";
        let _ = stream.write_all(header.as_bytes());
        // Stream chunks continuously without EOF using valid HTTP chunked encoding
        let data = "{\"answer\":\"streaming-data-chunk\"}";
        let chunk = format!("{:x}\r\n{data}\r\n", data.len());
        for _ in 0..100 {
            if stream.write_all(chunk.as_bytes()).is_err() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });

    let backend = HttpDescribeBackend::new(&url, None, 64);
    let img = sample();
    let req = DescribeRequest {
        question: "what is this?".into(),
        json: false,
    };
    let result = backend.describe(&img, &req, &CancellationToken::new());
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("output limit exceeded"),
        "expected output limit exceeded error, got: {err_msg}"
    );
    assert_eq!(
        backend.billed_requests(),
        0,
        "failed describe must not be billed"
    );

    let _ = stop.send(());
    let _ = handle.join();
}

#[test]
fn http_describe_bounded_output_exact_boundary() {
    let raw_body = "{\"answer\":\"exact-boundary\"}";
    let body_len = raw_body.len();
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\n\r\n"
    );
    let full_response = format!("{header}{raw_body}");
    let exact_len = raw_body.len();

    let (url, stop, handle) = spawn_mock_http_fixture(move |mut stream| {
        use std::io::{Read, Write};
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let _ = stream.write_all(full_response.as_bytes());
    });

    let img = sample();
    let req = DescribeRequest {
        question: "boundary?".into(),
        json: false,
    };

    // Exactly at or above limit (curl stdout gets body when HTTP headers are omitted by curl):
    let backend_exact = HttpDescribeBackend::new(&url, None, exact_len);
    let res = backend_exact.describe(&img, &req, &CancellationToken::new());
    assert!(
        res.is_ok(),
        "exact boundary should succeed, got: {:?}",
        res.err()
    );
    assert_eq!(res.unwrap(), "exact-boundary");
    assert_eq!(backend_exact.billed_requests(), 1);

    // Below body length: rejected with limit exceeded
    let backend_below = HttpDescribeBackend::new(&url, None, exact_len - 1);
    let err = backend_below.describe(&img, &req, &CancellationToken::new());
    assert!(err.is_err());
    let err_str = err.unwrap_err().to_string();
    assert!(
        err_str.contains("output limit exceeded"),
        "expected limit exceeded error, got: {err_str}"
    );
    assert_eq!(backend_below.billed_requests(), 0);

    let _ = stop.send(());
    let _ = handle.join();
}

#[test]
fn http_describe_bounded_output_on_http_error() {
    let (url, stop, handle) = spawn_mock_http_fixture(|mut stream| {
        use std::io::{Read, Write};
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let big_error = "E".repeat(5000);
        let response = format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
            big_error.len(),
            big_error
        );
        let _ = stream.write_all(response.as_bytes());
    });

    // Body limit 64 bytes. 5000-byte error body must trip output limit and terminate curl.
    let backend = HttpDescribeBackend::new(&url, None, 64);
    let img = sample();
    let req = DescribeRequest {
        question: "500 test?".into(),
        json: false,
    };
    let err = backend.describe(&img, &req, &CancellationToken::new());
    assert!(err.is_err());
    let err_msg = err.unwrap_err().to_string();
    assert!(
        err_msg.contains("output limit exceeded") || err_msg.contains("describe HTTP failed"),
        "expected error, got: {err_msg}"
    );
    assert_eq!(backend.billed_requests(), 0);

    let _ = stop.send(());
    let _ = handle.join();
}

#[test]
fn unconfigured_describe_from_env_fails_honestly() {
    // If ambient environment has describe URL or mode configured, skip direct execution
    // to avoid unexpected network calls or mode mismatch during ambient cargo test runs.
    // The isolated test `env_describe_isolated_process_resolution` runs this explicitly with cleared env.
    if std::env::var(ENV_DESCRIBE_URL).is_ok() || std::env::var("CAYW_VISION_DESCRIBE").is_ok() {
        return;
    }
    let img = sample();
    let service = VisionService::from_env(Limits::default()).unwrap();
    let result = service.execute_value(
        "vision.describe",
        &json!({
            "image": image_arg(&img),
            "question": "What is in this image?"
        }),
        &CancellationToken::new(),
    );
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("not configured"),
        "expected unconfigured error, got {err_msg}"
    );
}

#[test]
fn env_describe_isolated_process_resolution() {
    // In an isolated process with cleared environment variables,
    // resolve_describe_backend() must resolve to UnconfiguredDescribeBackend without network calls.
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.arg("--nocapture")
        .arg("unconfigured_describe_from_env_fails_honestly")
        .arg("--exact")
        .env_remove(ENV_DESCRIBE_URL)
        .env_remove(ENV_DESCRIBE_KEY)
        .env_remove("CAYW_VISION_DESCRIBE");
    let output = cmd.output().expect("run isolated test process");
    assert!(
        output.status.success(),
        "isolated subprocess failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
