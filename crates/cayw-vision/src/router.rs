//! VisionRouter facade: local pixel tools + cached OCR / describe.

use crate::cache::{AnswerCache, CacheKey};
use crate::image::{ImageBuffer, Region, content_sha256};
use crate::pixel::{
    ColorShare, PixelDiffReport, crop as pixel_crop, dominant_colors as pixel_colors,
    pixel_diff as pixel_compare,
};
use crate::types::{
    CropResult, DescribeRequest, DescribeResult, OcrRequest, OcrResult, VisionError,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Local / fixture OCR engine. Production hosts inject a real backend later.
pub trait OcrEngine: Send + Sync {
    fn name(&self) -> &str;
    fn recognize(&self, image: &ImageBuffer) -> Result<String, VisionError>;
}

/// Visual Q&A / describe backend. Each successful call is one billable request
/// unless served from the answer cache.
pub trait DescribeBackend: Send + Sync {
    fn describe(
        &self,
        image: &ImageBuffer,
        request: &DescribeRequest,
    ) -> Result<String, VisionError>;
    fn billed_requests(&self) -> u64;
}

/// Deterministic OCR over a content-hash → text map (offline fixtures).
#[derive(Debug, Default, Clone)]
pub struct FixtureOcrEngine {
    by_sha: HashMap<String, String>,
}

impl FixtureOcrEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, image_sha256: impl Into<String>, text: impl Into<String>) {
        self.by_sha.insert(image_sha256.into(), text.into());
    }

    pub fn with_mapping(
        pairs: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        let mut engine = Self::new();
        for (sha, text) in pairs {
            engine.insert(sha, text);
        }
        engine
    }
}

impl OcrEngine for FixtureOcrEngine {
    fn name(&self) -> &str {
        "fixture"
    }

    fn recognize(&self, image: &ImageBuffer) -> Result<String, VisionError> {
        self.by_sha.get(image.sha256()).cloned().ok_or_else(|| {
            VisionError::OcrFailed(format!("no fixture OCR text for image {}", image.sha256()))
        })
    }
}

/// Counting describe backend for offline tests / demos.
#[derive(Debug)]
pub struct CountingDescribeBackend {
    answers: Mutex<HashMap<(String, String), String>>,
    billed: AtomicU64,
}

impl CountingDescribeBackend {
    pub fn new() -> Self {
        Self {
            answers: Mutex::new(HashMap::new()),
            billed: AtomicU64::new(0),
        }
    }

    pub fn seed(&self, image_sha256: &str, question: &str, answer: impl Into<String>) {
        let q = content_sha256(question.as_bytes());
        self.answers
            .lock()
            .expect("describe map")
            .insert((image_sha256.to_string(), q), answer.into());
    }
}

impl Default for CountingDescribeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl DescribeBackend for CountingDescribeBackend {
    fn describe(
        &self,
        image: &ImageBuffer,
        request: &DescribeRequest,
    ) -> Result<String, VisionError> {
        let q = content_sha256(request.question.as_bytes());
        let key = (image.sha256().to_string(), q);
        let answer = self
            .answers
            .lock()
            .map_err(|_| VisionError::DescribeFailed("poisoned".into()))?
            .get(&key)
            .cloned()
            .ok_or_else(|| {
                VisionError::DescribeFailed(format!(
                    "no seeded answer for image {} question {}",
                    image.sha256(),
                    request.question
                ))
            })?;
        self.billed.fetch_add(1, Ordering::SeqCst);
        Ok(answer)
    }

    fn billed_requests(&self) -> u64 {
        self.billed.load(Ordering::SeqCst)
    }
}

/// Vision router: local pixel tools + cached OCR / describe entry points.
pub struct VisionRouter {
    ocr: Arc<dyn OcrEngine>,
    describe: Arc<dyn DescribeBackend>,
    cache: AnswerCache,
}

