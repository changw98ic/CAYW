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
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tempfile::NamedTempFile;

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

/// Build a curl `@file` argument. The file contents stay out of argv.
fn file_argument(path: &Path) -> OsString {
    let mut arg = OsString::from("@");
    arg.push(path);
    arg
}

/// `NamedTempFile` is created 0600, but enforce it explicitly so a host umask
/// or a future tempfile default cannot expose the request body or token.
fn private_file(path: &Path) -> Result<(), VisionError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|err| {
            VisionError::DescribeFailed(format!("restrict describe temp file: {err}"))
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Remove the configured key from a diagnostic and cap its size. The caller
/// must never pass response stdout here: it can echo the request body.
fn redact_secret(text: &str, key: Option<&str>) -> String {
    let mut redacted = match key.filter(|key| !key.is_empty()) {
        Some(key) => text.replace(key, "[redacted]"),
        None => text.to_string(),
    };
    const MAX_DIAGNOSTIC_BYTES: usize = 1024;
    if redacted.len() > MAX_DIAGNOSTIC_BYTES {
        redacted.truncate(redacted.floor_char_boundary(MAX_DIAGNOSTIC_BYTES));
        redacted.push('…');
    }
    redacted
}

/// HTTP describe backend. POSTs JSON to `CAYW_VISION_DESCRIBE_URL`.
///
/// Request body: `{ "width", "height", "rgba_base64", "image_sha256", "question", "json" }`.
/// Response JSON must include `"answer": string`.
pub struct HttpDescribeBackend {
    url: String,
    key: Option<String>,
    max_response_bytes: usize,
    billed: AtomicU64,
    curl_program: PathBuf,
}

impl std::fmt::Debug for HttpDescribeBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpDescribeBackend")
            .field("url", &self.url)
            .field("key", &self.key.as_ref().map(|_| "[redacted]"))
            .field("max_response_bytes", &self.max_response_bytes)
            .field("billed", &self.billed.load(Ordering::SeqCst))
            .field("curl_program", &self.curl_program)
            .finish()
    }
}

