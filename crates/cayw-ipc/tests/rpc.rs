use async_trait::async_trait;
use cayw_ipc::{Client, ClientConfig, RpcError, RpcHandler, ServerConfig, serve};
use serde_json::{Value, json};
use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream, tcp::OwnedReadHalf, tcp::OwnedWriteHalf},
    sync::Semaphore,
    task::{JoinHandle, JoinSet},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "test-secret-that-must-not-appear-in-debug";
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

struct Fixture {
    calls: AtomicUsize,
    active: Arc<AtomicUsize>,
    started: Semaphore,
    cancelled: Semaphore,
}

impl Default for Fixture {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            active: Arc::new(AtomicUsize::new(0)),
            started: Semaphore::new(0),
            cancelled: Semaphore::new(0),
        }
    }
}

struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl RpcHandler for Fixture {
    async fn call(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.active.fetch_add(1, Ordering::SeqCst);
        let _active = Active(self.active.clone());
        match method {
            "hold" => {
                self.started.add_permits(1);
                cancel.cancelled().await;
                self.cancelled.add_permits(1);
                Err(RpcError::new("cancelled", "handler observed cancellation"))
            }
            "ignore_cancel" => {
                self.started.add_permits(1);
                std::future::pending().await
            }
            "large" => Ok(Value::String("x".repeat(4 * 1024 * 1024))),
            "deep_result" => Ok(nested_arrays(64)),
            "error" => Err(RpcError::new("fixture_failure", "known failure")),
            "panic" => panic!("fixture panic"),
            _ => {
                if let Some(delay) = params.get("delay").and_then(Value::as_u64) {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                Ok(params)
            }
        }
    }
}

struct Server {
    addr: SocketAddr,
    handler: Arc<Fixture>,
    cancel: CancellationToken,
    task: JoinHandle<io::Result<()>>,
}

impl Server {
    async fn start(config: ServerConfig) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handler = Arc::new(Fixture::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(serve(listener, handler.clone(), config, cancel.clone()));
        Self {
            addr,
            handler,
            cancel,
            task,
        }
    }

    async fn stop(self) {
        self.cancel.cancel();
        timeout(TEST_TIMEOUT, self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(self.handler.active.load(Ordering::SeqCst), 0);
    }

    async fn raw(&self) -> Raw {
        Raw::connect(self.addr).await
    }
}

struct Raw {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Raw {
    async fn connect_peer(listener: TcpListener) -> Self {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, writer) = stream.into_split();
        Self {
            reader: BufReader::new(reader),
            writer,
        }
    }

    async fn connect(addr: SocketAddr) -> Self {
        let (reader, writer) = TcpStream::connect(addr).await.unwrap().into_split();
        Self {
            reader: BufReader::new(reader),
            writer,
        }
    }

    async fn send(&mut self, value: Value) {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        self.writer.write_all(&bytes).await.unwrap();
    }

    async fn receive(&mut self) -> Option<Value> {
        let mut line = String::new();
        match timeout(TEST_TIMEOUT, self.reader.read_line(&mut line))
            .await
            .unwrap()
        {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(serde_json::from_str(&line).unwrap()),
        }
    }

    async fn hello(&mut self) {
        self.send(json!({"version":1,"id":"hello","method":"hello","params":{"token":TOKEN}}))
            .await;
        let response = self.receive().await.unwrap();
        assert_eq!(response["version"], 1);
        assert_eq!(response["id"], "hello");
        assert_eq!(response["result"]["version"], 1);
    }
}

async fn signalled(semaphore: &Semaphore) {
    timeout(TEST_TIMEOUT, semaphore.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
}

fn config() -> ServerConfig {
    ServerConfig {
        request_timeout: Duration::from_secs(5),
        ..ServerConfig::new(TOKEN)
    }
}

fn request(id: &str, method: &str, params: Value) -> Value {
    json!({"version":1,"id":id,"method":method,"params":params})
}

#[tokio::test]
async fn authenticated_client_round_trip_preserves_null_and_rpc_errors() {
    fn send_sync<T: Send + Sync + Clone>() {}
    send_sync::<Client>();
    let server = Server::start(config()).await;
    let client = Client::connect(server.addr, TOKEN).await.unwrap();
    assert_eq!(
        client
            .request("echo", json!({"text":"你好","n":3}))
            .await
            .unwrap(),
        json!({"text":"你好","n":3})
    );
    assert_eq!(
        client.request("echo", Value::Null).await.unwrap(),
        Value::Null
    );
    let error = client.request("error", Value::Null).await.unwrap_err();
    assert_eq!(error.code, "fixture_failure");
    assert!(!error.outcome_unknown);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn hello_requires_authentication_and_current_version_before_handler() {
    let server = Server::start(config()).await;
    let error = Client::connect(server.addr, "wrong-token")
        .await
        .unwrap_err();
    assert_eq!(error.code, "unauthorized");
    assert!(!error.outcome_unknown);
    for (version, method, token, expected) in [
        (99, "hello", TOKEN, "unsupported_version"),
        (1, "echo", TOKEN, "handshake_required"),
        (1, "hello", "wrong", "unauthorized"),
    ] {
        let mut raw = server.raw().await;
        raw.send(json!({"version":version,"id":"h","method":method,"params":{"token":token}}))
            .await;
        let response = raw.receive().await.unwrap();
        assert_eq!(response["error"]["code"], expected);
        assert!(raw.receive().await.is_none());
    }
    assert_eq!(server.handler.calls.load(Ordering::SeqCst), 0);
    server.stop().await;
}

#[tokio::test]
async fn concurrent_requests_are_paired_with_their_own_response() {
    let server = Server::start(ServerConfig {
        max_in_flight: 128,
        response_queue_capacity: 128,
        ..config()
    })
    .await;
    let client = Client::connect_with_config(
        server.addr,
        TOKEN,
        ClientConfig {
            max_in_flight: 128,
            write_queue_capacity: 128,
            ..ClientConfig::default()
        },
    )
    .await
    .unwrap();
    let mut tasks = JoinSet::new();
    for n in 0..64 {
        let client = client.clone();
        tasks.spawn(async move {
            let value = json!({"number":n,"delay":(63-n)%7});
            assert_eq!(client.request("echo", value.clone()).await.unwrap(), value);
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
    }
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn cancel_frame_cancels_handler_once_and_preserves_connection() {
    let server = Server::start(config()).await;
    let mut raw = server.raw().await;
    raw.hello().await;
    raw.send(request("long", "hold", Value::Null)).await;
    signalled(&server.handler.started).await;
    raw.send(request(
        "cancel-long",
        "rpc.cancel",
        json!({"request_id":"long"}),
    ))
    .await;
    let first = raw.receive().await.unwrap();
    let second = raw.receive().await.unwrap();
    let mut replies = [first, second];
    replies.sort_by_key(|value| value["id"].as_str().unwrap().to_owned());
    assert_eq!(replies[0]["id"], "cancel-long");
    assert_eq!(replies[0]["result"]["cancelled"], true);
    assert_eq!(replies[1]["id"], "long");
    assert_eq!(replies[1]["error"]["code"], "cancelled_outcome_unknown");
    signalled(&server.handler.cancelled).await;
    raw.send(request("after", "echo", json!(42))).await;
    assert_eq!(
        raw.receive().await.unwrap(),
        json!({"version":1,"id":"after","result":42})
    );
    drop(raw);
    server.stop().await;
}

#[tokio::test]
async fn client_cancellation_waits_for_a_terminal_response() {
    let server = Server::start(config()).await;
    let client = Client::connect(server.addr, TOKEN).await.unwrap();
    let cancel = CancellationToken::new();
    let task = {
        let client = client.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            client
                .request_with_cancel("hold", Value::Null, cancel)
                .await
        })
    };
    signalled(&server.handler.started).await;
    cancel.cancel();
    let error = timeout(TEST_TIMEOUT, task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, "cancelled_outcome_unknown");
    assert!(error.outcome_unknown);
    signalled(&server.handler.cancelled).await;
    assert_eq!(client.request("echo", json!(7)).await.unwrap(), 7);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn disconnect_cancels_and_drops_all_handler_futures() {
    let server = Server::start(config()).await;
    let mut raw = server.raw().await;
    raw.hello().await;
    raw.send(request("long", "hold", Value::Null)).await;
    signalled(&server.handler.started).await;
    drop(raw);
    signalled(&server.handler.cancelled).await;
    server.stop().await;
}

#[tokio::test]
async fn shutdown_drops_a_handler_that_ignores_cancellation() {
    let server = Server::start(ServerConfig {
        cancellation_grace: Duration::from_millis(50),
        ..config()
    })
    .await;
    let mut raw = server.raw().await;
    raw.hello().await;
    raw.send(request("long", "ignore_cancel", Value::Null))
        .await;
    signalled(&server.handler.started).await;
    server.stop().await;
}

#[tokio::test]
async fn frame_limit_is_enforced_before_newline_and_bad_input_never_calls_handler() {
    let server = Server::start(ServerConfig {
        max_frame_bytes: 512,
        ..config()
    })
    .await;
    let mut raw = server.raw().await;
    raw.hello().await;
    raw.writer.write_all(&vec![b'x'; 513]).await.unwrap();
    assert!(raw.receive().await.is_none());
    for bytes in [
        b"\xff\n".as_slice(),
        b"{bad-json}\n".as_slice(),
        b"\n".as_slice(),
    ] {
        let mut raw = server.raw().await;
        raw.writer.write_all(bytes).await.unwrap();
        assert!(raw.receive().await.is_none());
    }
    assert_eq!(server.handler.calls.load(Ordering::SeqCst), 0);
    server.stop().await;
}

#[tokio::test]
async fn invalid_duplicate_and_unknown_cancel_ids_close_connection() {
    let server = Server::start(config()).await;
    for id in ["".to_owned(), "x".repeat(129)] {
        let mut raw = server.raw().await;
        raw.hello().await;
        raw.send(request(&id, "echo", Value::Null)).await;
        assert!(raw.receive().await.is_none());
    }
    let mut raw = server.raw().await;
    raw.hello().await;
    raw.send(request("once", "echo", json!(1))).await;
    assert_eq!(raw.receive().await.unwrap()["result"], 1);
    raw.send(request("once", "echo", json!(2))).await;
    assert!(raw.receive().await.is_none());
    let mut raw = server.raw().await;
    raw.hello().await;
    raw.send(request(
        "cancel",
        "rpc.cancel",
        json!({"request_id":"not-on-this-connection"}),
    ))
    .await;
    assert!(raw.receive().await.is_none());
    assert_eq!(server.handler.calls.load(Ordering::SeqCst), 1);
    server.stop().await;
}

#[tokio::test]
async fn duplicate_in_flight_id_cancels_original_instead_of_calling_twice() {
    let server = Server::start(config()).await;
    let mut raw = server.raw().await;
    raw.hello().await;
    raw.send(request("same", "hold", Value::Null)).await;
    signalled(&server.handler.started).await;
    raw.send(request("same", "echo", Value::Null)).await;
    assert!(raw.receive().await.is_none());
    signalled(&server.handler.cancelled).await;
    assert_eq!(server.handler.calls.load(Ordering::SeqCst), 1);
    server.stop().await;
}

#[tokio::test]
async fn connection_lifetime_bounds_the_seen_id_set() {
    let server = Server::start(ServerConfig {
        max_requests_per_connection: 3,
        ..config()
    })
    .await;
    let mut raw = server.raw().await;
    raw.hello().await;
    for id in ["one", "two"] {
        raw.send(request(id, "echo", json!(id))).await;
        assert_eq!(raw.receive().await.unwrap()["result"], id);
    }
    raw.send(request("three", "echo", Value::Null)).await;
    assert!(raw.receive().await.is_none());
    assert_eq!(server.handler.calls.load(Ordering::SeqCst), 2);
    let client = Client::connect(server.addr, TOKEN).await.unwrap();
    assert_eq!(client.request("echo", json!(1)).await.unwrap(), 1);
    assert_eq!(client.request("echo", json!(2)).await.unwrap(), 2);
    let error = client.request("echo", json!(3)).await.unwrap_err();
    assert_eq!(error.code, "connection_limit");
    assert!(!error.outcome_unknown);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn concurrency_limit_rejects_work_but_still_reads_cancellation() {
    let server = Server::start(ServerConfig {
        max_in_flight: 1,
        ..config()
    })
    .await;
    let mut raw = server.raw().await;
    raw.hello().await;
    raw.send(request("long", "hold", Value::Null)).await;
    signalled(&server.handler.started).await;
    raw.send(request("extra", "echo", Value::Null)).await;
    assert_eq!(raw.receive().await.unwrap()["error"]["code"], "busy");
    raw.send(request(
        "cancel",
        "rpc.cancel",
        json!({"request_id":"long"}),
    ))
    .await;
    let a = raw.receive().await.unwrap();
    let b = raw.receive().await.unwrap();
    assert_ne!(a["id"], b["id"]);
    signalled(&server.handler.cancelled).await;
    assert_eq!(server.handler.calls.load(Ordering::SeqCst), 1);
    drop(raw);
    server.stop().await;
}

#[tokio::test]
async fn slow_consumer_is_disconnected_and_other_clients_keep_working() {
    let server = Server::start(ServerConfig {
        max_frame_bytes: 8 * 1024 * 1024,
        response_queue_capacity: 2,
        write_timeout: Duration::from_millis(150),
        ..config()
    })
    .await;
    let mut raw = server.raw().await;
    raw.hello().await;
    raw.send(request("held", "hold", Value::Null)).await;
    signalled(&server.handler.started).await;
    for n in 0..12 {
        let value = request(&format!("large-{n}"), "large", Value::Null);
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        if raw.writer.write_all(&bytes).await.is_err() {
            break;
        }
    }
    signalled(&server.handler.cancelled).await;
    let client = Client::connect(server.addr, TOKEN).await.unwrap();
    assert_eq!(
        client
            .request("echo", json!("still available"))
            .await
            .unwrap(),
        "still available"
    );
    client.close().await;
    drop(raw);
    server.stop().await;
}

#[tokio::test]
async fn client_timeout_reports_unknown_and_cancels_remote_work() {
    let server = Server::start(config()).await;
    let client = Client::connect_with_config(
        server.addr,
        TOKEN,
        ClientConfig {
            request_timeout: Duration::from_millis(100),
            ..ClientConfig::default()
        },
    )
    .await
    .unwrap();
    let error = client.request("hold", Value::Null).await.unwrap_err();
    assert_eq!(error.code, "request_timeout");
    assert!(error.outcome_unknown);
    signalled(&server.handler.cancelled).await;
    assert!(client.request("echo", Value::Null).await.is_err());
    client.close().await;
    assert_eq!(server.handler.calls.load(Ordering::SeqCst), 1);
    server.stop().await;
}

#[tokio::test]
async fn server_timeout_and_oversized_response_report_unknown_outcome() {
    let server = Server::start(ServerConfig {
        request_timeout: Duration::from_millis(100),
        max_frame_bytes: 512,
        ..config()
    })
    .await;
    let client = Client::connect(server.addr, TOKEN).await.unwrap();
    let error = client.request("hold", Value::Null).await.unwrap_err();
    assert_eq!(error.code, "timeout_outcome_unknown");
    assert!(error.outcome_unknown);
    signalled(&server.handler.cancelled).await;
    let error = client.request("large", Value::Null).await.unwrap_err();
    assert_eq!(error.code, "response_too_large_outcome_unknown");
    assert!(error.outcome_unknown);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn dropping_last_client_or_request_future_closes_transport() {
    for drop_request in [false, true] {
        let server = Server::start(config()).await;
        let client = Client::connect(server.addr, TOKEN).await.unwrap();
        let task = {
            let client = client.clone();
            tokio::spawn(async move { client.request("hold", Value::Null).await })
        };
        signalled(&server.handler.started).await;
        if drop_request {
            task.abort();
            let _ = task.await;
        } else {
            client.close().await;
            assert!(task.await.unwrap().unwrap_err().outcome_unknown);
        }
        signalled(&server.handler.cancelled).await;
        drop(client);
        server.stop().await;
    }
    let server = Server::start(config()).await;
    let client = Client::connect(server.addr, TOKEN).await.unwrap();
    drop(client);
    server.stop().await;
}

async fn fake_hello(raw: &mut Raw) {
    let hello = raw.receive().await.unwrap();
    assert_eq!(hello["method"], "hello");
    raw.send(json!({"version":1,"id":hello["id"],"result":{
        "version":1,"max_frame_bytes":1048576,"max_in_flight":64,
        "max_requests_per_connection":65536,"max_id_bytes":128
    }}))
    .await;
}

#[tokio::test]
async fn invalid_response_id_version_or_shape_fails_all_pending_as_unknown() {
    for mode in ["unknown", "version", "both", "duplicate"] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, writer) = stream.into_split();
            let mut raw = Raw {
                reader: BufReader::new(reader),
                writer,
            };
            fake_hello(&mut raw).await;
            let first = raw.receive().await.unwrap();
            let _second = raw.receive().await.unwrap();
            let value = match mode {
                "version" => json!({"version":2,"id":first["id"],"result":1}),
                "both" => {
                    json!({"version":1,"id":first["id"],"result":1,"error":{"code":"x","message":"x"}})
                }
                "duplicate" => {
                    raw.send(json!({"version":1,"id":first["id"],"result":1}))
                        .await;
                    json!({"version":1,"id":first["id"],"result":2})
                }
                _ => json!({"version":1,"id":"unknown","result":1}),
            };
            raw.send(value).await;
            assert!(raw.receive().await.is_none());
        });
        let client = Client::connect(addr, TOKEN).await.unwrap();
        let (a, b) = tokio::join!(
            client.request("one", Value::Null),
            client.request("two", Value::Null)
        );
        let failures = [a, b]
            .into_iter()
            .filter_map(Result::err)
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), if mode == "duplicate" { 1 } else { 2 });
        assert!(failures.iter().all(|error| error.outcome_unknown));
        client.close().await;
        timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn unsolicited_response_buffered_with_hello_is_not_discarded() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, writer) = stream.into_split();
        let mut raw = Raw {
            reader: BufReader::new(reader),
            writer,
        };
        let hello = raw.receive().await.unwrap();
        let mut bytes = serde_json::to_vec(&json!({"version":1,"id":hello["id"],"result":{
            "version":1,"max_frame_bytes":1048576,"max_in_flight":64,
            "max_requests_per_connection":65536,"max_id_bytes":128
        }}))
        .unwrap();
        bytes.extend_from_slice(b"\n{\"version\":1,\"id\":\"unsolicited\",\"result\":null}\n");
        raw.writer.write_all(&bytes).await.unwrap();
        if let Some(request) = raw.receive().await {
            let response = json!({"version":1,"id":request["id"],"result":1});
            let mut bytes = serde_json::to_vec(&response).unwrap();
            bytes.push(b'\n');
            let _ = raw.writer.write_all(&bytes).await;
        }
    });
    match Client::connect(addr, TOKEN).await {
        Err(error) => {
            assert_eq!(error.code, "protocol_error");
            assert!(!error.outcome_unknown);
        }
        Ok(client) => {
            assert!(client.request("echo", Value::Null).await.is_err());
            client.close().await;
        }
    }
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
}

