#![cfg(unix)]

use cayw_device::{DirectoryProcessSpec, ProcessCleanup, ProcessRunner};
use std::{
    collections::BTreeMap,
    fs, io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

struct ExitBlock {
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}
impl Drop for ExitBlock {
    fn drop(&mut self) {
        let _ = self.entered.send(());
        let _ = self.release.recv_timeout(Duration::from_secs(10));
    }
}
thread_local! { static EXIT:std::cell::RefCell<Option<ExitBlock>>=const {std::cell::RefCell::new(None)}; }
struct Resource(Option<ExitBlock>);
impl ProcessCleanup for Resource {
    fn cleanup(&mut self) -> io::Result<()> {
        EXIT.with(|value| *value.borrow_mut() = self.0.take());
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn drain_poll_must_yield_while_supervisor_thread_local_destructor_is_blocked() {
    let root = std::env::temp_dir().join(format!("independent-thread-exit-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let program = PathBuf::from("/usr/bin/true");
    let runner = ProcessRunner::new([program.clone()]).unwrap();
    let (entered, started) = mpsc::sync_channel(1);
    let (release, resume) = mpsc::sync_channel(1);
    let resource = runner.own_cleanup(Resource(Some(ExitBlock {
        entered,
        release: resume,
    })));
    let mut run = Box::pin(runner.run_in_directory_with_resources(
        DirectoryProcessSpec {
            program,
            args: vec![],
            cwd: fs::File::open(&root).unwrap(),
            env: BTreeMap::new(),
            timeout: Duration::from_secs(10),
            max_output_bytes: 1024,
        },
        CancellationToken::new(),
        resource,
    ));
    std::future::poll_fn(|cx| {
        assert!(run.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    tokio::task::spawn_blocking(move || started.recv_timeout(Duration::from_secs(10)).unwrap())
        .await
        .unwrap();
    let yielded = Arc::new(AtomicBool::new(false));
    let shared = yielded.clone();
    let (polled, return_observed) = mpsc::sync_channel(1);
    // The watchdog is outside Tokio. It releases the destructor even if drain's
    // first poll blocks the only runtime thread and its timers cannot fire.
    let watchdog = std::thread::spawn(move || {
        let blocked = return_observed
            .recv_timeout(Duration::from_millis(300))
            .is_err();
        let before_release = shared.load(Ordering::SeqCst);
        release.send(()).unwrap();
        (blocked, before_release)
    });
    let mut drain = Box::pin(runner.drain());
    let pending =
        std::future::poll_fn(|cx| std::task::Poll::Ready(drain.as_mut().poll(cx).is_pending()))
            .await;
    yielded.store(true, Ordering::SeqCst);
    let _ = polled.send(());
    let (blocked, poll_returned_before_release) = watchdog.join().unwrap();
    if pending {
        drain.await.unwrap();
    }
    run.await.unwrap();
    println!(
        "THREAD_EXIT_DRAIN first_poll_pending={pending} first_poll_blocked_watchdog={blocked} poll_returned_before_destructor_release={poll_returned_before_release}"
    );
    assert!(
        pending && poll_returned_before_release && !blocked,
        "drain synchronously joined a still-running thread-local destructor"
    );
}
