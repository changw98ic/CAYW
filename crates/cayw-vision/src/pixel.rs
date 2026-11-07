//! Local pixel tools: crop, pixel-diff, dominant colors.
//!
//! These operations never call a vision model or leave the process.

use crate::image::{ImageBuffer, Region};
use crate::types::{CropResult, VisionError};
use serde::{Deserialize, Serialize};

/// One cell in the 8×8 worst-diff grid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorstRegion {
    pub grid_x: u32,
    pub grid_y: u32,
    pub differing_pixels: u64,
    pub cell_pixels: u64,
}

/// Per-pixel comparison report (vision_pixel_diff shape).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PixelDiffReport {
    pub width: u32,
    pub height: u32,
    pub differing_pixels: u64,
    pub total_pixels: u64,
    pub diff_ratio: f64,
    pub threshold: u8,
    pub worst_regions: Vec<WorstRegion>,
    pub original_sha256: String,
    pub rebuilt_sha256: String,
}

/// Dominant color share.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColorShare {
    pub hex: String,
    pub share: f64,
    pub pixels: u64,
}

/// Crop `region` out of `image` (original-pixel coordinates).
pub fn crop(image: &ImageBuffer, region: Region) -> Result<CropResult, VisionError> {
    if region.x2() > image.width() || region.y2() > image.height() {
        return Err(VisionError::InvalidInput(format!(
            "region {:?}/{}x{} exceeds image {}x{}",
            (region.x, region.y),
            region.width,
            region.height,
            image.width(),
            image.height()
        )));
    }
    let mut rgba = Vec::with_capacity((region.width as usize) * (region.height as usize) * 4);
    for y in region.y..region.y2() {
        for x in region.x..region.x2() {
            rgba.extend_from_slice(&image.pixel(x, y));
        }
    }
    let cropped = ImageBuffer::from_rgba8(region.width, region.height, rgba)?;
    Ok(CropResult {
        width: cropped.width(),
        height: cropped.height(),
        image_sha256: cropped.sha256().to_string(),
        rgba: cropped.rgba().to_vec(),
    })
}

/// Compare two same-size images. A pixel differs when any channel delta exceeds
/// `threshold` (inclusive of alpha). Worst regions are the top cells of an
/// 8×8 grid ranked by differing pixel count.
pub fn pixel_diff(
    original: &ImageBuffer,
    rebuilt: &ImageBuffer,
    threshold: u8,
) -> Result<PixelDiffReport, VisionError> {
    if original.width() != rebuilt.width() || original.height() != rebuilt.height() {
        return Err(VisionError::DimensionMismatch(format!(
            "{}x{} vs {}x{}",
            original.width(),
            original.height(),
            rebuilt.width(),
            rebuilt.height()
        )));
    }
    let w = original.width();
    let h = original.height();
    let total = (w as u64) * (h as u64);
    let mut differing = 0u64;
    let mut grid = [[0u64; 8]; 8];
    let cell_w = ((w as f64) / 8.0).max(1.0);
    let cell_h = ((h as f64) / 8.0).max(1.0);

    for y in 0..h {
        for x in 0..w {
            let a = original.pixel(x, y);
            let b = rebuilt.pixel(x, y);
            let mut diff = false;
            for i in 0..4 {
                let d = a[i].abs_diff(b[i]);
                if d > threshold {
                    diff = true;
                    break;
                }
            }
            if diff {
                differing += 1;
                let gx = ((x as f64) / cell_w).floor() as usize;
                let gy = ((y as f64) / cell_h).floor() as usize;
                let gx = gx.min(7);
                let gy = gy.min(7);
                grid[gy][gx] += 1;
            }
        }
    }

    let mut worst: Vec<WorstRegion> = Vec::new();
    for gy in 0..8u32 {
        for gx in 0..8u32 {
            let count = grid[gy as usize][gx as usize];
            if count == 0 {
                continue;
            }
            let x0 = ((gx as f64) * cell_w).floor() as u32;
            let y0 = ((gy as f64) * cell_h).floor() as u32;
            let x1 = (((gx as f64 + 1.0) * cell_w).floor() as u32).min(w);
            let y1 = (((gy as f64 + 1.0) * cell_h).floor() as u32).min(h);
            let cell_pixels = ((x1 - x0) as u64) * ((y1 - y0) as u64);
            worst.push(WorstRegion {
                grid_x: gx,
                grid_y: gy,
                differing_pixels: count,
                cell_pixels,
            });
        }
    }
    worst.sort_by_key(|b| std::cmp::Reverse(b.differing_pixels));
    worst.truncate(8);

    let diff_ratio = if total == 0 {
        0.0
    } else {
        (differing as f64) / (total as f64)
    };

    Ok(PixelDiffReport {
        width: w,
        height: h,
        differing_pixels: differing,
        total_pixels: total,
        diff_ratio,
        threshold,
        worst_regions: worst,
        original_sha256: original.sha256().to_string(),
        rebuilt_sha256: rebuilt.sha256().to_string(),
    })
}

/// Dominant opaque RGB colors (alpha ignored for grouping; zero-alpha pixels skipped).
pub fn dominant_colors(image: &ImageBuffer, top: usize) -> Result<Vec<ColorShare>, VisionError> {
    if top == 0 {
        return Err(VisionError::InvalidInput("top must be at least 1".into()));
    }
    use std::collections::HashMap;
    let mut counts: HashMap<[u8; 3], u64> = HashMap::new();
    let mut counted = 0u64;
    for y in 0..image.height() {
        for x in 0..image.width() {
            let px = image.pixel(x, y);
            if px[3] == 0 {
                continue;
            }
            *counts.entry([px[0], px[1], px[2]]).or_insert(0) += 1;
            counted += 1;
        }
    }
    let mut ranked: Vec<_> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(top);
    let denom = counted.max(1) as f64;
    Ok(ranked
        .into_iter()
        .map(|(rgb, pixels)| ColorShare {
            hex: format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]),
            share: (pixels as f64) / denom,
            pixels,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solid_images_diff_is_stable() {
        let a = ImageBuffer::solid(4, 4, [255, 0, 0, 255]).unwrap();
        let b = ImageBuffer::solid(4, 4, [255, 0, 0, 255]).unwrap();
        let report = pixel_diff(&a, &b, 16).unwrap();
        assert_eq!(report.differing_pixels, 0);
        assert_eq!(report.diff_ratio, 0.0);

        let c = ImageBuffer::solid(4, 4, [0, 255, 0, 255]).unwrap();
        let report = pixel_diff(&a, &c, 16).unwrap();
        assert_eq!(report.differing_pixels, 16);
        assert!((report.diff_ratio - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn crop_preserves_pixels() {
        let mut rgba = vec![0u8; 8 * 8 * 4];
        // Mark (2,3) as white.
        let idx = (3 * 8 + 2) * 4;
        rgba[idx..idx + 4].copy_from_slice(&[255, 255, 255, 255]);
        let img = ImageBuffer::from_rgba8(8, 8, rgba).unwrap();
        let cropped = crop(&img, Region::new(2, 3, 1, 1).unwrap()).unwrap();
        assert_eq!(cropped.width, 1);
        assert_eq!(cropped.height, 1);
        assert_eq!(&cropped.rgba[..], &[255, 255, 255, 255]);
    }
}