#[tokio::test]
async fn connection_loss_never_reconnects_or_replays_a_request() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, writer) = stream.into_split();
        let mut raw = Raw {
            reader: BufReader::new(reader),
            writer,
        };
        fake_hello(&mut raw).await;
        assert_eq!(raw.receive().await.unwrap()["method"], "side-effect");
        drop(raw);
        seen_tx.send(()).unwrap();
        assert!(
            timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err()
        );
    });
    let client = Client::connect(addr, TOKEN).await.unwrap();
    let error = client
        .request("side-effect", json!({"amount":1}))
        .await
        .unwrap_err();
    assert!(error.outcome_unknown);
    seen_rx.await.unwrap();
    let error = client
        .request("side-effect", json!({"amount":1}))
        .await
        .unwrap_err();
    assert!(!error.outcome_unknown);
    client.close().await;
    task.await.unwrap();
}

#[tokio::test]
async fn local_validation_and_configuration_reject_invalid_limits_without_secrets() {
    let mut server_config = config();
    assert!(!format!("{server_config:?}").contains(TOKEN));
    server_config.max_in_flight = 0;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    assert_eq!(
        serve(
            listener,
            Arc::new(Fixture::default()),
            server_config,
            CancellationToken::new()
        )
        .await
        .unwrap_err()
        .kind(),
        io::ErrorKind::InvalidInput
    );
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).await.unwrap();
    assert_eq!(
        serve(
            listener,
            Arc::new(Fixture::default()),
            config(),
            CancellationToken::new()
        )
        .await
        .unwrap_err()
        .kind(),
        io::ErrorKind::InvalidInput
    );
    let error = Client::connect(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 12),
        TOKEN,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "invalid_address");
    let server = Server::start(config()).await;
    let error = Client::connect_with_config(
        server.addr,
        TOKEN,
        ClientConfig {
            request_timeout: Duration::ZERO,
            ..ClientConfig::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "invalid_config");
    let client = Client::connect(server.addr, TOKEN).await.unwrap();
    assert!(!format!("{client:?}").contains(TOKEN));
    let error = client.request("", Value::Null).await.unwrap_err();
    assert!(!error.outcome_unknown);
    let error = client
        .request("echo", Value::String("x".repeat(2 * 1024 * 1024)))
        .await
        .unwrap_err();
    assert!(!error.outcome_unknown);
    assert_eq!(server.handler.calls.load(Ordering::SeqCst), 0);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn cancel_target_is_scoped_to_the_authenticated_connection() {
    let server = Server::start(config()).await;
    let mut first = server.raw().await;
    first.hello().await;
    first
        .send(request("owned-by-first", "hold", Value::Null))
        .await;
    signalled(&server.handler.started).await;
    let mut second = server.raw().await;
    second.hello().await;
    second
        .send(request(
            "cancel-first",
            "rpc.cancel",
            json!({"request_id":"owned-by-first"}),
        ))
        .await;
    assert!(second.receive().await.is_none());
    assert_eq!(server.handler.active.load(Ordering::SeqCst), 1);
    first
        .send(request(
            "cancel-own",
            "rpc.cancel",
            json!({"request_id":"owned-by-first"}),
        ))
        .await;
    assert!(first.receive().await.is_some());
    assert!(first.receive().await.is_some());
    signalled(&server.handler.cancelled).await;
    drop(first);
    server.stop().await;
}

#[tokio::test]
async fn partial_frame_survives_handler_completion_and_rejects_slow_frames() {
    let server = Server::start(ServerConfig {
        frame_timeout: Duration::from_millis(250),
        handshake_timeout: Duration::from_millis(250),
        ..config()
    })
    .await;
    let mut raw = server.raw().await;
    raw.hello().await;
    raw.send(request("first", "echo", json!({"delay":30})))
        .await;
    let bytes = serde_json::to_vec(&request("partial", "echo", json!(3))).unwrap();
    let midpoint = bytes.len() / 2;
    raw.writer.write_all(&bytes[..midpoint]).await.unwrap();
    assert_eq!(raw.receive().await.unwrap()["id"], "first");
    raw.writer.write_all(&bytes[midpoint..]).await.unwrap();
    raw.writer.write_all(b"\n").await.unwrap();
    assert_eq!(raw.receive().await.unwrap()["result"], 3);
    raw.writer.write_all(b"{").await.unwrap();
    assert!(raw.receive().await.is_none());
    let mut silent = server.raw().await;
    assert!(silent.receive().await.is_none());
    server.stop().await;
}

#[tokio::test]
async fn cancel_after_completed_call_is_an_acknowledged_noop() {
    let server = Server::start(config()).await;
    let mut raw = server.raw().await;
    raw.hello().await;
    raw.send(request("done", "echo", json!(1))).await;
    assert_eq!(raw.receive().await.unwrap()["result"], 1);
    raw.send(request(
        "cancel-late",
        "rpc.cancel",
        json!({"request_id":"done"}),
    ))
    .await;
    assert_eq!(raw.receive().await.unwrap()["result"]["cancelled"], false);
    assert_eq!(server.handler.calls.load(Ordering::SeqCst), 1);
    drop(raw);
    server.stop().await;
}

#[tokio::test]
async fn handler_panic_returns_unknown_and_releases_connection_slot() {
    let server = Server::start(config()).await;
    let client = Client::connect(server.addr, TOKEN).await.unwrap();
    let error = client.request("panic", Value::Null).await.unwrap_err();
    assert_eq!(error.code, "handler_panic_outcome_unknown");
    assert!(error.outcome_unknown);
    assert_eq!(client.request("echo", json!(8)).await.unwrap(), 8);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn aborted_server_future_drops_in_flight_handler_work() {
    let server = Server::start(config()).await;
    let client = Client::connect(server.addr, TOKEN).await.unwrap();
    let request = {
        let client = client.clone();
        tokio::spawn(async move { client.request("ignore_cancel", Value::Null).await })
    };
    signalled(&server.handler.started).await;
    server.task.abort();
    assert!(server.task.await.unwrap_err().is_cancelled());
    assert!(
        timeout(TEST_TIMEOUT, request)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .outcome_unknown
    );
    assert_eq!(server.handler.active.load(Ordering::SeqCst), 0);
    client.close().await;
}

#[tokio::test]
async fn last_client_drop_shuts_socket_after_other_clones_are_gone() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, writer) = stream.into_split();
        let mut raw = Raw {
            reader: BufReader::new(reader),
            writer,
        };
        fake_hello(&mut raw).await;
        started_tx.send(()).unwrap();
        assert!(raw.receive().await.is_none());
    });
    let first = Client::connect(addr, TOKEN).await.unwrap();
    let last = first.clone();
    drop(first);
    started_rx.await.unwrap();
    assert_eq!(
        last.request("hello", Value::Null).await.unwrap_err().code,
        "invalid_method"
    );
    drop(last);
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
}

