//! Host / Runtime capability definitions for vision tools.

use cayw_ai::ToolDefinition;
use serde_json::{Value, json};

fn object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn image_schema() -> Value {
    object(
        json!({
            "width": {"type":"integer","minimum":1,"maximum":8192},
            "height": {"type":"integer","minimum":1,"maximum":8192},
            "rgba_base64": {"type":"string","minLength":1}
        }),
        &["width", "height", "rgba_base64"],
    )
}

/// Canonical vision capability names exposed through Host → Runtime catalog.
pub fn definitions() -> Vec<ToolDefinition> {
    let image = image_schema();
    let variants = [
        (
            "vision.crop",
            "Crop a rectangle from an RGBA8 image. Returns cropped dimensions, content SHA-256, and rgba_base64.",
            object(
                json!({
                    "image": image.clone(),
                    "x": {"type":"integer","minimum":0},
                    "y": {"type":"integer","minimum":0},
                    "width": {"type":"integer","minimum":1},
                    "height": {"type":"integer","minimum":1}
                }),
                &["image", "x", "y", "width", "height"],
            ),
        ),
        (
            "vision.pixel_diff",
            "Compare two same-size RGBA8 images. Returns differing pixel counts, ratio, and 8×8 worst cells.",
            object(
                json!({
                    "original": image.clone(),
                    "rebuilt": image.clone(),
                    "threshold": {"type":"integer","minimum":0,"maximum":255}
                }),
                &["original", "rebuilt"],
            ),
        ),
        (
            "vision.colors",
            "Dominant opaque colors of an RGBA8 image (hex + share).",
            object(
                json!({
                    "image": image.clone(),
                    "top": {"type":"integer","minimum":1,"maximum":64}
                }),
                &["image"],
            ),
        ),
        (
            "vision.ocr",
            "OCR an RGBA8 image via the host OCR backend (tesseract when installed; fixture fallback for CI).",
            object(
                json!({
                    "image": image.clone(),
                    "engine_hint": {"type":"string","minLength":1,"maxLength":64}
                }),
                &["image"],
            ),
        ),
        (
            "vision.describe",
            "Describe / visual Q&A over an RGBA8 image. Uses HTTP describe when CAYW_VISION_DESCRIBE_URL is set; otherwise the offline counting/fixture backend.",
            object(
                json!({
                    "image": image,
                    "question": {"type":"string","minLength":1,"maxLength":4096},
                    "json": {"type":"boolean"}
                }),
                &["image", "question"],
            ),
        ),
    ];
    variants
        .into_iter()
        .map(|(name, description, parameters)| ToolDefinition {
            name: name.into(),
            description: description.into(),
            parameters,
        })
        .collect()
}
