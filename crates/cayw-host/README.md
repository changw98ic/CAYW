# cayw-host

`cayw-host` supplies bounded workspace capabilities to `cayw-codemode`. The application supplies permission decisions, durable audit storage, optional process and terminal profiles, an optional Git write policy, and its existing external services. It does not create MCP connections, load skills, or contact a model. The model never supplies an executable, argument vector, environment, working directory, or shell command line; it selects only application-defined profiles and fixed catalog operations.

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
| `fs.glob` | `path`, relative `pattern`, optional `max_results`, `respect_gitignore` | Workspace-relative regular-file matches; nested `.gitignore` rules by default, no links or ambient config |
| `fs.grep` | `path`, bounded `pattern`, optional `literal`, `multiline`, `dot_matches_new_line`, `case_insensitive`, `respect_gitignore`, `max_matches`, `context_lines` | Matches with source SHA-256, half-open byte ranges and inclusive 1-based lines; binary/non-UTF-8 files skipped explicitly |
| `code.outline` | `path`, `language` (`rust`), optional `max_symbols` | Local Rust syntax outline with source SHA-256 and symbol ranges; no macro expansion, resolution or module reads |
| `code.diff_hunks` | `diff`, optional `max_hunks` | Validated paths and exact old/new line spans; reads only the supplied diff |
| `fs.write` | `path`, UTF-8 `content`; replacement requires `overwrite: true`, `expected_sha256` | Committed `bytes`, `sha256` |
| `fs.patch` | `path`, `expected_sha256`, nonempty `old`, `new` | Committed `bytes`, `sha256`; exact old text must occur once |
| `git.status` | `{}` | Process result with porcelain status in `stdout` |
| `git.diff` | Optional `path`, `staged` | Process result with diff in `stdout` |
| `git.log` | Optional `limit` (1–100, default 20) | Process result with full commit IDs and subjects in `stdout` |
| `git.show` | Full 40/64 hex `commit`, optional `path` | Commit or blob in `stdout` |
| `git.blame` | `path`, optional full 40/64 hex `commit` | Process result with line attribution from the held worktree and object store |
| `git.stage` | `paths` (1–256 unique), `expected_head`, `expected_index_tree` | Confirmed `head`, `index_tree`, `paths` |
| `git.commit` | `message` (≤64 KiB), `expected_head`, `expected_index_tree` | Confirmed `commit`, `parent`, `tree`; host-owned identity |
| `git.branch` | `action` (`create`, `delete`), `name`, `commit`, `expected_head` | Confirmed `action`, `name`, `commit` |
| `git.worktree` | `action` (`create`, `remove`), `name`, `branch`/`commit` for create, `expected_head` | Confirmed in-root `action`, `name`, `path`, `branch`, `commit` |
| `build.run` | Application `profile` name | Process result |
| `test.run` | Application `profile` name, optional restricted `filter` | Process result |
| `check.run`, `lint.run`, `format.run` | Application `profile` name of the matching kind | Process result |
| `exec_command` | Application terminal `profile` name | Host-generated `session_id`, `profile`, `state`, `output_cursor`, `input_bytes`, `remaining_ms` |
| `write_stdin` | `session_id`, `action` (`write`, `read`, `wait`, `close`, `cancel`), `request_id`, and only the fields that action accepts | Action-specific result with cursor, bytes or terminal state |
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
| `mcp.resources.list` | Optional `server` | Application service result listing host-pinned resource metadata |
| `mcp.resources.read` | `server`, opaque `uri` | Application service result for one manifest-pinned resource |
| `skills.list` | `{}` | Application service result |
| `skills.read` | `name` | Application service result |

Every object rejects unknown fields. Result and error payloads have byte limits. File limits count UTF-8 bytes rather than characters. Result/total budgets below two bytes (the JSON string `""`) and invalid deadlines are rejected at construction. Limits and call counts apply across the lifetime of the host and all its scopes; there is no implicit budget reset.

