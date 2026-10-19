# cayw-workspace

Workspace executors for Issue #5: bounded file trees, exact file changes with durable recovery, and read-only ZIP/TAR inspection. This crate is **not a permission boundary** and deliberately does not implement `CapabilityHost`. Never expose `Workspace::execute` directly to a model. The application Host must validate schemas, authorize the exact arguments, start durable audit, enforce cancellation and cumulative budgets, dispatch, and durably settle the outcome. Catalog membership grants no permission. `cayw-host` dispatches these capabilities through its permission and audit path; the executor alone does not establish that integration's acceptance.

## Rust interface

```rust
pub fn Workspace::open(root: impl AsRef<Path>, limits: Limits)
    -> Result<Workspace, CapabilityError>;
pub fn Workspace::open_dir(root: cap_std::fs::Dir, limits: Limits)
    -> Result<Workspace, CapabilityError>;
pub fn Workspace::open_dir_until(
    root: cap_std::fs::Dir, limits: Limits,
    cancel: CancellationToken, deadline: std::time::Instant,
) -> Result<Workspace, CapabilityError>;
pub async fn Workspace::execute_until(
    &self, name: &str, arguments: &serde_json::Value,
    cancel: CancellationToken, deadline: std::time::Instant,
) -> Result<serde_json::Value, CapabilityError>;
pub fn Limits::validate(&self) -> Result<(), CapabilityError>;
pub fn Workspace::definitions(&self) -> Vec<ToolDefinition>;
pub fn definitions() -> Vec<ToolDefinition>;
pub async fn Workspace::execute(
    &self, name: &str, arguments: &serde_json::Value,
    cancel: CancellationToken,
) -> Result<serde_json::Value, CapabilityError>;
```

`CancellationToken` and `ToolDefinition` are re-exported from `cayw-ai`; `CapabilityError` is re-exported from `cayw-codemode`. The executor repeats strict argument validation. Limits include individual file bytes, total backup/candidate bytes, operation/journal count, tree depth/entries, argument/output bytes, archive container/entry/expanded sizes, compression ratio and elapsed time. Hard ceilings bound allocations even when application configuration is mistaken. Default syntax validation parses JSON and TOML; `Limits::syntax_validator: Arc<dyn SyntaxValidator>` lets the trusted application inject another bounded parser. Unknown languages are rejected. Parsing does not prove compilation or program safety.

A root supplied as a path must be absolute and application-selected. A Host that already owns an open root passes a cloned directory handle to `Workspace::open_dir_until`, then calls `execute_until` with the same token and absolute deadline. Opening, journal validation, status and recovery share that allowance; the executor also caps it by its local timeout. `open`/`open_dir` and `execute` are convenience entries for independently bounded application operations. Directory handles preserve the same directory when its pathname is renamed or replaced. `Limits::validate` checks configuration without opening or modifying a workspace. Every root/parent component is opened with a directory handle without following links. Public paths use a conservative printable ASCII subset with `/`, have no normalization components, Windows prefixes or reserved device names, and cannot address `.git` or any `.cayw-*` component, case-insensitively. Non-ASCII names are rejected to avoid Unicode normalization aliases between supported filesystems; content remains unrestricted bytes/UTF-8. Symlinks, sockets, devices and FIFOs are rejected. Regular files may have hard-link aliases; replacing a file uses a fresh inode, and removal deletes only the specified directory entry. The root and internal journals must be owned and protected by the application.

## Capability arguments and results

All arguments reject unknown properties. Optional fields must be omitted, not set to null. Configured limits can be smaller than schema ceilings.

