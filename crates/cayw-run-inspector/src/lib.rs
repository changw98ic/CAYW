//! Run inspector for CAYW execution insight (F06 first cut).
//!
//! Builds read-only views from native run-step snapshots: failure/retry
//! recovery paths, duration tracks, and cross-session timeline comparison.
//! This crate does **not** invent outcomes, call an LLM, or rewrite durable
//! evidence. Missing timing or evidence cursors stay empty.
//!
//! First cut (F06 / SLICE G):
//! - retry / recovery path folding (`SameParamsRetry` / `ChangedParams` /
//!   `ChangedTool` / `Unrecovered`)
//! - per-session duration tracks (and wall-aligned tracks for compare)
//! - session A/B turn-aligned comparison when `task_fingerprint` matches
//!
//! Runtime / Trace `EventRecord` → [`RunStep`] translation lives in
//! [`from_runtime`]. Maze UI, live replay, CodeMode internal spans, source
//! association, and Trace Debug node re-eval wiring stay out of scope.
//!
//! Design reference (not a code transplant):
//! [lamost423/dsh-maze](https://github.com/lamost423/dsh-maze) (formerly
//! dsh-trace-compare). See `docs/sources/f06-run-inspector.md`.
#![forbid(unsafe_code)]

mod compare;
mod from_runtime;
mod paths;
mod timeline;
mod tracks;
mod types;

pub use compare::{AlignedTurn, SessionCompare, TurnLane, compare_sessions};
pub use from_runtime::{
    RunInspectorView, inspect_runtime_events, params_digest, snapshot_from_runtime_events,
    snapshots_from_runtime_events, timelines_from_runtime_events,
};
pub use paths::{RetryPath, build_retry_paths};
pub use timeline::{RunTimeline, build_run_timeline};
pub use tracks::{
    DurationTrack, DurationTrackPoint, build_duration_track, build_duration_track_with_epoch,
    shared_wall_epoch,
};
pub use types::*;

/// Wire / document protocol for this first-cut inspector.
pub const RUN_INSPECTOR_PROTOCOL_VERSION: u32 = 1;
