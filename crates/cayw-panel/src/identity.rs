//! Bounded identifiers for panel and viewer contributions.

/// Contribution ids: non-empty, ≤128 bytes, ASCII alnum / `_` / `-` / `.` / `:`.
/// Colons allow namespaced third-party ids such as `vendor:tools`.
pub fn validate_contribution_id(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("contribution id is empty".into());
    }
    if value.len() > 128 {
        return Err("contribution id exceeds 128 bytes".into());
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
    {
        return Err("contribution id has unsupported characters".into());
    }
    if value.starts_with(':') || value.ends_with(':') || value.contains("::") {
        return Err("contribution id has empty namespace segment".into());
    }
    Ok(())
}

/// Display titles: non-empty Unicode, ≤128 chars after trim.
pub fn validate_title(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("title is empty".into());
    }
    if trimmed.chars().count() > 128 {
        return Err("title exceeds 128 characters".into());
    }
    Ok(trimmed.to_owned())
}

/// File extensions without a leading dot, lowercase ASCII, ≤32 bytes.
pub fn validate_extension(value: &str) -> Result<String, String> {
    let trimmed = value.trim().trim_start_matches('.');
    if trimmed.is_empty() {
        return Err("extension is empty".into());
    }
    if trimmed.len() > 32 {
        return Err("extension exceeds 32 bytes".into());
    }
    if !trimmed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'+')
    {
        return Err("extension has unsupported characters".into());
    }
    Ok(trimmed.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_namespaced_ids_and_rejects_empty_segments() {
        assert!(validate_contribution_id("files").is_ok());
        assert!(validate_contribution_id("vendor:db").is_ok());
        assert!(validate_contribution_id(":bad").is_err());
        assert!(validate_contribution_id("bad:").is_err());
        assert!(validate_contribution_id("a::b").is_err());
        assert!(validate_contribution_id("has space").is_err());
    }

    #[test]
    fn normalizes_extensions() {
        assert_eq!(validate_extension(".MD").unwrap(), "md");
        assert_eq!(validate_extension("tsx").unwrap(), "tsx");
        assert!(validate_extension(".").is_err());
        assert!(validate_extension("../x").is_err());
    }
}
