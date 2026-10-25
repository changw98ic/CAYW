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
//! Maze UI, live replay, CodeMode internal spans, source association, and
//! Trace Debug node re-eval wiring are out of scope for this slice.
//!
//! Design reference (not a code transplant):
//! [lamost423/dsh-maze](https://github.com/lamost423/dsh-maze) (formerly
//! dsh-trace-compare). See `docs/sources/f06-run-inspector.md`.
#![forbid(unsafe_code)]

mod compare;
mod paths;
mod timeline;
mod tracks;
mod types;

pub use compare::{AlignedTurn, SessionCompare, TurnLane, compare_sessions};
pub use paths::{RetryPath, build_retry_paths};
pub use timeline::{RunTimeline, build_run_timeline};
pub use tracks::{
    DurationTrack, DurationTrackPoint, build_duration_track, build_duration_track_with_epoch,
    shared_wall_epoch,
};
pub use types::*;

/// Wire / document protocol for this first-cut inspector.
pub const RUN_INSPECTOR_PROTOCOL_VERSION: u32 = 1;
