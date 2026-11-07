//! SLICE C / F04 smoke: version bump, shared symbol ids, diagnostic deep-link.

use cayw_codeintel::*;

fn sha(nibble: char) -> String {
    nibble.to_string().repeat(64)
}

fn symbol(name: &str, kind: &str, start: u64, end: u64) -> SymbolInput {
    SymbolInput {
        name: name.into(),
        kind: kind.into(),
        byte_start: start,
        byte_end: end,
        start_line: Some(1),
        end_line: Some(1),
        parent_index: None,
    }
}

#[test]
fn publish_bumps_version_only_when_digest_changes() {
    let bridge = SymbolVersionBridge::new();
    let v1 = bridge
        .publish_file(
            "src/lib.rs",
            &sha('a'),
            "rust",
            vec![symbol("hello", "function", 0, 5)],
        )
        .unwrap();
    assert_eq!(v1.version, 1);

    let same = bridge
        .publish_file(
            "src/lib.rs",
            &sha('a'),
            "rust",
            vec![symbol("hello", "function", 0, 5)],
        )
        .unwrap();
    assert_eq!(same.version, 1);
    assert_eq!(same.content_sha256, v1.content_sha256);

    let v2 = bridge
        .publish_file(
            "src/lib.rs",
            &sha('b'),
            "rust",
            vec![symbol("hello", "function", 0, 5)],
        )
        .unwrap();
    assert_eq!(v2.version, 2);
    assert_ne!(v2.content_sha256, v1.content_sha256);
}

#[test]
fn agent_editor_trace_share_same_symbol_and_file_version() {
    let bridge = SymbolVersionBridge::new();
    let file = bridge
        .publish_file(
            "src/math.rs",
            &sha('c'),
            "rust",
            vec![
                symbol("add", "function", 10, 20),
                symbol("AddOpts", "struct", 40, 60),
            ],
        )
        .unwrap();

    // Three consumers independently resolve the same definition query.
    let agent = bridge
        .definitions(DefinitionQuery {
            name: "add".into(),
            path: None,
            kind: Some("function".into()),
            file_version: Some(file.version),
        })
        .unwrap();
    let editor = bridge
        .definitions(DefinitionQuery {
            name: "add".into(),
            path: Some("src/math.rs".into()),
            kind: None,
            file_version: None,
        })
        .unwrap();
    let trace = bridge.symbols_for("src/math.rs").unwrap();

    assert_eq!(agent.len(), 1);
    assert_eq!(editor.len(), 1);
    assert_eq!(agent[0].id, editor[0].id);
    assert_eq!(agent[0].file, file);
    assert_eq!(agent[0].file, editor[0].file);
    assert!(
        trace
            .iter()
            .any(|hit| hit.id == agent[0].id && hit.file.version == file.version)
    );

    let resolved = bridge.resolve_symbol(&agent[0].id).unwrap();
    assert_eq!(resolved.id.wire(), agent[0].id.wire());
}

#[test]
fn diagnostic_deep_links_versioned_source_and_rejects_stale() {
    let bridge = SymbolVersionBridge::new();
    let v1 = bridge
        .publish_file(
            "src/bad.rs",
            &sha('d'),
            "rust",
            vec![symbol("broken", "function", 0, 6)],
        )
        .unwrap();

    let diag = bridge
        .report_diagnostic(DiagnosticInput {
            path: "src/bad.rs".into(),
            severity: DiagnosticSeverity::Error,
            message: "missing return".into(),
            byte_start: 0,
            byte_end: 6,
            file_version: Some(v1.version),
            content_sha256: Some(v1.content_sha256.clone()),
            code: Some("E0001".into()),
            symbol_name: Some("broken".into()),
        })
        .unwrap();
    assert_eq!(diag.location.file_version, v1.version);
    assert_eq!(diag.location.content_sha256, v1.content_sha256);
    assert_eq!(diag.symbol_id.as_ref().unwrap().name, "broken");

    let listed = bridge.diagnostics(Some("src/bad.rs")).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, diag.id);

    // Content change bumps version; old version id is rejected; diagnostics cleared.
    let v2 = bridge
        .publish_file(
            "src/bad.rs",
            &sha('e'),
            "rust",
            vec![symbol("broken", "function", 0, 6)],
        )
        .unwrap();
    assert_eq!(v2.version, 2);
    assert!(bridge.diagnostics(Some("src/bad.rs")).unwrap().is_empty());

    let stale = bridge.report_diagnostic(DiagnosticInput {
        path: "src/bad.rs".into(),
        severity: DiagnosticSeverity::Error,
        message: "stale".into(),
        byte_start: 0,
        byte_end: 1,
        file_version: Some(v1.version),
        content_sha256: None,
        code: None,
        symbol_name: None,
    });
    assert!(matches!(
        stale,
        Err(CodeIntelError::StaleVersion {
            got: 1,
            expected: 2,
            ..
        })
    ));
}

#[test]
fn resolve_symbol_detects_stale_after_republish() {
    let bridge = SymbolVersionBridge::new();
    let v1 = bridge
        .publish_file(
            "a.rs",
            &sha('1'),
            "rust",
            vec![symbol("x", "function", 0, 1)],
        )
        .unwrap();
    let id = bridge.symbols_for("a.rs").unwrap()[0].id.clone();
    assert_eq!(id.file_version, v1.version);

    bridge
        .publish_file(
            "a.rs",
            &sha('2'),
            "rust",
            vec![symbol("x", "function", 0, 1)],
        )
        .unwrap();
    let err = bridge.resolve_symbol(&id).unwrap_err();
    assert!(matches!(err, CodeIntelError::StaleVersion { .. }));
}

#[test]
fn rejects_outline_reimplementation_budget_and_bad_paths() {
    let bridge = SymbolVersionBridge::with_budgets(1, 8);
    let err = bridge.publish_file(
        "../escape.rs",
        &sha('a'),
        "rust",
        vec![symbol("a", "function", 0, 1)],
    );
    assert!(matches!(err, Err(CodeIntelError::Invalid(_))));

    let over = bridge.publish_file(
        "src/lib.rs",
        &sha('a'),
        "rust",
        vec![symbol("a", "function", 0, 1), symbol("b", "function", 2, 3)],
    );
    assert!(matches!(over, Err(CodeIntelError::BudgetExceeded(1))));
}

#[test]
fn content_sha256_is_stable_hex() {
    let hex = content_sha256(b"hello");
    assert_eq!(hex.len(), 64);
    assert_eq!(hex, content_sha256(b"hello"));
    assert_ne!(hex, content_sha256(b"world"));
}
