//! Configured OCR / describe backends report unavailable when not configured.
//!
//! - [`TesseractOcrEngine`]: shells out to the configured `tesseract` CLI.
//! - [`HttpDescribeBackend`]: optional HTTP describe when `CAYW_VISION_DESCRIBE_URL`
//!   is set (Bearer token from `CAYW_VISION_DESCRIBE_KEY` when present).
//! - Test fixtures require explicit injection or, for describe, explicit selection.
//!
//! Subprocess OCR / curl describe honor [`CancellationToken`]: cancel kills the
//! child via [`crate::process::run_command_cancellable`] (KillOnDrop), so Host
//! cancel does not leave orphan tesseract/curl processes.

use crate::image::ImageBuffer;
use crate::process::{RunError, run_command_bounded, run_command_cancellable};
use crate::router::{CountingDescribeBackend, DescribeBackend, OcrEngine};
use crate::types::{DescribeRequest, VisionError};
use cayw_ai::CancellationToken;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Env: force OCR engine selection (`tesseract`).
pub const ENV_OCR: &str = "CAYW_VISION_OCR";
/// Env: HTTP describe endpoint (POST JSON). Absent → unconfigured describe.
pub const ENV_DESCRIBE_URL: &str = "CAYW_VISION_DESCRIBE_URL";
/// Env: optional Bearer token for [`ENV_DESCRIBE_URL`].
pub const ENV_DESCRIBE_KEY: &str = "CAYW_VISION_DESCRIBE_KEY";

/// Default maximum response bytes collected from HTTP describe backend (1 MiB).
pub const DEFAULT_MAX_DESCRIBE_RESPONSE_BYTES: usize = 1024 * 1024;
/// Maximum stderr bytes collected from HTTP describe backend curl invocation (64 KiB).
pub const DEFAULT_MAX_DESCRIBE_STDERR_BYTES: usize = 64 * 1024;

/// OCR via the system `tesseract` binary (PPM stdin path, stdout text).
#[derive(Debug, Clone)]
pub struct TesseractOcrEngine {
    program: PathBuf,
    lang: String,
}

impl TesseractOcrEngine {
    pub fn new(program: impl Into<PathBuf>, lang: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            lang: lang.into(),
        }
    }

    /// Locate `tesseract` on PATH (or `CAYW_VISION_TESSERACT` absolute path).
    pub fn detect() -> Result<Self, VisionError> {
        let program = std::env::var_os("CAYW_VISION_TESSERACT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("tesseract"));
        let output = Command::new(&program)
            .arg("--version")
            .output()
            .map_err(|err| {
                VisionError::OcrFailed(format!("tesseract not runnable ({program:?}): {err}"))
            })?;
        if !output.status.success() {
            return Err(VisionError::OcrFailed(format!(
                "tesseract --version failed ({program:?})"
            )));
        }
        Ok(Self::new(program, "eng"))
    }

    pub fn available() -> bool {
        Self::detect().is_ok()
    }
}

impl OcrEngine for TesseractOcrEngine {
    fn name(&self) -> &str {
        "tesseract"
    }

    fn recognize(
        &self,
        image: &ImageBuffer,
        cancel: &CancellationToken,
    ) -> Result<String, VisionError> {
        if cancel.is_cancelled() {
            return Err(VisionError::Cancelled);
        }
        let dir = tempfile::tempdir()
            .map_err(|err| VisionError::OcrFailed(format!("tempdir for tesseract: {err}")))?;
        let ppm_path = dir.path().join("input.ppm");
        write_ppm(&ppm_path, image)?;
        let out_base = dir.path().join("out");
        let mut cmd = Command::new(&self.program);
        cmd.arg(&ppm_path)
            .arg(&out_base)
            .arg("-l")
            .arg(&self.lang)
            .arg("--psm")
            .arg("6");
        let output = run_command_cancellable(cmd, cancel).map_err(|err| match err {
            RunError::Cancelled => VisionError::Cancelled,
            other => VisionError::OcrFailed(format!("spawn tesseract: {}", other.message())),
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(VisionError::OcrFailed(format!(
                "tesseract exited {}: {stderr}",
                output.status
            )));
        }
        let text_path = dir.path().join("out.txt");
        let text = std::fs::read_to_string(&text_path).map_err(|err| {
            VisionError::OcrFailed(format!("read tesseract output {text_path:?}: {err}"))
        })?;
        Ok(text.trim().to_string())
    }
}

fn write_ppm(path: &Path, image: &ImageBuffer) -> Result<(), VisionError> {
    let mut file = std::fs::File::create(path)
        .map_err(|err| VisionError::OcrFailed(format!("create ppm {path:?}: {err}")))?;
    // Binary PPM (P6): drop alpha; tesseract/leptonica accept this without PNG deps.
    writeln!(file, "P6\n{} {}\n255", image.width(), image.height())
        .map_err(|err| VisionError::OcrFailed(format!("write ppm header: {err}")))?;
    let rgba = image.rgba();
    let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
    for px in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&px[..3]);
    }
    file.write_all(&rgb)
        .map_err(|err| VisionError::OcrFailed(format!("write ppm body: {err}")))?;
    Ok(())
}