Error budgets count the actual serialized JSON message, including quotes, escaping and UTF-8 bytes. Every error exit uses this accounting, including validation, permission, audit start, replacement diagnostics and failed settlement. Messages are truncated only at character boundaries. An admitted call reserves four bytes: an empty result message and an empty settlement-failure message. Reservations prevent queued/scoped calls from consuming the final diagnostic allowance. The sink receives the bounded, charged result. If settlement fails, its separate returned diagnostic is also bounded and charged, and `dropped` retains the unknown terminal record. If a diagnostic needs more cumulative bytes than remain, the budget is permanently exhausted. After the allowance is exhausted, further rejected calls emit an empty terminal exhaustion message and cannot reach permission or audit; no finite cumulative budget can encode infinitely many rejected calls. The JSON error/RPC envelope and diagnostic accessors are not additional call-result payloads.

The five basic file capabilities are always present. `HostConfig::data: Option<cayw_data::Limits>` and `HostConfig::workspace: Option<cayw_workspace::Limits>` default to `Some(Default::default())`, so the ordinary catalog has 27 bounded capabilities: 5 basic file operations, 8 pure data operations and 14 workspace file/search/structure/change/archive operations. Set either field to `None` to omit that group. Git read capabilities add five entries only when `git_program` is configured; policy-enabled Git writes add up to four more. Each configured profile kind adds its `*.run` entry, and `terminal_handoffs` adds `exec_command` and `write_stdin`. Supplying `external` adds eight service entries, including the two MCP resource operations. None of these optional groups is present by default. Both groups use the same Host validation, permission gate, cumulative byte/call budgets, durable audit, cancellation and permanent unknown stop as the other capabilities. Each catalog entry carries its executor; prefix matching cannot send a new `fs.*` operation to the basic-file executor. Host argument limits and each group's serialized input limits are checked before permission or opening files. Direct Rust arguments also have a 128-level nesting ceiling before JSON serialization or schema validation; rejected deep values are disposed of iteratively. Listing the catalog creates neither a lock nor a workspace store.

Workspace opens lazily after permission and durable audit start. Its file/tree limits are capped by the Host file/entry limits. One absolute deadline and the caller's child cancellation token cover opening, journal validation, status inspection, execution and recovery; these stages never restart the allowance. Workspace cleanup also uses that deadline/token. If cleanup cannot finish, the last durable intent/committing record keeps the recovery fence. Host audit settlement is still awaited to preserve the terminal record. Built-in data limits additionally bound parser depth, node count, rows and diff work. The default batch syntax validator accepts JSON and TOML only; an application can supply its own trusted `SyntaxValidator`. Unsupported languages are refused, not passed to an arbitrary command.

Git read capabilities appear only when `HostConfig::git_program` is an existing absolute executable path. Git writes appear only for operations enabled by `HostConfig::git_writes`, which also requires `git_program`. Build/test/check/lint/format appear only for configured profiles of the matching kind. Terminal handoffs appear only when `HostConfig::terminal_handoffs` supplies at least one profile. External capabilities, including `mcp.resources.list` and `mcp.resources.read`, appear only when `HostConfig::external` is supplied. Unconfigured capability calls fail before permission or audit. The Host never opens an MCP connection or reads a skill source itself; those operations forward to the application service through the same gate, budgets and audit, and a resource URI is opaque to the Host.

## Filesystem boundary

`cap-std` and `cap-fs-ext` 4.0.3 open a workspace directory handle. Each path component is opened relative to the previous handle without following symlinks. File reads use a no-follow, nonblocking open, reject special files, and enforce byte limits while reading. Writes never use a validated ambient path with `std::fs::write` afterward.

Paths are relative UTF-8 names inside the held root. Ordinary macOS names, including Chinese text, colons, backslashes, trailing dots/spaces and names such as `CON`, are accepted. Absolute paths, `.`/`..` components, empty components, control characters and configured length violations are rejected. Only actual internal names are reserved, compared without ASCII case: `.git`, `.cayw-workspace`, `.cayw-worktrees`, `.cayw-host-lock`, `.cayw-host-read-fence`, `.cayw-host-read-state`, `.cayw-host-read-fault`, `.cayw-host-read-faults`, plus the `.cayw-host-tmp-` and `.cayw-host-read-state-` prefixes. Other `.cayw-*` files are ordinary user files. `.git` is reserved because ordinary file writes to config, hooks or the index would bypass the Git boundary; Git writes use their own real-repository path. `fs.stat` reports a final symlink as a symlink; reading, traversing or replacing one is refused. Missing parent directories are errors; `fs.mkdir` creates exactly one explicitly named directory.

