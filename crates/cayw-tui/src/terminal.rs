//! RAII ownership of terminal modes. Every normal error and unwind restores them.
use crossterm::{
    cursor::{Hide, Show},
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    io::{self, Stdout, Write},
    sync::Once,
};

pub type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

/// Alternate screen ownership is independent of raw mode so it can be verified
/// using an output writer without changing the test runner's terminal.
pub struct ScreenGuard<W: Write> {
    writer: W,
}

impl<W: Write> ScreenGuard<W> {
    pub fn enter(writer: W) -> io::Result<Self> {
        let mut guard = Self { writer };
        execute!(guard.writer, EnterAlternateScreen, Hide)?;
        match execute!(guard.writer, EnableBracketedPaste) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {}
            Err(error) => return Err(error),
        }
        match execute!(guard.writer, EnableMouseCapture) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {}
            Err(error) => return Err(error),
        }
        Ok(guard)
    }
}

impl<W: Write> Drop for ScreenGuard<W> {
    fn drop(&mut self) {
        // Try every cleanup even when an earlier terminal command failed.
        let _ = execute!(self.writer, DisableMouseCapture);
        let _ = execute!(self.writer, DisableBracketedPaste);
        let _ = execute!(self.writer, Show);
        let _ = execute!(self.writer, LeaveAlternateScreen);
        let _ = self.writer.flush();
    }
}

struct RawGuard;
impl RawGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}
impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

pub struct TerminalGuard {
    terminal: TuiTerminal,
    // Fields drop in order: restore screen before disabling raw mode.
    _screen: ScreenGuard<Stdout>,
    _raw: RawGuard,
}

impl TerminalGuard {
    pub fn enter() -> io::Result<Self> {
        static PANIC_HOOK: Once = Once::new();
        PANIC_HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                let _ = disable_raw_mode();
                let _ = execute!(io::stdout(), DisableMouseCapture);
                let _ = execute!(io::stdout(), DisableBracketedPaste);
                let _ = execute!(io::stdout(), Show);
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                previous(info);
            }));
        });
        let raw = RawGuard::enter()?;
        let screen = ScreenGuard::enter(io::stdout())?;
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        terminal.clear()?;
        Ok(Self {
            terminal,
            _screen: screen,
            _raw: raw,
        })
    }

    pub fn terminal(&mut self) -> &mut TuiTerminal {
        &mut self.terminal
    }
}
