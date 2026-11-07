//! Built-in deterministic filter profiles (design-shaped after RTK filter intents).

use serde::{Deserialize, Serialize};

/// Named rule set applied after ANSI strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterProfile {
    /// Keep error-like lines + head/tail summary.
    Generic,
    /// Cargo / rustc style: error/warning/failed/panicked + test result lines.
    CargoLike,
    /// `git status` / short porcelain: keep status lines and branch header.
    GitStatusLike,
    /// No line selection beyond ANSI strip + max_lines (still dual-entry).
    Passthrough,
}

impl FilterProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::CargoLike => "cargo_like",
            Self::GitStatusLike => "git_status_like",
            Self::Passthrough => "passthrough",
        }
    }
}

/// Resolve a profile name; unknown names fall back to Generic.
pub fn builtin_profile(name: &str) -> FilterProfile {
    match name.trim().to_ascii_lowercase().as_str() {
        "generic" => FilterProfile::Generic,
        "cargo" | "cargo_like" | "rustc" => FilterProfile::CargoLike,
        "git" | "git_status" | "git_status_like" => FilterProfile::GitStatusLike,
        "passthrough" | "none" | "off" => FilterProfile::Passthrough,
        _ => FilterProfile::Generic,
    }
}

/// Whether a line is "error-like" for Generic / Cargo profiles.
pub fn is_error_line(profile: FilterProfile, line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let generic = lower.contains("error")
        || lower.contains("failed")
        || lower.contains("failure")
        || lower.contains("panic")
        || lower.contains("fatal")
        || lower.contains("traceback")
        || lower.contains("exception");
    match profile {
        FilterProfile::Generic => generic || lower.contains("warning"),
        FilterProfile::CargoLike => {
            generic
                || lower.contains("warning")
                || lower.contains("test result:")
                || lower.starts_with("---- ")
                || lower.contains("left:")
                || lower.contains("right:")
        }
        FilterProfile::GitStatusLike => false,
        FilterProfile::Passthrough => false,
    }
}

/// Whether a line is a "key change" line (diff / status / mutation).
pub fn is_change_line(profile: FilterProfile, line: &str) -> bool {
    let trimmed = line.trim_start();
    match profile {
        FilterProfile::Generic => {
            trimmed.starts_with("+++")
                || trimmed.starts_with("---")
                || trimmed.starts_with("@@")
                || trimmed.starts_with('+')
                || trimmed.starts_with('-')
                || trimmed.starts_with("diff ")
                || trimmed.to_ascii_lowercase().contains("changed")
                || trimmed.to_ascii_lowercase().contains("modified")
        }
        FilterProfile::CargoLike => {
            trimmed.starts_with("Compiling ")
                || trimmed.starts_with("Finished ")
                || trimmed.starts_with("Running ")
                || trimmed.starts_with("Doc-tests ")
        }
        FilterProfile::GitStatusLike => {
            trimmed.starts_with("On branch ")
                || trimmed.starts_with("Your branch ")
                || trimmed.starts_with("Changes ")
                || trimmed.starts_with("Untracked ")
                || trimmed.starts_with("new file:")
                || trimmed.starts_with("modified:")
                || trimmed.starts_with("deleted:")
                || trimmed.starts_with("renamed:")
                // Porcelain XY PATH keeps the leading status column; do not trim.
                || looks_like_porcelain(line)
        }
        FilterProfile::Passthrough => false,
    }
}

fn looks_like_porcelain(line: &str) -> bool {
    let b = line.as_bytes();
    if b.len() < 3 {
        return false;
    }
    // XY PATH porcelain: two status chars + space
    let x = b[0];
    let y = b[1];
    let space = b[2];
    space == b' ' && is_status_char(x) && is_status_char(y)
}

fn is_status_char(c: u8) -> bool {
    matches!(
        c,
        b'M' | b'A' | b'D' | b'R' | b'C' | b'U' | b'?' | b'!' | b' ' | b'*'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_aliases() {
        assert_eq!(builtin_profile("cargo"), FilterProfile::CargoLike);
        assert_eq!(builtin_profile("git_status"), FilterProfile::GitStatusLike);
    }

    #[test]
    fn porcelain_change() {
        assert!(is_change_line(
            FilterProfile::GitStatusLike,
            " M src/lib.rs"
        ));
        assert!(!is_change_line(FilterProfile::GitStatusLike, "hello world"));
    }
}
