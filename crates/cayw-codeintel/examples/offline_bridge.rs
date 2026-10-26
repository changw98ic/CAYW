//! Offline demo: publish outline snapshots, share file/symbol versions, deep-link an error.

use cayw_codeintel::*;

fn sha(fill: u8) -> String {
    format!("{:02x}", fill).repeat(32)
}

fn main() {
    let bridge = SymbolVersionBridge::new();
    let file = bridge
        .publish_file(
            "src/lib.rs",
            &sha(0xab),
            "rust",
            vec![
                SymbolInput {
                    name: "visible".into(),
                    kind: "function".into(),
                    byte_start: 0,
                    byte_end: 7,
                    start_line: Some(1),
                    end_line: Some(3),
                    parent_index: None,
                },
                SymbolInput {
                    name: "Helper".into(),
                    kind: "struct".into(),
                    byte_start: 20,
                    byte_end: 26,
                    start_line: Some(5),
                    end_line: Some(5),
                    parent_index: None,
                },
            ],
        )
        .expect("publish");

    let defs = bridge
        .definitions(DefinitionQuery {
            name: "visible".into(),
            path: None,
            kind: None,
            file_version: Some(file.version),
        })
        .expect("definitions");
    let hit = &defs[0];

    let diag = bridge
        .report_diagnostic(DiagnosticInput {
            path: "src/lib.rs".into(),
            severity: DiagnosticSeverity::Error,
            message: "demo failure at visible".into(),
            byte_start: hit.location.byte_start,
            byte_end: hit.location.byte_end,
            file_version: Some(file.version),
            content_sha256: Some(file.content_sha256.clone()),
            code: Some("DEMO".into()),
            symbol_name: Some("visible".into()),
        })
        .expect("diagnostic");

    println!("protocol={}", CODEINTEL_PROTOCOL_VERSION);
    println!(
        "file_version={} sha={} language={}",
        file.version, file.content_sha256, file.language
    );
    println!("symbol_id={}", hit.id.wire());
    println!(
        "diagnostic={} path={} file_version={} message={}",
        diag.id, diag.location.path, diag.location.file_version, diag.message
    );
}
