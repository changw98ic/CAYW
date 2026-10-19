#![cfg(feature = "encryption")]

use cayw_content::{
    ContentHash, ContentStore, EncryptedSnapshot, Error, ExpectedRevision, Id, KeyHandle, Limits,
    Manifest, RemoteObjectStore, RestoreMode, Result,
};
use std::{collections::BTreeMap, sync::Mutex};
use tempfile::TempDir;

fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}
fn root() -> TempDir {
    TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap()
}
fn store(root: &TempDir, device: &str) -> ContentStore {
    ContentStore::open(root.path(), id(device), Limits::default()).unwrap()
}
fn key() -> KeyHandle {
    KeyHandle::from_bytes(id("key-a"), [42; 32])
}
fn snapshot(source: &ContentStore, key: &KeyHandle) -> EncryptedSnapshot {
    source.register_key(key).unwrap();
    source
        .put(&id("note"), ExpectedRevision::Absent, b"private content")
        .unwrap();
    source.export_snapshot(&id("note"), key).unwrap()
}

#[test]
fn encrypted_backup_round_trip_requires_an_explicit_local_revision() {
    let source_root = root();
    let target_root = root();
    let source = store(&source_root, "device-a");
    let target = store(&target_root, "device-b");
    let key = key();
    let snapshot = snapshot(&source, &key);
    let encoded = snapshot.to_bytes().unwrap();
    assert!(!String::from_utf8_lossy(&encoded).contains("private content"));
    let decoded =
        EncryptedSnapshot::from_bytes(&encoded, Limits::default().max_content_bytes).unwrap();
    assert!(matches!(
        target.restore(
            &decoded,
            &key,
            ExpectedRevision::Absent,
            RestoreMode::Continue
        ),
        Err(Error::UnknownKey)
    ));
    target.register_key(&key).unwrap();
    let local = target
        .restore(
            &decoded,
            &key,
            ExpectedRevision::Absent,
            RestoreMode::Continue,
        )
        .unwrap();
    assert_eq!(local.device_id, id("device-b"));
    assert_eq!(local.version, 1);
    assert_eq!(
        local.restored_from.as_ref().unwrap().device_id,
        id("device-a")
    );
    assert_eq!(
        store(&target_root, "device-b")
            .read(&id("note"))
            .unwrap()
            .unwrap()
            .1,
        b"private content"
    );
    let another = source.export_snapshot(&id("note"), &key).unwrap();
    assert_ne!(snapshot.manifest.nonce, another.manifest.nonce);
    assert_ne!(
        snapshot.manifest.ciphertext_hash,
        another.manifest.ciphertext_hash
    );
    assert!(matches!(
        target.restore(
            &decoded,
            &key,
            ExpectedRevision::Absent,
            RestoreMode::Fork { name: id("replay") }
        ),
        Err(Error::Replay)
    ));
    assert!(!format!("{key:?}").contains("42, 42"));
    assert!(
        !String::from_utf8_lossy(&std::fs::read(source_root.path().join("state.json")).unwrap())
            .contains("42,42,42,42")
    );
}

