# Independent verifyTool client

This package is a separate Cargo workspace with its own lockfile. It depends on the
local public verifyTool and Runtime crates. Runtime's default features are disabled;
the example does not enable plugins or remote services.

The custom `Scenario` creates a real Runtime session, drops Runtime, then opens the
same database and queries the session. It repeats this task three times in independent
directories. Each sample keeps its actual session snapshots, task list and creation
events in a hashed `result.json` attachment. No model is called. The fixed offline
provider is injected only to satisfy Runtime's explicit provider boundary.

## Build and run

From the repository root, use Rust/Cargo 1.95 or newer. This example is validated only
on the macOS host where it is run. A short external `TMPDIR` avoids long local socket
paths in Runtime. The output parent must exist; every output directory must be new.

```sh
cargo +1.95.0 build --manifest-path examples/verify-tool-client/Cargo.toml --locked --offline --message-format=json
# With CARGO_TARGET_DIR set to an independent absolute directory:
"$CARGO_TARGET_DIR/debug/verify-tool-client" run /tmp/new-client-results
"$CARGO_TARGET_DIR/debug/verify-tool-client" read /tmp/new-client-results
"$CARGO_TARGET_DIR/debug/verify-tool-client" compare /tmp/new-client-results /tmp/new-client-results
```

`run` exits successfully only if all three concrete task checks pass. `wrong-result
NEW_OUT [RECEIPT]` executes the same real task with a deliberately incorrect expected
label; it must exit unsuccessfully and retain the failed checks. `read` uses public
`Bundle::read`: a valid report can contain failed task results. `compare` uses public
`evidence::compare`; it exits unsuccessfully for `pending`, `invalid` and `incomparable`.
A valid report without complete build-input evidence remains `pending`, including a
comparison with itself.

## Post-build receipt

`snapshot SOURCE_ROOT PATHS_JSON NEW_BEFORE_JSON` calls public `snapshot_sources`.
`PATHS_JSON` is an explicit JSON array of source paths relative to the physical CAYW
root (or absolute paths beneath it). It must cover the actual consumer and dependency
inputs. It is a measured list, not a proof that all compiler inputs were selected.

Compile once to obtain the snapshot command. Then prepare an explicit source manifest
for verifyTool's build script (`cayw.build-source.v2`), including the consumer's
`Cargo.toml`, `Cargo.lock`, `src/main.rs` and this README, the root manifest/lock and
actual dependency inputs. Its `root` must be the physical CAYW root. Record source
bytes before a final build into a fresh external target directory. Save the successful
Cargo command's JSON stdout and exit status separately:

```sh
"$BOOTSTRAP_CLIENT" snapshot "$CAYW_ROOT" "$PATHS_JSON" "$BEFORE_JSON"
CAYW_SOURCE_MANIFEST_PATH="$SOURCE_MANIFEST" cargo +1.95.0 build \
  --manifest-path "$CAYW_ROOT/examples/verify-tool-client/Cargo.toml" \
  --target-dir "$FINAL_TARGET" --locked --offline --message-format=json \
  > "$CARGO_JSONL" 2> "$CARGO_STDERR"
"$FINAL_TARGET/debug/verify-tool-client" capture "$CONTEXT_JSON" "$BEFORE_JSON" \
  "$CARGO_JSONL" "$NEW_RECEIPT_JSON" "$FINAL_TARGET/debug/verify-tool-client.d"
"$FINAL_TARGET/debug/verify-tool-client" run "$NEW_OUT" "$NEW_RECEIPT_JSON"
```

`CONTEXT_JSON` is a serialized public `BuildContext`. Supply the actual absolute
`workspace_root` (physical CAYW root), `package_manifest` (this consumer's manifest),
`dep_info_base`, `target_directory`, target triple, Rust compiler version and the
non-secret `cargo_arguments` used for that build. Only named files are read; capture
does not inspect credentials, environment settings or configuration directories.

`capture CONTEXT BEFORE CARGO_JSONL NEW_RECEIPT DEP_INFO...` reads explicit public
Cargo records and `.d` files, calls `capture_receipt`, then `verify_current_receipt`,
and writes a new receipt. It requires an actual executable artifact for this running
consumer, consistent source bytes before and after compilation, and agreement with
the file measurements embedded by verifyTool's own build script. Its JSON output
states the remaining gaps. Save Cargo's exit status: stdout alone cannot establish
that the Cargo process succeeded.

`run NEW_OUT [RECEIPT]` passes the optional receipt through public `sdk::Options`.
Missing inputs remain pending. A missing declared receipt, changed receipt bytes,
wrong executable, wrong source root or different compiler-time file bytes are rejected.
The receipt never claims all build-script, procedural-macro, native-tool or linker
inputs were traced. Even with its known inputs verified, build-input completeness and
performance comparison remain pending. The example does not contact a real Provider
and is not whole-Issue acceptance evidence.
