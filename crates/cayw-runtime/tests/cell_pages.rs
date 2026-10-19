use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
};
use cayw_cells::{CellOutput, CellPage, CellStatus};
use cayw_core::SessionState;
use cayw_runtime::{Runtime, RuntimeConfig};
use std::sync::{Arc, Mutex};

struct PagedScript {
    script: String,
    requests: Mutex<Vec<ModelRequest>>,
}
impl Provider for PagedScript {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let mut requests = self.requests.lock().unwrap();
        let reply = cayw_runtime::fixture::script_response(
            &request,
            &self.script,
            &format!("page-call-{}", requests.len()),
        );
        requests.push(request);
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message: reply,
        }]))
    }
}

#[tokio::test]
async fn offline_fixture_uses_advertised_limits_and_reads_every_output_page() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("workspace")).unwrap();
    let provider = Arc::new(PagedScript {
        script:
            "for i=1,12 do text('piece-'..i..':'..string.rep('x',3500)) end; return 'last-piece'"
                .into(),
        requests: Mutex::default(),
    });
    let mut config = RuntimeConfig::default();
    config.cells.max_yield_time = std::time::Duration::from_millis(500);
    config.cells.max_output_tokens = 16 * 1024;
    config.cells.item_bytes = 12 * 1024;
    let rt = Runtime::open(
        directory.path().join("session.db"),
        directory.path().join("workspace"),
        provider.clone(),
        Model::new("fixture", "bounded-cell-pages"),
        config,
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    let result = rt
        .prompt(
            &id,
            "read every bounded output page",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.state, SessionState::Idle);
    let mut pages = Vec::new();
    for message in &result.messages {
        if let Message::ToolResult(result) = message {
            assert!(!result.is_error, "{result:?}");
            let value = result
                .structured_content
                .clone()
                .expect("a cell output page");
            assert!(serde_json::to_vec(&value).unwrap().len() <= 16 * 1024);
            pages.push(serde_json::from_value::<CellPage>(value).unwrap());
        }
    }
    assert!(pages.len() >= 3, "multiple bounded pages must be consumed");
    let last = pages.last().unwrap();
    assert_eq!(last.status, CellStatus::Completed);
    assert!(!last.has_more && !last.vm_alive);
    let mut cursor = 0;
    let mut delivered = Vec::new();
    for page in &pages {
        assert_eq!(page.cell_id, last.cell_id);
        assert_eq!(page.cursor, cursor);
        for entry in &page.outputs {
            cursor += 1;
            assert_eq!(entry.cursor, cursor);
            delivered.push(entry.output.clone());
        }
        assert_eq!(page.next_cursor, cursor);
    }
    assert_eq!(delivered.len(), 13);
    for (index, output) in delivered.iter().take(12).enumerate() {
        assert!(matches!(output, CellOutput::Text { text }
            if text == &format!("piece-{}:{}", index + 1, "x".repeat(3500))));
    }
    let final_text = result.messages.last().unwrap().text();
    assert!(final_text.contains("piece-1:"));
    assert!(final_text.contains("piece-12:"));
    assert!(final_text.contains("last-piece"));
    assert_eq!(result.executions.len(), 1);
    assert!(result.unresolved_capabilities.is_empty());
    let requests = provider.requests.lock().unwrap();
    let calls: Vec<_> = result
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant.tool_calls()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(calls.len(), pages.len());
    assert_eq!(calls[0].arguments["action"], "exec");
    for call in &calls {
        assert!(call.arguments["max_output_tokens"].as_u64().unwrap() <= 16 * 1024);
        assert!(call.arguments["yield_time_ms"].as_u64().unwrap() <= 500);
    }
    assert!(
        calls
            .iter()
            .skip(1)
            .all(|call| call.arguments["action"] == "wait")
    );
    assert_eq!(requests.len(), calls.len() + 1);
}
