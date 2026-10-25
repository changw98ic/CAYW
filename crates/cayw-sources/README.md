# cayw-sources

`cayw-sources` prepares host-owned local sources for the injected Bridge boundary.
It does not discover or start sources by itself. The host explicitly creates a
source and moves its single execution handle into a `cayw-bridge::Registry`.
Runtime never creates a second connection or loader.

The crate currently supports three host-selected sources:

- A real local MCP server over stdio, using protocol `2026-07-28`.
- Explicit, immutable Markdown Skill files listed by the host.
- Workspace text search through the existing bounded `fs.grep` executor.

## MCP stdio API

```rust
use cayw_ai::CancellationToken;
use cayw_sources::{StdioConfig, StdioSource};

let source = StdioSource::connect(
    StdioConfig::new("/absolute/program", "/absolute/existing-directory"),
    CancellationToken::new(),
).await?;

let info = source.server_info().clone(); // ServerInfo { name, version }
let control = source.control();           // observations and shutdown only
let (manifest, handle) = source.into_parts();
```

`StdioConfig::new` takes an absolute executable file and an absolute existing
working directory. Optional fields set arguments, environment, frame, pending,
observation and stderr budgets, plus request and cleanup timeouts.

`StdioControl` is cloneable and cannot issue tool or resource requests:

- `snapshot()` returns `StdioSnapshot` with the process ID, spawn count, observed
  methods, tool/resource call counts, progress/cancellation/late-reply counters,
  received frames, stderr bytes, reaping and cleanup state, and a terminal
  failure reason.
- `close()` requests transport shutdown.
- `wait_closed()` waits until the cleanup attempt is marked `cleanup_complete`.
  Read `reaped` and `failure` to determine whether cleanup succeeded.

## Markdown Skill API

```rust
use cayw_bridge::Limits;
use cayw_sources::skills::{FileSkill, FileSkills};

let skills = FileSkills::load(
    vec![FileSkill {
        name: "addition".into(),
        description: "Host-owned addition Skill".into(),
        path: "/absolute/addition.md".into(),
    }],
    &Limits::default(),
)?;

let manifest = skills.manifest().clone();
let stats = skills.stats();             // FileSkillStats
let handle = skills.into_handle();
```

`name` is a logical name, never a path. `description` is bounded. The host must
list every file explicitly. A file must be an absolute path with the `.md`
extension, must be a regular UTF-8 file, and must not contain `.` or `..`
components. Symlinks below a root-level system alias are rejected. macOS
`/var` -> `/private/var` is resolved once at load; the pinned root, parent
directories and file device/inode are rechecked on every read.

The body is returned verbatim. It is never parsed for permissions, commands or
metadata. The manifest contains the SHA-256 computed by the host's single load
scan. `skills.list` uses manifest metadata only and opens no file. A model
`skills.read` performs a bounded on-demand read and verifies the pinned
identity, size, UTF-8 and digest before returning content.

`FileSkillStats::snapshot()` reports:

- `load_scans`: host scans performed by `FileSkills::load`.
- `on_demand_reads`: reads requested by the model through the Bridge.
- `rejections`: rejected reads or operations.
- `closed`: whether `disconnect()` has closed this loader.

## Workspace search API

`search::WorkspaceSearch::open(root)` opens one explicit workspace root and
retains its directory handle. Pass a canonical absolute path without symlink
components; a trusted host may resolve macOS `/var` aliases before opening it.
Register `manifest().clone()` and `into_handle()` with the Registry. `stats()`
returns observations only.

The source accepts `search.query` arguments `{query, path?, limit?}`. It uses
literal text matching with two context lines, respects gitignore files, and
bounds matches, traversal, file bytes and returned bytes through the existing
Workspace limits. Results contain paths, file digests, byte offsets, line
numbers, context and scan-completion status. The summary preserves skipped paths,
protected-entry omissions, gitignore policy and scan counts. `scan_complete`
means the allowed scan finished; it does not mean ignored or protected files
were searched. An oversized envelope is rejected as a known failure rather than
silently returning incomplete context. It does not create a code index.

Workspace preparation requires a writable root. It creates the existing
`.cayw-store` and uses Workspace's existing preparation checks and operation
locks. Only authorized calls scan file content. Disconnect closes the source for new calls; Runtime can still run
ordinary conversations without a search source. This source opens its own fixed
root handle; the host must select the intended directory before construction.
It does not inherit another Host object's directory handle.

## MCP protocol scope

The stdio adapter implements the local `2026-07-28` JSON-RPC shape used by the
fixture. There is no `initialize` handshake. Every outgoing request carries
`io.modelcontextprotocol/protocolVersion`,
`io.modelcontextprotocol/clientCapabilities` and
`io.modelcontextprotocol/clientInfo` inside `_meta`. Every successful response
object must carry `io.modelcontextprotocol/serverInfo`.

Supported calls are `tools/list`, `resources/list`, `tools/call`,
`resources/read`, progress notifications and cancellation notifications. The
adapter rejects paging, missing `resultType` where required and server-initiated
requests. Child processes receive only the environment explicitly configured by
the host. HTTP/SSE and subscriptions are outside this stdio adapter's scope. It
does not retry. Tool catalog and resource catalog sizes are bounded.

