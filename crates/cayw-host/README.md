# cayw-host

`cayw-host` supplies bounded workspace capabilities to `cayw-codemode`. The application supplies permission decisions, durable audit storage, optional process profiles, and its existing external services. It does not create MCP connections, load skills, expose a shell, or contact a model.

## API

```rust,ignore
Host::new(
    root: impl AsRef<std::path::Path>,
    gate: std::sync::Arc<dyn PermissionGate>,
    audit: std::sync::Arc<dyn AuditSink>,
    config: HostConfig,
) -> Result<Host, cayw_codemode::CapabilityError>
```

`Host` implements `cayw_codemode::CapabilityHost`. Its catalog contains only configured capabilities, and each direct call validates its own schema. Calling a Rust method directly does not skip validation, permission, or audit.

```rust,ignore
pub fn Host::scoped(
    &self,
    gate: std::sync::Arc<dyn PermissionGate>,
    audit: std::sync::Arc<dyn AuditSink>,
) -> Result<Host, cayw_codemode::CapabilityError>
```

A scope owns its immutable gate and audit sink. It shares the original open directory, configuration, catalog, cumulative call/byte budgets, execution lock, permanent unknown/audit-failure state and the same Device process runner. Creating a scope opens no path, resets no state and grants no capability. Ordinary calls remain ordered through durable settlement. Explicitly admitted frozen reads can wait concurrently, retaining separate original audits and budgets. An unknown result in any scope stops the whole group. Scope creation is allowed on a stopped Host, but its calls remain stopped. `task_host` creates a separate task's admission and process state over that same held directory; durable root fences still apply across tasks and independent Host connections.

Implement these application-owned interfaces with `async_trait`:

```rust,ignore
trait PermissionGate {
    async fn authorize(&self, capability: &str, arguments: &serde_json::Value,
        cancel: cayw_ai::CancellationToken)
        -> Result<(), cayw_codemode::CapabilityError>;
}
trait AuditSink {
    async fn started(&self, capability: &str, arguments: &serde_json::Value)
        -> Result<String, cayw_codemode::CapabilityError>;
    async fn settled(&self, call_id: &str,
        result: &Result<serde_json::Value, cayw_codemode::CapabilityError>)
        -> Result<(), cayw_codemode::CapabilityError>;
    fn dropped(&self, call_id: &str) -> Result<(), cayw_codemode::CapabilityError>;
}
trait ExternalHost {
    async fn call(&self, capability: &str, arguments: serde_json::Value,
        cancel: cayw_ai::CancellationToken)
        -> Result<serde_json::Value, cayw_codemode::CapabilityError>;
}
```

All three traits require `Send + Sync`. `started` returns an application-generated unique audit ID only after a durable commit. `settled` persists the full confirmed result or unknown status. `dropped` synchronously persists unknown status when execution or settlement is interrupted. A crash can leave a durable running record; the application must treat it as unknown during recovery. A partial audit write must return an unknown error.

Ordinary calls wait on a cancellable execution queue, then use the common admission path: schema and argument limits, cumulative budget reservation, permission, durable start, execution, result limits and durable settlement. Frozen read tickets use that same admission path without the ordinary execution lane, so a writer waiting on read leases cannot block their settlement or acknowledgment. Cancellation and the original deadline are checked before permission and execution and during fence waits. Permissions receive the exact validated arguments without default insertion or coercion. No advertised capability is automatically authorized.

A failed start prevents execution. A failed settlement or an interrupted call permanently marks that host instance as unknown. The same is true when permission or an external operation returns an unknown outcome. `Host::outcome_unknown()` and `Host::audit_failure()` expose this state, including a synchronous drop callback failure or panic. Creating a new host is not reconciliation: the application must first resolve durable running/unknown records. There is deliberately no reset method.

## Capabilities

