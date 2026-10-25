//! Vision router for CAYW image tools (F11 / SLICE N first cut).
//!
//! Host-owned local pixel ops (crop / pixel-diff / colors), pluggable OCR and
//! describe backends, and a content-hash answer cache so cache hits do not
//! re-bill vision backends. This crate does **not** call cloud providers, open
//! desktop capture, or wire Host/Runtime tool names — those land in later
//! slices once result handles (F01/F03) and model backends are connected.
//!
//! Design reference (not a code transplant):
//! [ysr666/dsh-vision-router](https://github.com/ysr666/dsh-vision-router).
//! See `docs/sources/f11-vision-router.md`.
#![forbid(unsafe_code)]

mod cache;
mod image;
mod pixel;
mod router;
mod types;

pub use cache::{AnswerCache, CacheKey, CacheStats};
pub use image::{ImageBuffer, Region, content_sha256};
pub use pixel::{ColorShare, PixelDiffReport, WorstRegion, crop, dominant_colors, pixel_diff};
pub use router::{
    CountingDescribeBackend, DescribeBackend, FixtureOcrEngine, OcrEngine, VisionRouter,
};
pub use types::*;

/// Wire / document protocol for this first-cut vision surface.
pub const VISION_PROTOCOL_VERSION: u32 = 1;
