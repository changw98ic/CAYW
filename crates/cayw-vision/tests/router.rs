//! SLICE N / F11 smoke: stable OCR / pixel-diff + cache avoids re-billing.

use cayw_vision::*;
use std::sync::Arc;

fn sample_glyph() -> ImageBuffer {
    // 8×8: top-left 4×4 white block on black — deterministic fixture identity.
    let mut rgba = vec![0u8; 8 * 8 * 4];
    for y in 0..4 {
        for x in 0..4 {
            let idx = (y * 8 + x) * 4;
            rgba[idx..idx + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
    }
    ImageBuffer::from_rgba8(8, 8, rgba).unwrap()
}

fn router_with_fixture(img: &ImageBuffer) -> (VisionRouter, Arc<CountingDescribeBackend>) {
    let mut ocr = FixtureOcrEngine::new();
    ocr.insert(img.sha256(), "CAYW");
    let describe = Arc::new(CountingDescribeBackend::new());
    describe.seed(img.sha256(), "what text?", "The glyph reads CAYW.");
    let router = VisionRouter::new(Arc::new(ocr), describe.clone());
    (router, describe)
}

#[test]
fn fixed_sample_ocr_is_stable() {
    let img = sample_glyph();
    let (router, _) = router_with_fixture(&img);
    let first = router.ocr(&img, &OcrRequest { engine_hint: None }).unwrap();
    let second = router.ocr(&img, &OcrRequest { engine_hint: None }).unwrap();
    assert_eq!(first.text, "CAYW");
    assert_eq!(second.text, "CAYW");
    assert_eq!(first.image_sha256, img.sha256());
    assert!(!first.from_cache);
    assert!(second.from_cache);
}

#[test]
fn fixed_sample_pixel_diff_is_stable() {
    let a = ImageBuffer::solid(16, 16, [10, 20, 30, 255]).unwrap();
    let mut rgba = a.rgba().to_vec();
    // Flip one pixel far beyond threshold 16.
    rgba[0] = 200;
    let b = ImageBuffer::from_rgba8(16, 16, rgba).unwrap();
    let router = VisionRouter::new(
        Arc::new(FixtureOcrEngine::new()),
        Arc::new(CountingDescribeBackend::new()),
    );
    let r1 = router.pixel_diff(&a, &b, 16).unwrap();
    let r2 = router.pixel_diff(&a, &b, 16).unwrap();
    assert_eq!(r1.differing_pixels, 1);
    assert_eq!(r1.total_pixels, 256);
    assert_eq!(r1.differing_pixels, r2.differing_pixels);
    assert_eq!(
        format!("{:.12}", r1.diff_ratio),
        format!("{:.12}", r2.diff_ratio)
    );
    assert!(!r1.worst_regions.is_empty());
}

#[test]
fn describe_cache_hit_does_not_rebill() {
    let img = sample_glyph();
    let (router, backend) = router_with_fixture(&img);
    let req = DescribeRequest {
        question: "what text?".into(),
        json: false,
    };
    let first = router.describe(&img, &req).unwrap();
    assert!(!first.from_cache);
    assert_eq!(first.billed_requests, 1);
    assert_eq!(backend.billed_requests(), 1);

    let second = router.describe(&img, &req).unwrap();
    assert!(second.from_cache);
    assert_eq!(second.billed_requests, 0);
    assert_eq!(backend.billed_requests(), 1, "cache hit must not re-bill");
    assert_eq!(first.answer, second.answer);

    let stats = router.cache_stats().unwrap();
    assert!(stats.hits >= 1);
    assert!(stats.misses >= 1);
}

#[test]
fn crop_and_colors_are_local() {
    let img = sample_glyph();
    let router = VisionRouter::new(
        Arc::new(FixtureOcrEngine::new()),
        Arc::new(CountingDescribeBackend::new()),
    );
    let cropped = router.crop(&img, Region::new(0, 0, 4, 4).unwrap()).unwrap();
    assert_eq!(cropped.width, 4);
    assert_eq!(cropped.height, 4);
    let colors = router.colors(&img, 2).unwrap();
    assert!(!colors.is_empty());
    assert_eq!(colors[0].hex, "#ffffff");
}

#[test]
fn dimension_mismatch_is_explicit() {
    let a = ImageBuffer::solid(2, 2, [0, 0, 0, 255]).unwrap();
    let b = ImageBuffer::solid(3, 3, [0, 0, 0, 255]).unwrap();
    let router = VisionRouter::new(
        Arc::new(FixtureOcrEngine::new()),
        Arc::new(CountingDescribeBackend::new()),
    );
    let err = router.pixel_diff(&a, &b, 0).unwrap_err();
    assert!(matches!(err, VisionError::DimensionMismatch(_)));
}
