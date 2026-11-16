use cayw_codeintel::{
    DiagnosticInput, DiagnosticSeverity, SymbolInput, SymbolVersionBridge, content_sha256,
};

#[test]
fn defaults_keep_large_symbol_sets_and_diagnostics() {
    let bridge = SymbolVersionBridge::new();
    let symbols = (0..1001)
        .map(|i| SymbolInput {
            name: format!("f{i}"),
            kind: "function".into(),
            byte_start: i,
            byte_end: i + 1,
            start_line: None,
            end_line: None,
            parent_index: None,
        })
        .collect();
    bridge
        .publish_file("large.rs", &content_sha256(b"large"), "rust", symbols)
        .unwrap();
    assert_eq!(bridge.symbols_for("large.rs").unwrap().len(), 1001);
    let message = "x".repeat(2048);
    for _ in 0..4097 {
        bridge
            .report_diagnostic(DiagnosticInput {
                path: "large.rs".into(),
                severity: DiagnosticSeverity::Error,
                message: message.clone(),
                byte_start: 0,
                byte_end: 1,
                file_version: Some(1),
                content_sha256: None,
                code: None,
                symbol_name: None,
            })
            .unwrap();
    }
    let diagnostics = bridge.diagnostics(None).unwrap();
    assert_eq!(diagnostics.len(), 4097);
    assert_eq!(diagnostics.last().unwrap().message, message);
}

#[test]
fn long_paths_names_and_kinds_preserve_identity() {
    let bridge = SymbolVersionBridge::new();
    let path = format!("{}file.rs", "directory/".repeat(60));
    let name = "f".repeat(300);
    let kind = "kind".repeat(20);
    let symbol = SymbolInput {
        name: name.clone(),
        kind: kind.clone(),
        byte_start: 0,
        byte_end: 300,
        start_line: None,
        end_line: None,
        parent_index: None,
    };
    bridge
        .publish_file(&path, &content_sha256(b"long"), "rust", vec![symbol])
        .unwrap();
    let hits = bridge.symbols_for(&path).unwrap();
    assert_eq!(hits[0].id.name, name);
    assert_eq!(hits[0].id.kind, kind);
    assert_eq!(hits[0].id.path, path);
    assert!(
        bridge
            .publish_file("../outside", &content_sha256(b"bad"), "rust", vec![])
            .is_err()
    );
}

#[test]
fn explicit_budgets_are_honored_including_zero() {
    let bridge = SymbolVersionBridge::with_budgets(0, 0);
    let digest = content_sha256(b"empty");
    bridge
        .publish_file("empty.rs", &digest, "rust", vec![])
        .unwrap();
    let symbol = SymbolInput {
        name: "f".into(),
        kind: "function".into(),
        byte_start: 0,
        byte_end: 1,
        start_line: None,
        end_line: None,
        parent_index: None,
    };
    assert!(matches!(
        bridge.publish_file("empty.rs", &digest, "rust", vec![symbol]),
        Err(cayw_codeintel::CodeIntelError::BudgetExceeded(0))
    ));
    assert!(matches!(
        bridge.report_diagnostic(DiagnosticInput {
            path: "empty.rs".into(),
            severity: DiagnosticSeverity::Error,
            message: "failed".into(),
            byte_start: 0,
            byte_end: 1,
            file_version: None,
            content_sha256: None,
            code: None,
            symbol_name: None
        }),
        Err(cayw_codeintel::CodeIntelError::BudgetExceeded(0))
    ));
}
