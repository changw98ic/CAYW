use cayw_ai::{
    AssistantEvent, CancellationToken, Content, Message, Model, ModelRequest, Provider, StopReason,
    StreamOptions,
};
use cayw_providers::{GeminiMode, GeminiProvider};
use futures::{StreamExt, future::poll_fn};
use serde_json::json;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{Duration, timeout};

struct Counted;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn record_allocation(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counted {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            record_allocation(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, old: Layout, new: usize) -> *mut u8 {
        let result = unsafe { System.realloc(ptr, old, new) };
        if !result.is_null() {
            if new >= old.size() {
                record_allocation(new - old.size());
            } else {
                LIVE.fetch_sub(old.size() - new, Ordering::Relaxed);
            }
        }
        result
    }
}

#[global_allocator]
static ALLOCATOR: Counted = Counted;

// This executable has one test so allocation counts cannot include another test.
// A retained Vec of full-message snapshots fails the memory assertion; processing
// all parts before the first yield also fails the cancellation-content assertion.
#[tokio::test]
async fn gemini_large_frames_keep_bounded_memory_and_cancel_before_remaining_parts() {
    for mode in [
        GeminiMode::StreamGenerateContent,
        GeminiMode::GenerateContent,
    ] {
        for parts in [2000usize, 500, 4000] {
            let text = "x".repeat(32);
            let value = json!({"candidates":[{"content":{"role":"model","parts":vec![json!({"text":text}); parts]},"finishReason":"STOP"}]});
            let payload = if mode == GeminiMode::GenerateContent {
                value.to_string()
            } else {
                format!("data: {value}\n\n")
            };
            let input_bytes = payload.len();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let provider = GeminiProvider::new(
                format!("http://{}", listener.local_addr().unwrap()),
                "fixture-key",
            )
            .unwrap()
            .with_mode(mode);
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut bytes = [0u8; 4096];
                    let size = socket.read(&mut bytes).await.unwrap();
                    assert_ne!(size, 0);
                    request.extend_from_slice(&bytes[..size]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let mime = if mode == GeminiMode::GenerateContent {
                    "application/json"
                } else {
                    "text/event-stream"
                };
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                socket.write_all(header.as_bytes()).await.unwrap();
                // A single large write exposes frame buffering, independently of
                // arbitrary network chunk sizes.
                let _ = socket.write_all(payload.as_bytes()).await;
            });
            let request = ModelRequest {
                model: Model::new("gemini", "fixture"),
                messages: vec![Message::user("hello")],
                options: StreamOptions::default(),
            };
            let token = CancellationToken::new();
            let mut stream = provider.stream(request, token.clone());
            let baseline = LIVE.load(Ordering::Relaxed);
            PEAK.store(baseline, Ordering::Relaxed);
            let first = timeout(Duration::from_secs(10), async {
                loop {
                    let event = stream.next().await.expect("missing first text delta");
                    match event {
                        AssistantEvent::TextDelta { delta, partial, .. } => {
                            assert_eq!(delta, "x".repeat(32));
                            assert_eq!(partial.content, vec![Content::text("x".repeat(32))]);
                            return;
                        }
                        event => assert!(!event.is_terminal(), "{event:?}"),
                    }
                }
            });
            first.await.unwrap();
            let retained = LIVE.load(Ordering::Relaxed).saturating_sub(baseline);
            let peak = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
            eprintln!(
                "{mode:?}: {parts} parts, {input_bytes} input bytes, {retained} retained bytes, {peak} peak allocation bytes"
            );
            assert!(
                retained < 8 * 1024 * 1024 && peak < 8 * 1024 * 1024,
                "single frame retained {retained} bytes; peak {peak} bytes"
            );
            token.cancel();
            let terminal = timeout(Duration::from_secs(5), stream.next())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(terminal, AssistantEvent::Error { .. }));
            assert_eq!(terminal.message().stop_reason, StopReason::Aborted);
            assert_eq!(
                terminal.message().content,
                vec![Content::text("x".repeat(32))]
            );
            assert!(stream.next().await.is_none());
            server.await.unwrap();
        }
    }

    // The producer has delivered valid JSON plus an invalid trailing suffix. A
    // cancellation injected at a deterministic decoder yield must win before
    // the decoder reaches that suffix. This exercises the actual TCP response
    // path, not only the parser's unit tests.
    for mode in [
        GeminiMode::StreamGenerateContent,
        GeminiMode::GenerateContent,
    ] {
        let value = json!({"candidates":[{"content":{"parts":vec![json!({"text":"x"}); 3000]},"finishReason":"STOP"}]});
        let json = value.to_string() + " BROKEN";
        let payload = if mode == GeminiMode::GenerateContent {
            json
        } else {
            format!("data: {json}\n\n")
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider = GeminiProvider::new(
            format!("http://{}", listener.local_addr().unwrap()),
            "fixture-key",
        )
        .unwrap()
        .with_mode(mode);
        let (sent_tx, sent_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut bytes = [0; 4096];
                let size = socket.read(&mut bytes).await.unwrap();
                assert_ne!(size, 0, "connection closed before request headers");
                request.extend_from_slice(&bytes[..size]);
                if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    break;
                }
            }
            let mime = if mode == GeminiMode::GenerateContent {
                "application/json"
            } else {
                "text/event-stream"
            };
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", payload.len()).as_bytes()).await.unwrap();
            socket.write_all(payload.as_bytes()).await.unwrap();
            sent_tx.send(()).unwrap();
        });
        let token = CancellationToken::new();
        let req = ModelRequest {
            model: Model::new("gemini", "fixture"),
            messages: vec![Message::user("hello")],
            options: StreamOptions::default(),
        };
        let mut stream = provider.stream(req, token.clone());
        assert!(matches!(
            stream.next().await,
            Some(AssistantEvent::Start { .. })
        ));
        let mut next = Box::pin(stream.next());
        timeout(Duration::from_secs(5), async {
            tokio::select! {
                biased;
                sent = sent_rx => sent.unwrap(),
                event = &mut next => panic!("response completed before cancellation setup: {event:?}"),
            }
        }).await.unwrap();
        let mut checkpoints = 0;
        let terminal = timeout(
            Duration::from_secs(10),
            poll_fn(|context| {
                let result = next.as_mut().poll(context);
                if result.is_pending() {
                    checkpoints += 1;
                    if checkpoints == 80 {
                        token.cancel();
                    }
                }
                result
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            checkpoints, 80,
            "decoder did not yield during frame parsing"
        );
        assert_eq!(terminal.message().stop_reason, StopReason::Aborted);
        assert!(terminal.message().content.is_empty());
        drop(next);
        assert!(stream.next().await.is_none());
        eprintln!(
            "{mode:?}: cancellation interrupted decoder after {checkpoints} cooperative checkpoints"
        );
        server.await.unwrap();
    }
}
