//! Evidence dump for the opt-in Magpie generation acceptance.
//!
//! `Runtime::open` may initialize or recover schema, so this example never opens
//! the original database. It first copies the Core database and its SQLite
//! sidecars into an isolated snapshot directory, then opens only that snapshot
//! with an offline provider. The original database is not opened by SQLite.
//!
//! The request-authority database is deliberately not copied. `Runtime::open`
//! creates a fresh request authority inside the snapshot; the ledger/events/
//! audit/grants evidence comes from the copied Core database.
//!
//! It never sends a model request and never reads a model credential. It exists
//! because the IPC surface intentionally does not expose the model-request ledger.
//!
//! It stays in `cayw-runtime/examples` because the public `Runtime` API and its
//! development dependencies are defined in this crate. Moving it to
//! `cayw-visual-editor/examples` would require a new Cargo dependency in that
//! crate. This is a development example, not a product runtime path.
use cayw_ai::Model;
use cayw_runtime::{HeadlessConfig, Runtime, fixture::OfflineProvider};
use serde_json::{Value, json};
use std::{
    env,
    error::Error,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;

fn value(args: &[String], name: &str) -> Result<String> {
    let index = args
        .iter()
        .position(|item| item == name)
        .ok_or_else(|| format!("{name} is required"))?;
    args.get(index + 1)
        .cloned()
        .ok_or_else(|| format!("{name} requires a value").into())
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn copy_sidecar(label: &str, source: &Path, destination: &Path, copied: &mut Vec<Value>) -> Result {
    match fs::metadata(source) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(format!("{label} source is not a regular file").into());
            }
            fs::copy(source, destination)?;
            let before = fs::read(source)?;
            let after = fs::read(destination)?;
            if before != after {
                return Err(format!("{label} snapshot copy verification failed").into());
            }
            copied.push(json!({
                "label": label,
                "source": source,
                "snapshot": destination,
                "bytes": after.len(),
                "verified_equal": true,
            }));
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            copied.push(json!({"label": label, "source": source, "present": false}));
        }
        Err(error) => return Err(format!("{label}: {error}").into()),
    }
    Ok(())
}

fn main() -> Result {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|item| item == "--help") {
        println!(
            "framework_magpie --db PATH --workspace DIR --config PATH --session ID --snapshot NEW_DIR --out NEW_FILE"
        );
        return Ok(());
    }
    let db = PathBuf::from(value(&args, "--db")?);
    let workspace = PathBuf::from(value(&args, "--workspace")?);
    let config_path = PathBuf::from(value(&args, "--config")?);
    let session = value(&args, "--session")?;
    let snapshot = PathBuf::from(value(&args, "--snapshot")?);
    let out = PathBuf::from(value(&args, "--out")?);

    // Loading and converting the host configuration validates the persisted
    // budgets. It does not construct the selected provider or read a key.
    let config = HeadlessConfig::load(&config_path)?.into_runtime_config()?;
    let limit = config.max_total_tokens;

    // The runtime is stopped before this helper runs. Copy the Core database and
    // its SQLite sidecars, then open only the copy. `Runtime::open` may initialize
    // schema or recover records inside the snapshot; that is why the original is
    // never handed to SQLite.
    let source_db = fs::canonicalize(&db)?;
    let source_before = fs::read(&source_db)?;
    let source_workspace = fs::canonicalize(&workspace).unwrap_or(workspace);
    if snapshot.exists() {
        return Err(format!("snapshot directory already exists: {}", snapshot.display()).into());
    }
    fs::create_dir_all(&snapshot)?;
    let snapshot = fs::canonicalize(&snapshot)?;
    let snapshot_db = snapshot.join(
        source_db
            .file_name()
            .ok_or("database path has no file name")?,
    );
    let mut copied = Vec::new();
    copy_sidecar("core-database", &source_db, &snapshot_db, &mut copied)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        copy_sidecar(
            suffix.trim_start_matches('-'),
            &sidecar(&source_db, suffix),
            &sidecar(&snapshot_db, suffix),
            &mut copied,
        )?;
    }
    let snapshot_workspace = snapshot.join("workspace");
    fs::create_dir_all(&snapshot_workspace)?;
    let snapshot_db = fs::canonicalize(&snapshot_db)?;
    let snapshot_workspace = fs::canonicalize(&snapshot_workspace)?;
    let mut snapshot_evidence = json!({
        "mode": "isolated_copy",
        "source_db": source_db.display().to_string(),
        "source_workspace": source_workspace,
        "snapshot_dir": snapshot,
        "snapshot_db": snapshot_db,
        "snapshot_workspace": snapshot_workspace,
        "copied": copied,
        "requests_database_copied": false,
        "requests_database_note": "Runtime::open creates a fresh request authority inside the snapshot; the original request sidecar is not opened.",
        "original_sqlite_opened": false,
        "snapshot_sqlite_may_initialize_or_recover": true,
        "note": "Runtime::open may initialize or recover schema only inside snapshot_dir.",
    });

    let runtime = Runtime::open(
        &snapshot_db,
        &snapshot_workspace,
        Arc::new(OfflineProvider::default()),
        Model::new("fixture", "ledger-only"),
        config,
    )?;

    let view = runtime.get(&session)?;
    let snapshot_cursor = view.cursor;
    let mut events = Vec::new();
    let mut after = 0_u64;
    while after < snapshot_cursor && events.len() < 200_000 {
        let page = runtime.events(&session, after, 1000)?;
        if page.is_empty() {
            break;
        }
        after = page.last().map_or(after, |event| event.cursor);
        events.extend(page);
    }

    let mut audit = Vec::new();
    let mut audit_after = 0_u64;
    while audit.len() < 200_000 {
        let page = runtime.audit(&session, audit_after, 1000)?;
        if page.is_empty() {
            break;
        }
        audit_after = page.last().map_or(audit_after, |record| record.cursor);
        audit.extend(page);
    }

    let grants = runtime.grants(&session)?;
    let ledger = runtime.model_requests(&session)?;
    drop(runtime);
    let source_after = fs::read(&source_db)?;
    if source_before != source_after {
        return Err("original database changed while the isolated snapshot was opened".into());
    }
    snapshot_evidence["source_db_unchanged"] = json!(true);
    let output = json!({
        "session_id": session,
        "configured_total_tokens": limit,
        "session": view,
        "model_requests": ledger,
        "events": events,
        "audit": audit,
        "grants": grants,
        "snapshot": snapshot_evidence,
        "source": "public cayw-runtime SDK; isolated Core database snapshot; offline provider; no model request",
    });

    let mut file: File = OpenOptions::new().write(true).create_new(true).open(&out)?;
    serde_json::to_writer_pretty(&mut file, &output)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    let summary = json!({
        "status": "ok",
        "out": out,
        "session_id": output["session_id"],
        "requests": output["model_requests"]["requests"].as_array().map_or(0, Vec::len),
        "settled_tokens": output["model_requests"]["settled_tokens"],
        "reserved_tokens": output["model_requests"]["reserved_tokens"],
        "budget_unknown": output["model_requests"]["budget_unknown"],
        "snapshot": output["snapshot"]["snapshot_dir"],
        "original_sqlite_opened": output["snapshot"]["original_sqlite_opened"],
    });
    println!("{summary}");
    Ok(())
}