#[test]
fn wrong_key_ciphertext_tampering_and_authenticated_metadata_are_rejected() {
    let source_root = root();
    let target_root = root();
    let source = store(&source_root, "device-a");
    let target = store(&target_root, "device-b");
    let key = key();
    let original = snapshot(&source, &key);
    target.register_key(&key).unwrap();
    let wrong = KeyHandle::from_bytes(id("key-a"), [7; 32]);
    assert!(matches!(
        target.restore(
            &original,
            &wrong,
            ExpectedRevision::Absent,
            RestoreMode::Continue
        ),
        Err(Error::Authentication)
    ));
    let mut tampered = original.clone();
    tampered.ciphertext[0] ^= 1;
    assert!(matches!(
        target.restore(
            &tampered,
            &key,
            ExpectedRevision::Absent,
            RestoreMode::Continue
        ),
        Err(Error::Integrity)
    ));
    tampered.manifest.ciphertext_hash = ContentHash::of(&tampered.ciphertext);
    assert!(matches!(
        target.restore(
            &tampered,
            &key,
            ExpectedRevision::Absent,
            RestoreMode::Continue
        ),
        Err(Error::Authentication)
    ));
    for field in [
        "device_id",
        "name",
        "version",
        "content_hash",
        "key_id",
        "nonce",
        "protocol_version",
    ] {
        let mut value = serde_json::to_value(&original).unwrap();
        match field {
            "device_id" => {
                value["manifest"]["record"]["device_id"] = serde_json::json!("other-device")
            }
            "name" => value["manifest"]["record"]["name"] = serde_json::json!("other-record"),
            "version" => value["manifest"]["record"]["version"] = serde_json::json!(99),
            "content_hash" => value["manifest"]["record"]["content_hash"][0] = serde_json::json!(0),
            "key_id" => {
                let alias = KeyHandle::from_bytes(id("alias"), [42; 32]);
                target.register_key(&alias).unwrap();
                value["manifest"]["key_id"] = serde_json::json!("alias");
            }
            "nonce" => {
                value["manifest"]["nonce"][0] =
                    serde_json::json!((original.manifest.nonce[0] as u16 + 1) % 256)
            }
            "protocol_version" => value["manifest"]["protocol_version"] = serde_json::json!(2),
            _ => unreachable!(),
        }
        let bad: EncryptedSnapshot = serde_json::from_value(value).unwrap();
        let alias;
        let used_key = if field == "key_id" {
            alias = KeyHandle::from_bytes(id("alias"), [42; 32]);
            &alias
        } else {
            &key
        };
        assert!(
            target
                .restore(
                    &bad,
                    used_key,
                    ExpectedRevision::Absent,
                    RestoreMode::Continue
                )
                .is_err(),
            "accepted substituted {field}"
        );
    }
    assert!(target.read(&id("note")).unwrap().is_none());
    target
        .restore(
            &original,
            &key,
            ExpectedRevision::Absent,
            RestoreMode::Continue,
        )
        .unwrap();
}

#[test]
fn authenticated_metadata_substitution_cannot_be_hidden_by_recomputing_public_hashes() {
    use sha2::{Digest, Sha256};
    let source_root = root();
    let target_root = root();
    let source = store(&source_root, "device-a");
    let target = store(&target_root, "device-b");
    let key = key();
    let original = snapshot(&source, &key);
    target.register_key(&key).unwrap();
    let mut bad = original.clone();
    bad.manifest.record.device_id = id("substitute");
    let record = &bad.manifest.record;
    // Rebuild the public revision digest; only AEAD authentication can now catch the substitution.
    let bytes = serde_json::to_vec(&(
        "cayw-content-record-v1",
        &record.name,
        record.version,
        record.content_hash,
        record.content_len,
        &record.device_id,
        record.lineage,
        record.parent_revision,
        &record.restored_from,
    ))
    .unwrap();
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    let mut value = serde_json::to_value(&bad).unwrap();
    value["manifest"]["record"]["revision"] = serde_json::to_value(digest).unwrap();
    let bad: EncryptedSnapshot = serde_json::from_value(value).unwrap();
    assert!(matches!(
        target.restore(&bad, &key, ExpectedRevision::Absent, RestoreMode::Continue),
        Err(Error::Authentication)
    ));
}

#[test]
fn revocation_survives_reopen_and_prevents_export_import_and_reregistration() {
    let source_root = root();
    let target_root = root();
    let source = store(&source_root, "device-a");
    let target = store(&target_root, "device-b");
    let key = key();
    let snapshot = snapshot(&source, &key);
    source.revoke_key(key.id()).unwrap();
    target.revoke_key(key.id()).unwrap(); // Preemptive revocation is allowed.
    let reopened = store(&source_root, "device-a");
    assert!(matches!(
        reopened.register_key(&key),
        Err(Error::RevokedKey)
    ));
    assert!(matches!(
        reopened.export_snapshot(&id("note"), &key),
        Err(Error::RevokedKey)
    ));
    assert!(matches!(
        store(&target_root, "device-b").restore(
            &snapshot,
            &key,
            ExpectedRevision::Absent,
            RestoreMode::Continue
        ),
        Err(Error::RevokedKey)
    ));
}

