use crate::{
    ContentHash, ContentStore, Error, ExpectedRevision, Id, Record, Result, SnapshotOrigin,
    local::{KeyState, State},
};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
};
use serde::{Deserialize, Serialize};
use std::fmt;
use zeroize::Zeroizing;

const PROTOCOL: u32 = 1;
const TAG_BYTES: u64 = 16;
const WIRE_OVERHEAD: u64 = 16 * 1024;

/// Owned by the caller, supplied explicitly to every cryptographic operation.
/// Dropping the handle zeroizes its copy. The caller owns upstream key lifecycle
/// and must not recycle a key id or assign aliases to the same secret.
pub struct KeyHandle {
    id: Id,
    bytes: Zeroizing<[u8; 32]>,
}
impl KeyHandle {
    pub fn from_bytes(id: Id, bytes: [u8; 32]) -> Self {
        Self {
            id,
            bytes: Zeroizing::new(bytes),
        }
    }
    pub fn id(&self) -> &Id {
        &self.id
    }
    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new((&*self.bytes).into())
    }
}
impl fmt::Debug for KeyHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyHandle")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// Public metadata. The complete record, key id, protocol and nonce are authenticated
/// by AEAD. Ciphertext has its own SHA-256 digest and the upload id binds both.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub protocol_version: u32,
    pub key_id: Id,
    pub record: Record,
    pub nonce: [u8; 24],
    pub ciphertext_hash: ContentHash,
    pub ciphertext_len: u64,
}
impl Manifest {
    fn aad(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&(
            "cayw-content-snapshot-aad-v1",
            self.protocol_version,
            &self.key_id,
            &self.record,
            self.nonce,
            self.ciphertext_len,
        ))?)
    }
    pub fn upload_id(&self) -> Result<ContentHash> {
        Ok(ContentHash::of(&serde_json::to_vec(&(
            "cayw-content-upload-v1",
            self,
        ))?))
    }
    fn nonce_id(&self) -> Result<ContentHash> {
        Ok(ContentHash::of(&serde_json::to_vec(&(
            "cayw-content-nonce-v1",
            &self.key_id,
            self.nonce,
        ))?))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncryptedSnapshot {
    pub manifest: Manifest,
    pub ciphertext: Vec<u8>,
}
impl EncryptedSnapshot {
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }
    /// Rejects oversize encoded input before deserializing it. A JSON byte array
    /// occupies up to four times its binary size; metadata has a fixed allowance.
    pub fn from_bytes(bytes: &[u8], max_content_bytes: u64) -> Result<Self> {
        if bytes.len() as u64 > Self::max_wire_bytes(max_content_bytes)? {
            return Err(Error::ContentTooLarge);
        }
        let snapshot: Self = serde_json::from_slice(bytes)?;
        snapshot.validate(max_content_bytes)?;
        Ok(snapshot)
    }
    pub fn max_wire_bytes(max_content_bytes: u64) -> Result<u64> {
        max_content_bytes
            .checked_add(TAG_BYTES)
            .and_then(|v| v.checked_mul(4))
            .and_then(|v| v.checked_add(WIRE_OVERHEAD))
            .ok_or(Error::ContentTooLarge)
    }
    fn validate(&self, max_content_bytes: u64) -> Result<()> {
        let m = &self.manifest;
        if m.protocol_version != PROTOCOL {
            return Err(Error::UnsupportedVersion);
        }
        m.record.validate()?;
        if m.record.content_len > max_content_bytes {
            return Err(Error::ContentTooLarge);
        }
        if m.ciphertext_len
            != m.record
                .content_len
                .checked_add(TAG_BYTES)
                .ok_or(Error::Integrity)?
            || m.ciphertext_len != self.ciphertext.len() as u64
            || ContentHash::of(&self.ciphertext) != m.ciphertext_hash
        {
            return Err(Error::Integrity);
        }
        Ok(())
    }
    fn decrypt(&self, key: &KeyHandle, max: u64) -> Result<Zeroizing<Vec<u8>>> {
        self.validate(max)?;
        if self.manifest.key_id != key.id {
            return Err(Error::UnknownKey);
        }
        let plaintext = Zeroizing::new(
            key.cipher()
                .decrypt(
                    XNonce::from_slice(&self.manifest.nonce),
                    Payload {
                        msg: &self.ciphertext,
                        aad: &self.manifest.aad()?,
                    },
                )
                .map_err(|_| Error::Authentication)?,
        );
        if plaintext.len() as u64 != self.manifest.record.content_len
            || ContentHash::of(&plaintext) != self.manifest.record.content_hash
        {
            return Err(Error::Integrity);
        }
        Ok(plaintext)
    }
}

