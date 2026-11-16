use cayw_ai::{AssistantMessage, CancellationToken, Content, Message, Model, StopReason, ToolCall};
use cayw_core::{EventRecord, SessionState, SessionStore};
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_runtime::{Runtime, RuntimeConfig};
use serde_json::{Value, json};
use std::{net::SocketAddr, path::Path, process::Output, sync::Arc, time::Duration};
use tokio::{net::TcpListener, process::Command, task::JoinHandle};

const TOKEN: &str = "headless-event-page-test-token";
const WATCHDOG: Duration = Duration::from_secs(20);

struct TestServerHandler {
    runtime: Arc<Runtime>,
    request_timeout_ms: Option<u64>,
}

#[async_trait::async_trait]
impl cayw_ipc::RpcHandler for TestServerHandler {
    async fn call(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, cayw_ipc::RpcError> {
        let mut response = self.runtime.call(method, params, cancel).await?;
        if method == "runtime.health"
            && let Value::Object(ref mut map) = response
        {
            map.insert("request_timeout_ms".into(), json!(self.request_timeout_ms));
        }
        Ok(response)
    }
    fn read_only(&self, method: &str, params: &Value) -> bool {
        self.runtime.read_only(method, params)
    }
}

struct TerminalProvider;
impl cayw_ai::Provider for TerminalProvider {
    fn stream(
        &self,
        request: cayw_ai::ModelRequest,
        _: CancellationToken,
    ) -> cayw_ai::AssistantStream {
        let mut message = AssistantMessage::new(&request.model);
        message.content = vec![Content::text("new task completed")];
        message.stop_reason = StopReason::Stop;
        Box::pin(futures::stream::iter([cayw_ai::AssistantEvent::Done {
            message,
        }]))
    }
}

struct Fixture {
    root: tempfile::TempDir,
    runtime: Runtime,
    store: SessionStore,
    id: String,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let database = root.path().join("sessions.sqlite");
        let runtime = Runtime::open(
            &database,
            &workspace,
            Arc::new(TerminalProvider),
            Model::new("fixture", "event-paging"),
            RuntimeConfig::default(),
        )
        .unwrap();
        let id = runtime
            .create(Some("event paging fixture".into()))
            .unwrap()
            .id;
        let store = SessionStore::open(database).unwrap();
        Self {
            root,
            runtime,
            store,
            id,
        }
    }
    fn history(&self, count: usize, bytes: usize) {
        let lease = self
            .store
            .begin_attempt(&self.id, "fixture-worker")
            .unwrap();
        let payload = "x".repeat(bytes);
        for index in 0..count {
            self.store
                .append_host_event(
                    &lease,
                    "paging_fixture",
                    json!({"index":index,"text":payload}),
                )
                .unwrap();
        }
        self.store.finish_attempt(&lease).unwrap();
    }
    fn unknown(&self) {
        let lease = self
            .store
            .begin_attempt(&self.id, "lost-fixture-worker")
            .unwrap();
        let call = ToolCall::new("original-unknown-call", "fixture-effect", json!({}));
        let mut assistant = AssistantMessage::new(&Model::new("fixture", "event-paging"));
        assistant.stop_reason = StopReason::ToolUse;
        assistant.content = vec![Content::ToolCall(call.clone())];
        let message = Message::Assistant(assistant);
        for value in [
            json!({"type":"agent_start"}),
            json!({"type":"turn_start"}),
            json!({"type":"message_start","message":message}),
            json!({"type":"message_end","message":message}),
            json!({"type":"tool_execution_start","call":call}),
        ] {
            self.store
                .append_agent_event(&lease, &serde_json::from_value(value).unwrap())
                .unwrap();
        }
        self.store.mark_tool_started(&lease, &call.id).unwrap();
        let recovered = self.store.recover(&self.id).unwrap();
        assert_eq!(recovered.state, SessionState::Idle);
        assert_eq!(recovered.unresolved_calls.len(), 1);
    }
    fn events(&self) -> Vec<EventRecord> {
        let mut events = Vec::new();
        loop {
            let after = events.last().map_or(0, |event: &EventRecord| event.cursor);
            let page = self.store.events(&self.id, after, 1000).unwrap();
            if page.is_empty() {
                return events;
            }
            events.extend(page);
        }
    }
    fn cursor(&self, after: u64) -> std::path::PathBuf {
        let path = self.root.path().join("events.cursor.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({"session_id":self.id,"after":after})).unwrap(),
        )
        .unwrap();
        path
    }
    async fn server(&self, max_frame_bytes: usize) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let stop = CancellationToken::new();
        let mut config = ServerConfig::new(TOKEN);
        config.max_frame_bytes = max_frame_bytes;
        let request_timeout_ms = config
            .request_timeout
            .map(|timeout| u64::try_from(timeout.as_millis()).unwrap_or(0));
        let handler = Arc::new(TestServerHandler {
            runtime: Arc::new(self.runtime.clone()),
            request_timeout_ms,
        });
        let task = tokio::spawn(serve(listener, handler, config, stop.clone()));
        Server {
            address,
            stop,
            task,
        }
    }
}
struct Server {
    address: SocketAddr,
    stop: CancellationToken,
    task: JoinHandle<std::io::Result<()>>,
}
impl Server {
    async fn command(&self, id: &str, cursor: &Path, args: &[&str]) -> Output {
        tokio::time::timeout(
            WATCHDOG,
            Command::new(env!("CARGO_BIN_EXE_cayw-client"))
                .args([
                    "--mode",
                    "ipc",
                    "--address",
                    &self.address.to_string(),
                    "--token-env",
                    "CAYW_PAGING_FIXTURE_TOKEN",
                ])
                .args(args)
                .args(["--session", id, "--cursor-file"])
                .arg(cursor)
                .env("CAYW_PAGING_FIXTURE_TOKEN", TOKEN)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("client must finish within the watchdog")
        .unwrap()
    }
    async fn close(self) {
        self.stop.cancel();
        tokio::time::timeout(WATCHDOG, self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
fn rows(bytes: &[u8]) -> Vec<Value> {
    std::str::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn emitted_events(output: &Output) -> Vec<EventRecord> {
    rows(&output.stdout)
        .into_iter()
        .filter(|row| row["type"] == "event")
        .map(|row| serde_json::from_value(row["event"].clone()).unwrap())
        .collect()
}
fn saved_cursor(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[tokio::test]
async fn large_history_and_original_unknown_are_read_without_replaying_work() {
    let fixture = Fixture::new();
    fixture.history(1000, 2200);
    fixture.unknown();
    let expected = fixture.events();
    let first_page_bytes = serde_json::to_vec(&expected[..1000]).unwrap().len();
    assert!(first_page_bytes > 1024 * 1024);
    let before = fixture.runtime.get(&fixture.id).unwrap();
    let cursor = fixture.cursor(0);
    let server = fixture.server(1024 * 1024).await;
    let client = Client::connect(server.address, TOKEN).await.unwrap();
    let page = client
        .request(
            "session.events",
            json!({"session_id":fixture.id,"after":0,"limit":1000}),
        )
        .await
        .unwrap();
    let page: Vec<EventRecord> = serde_json::from_value(page).unwrap();
    assert_eq!(page, expected[..1000]);
    client.close().await;
    let mut observed = Vec::new();
    loop {
        let output = server
            .command(&fixture.id, &cursor, &["events", "--limit", "1000"])
            .await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let page = emitted_events(&output);
        if page.is_empty() {
            break;
        }
        observed.extend(page);
    }
    server.close().await;
    assert_eq!(observed, expected);
    assert_eq!(saved_cursor(&cursor)["after"], before.cursor);
    assert!(fixture.runtime.tasks(&fixture.id).unwrap().is_empty());
    assert_eq!(fixture.runtime.get(&fixture.id).unwrap(), before);
    assert_eq!(fixture.events(), expected);
    eprintln!(
        "read {} original events; oversized first page {first_page_bytes} bytes; unknown retained; no task enqueued",
        expected.len()
    );
}

#[tokio::test]
async fn events_pages_continue_from_saved_cursor_without_gaps_or_repeats() {
    let fixture = Fixture::new();
    fixture.history(1000, 2200);
    let expected = fixture.events();
    let start = 7;
    let cursor = fixture.cursor(start);
    let server = fixture.server(1024 * 1024).await;
    let mut observed = Vec::new();
    loop {
        let before = saved_cursor(&cursor)["after"].as_u64().unwrap();
        let output = server
            .command(&fixture.id, &cursor, &["events", "--limit", "1000"])
            .await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let page = emitted_events(&output);
        if page.is_empty() {
            assert_eq!(saved_cursor(&cursor)["after"], before);
            break;
        }
        assert_eq!(page[0].cursor, before + 1);
        assert_eq!(saved_cursor(&cursor)["after"], page.last().unwrap().cursor);
        observed.extend(page);
        assert!(observed.len() <= expected.len() - start as usize);
    }
    server.close().await;
    assert_eq!(observed, expected[start as usize..]);
    assert_eq!(fixture.events(), expected);
    assert!(fixture.runtime.tasks(&fixture.id).unwrap().is_empty());
    eprintln!(
        "read {} events after cursor {start}; no gaps or repeats",
        observed.len()
    );
}

#[tokio::test]
async fn single_oversized_event_is_reassembled_without_gaps_or_replays() {
    let fixture = Fixture::new();
    fixture.history(1, 32 * 1024);
    let expected = fixture.events();
    let oversized = expected
        .iter()
        .find(|event| serde_json::to_vec(event).unwrap().len() > 8192)
        .unwrap();
    let start = oversized.cursor - 1;
    let cursor = fixture.cursor(start);
    let server = fixture.server(8192).await;
    let output = server
        .command(&fixture.id, &cursor, &["events", "--limit", "1000"])
        .await;
    server.close().await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(emitted_events(&output), expected[start as usize..]);
    assert_eq!(
        saved_cursor(&cursor)["after"],
        expected.last().unwrap().cursor
    );
    assert!(fixture.runtime.tasks(&fixture.id).unwrap().is_empty());
    assert_eq!(fixture.events(), expected);
}

#[tokio::test]
async fn prompt_after_historical_unknown_completes_without_replaying_the_original() {
    let fixture = Fixture::new();
    fixture.history(1000, 2200);
    fixture.unknown();
    let original = fixture.events();
    let cursor = fixture.cursor(0);
    let server = fixture.server(1024 * 1024).await;
    let output = server
        .command(
            &fixture.id,
            &cursor,
            &["prompt", "--text", "Start a separate harmless task."],
        )
        .await;
    server.close().await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let final_events = fixture.events();
    assert_eq!(&final_events[..original.len()], original.as_slice());
    assert_eq!(fixture.runtime.tasks(&fixture.id).unwrap().len(), 1);
    let view = fixture.runtime.get(&fixture.id).unwrap();
    assert_eq!(view.unresolved_calls.len(), 1);
    assert!(
        serde_json::to_string(&view)
            .unwrap()
            .contains("new task completed")
    );
}
