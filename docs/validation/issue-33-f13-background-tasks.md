# #33 SLICE P / F13 验收记录（源码级）

**分支**：`feat/f13-background-tasks`  
**基线**：`origin/dev` @ `9cc509d`  
**范围**：持久后台命令、日志索引、完成通知、结果归属 Session；网关 OwnershipStore 重启恢复。

## 命令

```sh
cargo test -p cayw-runtime --lib background:: --locked
cargo test -p cayw-runtime --test background --locked
cargo test -p cayw-webui ownership --locked
cargo fmt -p cayw-runtime -p cayw-webui -- --check
```

## 期望

1. `bg.run` 立即返回 id；profile 子进程在 Runtime 存活期间不因浏览器断开而停。
2. 完成后 Session 事件含 `background_task_completed`；`bg.result` / host inspect 仍绑定原 `session_id`。
3. Runtime 重开后已完成任务的元数据、日志与 Session 归属可恢复；仍为 `running` 的行标记 `runtime_restarted`（不宣称跨进程领养）。
4. `OwnershipStore` 写入 Session/Task→device 后，进程重开仍可读回。

## 未宣称

- macOS 真机浏览器关断 + 常驻设备 daemon 全链
- F13 复选框全范围完成
