//! Verification for bounded streaming HTTP body reads (F10).
//!
//! Tests that responses exceeding max_bytes are terminated immediately
//! without waiting for completion of the full response stream,
//! and that memory is bounded during the receive process.
//!
//! Uses deterministic channel handshakes instead of fixed sleep delays.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use cayw_web_access::{FailureClass, HttpTransport, ReqwestTransport};

/// Guard that coordinates releasing a server thread waiting on a channel
/// and guarantees joining the thread on both normal completion and panic unwinding.
struct ServerGuard {
    release_tx: Option<mpsc::Sender<()>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl ServerGuard {
    fn new(release_tx: mpsc::Sender<()>, handle: thread::JoinHandle<()>) -> Self {
        Self {
            release_tx: Some(release_tx),
            handle: Some(handle),
        }
    }

    fn release_and_join(mut self) {
        if let Some(tx) = self.release_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            handle.join().expect("server thread panicked");
        }
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        drop(self.release_tx.take());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Generic thread join guard for threads that do not need release signaling.
struct ThreadGuard<T> {
    handle: Option<thread::JoinHandle<T>>,
}

impl<T> ThreadGuard<T> {
    fn new(handle: thread::JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    fn join(mut self) -> T {
        self.handle
            .take()
            .expect("handle present")
            .join()
            .expect("thread panicked")
    }
}

impl<T> Drop for ThreadGuard<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn read_http_request(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
    let mut buf = [0u8; 4096];
    let mut n = 0;
    while n < buf.len() {
        let read_bytes = stream.read(&mut buf[n..])?;
        if read_bytes == 0 {
            break;
        }
        n += read_bytes;
        if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(());
        }
    }
    Ok(())
}

/// Test 1: Chunked transfer encoding without Content-Length, exceeding max_bytes.
/// The server sends chunk 1 (1000 B) and chunk 2 (1000 B), then waits on a release channel
/// without sending EOF. Max bytes is set to 1500 B.
///
/// The client must abort immediately upon receiving chunk 2 (2000 B > 1500 B)
/// without waiting for EOF. Only after confirming the client returned `BodyTooLarge`
/// does the test release the server thread.
#[tokio::test]
async fn chunked_exceeding_max_terminates_early_without_waiting_for_eof() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let port = listener.local_addr().unwrap().port();
    let (release_tx, release_rx) = mpsc::channel();

    let server_handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept connection");
        read_http_request(&mut stream).expect("read request");

        // Send HTTP/1.1 chunked response headers
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n")
            .expect("write headers");
        stream.flush().expect("flush headers");

        // Chunk 1: 1000 bytes
        let chunk1 = vec![b'A'; 1000];
        stream
            .write_all(format!("{:x}\r\n", chunk1.len()).as_bytes())
            .expect("chunk1 len");
        stream.write_all(&chunk1).expect("chunk1 data");
        stream.write_all(b"\r\n").expect("chunk1 crlf");

        // Chunk 2: 1000 bytes (cumulative: 2000 bytes > 1500 bytes limit)
        let chunk2 = vec![b'B'; 1000];
        stream
            .write_all(format!("{:x}\r\n", chunk2.len()).as_bytes())
            .expect("chunk2 len");
        stream.write_all(&chunk2).expect("chunk2 data");
        stream.write_all(b"\r\n").expect("chunk2 crlf");
        stream.flush().expect("flush chunks");

        // Do not send terminal chunk or close connection; wait for release from client test.
        let _ = release_rx.recv_timeout(Duration::from_secs(10));
    });

    let guard = ServerGuard::new(release_tx, server_handle);

    let transport = ReqwestTransport::new().expect("transport");
    let url = format!("http://127.0.0.1:{port}/chunked");

    let fetch_result =
        tokio::time::timeout(Duration::from_secs(10), transport.get(&url, 1500)).await;

    let err = fetch_result
        .expect("watchdog timeout: client should not wait for server EOF")
        .expect_err("must fail with BodyTooLarge");
    assert_eq!(
        err.diagnostic.class,
        FailureClass::BodyTooLarge,
        "unexpected error class: {:?}",
        err.diagnostic
    );
    assert_eq!(err.diagnostic.http_status, Some(200));

    // Release server thread and verify clean join
    guard.release_and_join();
}

