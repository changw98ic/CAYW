//! Cooperating Host generations. The stable lock inode is never replaced.
//! Arbitrary editors which ignore this protocol are outside the frozen view.
use crate::{Host, failure, filesystem, io_error};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, OpenOptions};
use cayw_ai::CancellationToken;
use cayw_codemode::CapabilityError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::TryLockError,
    io::{Read, Write},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const FENCE: &str = ".cayw-host-read-fence";
const STATE: &str = ".cayw-host-read-state";
const FAULT: &str = ".cayw-host-read-fault";
const READ_FAULTS: &str = ".cayw-host-read-faults";
static NEXT: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenReadStamp {
    pub workspace: String,
    pub incarnation: String,
    pub generation: u64,
    pub semantics: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingMutation {
    pub workspace: String,
    pub incarnation: String,
    pub generation: u64,
    pub audit_id: String,
    pub capability: String,
    pub arguments_sha256: String,
}
/// Trusted SDK evidence obtained after the original owner and Device stopped.
/// Runtime verifies the canonical Core terminal before supplying this value.
#[derive(Clone, Debug)]
pub struct FenceRecovery {
    pub pending: PendingMutation,
    pub stopped_owner: String,
    pub cleanup_evidence: String,
    pub terminal_evidence: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadFenceFault {
    pub stamp: FrozenReadStamp,
    pub audit_id: String,
    pub capability: String,
    pub arguments_sha256: String,
}
#[derive(Clone, Debug)]
pub struct ReadFenceRecovery {
    pub fault: ReadFenceFault,
    pub stopped_owner: String,
    pub cleanup_evidence: String,
    pub terminal_evidence: String,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum FenceFault {
    Mutation { original: PendingMutation },
    Read { original: ReadFenceFault },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    workspace: String,
    incarnation: String,
    generation: u64,
    reuse_disabled: bool,
    pending: Option<PendingMutation>,
}
pub(crate) struct ReadLease {
    _file: std::fs::File,
    pub stamp: FrozenReadStamp,
    can_share: bool,
}
pub(crate) struct Mutation {
    _file: std::fs::File,
    state: State,
}
fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
pub(crate) fn arguments_digest(arguments: &serde_json::Value) -> String {
    digest(&serde_json::to_vec(arguments).expect("bounded JSON"))
}
fn next() -> Result<u64, CapabilityError> {
    NEXT.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
        .map_err(|_| failure("Host fence counter exhausted"))
}
fn open(root: &Dir, name: &str) -> Result<std::fs::File, CapabilityError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .follow(FollowSymlinks::No)
        .nonblock(true);
    let file = root.open_with(name, &options).map_err(io_error)?;
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err(failure("Host fence must be a regular file"));
    }
    Ok(file.into_std())
}
async fn acquire(
    host: &Host,
    exclusive: bool,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<std::fs::File, CapabilityError> {
    let file = open(&host.root, FENCE)?;
    loop {
        crate::check(cancel, deadline)?;
        let result = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        match result {
            Ok(()) => return Ok(file),
            Err(TryLockError::Error(error)) => return Err(io_error(error)),
            Err(TryLockError::WouldBlock) => tokio::select! { biased;
                _ = cancel.cancelled() => return Err(failure("cancelled while waiting for frozen view")),
                _ = tokio::time::sleep_until(deadline.into()) => return Err(failure("frozen view deadline reached")),
                _ = tokio::time::sleep(Duration::from_millis(5)) => {},
            },
        }
    }
}
fn read_state(host: &Host) -> Result<Option<State>, CapabilityError> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let mut file = match host.root.open_with(STATE, &options) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err(failure("invalid Host generation state"));
    }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(16385)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > 16384 {
        return Err(failure("Host generation state exceeds limit"));
    }
    let state: State =
        serde_json::from_slice(&bytes).map_err(|_| failure("corrupt Host generation state"))?;
    if state.version != 1
        || state.workspace != host.workspace_identity()?
        || !valid_hash(&state.incarnation)
        || state.pending.as_ref().is_some_and(|p| {
            p.workspace != state.workspace
                || p.incarnation != state.incarnation
                || p.generation != state.generation
                || !valid_reference(&p.audit_id, 4096)
                || !valid_reference(&p.capability, 256)
                || !valid_hash(&p.arguments_sha256)
        })
    {
        return Err(failure("Host generation identity is corrupt"));
    }
    Ok(Some(state))
}
fn save(host: &Host, state: &State) -> Result<(), CapabilityError> {
    let name = format!(".cayw-host-read-state-{}-{}", std::process::id(), next()?);
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let mut file = host.root.open_with(&name, &options).map_err(io_error)?;
    let bytes = serde_json::to_vec(state).map_err(|error| failure(error.to_string()))?;
    let result = (|| {
        file.write_all(&bytes).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        host.root
            .rename(&name, &host.root, STATE)
            .map_err(io_error)?;
        sync(&host.root)
    })();
    if result.is_err() {
        let _ = host.root.remove_file(&name);
    }
    result
}
fn sync(root: &Dir) -> Result<(), CapabilityError> {
    root.try_clone()
        .map_err(io_error)?
        .into_std_file()
        .sync_all()
        .map_err(io_error)
}
fn initial(host: &Host) -> Result<State, CapabilityError> {
    let workspace = host.workspace_identity()?;
    let entropy = format!(
        "{workspace}:{}:{}:{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| failure("invalid system time"))?
            .as_nanos(),
        next()?
    );
    Ok(State {
        version: 1,
        workspace,
        incarnation: digest(entropy.as_bytes()),
        generation: 0,
        reuse_disabled: false,
        pending: None,
    })
}
fn fault_dir(host: &Host, create: bool) -> Result<Option<Dir>, CapabilityError> {
    if create {
        match host.root.create_dir(READ_FAULTS) {
            Ok(()) => sync(&host.root)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io_error(error)),
        }
    }
    match host.root.open_dir_nofollow(READ_FAULTS) {
        Ok(dir) => Ok(Some(dir)),
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(error)),
    }
}
fn read_faults(host: &Host) -> Result<Vec<ReadFenceFault>, CapabilityError> {
    let fallback = match read_fence_fault(host)? {
        Some(FenceFault::Read { original }) => Some(original),
        _ => None,
    };
    let Some(dir) = fault_dir(host, false)? else {
        return Ok(fallback.into_iter().collect());
    };
    let mut faults = Vec::new();
    for entry in dir.entries().map_err(io_error)? {
        if faults.len() >= 4096 {
            return Err(CapabilityError::unknown("too many unresolved read fences"));
        }
        let entry = entry.map_err(io_error)?;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No).nonblock(true);
        let mut file = dir
            .open_with(entry.file_name(), &options)
            .map_err(io_error)?;
        if !file.metadata().map_err(io_error)?.is_file() {
            return Err(CapabilityError::unknown("invalid read fence fault record"));
        }
        let mut bytes = Vec::new();
        std::io::Read::by_ref(&mut file)
            .take(16385)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() > 16384 {
            return Err(CapabilityError::unknown("read fence fault exceeds limit"));
        }
        let fault: ReadFenceFault = match serde_json::from_slice(&bytes) {
            Ok(fault) => fault,
            Err(_) => fallback
                .as_ref()
                .filter(|original| {
                    entry.file_name() == std::ffi::OsStr::new(&fault_name(&original.audit_id))
                })
                .cloned()
                .ok_or_else(|| CapabilityError::unknown("corrupt read fence fault"))?,
        };
        if fault.capability != "fs.read"
            || !valid_reference(&fault.audit_id, 4096)
            || fault.stamp.workspace != host.workspace_identity()?
            || !valid_hash(&fault.stamp.incarnation)
            || !valid_hash(&fault.stamp.semantics)
            || !valid_hash(&fault.arguments_sha256)
            || entry.file_name() != std::ffi::OsStr::new(&fault_name(&fault.audit_id))
        {
            return Err(CapabilityError::unknown(
                "read fence fault identity differs from original owner",
            ));
        }
        faults.push(fault);
    }
    if let Some(fallback) = fallback {
        if let Some(existing) = faults
            .iter()
            .find(|fault| fault.audit_id == fallback.audit_id)
        {
            if existing != &fallback {
                return Err(CapabilityError::unknown(
                    "read persistence marker differs from original fault",
                ));
            }
        } else {
            faults.push(fallback);
        }
    }
    faults.sort_by(|a, b| a.audit_id.cmp(&b.audit_id));
    Ok(faults)
}
fn fault_name(audit_id: &str) -> String {
    format!("{:x}", Sha256::digest(audit_id.as_bytes()))
}
fn valid_reference(value: &str, limit: usize) -> bool {
    !value.trim().is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}
