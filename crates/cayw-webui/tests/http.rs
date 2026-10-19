//! These checks protect startup validation. Network behavior is exercised via
//! real HTTP and authenticated IPC sockets in gateway.rs.
use cayw_webui::{GatewayLimits, IpcEndpoint, serve, serve_with_limits};
use std::io;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

fn endpoint() -> IpcEndpoint {
    IpcEndpoint::new(
        "127.0.0.1:12345".parse().unwrap(),
        "fedcba9876543210fedcba9876543210",
    )
    .unwrap()
}

#[tokio::test]
async fn refuses_public_listener_and_invalid_or_shared_web_credentials() {
    let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let error = serve(
        listener,
        endpoint(),
        "0123456789abcdef0123456789abcdef",
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    for token in [
        "",
        "ok",
        "id",
        "with space",
        "秘密",
        "fedcba9876543210fedcba9876543210",
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let error = serve(listener, endpoint(), token, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            !error
                .to_string()
                .contains("fedcba9876543210fedcba9876543210")
        );
    }
}

#[tokio::test]
async fn startup_rejects_budgets_that_could_starve_control_requests() {
    for limits in [
        GatewayLimits {
            max_in_flight: 1,
            ..Default::default()
        },
        GatewayLimits {
            max_prompt_in_flight: 32,
            ..Default::default()
        },
        GatewayLimits {
            max_pending_requests: 0,
            ..Default::default()
        },
        GatewayLimits {
            queue_timeout: std::time::Duration::ZERO,
            ..Default::default()
        },
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let error = serve_with_limits(
            listener,
            endpoint(),
            "0123456789abcdef0123456789abcdef",
            limits,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}

#[test]
fn ipc_configuration_requires_an_explicit_loopback_and_debug_redacts_credentials() {
    assert!(
        IpcEndpoint::new(
            "192.0.2.1:42".parse().unwrap(),
            "fedcba9876543210fedcba9876543210"
        )
        .is_err()
    );
    assert!(IpcEndpoint::new("127.0.0.1:42".parse().unwrap(), "").is_err());
    assert!(!format!("{:?}", endpoint()).contains("fedcba9876543210fedcba9876543210"));
}
