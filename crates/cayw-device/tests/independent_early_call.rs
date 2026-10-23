#![cfg(unix)]
use cayw_device::{DeviceError, DirectoryProcessSpec, ProcessCleanup, ProcessRunner};
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

struct BlockedCleanup {
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
    dropped: Arc<AtomicBool>,
}
impl ProcessCleanup for BlockedCleanup {
    fn cleanup(&mut self) -> io::Result<()> {
        self.entered.send(()).unwrap();
        self.release.recv_timeout(Duration::from_secs(10)).unwrap();
        Err(io::Error::other("prelaunch refusal"))
    }
}
impl Drop for BlockedCleanup {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn draining_a_real_precancelled_call_waits_for_its_registered_cleanup() {
    let root = std::env::temp_dir().join(format!("independent-early-call-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let program = PathBuf::from("/usr/bin/true");
    let runner = ProcessRunner::new([program.clone()]).unwrap();
    let (entered, started) = mpsc::sync_channel(1);
    let (release, resume) = mpsc::sync_channel(1);
    let dropped = Arc::new(AtomicBool::new(false));
    let resource = runner.own_cleanup(BlockedCleanup {
        entered,
        release: resume,
        dropped: dropped.clone(),
    });
    let spec = DirectoryProcessSpec {
        program,
        args: vec![],
        cwd: fs::File::open(&root).unwrap(),
        env: BTreeMap::new(),
        timeout: Duration::from_secs(10),
        max_output_bytes: 1024,
        io: cayw_device::ProcessIo::Pipes,
    };
    let cancel = CancellationToken::new();
    cancel.cancel();
    let cloned = runner.clone();
    let call = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(cloned.run_in_directory_with_resources(spec, cancel, resource))
    });
    tokio::task::spawn_blocking(move || started.recv_timeout(Duration::from_secs(10)).unwrap())
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_millis(100), runner.drain()).await;
    let premature_success = matches!(first, Ok(Ok(())));
    release.send(()).unwrap();
    let result = call.join().unwrap();
    let second = runner.drain().await;
    println!(
        "EARLY_CALL drain_while_cleanup={first:?} call={result:?} final_drain={second:?} retained={}",
        !dropped.load(Ordering::SeqCst)
    );
    assert!(matches!(second, Err(DeviceError::CleanupUnconfirmed(_))));
    drop(runner);
    assert!(!dropped.load(Ordering::SeqCst));
    assert!(
        !premature_success,
        "drain confirmed a real run whose prelaunch resource cleanup was still in progress"
    );
}
