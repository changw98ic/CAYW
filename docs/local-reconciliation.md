# 本地未知结果核对

`outcome_unknown` 表示原操作的副作用还没有确定结果。`session.recover` 核对停止和清理，不能代替结果核对。普通对话可以继续；持有未知写入记录的工作区需要核实该原操作后才能继续写入。

本地命令连接设备上唯一的 Client，再路由到所选工作区。它不另开 Runtime，不重做旧任务。

```sh
cayw reconcile --workspace /absolute/project --inspect SESSION_ID
cayw reconcile --workspace /absolute/project --decision /absolute/verified-decision.json
```

第一条命令只读取会话。检查 `unresolved_capabilities`、`unresolved_calls` 和 `executions`，并独立检查实际文件或外部系统回执。进程退出只证明它停止了，不能证明写入成功或未发生。

第二条命令代表本地操作者确认文件中的精确决定。以下为子能力核对文件的结构。身份字段须从同一个原调用复制；结果和证据须按实际核对情况填写。

```json
{
  "type": "capability",
  "reconciliation": {
    "key": {"execution_id": "原执行ID", "sequence": 0},
    "expected": {
      "lease": {
        "session_id": "原会话ID",
        "attempt_id": "原尝试ID",
        "worker_id": "原执行者ID",
        "fence": 1
      },
      "task_id": "原任务ID",
      "execution_id": "原执行ID",
      "parent_call_id": "原父调用ID",
      "source_hash": "原源码摘要"
    },
    "actor": "rpc-user",
    "evidence": "独立核对的方法、原始回执或文件摘要，以及实际发生的副作用",
    "outcome": {"type": "failed", "error": "已核实的失败及其副作用情况"},
    "external_provenance": null
  }
}
```

`key.sequence` 从 0 开始。`external_provenance` 也须从原调用复制；原值不为空时不能改成 `null`。`outcome` 使用现有 `CapabilityOutcome` 的已知结果：`completed`、`failed` 或 `not_executed`。只有确认未执行时才使用 `not_executed`。外层模型工具的核对使用 `type: "tool"` 和现有 `Reconciliation` 结构；子能力已经执行时不能把父调用确认为未执行。

命令在 `<CAYW_HOME>/workspaces/<目录摘要>/reconciliation-<决定摘要>.json` 保存完整决定，复用现有私有文件和原子保存规则。本地 Client 只接受与这份记录完全一致的核对。接口继续将 actor 固定为 `rpc-user`；这个名称本身不授予权限。模型的工作区文件能力不能写入私有确认目录，确认操作也不暴露为模型工具或网页接口。

Runtime 继续检查原任务、完整执行身份、原始来源、执行者停止、Cells 记录和进程清理。原未知事件保持不变，核对作为新事件追加。相同决定可重复提交；如果回执丢失，重提同一个决定即可核对结果，不重新发起原任务。损坏凭据或提交失败会明确报错，不能据此认为写入已恢复。

SDK 和完全显式 Headless 不读取这些本地凭据，仍由宿主注入自己的 `ReconciliationVerifier`。本命令不提供崩溃后丢失进程监管者的清理确认；该情况仍需可信宿主检查原进程身份并完成现有清理流程。
