# Core v3 storage sample

`v3-sample.sqlite` is a real Core v3 database produced through the public
`SessionStore` + `Agent` API from the frozen pre-move baseline (`e738dae`). It
contains one completed session with a persisted tool call and the full event
log. `crates/cayw-core/tests/v3_sample.rs` copies it, opens the copy, reads the
events and calls `recover`; the checked-in file is never opened in place.

Do not regenerate it from the moved tree. It is read-only evidence that Core v3
bytes remain readable without migration.

## Maintenance (maintainer only)

The exact generator is retained at
[`docs/validation/refactor-fixture-generators-20261029/v3_sample.rs`](../../../../docs/validation/refactor-fixture-generators-20261029/v3_sample.rs).
The [generator README](../../../../docs/validation/refactor-fixture-generators-20261029/README.md)
records its hash and how to run it in a separate copy of the old commit.
It is outside Cargo's test folders and is not part of the normal test suite.
The original generation command below records that historical run; it is not
a regeneration command for the current tree. An intentional contract change
needs a separately reviewed sample. Never overwrite this sample during a
normal test run.

## Provenance

- Original HEAD: `e738dae1af73bf0ad3fe15f729a4557a2d9f4d24`
- File: `v3-sample.sqlite`
- Bytes: 57344
- SHA-256: `fe3d760f6c2b1d7b9ac0aca0a5163a93b19f8f3ffb1655093fc73d5ae6000532`
- SQLite `user_version`: 3
- Event count: 22
- Contents: one completed session with a persisted tool call and the full event
  log, written through the public `SessionStore` + `Agent` API.
- Generation command (sealed baseline copy):
  `cargo test -p cayw-core --test v3_sample -- --ignored generate_v3_sample`