| Capability | Arguments | Result |
| --- | --- | --- |
| `fs.read` | `path` | UTF-8 `content`, `bytes`, `sha256`, `path` |
| `fs.list` | `path` (`.` for root) | Sorted direct `entries` with `name`, `kind`, `size` |
| `fs.stat` | `path` (`.` for root) | `kind`, `size`, `readonly`, `path`; does not follow the final link |
| `fs.write` | `path`, UTF-8 `content`; replacement requires `overwrite: true`, `expected_sha256` | Committed `bytes`, `sha256` |
| `fs.patch` | `path`, `expected_sha256`, nonempty `old`, `new` | Committed `bytes`, `sha256`; exact old text must occur once |
| `git.status` | `{}` | Process result with porcelain status in `stdout` |
| `git.diff` | Optional `path`, `staged` | Process result with diff in `stdout` |
| `git.log` | Optional `limit` (1–100, default 20) | Process result with full commit IDs and subjects in `stdout` |
| `git.show` | Full 40/64 hex `commit`, optional `path` | Commit or blob in `stdout` |
| `build.run` | Application `profile` name | Process result |
| `test.run` | Application `profile` name, optional restricted `filter` | Process result |
| `check.run`, `lint.run`, `format.run` | Application `profile` name of the matching kind | Process result |
| `data.parse`, `data.serialize` | Explicit format (`json`, `toml`, `yaml`, `xml`, `csv`) and text/value | Parsed value or serialized text |
| `data.query`, `data.transform` | Value, JSON pointer and explicit selection | Selected value |
| `text.diff` | `before`, `after`, optional context | Bounded unified line diff |
| `hash.digest` | `algorithm` (`sha256`, `blake3`), explicitly typed input | Lowercase `hex` |
| `codec.encode`, `codec.decode` | Explicit encoding (`base64`, `hex`) and typed bytes/text | Encoded text or decoded bytes/UTF-8 |
| `fs.tree` | `path`, `max_entries` | Tree entries and root digest, omitting protected entries |
| `fs.mkdir` | `path`, `expected_absent: true` | Created directory |
| `fs.move` | `source`, `destination`, `expected_sha256`, `expected_destination_absent: true` | Confirmed move |
| `fs.remove` | `path`, exact hash for files; explicit recursive options for nonempty directories | Removed entry count |
| `fs.prepare_change_batch` | Exact `operations` with old-state preconditions and trusted syntax language | Durable prepared ID, digest and candidate details; user files unchanged |
| `fs.submit_change_batch` | `batch_id`, `expected_batch_digest` | Durable terminal and per-path evidence |
| `fs.batch_status` | Optional `batch_id` | Durable phase and current old/new/third-party path evidence |
| `fs.recover_change_batch` | `batch_id`, `expected_batch_digest`, `action` (`commit`, `rollback`) | Explicit recovery result |
| `archive.list`, `archive.read` | Workspace `path`, `format` (`zip`, `tar`), `entry` for read | Validated entries or nested `data` with explicit encoding; no extraction |
| `search.query` | `query`, optional `path`, `limit` (1–1000) | Application service result |
| `mcp.discover` | Optional `server` | Application service result |
| `mcp.describe` | `server`, `tool` | Application service result |
| `mcp.call` | `server`, `tool`, object `arguments` | Application service result |
| `skills.list` | `{}` | Application service result |
| `skills.read` | `name` | Application service result |

Every object rejects unknown fields. Result and error payloads have byte limits. File limits count UTF-8 bytes rather than characters. Result/total budgets below two bytes (the JSON string `""`) and invalid deadlines are rejected at construction. Limits and call counts apply across the lifetime of the host and all its scopes; there is no implicit budget reset.

Error budgets count the actual serialized JSON message, including quotes, escaping and UTF-8 bytes. Every error exit uses this accounting, including validation, permission, audit start, replacement diagnostics and failed settlement. Messages are truncated only at character boundaries. An admitted call reserves four bytes: an empty result message and an empty settlement-failure message. Reservations prevent queued/scoped calls from consuming the final diagnostic allowance. The sink receives the bounded, charged result. If settlement fails, its separate returned diagnostic is also bounded and charged, and `dropped` retains the unknown terminal record. If a diagnostic needs more cumulative bytes than remain, the budget is permanently exhausted. After the allowance is exhausted, further rejected calls emit an empty terminal exhaustion message and cannot reach permission or audit; no finite cumulative budget can encode infinitely many rejected calls. The JSON error/RPC envelope and diagnostic accessors are not additional call-result payloads.

