const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

/// Frames bytes before decoding UTF-8, so network boundaries may split code points.
#[derive(Default)]
pub(super) struct SseDecoder {
    line: Vec<u8>,
    data: Vec<String>,
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

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, String> {
        let mut events = Vec::new();
        for &byte in bytes {
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
            match byte {
                b'\r' => {
                    self.end_line(&mut events)?;
                    self.skip_lf = true;
                }
                b'\n' => self.end_line(&mut events)?,
                _ => self.line.push(byte),
            }
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<String>, String> {
        let mut events = Vec::new();
        if !self.line.is_empty() {
            self.end_line(&mut events)?;
        }
        self.dispatch(&mut events);
        Ok(events)
    }

    fn end_line(&mut self, events: &mut Vec<String>) -> Result<(), String> {
        let bytes = std::mem::take(&mut self.line);
        let mut line =
            std::str::from_utf8(&bytes).map_err(|_| "Invalid UTF-8 in SSE event".to_owned())?;
        if self.first_line {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
            self.first_line = false;
        }
        if line.is_empty() {
            self.dispatch(events);
            self.event_bytes = 0;
        } else if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            if field == "data" {
                self.data
                    .push(value.strip_prefix(' ').unwrap_or(value).to_owned());
            }
        }
        Ok(())
    }

    fn dispatch(&mut self, events: &mut Vec<String>) {
        if !self.data.is_empty() {
            events.push(std::mem::take(&mut self.data).join("\n"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_utf8_splits_and_line_endings_preserve_data() {
        let bytes = "\u{feff}: comment\r\ndata: 你好\rdata: 🦀\n\r\ndata: tail".as_bytes();
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        for byte in bytes {
            events.extend(decoder.push(&[*byte]).unwrap());
        }
        events.extend(decoder.finish().unwrap());
        assert_eq!(events, ["你好\n🦀", "tail"]);
    }

    #[test]
    fn oversized_event_and_invalid_utf8_are_rejected() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(&vec![b'x'; MAX_EVENT_BYTES + 1]).is_err());
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: \xff\n\n").is_err());
    }
}