#[tokio::test]
async fn client_local_capacity_and_precancelled_calls_never_dispatch() {
    let server = Server::start(config()).await;
    let client = Client::connect_with_config(
        server.addr,
        TOKEN,
        ClientConfig {
            max_in_flight: 1,
            ..ClientConfig::default()
        },
    )
    .await
    .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = client
        .request_with_cancel("echo", Value::Null, cancel)
        .await
        .unwrap_err();
    assert!(!error.outcome_unknown);
    let running = {
        let client = client.clone();
        tokio::spawn(async move { client.request("hold", Value::Null).await })
    };
    signalled(&server.handler.started).await;
    let error = client.request("echo", Value::Null).await.unwrap_err();
    assert_eq!(error.code, "client_busy");
    assert!(!error.outcome_unknown);
    assert_eq!(server.handler.calls.load(Ordering::SeqCst), 1);
    client.close().await;
    assert!(running.await.unwrap().unwrap_err().outcome_unknown);
    signalled(&server.handler.cancelled).await;
    server.stop().await;
}

#[tokio::test]
async fn duplicate_json_fields_and_posthello_wrong_versions_are_rejected() {
    let server = Server::start(config()).await;
    for bytes in [
        b"{\"version\":1,\"id\":\"a\",\"id\":\"b\",\"method\":\"echo\",\"params\":null}\n"
            .as_slice(),
        b"{\"version\":1,\"id\":\"a\",\"method\":\"echo\",\"params\":null,\"extra\":1}\n"
            .as_slice(),
        b"{\"version\":1,\"id\":\"a\",\"method\":\"echo\"}\n".as_slice(),
    ] {
        let mut raw = server.raw().await;
        raw.hello().await;
        raw.writer.write_all(bytes).await.unwrap();
        assert!(raw.receive().await.is_none());
    }
    let mut raw = server.raw().await;
    raw.hello().await;
    raw.send(json!({"version":2,"id":"v","method":"echo","params":null}))
        .await;
    assert_eq!(
        raw.receive().await.unwrap()["error"]["code"],
        "unsupported_version"
    );
    assert!(raw.receive().await.is_none());
    assert_eq!(server.handler.calls.load(Ordering::SeqCst), 0);
    server.stop().await;
}

