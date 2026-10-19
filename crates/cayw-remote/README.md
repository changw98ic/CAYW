# cayw-remote

Optional Server relay for a Device that initiates an outbound WebSocket connection. The Server never connects into a Device. The Device client opens no listener. A host supplies the handler and keeps execution permissions and authoritative content on the Device. Neither the local Core nor local execution depends on this crate.

This is a runnable transport and echo/cancellation example for Issue 15. Independent two-machine acceptance, real runtime streaming tools and a deployed TLS proxy are **not verified**. No remote service, public listener, model connection or MCP connection is created by the tests.

## Public API

```rust
use async_trait::async_trait;
use cayw_remote::{CancellationToken, DeviceHandler, RemoteError};
use serde_json::Value;

struct Host;
#[async_trait]
impl DeviceHandler for Host {
    async fn handle(&self, request: Value, cancel: CancellationToken)
        -> Result<Value, RemoteError>
    {
        // Validate request and obtain host permission before effects.
        // Dispatch to the host's existing runtime; no implicit executors.
        if cancel.is_cancelled() { return Err(RemoteError::Cancelled); }
        Ok(request)
    }
}
```

- `ServerState::open(path: impl AsRef<Path>, operator_token: Secret, config: ServerConfig) -> Result<ServerState, RemoteError>` opens a single-owner SQLite store. Reopening requires the same explicit operator token. A second concurrent owner is rejected.
- `serve(listener: tokio::net::TcpListener, state: ServerState, cancel: CancellationToken) -> Result<(), RemoteError>` serves an already-bound listener. Cancellation and dropping its Future close Device sessions and mark unresolved requests unknown.
- `ServerUrl::new(value: &str, tls_terminated: bool) -> Result<ServerUrl, RemoteError>` validates an explicit `ws://` or `wss://` origin. Base paths, user information, query strings and fragments are rejected. Loopback means a literal loopback IP, not an arbitrary hostname.
- `Secret::generate()` uses 32 random bytes from the operating system. `Secret::new()` accepts a header-safe credential with 32–256 characters; callers supplying their own value must provide at least 256 bits of entropy. `Debug` redacts credentials; `expose()` is an explicit host-storage escape hatch.
- `OperatorClient::new(ServerUrl, Secret)` and `with_limits(Limits)` construct an explicitly authenticated operator client. `pair(Duration) -> PairingCode`, `device_status(&str) -> DeviceStatus`, `invoke(device, request_id, Value) -> RequestRecord`, `status(device, request_id) -> RequestRecord`, `cancel(device, request_id) -> RequestRecord`, and `revoke(device) -> ()` are async and return `Result<_, RemoteError>`.
- `PendingBind::new(pairing_code: Secret) -> Result<PendingBind, RemoteError>` creates a random `bind_id` and Device secret. The Device must persist `bind_id`, code and secret **before** calling `complete(&ServerUrl) -> Result<DeviceCredential, RemoteError>`. A repeated handshake with those exact values recovers the same Device during the recovery TTL. A different secret/code or reused code cannot create another identity.
- `RemoteDevice::run(&ServerUrl, &DeviceCredential, Arc<dyn DeviceHandler>, CancellationToken) -> Result<(), RemoteError>` makes one outbound connection attempt. `run_with_limits(..., Limits)` offers frame size, active request count, queue capacity and per-connection deduplication capacity during the version 1 handshake. The server admits work using the agreed minima. Invalid offers cannot replace a working connection. A host may call it again to reconnect, but it never replays old requests.

The Rust method signatures and `DeviceHandler` contract are unchanged. For direct protocol integrations, hello now requires `max_frame_bytes`, `max_inflight`, `max_records` and `queue_capacity`; welcome returns those four effective limits plus the server's heartbeat timings. This is the single canonical version 1 handshake; a hello missing the resource offer is rejected. A negotiated per-connection `max_records` exhaustion rejects new IDs with `Busy`; existing durable IDs remain queryable and never execute again. A deliberate fresh connection has an empty in-memory deduplication set and the server's durable deduplication still applies.

The operator token controls every Device in this single-operator store. There is no multi-tenant operator access model. Device credentials authenticate only the named Device: they cannot invoke operator routes or return results for another Device. The Server never returns a Device secret to the operator. The Device creates its own secret; a successful bind response contains only the assigned identity.

