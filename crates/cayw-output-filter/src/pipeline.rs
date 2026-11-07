//! Deterministic filter pipeline.

use sha2::{Digest, Sha256};

use crate::ansi::strip_ansi;
use crate::profiles::{FilterProfile, is_change_line, is_error_line};
use crate::types::{FilterOptions, FilterResult, OutputReduction};

pub use crate::types::OutputFilterError as FilterError;

/// Apply optional filtering; always retains full `original` alongside `filtered`.
pub fn apply_filter(original: &str, options: FilterOptions) -> Result<FilterResult, FilterError> {
    let original = original.to_string();
    let original_digest = hex_sha256(original.as_bytes());
    let original_lines = count_lines(&original);
    let original_bytes = original.len();

    if !options.enabled || options.profile == FilterProfile::Passthrough {
        let filtered = if options.enabled && options.profile == FilterProfile::Passthrough {
            let stripped = strip_ansi(&original);
            truncate_lines(&stripped, options.max_lines)
        } else {
            original.clone()
        };
        let reduction = OutputReduction {
            original_bytes,
            filtered_bytes: filtered.len(),
            original_lines,
            filtered_lines: count_lines(&filtered),
            changed: filtered != original,
        };
        return Ok(FilterResult {
            applied_profile: if options.enabled {
                options.profile.as_str().to_string()
            } else {
                "disabled".to_string()
            },
            original,
            filtered,
            options,
            reduction,
            original_digest,
        });
    }

    let stripped = strip_ansi(&original);
    let lines: Vec<&str> = stripped.lines().collect();
    let selected = select_lines(&lines, &options);
    let mut filtered = selected.join("\n");
    if stripped.ends_with('\n') && !filtered.is_empty() {
        filtered.push('\n');
    }
    if filtered.is_empty() && !stripped.trim().is_empty() {
        // Never return empty when original had content: keep head+tail fallback.
        filtered = truncate_lines(
            &stripped,
            options
                .max_lines
                .max(options.head_lines + options.tail_lines)
                .max(1),
        );
    }

    let reduction = OutputReduction {
        original_bytes,
        filtered_bytes: filtered.len(),
        original_lines,
        filtered_lines: count_lines(&filtered),
        changed: filtered != original,
    };

    Ok(FilterResult {
        applied_profile: options.profile.as_str().to_string(),
        original,
        filtered,
        options,
        reduction,
        original_digest,
    })
}

fn select_lines<'a>(lines: &[&'a str], options: &FilterOptions) -> Vec<&'a str> {
    let n = lines.len();
    let mut keep = vec![false; n];

    // Always keep error + change lines.
    for (i, line) in lines.iter().enumerate() {
        if is_error_line(options.profile, line) || is_change_line(options.profile, line) {
            keep[i] = true;
        }
    }

    // Head: first non-empty lines.
    let mut taken = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if options.head_lines == 0 {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        keep[i] = true;
        taken += 1;
        if taken >= options.head_lines {
            break;
        }
    }

    // Tail: last non-empty lines.
    taken = 0;
    for (i, line) in lines.iter().enumerate().rev() {
        if options.tail_lines == 0 {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        keep[i] = true;
        taken += 1;
        if taken >= options.tail_lines {
            break;
        }
    }

    let mut out: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| keep[*i])
        .map(|(_, l)| *l)
        .collect();

    if options.max_lines > 0 && out.len() > options.max_lines {
        // Prefer keeping errors: partition then truncate.
        let mut errors = Vec::new();
        let mut rest = Vec::new();
        for line in out {
            if is_error_line(options.profile, line) {
                errors.push(line);
            } else {
                rest.push(line);
            }
        }
        let mut combined = errors;
        let room = options.max_lines.saturating_sub(combined.len());
        if room > 0 {
            if rest.len() <= room {
                combined.extend(rest);
            } else {
                let head = room / 2;
                let tail = room - head;
                combined.extend(rest.iter().take(head).copied());
                if tail > 0 && rest.len() > head {
                    combined.extend(rest.iter().skip(rest.len() - tail).copied());
                }
            }
        }
        out = combined;
        if out.len() > options.max_lines {
            out.truncate(options.max_lines);
        }
    }
    out
}

fn truncate_lines(text: &str, max_lines: usize) -> String {
    if max_lines == 0 {
        return text.to_string();
    }
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        return text.to_string();
    }
    let mut out = lines[..max_lines].join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.lines().count()
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::FilterProfile;

    #[test]
    fn disabled_is_identity() {
        let opts = FilterOptions {
            enabled: false,
            ..FilterOptions::default()
        };
        let r = apply_filter("a\nb\n", opts).unwrap();
        assert_eq!(r.filtered, r.original);
        assert!(!r.reduction.changed);
        assert_eq!(r.applied_profile, "disabled");
    }

    #[test]
    fn keeps_errors_and_reduces_noise() {
        let mut noise = String::new();
        for i in 0..50 {
            noise.push_str(&format!("noise line {i}\n"));
        }
        noise.push_str("error: something broke\n");
        for i in 50..100 {
            noise.push_str(&format!("noise line {i}\n"));
        }
        let opts = FilterOptions {
            enabled: true,
            profile: FilterProfile::Generic,
            head_lines: 2,
            tail_lines: 2,
            max_lines: 40,
        };
        let r = apply_filter(&noise, opts).unwrap();
        assert!(r.filtered.contains("error: something broke"));
        assert!(r.reduction.filtered_bytes < r.reduction.original_bytes);
        assert!(r.reduction.reduction_bps().unwrap() > 0);
        assert_eq!(r.original, noise);
    }
}
