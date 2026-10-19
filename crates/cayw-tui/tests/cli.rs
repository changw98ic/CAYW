use std::process::Command;

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cayw-tui"));
    command
        .env_remove("CAYW_IPC_ADDR")
        .env_remove("CAYW_IPC_TOKEN");
    command
}

#[test]
fn help_runs_without_a_terminal_or_runtime_credentials() {
    let output = command().arg("--help").output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("CAYW_IPC_ADDR"));
    assert!(text.contains("CAYW_IPC_TOKEN"));
    assert!(text.contains("Ctrl+Q"));
}

#[test]
fn invalid_configuration_fails_before_entering_raw_mode_and_redacts_token() {
    let output = command()
        .env("CAYW_IPC_ADDR", "bad")
        .env("CAYW_IPC_TOKEN", "private-fixture-token")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(!error.contains("private-fixture-token"));
    assert!(error.contains("IP:PORT"));
    assert!(!output.stdout.contains(&0x1b));
}

#[test]
fn valid_configuration_without_a_terminal_exits_cleanly_without_starting_ui() {
    let output = command()
        .env("CAYW_IPC_ADDR", "127.0.0.1:1")
        .env("CAYW_IPC_TOKEN", "private-fixture-token")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("interactive terminal")
    );
    assert!(!output.stdout.contains(&0x1b));
}
