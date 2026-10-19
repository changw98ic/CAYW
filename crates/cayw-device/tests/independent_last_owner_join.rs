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
thread_local! {static EXIT:std::cell::RefCell<Option<ExitBlock>>=const{std::cell::RefCell::new(None)};}
struct Resource(Option<ExitBlock>);
impl ProcessCleanup for Resource {
    fn cleanup(&mut self) -> io::Result<()> {
        EXIT.with(|value| *value.borrow_mut() = self.0.take());
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_owner_drop_must_not_deadlock_supervisor_notification_against_drain_join() {
    let root = std::env::temp_dir().join(format!("independent-last-owner-{}", std::process::id()));
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
    drop(run);
    let finished = Arc::new(AtomicBool::new(false));
    let done = finished.clone();
    let drainer = runner.clone();
    let (begun, draining) = mpsc::sync_channel(1);
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        begun.send(()).unwrap();
        let result = runtime.block_on(drainer.drain());
        done.store(true, Ordering::SeqCst);
        result
    });
    draining.recv_timeout(Duration::from_secs(10)).unwrap();
    // Allow the separate drainer to enter its join while the TLS guard pins exit.
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(runner);
    release.send(()).unwrap();
    let settled = tokio::time::timeout(Duration::from_secs(2), async {
        while !finished.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    println!(
        "LAST_OWNER_JOIN drain_completed={} thread_finished={}",
        settled.is_ok(),
        thread.is_finished()
    );
    assert!(
        settled.is_ok(),
        "last-owner notification attempted to close Scope under the joiner's Scope mutex"
    );
    thread.join().unwrap().unwrap();
}
