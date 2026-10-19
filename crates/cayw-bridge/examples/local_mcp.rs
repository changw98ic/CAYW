//! Run with an explicit absolute fixture executable, e.g. after `cargo build -p
//! cayw-bridge --bin cayw-bridge-local-fixture`, pass the absolute target/debug path.
mod support;
use cayw_ai::CancellationToken;
use cayw_bridge::{Limits, Registry};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
};
use support::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let executable = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("supply absolute fixture executable path")?,
    );
    let workspace = Workspace::new();
    let stats = Arc::new(Stats::default());
    let client = LocalClient::spawn(&executable, &workspace.0, stats.clone()).await?;
    let manifest = client.manifest().await?;
    let registry = Registry::new(Limits::default(), Arc::new(Policy::default()))?;
    registry.register(identity("fixture-1"), manifest, Box::new(client.handle()))?;
    let gate = Arc::new(Gate::default());
    let audit = Arc::new(Audit::default());
    // Two independent CodeMode consumers, one registry, one initialized child.
    let first = mode(host(
        &workspace.0,
        consumer(&registry),
        gate.clone(),
        audit.clone(),
    ));
    let second = mode(host(
        &workspace.0,
        consumer(&registry),
        gate.clone(),
        audit.clone(),
    ));
    let discovery = first
        .execute(
            r#"
        local tools = cayw.call("mcp.discover", { server = "local" })
        local description = cayw.call("mcp.describe", { server = "local", tool = "add" })
        local skills = cayw.call("skills.list", {})
        local skill = cayw.call("skills.read", { name = "local/addition" })
        return { tools = tools, description = description, skills = skills, skill = skill }
    "#,
            CancellationToken::new(),
        )
        .await?;
    let execution = second.execute(r#"
        return cayw.call("mcp.call", { server = "local", tool = "add", arguments = { a = 2, b = 3 } })
    "#, CancellationToken::new()).await?;
    assert_eq!(execution.return_value["value"]["sum"], 5);
    assert_eq!(stats.spawned.load(Ordering::Relaxed), 1);
    assert_eq!(stats.initializations.load(Ordering::Relaxed), 1);
    assert_eq!(stats.skill_reads.load(Ordering::Relaxed), 1);
    println!(
        "{}",
        serde_json::json!({
            "protocol":PROTOCOL, "connections":stats.spawned.load(Ordering::Relaxed),
            "initializations":stats.initializations.load(Ordering::Relaxed),
            "discovery":discovery.return_value, "execution":execution.return_value,
            "host_gate_calls":gate.calls.lock().unwrap().clone(),
            "audit":audit.records.lock().unwrap().clone(),
            "scope":"explicit local stdio fixture; fixed application-supplied skill handle; in-memory demonstration audit"
        })
    );
    registry.disconnect("local")?;
    client.wait_closed().await;
    Ok(())
}
