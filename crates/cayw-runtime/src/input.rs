//! Canonical task input. Text and images are persisted together so a queued
//! task cannot lose or reorder media between enqueue and execution.

pub use cayw_queue::{ImageInput, TaskInput};