#[tokio::test]
async fn oversized_or_non_utf8_server_response_fails_all_pending() {
    for mode in ["oversized", "utf8", "json"] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, writer) = stream.into_split();
            let mut raw = Raw {
                reader: BufReader::new(reader),
                writer,
            };
            fake_hello(&mut raw).await;
            let _ = raw.receive().await.unwrap();
            let bytes = match mode {
                "oversized" => vec![b'x'; 513],
                "utf8" => vec![255, b'\n'],
                _ => b"{oops}\n".to_vec(),
            };
            raw.writer.write_all(&bytes).await.unwrap();
            assert!(raw.receive().await.is_none());
        });
        let client = Client::connect_with_config(
            addr,
            TOKEN,
            ClientConfig {
                max_frame_bytes: 512,
                ..ClientConfig::default()
            },
        )
        .await
        .unwrap();
        assert!(
            client
                .request("echo", Value::Null)
                .await
                .unwrap_err()
                .outcome_unknown
        );
        client.close().await;
        timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn client_write_timeout_closes_socket_without_replaying() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, writer) = stream.into_split();
        let mut raw = Raw {
            reader: BufReader::new(reader),
            writer,
        };
        let hello = raw.receive().await.unwrap();
        raw.send(json!({"version":1,"id":hello["id"],"result":{
            "version":1,"max_frame_bytes":67108864,"max_in_flight":64,
            "max_requests_per_connection":65536,"max_id_bytes":128
        }}))
        .await;
        release_rx.await.unwrap();
        // Drain any partially-written request to observe EOF without requiring valid JSON.
        let mut bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut raw.reader, &mut bytes)
            .await
            .unwrap();
        assert!(!bytes.is_empty());
    });
    let client = Client::connect_with_config(
        addr,
        TOKEN,
        ClientConfig {
            max_frame_bytes: 64 * 1024 * 1024,
            write_timeout: Duration::from_millis(100),
            request_timeout: Duration::from_secs(5),
            ..ClientConfig::default()
        },
    )
    .await
    .unwrap();
    let error = client
        .request("large-upload", Value::String("x".repeat(32 * 1024 * 1024)))
        .await
        .unwrap_err();
    assert_eq!(error.code, "write_failed");
    assert!(error.outcome_unknown);
    release_tx.send(()).unwrap();
    client.close().await;
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
}