Replacing a file requires both an explicit overwrite flag and the current SHA-256. Patch always requires the current SHA-256. All cooperating host instances and processes acquire the same OS file lock through `.cayw-host-lock`, then compare the digest and commit while holding it. Two writes with the same expected digest cannot both succeed. The lock file remains in place and is hidden from listings. External programs or users that ignore this advisory lock can still race a write; the host is not an OS sandbox.

Ordinary `fs.write`/`fs.patch`, all named process profiles, and policy-enabled Git write commands inspect persisted batch status while holding `.cayw-host-lock`, then keep that lock through the write or process completion. An interrupted batch blocks them across fresh Host instances, including hosts configured with `workspace: None`. Workspace mutation operations apply the same journal fence. Git reads do not require the workspace store and remain available. A pending batch found before dispatch is a known refusal with no new user-file effects. Reads, tree/status, Git reads and explicit recovery remain available through the gate on a fresh Host. A Host already marked unknown remains stopped even for recovery.

Change batches are recoverable multi-path changes, not atomically visible transactions. Before prepare, submit or recovery writes, leaf names in each shared target parent are checked through exclusive creation in a private temporary child directory. This uses that filesystem's actual case and Unicode behavior; names it distinguishes remain usable. Probes are removed before proceeding and never run during journal loading or status reads. Prepare persists backups and syntax-checked candidates without changing user target paths. Submit rechecks every old-state precondition under the shared lock and records intent before installing files. An interruption can leave some paths installed. Recovery requires the exact batch digest and an explicit commit/rollback choice; third-party path content blocks it. A denied or failed recovery cannot clear the journal. Result or audit loss after a mutation preserves unknown status.

Writes create a temporary file in the destination directory, write bounded data, synchronize file contents, atomically replace the entry, and synchronize the directory. Creation uses a hard link that fails if a destination already exists. Existing file permission bits are preserved; new Unix files start at `0600` subject to the process umask. Other metadata such as ACLs and extended attributes is not copied. A failure after the new entry may have committed returns unknown and stops the host. A renamed ancestor stays bound to the opened directory handle; no symlink is followed through a later ambient reopen.

Filesystem system calls and durable audit writes are synchronous bounded work inside an async call. Cancellation is checked between file chunks and before commit, but cannot interrupt an in-progress filesystem `fsync` or an audit sink that does not return. An audit sink must implement appropriate storage deadlines without claiming a partial write is a known failure.

## Frozen read lifecycle

`HostConfig::frozen_reads` defaults to `false`. A trusted host can enable an explicitly frozen `fs.read` view and call `admit_frozen_read(arguments, cancel)`. This returns a non-Clone `FrozenReadTicket` with its own permission decision, original audit ID, deadline, child cancellation token and charged argument/result budgets. `capture` reads bounded UTF-8 bytes under the held root. `accept` independently checks a shared value's exact path, bytes, length, digest and stamp. `settle` records that original Host terminal. `acknowledge` releases the ticket only after the caller has committed its own Cells dependencies and terminal; dropping it first records its exact abandoned original read.

A stamp contains held-directory identity, incarnation, generation and a digest of the read schema and configured bounds. `.cayw-host-read-fence` is a stable OS lock file, while `.cayw-host-read-state` is replaced with synchronized generation state. A ticket retains shared lock ownership through Host/Core settlement and Cells acknowledgment. Cooperating file and Workspace mutations, process profiles and unclassified external/plugin services acquire the exclusive fence and persist an advanced generation plus the exact original pending mutation before dispatch. Even an identical-content write changes generation. A confirmed terminal and durable audit clear the pending mutation. Unclassified services and profiles also disable later reuse for that root; independently admitted reads continue through the original uncached path.

An unknown mutation, abandoned read or retained persistence fault blocks further frozen admission across Host connections. `pending_mutation` and `pending_read_faults` inspect durable originals. The generation state retains each original mutation, and abandoned tickets create separately named read records. If that read record cannot be created, a separate marker retains the full original read identity. Recovery does not replace these identities with diagnostic text. Trusted `recover_mutation(FenceRecovery, cancel)` and `recover_read_fault(ReadFenceRecovery, cancel)` require the exact original record plus stopped-owner, Device cleanup and canonical terminal evidence. Runtime validates those facts before calling them. These APIs do not clear an already unsafe Host instance or retry the original effect. If all attempts to persist an uncertainty record fail, the process retains its permanent stop and reports the storage failure; this does not establish a durable recovery record on a failed storage device.

