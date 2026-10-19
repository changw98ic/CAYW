//! Run with an explicit NEW directory; this example never starts network transport.
use cayw_content::{
    ContentStore, Error, ExpectedRevision, Id, KeyHandle, Limits, RestoreMode, Result,
};
use rand_core::{OsRng, RngCore};

fn id(value: &str) -> Result<Id> {
    Id::new(value)
}

fn main() -> Result<()> {
    let path = std::env::args_os().nth(1).ok_or_else(|| {
        std::io::Error::other(
            "usage: cargo run -p cayw-content --example offline -- /absolute/new/private-directory",
        )
    })?;
    let path = std::path::PathBuf::from(path);
    if !path.is_absolute() || path.exists() {
        return Err(
            std::io::Error::other("provide an absolute directory that does not exist yet").into(),
        );
    }
    let store = ContentStore::open(&path, id("offline-device")?, Limits::default())?;
    let first = store.put(
        &id("notes")?,
        ExpectedRevision::Absent,
        b"first offline content",
    )?;
    drop(store);
    let store = ContentStore::open(&path, id("offline-device")?, Limits::default())?;
    let (_, bytes) = store.read(&id("notes")?)?.ok_or(Error::NotFound)?;
    assert_eq!(bytes, b"first offline content");
    println!(
        "Reopened local record: version {}, SHA-256 {}",
        first.version, first.content_hash
    );

    let mut secret = [0; 32];
    OsRng.fill_bytes(&mut secret);
    let key = KeyHandle::from_bytes(id("ephemeral-example-key")?, secret);
    // Move-only KeyHandle zeroizes its own copy. Clear this caller-owned input too.
    use zeroize::Zeroize;
    secret.zeroize();
    store.register_key(&key)?;
    let snapshot = store.export_snapshot(&id("notes")?, &key)?;
    println!(
        "Created encrypted object {} ({} ciphertext bytes)",
        snapshot.manifest.upload_id()?,
        snapshot.ciphertext.len()
    );

    let second = store.put(&id("notes")?, first.expected(), b"newer local content")?;
    assert!(matches!(
        store.restore(&snapshot, &key, second.expected(), RestoreMode::Continue),
        Err(Error::RestoreConflict)
    ));
    let recovered = store.restore(
        &snapshot,
        &key,
        ExpectedRevision::Absent,
        RestoreMode::Fork {
            name: id("recovered-notes")?,
        },
    )?;
    assert_eq!(
        store.read(&id("notes")?)?.ok_or(Error::NotFound)?.1,
        b"newer local content"
    );
    assert_eq!(
        store.read(&recovered.name)?.ok_or(Error::NotFound)?.1,
        b"first offline content"
    );
    println!(
        "Stale restore refused; explicitly recovered into '{}'",
        recovered.name
    );

    store.revoke_key(key.id())?;
    assert!(matches!(
        store.export_snapshot(&id("notes")?, &key),
        Err(Error::RevokedKey)
    ));
    println!("Revoked key blocks new encrypted export; local plaintext remains readable.");
    println!("Local store: {}", path.display());
    println!("No network or model calls. The example key was ephemeral and was not saved.");
    Ok(())
}