## Run locally

Commands below run from the repository root with Rust 1.95. Paths containing credentials must be in a private directory; on Unix the example creates its credential files with mode 0600. On Windows the host must supply a directory with a private ACL. Protect the SQLite file and its directory too: it contains request result data even though credentials are only stored as hashes.

Build and validate:

```sh
cargo +1.95.0 build -p cayw-remote --bin cayw-remote-server --example device --locked
cargo +1.95.0 test -p cayw-remote --locked
cargo +1.95.0 test -p cayw-remote --test binaries --locked -- --ignored
cargo +1.95.0 clippy -p cayw-remote --all-targets --locked -- -D warnings
cargo +1.95.0 fmt -p cayw-remote -- --check
```

`binaries` is explicitly run after building the Device example, so ordinary clean workspace tests do not depend on a preexisting example executable. It starts actual Server and Device processes using temporary files and ephemeral loopback ports, invokes an echo, restarts the Device, cancels a request, then revokes it. Child processes are stopped on test teardown.

In a Server terminal, supply a random 64-character hexadecimal operator token through a secret manager or environment. Do not put credentials in command-line arguments or URLs.

```sh
export CAYW_REMOTE_OPERATOR_TOKEN="$(openssl rand -hex 32)"
export CAYW_REMOTE_DATABASE=/private/path/remote.sqlite
export CAYW_REMOTE_LISTEN=127.0.0.1:8787
export CAYW_REMOTE_PUBLIC_URL=ws://127.0.0.1:8787
cargo +1.95.0 run -p cayw-remote --bin cayw-remote-server --locked -- serve
```

Keep the same token when restarting this database. There is no implicit credential reset or token rotation.

In an operator terminal, supply the same operator token securely and create a pairing code file. Only the pairing code goes to the Device, never the operator token.

```sh
export CAYW_REMOTE_SERVER_URL=ws://127.0.0.1:8787
export CAYW_REMOTE_PAIR_FILE=/private/path/pair-code
cargo +1.95.0 run -p cayw-remote --bin cayw-remote-server --locked -- pair
```

Transfer that file over an existing secure channel. In a Device terminal, load the code without displaying it and start the outbound client:

```sh
export CAYW_REMOTE_SERVER_URL=ws://127.0.0.1:8787
export CAYW_REMOTE_PAIR_CODE="$(cat /private/path/pair-code)"
export CAYW_REMOTE_PENDING_FILE=/private/path/device-pending.json
cargo +1.95.0 run -p cayw-remote --example device --locked
```

The example prints only `device_id=...`. It stores the confirmed ID beside the pending handshake, as `device-pending.id`. Subsequent starts read the ID and secret from those files without repeating the expired bind handshake. If the confirmation was lost, start again with the same pending file before the recovery TTL expires. After that TTL, request an explicit new pairing; the old `bind_id` stays reserved and is never repurposed. `CAYW_REMOTE_DEVICE_ID` can explicitly supply a known confirmed identity, and mismatches with a saved identity are rejected.

Set `DEVICE_ID` to the non-secret printed identity. Create an input JSON file, then use the operator commands:

```sh
cargo +1.95.0 run -p cayw-remote --bin cayw-remote-server --locked -- device-status "$DEVICE_ID"
cargo +1.95.0 run -p cayw-remote --bin cayw-remote-server --locked -- invoke "$DEVICE_ID" request-001 /private/path/input.json
cargo +1.95.0 run -p cayw-remote --bin cayw-remote-server --locked -- status "$DEVICE_ID" request-001
cargo +1.95.0 run -p cayw-remote --bin cayw-remote-server --locked -- cancel "$DEVICE_ID" request-001
cargo +1.95.0 run -p cayw-remote --bin cayw-remote-server --locked -- revoke "$DEVICE_ID"
```

The Device example echoes JSON as `{"device_reply": ...}`. Input `{"wait_for_cancel":true}` waits for an explicit cancel command and returns a known cancelled outcome. It does not run commands, tools or models. All CLI errors are fixed messages without transport error bodies or credentials. Successful request results are printed intentionally for the operator; handlers must decide what result content is appropriate to return.

## Protocol and failure handling

All three routes use a versioned JSON envelope over WebSocket:

```json
{"version":1,"request_id":"request-001","device_id":"assigned-device-id","generation":1,"body":{"type":"execute","payload":{"operation":"host-defined"}}}
```

