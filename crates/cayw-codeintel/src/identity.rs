//! Bounded path / name / hash validators for the symbol bridge.

/// Workspace-relative paths: non-empty, ≤512 bytes, no NUL, no leading `/`,
/// no `..` segments, forward slashes only after normalization.
pub fn normalize_path(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("path is empty".into());
    }
    if trimmed.len() > 512 {
        return Err("path exceeds 512 bytes".into());
    }
    if trimmed.contains('\0') {
        return Err("path contains NUL".into());
    }
    if trimmed.starts_with('/') || trimmed.starts_with('\\') {
        return Err("path must be workspace-relative".into());
    }
    let mut parts = Vec::new();
    for raw in trimmed.split(['/', '\\']) {
        if raw.is_empty() || raw == "." {
            continue;
        }
        if raw == ".." {
            return Err("path must not contain parent segments".into());
        }
        parts.push(raw);
    }
    if parts.is_empty() {
        return Err("path is empty after normalization".into());
    }
    Ok(parts.join("/"))
}

/// Symbol names: non-empty Unicode, ≤256 chars after trim.
pub fn validate_name(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("symbol name is empty".into());
    }
    if trimmed.chars().count() > 256 {
        return Err("symbol name exceeds 256 characters".into());
    }
    if trimmed.contains('\0') {
        return Err("symbol name contains NUL".into());
    }
    Ok(trimmed.to_owned())
}

/// Symbol kinds: non-empty ASCII snake / kebab, ≤64 bytes (e.g. `function`).
pub fn validate_kind(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("symbol kind is empty".into());
    }
    if trimmed.len() > 64 {
        return Err("symbol kind exceeds 64 bytes".into());
    }
    if !trimmed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("symbol kind has unsupported characters".into());
    }
    Ok(trimmed.to_ascii_lowercase())
}

/// Lowercase hex SHA-256 (64 chars). Accepts uppercase and normalizes.
pub fn validate_sha256_hex(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.len() != 64 {
        return Err("content sha256 must be 64 hex characters".into());
    }
    if !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("content sha256 must be hexadecimal".into());
    }
    Ok(trimmed.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths_and_rejects_traversal() {
        assert_eq!(normalize_path("src\\lib.rs").unwrap(), "src/lib.rs");
        assert_eq!(normalize_path("./a/./b.rs").unwrap(), "a/b.rs");
        assert!(normalize_path("../x").is_err());
        assert!(normalize_path("/abs").is_err());
        assert!(normalize_path("").is_err());
    }

    #[test]
    fn accepts_kinds_and_hashes() {
        assert_eq!(validate_kind("Function").unwrap(), "function");
        assert!(validate_kind("bad kind").is_err());
        let hex = "a".repeat(64);
        assert_eq!(validate_sha256_hex(&hex.to_uppercase()).unwrap(), hex);
        assert!(validate_sha256_hex("deadbeef").is_err());
    }
}
