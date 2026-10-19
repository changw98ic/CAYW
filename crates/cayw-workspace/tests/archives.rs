use cayw_workspace::{CancellationToken, Limits, Workspace};
use serde_json::json;
use std::{
    fs,
    io::{Cursor, Write},
};

fn zip(entries: &[(&str, &[u8])], deflate: bool) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(vec![]));
    for (name, bytes) in entries {
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(if deflate {
                zip::CompressionMethod::Deflated
            } else {
                zip::CompressionMethod::Stored
            })
            .unix_permissions(0o644);
        writer.start_file(*name, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}
fn tar(entries: &[(&str, &[u8], u8)]) -> Vec<u8> {
    let mut bytes = vec![];
    for (name, data, kind) in entries {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        let size = format!("{:011o}\0", data.len());
        header[124..136].copy_from_slice(size.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].fill(b' ');
        header[156] = *kind;
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u32 = header.iter().map(|byte| *byte as u32).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(data);
        bytes.resize(bytes.len().div_ceil(512) * 512, 0);
    }
    bytes.resize(bytes.len() + 1024, 0);
    bytes
}
async fn run(
    workspace: &Workspace,
    name: &str,
    value: serde_json::Value,
) -> Result<serde_json::Value, cayw_workspace::CapabilityError> {
    workspace
        .execute(name, &value, CancellationToken::new())
        .await
}

#[tokio::test]
async fn zip_and_tar_list_and_read_mark_utf8_and_binary_without_extraction() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("a.zip"),
        zip(&[("text.txt", b"hello"), ("bin", &[255, 0])], false),
    )
    .unwrap();
    fs::write(
        temp.path().join("a.tar"),
        tar(&[("text.txt", b"hello", b'0'), ("bin", &[255, 0], b'0')]),
    )
    .unwrap();
    let workspace =
        Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    for format in ["zip", "tar"] {
        let path = format!("a.{format}");
        let list = run(
            &workspace,
            "archive.list",
            json!({"path":path,"format":format}),
        )
        .await
        .unwrap();
        assert_eq!(list["entries"].as_array().unwrap().len(), 2);
        let text = run(
            &workspace,
            "archive.read",
            json!({"path":path,"format":format,"entry":"text.txt"}),
        )
        .await
        .unwrap();
        assert_eq!(text["data"]["encoding"], "utf8");
        assert_eq!(text["data"]["content"], "hello");
        let binary = run(
            &workspace,
            "archive.read",
            json!({"path":path,"format":format,"entry":"bin"}),
        )
        .await
        .unwrap();
        assert_eq!(binary["data"]["encoding"], "base64");
        assert_eq!(binary["data"]["content"], "/wA=");
    }
    assert!(!temp.path().join("text.txt").exists());
    assert!(!temp.path().join("bin").exists());
}
#[tokio::test]
async fn both_formats_reject_traversal_windows_paths_and_duplicate_entries() {
    let temp = tempfile::tempdir().unwrap();
    let workspace =
        Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    for path in [
        "../outside",
        "/absolute",
        "C:/windows",
        "a\\b",
        "a/../b",
        ".git/config",
        ".cayw-stuff/file",
        "CON",
        "trailing.",
        "a//b",
    ] {
        fs::write(
            temp.path().join("bad.tar"),
            tar(&[(path, b"payload", b'0')]),
        )
        .unwrap();
        fs::write(
            temp.path().join("bad.zip"),
            zip(&[(path, b"payload")], false),
        )
        .unwrap();
        for format in ["tar", "zip"] {
            let result = run(
                &workspace,
                "archive.list",
                json!({"path":format!("bad.{format}"),"format":format}),
            )
            .await;
            assert!(result.is_err(), "accepted {format} {path}");
        }
    }
    // ZIP writers reject duplicate names, so mutate equal-length central/local
    // names to create genuine duplicate records that the reader must detect.
    let mut duplicate = zip(&[("one", b"1"), ("two", b"2")], false);
    let mut at = 0;
    while at + 3 <= duplicate.len() {
        if &duplicate[at..at + 3] == b"two" {
            duplicate[at..at + 3].copy_from_slice(b"one");
            at += 3;
        } else {
            at += 1;
        }
    }
    fs::write(temp.path().join("dup.zip"), duplicate).unwrap();
    fs::write(
        temp.path().join("dup.tar"),
        tar(&[("one", b"1", b'0'), ("one", b"2", b'0')]),
    )
    .unwrap();
    for format in ["tar", "zip"] {
        assert!(
            run(
                &workspace,
                "archive.list",
                json!({"path":format!("dup.{format}"),"format":format})
            )
            .await
            .is_err()
        );
    }
    fs::write(
        temp.path().join("dup.tar"),
        tar(&[("one", b"1", b'0'), ("ONE", b"2", b'0')]),
    )
    .unwrap();
    assert!(
        run(
            &workspace,
            "archive.list",
            json!({"path":"dup.tar","format":"tar"})
        )
        .await
        .is_err()
    );
}
#[tokio::test]
async fn reject_links_special_entries_extensions_and_ancestor_collisions() {
    let temp = tempfile::tempdir().unwrap();
    let workspace =
        Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    for kind in [b'1', b'2', b'3', b'4', b'6', b'S', b'x', b'g', b'L', b'K'] {
        fs::write(temp.path().join("bad.tar"), tar(&[("entry", b"", kind)])).unwrap();
        assert!(
            run(
                &workspace,
                "archive.list",
                json!({"path":"bad.tar","format":"tar"})
            )
            .await
            .is_err(),
            "kind {kind}"
        );
    }
    let mut bytes = zip(&[("link", b"target")], false);
    let central = (0..bytes.len() - 4)
        .find(|at| &bytes[*at..*at + 4] == b"PK\x01\x02")
        .unwrap();
    bytes[central + 5] = 3;
    bytes[central + 38..central + 42].copy_from_slice(&(0o120777u32 << 16).to_le_bytes());
    fs::write(temp.path().join("bad.zip"), bytes).unwrap();
    assert!(
        run(
            &workspace,
            "archive.list",
            json!({"path":"bad.zip","format":"zip"})
        )
        .await
        .is_err()
    );
    for format in ["tar", "zip"] {
        let bytes = if format == "tar" {
            tar(&[("a", b"x", b'0'), ("a/b", b"y", b'0')])
        } else {
            zip(&[("a", b"x"), ("a/b", b"y")], false)
        };
        fs::write(temp.path().join(format!("bad.{format}")), bytes).unwrap();
        assert!(
            run(
                &workspace,
                "archive.list",
                json!({"path":format!("bad.{format}"),"format":format})
            )
            .await
            .is_err()
        );
    }
}
#[tokio::test]
async fn low_reported_sizes_corrupt_crc_and_concatenated_tar_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let workspace =
        Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    let mut understated = zip(&[("bomb", &vec![b'A'; 200_000])], true);
    let central = (0..understated.len() - 4)
        .find(|at| &understated[*at..*at + 4] == b"PK\x01\x02")
        .unwrap();
    understated[22..26].copy_from_slice(&1u32.to_le_bytes());
    understated[central + 24..central + 28].copy_from_slice(&1u32.to_le_bytes());
    fs::write(temp.path().join("understated.zip"), understated).unwrap();
    assert!(
        run(
            &workspace,
            "archive.read",
            json!({"path":"understated.zip","format":"zip","entry":"bomb"})
        )
        .await
        .is_err()
    );
    let mut corrupt = zip(&[("item", b"original")], false);
    let name_len = u16::from_le_bytes(corrupt[26..28].try_into().unwrap()) as usize;
    let extra_len = u16::from_le_bytes(corrupt[28..30].try_into().unwrap()) as usize;
    corrupt[30 + name_len + extra_len] ^= 1;
    fs::write(temp.path().join("crc.zip"), corrupt).unwrap();
    assert!(
        run(
            &workspace,
            "archive.read",
            json!({"path":"crc.zip","format":"zip","entry":"item"})
        )
        .await
        .is_err()
    );
    let mut joined = tar(&[("item", b"one", b'0')]);
    joined.extend_from_slice(&tar(&[("item", b"two", b'0')]));
    fs::write(temp.path().join("joined.tar"), joined).unwrap();
    assert!(
        run(
            &workspace,
            "archive.list",
            json!({"path":"joined.tar","format":"tar"})
        )
        .await
        .is_err()
    );
    let truncated = tar(&[("item", b"hello", b'0')]);
    fs::write(temp.path().join("truncated.tar"), &truncated[..514]).unwrap();
    assert!(
        run(
            &workspace,
            "archive.read",
            json!({"path":"truncated.tar","format":"tar","entry":"item"})
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn archive_bombs_counts_depth_total_output_and_cancellation_are_bounded() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("bomb.zip"),
        zip(&[("bomb", &vec![b'A'; 200_000])], true),
    )
    .unwrap();
    let workspace =
        Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    assert!(
        run(
            &workspace,
            "archive.list",
            json!({"path":"bomb.zip","format":"zip"})
        )
        .await
        .is_err()
    );
    fs::write(
        temp.path().join("normal.zip"),
        zip(&[("a", b"hello"), ("b", b"world")], true),
    )
    .unwrap();
    let normal = run(
        &workspace,
        "archive.read",
        json!({"path":"normal.zip","format":"zip","entry":"a"}),
    )
    .await
    .unwrap();
    assert_eq!(normal["data"]["content"], "hello");
    let limited = Workspace::open(
        temp.path().canonicalize().unwrap(),
        Limits {
            max_archive_entries: 1,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(
        run(
            &limited,
            "archive.list",
            json!({"path":"normal.zip","format":"zip"})
        )
        .await
        .is_err()
    );
    fs::write(temp.path().join("deep.tar"), tar(&[("a/b/c", b"1", b'0')])).unwrap();
    let limited = Workspace::open(
        temp.path().canonicalize().unwrap(),
        Limits {
            max_depth: 2,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(
        run(
            &limited,
            "archive.list",
            json!({"path":"deep.tar","format":"tar"})
        )
        .await
        .is_err()
    );
    let limited = Workspace::open(
        temp.path().canonicalize().unwrap(),
        Limits {
            max_archive_expanded_bytes: 6,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(
        run(
            &limited,
            "archive.list",
            json!({"path":"normal.zip","format":"zip"})
        )
        .await
        .is_err()
    );
    let limited = Workspace::open(
        temp.path().canonicalize().unwrap(),
        Limits {
            max_output_bytes: 10,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(
        run(
            &limited,
            "archive.read",
            json!({"path":"normal.zip","format":"zip","entry":"a"})
        )
        .await
        .is_err()
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        !workspace
            .execute(
                "archive.read",
                &json!({"path":"normal.zip","format":"zip","entry":"a"}),
                cancel
            )
            .await
            .unwrap_err()
            .outcome_unknown
    );
}
