//! Real OCR / describe backends with fixture fallback for CI.
//!
//! - [`TesseractOcrEngine`]: shells out to the `tesseract` CLI when installed.
//! - [`HttpDescribeBackend`]: optional HTTP describe when `CAYW_VISION_DESCRIBE_URL`
//!   is set (Bearer token from `CAYW_VISION_DESCRIBE_KEY` when present).
//! - [`FixtureOcrEngine`] / [`CountingDescribeBackend`]: remain the default when
//!   those deps / env keys are absent so CI stays offline and dependency-free.

use crate::image::ImageBuffer;
use crate::router::{CountingDescribeBackend, DescribeBackend, FixtureOcrEngine, OcrEngine};
use crate::types::{DescribeRequest, VisionError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Env: force OCR engine selection (`tesseract` | `fixture`).
pub const ENV_OCR: &str = "CAYW_VISION_OCR";
/// Env: HTTP describe endpoint (POST JSON). Absent → counting/fixture describe.
pub const ENV_DESCRIBE_URL: &str = "CAYW_VISION_DESCRIBE_URL";
/// Env: optional Bearer token for [`ENV_DESCRIBE_URL`].
pub const ENV_DESCRIBE_KEY: &str = "CAYW_VISION_DESCRIBE_KEY";

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

    fn recognize(&self, image: &ImageBuffer) -> Result<String, VisionError> {
        let dir = tempfile::tempdir()
            .map_err(|err| VisionError::OcrFailed(format!("tempdir for tesseract: {err}")))?;
        let ppm_path = dir.path().join("input.ppm");
        write_ppm(&ppm_path, image)?;
        let out_base = dir.path().join("out");
        let output = Command::new(&self.program)
            .arg(&ppm_path)
            .arg(&out_base)
            .arg("-l")
            .arg(&self.lang)
            .arg("--psm")
            .arg("6")
            .output()
            .map_err(|err| VisionError::OcrFailed(format!("spawn tesseract: {err}")))?;
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
    billed: AtomicU64,
}

impl HttpDescribeBackend {
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
        Ok(Self {
            url,
            key,
            billed: AtomicU64::new(0),
        })
    }
}

impl DescribeBackend for HttpDescribeBackend {
    fn describe(
        &self,
        image: &ImageBuffer,
        request: &DescribeRequest,
    ) -> Result<String, VisionError> {
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
        let output = cmd
            .output()
            .map_err(|err| VisionError::DescribeFailed(format!("spawn curl: {err}")))?;
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

/// Pick OCR: `CAYW_VISION_OCR=fixture` forces fixture; `=tesseract` requires CLI;
/// otherwise prefer tesseract when detectable, else empty fixture (CI-safe).
pub fn resolve_ocr_engine() -> Arc<dyn OcrEngine> {
    match std::env::var(ENV_OCR).ok().as_deref() {
        Some("fixture") => Arc::new(FixtureOcrEngine::new()),
        Some("tesseract") => Arc::new(
            TesseractOcrEngine::detect()
                .unwrap_or_else(|_| TesseractOcrEngine::new("tesseract", "eng")),
        ),
        Some(other) => {
            let _ = other;
            Arc::new(FixtureOcrEngine::new())
        }
        None => match TesseractOcrEngine::detect() {
            Ok(engine) => Arc::new(engine),
            Err(_) => Arc::new(FixtureOcrEngine::new()),
        },
    }
}

/// Pick describe: HTTP when `CAYW_VISION_DESCRIBE_URL` is set, else counting fixture.
pub fn resolve_describe_backend() -> Arc<dyn DescribeBackend> {
    match HttpDescribeBackend::from_env() {
        Ok(backend) => Arc::new(backend),
        Err(_) => Arc::new(CountingDescribeBackend::new()),
    }
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

    #[test]
    fn ppm_roundtrip_invokes_tesseract_when_present() {
        if !TesseractOcrEngine::available() {
            return;
        }
        let img = ImageBuffer::solid(32, 16, [255, 255, 255, 255]).unwrap();
        let engine = TesseractOcrEngine::detect().unwrap();
        let text = engine.recognize(&img).unwrap();
        // Blank image: engine must succeed; text may be empty.
        let _ = text;
        assert_eq!(engine.name(), "tesseract");
    }

    #[test]
    fn fixture_engine_name_is_stable() {
        let engine = FixtureOcrEngine::new();
        assert_eq!(engine.name(), "fixture");
    }
}
