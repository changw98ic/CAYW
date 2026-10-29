# Agent event golden fixture

`agent_events.golden.json` is a fixed historical serialization of all ten
`AgentEvent` variants plus `ToolResult` values covering `outcome_unknown` and the
optional fields. It pins field names and bytes so the relocation of the
definitions into `cayw-agent-types` cannot change the contract silently.
`crates/cayw-agent/tests/golden_events.rs` only reads it.

Do not regenerate it from the moved tree. The committed tests must always
compare against the frozen bytes.

## Maintenance (maintainer only)

The fixture was generated once from the frozen pre-move baseline (`e738dae`).
The exact generator is retained at
[`docs/validation/refactor-fixture-generators-20261029/golden_events.rs`](../../../../docs/validation/refactor-fixture-generators-20261029/golden_events.rs).
The [generator README](../../../../docs/validation/refactor-fixture-generators-20261029/README.md)
records its hash and how to run it in a separate copy of the old commit.
It is outside Cargo's test folders and is not part of the normal test suite.
The original generation command below records that historical run; it is not
a regeneration command for the current tree. An intentional contract change
needs a separately reviewed sample. Never overwrite this sample during a
normal test run.

## Provenance

- Original HEAD: `e738dae1af73bf0ad3fe15f729a4557a2d9f4d24`
- File: `agent_events.golden.json`
- Bytes: 7254
- SHA-256: `0e61257ea651329423c7532263daf17514d0c3008579bf30800e83f0f74eedc8`
- Contents: all ten `AgentEvent` variants plus two `ToolResult` values, one with
  `outcome_unknown` and one with the optional `details`, `structured_content`,
  `usage` and `terminate` fields.
- Generation command (sealed baseline copy):
  `cargo test -p cayw-agent --test golden_events -- --ignored generate_golden_events`