| Capability | Arguments | Success result |
| --- | --- | --- |
| `fs.glob` | directory `path` (`.` allowed), `pattern`; optional `respect_gitignore` (default true), `max_results` (default 1000) | `{path, pattern, paths, skipped, scan_complete, truncated, protected_entries_omitted, respect_gitignore, scan}`; deterministic regular-file paths relative to workspace |
| `fs.grep` | file or directory `path`, `pattern`; optional `literal`, `multiline`, `dot_matches_new_line`, `case_insensitive` (default false), `respect_gitignore` (default true), `max_matches` (default 100), `context_lines` (default 0, maximum 64) | `{path, matches, skipped, scan_complete, truncated, protected_entries_omitted, respect_gitignore, scan}`; full-file UTF-8 matches bind SHA-256, half-open byte ranges, inclusive 1-based lines and context |
| `code.outline` | file `path`, `language: "rust"`; optional `max_symbols` (default 1000) | `{path, language, sha256, symbols, total_symbols, truncated, syntax_only: true}`; parsed declarations and inline module/impl/trait members, with source ranges and parent indices |
| `code.diff_hunks` | supplied `diff` text; optional `max_hunks` (default 1000) | `{sha256, hunks, total_hunks, truncated, validated_complete: true}`; validated file paths, header/body locations and old/new line spans; no Git or filesystem read |
| `fs.mkdir` | `path`, `expected_absent: true` | `{path, created: true}`; exactly one directory, parents must exist |
| `fs.move` | `source`, `destination`, `expected_sha256`, `expected_destination_absent: true` | `{source, destination, sha256, moved: true}`; regular file only |
| `fs.remove` | `path`, file `expected_sha256`; or empty directory; or `recursive: true`, `expected_root_sha256`, `max_entries` | `{path, removed_entries, root_sha256?}` |
| `fs.tree` | `path` (`.` allowed), `max_entries` | `{path, root_sha256, entries: [{path,kind,bytes,sha256,mode}], protected_entries_omitted: true}`; root included in bound |
| `fs.prepare_change_batch` | `operations` as below | `{batch_id, batch_digest, status: "prepared", user_files_changed: false, operations: [{path,operation,expected,candidate,language,candidate_content}]}` |
| `fs.submit_change_batch` | `batch_id`, `expected_batch_digest` | Batch status below |
| `fs.batch_status` | optional `batch_id` | One batch status, or `{batches: [...]}` |
| `fs.recover_change_batch` | `batch_id`, `expected_batch_digest`, `action: "commit" \| "rollback"` | Batch status below |
| `archive.list` | `path`, `format: "zip" \| "tar"` | `{path, archive_sha256, entries: [{path,kind,bytes,compressed_bytes}]}` |
| `archive.read` | `path`, `format`, `entry` | `{path, archive_sha256, entry, data: {encoding: "utf8" \| "base64", content, bytes, sha256}}` |

Search shares a no-follow scanner: every traversed component uses held directory handles, regular files are opened without following links and with nonblocking options, and protected `.git` / `.cayw-*` entries are omitted. Discovered symlinks and special entries are explicitly listed in `skipped`; an explicitly selected unsafe root fails. Root and ancestor entries count toward `max_tree_entries`. Sorted directory traversal gives repeatable order for unchanged contents. Glob patterns support `**`, `*`, `?`, character classes and alternation, relative to the selected directory, with separators kept literal. Nested `.gitignore` precedence, negation, anchored patterns, directory rules and excluded parents use `ignore::gitignore`; rule bytes come only from no-follow bounded reads and `GitignoreBuilder::add_line`, without ambient user/global configuration. Ignore input is capped at 64 KiB total, 1024 lines (also bounded by `max_tree_entries`), and 4096 bytes per line. Invalid, non-UTF8, binary or unsafe ignore files fail. `respect_gitignore: false` bypasses rules while preserving confinement.

Grep reads complete bounded files and reports a SHA-256 of the exact bytes used. Match fields are `{path, sha256, byte_start, byte_end, start_line, end_line, text, context: {start_line, end_line, text}}`; offsets are UTF-8 bytes, end offsets are exclusive, and lines are inclusive and start at 1. NUL-containing or non-UTF8 regular files are explicitly skipped. Regex input is capped at 16384 bytes, compiled size at 1 MiB, per-call DFA cache at 256 KiB, nesting at 64, and repeated suffix-search work at 256 MiB of charged byte/pattern work. No regex persists between calls. File bytes, aggregate read bytes (including ignore files), entries, depth, input/output and deadline/cancellation retain the configured `Limits`. A file/traversal/work/output bound fails with a known error; it never implies a complete scan. One additional match after the result limit sets `truncated: true` and `scan_complete: false`; exactly reaching the limit can still be complete. Numeric result defaults are clamped to `max_tree_entries`.