impl HttpDescribeBackend {
    pub fn new(url: impl Into<String>, key: Option<String>, max_response_bytes: usize) -> Self {
        Self {
            url: url.into(),
            key,
            max_response_bytes,
            billed: AtomicU64::new(0),
            curl_program: PathBuf::from("curl"),
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
        if let Some(key) = &self.key
            && (key.contains('\n') || key.contains('\r'))
        {
            return Err(VisionError::DescribeFailed(
                "describe key contains a line break".into(),
            ));
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
        // The request body and the bearer token never go into argv. Both are
        // written to 0600 temporary files and passed to curl as `@file`
        // arguments, so `ps` cannot observe the image or the credential.
        // Keeping the NamedTempFile values alive until curl exits also removes
        // them on success, failure and cancellation.
        let mut body_file = NamedTempFile::new().map_err(|err| {
            VisionError::DescribeFailed(format!("create describe body file: {err}"))
        })?;
        body_file.write_all(body_str.as_bytes()).map_err(|err| {
            VisionError::DescribeFailed(format!("write describe body file: {err}"))
        })?;
        private_file(body_file.path())?;
        let header_file = match &self.key {
            Some(key) => {
                let mut file = NamedTempFile::new().map_err(|err| {
                    VisionError::DescribeFailed(format!("create describe header file: {err}"))
                })?;
                writeln!(file, "authorization: Bearer {key}").map_err(|err| {
                    VisionError::DescribeFailed(format!("write describe header file: {err}"))
                })?;
                private_file(file.path())?;
                Some(file)
            }
            None => None,
        };
        // Prefer curl so cayw-vision stays free of a blocking HTTP client feature
        // in the default build; CI never sets the URL and never reaches here.
        // --max-time 60 is a ceiling; Host cancel kills curl earlier via KillOnDrop.
        let mut cmd = Command::new(&self.curl_program);
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
            "--data-binary",
        ]);
        cmd.arg(file_argument(body_file.path()));
        if let Some(file) = &header_file {
            cmd.arg("-H").arg(file_argument(file.path()));
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
            // stdout can contain the response body or an echo of the request;
            // do not put it into a diagnostic. Redact the key from stderr in
            // case the configured backend echoes it.
            let stderr = redact_secret(
                &String::from_utf8_lossy(&output.stderr),
                self.key.as_deref(),
            );
            return Err(VisionError::DescribeFailed(format!(
                "describe HTTP failed ({}): {stderr}",
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

    #[test]
    fn http_describe_debug_redacts_key() {
        let backend = HttpDescribeBackend::new(
            "http://example.invalid/describe",
            Some("sk-debug-secret".into()),
            1024,
        );
        let debug = format!("{backend:?}");
        assert!(!debug.contains("sk-debug-secret"), "{debug}");
        assert!(debug.contains("[redacted]"), "{debug}");
    }
}

#[cfg(all(test, unix))]
#[derive(Clone, Copy)]
enum FakeCurlMode {
    Success,
    Fail,
    Sleep,
    HoldBeforeHeaderMode,
}

/// Fake curl that records argv, copies the `@file` payloads it receives, and
/// records the temp-file modes and sizes before returning a fixed answer.
#[cfg(all(test, unix))]
struct FakeCurl {
    _dir: tempfile::TempDir,
    program: PathBuf,
    argv: PathBuf,
    body: PathBuf,
    header: PathBuf,
    paths: PathBuf,
    modes: PathBuf,
    sizes: PathBuf,
    recording_complete: PathBuf,
    header_mode_pending: PathBuf,
}

#[cfg(all(test, unix))]
impl FakeCurl {
    fn new(mode: FakeCurlMode) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("curl");
        let argv = dir.path().join("argv");
        let body = dir.path().join("body");
        let header = dir.path().join("header");
        let paths = dir.path().join("paths");
        let modes = dir.path().join("modes");
        let sizes = dir.path().join("sizes");
        let recording_complete = dir.path().join("recording-complete");
        let header_mode_pending = dir.path().join("header-mode-pending");
        let before_header_mode = match mode {
            // Hold the exact race window with shell builtins only: killing the
            // direct child leaves no helper process holding the output pipes.
            FakeCurlMode::HoldBeforeHeaderMode => {
                ": > \"$header_mode_pending\"\nwhile :; do :; done"
            }
            _ => "",
        };
        let behaviour = match mode {
            FakeCurlMode::Success => "printf '{\"answer\":\"ok\"}'",
            FakeCurlMode::Fail => {
                "printf 'backend rejected: ' >&2\ncat \"$header_path\" >&2\nexit 22"
            }
            // `exec` keeps the shell pid as the direct child, so cancellation
            // kills the sleeper instead of orphaning a grandchild that holds
            // the stdout/stderr pipes open.
            FakeCurlMode::Sleep | FakeCurlMode::HoldBeforeHeaderMode => "exec sleep 30",
        };
        let script = format!(
            r#"#!/bin/sh
argv_file='{argv}'
body_copy='{body}'
header_copy='{header}'
paths_file='{paths}'
modes_file='{modes}'
sizes_file='{sizes}'
recording_complete='{recording_complete}'
header_mode_pending='{header_mode_pending}'
: > "$argv_file"
expect_body=0
expect_header=0
body_path=
header_path=
while [ "$#" -gt 0 ]; do
  printf '%s\n' "$1" >> "$argv_file"
  if [ "$expect_body" = 1 ]; then
    case "$1" in
      @*) body_path=$(printf '%s' "$1" | cut -c2-); cp "$body_path" "$body_copy";;
    esac
    expect_body=0
  elif [ "$expect_header" = 1 ]; then
    case "$1" in
      @*) header_path=$(printf '%s' "$1" | cut -c2-); cp "$header_path" "$header_copy";;
    esac
    expect_header=0
  elif [ "$1" = "--data-binary" ]; then
    expect_body=1
  elif [ "$1" = "-H" ]; then
    expect_header=1
  fi
  shift
done
if [ -n "$body_path" ]; then
  printf '%s\n' "$body_path" >> "$paths_file"
  ls -l "$body_path" | cut -c1-10 >> "$modes_file"
  wc -c < "$body_path" | tr -d ' ' >> "$sizes_file"
fi
if [ -n "$header_path" ]; then
  printf '%s\n' "$header_path" >> "$paths_file"
  {before_header_mode}
  ls -l "$header_path" | cut -c1-10 >> "$modes_file"
fi
: > "$recording_complete"
{behaviour}
"#,
            argv = argv.display(),
            body = body.display(),
            header = header.display(),
            paths = paths.display(),
            modes = modes.display(),
            sizes = sizes.display(),
            recording_complete = recording_complete.display(),
            header_mode_pending = header_mode_pending.display(),
            before_header_mode = before_header_mode,
            behaviour = behaviour,
        );
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            _dir: dir,
            program,
            argv,
            body,
            header,
            paths,
            modes,
            sizes,
            recording_complete,
            header_mode_pending,
        }
    }

    // Return on timeout so callers can cancel and join before asserting.
    fn wait_for_marker(&self, marker: &Path) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !marker.exists() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        true
    }

