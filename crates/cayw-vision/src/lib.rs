//! Vision router for CAYW image tools (F11 / SLICE N).
//!
//! Host-owned local pixel ops (crop / pixel-diff / colors), pluggable OCR and
//! describe backends, and a content-hash answer cache so cache hits do not
//! re-bill vision backends.
//!
//! **Backends:** [`TesseractOcrEngine`] shells out to the system `tesseract`
//! CLI when installed. [`HttpDescribeBackend`] POSTs when
//! `CAYW_VISION_DESCRIBE_URL` is set. Otherwise [`FixtureOcrEngine`] /
//! [`CountingDescribeBackend`] keep CI offline and dependency-free
//! (`CAYW_VISION_OCR=fixture` forces the fixture OCR path).
//!
//! Host / Runtime expose `vision.crop` / `vision.pixel_diff` / `vision.colors` /
//! `vision.ocr` / `vision.describe` via [`definitions`] and [`VisionService`].
//! OCR/describe subprocesses honor Host [`cayw_ai::CancellationToken`] (KillOnDrop).
//! Desktop capture, element locate, and cloud model failover remain out of scope.
//!
//! Design reference (not a code transplant):
//! [ysr666/dsh-vision-router](https://github.com/ysr666/dsh-vision-router).
//! See `docs/sources/f11-vision-router.md`.
#![forbid(unsafe_code)]

mod backends;
mod cache;
mod catalog;
mod image;
mod pixel;
mod process;
mod router;
mod service;
mod types;

pub use backends::{
    ENV_DESCRIBE_KEY, ENV_DESCRIBE_URL, ENV_OCR, HttpDescribeBackend, TesseractOcrEngine,
    resolve_describe_backend, resolve_ocr_engine,
};
pub use cache::{AnswerCache, CacheKey, CacheStats};
pub use image::{ImageBuffer, Region, content_sha256};
pub use pixel::{ColorShare, PixelDiffReport, WorstRegion, crop, dominant_colors, pixel_diff};
pub use router::{
    CountingDescribeBackend, DescribeBackend, FixtureOcrEngine, OcrEngine, VisionRouter,
};
pub use service::{Limits, VisionService, definitions, execute};
pub use types::*;

/// Wire / document protocol for this vision surface.
pub const VISION_PROTOCOL_VERSION: u32 = 1;
