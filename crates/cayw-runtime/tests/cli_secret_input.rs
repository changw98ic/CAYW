//! Real macOS terminal coverage for both `conf` entry points. All credentials
//! are synthetic, and configuring a model never sends a provider request.
#![cfg(target_os = "macos")]

use serde_json::Value;
use std::{
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

const WATCHDOG: Duration = Duration::from_secs(30);
const SECRET: &str = "SYNTHETIC_CAYW_TERMINAL_KEY";

struct Terminal {
    child: Child,
    master: File,
    slave: File,
    transcript: Vec<u8>,
}

impl Terminal {
    fn start(home: &Path, workspace: &Path, state: &Path, config: Option<&Path>) -> Self {
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: openpty initializes these descriptors; all optional pointers
        // are null. Each successful descriptor is immediately owned by a File.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0,
            "openpty: {}",
            io::Error::last_os_error()
        );
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        // Do not let the child inherit the original endpoints in addition to
        // its standard streams.
        for file in [&master, &slave] {
            assert_ne!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
                -1
            );
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_cayw"));
        command
            .env_clear()
            .env("HOME", home)
            .env("CAYW_HOME", state)
            .current_dir(workspace)
            .arg("conf")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        if let Some(config) = config {
            command.arg("--config").arg(config);
        }
        // SAFETY: only async-signal-safe operations run before exec. Stdin is
        // already the PTY slave, and the child becomes its controlling session.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        let terminal = Self {
            child,
            master,
            slave,
            transcript: Vec::new(),
        };
        assert!(terminal.echo_enabled(), "fixture must start with echo on");
        terminal
    }

    fn echo_enabled(&self) -> bool {
        let mut mode = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr initializes mode on success.
        assert_eq!(
            unsafe { libc::tcgetattr(self.slave.as_raw_fd(), mode.as_mut_ptr()) },
            0
        );
        unsafe { mode.assume_init().c_lflag & libc::ECHO != 0 }
    }

    fn read_until(&mut self, marker: &str) {
        let deadline = Instant::now() + WATCHDOG;
        while !String::from_utf8_lossy(&self.transcript).contains(marker) {
            assert!(
                Instant::now() < deadline,
                "missing terminal prompt: {marker}"
            );
            let mut descriptor = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // Poll is a watchdog, not a latency assertion.
            let ready = unsafe { libc::poll(&mut descriptor, 1, 100) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                panic!("terminal poll failed: {error}");
            }
            if ready > 0 {
                let mut bytes = [0; 4096];
                let count = self.master.read(&mut bytes).unwrap();
                assert_ne!(count, 0, "terminal closed before prompt: {marker}");
                self.transcript.extend_from_slice(&bytes[..count]);
            } else {
                assert!(
                    self.child.try_wait().unwrap().is_none(),
                    "conf exited before prompt: {marker}"
                );
            }
        }
    }

    fn send(&mut self, value: &str) {
        self.master.write_all(value.as_bytes()).unwrap();
        self.master.flush().unwrap();
    }

    fn enter_model(&mut self) {
        self.read_until("模型服务 [1-4");
        self.send("openai\n");
        self.read_until("接口格式 [chat_completions]");
        self.send("\n");
        self.read_until("服务地址 [");
        self.send("http://127.0.0.1:9/v1\n");
        self.read_until("模型名称（");
        self.send("terminal-fixture\n");
        self.read_until("模型密钥（");
        // The prompt can arrive just before rpassword changes terminal mode.
        let deadline = Instant::now() + WATCHDOG;
        while self.echo_enabled() {
            assert!(Instant::now() < deadline, "secret input must disable echo");
            assert!(self.child.try_wait().unwrap().is_none());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn finish(&mut self) -> ExitStatus {
        let deadline = Instant::now() + WATCHDOG;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "conf must finish");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn terminal_conf_hides_key_and_restores_echo_for_local_and_explicit_config() {
    for explicit in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let config = state.path().join("headless.json");
        let mut terminal = Terminal::start(
            home.path(),
            workspace.path(),
            state.path(),
            explicit.then_some(config.as_path()),
        );
        terminal.enter_model();
        terminal.send(&format!("{SECRET}\n"));
        terminal.read_until("单次输出 token");
        assert!(
            terminal.echo_enabled(),
            "echo must be restored after key input"
        );
        terminal.send("64\n");
        terminal.read_until("保存配置吗");
        terminal.send("y\n");
        terminal.read_until("\"configured\":true");
        assert!(terminal.finish().success());
        assert!(!String::from_utf8_lossy(&terminal.transcript).contains(SECRET));
        let models = if explicit {
            config
        } else {
            let mut entries = std::fs::read_dir(state.path().join("workspaces")).unwrap();
            let models = entries.next().unwrap().unwrap().path().join("models.json");
            assert!(entries.next().is_none());
            models
        };
        let saved: Value = serde_json::from_slice(&std::fs::read(models).unwrap()).unwrap();
        let model = if explicit {
            &saved["model"]
        } else {
            &saved["models"]["configured"]
        };
        assert_eq!(model["api_key"], SECRET);
        assert_eq!(model["max_output_tokens"], 64);
    }
}

#[test]
fn terminal_conf_accepts_empty_key_without_authentication() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let config = state.path().join("headless.json");
    let mut terminal = Terminal::start(home.path(), workspace.path(), state.path(), Some(&config));
    terminal.enter_model();
    terminal.send("\n");
    terminal.read_until("单次输出 token");
    assert!(terminal.echo_enabled());
    terminal.send("\n");
    terminal.read_until("保存配置吗");
    terminal.send("y\n");
    terminal.read_until("\"configured\":true");
    assert!(terminal.finish().success());
    let saved: Value = serde_json::from_slice(&std::fs::read(config).unwrap()).unwrap();
    assert!(saved["model"].get("api_key").is_none());
}

#[tokio::test]
async fn redirected_conf_reads_synthetic_key_from_its_pipe() {
    use tokio::io::AsyncWriteExt;

    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let config = state.path().join("headless.json");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_cayw"))
        .env_clear()
        .env("HOME", home.path())
        .env("CAYW_HOME", state.path())
        .current_dir(workspace.path())
        .args(["conf", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            format!("openai\n\nhttp://127.0.0.1:9/v1\npipe-fixture\n{SECRET}\n64\ny\n").as_bytes(),
        )
        .await
        .unwrap();
    let output = tokio::time::timeout(WATCHDOG, child.wait_with_output())
        .await
        .expect("redirected input must not block on a terminal")
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(SECRET));
    let saved: Value = serde_json::from_slice(&std::fs::read(config).unwrap()).unwrap();
    assert_eq!(saved["model"]["api_key"], SECRET);
}
