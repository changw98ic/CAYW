#![cfg(target_os = "macos")]
//! Contending content-policy amendments must never return an incompatible store.
//!
//! A simulated participant requests another legal same-root/same-device quota.
//! Each amendment holds the modification lock until the opener's shared policy
//! lock is restored. Runtime also checks every policy after its final amendment.
//! Every successful open must therefore agree with the durable content binding;
//! contention may reject the open, but cannot leave a usable stale Runtime.
use cayw_ai::{AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_content::{ContentStore, Id, Limits};
use cayw_runtime::{ContentConfig, Runtime, RuntimeConfig};
use futures::StreamExt;
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// The test never runs a model turn; only `Runtime::open` is exercised.
struct Unused;
impl Provider for Unused {
    fn stream(&self, _: ModelRequest, _: CancellationToken) -> AssistantStream {
        futures::stream::empty().boxed()
    }
}

fn limits(total: u64, records: u32) -> Limits {
    Limits {
        max_content_bytes: 1024,
        max_total_bytes: total,
        max_records: records,
        max_security_entries: 64,
    }
}
fn content(root: &Path, limits: Limits) -> ContentConfig {
    ContentConfig {
        directory: root.join("content"),
        device_id: "race-device".into(),
        limits,
    }
}

/// A simulated amendment participant without a lifetime shared lock. It takes
/// the same amendment and exclusive policy locks as the Runtime path. When it
/// wins, it applies a legal quota through the real store amendment API and
/// records the corresponding durable binding.
fn racer(
    lock: PathBuf,
    database: PathBuf,
    directory: PathBuf,
    config: ContentConfig,
    root: (u64, u64),
    stop: Arc<AtomicBool>,
    commits: Arc<AtomicU64>,
) {
    let device = Id::new(&config.device_id).unwrap();
    let identity = serde_json::to_string(&serde_json::json!({
        "config": serde_json::to_value(&config).unwrap(),
        "root_device": root.0,
        "root_inode": root.1,
    }))
    .unwrap();
    while !stop.load(Ordering::Relaxed) {
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock)
        else {
            continue;
        };
        let amendment = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(format!("{}.runtime-policy-amend-lock", database.display()))
            .unwrap();
        if amendment.try_lock().is_err() || file.try_lock().is_err() {
            continue;
        }
        let amended = ContentStore::open_private_amending_limits(
            &directory,
            device.clone(),
            config.limits,
            root,
        );
        if amended.is_ok() {
            let db = rusqlite::Connection::open(&database).unwrap();
            db.execute(
                "UPDATE runtime_content_config SET identity_json=?1 WHERE singleton=1",
                [&identity],
            )
            .unwrap();
            commits.fetch_add(1, Ordering::Relaxed);
        }
        let _ = file.unlock();
    }
}

#[test]
fn a_lost_content_amendment_race_never_returns_an_incompatible_store() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    std::fs::create_dir(root.join("content")).unwrap();
    std::fs::set_permissions(root.join("content"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let database = root.join("sessions.sqlite");
    let provider = Arc::new(Unused);
    let base = content(&root, limits(4096, 8));
    let rival = content(&root, limits(8192, 16));

    let open = |config: ContentConfig| {
        Runtime::open(
            &database,
            root.join("workspace"),
            provider.clone(),
            Model::new("fixture", "content-race"),
            RuntimeConfig {
                content: Some(config),
                ..RuntimeConfig::default()
            },
        )
    };

    // Establish the one canonical store and binding, plus a Session that the
    // compatibility probe can inspect without starting a model turn.
    let first = open(base.clone()).expect("the first trusted content enablement must open");
    let session = first.create(None).unwrap().id;
    assert!(
        first
            .content_inspect(&session, None, "note")
            .unwrap()
            .is_none()
    );
    drop(first);

    let metadata = std::fs::metadata(root.join("content")).unwrap();
    let root_identity = (metadata.dev(), metadata.ino());
    let stop = Arc::new(AtomicBool::new(false));
    let commits = Arc::new(AtomicU64::new(0));
    let lock = PathBuf::from(format!("{}.runtime-policy-lock", database.display()));
    let racers = (0..2)
        .map(|_| {
            let (lock, database, directory) =
                (lock.clone(), database.clone(), root.join("content"));
            let (rival, stop, commits) = (rival.clone(), stop.clone(), commits.clone());
            std::thread::spawn(move || {
                racer(
                    lock,
                    database,
                    directory,
                    rival,
                    root_identity,
                    stop,
                    commits,
                )
            })
        })
        .collect::<Vec<_>>();

    // Contention is supplemental coverage. Correctness does not depend on
    // the scheduler hitting a particular lock-upgrade window or error string.
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut opened, mut rejected) = (0u32, 0u32);
    for _ in 0..64 {
        if Instant::now() >= deadline {
            break;
        }
        match open(base.clone()) {
            Ok(runtime) => {
                // Every successful open must use the durable policy, even if
                // another amendment won during an earlier lock transition.
                runtime
                    .content_inspect(&session, None, "note")
                    .expect("an open Runtime must be able to use its content store");
                opened += 1;
                drop(runtime);
            }
            Err(_) => rejected += 1,
        }
    }
    stop.store(true, Ordering::Relaxed);
    for handle in racers {
        handle.join().unwrap();
    }
    eprintln!(
        "content amendment contention: commits={}, opened={opened}, rejected={rejected}; at most 64 attempts, checking a 10-second deadline before each attempt",
        commits.load(Ordering::Relaxed),
    );

    // Once competitors stop, both legal quotas must reopen and inspect the
    // original Session. These checks always run, even if contention rejected
    // every attempted open or did not land in the narrow amendment window.
    for config in [rival, base] {
        let runtime = open(config).expect("a settled legal content policy must reopen");
        assert!(
            runtime
                .content_inspect(&session, None, "note")
                .unwrap()
                .is_none()
        );
    }
}