#[tokio::test]
async fn missing_cancellation_acknowledgment_expires_even_after_original_response() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, writer) = stream.into_split();
        let mut raw = Raw {
            reader: BufReader::new(reader),
            writer,
        };
        fake_hello(&mut raw).await;
        let request = raw.receive().await.unwrap();
        started_tx.send(()).unwrap();
        let cancel = raw.receive().await.unwrap();
        assert_eq!(cancel["method"], "rpc.cancel");
        assert_eq!(cancel["params"]["request_id"], request["id"]);
        raw.send(json!({"version":1,"id":request["id"],"error":{
            "code":"cancelled_outcome_unknown","message":"cancelled"
        }}))
        .await;
        // Never send the cancel frame's acknowledgment. Its deadline must still run.
        assert!(raw.receive().await.is_none());
    });
    let client = Client::connect_with_config(
        addr,
        TOKEN,
        ClientConfig {
            request_timeout: Duration::from_millis(250),
            ..ClientConfig::default()
        },
    )
    .await
    .unwrap();
    let cancel = CancellationToken::new();
    let running = {
        let client = client.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            client
                .request_with_cancel("hold", Value::Null, cancel)
                .await
        })
    };
    started_rx.await.unwrap();
    cancel.cancel();
    assert!(running.await.unwrap().unwrap_err().outcome_unknown);
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
    let error = client.request("echo", Value::Null).await.unwrap_err();
    assert!(!error.outcome_unknown);
    client.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn premature_response_in_hello_write_is_rejected_before_connect_returns() {
    for tail in [
        "{\"version\":1,\"id\":\"1\",\"result\":\"forged success\"}\n",
        "{\"version\":1,\"id\":\"future-request\",\"result\":null}\n",
        "{\"version\":1,\"id\":",
    ] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut raw = Raw::connect_peer(listener).await;
            let hello = raw.receive().await.unwrap();
            let mut bytes = serde_json::to_vec(&json!({"version":1,"id":hello["id"],"result":{
                "version":1,"max_frame_bytes":1048576,"max_in_flight":64,
                "max_requests_per_connection":65536,"max_id_bytes":128
            }}))
            .unwrap();
            bytes.push(b'\n');
            bytes.extend_from_slice(tail.as_bytes());
            raw.writer.write_all(&bytes).await.unwrap();
            assert!(raw.receive().await.is_none());
        });
        let result = Client::connect(addr, TOKEN).await;
        if let Ok(client) = &result {
            client.close().await;
        }
        let error = result.unwrap_err();
        assert_eq!(error.code, "protocol_error");
        assert!(!error.outcome_unknown);
        timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_before_the_full_request_is_written_is_rejected() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut raw = Raw::connect_peer(listener).await;
        let hello = raw.receive().await.unwrap();
        raw.send(json!({"version":1,"id":hello["id"],"result":{
            "version":1,"max_frame_bytes":67108864,"max_in_flight":64,
            "max_requests_per_connection":65536,"max_id_bytes":128
        }}))
        .await;
        // Observe the ID from the header, but never drain the large request body.
        let mut header = Vec::new();
        for _ in 0..2 {
            timeout(TEST_TIMEOUT, raw.reader.read_until(b',', &mut header))
                .await
                .unwrap()
                .unwrap();
        }
        assert_eq!(header.pop(), Some(b','));
        header.push(b'}');
        let prefix: Value = serde_json::from_slice(&header).unwrap();
        raw.send(json!({"version":1,"id":prefix["id"],"result":"forged success"}))
            .await;
        // Keep backpressure until the request has produced its terminal outcome.
        // Draining sooner could let the writer finish before the response is checked.
        timeout(TEST_TIMEOUT, release_rx).await.unwrap().unwrap();
        let mut bytes = Vec::new();
        timeout(
            TEST_TIMEOUT,
            tokio::io::AsyncReadExt::read_to_end(&mut raw.reader, &mut bytes),
        )
        .await
        .unwrap()
        .unwrap();
    });
    let client = Client::connect_with_config(
        addr,
        TOKEN,
        ClientConfig {
            max_frame_bytes: 64 * 1024 * 1024,
            ..ClientConfig::default()
        },
    )
    .await
    .unwrap();
    let result = client
        .request("large-upload", Value::String("x".repeat(32 * 1024 * 1024)))
        .await;
    release_tx.send(()).unwrap();
    client.close().await;
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
    let error = result.unwrap_err();
    assert_eq!(error.code, "protocol_error");
    assert!(error.outcome_unknown);
}

