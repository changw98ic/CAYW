//! Minimal deterministic ANSI CSI/OSC stripper (no regex).

/// Remove common ANSI escape sequences from `input`.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for d in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&d) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(d) = chars.next() {
                    if d == '\u{07}' {
                        break;
                    }
                    if d == '\u{1b}' {
                        if matches!(chars.peek(), Some('\\')) {
                            let _ = chars.next();
                        }
                        break;
                    }
                }
            }
            Some('(') | Some(')') => {
                let _ = chars.next();
            }
            Some(_) | None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_color() {
        let raw = "\u{1b}[31merror\u{1b}[0m: boom";
        assert_eq!(strip_ansi(raw), "error: boom");
    }

    #[test]
    fn preserves_unicode() {
        assert_eq!(strip_ansi("你好\u{1b}[1m世界\u{1b}[0m"), "你好世界");
    }
}