/// Continue only a proven direct continuation of the currently observed branch.
/// Recovery of an old or unrelated snapshot must create a new, explicitly named record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RestoreMode {
    Continue,
    Fork { name: Id },
}

/// The host supplies transport and authorization. No network is created by this crate.
///
/// Implementations MUST bound reads, durably stage bytes by upload id, and atomically
/// validate/promote them with a head CAS on `(device_id, record_name)`. The expected
/// head comes from the caller. A committed upload id is an immutable receipt: retry
/// succeeds without moving the head, even after a newer upload commits. Interrupted
/// staging is invisible to committed reads; retry replaces a partial stage. A commit
/// with altered bytes/manifest under the same id is rejected. This trait cannot enforce
/// a remote service's implementation, so an untrusted remote is always revalidated.
pub trait RemoteObjectStore: Send + Sync {
    fn stage(&self, upload_id: ContentHash, bytes: &[u8]) -> Result<()>;
    fn read_staged(&self, upload_id: ContentHash, max_bytes: u64) -> Result<Option<Vec<u8>>>;
    fn commit(
        &self,
        upload_id: ContentHash,
        expected_head: Option<ContentHash>,
        manifest: &Manifest,
    ) -> Result<()>;
    fn read_committed(&self, upload_id: ContentHash, max_bytes: u64) -> Result<Option<Vec<u8>>>;
    fn head(&self, device: &Id, record: &Id) -> Result<Option<ContentHash>>;
}