#[tokio::test]
async fn duplicate_keys_in_hello_and_nested_business_params_never_dispatch() {
    let server = Server::start(config()).await;
    for params in [
        format!(r#"{{"token":"wrong","token":"{TOKEN}"}}"#),
        format!(
            r#"{{"token":"wrong","tok{}u0065n":"{TOKEN}"}}"#,
            char::from(92)
        ),
    ] {
        let mut raw = server.raw().await;
        let frame = format!(r#"{{"version":1,"id":"h","method":"hello","params":{params}}}"#);
        raw.writer.write_all(frame.as_bytes()).await.unwrap();
        raw.writer.write_all(b"\n").await.unwrap();
        assert!(
            raw.receive().await.is_none(),
            "duplicate hello token accepted"
        );
    }
    for params in [
        r#"{"x":1,"x":2}"#.to_owned(),
        format!(r#"{{"items":[{{"x":1,"{}u0078":2}}]}}"#, char::from(92)),
        r#"[{"outer":{"null":null,"null":true}}]"#.to_owned(),
    ] {
        let mut raw = server.raw().await;
        raw.hello().await;
        let frame = format!(r#"{{"version":1,"id":"d","method":"echo","params":{params}}}"#);
        raw.writer.write_all(frame.as_bytes()).await.unwrap();
        raw.writer.write_all(b"\n").await.unwrap();
        assert!(
            raw.receive().await.is_none(),
            "duplicate business field accepted"
        );
    }
    assert_eq!(server.handler.calls.load(Ordering::SeqCst), 0);
    server.stop().await;
}

#[tokio::test]
async fn duplicate_cancel_target_is_rejected_before_cancelling_by_last_value() {
    let server = Server::start(config()).await;
    for key in [
        "request_id".to_owned(),
        format!("request_{}u0069d", char::from(92)),
    ] {
        let mut raw = server.raw().await;
        raw.hello().await;
        raw.send(request("long", "hold", Value::Null)).await;
        signalled(&server.handler.started).await;
        let frame = format!(
            r#"{{"version":1,"id":"cancel","method":"rpc.cancel","params":{{"request_id":"missing","{key}":"long"}}}}"#
        );
        raw.writer.write_all(frame.as_bytes()).await.unwrap();
        raw.writer.write_all(b"\n").await.unwrap();
        assert!(
            raw.receive().await.is_none(),
            "ambiguous cancel was acknowledged"
        );
        signalled(&server.handler.cancelled).await;
    }
    assert_eq!(server.handler.calls.load(Ordering::SeqCst), 2);
    server.stop().await;
}

#[tokio::test]
async fn duplicate_limits_in_hello_response_are_rejected() {
    for field in [
        "max_frame_bytes".to_owned(),
        format!("max_frame_{}u0062ytes", char::from(92)),
        "version".to_owned(),
    ] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut raw = Raw::connect_peer(listener).await;
            let _hello = raw.receive().await.unwrap();
            let frame = format!(
                r#"{{"version":1,"id":"h","result":{{"{field}":1,"version":1,"max_frame_bytes":1048576,"max_in_flight":64,"max_requests_per_connection":65536,"max_id_bytes":128}}}}"#
            );
            raw.writer.write_all(frame.as_bytes()).await.unwrap();
            raw.writer.write_all(b"\n").await.unwrap();
            assert!(raw.receive().await.is_none());
        });
        let result = Client::connect(addr, TOKEN).await;
        if let Ok(client) = &result {
            client.close().await;
        }
        let error = result.unwrap_err();
        assert_eq!(error.code, "protocol_error");
        assert!(!error.outcome_unknown);
        timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn duplicate_keys_in_nested_response_fail_the_pending_call_as_unknown() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut raw = Raw::connect_peer(listener).await;
        fake_hello(&mut raw).await;
        let request = raw.receive().await.unwrap();
        let id = serde_json::to_string(&request["id"]).unwrap();
        let frame = format!(
            r#"{{"version":1,"id":{id},"result":{{"items":[{{"ok":false,"{}u006fk":true}}]}}}}"#,
            char::from(92)
        );
        raw.writer.write_all(frame.as_bytes()).await.unwrap();
        raw.writer.write_all(b"\n").await.unwrap();
        assert!(raw.receive().await.is_none());
    });
    let client = Client::connect(addr, TOKEN).await.unwrap();
    let result = client.request("echo", Value::Null).await;
    client.close().await;
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
    let error = result.unwrap_err();
    assert_eq!(error.code, "protocol_error");
    assert!(error.outcome_unknown);
}

