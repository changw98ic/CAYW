//! Reads one real Runtime event log through the plugin collector.
//!
//! The model provider is never called. Large events are synthetic host records.

use cayw_ai::{AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_runtime::{Runtime, RuntimeConfig};
use cayw_trace_debug::TraceStore;
use serde_json::json;
use std::{error::Error, sync::Arc, time::Duration};
use tempfile::TempDir;
use tokio::net::TcpListener;

#[allow(dead_code)]
#[path = "../src/main.rs"]
mod plugin;

struct NoRequests;
impl Provider for NoRequests {
    fn stream(&self, _: ModelRequest, _: CancellationToken) -> AssistantStream {
        panic!("this event read must not request a model")
    }
}

#[tokio::test]
async fn large_event_pages_are_read_without_skipping() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let runtime = Runtime::open(
        root.path().join("core.sqlite"),
        &workspace,
        Arc::new(NoRequests),
        Model::new("synthetic", "no-generation"),
        RuntimeConfig::default(),
    )?;
    let session = runtime.create(Some("large event page".into()))?.id;
    let mut cursor = runtime.events(&session, 0, 32)?.last().unwrap().cursor;
    for _ in 0..24 {
        cursor = runtime
            .store()
            .append_control_event(
                &session,
                cursor,
                None,
                "review-host",
                "review-page",
                json!({"text": "x".repeat(48 * 1024)}),
            )?
            .cursor;
    }
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let token = "issue31-observe-page-token-0123456789abcd";
    let cancel = tokio_util::sync::CancellationToken::new();
    let server = tokio::spawn(cayw_ipc::serve(
        listener,
        Arc::new(runtime),
        cayw_ipc::ServerConfig::new(token),
        cancel.clone(),
    ));
    let store = TraceStore::open(root.path().join("trace.json"))?;
    let collector = tokio::spawn(plugin::observe(
        store.clone(),
        address,
        token.into(),
        session.clone(),
        cancel.clone(),
    ));

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let saved = loop {
        let document = store.snapshot();
        let after = document.sessions.get(&session).map(|capture| capture.after);
        if after == Some(cursor) {
            break document;
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "collector stopped at {after:?}: {:?}",
                document
                    .sessions
                    .get(&session)
                    .and_then(|capture| capture.error.clone())
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    assert_eq!(saved.events.len(), cursor as usize);
    assert_eq!(
        saved
            .events
            .iter()
            .map(|event| event.cursor)
            .collect::<Vec<_>>(),
        (1..=cursor).collect::<Vec<_>>()
    );
    assert!(saved.sessions[&session].error.is_none());
    assert_eq!(
        saved
            .events
            .iter()
            .filter(|event| matches!(
                &event.event,
                cayw_core::CoreEvent::Host { kind, .. } if kind == "review-page"
            ))
            .count(),
        24
    );

    cancel.cancel();
    collector.await??;
    server.await??;
    Ok(())
}
