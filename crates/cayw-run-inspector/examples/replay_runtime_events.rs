//! Inspect an existing Core event array or Trace document without running tools.
use cayw_core::EventRecord;
use cayw_run_inspector::inspect_runtime_events;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(untagged)]
enum Capture {
    Core(Vec<EventRecord>),
    Trace { events: Vec<EventRecord> },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = args.next().ok_or("expected capture path and session id")?;
    let session = args.next().ok_or("expected session id")?;
    if args.next().is_some() {
        return Err("expected exactly capture path and session id".into());
    }
    let session = session.to_str().ok_or("session id must be UTF-8")?;
    let capture: Capture = serde_json::from_reader(std::fs::File::open(path)?)?;
    let records = match capture {
        Capture::Core(records) | Capture::Trace { events: records } => records,
    };
    let view = inspect_runtime_events(session, None, None, &records);
    serde_json::to_writer_pretty(std::io::stdout().lock(), &view)?;
    Ok(())
}