/// HTTP describe backend. POSTs JSON to `CAYW_VISION_DESCRIBE_URL`.
///
/// Request body: `{ "width", "height", "rgba_base64", "image_sha256", "question", "json" }`.
/// Response JSON must include `"answer": string`.
#[derive(Debug)]
pub struct HttpDescribeBackend {
    url: String,
    key: Option<String>,
    max_response_bytes: usize,
    billed: AtomicU64,
}

impl HttpDescribeBackend {
    pub fn new(url: impl Into<String>, key: Option<String>, max_response_bytes: usize) -> Self {
        Self {
            url: url.into(),
            key,
            max_response_bytes,
            billed: AtomicU64::new(0),
        }
    }

    pub fn from_env() -> Result<Self, VisionError> {
        let url = std::env::var(ENV_DESCRIBE_URL)
            .map_err(|_| VisionError::DescribeFailed(format!("{ENV_DESCRIBE_URL} is not set")))?;
        if url.trim().is_empty() {
            return Err(VisionError::DescribeFailed(format!(
                "{ENV_DESCRIBE_URL} is empty"
            )));
        }
        let key = std::env::var(ENV_DESCRIBE_KEY)
            .ok()
            .filter(|k| !k.is_empty());
        Ok(Self::new(url, key, DEFAULT_MAX_DESCRIBE_RESPONSE_BYTES))
    }
}

impl DescribeBackend for HttpDescribeBackend {
    fn describe(
        &self,
        image: &ImageBuffer,
        request: &DescribeRequest,
        cancel: &CancellationToken,
    ) -> Result<String, VisionError> {
        if cancel.is_cancelled() {
            return Err(VisionError::Cancelled);
        }
        let body = serde_json::json!({
            "width": image.width(),
            "height": image.height(),
            "rgba_base64": base64_encode(image.rgba()),
            "image_sha256": image.sha256(),
            "question": request.question,
            "json": request.json,
        });
        let body_str = serde_json::to_string(&body).map_err(|err| {
            VisionError::DescribeFailed(format!("serialize describe request: {err}"))
        })?;
        // Prefer curl so cayw-vision stays free of a blocking HTTP client feature
        // in the default build; CI never sets the URL and never reaches here.
        // --max-time 60 is a ceiling; Host cancel kills curl earlier via KillOnDrop.
        let mut cmd = Command::new("curl");
        cmd.args([
            "--silent",
            "--show-error",
            "--fail-with-body",
            "-X",
            "POST",
            "-H",
            "content-type: application/json",
            "--max-time",
            "60",
            "-d",
            &body_str,
        ]);
        if let Some(key) = &self.key {
            cmd.args(["-H", &format!("authorization: Bearer {key}")]);
        }
        cmd.arg(&self.url);
        let output = run_command_bounded(
            cmd,
            cancel,
            self.max_response_bytes,
            DEFAULT_MAX_DESCRIBE_STDERR_BYTES,
        )
        .map_err(|err| match err {
            RunError::Cancelled => VisionError::Cancelled,
            RunError::OutputLimitExceeded(msg) => {
                VisionError::DescribeFailed(format!("describe output limit exceeded: {msg}"))
            }
            RunError::Io(err) => VisionError::DescribeFailed(format!("spawn curl: {err}")),
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(VisionError::DescribeFailed(format!(
                "describe HTTP failed ({}): {stderr} {stdout}",
                output.status
            )));
        }
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|err| VisionError::DescribeFailed(format!("describe response JSON: {err}")))?;
        let answer = value
            .get("answer")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                VisionError::DescribeFailed("describe response missing string answer".into())
            })?
            .to_string();
        self.billed.fetch_add(1, Ordering::SeqCst);
        Ok(answer)
    }

    fn billed_requests(&self) -> u64 {
        self.billed.load(Ordering::SeqCst)
    }
}

