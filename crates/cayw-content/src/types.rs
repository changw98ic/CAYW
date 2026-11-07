use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::{fmt, io};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid identifier; use 1-64 ASCII letters, digits, '_' or '-'")]
    InvalidId,
    #[error("invalid content hash")]
    InvalidHash,
    #[error("store path must be absolute, without parent components or symlinks")]
    UnsafePath,
    #[error("content directory must not be writable by group or other users")]
    UnsafePermissions,
    #[error("content root identity differs from the original directory: device or inode changed")]
    RootIdentityMismatch,
    #[error("content exceeds the configured length limit")]
    ContentTooLarge,
    #[error("store quota exceeded")]
    QuotaExceeded,
    #[error("invalid store limits")]
    InvalidLimits,
    #[error("store device or limits differ from the persisted configuration")]
    StoreConfigurationMismatch,
    #[error("record revision conflict")]
    Conflict {
        expected: ExpectedRevision,
        actual: ExpectedRevision,
    },
    #[error("record not found")]
    NotFound,
    #[error("content store is busy; retry after the current transaction finishes")]
    Busy,
    #[error("content, metadata or store integrity check failed")]
    Integrity,
    #[error("unsupported protocol or store version")]
    UnsupportedVersion,
    #[error("key id is not registered")]
    UnknownKey,
    #[error("key id has been permanently revoked")]
    RevokedKey,
    #[error("snapshot authentication failed")]
    Authentication,
    #[error("nonce or snapshot has already been consumed")]
    Replay,
    #[error("restore is stale or not a direct continuation of the local branch")]
    RestoreConflict,
    #[error("remote object conflict")]
    RemoteConflict,
    #[error("remote operation failed; retry the same snapshot and upload id")]
    Remote,
    #[error("commit may have completed; reread local state before retrying")]
    CommitOutcomeUnknown(#[source] io::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("invalid serialized store or snapshot")]
    Encoding(#[from] serde_json::Error),
}

/// An opaque identifier, never a filesystem path. Validation also runs on JSON input.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(transparent)]
pub struct Id(String);

impl Id {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(Error::InvalidId);
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(transparent)]
pub struct ContentHash([u8; 32]);
impl ContentHash {
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
    pub(crate) fn random() -> Result<Self> {
        use rand_core::RngCore;
        let mut value = [0; 32];
        rand_core::OsRng
            .try_fill_bytes(&mut value)
            .map_err(|_| io::Error::other("OS random source unavailable"))?;
        Ok(Self(value))
    }
    pub(crate) fn parse_filename(value: &str) -> Result<Self> {
        Self::from_hex(value)
    }
    /// Parses the canonical 64-character lowercase SHA-256 representation.
    pub fn from_hex(value: &str) -> Result<Self> {
        if value.len() != 64 {
            return Err(Error::InvalidHash);
        }
        let mut bytes = [0; 32];
        for (i, slot) in bytes.iter_mut().enumerate() {
            let part = value.get(2 * i..2 * i + 2).ok_or(Error::InvalidHash)?;
            if !part
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(Error::InvalidHash);
            }
            *slot = u8::from_str_radix(part, 16).map_err(|_| Error::InvalidHash)?;
        }
        Ok(Self(bytes))
    }
}
impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Ok(Self(<[u8; 32]>::deserialize(d)?))
    }
}
impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}
impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Both fields must match, so equal version numbers on different branches conflict.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ExpectedRevision {
    Absent,
    Present { version: u64, revision: ContentHash },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotOrigin {
    pub device_id: Id,
    pub revision: ContentHash,
    pub version: u64,
    pub lineage: ContentHash,
    pub manifest_hash: ContentHash,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub name: Id,
    pub version: u64,
    pub content_hash: ContentHash,
    pub content_len: u64,
    pub device_id: Id,
    pub lineage: ContentHash,
    pub parent_revision: Option<ContentHash>,
    pub restored_from: Option<SnapshotOrigin>,
    pub revision: ContentHash,
}
impl Record {
    pub fn expected(&self) -> ExpectedRevision {
        ExpectedRevision::Present {
            version: self.version,
            revision: self.revision,
        }
    }
    pub(crate) fn calculate_revision(&self) -> Result<ContentHash> {
        Ok(ContentHash::of(&serde_json::to_vec(&(
            "cayw-content-record-v1",
            &self.name,
            self.version,
            self.content_hash,
            self.content_len,
            &self.device_id,
            self.lineage,
            self.parent_revision,
            &self.restored_from,
        ))?))
    }
    pub(crate) fn validate(&self) -> Result<()> {
        if self.version == 0 || self.revision != self.calculate_revision()? {
            return Err(Error::Integrity);
        }
        Ok(())
    }
}

/// Identity and current OS mode of the content directory held open by a store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RootMetadata {
    pub device: u64,
    pub inode: u64,
    pub mode: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_content_bytes: u64,
    /// Counts all retained objects, including older versions and interrupted writes.
    pub max_total_bytes: u64,
    pub max_records: u32,
    /// Bounds the total persistent key and nonce registry entries.
    pub max_security_entries: u32,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_content_bytes: 16 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
            max_records: 1024,
            max_security_entries: 16384,
        }
    }
}
impl Limits {
    pub fn validate(self) -> Result<()> {
        if self.max_content_bytes == 0
            || self.max_content_bytes > 1024 * 1024 * 1024
            || self.max_total_bytes < self.max_content_bytes
            || self.max_records == 0
            || self.max_records > 10000
            || self.max_security_entries == 0
            || self.max_security_entries > 65536
        {
            return Err(Error::InvalidLimits);
        }
        Ok(())
    }
}
