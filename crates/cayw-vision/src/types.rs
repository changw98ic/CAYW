//! Shared vision request / result types.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors from the vision router and local pixel tools.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum VisionError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("image dimensions do not match: {0}")]
    DimensionMismatch(String),
    #[error("ocr backend failed: {0}")]
    OcrFailed(String),
    #[error("describe backend failed: {0}")]
    DescribeFailed(String),
    #[error("cache poisoned")]
    CachePoisoned,
    #[error("cancelled")]
    Cancelled,
    #[error("image storage failed: {message}")]
    Storage {
        message: String,
        outcome_unknown: bool,
    },
}

/// OCR request over one image buffer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrRequest {
    /// Optional engine hint (`fixture`, `local`, `vision`, …). First cut uses
    /// whatever engine the router was constructed with; the hint is recorded
    /// in the result for diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_hint: Option<String>,
}

/// OCR transcription result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrResult {
    pub text: String,
    pub engine: String,
    pub image_sha256: String,
    pub from_cache: bool,
}

/// Visual Q&A / describe request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DescribeRequest {
    pub question: String,
    /// When true, backends that support it return structured JSON evidence.
    #[serde(default)]
    pub json: bool,
}

/// Visual Q&A / describe result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DescribeResult {
    pub answer: String,
    pub image_sha256: String,
    pub question_sha256: String,
    pub from_cache: bool,
    /// Billable backend invocations performed for this call (0 on cache hit).
    pub billed_requests: u64,
}

/// Crop result returning a new buffer identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CropResult {
    pub width: u32,
    pub height: u32,
    pub image_sha256: String,
    /// RGBA8 bytes (row-major).
    pub rgba: Vec<u8>,
}