This is a cooperating Host generation view. Outside editors and external programs that ignore these fences are outside the frozen view and are not detected automatically. The implementation does not use file size or modification time as a content snapshot, and does not provide a transactional snapshot of the whole workspace. Tests exercise actual OS lock contention, held-root rename, independent call/byte charges, cancellation, abandoned acknowledgments, malformed/symlinked fault markers and exact recovery.

## Git and process boundary

Git reads use a private metadata view for every call. It copies bounded passive config, `HEAD`, refs, packed refs, shallow state, shared index segments and `info/exclude` from held directories. Git parses a private copy of the local config with includes disabled. The view then replaces that copy with a clean config containing only supported passive settings: `core.filemode`, `core.autocrlf`, `core.ignorecase`, `core.precomposeunicode`, `core.quotepath` and `core.eol`. Explicit empty boolean values mean false; keys without a value mean true. Other config entries are not carried into the execution view. Object data is read in place with `GIT_OBJECT_DIRECTORY=.git/objects` and `GIT_INDEX_FILE=.git/index`, resolved relative to the Device's held working directory. The original `.git` and object directory handles and the optional index file handle remain open until the process returns. Neither data set is copied, so ignored build data, large unrelated files and repository size do not gate a read. A minimal explicit environment and fixed Git settings disable hooks, pagers, fsmonitor, filters, credentials, external diff and text conversion. Object alternates are refused rather than followed. After the process, `.git`, the object directory, and the index identity and presence are checked again. A replaced or disappeared entry discards the result. A missing index that appears during the read is also a conflict. Historical `show` and deleted-subtree `diff` paths need not exist in the current tree; every existing prefix is checked without following links, and the entire relative name is checked before dispatch. Regression tests supply clean filters, config includes, diff, textconv, pager and monitor settings and confirm none execute. Optional index locks, lazy fetch, replacement refs, automatic maintenance and submodule recursion are disabled. Only fixed command arguments are accepted; paths use literal pathspecs and `git.show` accepts a full object ID rather than arbitrary revision syntax.

Git writes are a separate path that operates on the real repository. They appear only when `HostConfig::git_writes: Option<GitWritePolicy>` is set; each operation is additionally enabled by `allow_stage`, `allow_commit`, `allow_branch` or `allow_worktree`. The policy also supplies the author and committer identity, which no model argument can override. A write requires a direct `.git` directory in the selected workspace: a symlink, a `gitdir:` file, a bare repository, `commondir`, `config.worktree`, object alternates and object/ref symlinks are refused. Linked worktrees cannot be the host root, but the main repository can create and remove controlled worktrees under the fixed `.cayw-worktrees` directory. Every write takes `.cayw-host-lock`, refuses while a workspace change batch is pending, inspects local config values according to each key's meaning, rejecting active includes, filters, helpers and other executable settings unless fixed command arguments neutralize them. A boolean false disables a boolean key, `protocol.*.allow=never` disables that protocol, and `core.gitProxy=none` disables that proxy; the string `false` is still a command in a filter or a filename in `include.path`. The command's fixed `--git-dir`/`--work-tree` and hooks/monitor/pager/signing overrides take precedence over the corresponding local values. The Host checks the caller's `expected_head` plus, for stage and commit, `expected_index_tree` before launching. It then runs fixed argv through the shared Device runner and verifies the resulting repository identity, object IDs, index tree, refs or worktree registration. Hooks, signing, external diff, pagers, credential helpers and network protocols are disabled. `git.stage` accepts 1–256 unique literal workspace paths; `git.commit` requires a nonempty message of at most 64 KiB; `git.branch` creates or safely deletes one local branch at a full object ID. Branch names follow Git's local ref rules and the catalog bounds; ordinary Unicode and names such as `head`, `@` and `refs/heads/ordinary` are accepted. A worktree name is a single directory component under `.cayw-worktrees`, so leading punctuation, spaces, trailing dots and `.lock` are valid directory names. `git.worktree` never forces removal and never accepts an external path. No capability pushes or opens a network protocol. A write that may have changed the repository but cannot be verified returns unknown; a confirmed nonzero exit with unchanged state is a known failure.

