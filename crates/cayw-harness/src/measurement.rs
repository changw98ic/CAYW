use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Instant;

pub fn elapsed_ns(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}
pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProcessPeakRss {
    pub value: u64,
    pub unit: String,
    pub method: String,
    pub scope: String,
    pub process_id: u32,
}

pub fn process_peak_rss() -> Option<ProcessPeakRss> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: usage points to a writable, correctly aligned rusage. We read it
        // only after getrusage succeeds; zeroing also initializes padding/extra fields.
        let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
        if result != 0 {
            return None;
        }
        // SAFETY: getrusage returned success and filled this zero-initialized value.
        let usage = unsafe { usage.assume_init() };
        let value = u64::try_from(usage.ru_maxrss).ok()?;
        Some(ProcessPeakRss {
            value,
            unit: if cfg!(target_os = "macos") { "bytes" } else { "KiB" }.into(),
            method: "getrusage(RUSAGE_SELF).ru_maxrss".into(),
            scope: "process lifetime high-water mark, including setup and earlier samples; not per-task RSS or a delta".into(),
            process_id: std::process::id(),
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}