/// Test 2: Missing Content-Length and non-chunked response (HTTP/1.0 close-delimited stream).
/// The server sends 1600 bytes, then waits without closing. Max bytes = 1000.
/// The client must terminate immediately when bytes exceed max_bytes without waiting for close.
#[tokio::test]
async fn missing_content_length_stream_terminates_on_exceeding_max() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let port = listener.local_addr().unwrap().port();
    let (release_tx, release_rx) = mpsc::channel();

    let server_handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept connection");
        read_http_request(&mut stream).expect("read request");

        stream
            .write_all(b"HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\n\r\n")
            .expect("headers");

        // Send 1600 bytes (> 1000 limit)
        let data = vec![b'X'; 1600];
        stream.write_all(&data).expect("write data");
        stream.flush().expect("flush data");

        // Keep socket open without closing until test releases
        let _ = release_rx.recv_timeout(Duration::from_secs(10));
    });

    let guard = ServerGuard::new(release_tx, server_handle);

    let transport = ReqwestTransport::new().expect("transport");
    let url = format!("http://127.0.0.1:{port}/close_delimited");

    let fetch_result =
        tokio::time::timeout(Duration::from_secs(10), transport.get(&url, 1000)).await;

    let err = fetch_result
        .expect("watchdog timeout: client waited for socket close instead of terminating early")
        .expect_err("must fail with BodyTooLarge");
    assert_eq!(err.diagnostic.class, FailureClass::BodyTooLarge);

    guard.release_and_join();
}

/// Test 3: Forged / inaccurate Content-Length header with chunked body.
/// Server claims `Content-Length: 50`, but sends Transfer-Encoding: chunked with 1600 bytes.
/// Max bytes is 1000. Client must not trust Content-Length and must terminate once
/// actual received bytes exceed 1000.
#[tokio::test]
async fn forged_content_length_does_not_prevent_enforcement() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let port = listener.local_addr().unwrap().port();
    let (release_tx, release_rx) = mpsc::channel();

    let server_handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept connection");
        read_http_request(&mut stream).expect("read request");

        stream
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 50\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n")
            .expect("headers");

        let chunk1 = vec![b'Z'; 800];
        stream
            .write_all(format!("{:x}\r\n", chunk1.len()).as_bytes())
            .unwrap();
        stream.write_all(&chunk1).unwrap();
        stream.write_all(b"\r\n").unwrap();

        let chunk2 = vec![b'Z'; 800];
        stream
            .write_all(format!("{:x}\r\n", chunk2.len()).as_bytes())
            .unwrap();
        stream.write_all(&chunk2).unwrap();
        stream.write_all(b"\r\n").unwrap();
        stream.flush().unwrap();

        let _ = release_rx.recv_timeout(Duration::from_secs(10));
    });

    let guard = ServerGuard::new(release_tx, server_handle);

    let transport = ReqwestTransport::new().expect("transport");
    let url = format!("http://127.0.0.1:{port}/forged_len");

    let res = tokio::time::timeout(Duration::from_secs(10), transport.get(&url, 1000))
        .await
        .expect("watchdog timeout")
        .expect_err("must fail");

    assert_eq!(res.diagnostic.class, FailureClass::BodyTooLarge);

    guard.release_and_join();
}