fn valid_hash(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
fn persist_read_fault(ticket: &FrozenReadTicket) -> Result<(), CapabilityError> {
    let host = &ticket.call.host;
    host.safety.unknown.store(true, Ordering::Release);
    let dir = fault_dir(host, true)?.expect("created fault directory");
    let record = ReadFenceFault {
        stamp: ticket.lease.stamp.clone(),
        audit_id: ticket.audit_id().into(),
        capability: "fs.read".into(),
        arguments_sha256: arguments_digest(&ticket.call.arguments.0),
    };
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let mut file = dir
        .open_with(fault_name(&record.audit_id), &options)
        .map_err(io_error)?;
    file.write_all(&serde_json::to_vec(&record).map_err(|e| failure(e.to_string()))?)
        .map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    sync(&dir)
}
fn check_fault(host: &Host) -> Result<(), CapabilityError> {
    if !read_faults(host)?.is_empty() {
        return Err(CapabilityError::unknown(
            "original read acknowledgment requires stopped-owner recovery",
        ));
    }
    match host.root.symlink_metadata(FAULT) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(error)),
        Ok(_) => Err(CapabilityError::unknown(
            "frozen read persistence requires original owner recovery",
        )),
    }
}
fn read_fence_fault(host: &Host) -> Result<Option<FenceFault>, CapabilityError> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let mut file = match host.root.open_with(FAULT, &options) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err(CapabilityError::unknown(
            "invalid original persistence fault",
        ));
    }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(16385)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > 16384 {
        return Err(CapabilityError::unknown(
            "original persistence fault exceeds limit",
        ));
    }
    let record: FenceFault = serde_json::from_slice(&bytes)
        .map_err(|_| CapabilityError::unknown("corrupt original persistence fault"))?;
    let valid = match &record {
        FenceFault::Mutation { original } => {
            original.workspace == host.workspace_identity()?
                && valid_hash(&original.incarnation)
                && original.generation > 0
                && valid_reference(&original.audit_id, 4096)
                && valid_reference(&original.capability, 256)
                && valid_hash(&original.arguments_sha256)
        }
        FenceFault::Read { original } => {
            original.stamp.workspace == host.workspace_identity()?
                && valid_hash(&original.stamp.incarnation)
                && valid_hash(&original.stamp.semantics)
                && original.capability == "fs.read"
                && valid_reference(&original.audit_id, 4096)
                && valid_hash(&original.arguments_sha256)
        }
    };
    if !valid {
        return Err(CapabilityError::unknown(
            "invalid original persistence identity",
        ));
    }
    Ok(Some(record))
}
fn pending(host: &Host, state: Option<&State>) -> Result<Option<PendingMutation>, CapabilityError> {
    if let Some(original) = state.and_then(|state| state.pending.as_ref()) {
        return Ok(Some(original.clone()));
    }
    match read_fence_fault(host)? {
        Some(FenceFault::Mutation { original }) => {
            if state.is_some_and(|state| {
                original.workspace != state.workspace
                    || original.incarnation != state.incarnation
                    || (original.generation != state.generation
                        && state.generation.checked_add(1) != Some(original.generation))
            }) {
                return Err(CapabilityError::unknown(
                    "persistence fault differs from original generation",
                ));
            }
            Ok(Some(original))
        }
        Some(FenceFault::Read { .. }) | None => Ok(None),
    }
}
fn read_fault(host: &Host, original: ReadFenceFault) {
    persist_fence_fault(host, &FenceFault::Read { original });
}
fn mutation_fault(host: &Host, original: PendingMutation) {
    persist_fence_fault(host, &FenceFault::Mutation { original });
}
fn persist_fence_fault(host: &Host, record: &FenceFault) {
    host.safety.unknown.store(true, Ordering::Release);
    let persist = (|| {
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        match host.root.open_with(FAULT, &options) {
            Ok(mut file) => {
                file.write_all(
                    &serde_json::to_vec(record).map_err(|error| failure(error.to_string()))?,
                )
                .map_err(io_error)?;
                file.sync_all().map_err(io_error)?;
                sync(&host.root)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(io_error(error)),
        }
    })();
    if let Err(error) = persist {
        *crate::lock(&host.safety.audit_failure) = Some(error.message);
    }
}
impl Host {
    pub(crate) async fn read_lease(
        &self,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<ReadLease, CapabilityError> {
        if !self.config.frozen_reads {
            return Err(failure("frozen file views are not enabled by the host"));
        }
        loop {
            let file = acquire(self, false, cancel, deadline).await?;
            check_fault(self)?;
            if let Some(state) = read_state(self)? {
                if state.pending.is_some() {
                    return Err(CapabilityError::unknown(
                        "workspace mutation requires original owner recovery",
                    ));
                }
                let can_share = !state.reuse_disabled;
                let definition = &self
                    .catalog
                    .get("fs.read")
                    .expect("filesystem catalog")
                    .definition;
                let semantics = digest(&serde_json::to_vec(&serde_json::json!({"contract":"held-root-nofollow-utf8-frozen-v1", "definition":definition, "file_bytes":self.config.max_file_bytes, "argument_bytes":self.config.max_argument_bytes,"result_bytes":self.config.max_result_bytes})).expect("read configuration"));
                return Ok(ReadLease {
                    _file: file,
                    stamp: FrozenReadStamp {
                        workspace: state.workspace,
                        incarnation: state.incarnation,
                        generation: state.generation,
                        semantics,
                    },
                    can_share,
                });
            }
            drop(file);
            let _exclusive = acquire(self, true, cancel, deadline).await?;
            if read_state(self)?.is_none() {
                save(self, &initial(self)?)?;
            }
        }
    }
    pub(crate) async fn mutation(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        audit_id: &str,
        unconfined: bool,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<Mutation, CapabilityError> {
        let file = acquire(self, true, cancel, deadline).await?;
        check_fault(self)?;
        let mut state = read_state(self)?.map_or_else(|| initial(self), Ok)?;
        if state.pending.is_some() {
            return Err(CapabilityError::unknown(
                "workspace mutation requires original owner recovery",
            ));
        }
        state.generation = state
            .generation
            .checked_add(1)
            .ok_or_else(|| failure("workspace generation exhausted"))?;
        state.reuse_disabled |= unconfined;
        state.pending = Some(PendingMutation {
            workspace: state.workspace.clone(),
            incarnation: state.incarnation.clone(),
            generation: state.generation,
            audit_id: audit_id.into(),
            capability: name.into(),
            arguments_sha256: arguments_digest(arguments),
        });
        if let Err(error) = save(self, &state) {
            mutation_fault(
                self,
                state.pending.clone().expect("original mutation retained"),
            );
            return Err(CapabilityError::unknown(error.message));
        }
        Ok(Mutation { _file: file, state })
    }
    pub async fn pending_mutation(
        &self,
        cancel: CancellationToken,
    ) -> Result<Option<PendingMutation>, CapabilityError> {
        let deadline = Instant::now()
            .checked_add(self.config.call_timeout)
            .ok_or_else(|| failure("invalid deadline"))?;
        let _file = acquire(self, false, &cancel, deadline).await?;
        pending(self, read_state(self)?.as_ref())
    }
    pub async fn pending_read_faults(
        &self,
        cancel: CancellationToken,
    ) -> Result<Vec<ReadFenceFault>, CapabilityError> {
        let deadline = Instant::now()
            .checked_add(self.config.call_timeout)
            .ok_or_else(|| failure("invalid deadline"))?;
        let _file = acquire(self, false, &cancel, deadline).await?;
        read_faults(self)
    }
    pub async fn recover_read_fault(
        &self,
        evidence: ReadFenceRecovery,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError> {
        for reference in [
            &evidence.stopped_owner,
            &evidence.cleanup_evidence,
            &evidence.terminal_evidence,
        ] {
            if reference.trim().is_empty() || reference.len() > 4096 {
                return Err(failure(
                    "exact stopped owner, cleanup and terminal evidence are required",
                ));
            }
        }
        let deadline = Instant::now()
            .checked_add(self.config.call_timeout)
            .ok_or_else(|| failure("invalid deadline"))?;
        let _file = acquire(self, true, &cancel, deadline).await?;
        let state =
            read_state(self)?.ok_or_else(|| failure("original generation state is absent"))?;
        if evidence.fault.stamp.workspace != state.workspace
            || evidence.fault.stamp.incarnation != state.incarnation
            || evidence.fault.stamp.generation != state.generation
            || !read_faults(self)?.contains(&evidence.fault)
        {
            return Err(failure("recovery differs from original abandoned read"));
        }
        let fallback = match read_fence_fault(self)? {
            Some(FenceFault::Read { original }) if original == evidence.fault => true,
            Some(_) => {
                return Err(CapabilityError::unknown(
                    "a different persistence fault requires its original owner",
                ));
            }
            None => false,
        };
        let removal = (|| {
            if let Some(dir) = fault_dir(self, false)? {
                match dir.remove_file(fault_name(&evidence.fault.audit_id)) {
                    Ok(()) => {}
                    Err(error) if fallback && error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(io_error(error)),
                }
                sync(&dir)?;
            } else if !fallback {
                return Err(failure("read fault directory absent"));
            }
            if fallback {
                self.root.remove_file(FAULT).map_err(io_error)?;
                sync(&self.root)?;
            }
            Ok(())
        })();
        if let Err(error) = removal {
            read_fault(self, evidence.fault);
            return Err(CapabilityError::unknown(error.message));
        }
        Ok(())
    }
    pub async fn recover_mutation(
        &self,
        evidence: FenceRecovery,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError> {
        for reference in [
            &evidence.stopped_owner,
            &evidence.cleanup_evidence,
            &evidence.terminal_evidence,
        ] {
            if reference.trim().is_empty() || reference.len() > 4096 {
                return Err(failure(
                    "exact stopped owner, cleanup and terminal evidence are required",
                ));
            }
        }
        let deadline = Instant::now()
            .checked_add(self.config.call_timeout)
            .ok_or_else(|| failure("invalid deadline"))?;
        let _file = acquire(self, true, &cancel, deadline).await?;
        let saved = read_state(self)?;
        if pending(self, saved.as_ref())?.as_ref() != Some(&evidence.pending) {
            return Err(failure("recovery differs from original mutation fence"));
        }
        let mut state = saved.unwrap_or_else(|| State {
            version: 1,
            workspace: evidence.pending.workspace.clone(),
            incarnation: evidence.pending.incarnation.clone(),
            generation: evidence.pending.generation,
            // A lost state cannot prove that an earlier unconfined route never
            // ran. Preserve a conservative original uncached view after recovery.
            reuse_disabled: true,
            pending: Some(evidence.pending.clone()),
        });
        state.generation = evidence.pending.generation;
        state.pending = Some(evidence.pending.clone());
        if let Some(marker) = read_fence_fault(self)? {
            let matches = match marker {
                FenceFault::Mutation { original } => original == evidence.pending,
                FenceFault::Read { .. } => false,
            };
            if !matches {
                return Err(CapabilityError::unknown(
                    "a different persistence fault requires its original owner",
                ));
            }
            // Reestablish the durable pending record before removing its marker:
            // a prior rename may be visible even when its directory sync failed.
            state.pending = Some(evidence.pending.clone());
            save(self, &state)?;
            self.root.remove_file(FAULT).map_err(io_error)?;
            sync(&self.root)?;
        }
        state.pending = None;
        if let Err(error) = save(self, &state) {
            mutation_fault(self, evidence.pending);
            return Err(CapabilityError::unknown(error.message));
        }
        Ok(())
    }
}
impl Mutation {
    pub(crate) fn settle(mut self, host: &Host) -> Result<(), CapabilityError> {
        let original = self.state.pending.take().expect("owned pending mutation");
        if let Err(error) = save(host, &self.state) {
            mutation_fault(host, original);
            return Err(CapabilityError::unknown(error.message));
        }
        Ok(())
    }
}

// A live read keeps the fence even after its audit is settled; Cells owns the
// final release. The exact UTF-8 bytes are captured once, never reconstructed
// from metadata or by rereading after a shared result arrives.
pub struct FrozenReadTicket {
    pub(crate) call: crate::OwnedCall,
    pub(crate) lease: ReadLease,
    acknowledged: bool,
}
#[derive(Clone, Debug)]
pub struct FrozenReadValue {
    stamp: FrozenReadStamp,
    value: serde_json::Value,
}
impl FrozenReadTicket {
    pub(crate) fn new(call: crate::OwnedCall, lease: ReadLease) -> Self {
        Self {
            call,
            lease,
            acknowledged: false,
        }
    }
    pub fn stamp(&self) -> &FrozenReadStamp {
        &self.lease.stamp
    }
    pub fn can_share(&self) -> bool {
        self.lease.can_share
    }
    pub fn audit_id(&self) -> &str {
        &self.call.guard.id
    }
    pub fn deadline(&self) -> Instant {
        self.call.deadline
    }
    pub fn check(&self) -> Result<(), CapabilityError> {
        self.call.host.check(&self.call.child, self.call.deadline)
    }
    pub async fn capture(&mut self) -> Result<FrozenReadValue, CapabilityError> {
        self.check()?;
        let value = filesystem::execute(
            &self.call.host,
            "fs.read",
            &self.call.arguments.0,
            &self.call.child,
            self.call.deadline,
        )
        .await?;
        crate::json_size(&value, self.call.host.config.max_result_bytes)?;
        Ok(FrozenReadValue {
            stamp: self.lease.stamp.clone(),
            value,
        })
    }
    pub fn accept(
        &self,
        stamp: &FrozenReadStamp,
        value: serde_json::Value,
    ) -> Result<FrozenReadValue, CapabilityError> {
        let value = crate::Arguments(value);
        self.check()?;
        crate::json_size(&value.0, self.call.host.config.max_result_bytes)?;
        if stamp != self.stamp() || value.0.get("path") != self.call.arguments.0.get("path") {
            return Err(failure("shared value differs from original frozen read"));
        }
        let content = value
            .0
            .get("content")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| failure("frozen value has no exact bytes"))?;
        if content.len() > self.call.host.config.max_file_bytes
            || value.0["bytes"].as_u64() != Some(content.len() as u64)
            || value.0["sha256"].as_str()
                != Some(format!("{:x}", Sha256::digest(content.as_bytes())).as_str())
            || value.0.as_object().is_none_or(|o| o.len() != 4)
        {
            return Err(failure("frozen value byte evidence does not match"));
        }
        Ok(FrozenReadValue {
            stamp: stamp.clone(),
            value: value.0.clone(),
        })
    }
    pub async fn settle(
        &mut self,
        result: Result<FrozenReadValue, CapabilityError>,
    ) -> Result<serde_json::Value, CapabilityError> {
        let result = result.and_then(|value| {
            self.check()?;
            if value.stamp != self.lease.stamp {
                return Err(failure("frozen generation mismatch"));
            }
            Ok(value.value)
        });
        self.call.settle(result, false).await
    }
    /// Infallible release only, called after the original Cells terminal commit.
    pub fn acknowledge(mut self) {
        self.acknowledged = true;
    }
}
impl FrozenReadValue {
    pub fn value(&self) -> &serde_json::Value {
        &self.value
    }
}
impl Drop for FrozenReadTicket {
    fn drop(&mut self) {
        if !self.acknowledged
            && let Err(error) = persist_read_fault(self)
        {
            *crate::lock(&self.call.host.safety.audit_failure) = Some(error.message);
            read_fault(
                &self.call.host,
                ReadFenceFault {
                    stamp: self.lease.stamp.clone(),
                    audit_id: self.call.guard.id.clone(),
                    capability: "fs.read".into(),
                    arguments_sha256: arguments_digest(&self.call.arguments.0),
                },
            );
        }
    }
}