fn nested_arrays(depth: usize) -> Value {
    (0..depth).fold(Value::Null, |value, _| Value::Array(vec![value]))
}

#[tokio::test]
async fn json_depth_is_bounded_locally_and_on_the_socket_without_coercing_values() {
    let server = Server::start(config()).await;
    let client = Client::connect(server.addr, TOKEN).await.unwrap();
    let allowed = nested_arrays(63); // The request/response object is container 64.
    assert_eq!(
        client.request("echo", allowed.clone()).await.unwrap(),
        allowed
    );
    let result = client.request("echo", nested_arrays(64)).await;
    assert!(result.is_err(), "too-deep local params were admitted");
    assert!(!result.unwrap_err().outcome_unknown);
    let allowed_objects = (0..63).fold(Value::Null, |value, _| json!({"child":value}));
    assert_eq!(
        client
            .request("echo", allowed_objects.clone())
            .await
            .unwrap(),
        allowed_objects
    );
    let deep_objects = (0..64).fold(Value::Null, |value, _| json!({"child":value}));
    let error = client
        .request("echo", deep_objects.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, "invalid_params");
    assert!(!error.outcome_unknown);
    let values = json!({"string":"12","number":12,"boolean":false,"null":null,
        "array":["false",true,-9,18446744073709551615u64],"float":1.25,
        "json_as_text":r#"{"x":1,"x":2}"#});
    assert_eq!(
        client.request("echo", values.clone()).await.unwrap(),
        values
    );
    let mut raw = server.raw().await;
    raw.hello().await;
    raw.send(request("deep", "echo", nested_arrays(64))).await;
    assert!(
        raw.receive().await.is_none(),
        "too-deep socket params were dispatched"
    );
    let mut raw = server.raw().await;
    raw.hello().await;
    raw.send(request("deep-object", "echo", deep_objects)).await;
    assert!(
        raw.receive().await.is_none(),
        "too-deep socket object was dispatched"
    );
    assert_eq!(server.handler.calls.load(Ordering::SeqCst), 3);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn fresh_request_and_cancel_ids_cannot_be_derived_from_an_observed_id() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut raw = Raw::connect_peer(listener).await;
        fake_hello(&mut raw).await;
        let first = raw.receive().await.unwrap();
        let first_id = first["id"].as_str().unwrap();
        let (counter, nonce) = first_id.split_once('-').expect("ID needs fresh randomness");
        assert_eq!(counter, "1");
        assert_eq!(nonce.len(), 32);
        assert!(nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
        raw.send(json!({"version":1,"id":first["id"],"result":null}))
            .await;
        let held = raw.receive().await.unwrap();
        let held_id = held["id"].as_str().unwrap();
        let (_, held_nonce) = held_id.split_once('-').unwrap();
        assert_ne!(held_nonce, nonce);
        started_tx.send(()).unwrap();
        let cancel = raw.receive().await.unwrap();
        assert_eq!(cancel["method"], "rpc.cancel");
        assert_eq!(cancel["params"]["request_id"], held["id"]);
        let (_, cancel_nonce) = cancel["id"].as_str().unwrap().split_once('-').unwrap();
        assert_ne!(cancel_nonce, nonce);
        assert_ne!(cancel_nonce, held_nonce);
        raw.send(json!({"version":1,"id":cancel["id"],"result":{"cancelled":true}}))
            .await;
        raw.send(json!({"version":1,"id":held["id"],"error":{
            "code":"cancelled_outcome_unknown","message":"cancelled"
        }}))
        .await;
        assert!(raw.receive().await.is_none());
    });
    let client = Client::connect(addr, TOKEN).await.unwrap();
    assert_eq!(
        client.request("echo", Value::Null).await.unwrap(),
        Value::Null
    );
    let cancel = CancellationToken::new();
    let held = {
        let client = client.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            client
                .request_with_cancel("hold", Value::Null, cancel)
                .await
        })
    };
    started_rx.await.unwrap();
    cancel.cancel();
    assert!(held.await.unwrap().unwrap_err().outcome_unknown);
    client.close().await;
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
}