struct UnavailableOcrEngine {
    cause: VisionError,
}

impl OcrEngine for UnavailableOcrEngine {
    fn name(&self) -> &str {
        "unavailable"
    }

    fn recognize(
        &self,
        _image: &ImageBuffer,
        _cancel: &CancellationToken,
    ) -> Result<String, VisionError> {
        Err(self.cause.clone())
    }
}

/// Pick OCR: `CAYW_VISION_OCR=tesseract` configures tesseract; unconfigured or invalid
/// values return explicit unavailable engines; test fixtures are injected explicitly.
pub fn resolve_ocr_engine() -> Arc<dyn OcrEngine> {
    ocr_engine(
        std::env::var(ENV_OCR).ok().as_deref(),
        TesseractOcrEngine::detect,
    )
}

fn ocr_engine(
    config: Option<&str>,
    detect: impl FnOnce() -> Result<TesseractOcrEngine, VisionError>,
) -> Arc<dyn OcrEngine> {
    let cause = match config {
        Some("tesseract") => match detect() {
            Ok(engine) => return Arc::new(engine),
            Err(cause) => cause,
        },
        Some(other) => VisionError::OcrFailed(format!(
            "invalid {ENV_OCR} value '{other}': expected 'tesseract'"
        )),
        None => VisionError::OcrFailed(
            "vision ocr backend is not configured: set CAYW_VISION_OCR=tesseract or inject an OcrEngine".into(),
        ),
    };
    Arc::new(UnavailableOcrEngine { cause })
}

/// Unconfigured describe backend that returns an explicit failure explaining how to configure a backend.
/// The test counting backend does not masquerade as default Q&A in production.
#[derive(Debug, Default)]
pub struct UnconfiguredDescribeBackend;

impl DescribeBackend for UnconfiguredDescribeBackend {
    fn describe(
        &self,
        _image: &ImageBuffer,
        _request: &DescribeRequest,
        _cancel: &CancellationToken,
    ) -> Result<String, VisionError> {
        Err(VisionError::DescribeFailed(
            "vision describe backend is not configured: set CAYW_VISION_DESCRIBE_URL or configure a DescribeBackend explicitly (set CAYW_VISION_DESCRIBE=fixture for testing)".into(),
        ))
    }

    fn billed_requests(&self) -> u64 {
        0
    }
}

/// Pick describe: HTTP when `CAYW_VISION_DESCRIBE_URL` is set;
/// counting fixture only when explicitly configured via `CAYW_VISION_DESCRIBE=fixture` or `counting`;
/// otherwise `UnconfiguredDescribeBackend` so testing counting does not masquerade as default Q&A.
pub fn resolve_describe_backend() -> Arc<dyn DescribeBackend> {
    describe_backend(
        std::env::var(ENV_DESCRIBE_URL).ok().as_deref(),
        std::env::var(ENV_DESCRIBE_KEY).ok().as_deref(),
        std::env::var("CAYW_VISION_DESCRIBE").ok().as_deref(),
    )
}