The five basic file capabilities are always present. `HostConfig::data: Option<cayw_data::Limits>` and `HostConfig::workspace: Option<cayw_workspace::Limits>` default to `Some(Default::default())`, so the ordinary catalog has 23 bounded capabilities. Set either field to `None` to omit that group. Both groups use the same Host validation, permission gate, cumulative byte/call budgets, durable audit, cancellation and permanent unknown stop as the other capabilities. Each catalog entry carries its executor; prefix matching cannot send a new `fs.*` operation to the basic-file executor. Host argument limits and each group's serialized input limits are checked before permission or opening files. Direct Rust arguments also have a 128-level nesting ceiling before JSON serialization or schema validation; rejected deep values are disposed of iteratively. Listing the catalog creates neither a lock nor a workspace store.

Workspace opens lazily after permission and durable audit start. Its file/tree limits are capped by the Host file/entry limits. One absolute deadline and the caller's child cancellation token cover opening, journal validation, status inspection, execution and recovery; these stages never restart the allowance. Workspace cleanup also uses that deadline/token. If cleanup cannot finish, the last durable intent/committing record keeps the recovery fence. Host audit settlement is still awaited to preserve the terminal record. Built-in data limits additionally bound parser depth, node count, rows and diff work. The default batch syntax validator accepts JSON and TOML only; an application can supply its own trusted `SyntaxValidator`. Unsupported languages are refused, not passed to an arbitrary command.

Git appears only when `HostConfig::git_program` is an existing absolute executable path. Build/test/check/lint/format appear only for configured profiles of the matching kind. External capabilities appear only when `HostConfig::external` is supplied. Unconfigured capability calls fail before permission or audit.

## Filesystem boundary

`cap-std` and `cap-fs-ext` 4.0.3 open a workspace directory handle. Each path component is opened relative to the previous handle without following symlinks. File reads use a no-follow, nonblocking open, reject special files, and enforce byte limits while reading. Writes never use a validated ambient path with `std::fs::write` afterward.

Paths must be portable relative names. Absolute paths, `..`, redundant separators, backslashes, colons, control characters, Windows device names, `.git` components, and reserved `.cayw-*` names are rejected. `.git` is reserved because modifying its config, hooks, or index crosses the boundary of the fixed Git capabilities. `fs.stat` reports a final symlink as a symlink; reading, traversing, or replacing one is refused. Missing parent directories are errors; `fs.mkdir` creates exactly one explicitly named directory.

Replacing a file requires both an explicit overwrite flag and the current SHA-256. Patch always requires the current SHA-256. All cooperating host instances and processes acquire the same OS file lock through `.cayw-host-lock`, then compare the digest and commit while holding it. Two writes with the same expected digest cannot both succeed. The lock file remains in place and is hidden from listings. External programs or users that ignore this advisory lock can still race a write; the host is not an OS sandbox.

Ordinary `fs.write`/`fs.patch` and all named process profiles inspect persisted batch status while holding `.cayw-host-lock`, then keep that lock through the write or process completion. An interrupted batch blocks them across fresh Host instances, including hosts configured with `workspace: None`. Workspace mutation operations apply the same journal fence. No Git mutation is exposed; the fixed read-only Git capabilities remain available. A pending batch found before dispatch is a known refusal with no new user-file effects. Reads, tree/status and explicit recovery remain available through the gate on a fresh Host. A Host already marked unknown remains stopped even for recovery.

Change batches are recoverable multi-path changes, not atomically visible transactions. Prepare persists backups and syntax-checked candidates without changing user paths. Submit rechecks every old-state precondition under the shared lock and records intent before installing files. An interruption can leave some paths installed. Recovery requires the exact batch digest and an explicit commit/rollback choice; third-party path content blocks it. A denied or failed recovery cannot clear the journal. Result or audit loss after a mutation preserves unknown status.