#[test]
fn stale_remote_cannot_replace_new_local_even_when_local_revision_is_confirmed() {
    let source_root = root();
    let source = store(&source_root, "device-a");
    let key = key();
    let old = snapshot(&source, &key);
    let current = source.read(&id("note")).unwrap().unwrap().0;
    let newer = source
        .put(&id("note"), current.expected(), b"new local")
        .unwrap();
    assert!(matches!(
        source.restore(&old, &key, current.expected(), RestoreMode::Continue),
        Err(Error::Conflict { .. })
    ));
    assert!(matches!(
        source.restore(&old, &key, newer.expected(), RestoreMode::Continue),
        Err(Error::RestoreConflict)
    ));
    assert_eq!(source.read(&id("note")).unwrap().unwrap().1, b"new local");
    let fork = source
        .restore(
            &old,
            &key,
            ExpectedRevision::Absent,
            RestoreMode::Fork {
                name: id("recovered-old"),
            },
        )
        .unwrap();
    assert_eq!(fork.version, 1);
    assert_eq!(
        source.read(&id("recovered-old")).unwrap().unwrap().1,
        b"private content"
    );
    assert_eq!(source.read(&id("note")).unwrap().unwrap().0, newer);
    // Replay stays rejected after restart and cannot create another fork.
    assert!(matches!(
        store(&source_root, "device-a").restore(
            &old,
            &key,
            ExpectedRevision::Absent,
            RestoreMode::Fork { name: id("again") }
        ),
        Err(Error::Replay)
    ));
}

#[test]
fn a_different_branch_needs_an_explicit_fork() {
    let a_root = root();
    let b_root = root();
    let a = store(&a_root, "device-a");
    let b = store(&b_root, "device-b");
    let key = key();
    let remote = snapshot(&a, &key);
    b.register_key(&key).unwrap();
    let local = b
        .put(&id("note"), ExpectedRevision::Absent, b"other branch")
        .unwrap();
    assert!(matches!(
        b.restore(&remote, &key, local.expected(), RestoreMode::Continue),
        Err(Error::RestoreConflict)
    ));
    let recovered = b
        .restore(
            &remote,
            &key,
            ExpectedRevision::Absent,
            RestoreMode::Fork {
                name: id("separate"),
            },
        )
        .unwrap();
    assert_eq!(
        recovered.restored_from.as_ref().unwrap().revision,
        remote.manifest.record.revision
    );
    assert_eq!(b.read(&id("note")).unwrap().unwrap().1, b"other branch");
}

#[test]
fn key_and_nonce_registry_and_wire_size_are_bounded() {
    let root = root();
    let limits = Limits {
        max_security_entries: 2,
        ..Limits::default()
    };
    let db = ContentStore::open(root.path(), id("device-a"), limits).unwrap();
    let key = key();
    let first = snapshot(&db, &key);
    assert!(matches!(
        db.export_snapshot(&id("note"), &key),
        Err(Error::QuotaExceeded)
    ));
    let mut too_long = first.clone();
    too_long.ciphertext.push(1);
    assert!(EncryptedSnapshot::from_bytes(&too_long.to_bytes().unwrap(), 5).is_err());
    assert!(EncryptedSnapshot::from_bytes(&vec![b' '; 200_000], 8).is_err());
}