## Bridge envelope

After registration, `skills.read` and `mcp.call` return the Registry envelope:

```json
{"request": {...}, "source": {...}, "value": ...}
```

Luau must read `.value` for those two calls. Metadata calls such as
`mcp.discover`, `mcp.describe`, `skills.list` and `mcp.resources.list` return
their result object directly. Do not apply `.value` blindly.

Qualified Skill names are `source_id/name`, for example `files/addition`. The
Registry passes only the local name (`addition`) to the file handle. Visibility
and authorization are separate: discovery never grants execution. Every actual
invoke or read is journaled before dispatch.

## Budgets and unknown outcomes

The caller supplies `cayw_bridge::Limits`. They bound source count, catalog
metadata, tool arguments, results, Skill bytes, resources, in-flight calls,
source operations and total source bytes. `StdioConfig` adds frame, pending,
received-frame, stderr and timeout bounds. A Skill read is additionally bounded
by both `max_skill_bytes` and the Registry result budget.

Cancellation and deadline expiry are terminal. If an effect may have happened,
the error is returned with `outcome_unknown`; callers must not retry or replay
it. The host must reconcile the durable Core/Bridge records and the original
execution identity before resuming a task with unknown effects. Installing a
replacement source does not clear the old unknown outcome. There is no automatic
reconnect or replay in this crate. Process-group signalling and child reaping
are cleanup steps, not a security sandbox and not proof that external effects
were rolled back. Bridge conservatively treats `search.query` as an external
operation: cancellation or disconnect after dispatch may retain unknown even
when this particular adapter only scans files. Cumulative reply bytes are
charged on acceptance; exhaustion after an external operation also retains
unknown. Reconnect does not reset these counters.

Blocking file reads and scanner system calls can finish their bounded work after
the outer request is dropped. Their late results are discarded; cancellation
cannot interrupt a kernel read already in progress. Skill preparation is not a
transactional file snapshot; concurrent changes can require a fresh load.

## Shutdown

For stdio:

1. Call `StdioControl::close()`.
2. Await `wait_closed()`.
3. Confirm `cleanup_complete == true`, `reaped == true` and `failure == None`
   in the final snapshot.

For file Skills, call `Registry::disconnect("files")` (or the registered source
ID) and confirm `FileSkillStats::snapshot().closed == true`.

## Runnable example

The `host_sources` example in `cayw-runtime` shows both the SDK and authenticated
IPC paths using the real stdio fixture and a real Markdown file. Its Provider is
a fixed script, not a real model.

After building the fixture, pass its absolute path and three new absolute
directories:

```sh
cargo build -p cayw-runtime --bin cayw-runtime-mcp-fixture --locked
cargo run -p cayw-runtime --example host_sources --locked -- \
  --fixture /absolute/path/to/cayw-runtime-mcp-fixture \
  --workspace /absolute/new-workspace \
  --state /absolute/new-state \
  --artifacts /absolute/new-artifacts
```

The example writes its JSON records with `create_new` and `sync_all`, reports
`real_model: false`, and fails unless the stdio child was reaped, the Skill
source was closed, and the loopback IPC server stopped. The IPC token is not
written to any artifact.

## Managed lifecycle (F14)

`ManagedStdio` wraps the same stdio transport with host-selected residency:

| Mode | Probe | First use | Idle recycle |
| --- | --- | --- | --- |
| `Lazy` (default) | connect → catalog → stop | reconnect | stop after `idle_timeout` when `in_flight == 0` |
| `LazyKeepAlive` | connect → catalog → stop | reconnect | never while registered |
| `Eager` | keep first connection | already live | never |

```rust
use cayw_ai::CancellationToken;
use cayw_sources::{LifecycleMode, ManagedStdio, ServerSpec, StdioConfig};
use std::time::Duration;

let prepared = ManagedStdio::prepare(
    ServerSpec {
        id: "local".into(),
        version: "2".into(),
        origin: "host:example".into(),
        config: StdioConfig::new("/absolute/program", "/absolute/directory"),
        lifecycle: LifecycleMode::Lazy,
        idle_timeout: Duration::from_secs(60),
    },
    CancellationToken::new(),
).await?;

let control = prepared.control(); // observations only
let (manifest, handle) = prepared.into_parts();
// register handle into cayw-bridge::Registry; mcp.discover stays metadata-only
```

Catalog entries remain registered while the child is stopped. `mcp.call`,
`mcp.resources.read` and `mcp.prompts.get` start a fresh process. A recycled
connection that returns a different tool/resource/prompt set or server identity
fails closed and requires an explicit host reconnect.

## External config import

`import_mcp_servers_json` accepts Claude Desktop / Cursor / VS Code / Codex style
documents with an `mcpServers` object. Only absolute local stdio `command` paths
are accepted; `npx`/`uvx`/relative names and URL/SSE/HTTP entries are skipped with
reasons. Optional `lifecycle` and `idleTimeout` (minutes) map onto `ServerSpec`.

Prompt templates are discovered through `prompts/list` and fetched with
`mcp.prompts.get`. Listing is metadata-only and never grants execution.
Server-driven elicitation / forms are outside this local adapter.
