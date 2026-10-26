//! Host-facing vision service: limits, catalog execute, backend selection.

use crate::backends::{base64_decode, base64_encode, resolve_describe_backend, resolve_ocr_engine};
use crate::catalog;
use crate::image::{ImageBuffer, Region};
use crate::router::VisionRouter;
use crate::types::{DescribeRequest, OcrRequest, VisionError};
use cayw_ai::CancellationToken;
use cayw_ai::ToolDefinition;
use cayw_codemode::CapabilityError;
use serde_json::{Value, json};
use std::future::Future;
use std::sync::Arc;

/// Host-selected vision budgets. Catalog membership never grants permission.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Complete JSON-encoded argument envelope.
    pub input_bytes: usize,
    /// Complete JSON-encoded result envelope.
    pub output_bytes: usize,
    /// Max raw RGBA byte length (`width * height * 4`).
    pub max_image_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            input_bytes: 8 * 1024 * 1024,
            output_bytes: 8 * 1024 * 1024,
            max_image_bytes: 16 * 1024 * 1024,
        }
    }
}

impl Limits {
    pub fn validate(self) -> Result<(), CapabilityError> {
        if self.input_bytes == 0
            || self.input_bytes > 32 * 1024 * 1024
            || self.output_bytes == 0
            || self.output_bytes > 32 * 1024 * 1024
            || self.max_image_bytes == 0
            || self.max_image_bytes > 64 * 1024 * 1024
        {
            return Err(error(
                "invalid_limits",
                "vision limits must be nonzero and within documented hard ceilings",
            ));
        }
        Ok(())
    }
}

/// Stateful vision router held by the Host for catalog dispatch.
pub struct VisionService {
    router: VisionRouter,
    limits: Limits,
}

impl VisionService {
    /// Build from env-selected backends (tesseract / HTTP when present; fixture otherwise).
    pub fn from_env(limits: Limits) -> Result<Self, CapabilityError> {
        limits.validate()?;
        Ok(Self {
            router: VisionRouter::new(resolve_ocr_engine(), resolve_describe_backend()),
            limits,
        })
    }

    pub fn with_router(router: VisionRouter, limits: Limits) -> Result<Self, CapabilityError> {
        limits.validate()?;
        Ok(Self { router, limits })
    }

    pub fn router(&self) -> &VisionRouter {
        &self.router
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    pub fn execute_value(
        &self,
        name: &str,
        args: &Value,
        cancel: &CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.limits.validate()?;
        if cancel.is_cancelled() {
            return Err(error("cancelled", "vision operation cancelled"));
        }
        let encoded = serde_json::to_vec(args).map_err(|err| error("arguments", err))?;
        if encoded.len() > self.limits.input_bytes {
            return Err(error(
                "arguments",
                "vision arguments exceed input byte limit",
            ));
        }
        let result = match name {
            "vision.crop" => self.crop(args),
            "vision.pixel_diff" => self.pixel_diff(args),
            "vision.colors" => self.colors(args),
            "vision.ocr" => self.ocr(args, cancel),
            "vision.describe" => self.describe(args, cancel),
            _ => Err(VisionError::InvalidInput(format!(
                "unknown vision capability: {name}"
            ))),
        }
        .map_err(vision_error)?;
        let out = serde_json::to_vec(&result).map_err(|err| error("result", err))?;
        if out.len() > self.limits.output_bytes {
            return Err(error("result", "vision result exceeds output byte limit"));
        }
        Ok(result)
    }

    fn decode_image(&self, value: &Value) -> Result<ImageBuffer, VisionError> {
        let width = value["width"].as_u64().ok_or_else(|| {
            VisionError::InvalidInput("image.width must be a positive integer".into())
        })? as u32;
        let height = value["height"].as_u64().ok_or_else(|| {
            VisionError::InvalidInput("image.height must be a positive integer".into())
        })? as u32;
        let b64 = value["rgba_base64"].as_str().ok_or_else(|| {
            VisionError::InvalidInput("image.rgba_base64 must be a string".into())
        })?;
        let rgba = base64_decode(b64)?;
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(4))
            .ok_or_else(|| VisionError::InvalidInput("image dimensions overflow".into()))?;
        if rgba.len() != expected {
            return Err(VisionError::InvalidInput(format!(
                "rgba_base64 length {} does not match {width}x{height} (need {expected})",
                rgba.len()
            )));
        }
        if rgba.len() > self.limits.max_image_bytes {
            return Err(VisionError::InvalidInput(format!(
                "image exceeds max_image_bytes ({})",
                self.limits.max_image_bytes
            )));
        }
        ImageBuffer::from_rgba8(width, height, rgba)
    }