#[test]
fn direct_remote_continuation_is_accepted_but_skipped_history_is_rejected() {
    let a_root = root();
    let b_root = root();
    let a = store(&a_root, "device-a");
    let b = store(&b_root, "device-b");
    let key = key();
    let first = snapshot(&a, &key);
    b.register_key(&key).unwrap();
    let b_first = b
        .restore(
            &first,
            &key,
            ExpectedRevision::Absent,
            RestoreMode::Continue,
        )
        .unwrap();
    let a_first = a.read(&id("note")).unwrap().unwrap().0;
    let a_second = a.put(&id("note"), a_first.expected(), b"second").unwrap();
    let second = a.export_snapshot(&id("note"), &key).unwrap();
    let a_third = a.put(&id("note"), a_second.expected(), b"third").unwrap();
    let third = a.export_snapshot(&id("note"), &key).unwrap();
    assert_eq!(a_third.version, 3);
    assert!(matches!(
        b.restore(&third, &key, b_first.expected(), RestoreMode::Continue),
        Err(Error::RestoreConflict)
    ));
    let b_second = b
        .restore(&second, &key, b_first.expected(), RestoreMode::Continue)
        .unwrap();
    assert_eq!(b_second.version, 2);
    assert_eq!(b.read(&id("note")).unwrap().unwrap().1, b"second");
    let b_third = b
        .restore(&third, &key, b_second.expected(), RestoreMode::Continue)
        .unwrap();
    assert_eq!(b_third.version, 3);
    assert_eq!(b.read(&id("note")).unwrap().unwrap().1, b"third");
}

#[test]
fn a_reused_nonce_is_rejected_even_for_valid_new_authenticated_ciphertext() {
    use chacha20poly1305::{
        XChaCha20Poly1305, XNonce,
        aead::{Aead, KeyInit, Payload},
    };
    let a_root = root();
    let b_root = root();
    let a = store(&a_root, "device-a");
    let b = store(&b_root, "device-b");
    let key = key();
    let first = snapshot(&a, &key);
    b.register_key(&key).unwrap();
    let local = b
        .restore(
            &first,
            &key,
            ExpectedRevision::Absent,
            RestoreMode::Continue,
        )
        .unwrap();
    let a_first = a.read(&id("note")).unwrap().unwrap().0;
    a.put(&id("note"), a_first.expected(), b"second").unwrap();
    let mut second = a.export_snapshot(&id("note"), &key).unwrap();
    second.manifest.nonce = first.manifest.nonce;
    let m = &second.manifest;
    let aad = serde_json::to_vec(&(
        "cayw-content-snapshot-aad-v1",
        m.protocol_version,
        &m.key_id,
        &m.record,
        m.nonce,
        m.ciphertext_len,
    ))
    .unwrap();
    second.ciphertext = XChaCha20Poly1305::new((&[42u8; 32]).into())
        .encrypt(
            XNonce::from_slice(&m.nonce),
            Payload {
                msg: b"second",
                aad: &aad,
            },
        )
        .unwrap();
    second.manifest.ciphertext_hash = ContentHash::of(&second.ciphertext);
    assert!(matches!(
        b.restore(&second, &key, local.expected(), RestoreMode::Continue),
        Err(Error::Replay)
    ));
    assert_eq!(b.read(&id("note")).unwrap().unwrap().1, b"private content");
}

#[test]
fn an_exported_nonce_cannot_be_repurposed_before_it_has_been_imported() {
    use chacha20poly1305::{
        XChaCha20Poly1305, XNonce,
        aead::{Aead, KeyInit, Payload},
    };
    let a_root = root();
    let a = store(&a_root, "device-a");
    let key = key();
    let first = snapshot(&a, &key);
    let a_first = a.read(&id("note")).unwrap().unwrap().0;
    a.put(&id("note"), a_first.expected(), b"second").unwrap();
    let mut second = a.export_snapshot(&id("note"), &key).unwrap();
    second.manifest.nonce = first.manifest.nonce;
    let m = &second.manifest;
    let aad = serde_json::to_vec(&(
        "cayw-content-snapshot-aad-v1",
        m.protocol_version,
        &m.key_id,
        &m.record,
        m.nonce,
        m.ciphertext_len,
    ))
    .unwrap();
    second.ciphertext = XChaCha20Poly1305::new((&[42u8; 32]).into())
        .encrypt(
            XNonce::from_slice(&m.nonce),
            Payload {
                msg: b"second",
                aad: &aad,
            },
        )
        .unwrap();
    second.manifest.ciphertext_hash = ContentHash::of(&second.ciphertext);
    assert!(matches!(
        a.restore(
            &second,
            &key,
            ExpectedRevision::Absent,
            RestoreMode::Fork { name: id("forged") }
        ),
        Err(Error::Replay)
    ));
    assert!(matches!(
        a.upload_snapshot(&second, &key, &MemoryRemote::default(), None),
        Err(Error::Integrity)
    ));
    assert!(a.read(&id("forged")).unwrap().is_none());
}

