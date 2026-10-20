use crate::{
    measurement::{ProcessPeakRss, elapsed_ns, process_peak_rss},
    provenance,
};
use cayw_core::EventRecord;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
    time::Instant,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Observation {
    pub sequence: usize,
    pub elapsed_ns: u64,
    pub kind: String,
    pub value: Value,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct EventPage {
    pub stage: String,
    pub session_id: String,
    pub after: u64,
    pub records: Vec<EventRecord>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct ScenarioReport {
    pub schema_version: u32,
    pub sample_id: String,
    pub scenario: String,
    pub iteration: u32,
    pub source: String,
    pub process_id: u32,
    pub status: String,
    pub binding: Value,
    pub specification: Value,
    pub observations: Vec<Observation>,
    pub event_pages: Vec<EventPage>,
    pub checks: Vec<Check>,
    pub task_checks: Vec<String>,
    pub timing_ns: BTreeMap<String, u64>,
    pub process_peak_rss: Option<ProcessPeakRss>,
    pub limitations: Vec<String>,
    pub artifacts: Value,
    #[serde(skip)]
    records: Option<File>,
    #[serde(skip)]
    disposition: Option<String>,
    #[serde(skip, default = "Instant::now")]
    start: Instant,
}
impl ScenarioReport {
    pub fn new(name: &str, iteration: u32, specification: Value, binding: Value) -> Self {
        Self {
            schema_version: 2,
            sample_id: format!("{iteration:04}/{name}"),
            scenario: name.into(),
            iteration,
            source: "offline synthetic".into(),
            process_id: std::process::id(),
            status: "unverified".into(),
            binding,
            specification,
            observations: vec![],
            event_pages: vec![],
            checks: vec![],
            task_checks: vec![],
            timing_ns: BTreeMap::new(),
            process_peak_rss: None,
            limitations: vec![],
            artifacts: json!({}),
            records: None,
            disposition: None,
            start: Instant::now(),
        }
    }
    pub(crate) fn attach_records(&mut self, path: &Path) -> super::Result<()> {
        self.records = Some(OpenOptions::new().write(true).create_new(true).open(path)?);
        self.observe("scenario_start",json!({"sample_id":self.sample_id,"specification":self.specification,"binding":self.binding}))
    }
    pub fn observe(&mut self, kind: &str, value: impl Serialize) -> super::Result<()> {
        let observation = Observation {
            sequence: self.observations.len() + 1,
            elapsed_ns: elapsed_ns(self.start),
            kind: kind.into(),
            value: serde_json::to_value(value)?,
        };
        if let Some(file) = &mut self.records {
            serde_json::to_writer(&mut *file, &observation)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
        }
        self.observations.push(observation);
        Ok(())
    }
    pub fn event_page(&mut self, page: EventPage) -> super::Result<()> {
        self.observe("event_page", &page)?;
        self.event_pages.push(page);
        Ok(())
    }
    pub(crate) fn persist_terminal(&mut self) -> super::Result<()> {
        self.observe(
            "scenario_terminal",
            json!({"status":self.status,"checks":self.checks,"task_checks":self.task_checks,"timing_ns":self.timing_ns,
            "process_peak_rss":self.process_peak_rss,"artifacts":self.artifacts}),
        )?;
        Ok(())
    }
    pub(crate) fn bind_records(&mut self, path: &Path) -> super::Result<()> {
        self.artifacts["records"] =
            json!({"path":"records.jsonl","sha256":crate::measurement::sha256(&fs::read(path)?)});
        Ok(())
    }
    pub fn unsupported(&mut self, reason: &str) -> super::Result<()> {
        self.disposition = Some("unsupported".into());
        self.limitations.push(reason.into());
        self.observe("unsupported", json!({"reason":reason}))
    }
    pub fn unverified(&mut self, reason: &str) -> super::Result<()> {
        self.disposition = Some("unverified".into());
        self.limitations.push(reason.into());
        self.observe("unverified", json!({"reason":reason}))
    }
    pub fn check(&mut self, name: &str, passed: bool, detail: impl Into<String>) {
        self.checks.push(Check {
            name: name.into(),
            passed,
            detail: detail.into(),
        });
    }
    /// A scenario must check its concrete task result, separately from cleanup.
    /// The caller is responsible for the task-specific meaning of this check.
    pub fn task_check(&mut self, name: &str, passed: bool, detail: impl Into<String>) {
        self.task_checks.push(name.into());
        self.check(name, passed, detail);
    }
    pub fn has_task_checks(&self) -> bool {
        let mut names = std::collections::BTreeSet::new();
        !self.task_checks.is_empty()
            && self.task_checks.iter().all(|name| {
                names.insert(name)
                    && self
                        .checks
                        .iter()
                        .filter(|check| check.name == *name && check.passed)
                        .count()
                        == 1
            })
    }
    pub fn bind_attachment(
        &mut self,
        name: &str,
        root: &Path,
        relative: &str,
    ) -> super::Result<()> {
        if name.is_empty() || name == "records" || self.artifacts.get(name).is_some() {
            return Err("attachment name is empty, reserved or already registered".into());
        }
        let bytes = crate::evidence::attachment(root, relative)?;
        self.artifacts[name] = json!({"path":relative,"sha256":crate::measurement::sha256(&bytes)});
        Ok(())
    }
    pub fn finish(&mut self) {
        self.timing_ns
            .insert("scenario_total".into(), elapsed_ns(self.start));
        self.process_peak_rss = process_peak_rss();
        self.status = if self.checks.iter().any(|check| !check.passed) {
            "failed"
        } else if let Some(disposition) = &self.disposition {
            disposition.as_str()
        } else if !self.has_task_checks() {
            "unverified"
        } else {
            "passed"
        }
        .into();
    }
}

pub fn local_binding() -> super::Result<Value> {
    Ok(
        json!({"build":provenance::build_binding(), "executable":provenance::executable()?,
        "platform":provenance::platform(), "source_evidence":"listed file bytes measured by the build script; optional post-build receipt checks recorded inputs; build input completeness remains pending",
        "protocol":{"core":cayw_core::PROTOCOL_VERSION,"ipc":cayw_ipc::PROTOCOL_VERSION}}),
    )
}

/// Strict contiguous pages prevent duplicates, skipped starts and cross-session
/// records from passing a saved-cursor reconnect check.
pub fn contiguous_events(
    session: &str,
    after: u64,
    records: &[EventRecord],
    final_cursor: u64,
) -> bool {
    let mut cursor = after;
    for event in records {
        let Some(next) = cursor.checked_add(1) else {
            return false;
        };
        if event.session_id != session
            || event.protocol_version != cayw_core::PROTOCOL_VERSION
            || event.cursor != next
        {
            return false;
        }
        cursor = next;
    }
    cursor == final_cursor
}

#[cfg(test)]
mod tests {
    use super::*;
    use cayw_core::CoreEvent;
    fn event(cursor: u64) -> EventRecord {
        EventRecord {
            protocol_version: cayw_core::PROTOCOL_VERSION,
            session_id: "s".into(),
            cursor,
            timestamp: 0,
            attempt: None,
            event: CoreEvent::SessionCreated {
                label: "fixture".into(),
            },
        }
    }
    #[test]
    fn reconnect_validator_rejects_missing_duplicated_reordered_and_foreign_events() {
        let good = vec![event(8), event(9), event(10)];
        assert!(contiguous_events("s", 7, &good, 10));
        for bad in [
            vec![event(9), event(10)],
            vec![event(8), event(8), event(10)],
            vec![event(8), event(10), event(9)],
            vec![event(8), event(9)],
        ] {
            assert!(!contiguous_events("s", 7, &bad, 10));
        }
        let mut foreign = good.clone();
        foreign[1].session_id = "another".into();
        assert!(!contiguous_events("s", 7, &foreign, 10));
        foreign[1] = event(9);
        foreign[1].protocol_version += 1;
        assert!(!contiguous_events("s", 7, &foreign, 10));
    }
    #[test]
    fn empty_checks_do_not_pass_and_declared_digest_is_never_verified() {
        let mut sample = ScenarioReport::new("empty", 1, json!({}), local_binding().unwrap());
        sample.finish();
        assert_eq!(sample.status, "unverified");
        assert_eq!(
            sample.binding["build"]["source_manifest_verification"],
            provenance::build_binding()["source_manifest_verification"]
        );
        assert_ne!(sample.binding["build"]["status"], "self_verified");
    }
    #[test]
    fn unsupported_and_unverified_never_pass_or_hide_failed_checks() {
        for unsupported in [false, true] {
            let mut sample = ScenarioReport::new("fixture", 1, json!({}), json!({}));
            sample.check("setup", true, "observed setup only");
            if unsupported {
                sample.unsupported("platform unavailable").unwrap();
            } else {
                sample.unverified("not executed").unwrap();
            }
            sample.finish();
            assert_eq!(
                sample.status,
                if unsupported {
                    "unsupported"
                } else {
                    "unverified"
                }
            );
            sample.check("real_failure", false, "must remain failed");
            sample.finish();
            assert_eq!(sample.status, "failed");
        }
    }
}
