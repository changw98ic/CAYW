//! Offline demo: OCR + pixel-diff + describe cache (no network).

use cayw_ai::CancellationToken;
use cayw_vision::*;
use std::sync::Arc;

fn main() {
    let mut rgba = vec![0u8; 8 * 8 * 4];
    for y in 0..4 {
        for x in 0..4 {
            let idx = (y * 8 + x) * 4;
            rgba[idx..idx + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
    }
    let img = ImageBuffer::from_rgba8(8, 8, rgba).expect("fixture");
    let twin = ImageBuffer::solid(8, 8, [255, 255, 255, 255]).expect("solid");

    let mut ocr = FixtureOcrEngine::new();
    ocr.insert(img.sha256(), "CAYW");
    let describe = Arc::new(CountingDescribeBackend::new());
    describe.seed(img.sha256(), "label?", "white block on black");
    let router = VisionRouter::new(Arc::new(ocr), describe.clone());

    let ocr_hit = router
        .ocr(
            &img,
            &OcrRequest { engine_hint: None },
            &CancellationToken::new(),
        )
        .expect("ocr");
    println!(
        "ocr text={} from_cache={}",
        ocr_hit.text, ocr_hit.from_cache
    );

    let diff = router.pixel_diff(&img, &twin, 16).expect("diff");
    println!(
        "pixel_diff differing={}/{} ratio={:.4}",
        diff.differing_pixels, diff.total_pixels, diff.diff_ratio
    );

    let q = DescribeRequest {
        question: "label?".into(),
        json: false,
    };
    let d1 = router
        .describe(&img, &q, &CancellationToken::new())
        .expect("describe");
    let d2 = router
        .describe(&img, &q, &CancellationToken::new())
        .expect("describe cached");
    println!(
        "describe answer={:?} billed_first={} billed_second={} backend_total={}",
        d1.answer,
        d1.billed_requests,
        d2.billed_requests,
        describe.billed_requests()
    );
    assert!(d2.from_cache);
    assert_eq!(d2.billed_requests, 0);
}
