use cayw_core::ReconciliationRequest;
use cayw_ipc::Client;
use cayw_runtime::local_cli::LocalPaths;
use serde_json::{Value, json};
use std::{error::Error, path::PathBuf};

const USAGE: &str = "cayw reconcile [--workspace DIR] --inspect SESSION\ncayw reconcile [--workspace DIR] --decision FILE\n\n查看原会话的未知调用和执行记录，再独立核实实际结果。\nFILE 是完整的 ReconciliationRequest JSON：type 为 capability 或 tool，reconciliation 包含原调用身份、actor=rpc-user、已核实的 outcome 和 evidence。\n此命令代表本地操作者确认该精确结果，并在工作区外保存凭据。进程退出本身不能证明副作用成功或未发生。\n提交经唯一 Client 路由；原始未知记录保留，不重做任务。重复相同决定可重试；提交失败时不得假定已恢复。";

pub async fn run(arguments: impl IntoIterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut arguments = arguments.into_iter();
    let (mut workspace, mut inspect, mut decision) = (None, None, None);
    while let Some(flag) = arguments.next() {
        let field = match flag.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--workspace" => &mut workspace,
            "--inspect" => &mut inspect,
            "--decision" => &mut decision,
            _ => return Err(format!("unknown reconcile option {flag}; use --help").into()),
        };
        if field
            .replace(arguments.next().ok_or("missing reconcile option value")?)
            .is_some()
        {
            return Err(format!("duplicate reconcile option {flag}").into());
        }
    }
    if inspect.is_some() == decision.is_some() {
        return Err("choose exactly one of --inspect SESSION or --decision FILE".into());
    }
    let paths = LocalPaths::from_environment(
        workspace
            .map(PathBuf::from)
            .map_or_else(std::env::current_dir, Ok)?,
    )?;
    let request: Option<ReconciliationRequest> = decision
        .map(|file| -> Result<_, Box<dyn Error>> {
            Ok(serde_json::from_slice(&std::fs::read(file)?)?)
        })
        .transpose()?;
    let (address, token) = paths.connect()?;
    let client = Client::connect(address, token).await?;
    let result = async {
        let catalog = client.request("client.workspaces.list", json!({})).await?;
        let workspace_id = catalog["workspaces"].as_array().and_then(|workspaces| workspaces.iter().find_map(|entry| {
            (entry["path"].as_str() == paths.workspace.to_str()).then(|| entry["id"].as_str()).flatten()
        })).ok_or("workspace is not registered; run cayw init --workspace DIR")?;
        client.request("client.workspaces.open", json!({"id":workspace_id})).await?;
        if let Some(request) = &request {
            paths.record_reconciliation(request)?;
        }
        let (method, params) = match &request {
            Some(ReconciliationRequest::Capability { reconciliation }) => {
                ("capability.reconcile", json!({"session_id":reconciliation.expected.lease.session_id,"reconciliation":reconciliation}))
            }
            Some(ReconciliationRequest::Tool { reconciliation }) => {
                ("session.reconcile", json!({"session_id":reconciliation.expected.lease.session_id,"reconciliation":reconciliation}))
            }
            None => ("session.get", json!({"session_id":inspect.expect("validated inspect")})),
        };
        let value = client.request("client.workspace.call", json!({"workspace_id":workspace_id,"rpc":{"method":method,"params":params}})).await?;
        Ok::<Value, Box<dyn Error>>(value)
    }.await;
    client.close().await;
    println!("{}", result?);
    Ok(())
}