/// Test 4: Content-Length header exceeding max_bytes fails fast without reading body.
#[tokio::test]
async fn declared_content_length_exceeding_max_fails_fast() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let port = listener.local_addr().unwrap().port();

    let server_handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept connection");
        read_http_request(&mut stream).expect("read request");

        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 50000000\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n")
            .expect("headers");
        stream.flush().expect("flush");
        // No body sent at all
    });

    let guard = ThreadGuard::new(server_handle);

    let transport = ReqwestTransport::new().expect("transport");
    let url = format!("http://127.0.0.1:{port}/declared_too_large");

    let res = transport.get(&url, 1000).await.expect_err("must reject");
    assert_eq!(res.diagnostic.class, FailureClass::BodyTooLarge);

    guard.join();
}

/// Test 5: Exact boundary: exactly max_bytes succeeds, max_bytes + 1 fails.
#[tokio::test]
async fn exact_boundary_check() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let port = listener.local_addr().unwrap().port();

    let server_handle = thread::spawn(move || {
        for expected_len in [500usize, 501usize] {
            let (mut stream, _) = listener.accept().expect("accept connection");
            read_http_request(&mut stream).expect("read request");

            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {expected_len}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(header.as_bytes()).expect("write header");
            stream
                .write_all(&vec![b'M'; expected_len])
                .expect("write body");
            stream.flush().expect("flush");
        }
    });

    let guard = ThreadGuard::new(server_handle);

    let transport = ReqwestTransport::new().expect("transport");

    // Exactly 500 bytes with limit 500 -> Ok
    let ok_url = format!("http://127.0.0.1:{port}/exact_fit");
    let res = transport
        .get(&ok_url, 500)
        .await
        .expect("exact fit should succeed");
    assert_eq!(res.body.len(), 500);

    // 501 bytes with limit 500 -> Err(BodyTooLarge)
    let too_large_url = format!("http://127.0.0.1:{port}/one_byte_over");
    let err = transport
        .get(&too_large_url, 500)
        .await
        .expect_err("501 must fail");
    assert_eq!(err.diagnostic.class, FailureClass::BodyTooLarge);

    guard.join();
}

/// Test 6: Cancelling future aborts promptly upon handshake and stops underlying request.
///
/// Handshake:
/// 1. Server accepts connection and reads HTTP request headers.
/// 2. Server notifies test via `tx_arrived` that request was received.
/// 3. Test receives arrival notification and immediately drops the transport future.
/// 4. Server detects that client dropped connection (reading EOF / ConnectionReset)
///    and reports confirmation via `tx_stopped`.
/// 5. Test asserts that server confirmed request was stopped, without sleep polling.
#[tokio::test]
async fn cancelling_future_aborts_promptly() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let port = listener.local_addr().unwrap().port();
    let (tx_arrived, rx_arrived) = tokio::sync::oneshot::channel();
    let (tx_dropped, rx_dropped) = tokio::sync::oneshot::channel();
    let (tx_stopped, rx_stopped) = tokio::sync::oneshot::channel();

    let server_handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept connection");
        read_http_request(&mut stream).expect("read request");

        // 1. Signal arrival handshake to test
        let _ = tx_arrived.send(());

        // Wait for test to confirm future was dropped
        let _ = rx_dropped.blocking_recv();

        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set read timeout");
        let mut drain = [0u8; 128];
        let stopped = match stream.read(&mut drain) {
            Ok(0) => true,
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => true,
            _ => false,
        };
        let _ = tx_stopped.send(stopped);
    });

    let guard = ThreadGuard::new(server_handle);

    let transport = ReqwestTransport::new().expect("transport");
    let url = format!("http://127.0.0.1:{port}/cancel");

    let mut fut = transport.get(&url, 1000);

    tokio::select! {
        biased;
        res = rx_arrived => {
            assert!(res.is_ok(), "server confirmed request arrived");
        }
        res = &mut fut => {
            panic!("request should not complete before cancellation: {res:?}");
        }
    }
    // Drop future
    drop(fut);
    let _ = tx_dropped.send(());

    let stopped = tokio::time::timeout(Duration::from_secs(10), rx_stopped)
        .await
        .expect("watchdog timeout")
        .expect("server must report connection closure");
    assert!(
        stopped,
        "dropping the future must close the original request"
    );

    guard.join();
}