impl ContentStore {
    /// Registers an allowed id only; never stores key bytes. Revocation is irreversible.
    pub fn register_key(&self, key: &KeyHandle) -> Result<()> {
        self.transaction(|state, _| {
            match state.keys.get(key.id()) {
                Some(KeyState::Revoked) => return Err(Error::RevokedKey),
                Some(KeyState::Active) => return Ok(()),
                None => {}
            }
            state.security_capacity()?;
            state.keys.insert(key.id().clone(), KeyState::Active);
            self.save_state(state)
        })
    }
    /// A local durable deny entry. It cannot erase previously disclosed plaintext or
    /// keys, or revoke copies in another store without host propagation.
    pub fn revoke_key(&self, key_id: &Id) -> Result<()> {
        self.transaction(|state, _| {
            if !state.keys.contains_key(key_id) {
                state.security_capacity()?;
            }
            state.keys.insert(key_id.clone(), KeyState::Revoked);
            self.save_state(state)
        })
    }
    pub fn export_snapshot(&self, name: &Id, key: &KeyHandle) -> Result<EncryptedSnapshot> {
        self.transaction(|state, dirs| {
            require_key(state, key.id())?;
            state.security_capacity()?;
            let record = state.records.get(name).ok_or(Error::NotFound)?.clone();
            let plaintext = Zeroizing::new(self.read_content(dirs, &record)?);
            let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
            let mut manifest = Manifest {
                protocol_version: PROTOCOL,
                key_id: key.id().clone(),
                record,
                nonce: nonce.into(),
                ciphertext_hash: ContentHash::of(b""),
                ciphertext_len: plaintext.len() as u64 + TAG_BYTES,
            };
            let nonce_id = manifest.nonce_id()?;
            if state.exported.contains_key(&nonce_id.to_string())
                || state.imported.contains(&nonce_id)
            {
                return Err(Error::Replay);
            }
            let ciphertext = key
                .cipher()
                .encrypt(
                    &nonce,
                    Payload {
                        msg: &plaintext,
                        aad: &manifest.aad()?,
                    },
                )
                .map_err(|_| Error::Authentication)?;
            manifest.ciphertext_hash = ContentHash::of(&ciphertext);
            state
                .exported
                .insert(nonce_id.to_string(), manifest.upload_id()?);
            self.save_state(state)?;
            Ok(EncryptedSnapshot {
                manifest,
                ciphertext,
            })
        })
    }
    /// Decrypts and validates before staging content. Local CAS, branch checks,
    /// replay protection and authority promotion share one exclusive transaction.
    /// The caller's expected revision is mandatory; Fork always requires Absent.
    pub fn restore(
        &self,
        snapshot: &EncryptedSnapshot,
        key: &KeyHandle,
        expected: ExpectedRevision,
        mode: RestoreMode,
    ) -> Result<Record> {
        snapshot.validate(self.limits().max_content_bytes)?;
        self.transaction(|state, dirs| {
            require_key(state, key.id())?;
            if key.id() != &snapshot.manifest.key_id {
                return Err(Error::UnknownKey);
            }
            let nonce_id = snapshot.manifest.nonce_id()?;
            let upload_id = snapshot.manifest.upload_id()?;
            if state.imported.contains(&nonce_id)
                || state
                    .exported
                    .get(&nonce_id.to_string())
                    .is_some_and(|id| *id != upload_id)
            {
                return Err(Error::Replay);
            }
            let source = &snapshot.manifest.record;
            let target = match &mode {
                RestoreMode::Continue => source.name.clone(),
                RestoreMode::Fork { name } => name.clone(),
            };
            state.check_expected(&target, expected)?;
            match mode {
                RestoreMode::Fork { .. } => {
                    if expected != ExpectedRevision::Absent {
                        return Err(Error::RestoreConflict);
                    }
                }
                RestoreMode::Continue => {
                    if let Some(local) = state.records.get(&target) {
                        let direct = source.lineage == local.lineage
                            && source.parent_revision == Some(local.revision)
                            && source.version
                                == local.version.checked_add(1).ok_or(Error::RestoreConflict)?;
                        let continuation_of_restored =
                            local.restored_from.as_ref().is_some_and(|origin| {
                                origin.device_id == source.device_id
                                    && origin.lineage == source.lineage
                                    && source.parent_revision == Some(origin.revision)
                                    && origin.version.checked_add(1) == Some(source.version)
                            });
                        if !direct && !continuation_of_restored {
                            return Err(Error::RestoreConflict);
                        }
                    }
                }
            }
            let plaintext = snapshot.decrypt(key, self.limits().max_content_bytes)?;
            state.security_capacity()?;
            state.imported.insert(nonce_id);
            self.put_locked(
                state,
                dirs,
                &target,
                &plaintext,
                Some(SnapshotOrigin {
                    device_id: source.device_id.clone(),
                    revision: source.revision,
                    version: source.version,
                    lineage: source.lineage,
                    manifest_hash: snapshot.manifest.upload_id()?,
                }),
            )
        })
    }
    /// Uses the SAME snapshot/id for retries. Uploading a backup never changes the
    /// local record. Remote calls run outside the local lock; keys are rechecked at
    /// the final commit boundary, so a failed or interrupted stage is not authoritative.
    pub fn upload_snapshot(
        &self,
        snapshot: &EncryptedSnapshot,
        key: &KeyHandle,
        remote: &dyn RemoteObjectStore,
        expected_head: Option<ContentHash>,
    ) -> Result<ContentHash> {
        self.transaction(|state, _| {
            require_key(state, key.id())?;
            snapshot.decrypt(key, self.limits().max_content_bytes)?;
            if state
                .exported
                .get(&snapshot.manifest.nonce_id()?.to_string())
                != Some(&snapshot.manifest.upload_id()?)
            {
                return Err(Error::Integrity);
            }
            Ok(())
        })?;
        let upload_id = snapshot.manifest.upload_id()?;
        let wire = snapshot.to_bytes()?;
        let max = EncryptedSnapshot::max_wire_bytes(self.limits().max_content_bytes)?;
        remote.stage(upload_id, &wire)?;
        let staged = remote.read_staged(upload_id, max)?.ok_or(Error::Remote)?;
        if staged.len() as u64 > max || staged != wire {
            return Err(Error::Integrity);
        }
        let checked = EncryptedSnapshot::from_bytes(&staged, self.limits().max_content_bytes)?;
        if checked.manifest.upload_id()? != upload_id {
            return Err(Error::Integrity);
        }
        // Linearizes local revocation against the remote commit. Host callbacks must
        // not call back into this ContentStore while commit is in progress.
        self.transaction(|state, _| {
            require_key(state, key.id())?;
            remote.commit(upload_id, expected_head, &snapshot.manifest)
        })?;
        Ok(upload_id)
    }
    /// Fetch is untrusted data only, with no local authority changes. Restore is a
    /// separate explicit action with a key and expected revision.
    pub fn fetch_snapshot(
        &self,
        remote: &dyn RemoteObjectStore,
        upload_id: ContentHash,
    ) -> Result<EncryptedSnapshot> {
        let max = EncryptedSnapshot::max_wire_bytes(self.limits().max_content_bytes)?;
        let wire = remote
            .read_committed(upload_id, max)?
            .ok_or(Error::NotFound)?;
        if wire.len() as u64 > max {
            return Err(Error::ContentTooLarge);
        }
        let snapshot = EncryptedSnapshot::from_bytes(&wire, self.limits().max_content_bytes)?;
        if snapshot.manifest.upload_id()? != upload_id {
            return Err(Error::Integrity);
        }
        Ok(snapshot)
    }
}

fn require_key(state: &State, key_id: &Id) -> Result<()> {
    match state.keys.get(key_id) {
        Some(KeyState::Active) => Ok(()),
        Some(KeyState::Revoked) => Err(Error::RevokedKey),
        None => Err(Error::UnknownKey),
    }
}