- `/v1/bind` accepts only a bind handshake. `request_id == bind_id`, empty `device_id`, generation 0. Codes are random, single-use and expiring; the default CLI pairing TTL is five minutes. The bind recovery TTL is ten minutes by default and is separate from the initial code TTL.
- `/v1/operator` requires `Authorization: Bearer <operator-token>`. One command and one response per connection. It accepts pair, invoke, status, cancel, device_status and revoke. Generation 0 on an operator command selects the current connection; it is never a Device execution generation.
- `/v1/device` requires the independent Device secret in `Authorization` and `x-device-id`. It first sends hello at generation 0 and receives welcome with the assigned persistent monotonically increasing generation and negotiated limits. Only that active generation may deliver outcomes. A valid new connection fences and closes the old one. Outcome settlement checks the exact request deadline, active generation and revocation together under the server state lock, independently of the maintenance tick. Results arriving after timeout, revocation, disconnection or generation replacement cannot overwrite the terminal record.

The hello body is `{"type":"hello","max_frame_bytes":65536,"max_inflight":16,"max_records":100000,"queue_capacity":32}`. Welcome uses the same resource field names and adds `heartbeat_interval_ms` and `heartbeat_timeout_ms`; `max_inflight` is the wire name for `Limits::max_inflight_per_device`. The server validates the offer before creating a session and stores effective limits before operators can submit work. Payloads too large for the Device return `TooLarge` before a running record is committed. Excess active requests or exhausted per-connection record capacity return `Busy`, leave no durable record for the rejected ID, and preserve existing execution.

SQLite uses transactions, FULL synchronous commits and an exclusive lifetime lock. Bind consumes the code and creates the Device in one transaction. Credential and pairing-code plaintext is not stored. Request payloads are not stored; their deterministic JSON SHA-256 hashes and durable results are. The durable request key is `(device_id, request_id)`. The same ID/content returns the original record; different content returns `Conflict`. Tombstones are never silently evicted or reset. Keep the database across restarts; deleting/replacing it discards identities, revocations and deduplication history.

`invoke` returns a durable running record, not an execution completion. Poll `status`. Request states are `running`, `cancel_requested`, `completed`, `failed`, `cancelled`, and `outcome_unknown`. A cancel request is not proof of rollback. The Device handler must return `Cancelled` only for a known cancellation, and `OutcomeUnknown` whenever effects cannot be established. Disconnect, heartbeat loss, deadline expiry, Server restart with unfinished intent, revoked active work, handler panic and an oversized executed result become unknown. No client automatically resends a request after a transport error. Query using the same Device/ID; an unknown record never executes again. A missing record after a transport failure is not permission for a client to choose a fresh ID and duplicate effects.

`Limits` default to a 64 KiB complete frame/message limit, queue 32, 16 active requests per Device, 128 upgraded connections, 100,000 rows per retained record category, 60-second request timeout, 10-second handshake/write timeout, and heartbeat every 10 seconds with 30-second loss timeout. Full queues/connection slots/inflight budgets return Busy or HTTP 503. Record capacity exhaustion rejects new IDs instead of deleting replay protection. Configurations with excessive counts, zero durations or durations beyond 24 hours are rejected. Heartbeat durations must be exactly representable as nonzero whole milliseconds; 500 microseconds and fractional milliseconds are rejected at configuration entry rather than truncated on the wire. Host handlers must independently bound their own allocations and external operations.

The Device reads independently from a bounded result writer, so a valid large result blocked by a slow peer cannot stop receipt of protocol cancellation. Queued results retain execution slots until written. Deadline expiry records uncertainty but does not free the Device's execution capacity until its outcome arrives or its connection ends, since a host handler may still be running. Local cancellation, malformed input, disconnect, heartbeat loss and writer failure cancel all owned handler tokens and reclaim the connection, queues and handler tasks. Termination drops both socket halves directly instead of awaiting another blocked close handshake. Cooperative handlers get a scheduling turn to observe cancellation; handlers that do not yield are then aborted and joined. Dropping the connection Future also cancels tokens and drops owned resources. No writer task is detached and no queued result is replayed.

Dropping a handler Future or cancelling a token does not prove that a remote effect was reversed. The host must reconcile unknown effects through its runtime permission/session mechanism. This crate keeps that request unknown and does not provide an unsafe reset/replay command.

