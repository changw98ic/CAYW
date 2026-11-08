//! Product settings through the real CLI/IPC, with synthetic loopback HTTP only.
//! Covers stored credentials and the provider hot-applied to subsequent tasks.
#![cfg(target_os = "macos")]

use cayw_ipc::Client;
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    process::Command,
};

const WATCHDOG: Duration = Duration::from_secs(30);

async fn request(client: &Client, method: &str, params: Value) -> Value {
    tokio::time::timeout(WATCHDOG, client.request(method, params))
        .await
        .expect("settings request must finish")
        .expect("settings request must succeed")
}

fn saved(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

async fn prompt_headers(client: &Client, listener: &TcpListener, model: &str) -> String {
    let session = request(
        client,
        "session.create",
        json!({"label":"synthetic auth fixture"}),
    )
    .await;
    let response = async {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut buffer = [0; 4096];
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break index + 4;
            }
            assert!(bytes.len() < 65536);
        };
        let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        assert!(length < 1024 * 1024);
        while bytes.len() < header_end + length {
            let mut buffer = [0; 4096];
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
        }
        let body: Value = serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
        assert_eq!(body["model"], model);
        let body = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"id":"synthetic","choices":[{"index":0,"delta":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]})
        );
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        headers
    };
    let prompt = client.request(
        "session.prompt",
        json!({"session_id":session["id"],"text":"synthetic loopback fixture"}),
    );
    let (headers, result) =
        tokio::time::timeout(WATCHDOG, async { tokio::join!(response, prompt) })
            .await
            .expect("local HTTP fixture must finish");
    let result = result.unwrap();
    assert!(!result.to_string().contains("SYNTHETIC_KEY"));
    headers.to_ascii_lowercase()
}

#[tokio::test]
async fn edits_preserve_persisted_and_live_auth_for_selected_and_other_profiles() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut server = Command::new(env!("CARGO_BIN_EXE_cayw"))
        .env_clear()
        .env("HOME", home.path())
        .env("CAYW_HOME", state.path())
        .current_dir(workspace.path())
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(server.stdout.take().unwrap()).lines();
    let ready: Value = serde_json::from_str(
        &tokio::time::timeout(WATCHDOG, lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let token = std::fs::read_to_string(ready["token_file"].as_str().unwrap()).unwrap();
    let client = Client::connect(
        ready["listening"].as_str().unwrap().parse().unwrap(),
        token.trim(),
    )
    .await
    .unwrap();
    let models_file = std::fs::read_dir(state.path().join("workspaces"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("models.json");
    let model = |id: &str| {
        json!({
            "service":"openai", "api":"chat_completions", "base_url":base_url,
            "id":id, "api_version":"v1", "max_output_tokens":128
        })
    };
    for (name, key) in [("daily", "SYNTHETIC_KEY_A"), ("backup", "SYNTHETIC_KEY_B")] {
        let mut config = model(name);
        config["api_key"] = json!(key);
        let public = request(
            &client,
            "settings.models.put",
            json!({"name":name,"model":config}),
        )
        .await;
        assert!(!public.to_string().contains(key));
    }
    request(&client, "settings.models.use", json!({"name":"daily"})).await;
    for (name, key) in [("daily", "SYNTHETIC_KEY_A"), ("backup", "SYNTHETIC_KEY_B")] {
        // Legacy omission and the new explicit keep action both preserve keys.
        for credential in [None, Some("keep")] {
            let mut config = model(&format!("{name}-edited"));
            config["max_output_tokens"] = json!(256);
            let mut params = json!({"name":name,"model":config});
            if let Some(credential) = credential {
                params["credential"] = json!(credential);
            }
            let public = request(&client, "settings.models.put", params).await;
            assert_eq!(saved(&models_file)["models"][name]["api_key"], key);
            assert_eq!(public["models"][name]["authentication"], "plain_file");
            assert!(!public.to_string().contains(key));
            assert_eq!(public["health"]["model"], "daily-edited");
        }
    }
    let headers = prompt_headers(&client, &listener, "daily-edited").await;
    assert!(headers.contains("authorization: bearer synthetic_key_a\r\n"));
    request(&client, "settings.models.use", json!({"name":"backup"})).await;
    let headers = prompt_headers(&client, &listener, "backup-edited").await;
    assert!(headers.contains("authorization: bearer synthetic_key_b\r\n"));

    let before = std::fs::read(&models_file).unwrap();
    let mut changed = model("backup-edited");
    changed["base_url"] = json!(format!("{base_url}/different-deployment"));
    let error = tokio::time::timeout(
        WATCHDOG,
        client.request(
            "settings.models.put",
            json!({"name":"backup","model":changed,"credential":"keep"}),
        ),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(!error.outcome_unknown);
    assert_eq!(std::fs::read(&models_file).unwrap(), before);
    let headers = prompt_headers(&client, &listener, "backup-edited").await;
    assert!(headers.contains("authorization: bearer synthetic_key_b\r\n"));

    let mut replacement = model("backup-replaced");
    replacement["api_key"] = json!("SYNTHETIC_KEY_C");
    request(
        &client,
        "settings.models.put",
        json!({"name":"backup","model":replacement,"credential":"replace"}),
    )
    .await;
    assert_eq!(
        saved(&models_file)["models"]["backup"]["api_key"],
        "SYNTHETIC_KEY_C"
    );
    let headers = prompt_headers(&client, &listener, "backup-replaced").await;
    assert!(headers.contains("authorization: bearer synthetic_key_c\r\n"));
    request(
        &client,
        "settings.models.put",
        json!({"name":"backup","model":model("backup-noauth"),"credential":"remove"}),
    )
    .await;
    assert!(
        saved(&models_file)["models"]["backup"]
            .get("api_key")
            .is_none()
    );
    let headers = prompt_headers(&client, &listener, "backup-noauth").await;
    assert!(!headers.contains("authorization:"));
    client.close().await;
    server.kill().await.unwrap();
    server.wait().await.unwrap();
}