pub(crate) fn describe_backend(
    url: Option<&str>,
    key: Option<&str>,
    mode: Option<&str>,
) -> Arc<dyn DescribeBackend> {
    if let Some(url) = url.filter(|u| !u.trim().is_empty()) {
        let key = key.filter(|k| !k.is_empty()).map(String::from);
        return Arc::new(HttpDescribeBackend::new(
            url,
            key,
            DEFAULT_MAX_DESCRIBE_RESPONSE_BYTES,
        ));
    }
    if let Some(mode) = mode
        && (mode == "fixture" || mode == "counting")
    {
        return Arc::new(CountingDescribeBackend::new());
    }
    Arc::new(UnconfiguredDescribeBackend)
}

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub(crate) fn base64_decode(text: &str) -> Result<Vec<u8>, VisionError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|err| VisionError::InvalidInput(format!("invalid rgba_base64: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::FixtureOcrEngine;

    #[test]
    fn ppm_roundtrip_invokes_tesseract_when_present() {
        if !TesseractOcrEngine::available() {
            return;
        }
        let img = ImageBuffer::solid(32, 16, [255, 255, 255, 255]).unwrap();
        let engine = TesseractOcrEngine::detect().unwrap();
        let text = engine.recognize(&img, &CancellationToken::new()).unwrap();
        // Blank image: engine must succeed; text may be empty.
        let _ = text;
        assert_eq!(engine.name(), "tesseract");
    }

    #[test]
    fn fixture_engine_name_is_stable() {
        let engine = FixtureOcrEngine::new();
        assert_eq!(engine.name(), "fixture");
    }

    #[test]
    fn ocr_configuration_preserves_unavailable_causes_through_service() {
        use crate::VisionRouter;
        use crate::service::{Limits, VisionService};
        use serde_json::json;

        for config in [None, Some("misspelled"), Some("fixture"), Some("tesseract")] {
            let engine = ocr_engine(config, || {
                assert_eq!(config, Some("tesseract"));
                Err(VisionError::OcrFailed("tesseract binary missing".into()))
            });
            assert_eq!(engine.name(), "unavailable");
            let service = VisionService::with_router(
                VisionRouter::new(engine, Arc::new(CountingDescribeBackend::new())),
                Limits::default(),
            )
            .unwrap();
            let error = service
                .execute_value(
                    "vision.ocr",
                    &json!({"image":{"width":1,"height":1,"rgba_base64":"AAAA/w=="}}),
                    &CancellationToken::new(),
                )
                .unwrap_err()
                .to_string();
            let expected = match config {
                None => "not configured",
                Some("tesseract") => "tesseract binary missing",
                Some(value) => value,
            };
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn describe_backend_selection_preserves_unconfigured_and_fixture() {
        let empty_img = ImageBuffer::solid(1, 1, [0, 0, 0, 255]).unwrap();
        let cancel = CancellationToken::new();
        let req = DescribeRequest {
            question: "test?".into(),
            json: false,
        };

        // None / empty URL → UnconfiguredDescribeBackend
        let unconfigured = describe_backend(None, None, None);
        let err = unconfigured
            .describe(&empty_img, &req, &cancel)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not configured"), "{err}");

        let unconfigured_empty = describe_backend(Some("   "), None, None);
        let err = unconfigured_empty
            .describe(&empty_img, &req, &cancel)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not configured"), "{err}");

        // Explicit fixture / counting mode → CountingDescribeBackend
        // Counting describe backend requires explicit seed; unseeded calls fail honestly
        // without masquerading as real Q&A.
        for mode in ["fixture", "counting"] {
            let counting = describe_backend(None, None, Some(mode));
            let err = counting
                .describe(&empty_img, &req, &cancel)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("no seeded answer"),
                "expected counting backend requiring explicit seed, got: {err}"
            );
            assert_eq!(counting.billed_requests(), 0);
        }

        // Explicit seed on CountingDescribeBackend succeeds and increments billing
        let seeded = CountingDescribeBackend::new();
        seeded.seed(empty_img.sha256(), &req.question, "seeded answer");
        let res = seeded.describe(&empty_img, &req, &cancel).unwrap();
        assert_eq!(res, "seeded answer");
        assert_eq!(seeded.billed_requests(), 1);

        // Unknown / unsupported mode string → fallback to UnconfiguredDescribeBackend
        for unknown_mode in ["unsupported", "invalid_mode", "custom"] {
            let unconfigured_unknown = describe_backend(None, None, Some(unknown_mode));
            let err = unconfigured_unknown
                .describe(&empty_img, &req, &cancel)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("not configured"),
                "expected unconfigured fallback for mode '{unknown_mode}', got: {err}"
            );
            assert_eq!(unconfigured_unknown.billed_requests(), 0);
        }
    }
}