#[test]
fn revocation_blocks_upload_of_an_already_exported_object() {
    let a_root = root();
    let a = store(&a_root, "device-a");
    let key = key();
    let snapshot = snapshot(&a, &key);
    a.revoke_key(key.id()).unwrap();
    let remote = MemoryRemote::default();
    assert!(matches!(
        a.upload_snapshot(&snapshot, &key, &remote, None),
        Err(Error::RevokedKey)
    ));
    assert_eq!(remote.head(&id("device-a"), &id("note")).unwrap(), None);
    assert_eq!(a.read(&id("note")).unwrap().unwrap().1, b"private content");
}

#[derive(Default)]
struct MemoryState {
    staged: BTreeMap<ContentHash, Vec<u8>>,
    committed: BTreeMap<ContentHash, Vec<u8>>,
    heads: BTreeMap<(Id, Id), ContentHash>,
    fail_stage_once: bool,
    lose_commit_response_once: bool,
    corrupt_stage: bool,
}
/// An explicit host fixture. This does not establish real network/two-device behavior.
#[derive(Default)]
struct MemoryRemote(Mutex<MemoryState>);
impl RemoteObjectStore for MemoryRemote {
    fn stage(&self, upload_id: ContentHash, bytes: &[u8]) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        if state.fail_stage_once {
            state.fail_stage_once = false;
            state
                .staged
                .insert(upload_id, bytes[..bytes.len() / 2].to_vec());
            return Err(Error::Remote);
        }
        if let Some(existing) = state.committed.get(&upload_id)
            && existing != bytes
        {
            return Err(Error::RemoteConflict);
        }
        let mut bytes = bytes.to_vec();
        if state.corrupt_stage {
            bytes[0] ^= 1;
        }
        state.staged.insert(upload_id, bytes);
        Ok(())
    }
    fn read_staged(&self, upload_id: ContentHash, max_bytes: u64) -> Result<Option<Vec<u8>>> {
        bounded(self.0.lock().unwrap().staged.get(&upload_id), max_bytes)
    }
    fn commit(
        &self,
        upload_id: ContentHash,
        expected_head: Option<ContentHash>,
        manifest: &Manifest,
    ) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        // Receipts are permanent: retrying an old commit never rewinds a newer head.
        if state.committed.contains_key(&upload_id) {
            return Ok(());
        }
        let name = (
            manifest.record.device_id.clone(),
            manifest.record.name.clone(),
        );
        if state.heads.get(&name).copied() != expected_head {
            return Err(Error::RemoteConflict);
        }
        let bytes = state.staged.get(&upload_id).cloned().ok_or(Error::Remote)?;
        let snapshot = EncryptedSnapshot::from_bytes(&bytes, Limits::default().max_content_bytes)?;
        if snapshot.manifest != *manifest || manifest.upload_id()? != upload_id {
            return Err(Error::Integrity);
        }
        state.committed.insert(upload_id, bytes);
        state.heads.insert(name, upload_id);
        if state.lose_commit_response_once {
            state.lose_commit_response_once = false;
            return Err(Error::Remote);
        }
        Ok(())
    }
    fn read_committed(&self, upload_id: ContentHash, max_bytes: u64) -> Result<Option<Vec<u8>>> {
        bounded(self.0.lock().unwrap().committed.get(&upload_id), max_bytes)
    }
    fn head(&self, device: &Id, record: &Id) -> Result<Option<ContentHash>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .heads
            .get(&(device.clone(), record.clone()))
            .copied())
    }
}
fn bounded(bytes: Option<&Vec<u8>>, max: u64) -> Result<Option<Vec<u8>>> {
    if bytes.is_some_and(|b| b.len() as u64 > max) {
        return Err(Error::ContentTooLarge);
    }
    Ok(bytes.cloned())
}