Rust outlines use `syn` 2 and `proc-macro2` source spans. They preserve raw identifiers, Unicode and original BOM/shebang offsets, parse the entire file before returning any symbols, and include inline module, impl, trait and foreign-block members. They do not expand macros, follow external modules, resolve names or prove compilation. A conservative pre-parser shape bound limits nesting and punctuation, so some valid but very complex Rust is refused as a resource limit. `code.diff_hunks` validates the entire supplied unified text diff even when the returned hunk list is truncated. Every hunk checks body counts, nonoverlapping order, null-side insertion/deletion rules, path metadata, no-newline markers and bounded counters. Unsupported binary/combined diff formats fail explicitly. Both return known failures without user-path effects. The synchronous parser/regex/kernel calls cannot be interrupted mid-call; the next bounded checkpoint observes cancellation or expiry.

A tree's SHA-256 binds its sorted relative paths, kinds, contents and Unix mode bits; it is not a filesystem snapshot. Protected entries are omitted from reads. Recursive removal rejects any protected entry and requires the current complete root hash and explicit entry bound, then checks each file again. It is sequential and can partially finish; after the first deletion, interruption is `outcome_unknown`. No default recursive removal or `remove_dir_all` is used.

Batch inputs, with exact paths and complete candidates:

```json
{
  "operations": [
    {"op":"write","path":"new.json","expected":{"kind":"absent"},"encoding":"utf8","content":"{\"answer\":42}","language":"json"},
    {"op":"write","path":"config.toml","expected":{"kind":"sha256","sha256":"<64 hex>"},"encoding":"base64","content":"YW5zd2VyID0gNDIK","language":"toml"},
    {"op":"patch","path":"other.json","expected_sha256":"<64 hex>","old":"true","new":"false","language":"json"},
    {"op":"remove","path":"obsolete.json","expected_sha256":"<64 hex>"}
  ]
}
```

A patch requires one nonempty unique exact match, including overlapping occurrences: `aa` in `aaa` is rejected. Match checks retain UTF-8 character boundaries; there is no implicit fuzzy matching. No ancestor/descendant paths, duplicates or case aliases are accepted within a batch. Parents must already exist; batch operations affect regular files only. Prepare reads exact preconditions, parses complete candidates, and durably stores backups/candidates. It does not write user paths. Prepare's returned candidate descriptors are `{sha256,bytes,mode}`; `candidate_content` is explicitly base64. Absence is represented as `null` in result descriptors. Existing mode bits are preserved; new files use `0600`. Full candidate bytes and checksums remain in the private journal and are rechecked at submit/recover.

Batch status is:

```json
{
  "batch_id":"b-...",
  "batch_digest":"<64 hex>",
  "status":"committing",
  "outcome_unknown":true,
  "recorded_installed":0,
  "paths":[{
    "path":"config.toml",
    "old":{"sha256":"...","bytes":12,"mode":420},
    "new":{"sha256":"...","bytes":12,"mode":420},
    "current":{"sha256":"...","bytes":12,"mode":420},
    "matches":"old | new | both | third_party",
    "diagnostic":null
  }]
}
```

Phases are `prepared`, `intent`, `committing`, `committed`, `aborted`, `recovering_commit`, `recovering_rollback`, `rolled_back`, `outcome_unknown`. `recorded_installed` is durable progress information, not proof of actual file contents. Per-path evidence re-reads disk and includes mode, bytes and SHA-256. Errors/special entries are `third_party` with a diagnostic. Concurrent status reads are observations, not a locked snapshot.

## Persistence and concurrency

All mutations coordinate with existing Host filesystem writes using an OS advisory exclusive lock on `.cayw-host-lock`. The same lock covers precondition checks, durable installation and settlement; wait honors cancellation/deadline. Ordinary editors and arbitrary programs do not cooperate with this lock. This is not an OS sandbox, and no ordinary filesystem sequence can make multi-path updates atomically visible or guarantee compare-and-swap against an arbitrary uncooperative writer at the last instruction. Applications needing that guarantee must isolate workspace writers. Direct Host calls during pending workspace recovery must also be blocked by the integrating Host.

`fs.move` installs a no-clobber hard link and then unlinks the source, synchronizing both directories. It requires the same filesystem. There is a window where both names exist. Failure/cancellation after linking is unknown and may leave both paths; the Host must retain the arguments and reconcile them. Batch creation also uses hard-link no-clobber installation. Existing paths are replaced by renaming a synced staging file in the destination directory; source mode bits are preserved. Files and parent directories are synchronized.