#[tokio::test]
async fn legal_pipelined_frames_and_coalesced_responses_are_preserved() {
    let server = Server::start(config()).await;
    let mut raw = server.raw().await;
    let mut bytes = Vec::new();
    for value in [
        request("hello", "hello", json!({"token":TOKEN})),
        request("one", "echo", json!({"first":{"x":1},"second":{"x":2}})),
        request("two", "echo", json!([null, true, "12", 12])),
    ] {
        bytes.extend_from_slice(&serde_json::to_vec(&value).unwrap());
        bytes.push(b'\n');
    }
    raw.writer.write_all(&bytes).await.unwrap();
    assert_eq!(raw.receive().await.unwrap()["id"], "hello");
    let first = raw.receive().await.unwrap();
    let second = raw.receive().await.unwrap();
    assert_eq!(first["id"], "one");
    assert_eq!(first["result"], json!({"first":{"x":1},"second":{"x":2}}));
    assert_eq!(second["id"], "two");
    assert_eq!(second["result"], json!([null, true, "12", 12]));
    drop(raw);
    server.stop().await;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut raw = Raw::connect_peer(listener).await;
        fake_hello(&mut raw).await;
        let one = raw.receive().await.unwrap();
        let two = raw.receive().await.unwrap();
        let mut bytes = Vec::new();
        for request in [two, one] {
            bytes.extend_from_slice(
                &serde_json::to_vec(&json!({
                    "version":1,"id":request["id"],"result":request["params"]
                }))
                .unwrap(),
            );
            bytes.push(b'\n');
        }
        raw.writer.write_all(&bytes).await.unwrap();
        assert!(raw.receive().await.is_none());
    });
    let client = Client::connect(addr, TOKEN).await.unwrap();
    let (one, two) = tokio::join!(
        client.request("one", json!(1)),
        client.request("two", json!(2))
    );
    assert_eq!(one.unwrap(), 1);
    assert_eq!(two.unwrap(), 2);
    client.close().await;
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
}

#[tokio::test]
async fn hello_and_cancel_fields_do_not_coerce_non_string_values() {
    let server = Server::start(config()).await;
    for token in [
        json!(123),
        json!(true),
        Value::Null,
        json!([TOKEN]),
        json!({"value":TOKEN}),
    ] {
        let mut raw = server.raw().await;
        raw.send(request("h", "hello", json!({"token":token})))
            .await;
        assert_eq!(
            raw.receive().await.unwrap()["error"]["code"],
            "unauthorized"
        );
        assert!(raw.receive().await.is_none());
    }
    for target in [json!(123), json!(true), Value::Null, json!(["long"])] {
        let mut raw = server.raw().await;
        raw.hello().await;
        raw.send(request("long", "hold", Value::Null)).await;
        signalled(&server.handler.started).await;
        raw.send(request(
            "cancel",
            "rpc.cancel",
            json!({"request_id":target}),
        ))
        .await;
        assert!(raw.receive().await.is_none());
        signalled(&server.handler.cancelled).await;
    }
    assert_eq!(server.handler.calls.load(Ordering::SeqCst), 4);
    server.stop().await;
}

#[tokio::test]
async fn excessive_response_depth_is_unknown_and_releases_the_call() {
    let server = Server::start(config()).await;
    let client = Client::connect(server.addr, TOKEN).await.unwrap();
    let error = client
        .request("deep_result", Value::Null)
        .await
        .unwrap_err();
    assert_eq!(error.code, "response_too_large_outcome_unknown");
    assert!(error.outcome_unknown);
    assert_eq!(client.request("echo", json!(7)).await.unwrap(), 7);
    client.close().await;
    server.stop().await;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut raw = Raw::connect_peer(listener).await;
        fake_hello(&mut raw).await;
        let request = raw.receive().await.unwrap();
        raw.send(json!({"version":1,"id":request["id"],"result":nested_arrays(64)}))
            .await;
        assert!(raw.receive().await.is_none());
    });
    let client = Client::connect(addr, TOKEN).await.unwrap();
    let error = client.request("echo", Value::Null).await.unwrap_err();
    assert_eq!(error.code, "protocol_error");
    assert!(error.outcome_unknown);
    client.close().await;
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
}

#[tokio::test]
async fn id_limit_too_small_for_secure_ids_never_exposes_a_usable_client() {
    let server = Server::start(ServerConfig {
        max_id_bytes: 33,
        ..config()
    })
    .await;
    let error = Client::connect(server.addr, TOKEN).await.unwrap_err();
    assert_eq!(error.code, "protocol_error");
    assert!(!error.outcome_unknown);
    assert_eq!(server.handler.calls.load(Ordering::SeqCst), 0);
    server.stop().await;
}

#[tokio::test]
async fn separate_client_process_completes_authenticated_round_trip() {
    let server = Server::start(config()).await;
    let output = timeout(
        TEST_TIMEOUT,
        tokio::process::Command::new(env!("CARGO_BIN_EXE_cayw-ipc-fixture"))
            .arg(server.addr.to_string())
            .env("CAYW_IPC_TOKEN", TOKEN)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value, json!({"process":"separate","value":23}));
    assert_eq!(server.handler.calls.load(Ordering::SeqCst), 1);
    server.stop().await;
}
