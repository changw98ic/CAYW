use cayw_runtime::HeadlessConfig;
use serde_json::{Value, json};

fn provider(service: &str, api: &str, version: &str, base: &str) -> Value {
    json!({
        "service": service,
        "api": api,
        "id": "fixture-model",
        "base_url": base,
        "api_version": version,
        "max_output_tokens": 64,
        "api_key_env": "CAYW_CONFIG_TEST_KEY"
    })
}

fn load(value: Value) -> Result<HeadlessConfig, cayw_runtime::RuntimeError> {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("host.json");
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    HeadlessConfig::load(path)
}

// Break caught: the host configuration cannot select any of the real adapters.
#[tokio::test]
async fn each_service_and_api_can_be_selected_without_loading_credentials() {
    for (service, api, version, base) in [
        (
            "openai",
            "chat_completions",
            "v1",
            "https://api.openai.com/v1",
        ),
        (
            "anthropic",
            "messages",
            "2023-06-01",
            "https://api.anthropic.com/v1",
        ),
        (
            "gemini",
            "stream_generate_content",
            "v1beta",
            "https://generativelanguage.googleapis.com/v1beta",
        ),
        (
            "gemini",
            "generate_content",
            "v1beta",
            "https://generativelanguage.googleapis.com/v1beta",
        ),
        (
            "magpie",
            "chat_completions",
            "v1",
            "http://127.0.0.1:3425/v1",
        ),
    ] {
        let config = load(json!({"model": provider(service, api, version, base)}));
        assert!(
            config.is_ok(),
            "{service}/{api} must load without credentials"
        );
        let config = config.unwrap();
        let mut reads = Vec::new();
        let configured = config
            .model()
            .unwrap()
            .build_with_environment(|name| {
                reads.push(name.to_owned());
                Some("synthetic-selection-key".into())
            })
            .unwrap();
        assert_eq!(reads, ["CAYW_CONFIG_TEST_KEY"]);
        assert_eq!(configured.model.provider, service);
        assert_eq!(configured.model.id, "fixture-model");
        assert_eq!(configured.model.max_output_tokens, Some(64));
        assert!(config.into_runtime_config().is_ok());
    }
}

// Break caught: Magpie can select only Chat, or a routed Gemini ID is rejected as an official name.
#[tokio::test]
async fn magpie_selects_all_native_protocols_with_exact_routed_models() {
    for (api, version, base, id) in [
        (
            "chat_completions",
            "v1",
            "http://127.0.0.1:3425/v1",
            "opencode-go/deepseek-v4.1-flash",
        ),
        (
            "responses",
            "v1",
            "http://127.0.0.1:3425/v1",
            "codex/gpt-6-luna",
        ),
        (
            "messages",
            "2023-06-01",
            "http://127.0.0.1:3425/v1",
            "factory/claude-haiku-5-5",
        ),
        (
            "stream_generate_content",
            "v1beta",
            "http://127.0.0.1:3425/v1beta",
            "antigravity/gemini-3.8-flash",
        ),
        (
            "generate_content",
            "v1beta",
            "http://127.0.0.1:3425/v1beta",
            "antigravity/gemini-3.8-flash",
        ),
    ] {
        let mut model = provider("magpie", api, version, base);
        model["id"] = json!(id);
        let config = load(json!({"model":model})).unwrap_or_else(|error| panic!("{api}: {error}"));
        let mut reads = Vec::new();
        let configured = config
            .model()
            .unwrap()
            .build_with_environment(|name| {
                reads.push(name.to_owned());
                Some("synthetic-selection-key".into())
            })
            .unwrap();
        assert_eq!(configured.model.provider, "magpie");
        assert_eq!(configured.model.id, id);
        assert_eq!(reads, ["CAYW_CONFIG_TEST_KEY"]);
    }
}

#[test]
fn routed_gemini_names_are_scoped_to_magpie_and_reject_path_injection() {
    for service in ["gemini", "magpie"] {
        for id in [
            "../model",
            "route/../model",
            "route//model",
            "route/model/extra",
            "route/model%2fother",
            "route/model:generateContent",
            "/route/model",
            "route/model?x",
            "route/model#x",
            "route/.",
            "route/..",
        ] {
            let mut model = provider(
                service,
                "stream_generate_content",
                "v1beta",
                "http://127.0.0.1:9/v1beta",
            );
            model["id"] = json!(id);
            assert!(load(json!({"model":model})).is_err(), "{service}: {id}");
        }
    }
    let mut official = provider(
        "gemini",
        "stream_generate_content",
        "v1beta",
        "https://generativelanguage.googleapis.com/v1beta",
    );
    official["id"] = json!("antigravity/gemini-3.8-flash");
    assert!(load(json!({"model":official})).is_err());
}

