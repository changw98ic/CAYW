//! Image buffers and content identity.

use crate::types::VisionError;
use sha2::{Digest, Sha256};

/// SHA-256 hex digest (lowercase) of arbitrary bytes.
pub fn content_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Inclusive-exclusive pixel rectangle in original image coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Region {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Result<Self, VisionError> {
        if width == 0 || height == 0 {
            return Err(VisionError::InvalidInput(
                "region width and height must be positive".into(),
            ));
        }
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }

    pub fn x2(self) -> u32 {
        self.x.saturating_add(self.width)
    }

    pub fn y2(self) -> u32 {
        self.y.saturating_add(self.height)
    }
}

/// Host-canonical RGBA8 raster (8-bit sRGB / sRGBA, row-major).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageBuffer {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    sha256: String,
}

impl ImageBuffer {
    /// Build from raw RGBA8 bytes. Length must equal `width * height * 4`.
    pub fn from_rgba8(width: u32, height: u32, rgba: Vec<u8>) -> Result<Self, VisionError> {
        if width == 0 || height == 0 {
            return Err(VisionError::InvalidInput(
                "image width and height must be positive".into(),
            ));
        }
        let expected = (width as u64)
            .checked_mul(height as u64)
            .and_then(|px| px.checked_mul(4))
            .ok_or_else(|| VisionError::InvalidInput("image dimensions overflow".into()))?;
        if rgba.len() as u64 != expected {
            return Err(VisionError::InvalidInput(format!(
                "rgba length {} does not match {}x{} (need {expected})",
                rgba.len(),
                width,
                height
            )));
        }
        // Identity covers dimensions + pixels so same pixels at different
        // sizes never collide, and pure-pixel ops stay content-addressed.
        let mut identity = Vec::with_capacity(8 + rgba.len());
        identity.extend_from_slice(&width.to_be_bytes());
        identity.extend_from_slice(&height.to_be_bytes());
        identity.extend_from_slice(&rgba);
        let sha256 = content_sha256(&identity);
        Ok(Self {
            width,
            height,
            rgba,
            sha256,
        })
    }

    /// Solid-color fixture helper (used by offline tests / examples).
    pub fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Result<Self, VisionError> {
        let px = (width as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| VisionError::InvalidInput("image dimensions overflow".into()))?;
        let mut buf = Vec::with_capacity(px.saturating_mul(4));
        for _ in 0..px {
            buf.extend_from_slice(&rgba);
        }
        Self::from_rgba8(width, height, buf)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub(crate) fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let idx = ((y as usize) * (self.width as usize) + (x as usize)) * 4;
        [
            self.rgba[idx],
            self.rgba[idx + 1],
            self.rgba[idx + 2],
            self.rgba[idx + 3],
        ]
    }
}
