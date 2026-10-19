use cayw_tui::{config::parse, terminal::ScreenGuard};
use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

#[test]
fn cli_requires_explicit_endpoint_and_token_and_never_prints_credentials() {
    let config = parse(["--addr", "127.0.0.1:4567"].map(str::to_owned), |key| {
        (key == "CAYW_IPC_TOKEN").then(|| "test-secret".into())
    })
    .unwrap();
    assert_eq!(config.address.to_string(), "127.0.0.1:4567");
    assert_eq!(config.token(), "test-secret");
    assert!(!format!("{config:?}").contains("test-secret"));
    assert!(
        parse(["--addr", "0.0.0.0:4567"].map(str::to_owned), |_| Some(
            "token".into()
        ))
        .is_err()
    );
    assert!(
        parse(
            ["--addr", "127.0.0.1:4567", "--token", "secret"].map(str::to_owned),
            |_| Some("token".into())
        )
        .is_err()
    );
    assert!(parse(["--addr", "127.0.0.1:4567"].map(str::to_owned), |_| None).is_err());
    assert!(
        parse(["--addr", "127.0.0.1:4567"].map(str::to_owned), |_| Some(
            "bad\ntoken".into()
        ))
        .is_err()
    );
}

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn restore_guard_runs_once_on_normal_exit_and_unwind() {
    let output = Arc::new(Mutex::new(Vec::new()));
    {
        let _guard = ScreenGuard::enter(Capture(output.clone())).unwrap();
    }
    let captured = output.clone();
    let result = std::panic::catch_unwind(move || {
        let _guard = ScreenGuard::enter(Capture(captured)).unwrap();
        panic!("fixture terminal failure");
    });
    assert!(result.is_err());
    let bytes = String::from_utf8(output.lock().unwrap().clone()).unwrap();
    assert_eq!(bytes.matches("\x1b[?1049l").count(), 2);
}