Journals live under `.cayw-workspace/b-.../`: immutable `manifest.json`, `backup-N`, `candidate-N`, and atomically replaced, synced `state.json`. A fixed private `state.tmp` and deterministic per-operation `.cayw-workspace-b-...-N-commit` / `-rollback` staging names bound crash leftovers. A stale install staging file is removed and rebuilt only after its bytes match a prefix of the authorized candidate and its mode matches the initial private mode or final candidate mode; unexpected contents block installation. Intent and commit-in-progress states are durable before installing any path. A crash before a prepare is published may leave an unpublished internal directory; it never touched user paths. Reopening validates pending journals and computes current evidence, without replay. Pending batches block further mutations in this executor; status/tree/archive reads remain available. A new mkdir/move/remove/prepare/submit refused before starting has a known failure even when the older journal remains unknown. This classification neither releases the journal fence nor clears a Host's previous unknown result. Explicit recover checks every blob and every path before any recovery write, refuses third-party changes, and can be resumed after another interruption. Recover does not silently apply a prepared or aborted batch.

`max_output_bytes` must be at least 2, the size of an empty JSON string. Execution errors are fitted to that limit using the actual JSON-encoded message length, including quotes and escaped characters; truncation preserves `outcome_unknown`. This bounds the message, not the surrounding Host or RPC error envelope. An ordinary error before user-path effects is a known failure. Any uncertain error after a user-path effect is `CapabilityError { outcome_unknown: true }`, including directory sync/result failures. A process crash may leave `committing` even with progress zero and an already changed file; actual content is authoritative during recovery. Journals are retained and bounded; application maintenance must archive settled records offline while no writer uses the root. There is no automatic journal deletion or claim that a digest authenticates against someone who can rewrite the journal directory.

This is synchronous bounded local filesystem work inside an async entry point, with async lock waits. It does not detach background writes. Cancellation/time checks run between bounded reads, writes and install steps. Journal cleanup never substitutes a fresh token/deadline after expiry; the last durable pending record remains for explicit recovery. A cancellation before the first user-file effect can therefore be a known call failure while its durable intent still needs recovery. OS calls, decompressor calls and trusted parser calls cannot be preempted mid-call; a blocked kernel filesystem operation can exceed elapsed-time limits, but the next checkpoint refuses further effects. Permission/audit/overall cancellation still belongs to the Host.

## Archives

ZIP supports classic single-disk stored/deflate members with matching local and central headers; ZIP64, encryption, data descriptors, unknown extra fields and overlapping records are rejected. Only common timestamp/uid/Windows metadata extra fields are accepted; Unix link extras are rejected. The raw central directory is checked before constructing `ZipArchive`, including physical duplicate records. TAR supports uncompressed regular-file/directory entries; links, devices, sparse entries and GNU/PAX extension entries are rejected in raw mode. Windows backslashes are checked in original TAR header bytes before any library normalization. Nested archives are returned as ordinary bytes, never recursively decoded.

TAR reads check cancellation and the deadline inside the parser's reader as well as between entries and before returning; scanning empty zero blocks cannot bypass those checks. Both formats validate all names, kinds, sizes, duplicates/case aliases and ancestor collisions even when reading one entry. Input container bytes, entry count, depth, per-entry output, total advertised expanded bytes, compression ratio and serialized response are bounded. Only the selected ZIP entry is decompressed and its actual output/CRC checked by the ZIP reader; list reports validated metadata without decompressing every payload. TAR streams every payload to detect truncation and checks members following zero headers, including concatenated archives. Nothing is extracted or written to an archive-supplied path. Text is returned as UTF-8; binary data is explicitly base64.

## Dependencies and licenses

Search/outline additions pin `regex = 1.13.1`, `syn = 2.0.119` (`full`) and `proc-macro2 = 1.0.107` (`span-locations`), all MIT OR Apache-2.0; `ignore = 0.4.33` and `globset = 0.4.20` are Unlicense OR MIT. The newly resolved transitive `crossbeam-deque = 0.8.8` and `crossbeam-epoch = 0.9.21` are MIT OR Apache-2.0. Exact metadata and closure licenses are captured in `target/workspace-search-dependency-metadata.json` and `target/workspace-search-dependency-licenses.json`; no existing registry version was upgraded for these additions.