This initial Git implementation requires an ordinary repository layout: a physical `.git` directory directly inside the selected workspace, SHA-1 objects, and the files/packed-refs storage format. Selecting a linked worktree/gitdir pointer, SHA-256 repositories, sparse checkouts and reftable storage are not supported. Object alternates are refused; repositories without them are unaffected. Ordinary user files whose names merely start with `.cayw-` (for example `.cayw-note.md`) are normal tracked or untracked files and never block a read. Only exact internal Host state (`.cayw-host-lock`, read-fence records, `.cayw-workspace` and `.cayw-worktrees`) is hidden from status output; a tracked file with such a name is still reported normally. The host suite compares clean `filemode=false` and `autocrlf=true` repositories against real Git, confirms that local filters, config includes, fsmonitor settings and hooks never execute, and reads a repository with more than 64 MiB of ignored build data and a safe tracked symlink. Copied metadata shares the file/list entry budgets; object and worktree data are read in place. A read observes concurrent outside Git writers as an ordinary error or a mixed view; it does not provide a repository transaction. It also does not sandbox Git's parsing of object files or grant access to an untrusted executable.

A `Profile` contains `ProfileKind::{Build, Test, Check, Lint, Format}`, an absolute `program`, fixed `args`, an explicit `env` map, timeout, output limit, `allow_filter`, and host-selected `ProcessIo`. `ProcessIo::Pipes` keeps stdout and stderr separate. On macOS, `ProcessIo::Pty { columns, rows }` attaches all three standard descriptors to one controlling terminal and merges output; both dimensions must be positive. Mode and dimensions are part of the Requests configuration identity, so a permission receipt cannot be reused after changing them. `HostConfig::profiles` maps application-selected names to those profiles. A test filter is one literal positional argv item; it may use ordinary Unicode, spaces, parentheses or a leading underscore, but it may not start with `-` (which a test program could read as an option) or contain control characters. The application decides whether its test command interprets that position as a filter. Only test profiles can accept a filter. Agent-supplied argv, executable names, environment variables, working directories and terminal settings are rejected.

The actual `cayw-device::ProcessRunner` executes every process using its program allowlist and literal argv, workspace cwd, explicit environment, cancellation, timeout and one lifetime output limit. Process profiles use closed stdin; a PTY process profile has no Host input API. Terminal handoffs are the separate opt-in path with `write_stdin`. Git always uses pipes. PTY echo and line-ending conversion count toward the same output limit. A confirmed nonzero exit is a successful capability response with `success: false`; it does not imply that build outputs were rolled back. Cancelled, timed out, overflowed or incompletely supervised processes return unknown because their filesystem/remote effects may already have occurred. A dropped process future requests cleanup through Device and retains an unknown audit record.

`Host::with_process_observer` attaches trusted durable identity records to future launches through the original Device owner. Callbacks run synchronously and must enforce their own storage deadlines. Any record error or unwinding panic closes admission and preserves the failure and transferred permit even if OS cleanup later succeeds. On macOS, the original supervisor waits for already observed PID/startup identities to stop, including identities that later leave its group, but does not signal an escaped group. Descendants that escape before observation and processes that start and exit between observations are outside that recorded coverage; process groups do not provide a security sandbox.

Profiles and terminal profiles pass an owned clone of the held workspace directory to Device through `DirectoryProcessSpec`. Git reads copy bounded passive metadata into an owned private view and run with the held root as their working directory. `GIT_OBJECT_DIRECTORY=.git/objects` and `GIT_INDEX_FILE=.git/index` resolve from that held directory; original repository/object/index handles remain open for post-read identity checks. Git writes launch in the held root and pin `--git-dir=.git --work-tree=.`; model arguments cannot redirect them. Renaming or replacing the root's original pathname does not change the process cwd. Repository, object-directory or index replacement causes the read result to be discarded. In-place edits to the same inode can still produce a mixed read; this is not a repository transaction. Device supervision and view cleanup must finish before Runtime releases admission; unconfirmed cleanup preserves unknown status. Unsupported process platforms fail closed. The macOS tests do not establish Windows or Linux acceptance.

A configured process and optional external service retain the OS access granted by the application. These are explicit permissions to run host code, not isolation from the rest of the machine. No generic capability accepts a model-supplied program, argv, environment, working directory or shell command line. An application that configures an interactive shell as a terminal profile is explicitly granting that program's access; the Host does not create or infer one.

## Terminal handoffs

