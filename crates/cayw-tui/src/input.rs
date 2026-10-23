//! Grapheme-aware editing and removal of terminal escape/control sequences.
use unicode_segmentation::UnicodeSegmentation;

const MAX_DRAFT_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Default)]
pub struct Editor {
    text: String,
    cursor: usize,
    revision: u64,
}

impl Editor {
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Byte offset, always at a grapheme boundary.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Changes only when text changes, so a late RPC cannot overwrite a newer draft.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn insert(&mut self, text: &str) {
        let clean = sanitize(text);
        let remaining = MAX_DRAFT_BYTES.saturating_sub(self.text.len());
        let end = clean
            .grapheme_indices(true)
            .map(|(offset, grapheme)| offset + grapheme.len())
            .take_while(|&end| end <= remaining)
            .last()
            .unwrap_or(0);
        if end > 0 {
            self.revision = self.revision.wrapping_add(1);
        }
        self.text.insert_str(self.cursor, &clean[..end]);
        self.cursor += end;
        // A combining character or ZWJ can join the following grapheme.
        self.cursor = self.next_boundary_at_or_after(self.cursor);
    }

    pub fn clear(&mut self) {
        if !self.text.is_empty() {
            self.revision = self.revision.wrapping_add(1);
        }
        self.text.clear();
        self.cursor = 0;
    }

    pub fn left(&mut self) {
        self.cursor = self.previous();
    }

    pub fn right(&mut self) {
        self.cursor = self.next();
    }

    pub fn backspace(&mut self) {
        let previous = self.previous();
        if previous != self.cursor {
            self.revision = self.revision.wrapping_add(1);
        }
        self.text.replace_range(previous..self.cursor, "");
        self.cursor = self.next_boundary_at_or_after(previous);
    }

    pub fn delete(&mut self) {
        if self.cursor != self.next() {
            self.revision = self.revision.wrapping_add(1);
        }
        self.text.replace_range(self.cursor..self.next(), "");
        self.cursor = self.next_boundary_at_or_after(self.cursor);
    }

    pub fn home(&mut self) {
        self.cursor = self.text[..self.cursor].rfind('\n').map_or(0, |at| at + 1);
    }

    pub fn end(&mut self) {
        self.cursor += self.text[self.cursor..]
            .find('\n')
            .unwrap_or(self.text.len() - self.cursor);
    }

    pub fn delete_word_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let before = &self.text[..self.cursor];
        let trimmed_len = before.trim_end_matches([' ', '\t']).len();
        let target = if trimmed_len == 0 {
            0
        } else {
            before[..trimmed_len]
                .rfind(|c: char| c.is_whitespace())
                .map_or(0, |idx| idx + 1)
        };
        let start = self.next_boundary_at_or_after(target);
        if start < self.cursor {
            self.revision = self.revision.wrapping_add(1);
            self.text.replace_range(start..self.cursor, "");
            self.cursor = self.next_boundary_at_or_after(start);
        }
    }

    pub fn up(&mut self) {
        let start = self.text[..self.cursor].rfind('\n').map_or(0, |at| at + 1);
        if start == 0 {
            return;
        }
        let column = self.text[start..self.cursor].graphemes(true).count();
        let previous_end = start - 1;
        let previous_start = self.text[..previous_end].rfind('\n').map_or(0, |at| at + 1);
        self.cursor =
            previous_start + offset_for_column(&self.text[previous_start..previous_end], column);
    }

    pub fn down(&mut self) {
        let start = self.text[..self.cursor].rfind('\n').map_or(0, |at| at + 1);
        let Some(end) = self.text[self.cursor..]
            .find('\n')
            .map(|at| at + self.cursor)
        else {
            return;
        };
        let column = self.text[start..self.cursor].graphemes(true).count();
        let next_start = end + 1;
        let next_end = self.text[next_start..]
            .find('\n')
            .map_or(self.text.len(), |at| at + next_start);
        self.cursor = next_start + offset_for_column(&self.text[next_start..next_end], column);
    }

    fn previous(&self) -> usize {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(at, _)| at)
    }

    fn next(&self) -> usize {
        self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.cursor, |value| self.cursor + value.len())
    }

    fn next_boundary_at_or_after(&self, cursor: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .map(|(at, _)| at)
            .find(|&at| at >= cursor)
            .unwrap_or(self.text.len())
    }
}

fn offset_for_column(line: &str, column: usize) -> usize {
    line.grapheme_indices(true)
        .nth(column)
        .map_or(line.len(), |(at, _)| at)
}

/// Strip ANSI/OSC/DCS commands, C0/C1 controls, and bidi direction overrides.
/// Keep ordinary Unicode (including emoji joins), newlines and expanded tabs.
pub fn sanitize(text: &str) -> String {
    #[derive(Clone, Copy)]
    enum Mode {
        Text,
        Escape,
        EscapeIntermediate,
        Csi,
        String { osc: bool, escape: bool },
    }
    let mut mode = Mode::Text;
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        mode = match mode {
            Mode::Text => match ch {
                '\u{1b}' => Mode::Escape,
                '\u{9b}' => Mode::Csi,
                '\u{9d}' => Mode::String {
                    osc: true,
                    escape: false,
                },
                '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => Mode::String {
                    osc: false,
                    escape: false,
                },
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    output.push('\n');
                    Mode::Text
                }
                '\n' => {
                    output.push('\n');
                    Mode::Text
                }
                '\t' => {
                    output.push_str("    ");
                    Mode::Text
                }
                ch if ch.is_control()
                    || matches!(ch, '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
                {
                    Mode::Text
                }
                ch => {
                    output.push(ch);
                    Mode::Text
                }
            },
            Mode::Escape => match ch {
                '[' => Mode::Csi,
                ']' => Mode::String {
                    osc: true,
                    escape: false,
                },
                'P' | 'X' | '^' | '_' => Mode::String {
                    osc: false,
                    escape: false,
                },
                '\u{20}'..='\u{2f}' => Mode::EscapeIntermediate,
                '\u{1b}' => Mode::Escape,
                _ => Mode::Text,
            },
            Mode::EscapeIntermediate => {
                if ('\u{30}'..='\u{7e}').contains(&ch) {
                    Mode::Text
                } else {
                    Mode::EscapeIntermediate
                }
            }
            Mode::Csi => match ch {
                '\u{40}'..='\u{7e}' => Mode::Text,
                '\u{1b}' => Mode::Escape,
                _ => Mode::Csi,
            },
            Mode::String { osc, escape } => {
                if ch == '\u{9c}' || (osc && ch == '\u{7}') || (escape && ch == '\\') {
                    Mode::Text
                } else {
                    Mode::String {
                        osc,
                        escape: ch == '\u{1b}',
                    }
                }
            }
        };
    }
    output
}
