use cayw_ai::CancellationToken;
use cayw_bridge::{
    Operation, Provenance, RequestKey, SourceIdentity, SourceOperation, SourceRequest,
};
use cayw_sources::search::WorkspaceSearch;
use serde_json::{Value, json};

fn request(arguments: Value, max_result_bytes: usize) -> SourceRequest {
    SourceRequest {
        key: RequestKey {
            id: 1,
            source: Provenance {
                registry_incarnation: "adapter-search-test".into(),
                source: SourceIdentity {
                    id: "workspace-search".into(),
                    version: "1".into(),
                    origin: "host:adapter-test".into(),
                },
                generation: 1,
                manifest_sha256: "0".repeat(64),
            },
            operation: Operation::Search,
            target: "search.query".into(),
        },
        operation: SourceOperation::Search { arguments },
        max_result_bytes,
    }
}

#[tokio::test]
async fn actual_search_keeps_context_and_rejects_an_oversized_envelope_as_known_failure() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("sample.txt"),
        "before\nunique-marker\nafter\n",
    )
    .unwrap();
    let source = WorkspaceSearch::open(root.path().canonicalize().unwrap()).unwrap();
    let stats = source.stats();
    let handle = source.into_handle();
    let arguments = json!({"query":"unique-marker","path":"sample.txt","limit":1});
    let error = handle
        .request(request(arguments.clone(), 2), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(!error.outcome_unknown);
    assert!(error.message.contains("result budget"));
    assert_eq!(stats.snapshot().queries, 1);
    let original = request(arguments, 16 * 1024);
    let key = original.key.clone();
    let reply = handle
        .request(original, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(reply.key, key);
    assert_eq!(reply.value["matches"][0]["text"], "unique-marker");
    assert_eq!(reply.value["matches"][0]["start_line"], 2);
    let context = reply.value["matches"][0]["context"]["text"]
        .as_str()
        .unwrap();
    assert!(context.contains("before\nunique-marker\nafter"));
    assert_eq!(reply.value["summary"]["match_count"], 1);
    assert_eq!(reply.value["summary"]["scan_complete"], true);
    assert_eq!(stats.snapshot().queries, 2);
    assert_eq!(stats.snapshot().rejections, 1);
    handle.disconnect();
    assert!(stats.snapshot().closed);
}

#[tokio::test]
async fn excluded_files_are_reported_even_when_the_allowed_scan_has_no_matches() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    std::fs::write(root.path().join(".git/protected.txt"), "hidden-marker\n").unwrap();
    std::fs::write(root.path().join(".gitignore"), "excluded.txt\n").unwrap();
    std::fs::write(root.path().join("excluded.txt"), "hidden-marker\n").unwrap();
    std::fs::write(root.path().join("visible.txt"), "ordinary content\n").unwrap();
    let source = WorkspaceSearch::open(root.path().canonicalize().unwrap()).unwrap();
    let handle = source.into_handle();
    let reply = handle
        .request(
            request(json!({"query":"hidden-marker","limit":5}), 16 * 1024),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(reply.value["matches"].as_array().unwrap().is_empty());
    let summary = &reply.value["summary"];
    assert_eq!(summary["scan_complete"], true);
    assert_eq!(summary["protected_entries_omitted"], true);
    assert_eq!(summary["respect_gitignore"], true);
    assert_eq!(summary["scan"]["ignored_entries"], 1);
    assert!(
        summary["scan"]["protected_entries_omitted"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert!(summary["skipped"].as_array().unwrap().is_empty());
    handle.disconnect();
}

#[tokio::test]
async fn moved_workspace_name_cannot_redirect_search_to_a_replacement_directory() {
    let parent = tempfile::tempdir().unwrap();
    let parent_path = parent.path().canonicalize().unwrap();
    let selected = parent_path.join("selected");
    let moved = parent_path.join("moved");
    std::fs::create_dir(&selected).unwrap();
    std::fs::write(selected.join("sample.txt"), "marker original-root\n").unwrap();
    let source = WorkspaceSearch::open(&selected).unwrap();
    let handle = source.into_handle();
    std::fs::rename(&selected, &moved).unwrap();
    std::fs::create_dir(&selected).unwrap();
    std::fs::write(selected.join("sample.txt"), "marker replacement-root\n").unwrap();
    let reply = handle
        .request(
            request(json!({"query":"marker","path":"sample.txt"}), 16 * 1024),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(reply.value["matches"][0]["text"], "marker");
    assert_eq!(
        reply.value["matches"][0]["context"]["text"],
        "marker original-root\n"
    );
    assert!(
        !serde_json::to_string(&reply.value)
            .unwrap()
            .contains("replacement-root")
    );
    handle.disconnect();
}

#[tokio::test]
async fn invalid_operations_arguments_and_pre_cancel_do_not_scan() {
    let root = tempfile::tempdir().unwrap();
    let source = WorkspaceSearch::open(root.path().canonicalize().unwrap()).unwrap();
    let stats = source.stats();
    let handle = source.into_handle();
    for arguments in [
        json!({"query":""}),
        json!({"query":"marker","limit":0}),
        json!({"query":"marker","limit":1001}),
        json!({"query":"marker","limit":"1"}),
        json!({"query":"marker","role":"system"}),
        json!({"query":"marker","path":""}),
    ] {
        let error = handle
            .request(request(arguments, 4096), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(!error.outcome_unknown);
    }
    let mut wrong = request(json!({"query":"marker"}), 4096);
    wrong.operation = SourceOperation::InvokeTool {
        name: "add".into(),
        arguments: json!({}),
    };
    assert!(
        !handle
            .request(wrong, CancellationToken::new())
            .await
            .unwrap_err()
            .outcome_unknown
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        !handle
            .request(request(json!({"query":"marker"}), 4096), cancel)
            .await
            .unwrap_err()
            .outcome_unknown
    );
    assert_eq!(stats.snapshot().queries, 0);
    handle.disconnect();
    assert!(
        !handle
            .request(
                request(json!({"query":"marker"}), 4096),
                CancellationToken::new()
            )
            .await
            .unwrap_err()
            .outcome_unknown
    );
    assert_eq!(stats.snapshot().queries, 0);
}