    fn backend(&self, key: Option<&str>) -> HttpDescribeBackend {
        let mut backend = HttpDescribeBackend::new(
            "http://127.0.0.1:9/describe",
            key.map(str::to_string),
            DEFAULT_MAX_DESCRIBE_RESPONSE_BYTES,
        );
        backend.curl_program = self.program.clone();
        backend
    }

    fn argv(&self) -> String {
        std::fs::read_to_string(&self.argv).unwrap_or_default()
    }

    fn body(&self) -> String {
        std::fs::read_to_string(&self.body).unwrap_or_default()
    }

    fn header(&self) -> String {
        std::fs::read_to_string(&self.header).unwrap_or_default()
    }

    fn paths(&self) -> Vec<PathBuf> {
        std::fs::read_to_string(&self.paths)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect()
    }

    fn modes(&self) -> Vec<String> {
        std::fs::read_to_string(&self.modes)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn sizes(&self) -> Vec<u64> {
        std::fs::read_to_string(&self.sizes)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.parse().ok())
            .collect()
    }
}

#[cfg(all(test, unix))]
#[test]
fn http_describe_keeps_body_and_key_out_of_argv() {
    let fake = FakeCurl::new(FakeCurlMode::Success);
    let key = "sk-test-very-secret-123";
    let backend = fake.backend(Some(key));
    let image = ImageBuffer::solid(8, 8, [10, 20, 30, 255]).unwrap();
    let request = DescribeRequest {
        question: "what is this?".into(),
        json: true,
    };

    let answer = backend
        .describe(&image, &request, &CancellationToken::new())
        .unwrap();
    assert_eq!(answer, "ok");

    let argv = fake.argv();
    assert!(!argv.contains(key), "key leaked into argv: {argv}");
    assert!(
        !argv.contains(&base64_encode(image.rgba())),
        "image leaked into argv"
    );
    assert!(!argv.contains("rgba_base64"));
    assert!(!argv.contains("what is this?"));
    assert!(!argv.lines().any(|line| line == "-d"));
    assert!(argv.contains("--data-binary"));
    assert!(argv.contains("-H"));

    let body = fake.body();
    assert!(body.contains("\"rgba_base64\""));
    assert!(body.contains("what is this?"));
    assert_eq!(
        fake.header(),
        format!("authorization: Bearer {key}\n"),
        "header file must carry the bearer token"
    );
    assert_eq!(fake.modes(), vec!["-rw-------", "-rw-------"]);
}

#[cfg(all(test, unix))]
#[test]
fn http_describe_accepts_body_above_macos_arg_max() {
    let fake = FakeCurl::new(FakeCurlMode::Success);
    let backend = fake.backend(Some("k"));
    // 1024x1024 RGBA is 4 MiB raw. Base64 JSON is ~5.6 MiB, above the
    // 1,048,576-byte macOS ARG_MAX measured for this workspace.
    let image = ImageBuffer::solid(1024, 1024, [1, 2, 3, 255]).unwrap();
    let request = DescribeRequest {
        question: "large".into(),
        json: false,
    };
    backend
        .describe(&image, &request, &CancellationToken::new())
        .unwrap();
    let sizes = fake.sizes();
    assert_eq!(sizes.len(), 1);
    assert!(
        sizes[0] > 1024 * 1024,
        "body file was only {} bytes",
        sizes[0]
    );
    assert!(!fake.argv().contains("rgba_base64"));
}