    fn crop(&self, args: &Value) -> Result<Value, VisionError> {
        let image = self.decode_image(&args["image"])?;
        let x = args["x"].as_u64().unwrap_or(0) as u32;
        let y = args["y"].as_u64().unwrap_or(0) as u32;
        let width = args["width"].as_u64().unwrap_or(0) as u32;
        let height = args["height"].as_u64().unwrap_or(0) as u32;
        let cropped = self
            .router
            .crop(&image, Region::new(x, y, width, height)?)?;
        Ok(json!({
            "width": cropped.width,
            "height": cropped.height,
            "image_sha256": cropped.image_sha256,
            "rgba_base64": base64_encode(&cropped.rgba),
        }))
    }

    fn pixel_diff(&self, args: &Value) -> Result<Value, VisionError> {
        let original = self.decode_image(&args["original"])?;
        let rebuilt = self.decode_image(&args["rebuilt"])?;
        let threshold = args
            .get("threshold")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(255) as u8;
        let report = self.router.pixel_diff(&original, &rebuilt, threshold)?;
        serde_json::to_value(report)
            .map_err(|err| VisionError::InvalidInput(format!("serialize pixel_diff: {err}")))
    }

    fn colors(&self, args: &Value) -> Result<Value, VisionError> {
        let image = self.decode_image(&args["image"])?;
        let top = args
            .get("top")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .clamp(1, 64) as usize;
        let colors = self.router.colors(&image, top)?;
        serde_json::to_value(colors)
            .map_err(|err| VisionError::InvalidInput(format!("serialize colors: {err}")))
    }

    fn ocr(&self, args: &Value, cancel: &CancellationToken) -> Result<Value, VisionError> {
        let image = self.decode_image(&args["image"])?;
        let engine_hint = args
            .get("engine_hint")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let result = self
            .router
            .ocr(&image, &OcrRequest { engine_hint }, cancel)?;
        serde_json::to_value(result)
            .map_err(|err| VisionError::OcrFailed(format!("serialize ocr: {err}")))
    }

    fn describe(&self, args: &Value, cancel: &CancellationToken) -> Result<Value, VisionError> {
        let image = self.decode_image(&args["image"])?;
        let question = args["question"]
            .as_str()
            .ok_or_else(|| VisionError::InvalidInput("question must be a string".into()))?
            .to_string();
        let json_mode = args.get("json").and_then(|v| v.as_bool()).unwrap_or(false);
        let result = self.router.describe(
            &image,
            &DescribeRequest {
                question,
                json: json_mode,
            },
            cancel,
        )?;
        serde_json::to_value(result)
            .map_err(|err| VisionError::DescribeFailed(format!("serialize describe: {err}")))
    }
}

/// Re-export catalog definitions for Host registration.
pub fn definitions() -> Vec<ToolDefinition> {
    catalog::definitions()
}

/// Validate + execute one vision capability on a blocking worker (Host pattern).
///
/// The Host `CancellationToken` is a child-token drop-guarded here: when the
/// Host cancels (or this future is dropped by `select!`), the token cancels and
/// OCR/describe backends kill their subprocess via KillOnDrop. When cancel wins
/// the race against the worker, we still await the join so cleanup finishes
/// before returning (no orphan tesseract/curl from a detached blocking task).
pub fn execute(
    service: Arc<VisionService>,
    name: &str,
    args: Value,
    cancel: CancellationToken,
) -> impl Future<Output = Result<Value, CapabilityError>> + Send {
    let name = name.to_owned();
    async move {
        let token = cancel.child_token();
        let guard = token.clone().drop_guard();
        if token.is_cancelled() {
            return Err(error("cancelled", "vision operation cancelled"));
        }
        let worker_token = token.clone();
        let mut handle = tokio::task::spawn_blocking(move || {
            if worker_token.is_cancelled() {
                return Err(error("cancelled", "vision operation cancelled"));
            }
            let result = service.execute_value(&name, &args, &worker_token);
            if worker_token.is_cancelled() {
                Err(error("cancelled", "vision operation cancelled"))
            } else {
                result
            }
        });
        let cancelled = tokio::select! {
            biased;
            _ = token.cancelled() => true,
            joined = &mut handle => {
                drop(guard);
                return joined.map_err(|_| error("worker", "vision worker failed"))?;
            }
        };
        let _ = cancelled;
        // Cancel won: backends KillOnDrop the child; await join so cleanup finishes.
        let result = match handle.await {
            Ok(Err(err)) => Err(err),
            Ok(Ok(_)) => Err(error("cancelled", "vision operation cancelled")),
            Err(_) => Err(error("worker", "vision worker failed")),
        };
        drop(guard);
        result
    }
}

fn vision_error(err: VisionError) -> CapabilityError {
    match err {
        VisionError::Cancelled => error("cancelled", "vision operation cancelled"),
        other => error("vision", other),
    }
}

fn error(kind: &str, message: impl std::fmt::Display) -> CapabilityError {
    CapabilityError::new(format!("{kind}: {message}"))
}
