use cayw_tui::{
    App,
    config::{Config, USAGE},
    draw,
    driver::Driver,
    terminal::TerminalGuard,
    transport::{Transport, Watch},
};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream},
    execute,
};
use futures::{FutureExt, StreamExt};
use ratatui::layout::Rect;
use std::{
    any::Any,
    io::{self, IsTerminal},
    panic::AssertUnwindSafe,
    time::Duration,
};
use tokio::time::{MissedTickBehavior, interval};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args()
        .skip(1)
        .any(|arg| arg == "--help" || arg == "-h")
    {
        println!("{USAGE}");
        return Ok(());
    }
    let config = Config::from_environment()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "cayw-tui requires an interactive terminal. Use --help for connection settings.",
        )
        .into());
    }
    let mut terminal = TerminalGuard::enter()?;
    let mut transport = Transport::start(config.address, config.token().to_owned());
    let mut app = App::default();
    app.status = "Connecting to the host…".into();
    let mut driver = Driver::default();
    let result = AssertUnwindSafe(run(&mut terminal, &mut transport, &mut app, &mut driver))
        .catch_unwind()
        .await;
    // Restore the terminal immediately, before waiting for network cancellation.
    drop(terminal);
    transport.shutdown().await;
    match result {
        Ok(result) => result.map_err(Into::into),
        Err(payload) => std::panic::resume_unwind(payload as Box<dyn Any + Send>),
    }
}

async fn run(
    terminal: &mut TerminalGuard,
    transport: &mut Transport,
    app: &mut App,
    driver: &mut Driver,
) -> io::Result<()> {
    let mut events = EventStream::new();
    let mut redraw = interval(Duration::from_millis(50));
    redraw.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let stop = shutdown_signal();
    tokio::pin!(stop);
    let mut last_watch: Option<Option<Watch>> = None;
    let mut last_mouse_disabled = app.mouse_disabled;
    terminal.terminal().draw(|frame| draw(frame, app))?;
    loop {
        let mut consumed_events = false;
        let mut needs_draw = true;
        tokio::select! {
            _ = &mut stop => { app.quitting = true; }
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) => {
                    let commands = app.handle_key(key);
                    driver.dispatch(app, transport, commands);
                    if app.mouse_disabled != last_mouse_disabled {
                        last_mouse_disabled = app.mouse_disabled;
                        if app.mouse_disabled {
                            let _ = execute!(io::stdout(), DisableMouseCapture);
                        } else {
                            let _ = execute!(io::stdout(), EnableMouseCapture);
                        }
                    }
                }
                Some(Ok(Event::Paste(text))) => app.paste(&text),
                Some(Ok(Event::Mouse(mouse))) => {
                    let size = terminal.terminal().size()?;
                    let area = Rect::new(0, 0, size.width, size.height);
                    let commands = app.handle_mouse(mouse, area);
                    driver.dispatch(app, transport, commands);
                }
                Some(Ok(_)) => {},
                Some(Err(error)) => return Err(error),
                None => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "terminal input closed")),
            },
            update = transport.next() => match update {
                Some(update) => {
                    consumed_events = matches!(update, cayw_tui::transport::Update::Events { .. } | cayw_tui::transport::Update::Connected);
                    let commands = driver.update(app, update);
                    driver.dispatch(app, transport, commands);
                }
                None => return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "IPC worker stopped")),
            },
            _ = redraw.tick() => {
                if app.is_animating() {
                    app.advance_tick();
                } else {
                    needs_draw = false;
                }
            }
        }
        if app.quitting {
            break;
        }
        let watch = driver.watch(app);
        if last_watch.as_ref() != Some(&watch) || consumed_events {
            match transport.watch(watch.clone()) {
                Ok(()) => last_watch = Some(watch),
                Err(error) => app.status = error.into(),
            }
        }
        if needs_draw {
            terminal.terminal().draw(|frame| draw(frame, app))?;
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) {
            (Ok(mut terminate), Ok(mut hangup)) => {
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {}, _ = hangup.recv() => {} }
            }
            _ => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
