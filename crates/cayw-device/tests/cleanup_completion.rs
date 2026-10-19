#![cfg(unix)]

use cayw_device::{DeviceError, DirectoryProcessSpec, ProcessCleanup, ProcessRunner};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cayw-cleanup-completion-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn spec(root: &Path, program: PathBuf) -> DirectoryProcessSpec {
    DirectoryProcessSpec {
        program,
        args: vec![],
        cwd: fs::File::open(root).unwrap(),
        env: BTreeMap::new(),
        timeout: Duration::from_secs(10),
        max_output_bytes: 1024,
    }
}

async fn pending<F: std::future::Future>(mut future: std::pin::Pin<&mut F>) -> bool {
    std::future::poll_fn(|cx| std::task::Poll::Ready(future.as_mut().poll(cx).is_pending())).await
}

struct Refused {
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
    dropped: Arc<AtomicBool>,
}
impl ProcessCleanup for Refused {
    fn cleanup(&mut self) -> std::io::Result<()> {
        self.entered.send(()).unwrap();
        self.release.recv_timeout(Duration::from_secs(10)).unwrap();
        Err(std::io::Error::other("cleanup completion refusal"))
    }
}
impl Drop for Refused {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn drain_waits_for_registered_cleanup_even_before_resource_drop_begins() {
    let runner = ProcessRunner::new([]).unwrap();
    let (entered, started) = mpsc::sync_channel(1);
    let (release, resume) = mpsc::sync_channel(1);
    let dropped = Arc::new(AtomicBool::new(false));
    let resource = runner.own_cleanup(Refused {
        entered,
        release: resume,
        dropped: dropped.clone(),
    });
    let mut drain = Box::pin(runner.drain());
    let registered_pending = pending(drain.as_mut()).await;
    let cleanup = std::thread::spawn(move || drop(resource));
    tokio::task::spawn_blocking(move || started.recv_timeout(Duration::from_secs(10)).unwrap())
        .await
        .unwrap();
    let cleaning_pending = if registered_pending {
        pending(drain.as_mut()).await
    } else {
        false
    };
    drop(drain);
    release.send(()).unwrap();
    cleanup.join().unwrap();
    assert!(matches!(
        runner.drain().await,
        Err(DeviceError::CleanupUnconfirmed(_))
    ));
    drop(runner);
    assert!(!dropped.load(Ordering::SeqCst));
    assert!(
        registered_pending,
        "registered resource was omitted from drain"
    );
    assert!(cleaning_pending, "running cleanup was omitted from drain");
}

// Moving a return ahead of registered cleanup completion must fail this test,
// including a rejection after close and real exec failure (no shell fallback).
#[tokio::test(flavor = "current_thread")]
async fn every_early_return_waits_for_cleanup_and_drain_retains_its_failure() {
    use std::os::unix::fs::PermissionsExt;
    for case in ["cancelled", "validation", "closed", "spawn"] {
        let root = Directory::new();
        let program = if case == "spawn" {
            let program = root.0.join("not-executable");
            fs::write(&program, b"not executable").unwrap();
            fs::set_permissions(&program, fs::Permissions::from_mode(0o600)).unwrap();
            program
        } else {
            PathBuf::from("/usr/bin/true")
        };
        let runner = ProcessRunner::new([program.clone()]).unwrap();
        let (entered, started) = mpsc::sync_channel(1);
        let (release, resume) = mpsc::sync_channel(1);
        let dropped = Arc::new(AtomicBool::new(false));
        let resource = runner.own_cleanup(Refused {
            entered,
            release: resume,
            dropped: dropped.clone(),
        });
        let mut command = spec(&root.0, program);
        let cancel = CancellationToken::new();
        match case {
            "cancelled" => cancel.cancel(),
            "validation" => command.timeout = Duration::ZERO,
            "closed" => runner.close(),
            _ => {}
        }
        let completed = Arc::new(AtomicBool::new(false));
        let done = completed.clone();
        let cloned = runner.clone();
        let call = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result =
                runtime.block_on(cloned.run_in_directory_with_resources(command, cancel, resource));
            done.store(true, Ordering::SeqCst);
            result
        });
        tokio::task::spawn_blocking(move || started.recv_timeout(Duration::from_secs(10)).unwrap())
            .await
            .unwrap();
        let first = tokio::time::timeout(Duration::from_millis(100), runner.drain()).await;
        let premature_drain = first.is_ok();
        let premature_return = completed.load(Ordering::SeqCst);
        release.send(()).unwrap();
        let result = call.join().unwrap();
        let final_drain = runner.drain().await;
        assert!(result.is_err(), "{case}: {result:?}");
        assert!(
            matches!(final_drain, Err(DeviceError::CleanupUnconfirmed(_))),
            "{case}: {final_drain:?}"
        );
        assert!(
            runner
                .cleanup_failure()
                .unwrap()
                .contains("cleanup completion refusal")
        );
        drop(runner);
        assert!(
            !dropped.load(Ordering::SeqCst),
            "{case}: failed resource released"
        );
        assert!(!premature_drain, "{case}: drain skipped running cleanup");
        assert!(
            !premature_return,
            "{case}: call returned before cleanup completed"
        );
    }
}

struct BlockedDrop {
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
    finished: Arc<AtomicBool>,
}
impl ProcessCleanup for BlockedDrop {
    fn cleanup(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Drop for BlockedDrop {
    fn drop(&mut self) {
        self.entered.send(()).unwrap();
        self.release.recv_timeout(Duration::from_secs(10)).unwrap();
        self.finished.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn successful_cleanup_and_plain_resource_drop_stay_pending_until_destructor_returns() {
    for fallible in [false, true] {
        let root = Directory::new();
        let program = PathBuf::from("/usr/bin/true");
        let runner = ProcessRunner::new([program.clone()]).unwrap();
        let (entered, started) = mpsc::sync_channel(1);
        let (release, resume) = mpsc::sync_channel(1);
        let finished = Arc::new(AtomicBool::new(false));
        let resource = BlockedDrop {
            entered,
            release: resume,
            finished: finished.clone(),
        };
        let mut command = spec(&root.0, program);
        command.timeout = Duration::ZERO;
        let cloned = runner.clone();
        let call = if fallible {
            let resource = runner.own_cleanup(resource);
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(cloned.run_in_directory_with_resources(
                    command,
                    CancellationToken::new(),
                    resource,
                ))
            })
        } else {
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(cloned.run_in_directory_with_resources(
                    command,
                    CancellationToken::new(),
                    resource,
                ))
            })
        };
        tokio::task::spawn_blocking(move || started.recv_timeout(Duration::from_secs(10)).unwrap())
            .await
            .unwrap();
        let first = tokio::time::timeout(Duration::from_millis(100), runner.drain()).await;
        let premature = matches!(first, Ok(Ok(())));
        assert!(!finished.load(Ordering::SeqCst));
        release.send(()).unwrap();
        assert!(matches!(
            call.join().unwrap(),
            Err(DeviceError::InvalidSpec(_))
        ));
        runner.drain().await.unwrap();
        assert!(finished.load(Ordering::SeqCst));
        assert!(
            !premature,
            "fallible={fallible}: drain skipped resource destructor"
        );
    }
}
