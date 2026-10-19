use cayw_tui::terminal::ScreenGuard;
use std::{
    io::{self, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn screen_guard_restores_alternate_screen_paste_and_cursor_when_scope_exits() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    {
        let _guard = ScreenGuard::enter(Capture(bytes.clone())).unwrap();
        let current = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(current.contains("\x1b[?1049h"));
        assert!(current.contains("\x1b[?2004h"));
        assert!(!current.contains("\x1b[?1049l"));
    }
    let restored = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(restored.contains("\x1b[?2004l"));
    assert!(restored.contains("\x1b[?1049l"));
    assert!(restored.contains("\x1b[?25h"));
}

#[test]
fn screen_guard_restores_during_unwinding() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let output = bytes.clone();
    let result = std::panic::catch_unwind(move || {
        let _guard = ScreenGuard::enter(Capture(output)).unwrap();
        panic!("fixture failure after screen setup");
    });
    assert!(result.is_err());
    assert!(
        String::from_utf8(bytes.lock().unwrap().clone())
            .unwrap()
            .contains("\x1b[?1049l")
    );
}

struct TransientWriteFailure {
    failed: Arc<AtomicBool>,
    output: Capture,
}
impl Write for TransientWriteFailure {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.windows(8).any(|part| part == b"\x1b[?2004l")
            && !self.failed.swap(true, Ordering::SeqCst)
        {
            return Err(io::Error::other("fixture one-command failure"));
        }
        self.output.write(data)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn restore_attempts_cursor_and_screen_even_when_disabling_paste_fails() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    {
        let _guard = ScreenGuard::enter(TransientWriteFailure {
            failed: Arc::new(AtomicBool::new(false)),
            output: Capture(bytes.clone()),
        })
        .unwrap();
    }
    let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(text.contains("\x1b[?25h"));
    assert!(text.contains("\x1b[?1049l"));
}