Writes create a temporary file in the destination directory, write bounded data, synchronize file contents, atomically replace the entry, and synchronize the directory. Creation uses a hard link that fails if a destination already exists. Existing file permission bits are preserved; new Unix files start at `0600` subject to the process umask. Other metadata such as ACLs and extended attributes is not copied. A failure after the new entry may have committed returns unknown and stops the host. A renamed ancestor stays bound to the opened directory handle; no symlink is followed through a later ambient reopen.

Filesystem system calls and durable audit writes are synchronous bounded work inside an async call. Cancellation is checked between file chunks and before commit, but cannot interrupt an in-progress filesystem `fsync` or an audit sink that does not return. An audit sink must implement appropriate storage deadlines without claiming a partial write is a known failure.

## Frozen read lifecycle

`HostConfig::frozen_reads` defaults to `false`. A trusted host can enable an explicitly frozen `fs.read` view and call `admit_frozen_read(arguments, cancel)`. This returns a non-Clone `FrozenReadTicket` with its own permission decision, original audit ID, deadline, child cancellation token and charged argument/result budgets. `capture` reads bounded UTF-8 bytes under the held root. `accept` independently checks a shared value's exact path, bytes, length, digest and stamp. `settle` records that original Host terminal. `acknowledge` releases the ticket only after the caller has committed its own Cells dependencies and terminal; dropping it first records its exact abandoned original read.

A stamp contains held-directory identity, incarnation, generation and a digest of the read schema and configured bounds. `.cayw-host-read-fence` is a stable OS lock file, while `.cayw-host-read-state` is replaced with synchronized generation state. A ticket retains shared lock ownership through Host/Core settlement and Cells acknowledgment. Cooperating file and Workspace mutations, process profiles and unclassified external/plugin services acquire the exclusive fence and persist an advanced generation plus the exact original pending mutation before dispatch. Even an identical-content write changes generation. A confirmed terminal and durable audit clear the pending mutation. Unclassified services and profiles also disable later reuse for that root; independently admitted reads continue through the original uncached path.

An unknown mutation, abandoned read or retained persistence fault blocks further frozen admission across Host connections. `pending_mutation` and `pending_read_faults` inspect durable originals. The generation state retains each original mutation, and abandoned tickets create separately named read records. If that read record cannot be created, a separate marker retains the full original read identity. Recovery does not replace these identities with diagnostic text. Trusted `recover_mutation(FenceRecovery, cancel)` and `recover_read_fault(ReadFenceRecovery, cancel)` require the exact original record plus stopped-owner, Device cleanup and canonical terminal evidence. Runtime validates those facts before calling them. These APIs do not clear an already unsafe Host instance or retry the original effect. If all attempts to persist an uncertainty record fail, the process retains its permanent stop and reports the storage failure; this does not establish a durable recovery record on a failed storage device.

This is a cooperating Host generation view. Outside editors and external programs that ignore these fences are outside the frozen view and are not detected automatically. The implementation does not use file size or modification time as a content snapshot, and does not provide a transactional snapshot of the whole workspace. Tests exercise actual OS lock contention, held-root rename, independent call/byte charges, cancellation, abandoned acknowledgments, malformed/symlinked fault markers and exact recovery.

## Git and process boundary

Git uses a private view for every call. It copies bounded passive HEAD/index/sharedindex files/refs/packed refs/shallow data, `info/exclude`, object files and permitted worktree bytes from held directories, writes its own clean config, and supplies an empty process environment plus explicit Git settings. Historical `show` and deleted-subtree `diff` paths need not exist in the current tree; every existing prefix is checked without following links, and the entire relative name is checked before dispatch. Repository config, worktree config, config includes, hooks, pagers, fsmonitor, filters, credentials, external diff and text conversion commands are never loaded. A regression test supplies a malicious clean filter as well as diff, textconv, pager and monitor settings and confirms none execute. Optional index locks, lazy fetch, replacement refs, automatic maintenance and submodule recursion are disabled. Only fixed command arguments are accepted; paths use literal pathspecs and `git.show` accepts a full object ID rather than arbitrary revision syntax.