## TLS, origins and deployment boundary

Default is literal loopback `ws://127.0.0.1:8787`. Remote clients use an explicit `wss://relay.example` URL with normal certificate and hostname validation via rustls/WebPKI. No invalid-certificate switch is provided and redirects are not followed. Credentials cannot be placed in URLs, URL queries or fragments.

The included binary is an HTTP/WebSocket listener and does not terminate TLS. To expose it beyond loopback, an operator must independently provision an existing TLS reverse proxy or secure transport boundary. `CAYW_REMOTE_TLS_TERMINATED=1` (or `ServerConfig::tls_terminated`) is an explicit declaration of that existing boundary; it does not create encryption. `CAYW_REMOTE_PUBLIC_URL` is the advertised origin, separate from `CAYW_REMOTE_LISTEN`. Clients use `CAYW_REMOTE_SERVER_URL` explicitly. The public TLS proxy must pass WebSocket upgrades, preserve Authorization and x-device-id without logging them, and prevent direct unencrypted access to the backend. Non-loopback cleartext `ws://` URLs are refused unless the same existing-TLS declaration is explicit; use that exception only for an internal hop already protected by the deployment boundary.

No forwarded host/proto/client-IP headers are trusted. This implementation has no trusted-proxy allowlist because it never consumes proxy identity claims. Incoming Origin is rejected by default. `CAYW_REMOTE_ALLOWED_ORIGIN` / `allowed_origin` permits one exact Origin value; native clients send none. TLS proxy/origin policy and ingress connection limits before WebSocket upgrade belong to the deployment operator. No automatic tunnels, port forwarding, public addresses or remote services are configured.

## Verification and remaining acceptance

The reproducible package checks run on macOS arm64 with Rust 1.95.0 using the repository workspace manifest. The regression suite includes the exact-deadline settlement unit test, 28 tests in `tests/loopback.rs` and seven real-socket backpressure tests in `tests/transport.rs`. The process acceptance in `tests/binaries.rs` is run explicitly after building the binary and example. All listeners in those tests are ephemeral loopback listeners. No Windows, Linux, independent-machine, TLS handshake/proxy, real-model or real-tool acceptance is implied.

Covered cases: initial binding, discarded confirmation/idempotent recovery, one-use/expired pairing codes, bind TTL, authenticated outbound connection, forwarding and successful null result, request deduplication/content conflicts, cancel reaching a real handler, Device drop/disconnect unknown, no replay after reconnect, generation fencing/late outcomes, exact deadline fencing without maintenance, immutable terminal records, online/offline revocation and Server restart, restart recovery of committed unfinished intent, raw credentials absent from durable storage and Debug, oversize frames/results, incompatible protocol, cross-Device credentials/outcome forgery with intervening heartbeat frames, heartbeat loss, server Future drop, handshake cancellation, negotiated inflight/frame/record capacity and rejection before admission, invalid offers preserving a working session, execution capacity retained after deadline, nonrepresentable heartbeat rejection, and rejection of HTTP redirects without forwarding credentials. Backpressure tests use a valid 12 MiB result and an unread peer to verify local/protocol cancellation, malformed frames, disconnect, heartbeat loss, writer timeout and owner Future drop, including handler/token cleanup and socket EOF. Process acceptance also exercises the actual CLI and restart using saved Device identity.

Issue 15 remains unaccepted until two independent hosts execute the same host runtime tool contract, stream progress, cancel and revoke through a real TLS boundary while collecting source/build identity, both endpoint addresses, TLS configuration, execution/permission identity, input and raw event evidence. The current handler API returns one final JSON value; runtime streaming adaptation and a dedicated two-machine evidence runner are follow-up integration work. There is no credential rotation, multi-operator tenancy, database pruning, remote encrypted synchronization or production proxy deployment here.

Direct dependencies use Apache-2.0-compatible licenses: axum, tokio-tungstenite, rusqlite, tokio, tokio-util, async-trait and thiserror are MIT; serde/serde_json, futures, sha2, getrandom, url and tempfile offer MIT and/or Apache-2.0. Bundled SQLite is public-domain software; rustls is Apache-2.0/ISC/MIT and the WebPKI root bundle has its own redistribution terms. These dependencies are linked normally; no third-party implementation source is copied into this crate.