// Break caught: configuration contradicts the selected adapter or its fixed API version.
#[test]
fn invalid_provider_configuration_fails_during_load_without_echoing_values() {
    let valid = provider(
        "anthropic",
        "messages",
        "2023-06-01",
        "https://api.anthropic.com/v1",
    );
    for (field, value) in [
        ("service", json!("unknown-secret-service")),
        ("api", json!("chat_completions")),
        ("api_version", json!("2099-secret-version")),
        (
            "base_url",
            json!("https://user:secret-password@api.anthropic.com/v1"),
        ),
        (
            "base_url",
            json!("https://api.anthropic.com/v1?key=secret-query"),
        ),
        (
            "base_url",
            json!("https://api.anthropic.com/v1#secret-fragment"),
        ),
        ("base_url", json!("not a url")),
        ("base_url", json!("file:///v1")),
        ("id", json!("secret model\n")),
        ("max_output_tokens", json!(0)),
        ("api_key_env", json!("secret-invalid=name")),
        ("api_key", json!("secret-inline-key")),
    ] {
        let mut config = valid.clone();
        config[field] = value;
        let parsed = serde_json::from_value::<HeadlessConfig>(json!({"model": config.clone()}));
        if let Ok(parsed) = parsed {
            let mut looked_up = false;
            let result = parsed.model().unwrap().build_with_environment(|_| {
                looked_up = true;
                Some("synthetic-selection-key".into())
            });
            assert!(result.is_err());
            assert!(
                !looked_up,
                "invalid settings must fail before credential lookup"
            );
        }
        let error = load(json!({"model": config}))
            .err()
            .expect("invalid provider must fail");
        assert!(!error.to_string().contains("secret"), "{error}");
    }
    for field in [
        "service",
        "api",
        "id",
        "base_url",
        "api_version",
        "max_output_tokens",
        "api_key_env",
    ] {
        let mut config = valid.clone();
        config.as_object_mut().unwrap().remove(field);
        assert!(load(json!({"model": config})).is_err(), "missing {field}");
    }
}

// Break caught: a host-selected deployment prefix, hostname, port or local
// hostname is rejected even though the adapters retain the supplied base path.
#[tokio::test]
async fn host_selected_deployments_accept_prefix_host_and_port() {
    for (service, api, version, base) in [
        (
            "openai",
            "chat_completions",
            "v1",
            "https://gateway.example/gateway/v1",
        ),
        (
            "openai",
            "responses",
            "v1",
            "https://gateway.example:8443/deploy/team/v1",
        ),
        (
            "anthropic",
            "messages",
            "2023-06-01",
            "http://localhost:3425/proxy/anthropic",
        ),
        (
            "magpie",
            "messages",
            "2023-06-01",
            "https://gateway.example/deploy/magpie",
        ),
        (
            "gemini",
            "stream_generate_content",
            "v1beta",
            "http://[::1]:8080/deploy/v1beta",
        ),
        (
            "gemini",
            "generate_content",
            "v1beta",
            "https://generativelanguage.googleapis.com",
        ),
    ] {
        let config = load(json!({"model": provider(service, api, version, base)}))
            .unwrap_or_else(|error| panic!("{service}/{api} {base}: {error}"));
        let mut reads = Vec::new();
        let configured = config
            .model()
            .unwrap()
            .build_with_environment(|name| {
                reads.push(name.to_owned());
                Some("synthetic-selection-key".into())
            })
            .unwrap();
        assert_eq!(reads, ["CAYW_CONFIG_TEST_KEY"]);
        assert_eq!(configured.model.provider, service);
        assert_eq!(configured.model.id, "fixture-model");
        assert_eq!(configured.model.max_output_tokens, Some(64));
        assert!(config.into_runtime_config().is_ok());
    }
}

#[test]
fn headless_config_loads_optional_web_access_and_vision() {
    let raw = json!({
        "web_access": {
            "search": "fixture",
            "limits": {
                "max_search_limit": 5,
                "max_body_bytes": 1048576,
                "max_extract_chars": 50000
            }
        },
        "vision": {
            "input_bytes": 4194304,
            "output_bytes": 4194304,
            "max_image_bytes": 8388608
        }
    });
    let config = load(raw).expect("load headless with web and vision");
    assert!(config.web_access().is_some());
    assert!(config.vision().is_some());

    let runtime_config = config
        .into_runtime_config()
        .expect("convert to runtime config");
    assert!(runtime_config.host.web_access.is_some());
    assert!(runtime_config.host.vision.is_some());
}
