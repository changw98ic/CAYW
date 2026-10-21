//! These checks protect startup validation. Network behavior is exercised via
//! real HTTP and authenticated IPC sockets in gateway.rs.
use cayw_webui::{
    GatewayLimits, IpcEndpoint, ProjectEndpoint, serve, serve_projects_with_limits,
    serve_with_limits,
};
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
async fn refuses_public_listener() {
    let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let error = serve(listener, endpoint(), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
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
        let error = serve_with_limits(listener, endpoint(), limits, CancellationToken::new())
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
    assert!(ProjectEndpoint::new("", "默认", endpoint()).is_err());
    assert!(ProjectEndpoint::new("bad id", "默认", endpoint()).is_err());
    assert!(ProjectEndpoint::new("ok", "  ", endpoint()).is_err());
    let project = ProjectEndpoint::new("alpha", "项目甲", endpoint()).unwrap();
    assert_eq!(project.id(), "alpha");
    assert_eq!(project.label(), "项目甲");
    assert!(!format!("{project:?}").contains("fedcba9876543210fedcba9876543210"));
}

#[tokio::test]
async fn multi_project_startup_rejects_empty_and_duplicate_ids() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    assert!(
        serve_projects_with_limits(
            listener,
            vec![],
            GatewayLimits::default(),
            CancellationToken::new(),
        )
        .await
        .is_err()
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    assert!(
        serve_projects_with_limits(
            listener,
            vec![
                ProjectEndpoint::new("dup", "一", endpoint()).unwrap(),
                ProjectEndpoint::new("dup", "二", endpoint()).unwrap(),
            ],
            GatewayLimits::default(),
            CancellationToken::new(),
        )
        .await
        .is_err()
    );
}