impl VisionRouter {
    pub fn new(ocr: Arc<dyn OcrEngine>, describe: Arc<dyn DescribeBackend>) -> Self {
        Self {
            ocr,
            describe,
            cache: AnswerCache::new(200),
        }
    }

    pub fn with_cache_capacity(
        ocr: Arc<dyn OcrEngine>,
        describe: Arc<dyn DescribeBackend>,
        max_entries: usize,
    ) -> Self {
        Self {
            ocr,
            describe,
            cache: AnswerCache::new(max_entries),
        }
    }

    pub fn cache_stats(&self) -> Result<crate::cache::CacheStats, VisionError> {
        self.cache.stats()
    }

    pub fn describe_billed_requests(&self) -> u64 {
        self.describe.billed_requests()
    }

    pub fn crop(&self, image: &ImageBuffer, region: Region) -> Result<CropResult, VisionError> {
        pixel_crop(image, region)
    }

    pub fn pixel_diff(
        &self,
        original: &ImageBuffer,
        rebuilt: &ImageBuffer,
        threshold: u8,
    ) -> Result<PixelDiffReport, VisionError> {
        pixel_compare(original, rebuilt, threshold)
    }

    pub fn colors(&self, image: &ImageBuffer, top: usize) -> Result<Vec<ColorShare>, VisionError> {
        pixel_colors(image, top)
    }

    /// OCR with answer cache. Cache hits do not call the OCR engine again.
    pub fn ocr(&self, image: &ImageBuffer, request: &OcrRequest) -> Result<OcrResult, VisionError> {
        let key = CacheKey {
            operation: "ocr".into(),
            image_sha256: image.sha256().to_string(),
            secondary_sha256: None,
            prompt_sha256: request
                .engine_hint
                .as_ref()
                .map(|h| content_sha256(h.as_bytes())),
        };
        if let Some(payload) = self.cache.get(&key)? {
            let mut result: OcrResult = serde_json::from_str(&payload)
                .map_err(|err| VisionError::OcrFailed(format!("corrupt cache payload: {err}")))?;
            result.from_cache = true;
            return Ok(result);
        }
        let text = self.ocr.recognize(image)?;
        let result = OcrResult {
            text,
            engine: self.ocr.name().to_string(),
            image_sha256: image.sha256().to_string(),
            from_cache: false,
        };
        self.cache
            .put(key, serde_json::to_string(&result).expect("ocr json"))?;
        Ok(result)
    }

    /// Describe / visual Q&A with answer cache. Cache hits report
    /// `billed_requests = 0` and do not increment the backend counter.
    pub fn describe(
        &self,
        image: &ImageBuffer,
        request: &DescribeRequest,
    ) -> Result<DescribeResult, VisionError> {
        if request.question.trim().is_empty() {
            return Err(VisionError::InvalidInput(
                "describe question must not be empty".into(),
            ));
        }
        let question_sha = content_sha256(request.question.as_bytes());
        let key = CacheKey {
            operation: if request.json {
                "describe_json".into()
            } else {
                "describe".into()
            },
            image_sha256: image.sha256().to_string(),
            secondary_sha256: None,
            prompt_sha256: Some(question_sha.clone()),
        };
        if let Some(payload) = self.cache.get(&key)? {
            let mut result: DescribeResult = serde_json::from_str(&payload).map_err(|err| {
                VisionError::DescribeFailed(format!("corrupt cache payload: {err}"))
            })?;
            result.from_cache = true;
            result.billed_requests = 0;
            return Ok(result);
        }
        let before = self.describe.billed_requests();
        let answer = self.describe.describe(image, request)?;
        let after = self.describe.billed_requests();
        let billed = after.saturating_sub(before);
        let result = DescribeResult {
            answer,
            image_sha256: image.sha256().to_string(),
            question_sha256: question_sha,
            from_cache: false,
            billed_requests: billed,
        };
        self.cache
            .put(key, serde_json::to_string(&result).expect("describe json"))?;
        Ok(result)
    }
}
