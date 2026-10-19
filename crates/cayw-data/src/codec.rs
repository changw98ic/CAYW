use crate::{
    budget::{Budget, Result, Sink, check_value},
    error,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::borrow::Cow;

fn bytes<'a>(input: &'a Value, budget: &mut Budget<'_>) -> Result<Cow<'a, [u8]>> {
    // This is the logical input, separate from the fixed argument envelope.
    // Array roots and individual bytes follow the same rules as JSON data;
    // UTF-8 and Base64 strings each remain a single scalar node.
    check_value(&input["value"], budget, 1)?;
    match input["kind"].as_str() {
        Some("utf8") => Ok(Cow::Borrowed(
            input["value"]
                .as_str()
                .ok_or_else(|| error("arguments", "UTF-8 input must be a string"))?
                .as_bytes(),
        )),
        Some("base64") => decode(
            "base64",
            input["value"]
                .as_str()
                .ok_or_else(|| error("arguments", "Base64 input must be string"))?,
            budget,
        )
        .map(Cow::Owned),
        Some("bytes") => {
            let values = input["value"]
                .as_array()
                .ok_or_else(|| error("arguments", "byte input must be an array"))?;
            let mut bytes = Vec::new();
            for value in values {
                budget.checkpoint()?;
                let byte = value
                    .as_u64()
                    .and_then(|v| u8::try_from(v).ok())
                    .ok_or_else(|| {
                        error("arguments", "bytes must be integer tokens from 0 to 255")
                    })?;
                bytes.push(byte);
            }
            Ok(Cow::Owned(bytes))
        }
        _ => Err(error(
            "arguments",
            "input kind must be utf8, bytes or base64",
        )),
    }
}

pub(crate) fn encode(args: &Value, budget: &mut Budget<'_>) -> Result<Value> {
    let bytes = bytes(&args["input"], budget)?;
    let text = match args["encoding"].as_str() {
        Some("hex") => hex(&bytes, budget)?,
        Some("base64") => {
            let size = bytes
                .len()
                .div_ceil(3)
                .checked_mul(4)
                .ok_or_else(|| error("output_limit", "Base64 size overflow"))?;
            if size > budget.limits.output_bytes {
                return Err(error("output_limit", "Base64 result exceeds output_bytes"));
            }
            let mut sink = Sink::new(budget.limits.output_bytes, budget.cancel);
            // Every chunk except the last is a multiple of three, so padding is canonical.
            for chunk in bytes.chunks(3072) {
                budget.checkpoint()?;
                sink.push(&STANDARD.encode(chunk))?;
            }
            sink.finish()?
        }
        _ => return Err(error("arguments", "unsupported encoding")),
    };
    Ok(json!({"text":text}))
}

pub(crate) fn decode_result(args: &Value, budget: &mut Budget<'_>) -> Result<Value> {
    let text = args["text"]
        .as_str()
        .ok_or_else(|| error("arguments", "encoded text must be string"))?;
    let bytes = decode(args["encoding"].as_str().unwrap_or_default(), text, budget)?;
    match args["output"].as_str() {
        Some("bytes") => {
            budget.node(1)?;
            budget.rows(bytes.len())?;
            let mut values = Vec::new();
            for byte in bytes {
                budget.node(2)?;
                values.push(Value::from(byte));
            }
            Ok(json!({"bytes":values}))
        }
        Some("utf8") => {
            budget.node(1)?;
            String::from_utf8(bytes)
                .map(|text| json!({"text":text}))
                .map_err(|_| error("encoding", "decoded bytes are not valid UTF-8"))
        }
        _ => Err(error("arguments", "output must be bytes or utf8")),
    }
}

fn decode(encoding: &str, text: &str, budget: &Budget<'_>) -> Result<Vec<u8>> {
    budget.checkpoint()?;
    match encoding {
        "base64" => {
            if !text.len().is_multiple_of(4) {
                return Err(error("syntax", "Base64 must use canonical padding"));
            }
            let mut bytes = Vec::new();
            let count = text.len().div_ceil(4096);
            for (i, chunk) in text.as_bytes().chunks(4096).enumerate() {
                budget.checkpoint()?;
                if i + 1 < count && chunk.contains(&b'=') {
                    return Err(error("syntax", "Base64 padding must be at the end"));
                }
                let decoded = STANDARD
                    .decode(chunk)
                    .map_err(|_| error("syntax", "invalid canonical standard Base64"))?;
                bytes.extend(decoded);
            }
            Ok(bytes)
        }
        "hex" => {
            if !text.len().is_multiple_of(2) {
                return Err(error("syntax", "hex must contain pairs of digits"));
            }
            let mut bytes = Vec::new();
            for pair in text.as_bytes().chunks_exact(2) {
                budget.checkpoint()?;
                let hi = digit(pair[0]).ok_or_else(|| error("syntax", "invalid hex digit"))?;
                let lo = digit(pair[1]).ok_or_else(|| error("syntax", "invalid hex digit"))?;
                bytes.push(hi * 16 + lo);
            }
            Ok(bytes)
        }
        _ => Err(error("arguments", "unsupported encoding")),
    }
}

fn digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn hex(bytes: &[u8], budget: &Budget<'_>) -> Result<String> {
    if bytes.len() > budget.limits.output_bytes / 2 {
        return Err(error("output_limit", "hex result exceeds output_bytes"));
    }
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut sink = Sink::new(budget.limits.output_bytes, budget.cancel);
    for byte in bytes {
        sink.push(
            std::str::from_utf8(&[
                DIGITS[usize::from(byte >> 4)],
                DIGITS[usize::from(byte & 15)],
            ])
            .expect("ASCII hex digits"),
        )?;
    }
    sink.finish()
}

pub(crate) fn digest(args: &Value, budget: &mut Budget<'_>) -> Result<Value> {
    let bytes = bytes(&args["input"], budget)?;
    let digest = match args["algorithm"].as_str() {
        Some("sha256") => {
            let mut hasher = Sha256::new();
            for chunk in bytes.chunks(4096) {
                budget.checkpoint()?;
                hasher.update(chunk);
            }
            hasher.finalize().to_vec()
        }
        Some("blake3") => {
            let mut hasher = blake3::Hasher::new();
            for chunk in bytes.chunks(4096) {
                budget.checkpoint()?;
                hasher.update(chunk);
            }
            hasher.finalize().as_bytes().to_vec()
        }
        _ => return Err(error("arguments", "unsupported hash algorithm")),
    };
    Ok(json!({"hex":hex(&digest, budget)?}))
}
