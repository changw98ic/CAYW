use crate::{Context, Workspace, failure, filesystem as fs, io_error, parse, unknown};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use cayw_codemode::CapabilityError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Blob {
    sha256: String,
    bytes: usize,
    mode: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    path: String,
    operation: String,
    old: Option<Blob>,
    new: Option<Blob>,
    language: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    batch_id: String,
    operations: Vec<Operation>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Prepared,
    Intent,
    Committing,
    Committed,
    Aborted,
    RecoveringCommit,
    RecoveringRollback,
    RolledBack,
    OutcomeUnknown,
}
impl Phase {
    fn pending(self) -> bool {
        matches!(
            self,
            Self::Intent
                | Self::Committing
                | Self::RecoveringCommit
                | Self::RecoveringRollback
                | Self::OutcomeUnknown
        )
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    batch_digest: String,
    phase: Phase,
    installed: usize,
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Expected {
    Absent,
    Sha256 { sha256: String },
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Encoding {
    Utf8,
    Base64,
}
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Input {
    Write {
        path: String,
        expected: Expected,
        content: String,
        encoding: Encoding,
        language: String,
    },
    Patch {
        path: String,
        expected_sha256: String,
        old: String,
        new: String,
        language: String,
    },
    Remove {
        path: String,
        expected_sha256: String,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareArgs {
    operations: Vec<Input>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitArgs {
    batch_id: String,
    expected_batch_digest: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    batch_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Recovery {
    Commit,
    Rollback,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoverArgs {
    batch_id: String,
    expected_batch_digest: String,
    action: Recovery,
}

struct Journal {
    dir: Dir,
    manifest: Manifest,
    digest: String,
    state: State,
}
struct Anchor {
    dir: Dir,
    name: String,
}

fn id_valid(id: &str) -> bool {
    id.starts_with("b-")
        && (6..=96).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
}
fn serialize(value: &impl Serialize) -> Result<Vec<u8>, CapabilityError> {
    serde_json::to_vec(value).map_err(|error| failure(error.to_string()))
}
fn blob(bytes: &[u8], mode: u32) -> Blob {
    Blob {
        sha256: fs::hash(bytes),
        bytes: bytes.len(),
        mode,
    }
}
fn digest(manifest: &Manifest) -> Result<String, CapabilityError> {
    Ok(fs::hash(&serialize(manifest)?))
}
fn read_json<T: serde::de::DeserializeOwned>(
    dir: &Dir,
    name: &str,
    limit: usize,
    context: &Context,
) -> Result<T, CapabilityError> {
    serde_json::from_slice(&fs::read(dir, name, limit, context)?)
        .map_err(|error| failure(format!("invalid journal {name}: {error}")))
}
fn state_write(dir: &Dir, state: &State, context: &Context) -> Result<(), CapabilityError> {
    context.check()?;
    let name = "state.tmp";
    if fs::metadata(dir, name)?.is_some() {
        fs::regular(dir, name)?;
        dir.remove_file(name).map_err(io_error)?;
        fs::sync(dir)?;
    }
    let result = (|| {
        fs::write_exclusive(dir, name, &serialize(state)?, 0o600, context)?;
        context.check()?;
        dir.rename(name, dir, "state.json").map_err(io_error)?;
        fs::sync(dir)
    })();
    if result.is_err() && context.check().is_ok() {
        let _ = dir.remove_file(name);
    }
    result
}
fn capture(
    anchor: &Anchor,
    limit: usize,
    context: &Context,
) -> Result<(Option<Blob>, Option<Vec<u8>>), CapabilityError> {
    match fs::metadata(&anchor.dir, &anchor.name)? {
        None => Ok((None, None)),
        Some(meta) if meta.is_file() => {
            let file = fs::regular(&anchor.dir, &anchor.name)?;
            let actual = file.metadata().map_err(io_error)?;
            let mut file = file;
            let bytes = fs::read_bounded(&mut file, limit, context)?;
            let descriptor = blob(&bytes, fs::mode(&actual));
            Ok((Some(descriptor), Some(bytes)))
        }
        _ => Err(failure("batch paths must be regular files or absent")),
    }
}
fn anchor(root: &Dir, path: &str) -> Result<Anchor, CapabilityError> {
    let parts = fs::components(path, false)?;
    let (dir, name) = fs::parent(root, &parts)?;
    Ok(Anchor {
        dir,
        name: name.into(),
    })
}
fn expected(actual: &Option<Blob>, expected: &Expected) -> Result<(), CapabilityError> {
    match (actual, expected) {
        (None, Expected::Absent) => Ok(()),
        (Some(actual), Expected::Sha256 { sha256 }) => {
            fs::validate_hash(sha256)?;
            if actual.sha256.eq_ignore_ascii_case(sha256) {
                Ok(())
            } else {
                Err(failure("batch compare-and-swap hash conflict"))
            }
        }
        _ => Err(failure("batch compare-and-swap absence conflict")),
    }
}
fn check_paths(
    paths: impl IntoIterator<Item = String>,
    context: &Context,
) -> Result<(), CapabilityError> {
    context.check()?;
    let mut names = BTreeSet::new();
    for path in paths {
        context.check()?;
        fs::components(&path, false)?;
        if !names.insert(path) {
            return Err(failure("batch contains duplicate paths"));
        }
    }
    for path in &names {
        let mut prefix = String::new();
        for part in path
            .split('/')
            .take(path.split('/').count().saturating_sub(1))
        {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            if names.contains(&prefix) {
                return Err(failure("batch paths cannot be ancestors of one another"));
            }
        }
    }
    Ok(())
}

/// Probe only before a mutation, never while loading or reporting a journal.
/// Each private directory inherits the actual target parent's name rules, so
/// create_new detects aliases without assuming a case or Unicode mapping.
fn check_target_aliases(anchors: &[Anchor], context: &Context) -> Result<(), CapabilityError> {
    use cap_std::fs::MetadataExt;
    let mut groups = std::collections::BTreeMap::<_, Vec<&Anchor>>::new();
    for target in anchors {
        context.check()?;
        let metadata = target.dir.dir_metadata().map_err(io_error)?;
        groups
            .entry((metadata.dev(), metadata.ino()))
            .or_default()
            .push(target);
    }
    for group in groups.values().filter(|group| group.len() > 1) {
        let mut probe = AliasProbe::new(&group[0].dir, context)?;
        let result = (|| {
            for target in group {
                context.check()?;
                match probe.dir.open_with(&target.name, &crate::private_options()) {
                    Ok(file) => {
                        probe.names.push(target.name.clone());
                        drop(file);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        return Err(failure("batch contains alias paths on this volume"));
                    }
                    Err(error) => return Err(io_error(error)),
                }
            }
            Ok(())
        })();
        probe.close()?;
        result?;
    }
    Ok(())
}

struct AliasProbe<'a> {
    parent: Dir,
    dir: Dir,
    name: String,
    names: Vec<String>,
    context: &'a Context,
    closed: bool,
}
impl<'a> AliasProbe<'a> {
    fn new(parent: &Dir, context: &'a Context) -> Result<Self, CapabilityError> {
        context.check()?;
        let parent = parent.try_clone().map_err(io_error)?;
        let name = format!(".cayw-host-tmp-batch-alias-{}", fs::unique());
        let mut builder = cap_std::fs::DirBuilder::new();
        {
            use cap_std::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        parent.create_dir_with(&name, &builder).map_err(io_error)?;
        let dir = parent.open_dir_nofollow(&name).map_err(io_error)?;
        Ok(Self {
            parent,
            dir,
            name,
            names: vec![],
            context,
            closed: false,
        })
    }
    fn close(&mut self) -> Result<(), CapabilityError> {
        self.closed = true;
        for name in &self.names {
            self.context.check()?;
            self.dir.remove_file(name).map_err(io_error)?;
        }
        self.context.check()?;
        use cap_std::fs::MetadataExt;
        let original = self.dir.dir_metadata().map_err(io_error)?;
        let current = self
            .parent
            .open_dir_nofollow(&self.name)
            .map_err(io_error)?;
        let current = current.dir_metadata().map_err(io_error)?;
        if original.dev() != current.dev() || original.ino() != current.ino() {
            return Err(failure("batch alias probe moved during cleanup"));
        }
        self.parent.remove_dir(&self.name).map_err(io_error)?;
        fs::sync(&self.parent)
    }
}
impl Drop for AliasProbe<'_> {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.close();
        }
    }
}

impl Workspace {
    fn batch_ids(&self, context: &Context) -> Result<Vec<String>, CapabilityError> {
        let mut ids = vec![];
        let mut seen = 0;
        for entry in self.store.entries().map_err(io_error)? {
            context.check()?;
            seen += 1;
            if seen > self.limits.max_batches {
                return Err(failure(
                    "journal count limit reached; application must archive settled records",
                ));
            }
            let entry = entry.map_err(io_error)?;
            let id = entry
                .file_name()
                .into_string()
                .map_err(|_| failure("non-UTF8 journal name"))?;
            if !id_valid(&id) || !self.store.symlink_metadata(&id).map_err(io_error)?.is_dir() {
                return Err(failure("unexpected or non-directory journal entry"));
            }
            let dir = self.store.open_dir_nofollow(&id).map_err(io_error)?;
            // A crash before publication may leave an unpublished prepare directory.
            // It never touched user paths and is never inferred to be a submitted batch.
            if fs::metadata(&dir, "state.json")?.is_some() {
                ids.push(id);
            }
        }
        ids.sort();
        Ok(ids)
    }
    fn load_journal(&self, id: &str, context: &Context) -> Result<Journal, CapabilityError> {
        if !id_valid(id) {
            return Err(failure("invalid batch ID"));
        }
        let dir = self.store.open_dir_nofollow(id).map_err(io_error)?;
        let manifest: Manifest = read_json(
            &dir,
            "manifest.json",
            self.limits.max_argument_bytes,
            context,
        )?;
        if manifest.version != 1
            || manifest.batch_id != id
            || manifest.operations.is_empty()
            || manifest.operations.len() > self.limits.max_batch_operations
        {
            return Err(failure("invalid journal version, ID or operation count"));
        }
        check_paths(
            manifest.operations.iter().map(|op| op.path.clone()),
            context,
        )?;
        let mut bytes = 0usize;
        for op in &manifest.operations {
            if !matches!(op.operation.as_str(), "write" | "patch" | "remove")
                || (op.operation == "remove"
                    && (op.old.is_none() || op.new.is_some() || op.language.is_some()))
                || (op.operation != "remove"
                    && (op.new.is_none()
                        || op
                            .language
                            .as_ref()
                            .is_none_or(|language| language.is_empty() || language.len() > 64)))
                || (op.operation == "patch" && op.old.is_none())
            {
                return Err(failure("invalid journal operation"));
            }
            for descriptor in [&op.old, &op.new].into_iter().flatten() {
                fs::validate_hash(&descriptor.sha256)?;
                if descriptor.bytes > self.limits.max_file_bytes || descriptor.mode > 0o7777 {
                    return Err(failure("invalid journal blob bound or mode"));
                }
                bytes = bytes
                    .checked_add(descriptor.bytes)
                    .ok_or_else(|| failure("journal byte overflow"))?;
            }
        }
        if bytes > self.limits.max_batch_bytes {
            return Err(failure("journal batch byte limit exceeded"));
        }
        let digest = digest(&manifest)?;
        let state: State = read_json(&dir, "state.json", 4096, context)?;
        if state.version != 1
            || state.batch_digest != digest
            || state.installed > manifest.operations.len()
        {
            return Err(failure("journal state or digest mismatch"));
        }
        Ok(Journal {
            dir,
            manifest,
            digest,
            state,
        })
    }
    fn blob_read(
        &self,
        journal: &Journal,
        index: usize,
        new: bool,
        context: &Context,
    ) -> Result<Option<Vec<u8>>, CapabilityError> {
        let descriptor = if new {
            &journal.manifest.operations[index].new
        } else {
            &journal.manifest.operations[index].old
        };
        let Some(descriptor) = descriptor else {
            return Ok(None);
        };
        let name = format!("{}-{index}", if new { "candidate" } else { "backup" });
        let bytes = fs::read(&journal.dir, &name, self.limits.max_file_bytes, context)?;
        if bytes.len() != descriptor.bytes || fs::hash(&bytes) != descriptor.sha256 {
            return Err(failure("journal blob integrity mismatch"));
        }
        if new {
            self.limits.syntax_validator.validate(
                journal.manifest.operations[index]
                    .language
                    .as_deref()
                    .ok_or_else(|| failure("candidate has no language"))?,
                &bytes,
            )?;
            context.check()?;
        }
        Ok(Some(bytes))
    }
    fn verify_blobs(&self, journal: &Journal, context: &Context) -> Result<(), CapabilityError> {
        for index in 0..journal.manifest.operations.len() {
            self.blob_read(journal, index, false, context)?;
            self.blob_read(journal, index, true, context)?;
        }
        Ok(())
    }
    pub(crate) fn inspect_journals(&self, context: &Context) -> Result<(), CapabilityError> {
        for id in self.batch_ids(context)? {
            let journal = self.load_journal(&id, context)?;
            if journal.state.phase.pending() {
                self.verify_blobs(&journal, context)?;
                self.evidence(&journal, context)?;
            }
        }
        Ok(())
    }
    pub(crate) fn ensure_settled(&self, context: &Context) -> Result<(), CapabilityError> {
        for id in self.batch_ids(context)? {
            if self.load_journal(&id, context)?.state.phase.pending() {
                // The journal describes an earlier call's uncertainty. This
                // new operation has not begun, so refusing it is a known result.
                return Err(failure(format!("batch {id} requires explicit recovery")));
            }
        }
        Ok(())
    }
    pub(crate) fn prepare_batch(
        &self,
        args: &Value,
        context: &Context,
    ) -> Result<Value, CapabilityError> {
        self.ensure_settled(context)?;
        let args: PrepareArgs = parse(args)?;
        if args.operations.is_empty() || args.operations.len() > self.limits.max_batch_operations {
            return Err(failure("batch operation count limit exceeded"));
        }
        let paths: Vec<_> = args
            .operations
            .iter()
            .map(|op| match op {
                Input::Write { path, .. }
                | Input::Patch { path, .. }
                | Input::Remove { path, .. } => path.clone(),
            })
            .collect();
        check_paths(paths.iter().cloned(), context)?;
        let targets = paths
            .iter()
            .map(|path| anchor(&self.root, path))
            .collect::<Result<Vec<_>, _>>()?;
        check_target_aliases(&targets, context)?;
        // Count unpublished directories as well, so repeated interrupted prepares
        // cannot grow disk storage without bound.
        let mut count = 0usize;
        for entry in self.store.entries().map_err(io_error)? {
            context.check()?;
            entry.map_err(io_error)?;
            count += 1;
            if count >= self.limits.max_batches {
                return Err(failure("journal count limit reached"));
            }
        }
        let mut operations = vec![];
        let mut contents = vec![];
        let mut total = 0usize;
        for (input, target) in args.operations.into_iter().zip(targets) {
            context.check()?;
            let path = match &input {
                Input::Write { path, .. }
                | Input::Patch { path, .. }
                | Input::Remove { path, .. } => path.clone(),
            };
            let (old, before) = capture(&target, self.limits.max_file_bytes, context)?;
            let (operation, candidate, language) = match input {
                Input::Write {
                    expected: condition,
                    content,
                    encoding,
                    language,
                    ..
                } => {
                    expected(&old, &condition)?;
                    let bytes = match encoding {
                        Encoding::Utf8 => content.into_bytes(),
                        Encoding::Base64 => STANDARD
                            .decode(content)
                            .map_err(|_| failure("invalid base64 candidate"))?,
                    };
                    ("write", Some(bytes), Some(language))
                }
                Input::Patch {
                    expected_sha256,
                    old: find,
                    new,
                    language,
                    ..
                } => {
                    expected(
                        &old,
                        &Expected::Sha256 {
                            sha256: expected_sha256,
                        },
                    )?;
                    if find.is_empty() {
                        return Err(failure("patch old text must be nonempty"));
                    }
                    let text = std::str::from_utf8(
                        before
                            .as_ref()
                            .ok_or_else(|| failure("patch target absent"))?,
                    )
                    .map_err(|_| failure("patch target is not UTF-8"))?;
                    let at = text
                        .find(&find)
                        .ok_or_else(|| failure("patch text not found"))?;
                    let next = at
                        + text[at..]
                            .chars()
                            .next()
                            .expect("nonempty match")
                            .len_utf8();
                    if text[next..].contains(&find) {
                        return Err(failure("patch text must match exactly once"));
                    }
                    let size = text
                        .len()
                        .checked_sub(find.len())
                        .and_then(|size| size.checked_add(new.len()))
                        .ok_or_else(|| failure("patch size overflow"))?;
                    if size > self.limits.max_file_bytes {
                        return Err(failure("patched candidate byte limit exceeded"));
                    }
                    let mut bytes = Vec::with_capacity(size);
                    bytes.extend_from_slice(&text.as_bytes()[..at]);
                    bytes.extend_from_slice(new.as_bytes());
                    bytes.extend_from_slice(&text.as_bytes()[at + find.len()..]);
                    ("patch", Some(bytes), Some(language))
                }
                Input::Remove {
                    expected_sha256, ..
                } => {
                    expected(
                        &old,
                        &Expected::Sha256 {
                            sha256: expected_sha256,
                        },
                    )?;
                    ("remove", None, None)
                }
            };
            if let Some(bytes) = &candidate {
                if bytes.len() > self.limits.max_file_bytes {
                    return Err(failure("candidate file byte limit exceeded"));
                }
                let language = language
                    .as_deref()
                    .ok_or_else(|| failure("candidate language required"))?;
                if language.is_empty() || language.len() > 64 {
                    return Err(failure("invalid language"));
                }
                self.limits.syntax_validator.validate(language, bytes)?;
                context.check()?;
            }
            total = total
                .checked_add(before.as_ref().map_or(0, Vec::len))
                .and_then(|size| size.checked_add(candidate.as_ref().map_or(0, Vec::len)))
                .ok_or_else(|| failure("batch size overflow"))?;
            if total > self.limits.max_batch_bytes {
                return Err(failure("batch backup and candidate byte limit exceeded"));
            }
            let new = candidate
                .as_ref()
                .map(|bytes| blob(bytes, old.as_ref().map_or(0o600, |old| old.mode)));
            operations.push(Operation {
                path,
                operation: operation.into(),
                old,
                new,
                language,
            });
            contents.push((before, candidate));
        }
        let batch_id = format!("b-{}", fs::unique());
        let manifest = Manifest {
            version: 1,
            batch_id: batch_id.clone(),
            operations,
        };
        if serialize(&manifest)?.len() > self.limits.max_argument_bytes {
            return Err(failure("journal manifest byte limit exceeded"));
        }
        let digest = digest(&manifest)?;
        let detail: Vec<_> = manifest.operations.iter().zip(&contents).map(|(operation, (_, candidate))| json!({
            "path":operation.path,"operation":operation.operation,"expected":operation.old,"candidate":operation.new,"language":operation.language,
            "candidate_content":candidate.as_ref().map(|bytes| json!({"encoding":"base64","content":STANDARD.encode(bytes)}))
        })).collect();
        let result = json!({"batch_id":batch_id,"batch_digest":digest,"status":"prepared","operations":detail,"user_files_changed":false});
        crate::json_size(&result, self.limits.max_output_bytes)?;
        context.check()?;
        self.store.create_dir(&batch_id).map_err(io_error)?;
        fs::sync(&self.store)?;
        let dir = self.store.open_dir_nofollow(&batch_id).map_err(io_error)?;
        #[cfg(unix)]
        {
            use cap_std::fs::PermissionsExt;
            dir.set_permissions(".", cap_std::fs::Permissions::from_mode(0o700))
                .map_err(io_error)?;
        }
        for (index, (before, candidate)) in contents.into_iter().enumerate() {
            if let Some(bytes) = before {
                fs::write_exclusive(&dir, &format!("backup-{index}"), &bytes, 0o600, context)?;
            }
            if let Some(bytes) = candidate {
                fs::write_exclusive(&dir, &format!("candidate-{index}"), &bytes, 0o600, context)?;
            }
        }
        fs::write_exclusive(
            &dir,
            "manifest.json",
            &serialize(&manifest)?,
            0o600,
            context,
        )?;
        state_write(
            &dir,
            &State {
                version: 1,
                batch_digest: digest,
                phase: Phase::Prepared,
                installed: 0,
            },
            context,
        )?;
        Ok(result)
    }
    fn evidence(
        &self,
        journal: &Journal,
        context: &Context,
    ) -> Result<Vec<Value>, CapabilityError> {
        let mut evidence = vec![];
        for operation in &journal.manifest.operations {
            context.check()?;
            let current = anchor(&self.root, &operation.path)
                .and_then(|target| capture(&target, self.limits.max_file_bytes, context));
            let (actual, diagnostic, matches) = match current {
                Ok((current, _)) => {
                    let old = current == operation.old;
                    let new = current == operation.new;
                    (
                        current,
                        None,
                        match (old, new) {
                            (true, true) => "both",
                            (true, false) => "old",
                            (false, true) => "new",
                            _ => "third_party",
                        },
                    )
                }
                Err(error) => {
                    context.check()?;
                    (None, Some(error.message), "third_party")
                }
            };
            evidence.push(json!({"path":operation.path,"old":operation.old,"new":operation.new,"current":actual,"matches":matches,"diagnostic":diagnostic}));
        }
        Ok(evidence)
    }
    fn report(&self, journal: &Journal, context: &Context) -> Result<Value, CapabilityError> {
        Ok(
            json!({"batch_id":journal.manifest.batch_id,"batch_digest":journal.digest,"status":journal.state.phase,"outcome_unknown":journal.state.phase.pending(),"recorded_installed":journal.state.installed,"paths":self.evidence(journal, context)?}),
        )
    }
    pub(crate) fn batch_status(
        &self,
        args: &Value,
        context: &Context,
    ) -> Result<Value, CapabilityError> {
        let args: StatusArgs = parse(args)?;
        if let Some(id) = args.batch_id {
            return self.report(&self.load_journal(&id, context)?, context);
        }
        let mut reports = vec![];
        for id in self.batch_ids(context)? {
            reports.push(self.report(&self.load_journal(&id, context)?, context)?);
            crate::json_size(&json!({"batches":&reports}), self.limits.max_output_bytes)?;
        }
        Ok(json!({"batches":reports}))
    }
    fn require_old(
        &self,
        operation: &Operation,
        target: &Anchor,
        context: &Context,
    ) -> Result<(), CapabilityError> {
        let (actual, _) = capture(target, self.limits.max_file_bytes, context)?;
        if actual != operation.old {
            return Err(failure(format!(
                "batch precondition conflict: {}",
                operation.path
            )));
        }
        Ok(())
    }
    fn require_final(
        &self,
        journal: &Journal,
        commit: bool,
        context: &Context,
    ) -> Result<(), CapabilityError> {
        for operation in &journal.manifest.operations {
            let target = anchor(&self.root, &operation.path)?;
            let (actual, _) = capture(&target, self.limits.max_file_bytes, context)?;
            let desired = if commit {
                &operation.new
            } else {
                &operation.old
            };
            if &actual != desired {
                return Err(failure(format!(
                    "final path verification failed: {}",
                    operation.path
                )));
            }
        }
        Ok(())
    }

    fn install(
        target: &Anchor,
        current: &Option<Blob>,
        desired: &Option<Blob>,
        bytes: Option<&[u8]>,
        context: &Context,
        changed: &mut bool,
        staging: &str,
    ) -> Result<(), CapabilityError> {
        let file_limit = current
            .as_ref()
            .map_or(0, |blob| blob.bytes)
            .max(desired.as_ref().map_or(0, |blob| blob.bytes));
        // One deterministic, operation-specific staging name bounds crash
        // leftovers. Never remove an unexpected file occupying that name.
        let staged = capture(
            &Anchor {
                dir: target.dir.try_clone().map_err(io_error)?,
                name: staging.into(),
            },
            file_limit,
            context,
        )?
        .0;
        if let Some(staged) = staged {
            let staged_bytes = fs::read(&target.dir, staging, file_limit, context)?;
            if bytes.is_none_or(|bytes| !bytes.starts_with(&staged_bytes))
                || desired
                    .as_ref()
                    .is_none_or(|desired| staged.mode != 0o600 && desired.mode != staged.mode)
            {
                return Err(failure("unexpected staging file blocks installation"));
            }
            context.check()?;
            target.dir.remove_file(staging).map_err(io_error)?;
            fs::sync(&target.dir)?;
        }
        if current == desired {
            return Ok(());
        }
        let result = (|| {
            if let Some(desired) = desired {
                let bytes = bytes.ok_or_else(|| failure("missing install candidate"))?;
                fs::write_exclusive(&target.dir, staging, bytes, desired.mode, context)?;
            }
            let (actual, _) = capture(target, file_limit, context)?;
            if &actual != current {
                return Err(failure("path changed before installation"));
            }
            context.check()?;
            if desired.is_none() {
                target.dir.remove_file(&target.name).map_err(io_error)?;
                *changed = true;
            } else if current.is_none() {
                target
                    .dir
                    .hard_link(staging, &target.dir, &target.name)
                    .map_err(io_error)?;
                *changed = true;
                target.dir.remove_file(staging).map_err(io_error)?;
            } else {
                target
                    .dir
                    .rename(staging, &target.dir, &target.name)
                    .map_err(io_error)?;
                *changed = true;
            }
            fs::sync(&target.dir)
        })();
        if context.check().is_ok() {
            let _ = target.dir.remove_file(staging);
        }
        result
    }
    pub(crate) fn submit_batch(
        &self,
        args: &Value,
        context: &Context,
    ) -> Result<Value, CapabilityError> {
        let args: SubmitArgs = parse(args)?;
        fs::validate_hash(&args.expected_batch_digest)?;
        self.ensure_settled(context)?;
        let mut journal = self.load_journal(&args.batch_id, context)?;
        if journal.digest != args.expected_batch_digest {
            return Err(failure("batch digest conflict"));
        }
        if journal.state.phase != Phase::Prepared {
            return Err(failure("only prepared batches can be submitted"));
        }
        self.verify_blobs(&journal, context)?;
        let mut anchors = vec![];
        for operation in &journal.manifest.operations {
            let target = anchor(&self.root, &operation.path)?;
            self.require_old(operation, &target, context)?;
            anchors.push(target);
        }
        check_target_aliases(&anchors, context)?;
        context.check()?;
        journal.state.phase = Phase::Intent;
        if let Err(error) = state_write(&journal.dir, &journal.state, context) {
            journal.state.phase = Phase::Aborted;
            let _ = state_write(&journal.dir, &journal.state, context);
            return Err(error);
        }
        let mut changed = false;
        let result = (|| {
            self.fault("intent", 0, context)?;
            for (index, target) in anchors.iter().enumerate() {
                context.check()?;
                self.fault("before_install", index, context)?;
                let operation = &journal.manifest.operations[index];
                self.require_old(operation, target, context)?;
                let bytes = self.blob_read(&journal, index, true, context)?;
                journal.state.phase = Phase::Committing;
                state_write(&journal.dir, &journal.state, context)?;
                Self::install(
                    target,
                    &operation.old,
                    &operation.new,
                    bytes.as_deref(),
                    context,
                    &mut changed,
                    &format!(
                        ".cayw-workspace-{}-{index}-commit",
                        journal.manifest.batch_id
                    ),
                )?;
                self.fault("after_install", index, context)?;
                journal.state.installed = index + 1;
                state_write(&journal.dir, &journal.state, context)?;
            }
            self.require_final(&journal, true, context)?;
            self.fault("commit_record", 0, context)?;
            journal.state.phase = Phase::Committed;
            state_write(&journal.dir, &journal.state, context)?;
            self.report(&journal, context)
        })();
        match result {
            Ok(report) => Ok(report),
            Err(error) => {
                journal.state.phase = if changed {
                    Phase::OutcomeUnknown
                } else {
                    Phase::Aborted
                };
                // Never restart the caller's allowance for cleanup. If expired,
                // the last durable intent/committing record retains the fence.
                let persistence = state_write(&journal.dir, &journal.state, context);
                let message = format!(
                    "batch {}: {}; journal settlement: {}",
                    journal.manifest.batch_id,
                    error.message,
                    persistence
                        .as_ref()
                        .err()
                        .map_or("durable", |error| error.message.as_str())
                );
                Err(if changed {
                    CapabilityError::unknown(message)
                } else {
                    failure(message)
                })
            }
        }
    }
    pub(crate) fn recover_batch(
        &self,
        args: &Value,
        context: &Context,
    ) -> Result<Value, CapabilityError> {
        let args: RecoverArgs = parse(args)?;
        fs::validate_hash(&args.expected_batch_digest)?;
        let mut journal = self.load_journal(&args.batch_id, context)?;
        if journal.digest != args.expected_batch_digest {
            return Err(failure("batch digest conflict"));
        }
        if !journal.state.phase.pending() {
            return Err(failure("only interrupted batches can be recovered"));
        }
        self.verify_blobs(&journal, context).map_err(unknown)?;
        // Check every path and every blob before the first recovery write. Old/new
        // states include mode. Any third-party content blocks the whole recovery.
        let mut anchors = vec![];
        let mut actual = vec![];
        for operation in &journal.manifest.operations {
            let target = anchor(&self.root, &operation.path).map_err(unknown)?;
            let (current, _) =
                capture(&target, self.limits.max_file_bytes, context).map_err(unknown)?;
            if current != operation.old && current != operation.new {
                return Err(CapabilityError::unknown(format!(
                    "third-party change blocks recovery: {}",
                    operation.path
                )));
            }
            anchors.push(target);
            actual.push(current);
        }
        check_target_aliases(&anchors, context).map_err(unknown)?;
        let commit = matches!(args.action, Recovery::Commit);
        journal.state.phase = if commit {
            Phase::RecoveringCommit
        } else {
            Phase::RecoveringRollback
        };
        state_write(&journal.dir, &journal.state, context).map_err(unknown)?;
        let result = (|| {
            for (index, target) in anchors.iter().enumerate() {
                let bytes = self.blob_read(&journal, index, commit, context)?;
                let desired = if commit {
                    &journal.manifest.operations[index].new
                } else {
                    &journal.manifest.operations[index].old
                };
                let mut changed = false;
                Self::install(
                    target,
                    &actual[index],
                    desired,
                    bytes.as_deref(),
                    context,
                    &mut changed,
                    &format!(
                        ".cayw-workspace-{}-{index}-{}",
                        journal.manifest.batch_id,
                        if commit { "commit" } else { "rollback" }
                    ),
                )?;
                self.fault("recovery", index, context)?;
                journal.state.installed = index + 1;
                state_write(&journal.dir, &journal.state, context)?;
            }
            self.require_final(&journal, commit, context)?;
            journal.state.phase = if commit {
                Phase::Committed
            } else {
                Phase::RolledBack
            };
            state_write(&journal.dir, &journal.state, context)?;
            self.report(&journal, context)
        })();
        result.map_err(unknown)
    }
}
