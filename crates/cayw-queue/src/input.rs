//! Canonical task input. Text and images are persisted together so a queued
//! task cannot lose or reorder media between enqueue and execution.

use crate::QueueError;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cayw_ai::{Content, Message, Model, timestamp_ms};
use serde::{Deserialize, Serialize};

/// Image formats accepted by every provider the runtime currently supports.
/// Anything else is refused before a task row is created.
const IMAGE_MIME_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageInput {
    pub mime_type: String,
    /// Standard padded base64. URL-safe alphabets, whitespace and non-zero pad
    /// bits are rejected so one image has exactly one stored representation.
    pub data: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskInput {
    pub text: String,
    pub images: Vec<ImageInput>,
}

impl TaskInput {
    /// Validate trusted host input before it is written to the durable queue.
    /// `max_prompt_bytes` counts text plus each image's MIME type and encoded
    /// data, matching the bytes retained in the task row.
    pub fn validate(&self, model: &Model, max_prompt_bytes: usize) -> Result<(), QueueError> {
        if self.text.trim().is_empty() && self.images.is_empty() {
            return Err(QueueError::InvalidInput(
                "prompt must contain text or at least one image".into(),
            ));
        }
        if !self.images.is_empty() && !model.supports_images {
            return Err(QueueError::InvalidInput(
                "the configured model does not support image input".into(),
            ));
        }
        let mut bytes = self.text.len();
        if bytes > max_prompt_bytes {
            return Err(QueueError::InvalidInput(
                "prompt exceeds the byte budget".into(),
            ));
        }
        for image in &self.images {
            bytes = bytes
                .checked_add(image.mime_type.len())
                .and_then(|total| total.checked_add(image.data.len()))
                .ok_or_else(|| QueueError::InvalidInput("prompt size overflow".into()))?;
            if bytes > max_prompt_bytes {
                return Err(QueueError::InvalidInput(
                    "prompt exceeds the byte budget".into(),
                ));
            }
        }
        for image in &self.images {
            if !IMAGE_MIME_TYPES.contains(&image.mime_type.as_str()) {
                return Err(QueueError::InvalidInput(
                    "image MIME type is not supported".into(),
                ));
            }
            if image.data.is_empty() {
                return Err(QueueError::InvalidInput(
                    "image data must not be empty".into(),
                ));
            }
            let decoded = STANDARD
                .decode(&image.data)
                .map_err(|_| QueueError::InvalidInput("image data is not valid base64".into()))?;
            if STANDARD.encode(&decoded) != image.data {
                return Err(QueueError::InvalidInput(
                    "image data is not canonical padded base64".into(),
                ));
            }
        }
        Ok(())
    }

    /// One user message with the exact stored text and images. The text part is
    /// retained even when empty so the provider sees the original shape.
    pub fn message(&self) -> Message {
        let mut content = Vec::with_capacity(1 + self.images.len());
        content.push(Content::Text {
            text: self.text.clone(),
        });
        content.extend(self.images.iter().map(|image| Content::Image {
            data: image.data.clone(),
            mime_type: image.mime_type.clone(),
        }));
        Message::User {
            content,
            timestamp: timestamp_ms(),
        }
    }
}