#[test]
fn interrupted_upload_and_lost_commit_ack_retry_the_same_id_without_local_changes() {
    let source_root = root();
    let target_root = root();
    let source = store(&source_root, "device-a");
    let target = store(&target_root, "device-b");
    let key = key();
    let snapshot = snapshot(&source, &key);
    let local = source.read(&id("note")).unwrap().unwrap().0;
    let remote = MemoryRemote::default();
    remote.0.lock().unwrap().fail_stage_once = true;
    assert!(matches!(
        source.upload_snapshot(&snapshot, &key, &remote, None),
        Err(Error::Remote)
    ));
    assert_eq!(remote.head(&id("device-a"), &id("note")).unwrap(), None);
    assert!(remote.0.lock().unwrap().committed.is_empty());
    remote.0.lock().unwrap().lose_commit_response_once = true;
    assert!(matches!(
        source.upload_snapshot(&snapshot, &key, &remote, None),
        Err(Error::Remote)
    ));
    let upload_id = snapshot.manifest.upload_id().unwrap();
    assert_eq!(
        source
            .upload_snapshot(&snapshot, &key, &remote, None)
            .unwrap(),
        upload_id
    );
    assert_eq!(
        source
            .upload_snapshot(&snapshot, &key, &remote, None)
            .unwrap(),
        upload_id
    );
    assert_eq!(remote.0.lock().unwrap().committed.len(), 1);
    assert_eq!(source.read(&id("note")).unwrap().unwrap().0, local);
    assert!(target.read(&id("note")).unwrap().is_none());
    let downloaded = target.fetch_snapshot(&remote, upload_id).unwrap();
    target.register_key(&key).unwrap();
    target
        .restore(
            &downloaded,
            &key,
            ExpectedRevision::Absent,
            RestoreMode::Continue,
        )
        .unwrap();
    assert_eq!(
        target.read(&id("note")).unwrap().unwrap().1,
        b"private content"
    );
}

#[test]
fn remote_head_cas_and_idempotent_receipts_prevent_rewinding_a_newer_backup() {
    let source_root = root();
    let source = store(&source_root, "device-a");
    let key = key();
    let first = snapshot(&source, &key);
    let remote = MemoryRemote::default();
    let first_id = source.upload_snapshot(&first, &key, &remote, None).unwrap();
    let local = source.read(&id("note")).unwrap().unwrap().0;
    source
        .put(&id("note"), local.expected(), b"new content")
        .unwrap();
    let second = source.export_snapshot(&id("note"), &key).unwrap();
    assert!(matches!(
        source.upload_snapshot(&second, &key, &remote, None),
        Err(Error::RemoteConflict)
    ));
    assert_eq!(
        remote.head(&id("device-a"), &id("note")).unwrap(),
        Some(first_id)
    );
    let second_id = source
        .upload_snapshot(&second, &key, &remote, Some(first_id))
        .unwrap();
    source.upload_snapshot(&first, &key, &remote, None).unwrap();
    assert_eq!(
        remote.head(&id("device-a"), &id("note")).unwrap(),
        Some(second_id)
    );
    assert_eq!(remote.0.lock().unwrap().committed.len(), 2);
}

#[test]
fn staged_bytes_must_be_read_back_and_verified_before_remote_commit() {
    let source_root = root();
    let source = store(&source_root, "device-a");
    let key = key();
    let snapshot = snapshot(&source, &key);
    let remote = MemoryRemote::default();
    remote.0.lock().unwrap().corrupt_stage = true;
    assert!(
        source
            .upload_snapshot(&snapshot, &key, &remote, None)
            .is_err()
    );
    assert_eq!(remote.head(&id("device-a"), &id("note")).unwrap(), None);
    assert!(remote.0.lock().unwrap().committed.is_empty());
}