This initial Git implementation requires a physical `.git` directory directly inside the selected workspace, SHA-1 objects, and the ordinary files/packed-refs storage format. Linked worktree/gitdir pointers, SHA-256 repositories and reftable storage are not supported. Metadata copies share the file/list entry budgets. The command reads a bounded snapshot of metadata, so concurrent outside Git writers can produce an error or an older view; it does not provide a repository transaction. It also does not sandbox Git's parsing of object files or grant access to an untrusted executable.

A `Profile` contains `ProfileKind::{Build, Test, Check, Lint, Format}`, an absolute `program`, fixed `args`, an explicit `env` map, timeout, output limit and `allow_filter`. `HostConfig::profiles` maps application-selected names to those profiles. Test filters are a single positional argument, begin with an ASCII letter/digit, and contain only ASCII letters/digits plus `_ : . - /`. The application decides whether its test command interprets that position as a filter. Only test profiles can accept a filter. Agent-supplied argv, executable names, environment variables and working directories are rejected.

The actual `cayw-device::ProcessRunner` executes every process using its program allowlist and literal argv, closed stdin, workspace cwd, explicit environment, cancellation, timeout and combined output limit. A confirmed nonzero exit is a successful capability response with `success: false`; it does not imply that build outputs were rolled back. Cancelled, timed out, overflowed or incompletely supervised processes return unknown because their filesystem/remote effects may already have occurred. A dropped process future requests cleanup through Device and retains an unknown audit record.

Profiles pass an owned clone of the held workspace directory to Device through `DirectoryProcessSpec`. Git copies bounded passive metadata, objects and worktree bytes from held nofollow directories into an owned private snapshot and launches there. Renaming or replacing the original path cannot redirect process cwd or prepared Git inputs. Device supervision and snapshot cleanup must finish before Runtime releases admission; unconfirmed cleanup preserves unknown status. Unsupported process platforms fail closed. The macOS tests do not establish Windows or Linux acceptance.

A configured process and optional external service retain the OS access granted by the application. These are explicit permissions to run host code, not isolation from the rest of the machine. No general Bash, Shell, Node or Python capability is exposed.

## Reproduce

```sh
cargo +1.95.0 test -p cayw-host --locked
cargo +1.95.0 check -p cayw-host --all-targets --locked
cargo +1.95.0 clippy -p cayw-host --all-targets --locked -- -D warnings
cargo +1.95.0 run -p cayw-host --example local --locked
```

The offline example creates a temporary workspace, authorizes one exact write, invokes it through real Luau CodeMode, and writes a synchronized JSON-lines audit next to the workspace. It prints both paths and the execution report. It preserves those files for inspection.

The integration suite uses real filesystem operations, two independent hosts contending for the same digest, real Device child processes, a local Git fixture, a hand-built TAR and real Luau data/hash/change-batch calls. Development-only workspace fault injection cancels a two-file batch after the first installation; fresh Hosts demonstrate ordinary-write/profile fencing, denied recovery and explicit commit/rollback. Mutation result overflow and audit settlement failure retain unknown and stop the Host. The workspace fault-injection feature is enabled only by this crate's development dependency. Unix Git tests use `/usr/bin/git` by default; set `CAYW_HOST_TEST_GIT` to another absolute executable when needed. Process tests are Unix-only because Device currently refuses process supervision on unsupported platforms.

Validation in this workflow is on macOS with Rust 1.95.0. Windows and Linux behavior remains unverified; do not treat the macOS results as three-platform acceptance. Real model calls and external MCP/skill services are not exercised. External adapters are application-supplied; deterministic fixtures cover routing, permission, cancellation, result bounds and audit behavior here.

## Dependencies

The directory libraries use `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT`; SHA-256 (`sha2`) uses `MIT OR Apache-2.0`. The selected Apache-2.0/MIT terms are compatible with this workspace's Apache-2.0 license. No third-party implementation source is copied into this crate. The crate uses the workspace's existing async, JSON and runtime dependencies.