`HostConfig::terminal_handoffs: Option<TerminalConfig>` is an explicit opt-in. When it is `None`, neither `exec_command` nor `write_stdin` appears in the catalog. `TerminalConfig` contains application-defined `profiles: BTreeMap<String, Profile>` and `TerminalLimits`. The profile fixes the absolute program, argument vector, environment, working directory (the held root), I/O mode, timeout and output limit. A terminal profile may not set `allow_filter`. `ProcessIo::Pty` is accepted only on macOS and requires positive dimensions; `ProcessIo::Pipes` is also available. The model supplies only a profile name and, after the Host returns a session ID, operations addressed to that exact session.

`TerminalLimits::default()` allows 4 concurrent sessions per Host and 64 sessions over the manager's lifetime, 64 KiB per `write`, 1 MiB of input and 1 MiB of output per session, a 64 KiB read page and a 30-second maximum wait. Construction rejects zero or out-of-range limits and requires `max_read_bytes * 4 + 4096` to fit the Host result budget. `max_sessions` is the concurrent limit; the total-session and byte counters are lifetime budgets for the manager. None is reset by a wait, read, close or cancel. The per-session output bound is also capped by the profile's `max_output_bytes`.

`exec_command {profile}` starts one process through Device and returns `session_id`, `profile`, `state`, `output_cursor`, `input_bytes` and `remaining_ms`. A startup cancellation or deadline that cannot prove cleanup returns unknown. `write_stdin` requires `session_id`, `action` and `request_id`; actions are:

- `write`: raw UTF-8 `data`, optional `expected_cursor`; idempotent for the same `request_id`.
- `read`: absolute `cursor` and optional `max_bytes`; pages forward and advances one contiguous delivered offset.
- `wait`: optional `wait_ms`; observes state without extending the session lifetime.
- `close` or `cancel`: stops and joins the original supervisor. It never claims that business effects were rolled back.

A task cannot complete while any session is running or closing, its supervisor has not joined, its exit was nonzero, its output has not reached EOF, output was truncated, or unread bytes remain. `Host::terminal_completion_error()` exposes that check, and `Host::drain_processes()` closes, joins and confirms cleanup before Runtime releases the task. A session belongs to one task's Host and does not survive task drain. An unconfirmed cleanup or unknown terminal outcome is retained and never converted into a successful terminal state by waiting, reading or closing. The model cannot supply a program, argv, environment, working directory, terminal size or shell command line.

## Reproduce

```sh
cargo +1.95.0 test -p cayw-host --locked
cargo +1.95.0 check -p cayw-host --all-targets --locked
cargo +1.95.0 clippy -p cayw-host --all-targets --locked -- -D warnings
cargo +1.95.0 run -p cayw-host --example local --locked
```

The offline example creates a temporary workspace, authorizes one exact write, invokes it through real Luau CodeMode, and writes a synchronized JSON-lines audit next to the workspace. It prints both paths and the execution report. It preserves those files for inspection.

The integration suite uses real filesystem operations, two independent hosts contending for the same digest, real Device child processes, real interactive terminal sessions, a local Git fixture including policy-gated writes and controlled worktrees, a hand-built TAR and real Luau data/hash/change-batch calls. Development-only workspace fault injection cancels a two-file batch after the first installation; fresh Hosts demonstrate ordinary-write/profile fencing, denied recovery and explicit commit/rollback. Mutation result overflow and audit settlement failure retain unknown and stop the Host. The workspace fault-injection feature is enabled only by this crate's development dependency. Unix Git tests use `/usr/bin/git` by default; set `CAYW_HOST_TEST_GIT` to another absolute executable when needed. Process tests are Unix-only because Device currently refuses process supervision on unsupported platforms.

Validation uses macOS with Rust 1.95.0. The commands above exercise this crate's actual filesystem, Git and Device paths. Each validation record covers only its recorded source, target directory and inputs; see the Issue 5 capability matrix and validation record for the recorded results. Real model calls and external MCP/skill services are not exercised by this crate's tests. External adapters are application-supplied; deterministic fixtures cover routing, permission, cancellation, result bounds and audit behavior here.

## Dependencies

The directory libraries use `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT`; SHA-256 (`sha2`) uses `MIT OR Apache-2.0`. The selected Apache-2.0/MIT terms are compatible with this workspace's Apache-2.0 license. No third-party implementation source is copied into this crate. The crate uses the workspace's existing async, JSON and runtime dependencies.