Own code: Apache-2.0. Direct dependency licenses are compatible: `cap-std` / `cap-fs-ext` (Apache-2.0 with LLVM exception OR Apache-2.0 OR MIT), `sha2`, `serde`, `serde_json`, `base64`, `toml`, `tar`, `tempfile` (MIT OR Apache-2.0), `tokio` and `zip` (MIT); `cayw-ai` / `cayw-codemode` use the repository license. The read capabilities pin `regex = 1.13.1`, `syn = 2.0.119` with `full`, `proc-macro2 = 1.0.107` with `span-locations` (MIT OR Apache-2.0), and `ignore = 0.4.33` / `globset = 0.4.20` (Unlicense OR MIT). Resolved transitive additions include `bstr`, `log`, the crossbeam crates (MIT OR Apache-2.0), and `walkdir`, `same-file`, `winapi-util` (Unlicense OR MIT). Their permissive alternatives are compatible with Apache-2.0. Context7 supplied the search/parser API references; Cargo's resolved metadata records exact package versions, features and license expressions. No code or license is inherited from the older application.

## Reproducible verification

From the repository root, format only this package's source files, then run:

```sh
cargo check -p cayw-workspace --locked
cargo test -p cayw-workspace --locked
cargo test -p cayw-workspace --features fault-injection --locked
cargo +1.95.0 clippy -p cayw-workspace --all-targets --all-features --locked -- -D warnings
```

Tests use real temporary directories and OS subprocesses, including cross-process compare-and-swap, shared Host lock cancellation, actual process exit after one install, durable reopening, commit/rollback recovery, third-party conflict refusal, syntax rejection, symlink/FIFO rejection, strict paths, tree bounds, ZIP bomb/duplicate/link cases and TAR unsafe types. `fault-injection` additionally exposes per-instance failure points; ordinary builds omit the API entirely.

The 2026-10-19 native-read follow-up passed package formatting, `cargo check -p cayw-workspace --all-targets --all-features --offline --locked`, 67 tests via `cargo test -p cayw-workspace --all-targets --all-features --offline --locked`, and Rust 1.95.0 strict Clippy with the same target/feature/offline/locked flags. Logs are `target/workspace-search-{check-final,all-tests-final,clippy-final,fmt-final}.log`. The immutable local source closure snapshot is `.local/workspace-search-final-snapshot` (workspace, codemode, agent and ai); the root lock is preserved as `Cargo.lock.root-original`, while the snapshot-only pruned lock is separately audited and introduces no package versions. Full source inventories, hashes and license metadata are in `target/workspace-search-hashes.json` and `target/workspace-search-evidence.json`. Snapshot commands use that directory's manifest and emit `target/workspace-search-snapshot-{check,tests,clippy,fmt}-final.log`. Tests exercise real temporary directories, nested/ancestor ignore rules and negations, UTF-8 multiline offsets, binary skips, held roots after rename, protected paths, symlink/FIFO/socket refusal, finite regex work, cancellation during scans, malformed syntax/diffs, limits and explicit truncation. These are synthetic local filesystem fixtures; Linux and Windows remain unverified, Windows still fails closed at workspace opening, and this slice does not complete Issue #5 or establish final Runtime/Host acceptance.

The earlier 2026-10-19 macOS ARM64 follow-up ran package formatting, 31 tests with all features (2 unit, 5 archive, 10 recovery, 14 workspace, including subprocess workers), and Rust 1.95.0 all-target/all-feature Clippy with warnings denied. An independent public-API suite also passed all 11 tests after reproducing the earlier empty-TAR cancellation/deadline, overlapping-patch and oversized-error failures. Package and independent logs are `/private/tmp/cayw-workspace-reviewed-repairs-tests-2.log` and `/private/tmp/cayw-workspace-reviewed-repairs-external-2.log`; the independent source is `/private/tmp/cayw-workspace-audit-0kck89h5/audit.rs`. Validation used temporary directories, local synthetic archives and OS subprocesses, with no model service. macOS is the current validation platform. Linux is unverified; Windows fails closed at open because equivalent directory durability/no-follow behavior has not been implemented. No real model/provider calls or paid tests are used. This crate alone does not complete Issue #5 or establish Host integration or three-platform acceptance.