#[cfg(all(test, unix))]
#[test]
fn http_describe_failure_redacts_key_and_cleans_temp_files() {
    let fake = FakeCurl::new(FakeCurlMode::Fail);
    let key = "sk-failure-secret-456";
    let backend = fake.backend(Some(key));
    let image = ImageBuffer::solid(2, 2, [0, 0, 0, 255]).unwrap();
    let request = DescribeRequest {
        question: "q".into(),
        json: false,
    };

    let error = backend
        .describe(&image, &request, &CancellationToken::new())
        .unwrap_err()
        .to_string();
    assert!(error.contains("describe HTTP failed"), "{error}");
    assert!(!error.contains(key), "key leaked into error: {error}");
    assert!(error.contains("[redacted]"), "{error}");
    assert_eq!(fake.modes(), vec!["-rw-------", "-rw-------"]);

    let paths = fake.paths();
    assert_eq!(paths.len(), 2, "expected body and header temp files");
    for path in paths {
        assert!(!path.exists(), "temp file survived failure: {path:?}");
    }
}

#[cfg(all(test, unix))]
#[test]
fn http_describe_cancel_cleans_temp_files() {
    let fake = FakeCurl::new(FakeCurlMode::Sleep);
    let backend = Arc::new(fake.backend(Some("sk-cancel-secret-789")));
    let image = Arc::new(ImageBuffer::solid(2, 2, [1, 2, 3, 255]).unwrap());
    let cancel = CancellationToken::new();
    let thread_cancel = cancel.clone();
    let handle = std::thread::spawn(move || {
        backend.describe(
            &image,
            &DescribeRequest {
                question: "q".into(),
                json: false,
            },
            &thread_cancel,
        )
    });

    // Paths are published before their modes. Only the final marker proves
    // that all permission evidence is recorded before cancellation kills curl.
    let recording_complete = fake.wait_for_marker(&fake.recording_complete);
    cancel.cancel();

    let result = handle.join().unwrap();
    assert!(recording_complete, "fake curl did not finish recording");
    assert!(matches!(result, Err(VisionError::Cancelled)));
    assert_eq!(fake.modes(), vec!["-rw-------", "-rw-------"]);
    let paths = fake.paths();
    assert_eq!(paths.len(), 2);
    for path in paths {
        assert!(!path.exists(), "temp file survived cancel: {path:?}");
    }
}

#[cfg(all(test, unix))]
#[test]
fn http_describe_cancel_between_path_and_mode_records() {
    let fake = FakeCurl::new(FakeCurlMode::HoldBeforeHeaderMode);
    let backend = fake.backend(Some("sk-cancel-partial-recording"));
    let image = ImageBuffer::solid(2, 2, [1, 2, 3, 255]).unwrap();
    let cancel = CancellationToken::new();
    let thread_cancel = cancel.clone();
    let handle = std::thread::spawn(move || {
        backend.describe(
            &image,
            &DescribeRequest {
                question: "q".into(),
                json: false,
            },
            &thread_cancel,
        )
    });

    // Deterministically expose the old readiness condition: both paths are
    // visible, but the header mode cannot be written until after this hold.
    let header_mode_pending = fake.wait_for_marker(&fake.header_mode_pending);
    let paths_before_cancel = fake.paths();
    let modes_before_cancel = fake.modes();
    let ready_before_cancel = fake.recording_complete.exists();
    // Always reap the worker, including when the readiness wait times out.
    cancel.cancel();
    let result = handle.join().unwrap();

    assert!(
        header_mode_pending,
        "fake curl did not reach recording hold"
    );
    assert_eq!(paths_before_cancel.len(), 2);
    assert_eq!(modes_before_cancel, vec!["-rw-------"]);
    assert!(!ready_before_cancel, "partial recording must not be ready");
    assert!(matches!(result, Err(VisionError::Cancelled)));
    assert_eq!(fake.modes(), vec!["-rw-------"]);
    assert!(!fake.recording_complete.exists());
    for path in paths_before_cancel {
        assert!(!path.exists(), "temp file survived cancel: {path:?}");
    }
}

#[cfg(all(test, unix))]
#[test]
fn http_describe_rejects_control_characters_in_key_without_echo() {
    let fake = FakeCurl::new(FakeCurlMode::Success);
    let backend = fake.backend(Some("sk-secret\ninjected: header"));
    let image = ImageBuffer::solid(1, 1, [0, 0, 0, 255]).unwrap();
    let request = DescribeRequest {
        question: "q".into(),
        json: false,
    };

    let error = backend
        .describe(&image, &request, &CancellationToken::new())
        .unwrap_err()
        .to_string();
    assert!(error.contains("line break"), "{error}");
    assert!(!error.contains("sk-secret"), "{error}");
    assert!(!fake.argv().contains("--data-binary"));
    assert!(fake.paths().is_empty());
}
