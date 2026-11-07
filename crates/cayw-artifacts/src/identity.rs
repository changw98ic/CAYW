//! Bounded identifiers for artifacts and actions.

/// Session / artifact / action ids: non-empty, ≤128 bytes, ASCII alnum / `_` / `-` / `.` / `:`.
pub fn validate_id(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{field} is empty"));
    }
    if value.len() > 128 {
        return Err(format!("{field} exceeds 128 bytes"));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
    {
        return Err(format!("{field} has unsupported characters"));
    }
    if value.starts_with(':') || value.ends_with(':') || value.contains("::") {
        return Err(format!("{field} has empty namespace segment"));
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

/// HTML / chart body: non-empty UTF-8, ≤1 MiB.
pub fn validate_body(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("body is empty".into());
    }
    if value.len() > 1024 * 1024 {
        return Err("body exceeds 1 MiB".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_namespaced_ids() {
        assert!(validate_id("artifact_id", "card:summary").is_ok());
        assert!(validate_id("session_id", "sess-1").is_ok());
        assert!(validate_id("artifact_id", "").is_err());
        assert!(validate_id("artifact_id", "a::b").is_err());
        assert!(validate_id("artifact_id", "has space").is_err());
    }

    #[test]
    fn rejects_empty_title_and_body() {
        assert!(validate_title("  ").is_err());
        assert_eq!(validate_title(" Hello ").unwrap(), "Hello");
        assert!(validate_body("").is_err());
        assert!(validate_body("<p>ok</p>").is_ok());
    }
}
