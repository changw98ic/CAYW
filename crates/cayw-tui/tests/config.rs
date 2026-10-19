use cayw_tui::config::{Config, parse};
use std::collections::BTreeMap;

fn config(args: &[&str], env: &[(&str, &str)]) -> Result<Config, String> {
    let values: BTreeMap<_, _> = env.iter().copied().collect();
    parse(args.iter().map(|v| v.to_string()), |key| {
        values.get(key).map(|v| v.to_string())
    })
}

#[test]
fn explicit_endpoint_and_token_are_required_without_local_credential_discovery() {
    assert!(config(&[], &[]).is_err());
    let parsed = config(
        &[],
        &[
            ("CAYW_IPC_ADDR", "127.0.0.1:4100"),
            ("CAYW_IPC_TOKEN", "host secret"),
        ],
    )
    .unwrap();
    assert_eq!(parsed.address.to_string(), "127.0.0.1:4100");
    assert_eq!(parsed.token(), "host secret");
    assert!(!format!("{parsed:?}").contains("host secret"));
}

#[test]
fn command_line_endpoint_overrides_environment_and_rejects_ambiguous_input() {
    let parsed = config(
        &["--addr", "[::1]:4567"],
        &[
            ("CAYW_IPC_ADDR", "127.0.0.1:4100"),
            ("CAYW_IPC_TOKEN", "fixture"),
        ],
    )
    .unwrap();
    assert_eq!(parsed.address.to_string(), "[::1]:4567");
    for args in [
        vec!["--addr"],
        vec!["--addr", "localhost:99"],
        vec!["--unknown"],
        vec!["--addr", "127.0.0.1:0"],
    ] {
        assert!(config(&args, &[("CAYW_IPC_TOKEN", "fixture")]).is_err());
    }
    assert!(config(&["--addr", "127.0.0.1:123"], &[("CAYW_IPC_TOKEN", "   ")]).is_err());
}
