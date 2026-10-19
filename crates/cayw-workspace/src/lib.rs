//! Bounded workspace executors. A host must authorize, audit and budget every call.
//! This crate deliberately does not implement `CapabilityHost`.

mod archive;
mod batch;
mod catalog;
mod code;
mod filesystem;
mod search;

pub use cayw_ai::{CancellationToken, ToolDefinition};
pub use cayw_codemode::CapabilityError;

use cap_std::fs::{Dir, OpenOptions};
use serde_json::Value;
use std::{
    io::{self, Write},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

/// A trusted, synchronous, bounded syntax checker selected by the application.
/// Implementations must reject unsupported languages and must not run untrusted code.
/// Successful validation proves parsing only, not compilation or program safety.
pub trait SyntaxValidator: Send + Sync {
    fn validate(&self, language: &str, candidate: &[u8]) -> Result<(), CapabilityError>;
}

/// Parses JSON and TOML; rejects every other language, including "text".
#[derive(Default)]
pub struct JsonTomlValidator;
impl SyntaxValidator for JsonTomlValidator {
    fn validate(&self, language: &str, candidate: &[u8]) -> Result<(), CapabilityError> {
        let text =
            std::str::from_utf8(candidate).map_err(|_| failure("syntax input must be UTF-8"))?;
        match language {
            "json" => {
                serde_json::from_str::<Value>(text)
                    .map_err(|error| failure(format!("JSON syntax: {error}")))?;
            }
            "toml" => {
                toml::from_str::<toml::Value>(text)
                    .map_err(|error| failure(format!("TOML syntax: {error}")))?;
            }
            _ => return Err(failure("no trusted syntax validator for this language")),
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct Limits {
    pub max_file_bytes: usize,
    pub max_batch_bytes: usize,
    pub max_batch_operations: usize,
    /// Completed journals are retained as evidence; this is also a storage bound.
    pub max_batches: usize,
    pub max_tree_entries: usize,
    pub max_depth: usize,
    pub max_argument_bytes: usize,
    pub max_output_bytes: usize,
    pub max_archive_bytes: usize,
    pub max_archive_entries: usize,
    pub max_archive_expanded_bytes: usize,
    pub max_compression_ratio: u64,
    pub timeout: Duration,
    pub syntax_validator: Arc<dyn SyntaxValidator>,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: 1024 * 1024,
            max_batch_bytes: 16 * 1024 * 1024,
            max_batch_operations: 64,
            max_batches: 128,
            max_tree_entries: 4096,
            max_depth: 32,
            max_argument_bytes: 8 * 1024 * 1024,
            max_output_bytes: 8 * 1024 * 1024,
            max_archive_bytes: 16 * 1024 * 1024,
            max_archive_entries: 4096,
            max_archive_expanded_bytes: 16 * 1024 * 1024,
            max_compression_ratio: 100,
            timeout: Duration::from_secs(30),
            syntax_validator: Arc::new(JsonTomlValidator),
        }
    }
}
impl Limits {
    /// Checks configuration without opening or modifying the workspace.
    pub fn validate(&self) -> Result<(), CapabilityError> {
        for (value, ceiling) in [
            (self.max_file_bytes, 16 * 1024 * 1024),
            (self.max_batch_bytes, 64 * 1024 * 1024),
            (self.max_batch_operations, 256),
            (self.max_batches, 1024),
            (self.max_tree_entries, 100_000),
            (self.max_depth, 128),
            (self.max_argument_bytes, 64 * 1024 * 1024),
            (self.max_output_bytes, 64 * 1024 * 1024),
            (self.max_archive_bytes, 64 * 1024 * 1024),
            (self.max_archive_entries, 100_000),
            (self.max_archive_expanded_bytes, 64 * 1024 * 1024),
        ] {
            if value == 0 || value > ceiling {
                return Err(failure(
                    "workspace limits must be positive and within hard ceilings",
                ));
            }
        }
        if self.max_output_bytes < 2
            || self.max_compression_ratio == 0
            || self.max_compression_ratio > 10_000
            || self.timeout.is_zero()
            || self.timeout > Duration::from_secs(300)
            || Instant::now().checked_add(self.timeout).is_none()
        {
            return Err(failure("invalid workspace output limit, ratio or timeout"));
        }
        Ok(())
    }
}

pub struct Workspace {
    root: Dir,
    store: Dir,
    limits: Limits,
    #[cfg(feature = "fault-injection")]
    fault: std::sync::Mutex<Option<(FaultPoint, FaultAction)>>,
}

/// Development-only failure windows; unavailable in ordinary builds.
#[cfg(feature = "fault-injection")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FaultPoint {
    AfterIntent,
    BeforeInstall(usize),
    AfterInstall(usize),
    BeforeCommitRecord,
    AfterRecoveryInstall(usize),
}
#[cfg(feature = "fault-injection")]
#[derive(Debug, Clone, Copy)]
pub enum FaultAction {
    Error,
    /// Exits immediately with status 86, without Rust destructors.
    Crash,
    /// Cancels the executing call at this exact window, then returns an error.
    Cancel,
}

impl Workspace {
    /// Opens a trusted application-selected root through no-follow directory
    /// handles. Unix is supported; other platforms currently fail closed.
    /// Reopening checks journals and pending path evidence without replaying work.
    pub fn open(root: impl AsRef<Path>, limits: Limits) -> Result<Self, CapabilityError> {
        limits.validate()?;
        let context = Context::new(CancellationToken::new(), limits.timeout)?;
        context.check()?;
        Self::open_dir_inner(filesystem::open_root(root.as_ref())?, limits, &context)
    }

    /// Uses an application-selected directory handle without resolving its path
    /// again. The root stays the same directory if its name is moved or replaced.
    pub fn open_dir(root: Dir, limits: Limits) -> Result<Self, CapabilityError> {
        let context = Context::new(CancellationToken::new(), limits.timeout)?;
        Self::open_dir_inner(root, limits, &context)
    }

    /// Host entry point: opening and journal validation share the caller's
    /// cancellation and absolute deadline. Pass the same deadline to execute_until.
    pub fn open_dir_until(
        root: Dir,
        limits: Limits,
        cancel: CancellationToken,
        deadline: Instant,
    ) -> Result<Self, CapabilityError> {
        let context = Context::until(cancel, deadline, limits.timeout)?;
        Self::open_dir_inner(root, limits, &context)
    }

    fn open_dir_inner(
        root: Dir,
        limits: Limits,
        context: &Context,
    ) -> Result<Self, CapabilityError> {
        limits.validate()?;
        context.check()?;
        #[cfg(not(unix))]
        {
            let _ = root;
            Err(failure(
                "workspace directory durability is not implemented on this platform",
            ))
        }
        #[cfg(unix)]
        {
            let lock = filesystem::lock_file(&root)?;
            context.check()?;
            lock.try_lock()
                .map_err(|error| failure(format!("workspace is busy: {error}")))?;
            context.check()?;
            let store = filesystem::open_store(&root, context)?;
            let workspace = Self {
                root,
                store,
                limits,
                #[cfg(feature = "fault-injection")]
                fault: std::sync::Mutex::new(None),
            };
            workspace.inspect_journals(context)?;
            context.check()?;
            Ok(workspace)
        }
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        definitions()
    }

    /// Dispatch only after the host has applied its schema, permission, durable
    /// audit, cancellation and cumulative budgets. Local validation is repeated
    /// defensively. No work is detached when this future is dropped.
    pub async fn execute(
        &self,
        name: &str,
        arguments: &Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        let deadline = Instant::now()
            .checked_add(self.limits.timeout)
            .ok_or_else(|| failure("invalid deadline"))?;
        self.execute_until(name, arguments, cancel, deadline).await
    }

    /// Executes within the same absolute allowance used by open_dir_until.
    /// Synchronous parsers and kernel calls cannot be interrupted, but the next
    /// checkpoint refuses further effects after cancellation or expiry.
    pub async fn execute_until(
        &self,
        name: &str,
        arguments: &Value,
        cancel: CancellationToken,
        deadline: Instant,
    ) -> Result<Value, CapabilityError> {
        let context = Context::until(cancel, deadline, self.limits.timeout)?;
        self.execute_inner(name, arguments, &context)
            .await
            .map_err(|mut error| {
                error.message = fit_error(error.message, self.limits.max_output_bytes);
                error
            })
    }

    async fn execute_inner(
        &self,
        name: &str,
        arguments: &Value,
        context: &Context,
    ) -> Result<Value, CapabilityError> {
        context.check()?;
        check_argument_shape(arguments, 0, &mut 0, self.limits.max_argument_bytes)?;
        json_size(arguments, self.limits.max_argument_bytes)?;
        let result = match name {
            "fs.tree" => filesystem::tree(self, arguments, context),
            "fs.glob" | "fs.grep" => search::execute(self, name, arguments, context),
            "code.outline" | "code.diff_hunks" => code::execute(self, name, arguments, context),
            "archive.list" | "archive.read" => archive::execute(self, name, arguments, context),
            "fs.batch_status" => self.batch_status(arguments, context),
            "fs.mkdir"
            | "fs.move"
            | "fs.remove"
            | "fs.prepare_change_batch"
            | "fs.submit_change_batch"
            | "fs.recover_change_batch" => {
                let _lock = filesystem::write_lock(&self.root, context).await?;
                match name {
                    "fs.prepare_change_batch" => self.prepare_batch(arguments, context),
                    "fs.submit_change_batch" => self.submit_batch(arguments, context),
                    "fs.recover_change_batch" => self.recover_batch(arguments, context),
                    _ => {
                        self.ensure_settled(context)?;
                        filesystem::mutate(self, name, arguments, context)
                    }
                }
            }
            _ => Err(failure("unknown workspace capability")),
        }?;
        json_size(&result, self.limits.max_output_bytes).map_err(|error| {
            if matches!(
                name,
                "fs.mkdir"
                    | "fs.move"
                    | "fs.remove"
                    | "fs.submit_change_batch"
                    | "fs.recover_change_batch"
            ) {
                CapabilityError::unknown(format!(
                    "operation finished but result exceeded budget: {}",
                    error.message
                ))
            } else {
                error
            }
        })?;
        Ok(result)
    }

    #[cfg(feature = "fault-injection")]
    pub fn inject_fault(&self, point: FaultPoint, action: FaultAction) {
        *self.fault.lock().unwrap_or_else(|error| error.into_inner()) = Some((point, action));
    }

    pub(crate) fn fault(
        &self,
        stage: &str,
        index: usize,
        context: &Context,
    ) -> Result<(), CapabilityError> {
        #[cfg(feature = "fault-injection")]
        {
            let point = match stage {
                "intent" => FaultPoint::AfterIntent,
                "before_install" => FaultPoint::BeforeInstall(index),
                "after_install" => FaultPoint::AfterInstall(index),
                "commit_record" => FaultPoint::BeforeCommitRecord,
                "recovery" => FaultPoint::AfterRecoveryInstall(index),
                _ => return Err(failure("invalid internal fault point")),
            };
            let mut fault = self.fault.lock().unwrap_or_else(|error| error.into_inner());
            if fault
                .as_ref()
                .is_some_and(|(configured, _)| *configured == point)
            {
                let (_, action) = fault.take().expect("checked fault");
                match action {
                    FaultAction::Error => {
                        return Err(failure(format!("injected fault: {point:?}")));
                    }
                    FaultAction::Crash => std::process::exit(86),
                    FaultAction::Cancel => {
                        context.cancel.cancel();
                        return context.check();
                    }
                }
            }
        }
        let _ = (stage, index, context);
        Ok(())
    }
}

pub fn definitions() -> Vec<ToolDefinition> {
    catalog::definitions()
}

pub(crate) struct Context {
    cancel: CancellationToken,
    deadline: Instant,
}
impl Context {
    fn new(cancel: CancellationToken, timeout: Duration) -> Result<Self, CapabilityError> {
        Ok(Self {
            cancel,
            deadline: Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| failure("invalid deadline"))?,
        })
    }
    fn until(
        cancel: CancellationToken,
        deadline: Instant,
        timeout: Duration,
    ) -> Result<Self, CapabilityError> {
        let local = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| failure("invalid deadline"))?;
        Ok(Self {
            cancel,
            deadline: deadline.min(local),
        })
    }
    pub(crate) fn check(&self) -> Result<(), CapabilityError> {
        if self.cancel.is_cancelled() {
            return Err(failure("workspace operation cancelled"));
        }
        if Instant::now() >= self.deadline {
            return Err(failure("workspace operation deadline reached"));
        }
        Ok(())
    }
}

