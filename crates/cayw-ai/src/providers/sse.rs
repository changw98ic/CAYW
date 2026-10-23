use crate::{CancellationToken, json::checkpoint};

const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;
const DECODE_BATCH_BYTES: usize = 4096;

/// Frames bytes before decoding UTF-8, so network boundaries may split code points.
#[derive(Default)]
pub(super) struct SseDecoder {
    line: Vec<u8>,
    data: Option<String>,
    skip_lf: bool,
    event_bytes: usize,
    first_line: bool,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self {
            first_line: true,
            ..Self::default()
        }
    }

    /// Stop at the first data frame. The caller can accept a terminal frame
    /// before inspecting any later bytes from the same network chunk.
    pub async fn push_frame(
        &mut self,
        bytes: &[u8],
        cancel: &CancellationToken,
    ) -> Result<(usize, Option<String>), String> {
        for (index, &byte) in bytes.iter().enumerate() {
            if index.is_multiple_of(DECODE_BATCH_BYTES) {
                checkpoint(cancel).await?;
            }
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            self.event_bytes += 1;
            if self.event_bytes > MAX_EVENT_BYTES {
                return Err("SSE event exceeds the 8 MiB limit".into());
            }
            let frame = match byte {
                b'\r' => {
                    let frame = self.end_line(cancel).await?;
                    self.skip_lf = true;
                    frame
                }
                b'\n' => self.end_line(cancel).await?,
                _ => {
                    self.line.push(byte);
                    None
                }
            };
            if frame.is_some() {
                return Ok((index + 1, frame));
            }
        }
        Ok((bytes.len(), None))
    }

    pub async fn finish(&mut self, cancel: &CancellationToken) -> Result<Option<String>, String> {
        checkpoint(cancel).await?;
        if !self.line.is_empty()
            && let Some(frame) = self.end_line(cancel).await?
        {
            return Ok(Some(frame));
        }
        Ok(self.data.take())
    }

    async fn end_line(&mut self, cancel: &CancellationToken) -> Result<Option<String>, String> {
        let bytes = std::mem::take(&mut self.line);
        let decoded = decode_line(&bytes, cancel).await?;
        let mut line = decoded.as_str();
        if self.first_line {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
            self.first_line = false;
        }
        if line.is_empty() {
            self.event_bytes = 0;
            return Ok(self.data.take());
        }
        if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            if field == "data" {
                let value = value.strip_prefix(' ').unwrap_or(value);
                if let Some(data) = &mut self.data {
                    data.push('\n');
                    append_text(data, value, cancel).await?;
                } else {
                    let mut data = String::new();
                    append_text(&mut data, value, cancel).await?;
                    self.data = Some(data);
                }
            }
        }
        Ok(None)
    }
}

async fn decode_line(bytes: &[u8], cancel: &CancellationToken) -> Result<String, String> {
    let mut text = String::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        checkpoint(cancel).await?;
        let mut end = (cursor + DECODE_BATCH_BYTES).min(bytes.len());
        let batch = loop {
            match std::str::from_utf8(&bytes[cursor..end]) {
                Ok(batch) => break batch,
                Err(error) if error.error_len().is_none() && end < bytes.len() => {
                    end = cursor + error.valid_up_to()
                }
                Err(_) => return Err("Invalid UTF-8 in SSE event".into()),
            }
        };
        text.push_str(batch);
        cursor = end;
    }
    Ok(text)
}

async fn append_text(
    text: &mut String,
    source: &str,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let mut cursor = 0;
    while cursor < source.len() {
        checkpoint(cancel).await?;
        let mut end = (cursor + DECODE_BATCH_BYTES).min(source.len());
        while !source.is_char_boundary(end) {
            end -= 1;
        }
        text.push_str(&source[cursor..end]);
        cursor = end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn push(decoder: &mut SseDecoder, mut bytes: &[u8]) -> Result<Vec<String>, String> {
        let mut frames = Vec::new();
        while !bytes.is_empty() {
            let (used, frame) = decoder.push_frame(bytes, &CancellationToken::new()).await?;
            bytes = &bytes[used..];
            frames.extend(frame);
        }
        Ok(frames)
    }

    #[tokio::test]
    async fn arbitrary_utf8_splits_and_line_endings_preserve_data() {
        let bytes = "\u{feff}: comment\r\ndata: 你好\rdata: 🦀\n\r\ndata: tail".as_bytes();
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        for byte in bytes {
            events.extend(push(&mut decoder, &[*byte]).await.unwrap());
        }
        events.extend(decoder.finish(&CancellationToken::new()).await.unwrap());
        assert_eq!(events, ["你好\n🦀", "tail"]);
    }

    #[tokio::test]
    async fn oversized_event_and_invalid_utf8_are_rejected() {
        assert!(
            push(&mut SseDecoder::new(), &vec![b'x'; MAX_EVENT_BYTES + 1])
                .await
                .is_err()
        );
        assert!(
            push(&mut SseDecoder::new(), b"data: \xff\n\n")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_terminal_frame_does_not_predecode_bad_or_oversized_trailing_bytes() {
        for tail in [b"data: \xff\n\n".to_vec(), vec![b'x'; MAX_EVENT_BYTES + 1]] {
            let prefix = b"data: [DONE]\r\n\r\n";
            let input = [prefix.as_slice(), &tail].concat();
            let mut decoder = SseDecoder::new();
            let (used, frame) = decoder
                .push_frame(&input, &CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(frame.as_deref(), Some("[DONE]"));
            assert!(used <= prefix.len());
            assert!(push(&mut decoder, &input[used..]).await.is_err());
        }
    }
}