fn fit_error(mut message: String, limit: usize) -> String {
    let mut used = 2usize;
    let mut end = 0;
    for (at, character) in message.char_indices() {
        let bytes = match character {
            '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
            '\u{0}'..='\u{1f}' => 6,
            _ => character.len_utf8(),
        };
        if bytes > limit.saturating_sub(used) {
            break;
        }
        used += bytes;
        end = at + character.len_utf8();
    }
    message.truncate(end);
    message
}

pub(crate) fn failure(message: impl Into<String>) -> CapabilityError {
    CapabilityError::new(message)
}
pub(crate) fn io_error(error: io::Error) -> CapabilityError {
    failure(error.to_string())
}
pub(crate) fn unknown(error: CapabilityError) -> CapabilityError {
    CapabilityError::unknown(error.message)
}
fn check_argument_shape(
    value: &Value,
    depth: usize,
    used: &mut usize,
    limit: usize,
) -> Result<(), CapabilityError> {
    if depth > 128 {
        return Err(failure("argument nesting limit exceeded"));
    }
    *used = used.saturating_add(1);
    if *used > limit {
        return Err(failure("argument byte limit exceeded"));
    }
    match value {
        Value::Null => {
            return Err(failure(
                "null arguments are unsupported; omit optional fields",
            ));
        }
        Value::Array(values) => {
            for value in values {
                check_argument_shape(value, depth + 1, used, limit)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                *used = used.saturating_add(key.len());
                check_argument_shape(value, depth + 1, used, limit)?;
            }
        }
        Value::String(text) => {
            *used = used.saturating_add(text.len());
            if *used > limit {
                return Err(failure("argument byte limit exceeded"));
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn parse<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, CapabilityError> {
    serde_json::from_value(value.clone())
        .map_err(|error| failure(format!("invalid arguments: {error}")))
}
pub(crate) fn json_size(value: &Value, limit: usize) -> Result<usize, CapabilityError> {
    struct Counter {
        used: usize,
        limit: usize,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.used) {
                return Err(io::Error::other("JSON byte budget exceeded"));
            }
            self.used += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { used: 0, limit };
    serde_json::to_writer(&mut counter, value).map_err(|error| failure(error.to_string()))?;
    Ok(counter.used)
}

/// Create private, exclusive journal files. All callers supply fixed internal names.
pub(crate) fn private_options() -> OpenOptions {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
