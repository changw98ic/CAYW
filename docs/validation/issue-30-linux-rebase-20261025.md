# Issue #30：Linux 变基与 Host 目录同步修复

日期：2026-10-25（America/New_York）。范围：将 `issue/30-visual-editor` 变基到 `dev` tip `ac17893`，并在 Linux x86_64 上复跑 `cayw-visual-editor` 包测试。

## 变更

1. 以 `6043e27` 功能提交 cherry-pick 到 `ac17893`，形成本分支 tip。
2. Linux 上 `cap_std::Dir` 以 `O_PATH` 打开，直接 `into_std_file().sync_all()` 会返回 `EBADF`，导致 Host/Workspace 写入在目录同步阶段失败。修复：
   - `cayw-host`：新增 `filesystem::sync_directory`，Linux 下通过 `dir.open(".")` 取得可 `fsync` 的描述符；`read_fence` 复用该路径。
   - `cayw-workspace`：同样调整 `filesystem::sync`。
3. `cayw-visual-editor`：`leader_exited` 使用可移植的 `si_signo != 0`（Linux 上 `si_pid` 是方法而非字段）。

## 本机验证（Linux x86_64，Rust 1.95.0）

```sh
cargo test -p cayw-visual-editor --locked
```

结果：库 47、CLI 3、framework 12、project 33、server 20，共 115 passed、0 failed。

未在本环境重跑 macOS Playwright 浏览器验收与 Magpie 真实生成（脚本限定 `darwin`，且需本机 Magpie `127.0.0.1:3425`）。macOS 既有验收记录仍有效；Magpie 生成仍为独立未完成扩展项。

## Issue 八项验收对照

| 标准 | 状态 | 依据 |
| --- | --- | --- |
| 进入可视化编辑且布局一致 | pass（既有 macOS） | issue-30-*-macos.md |
| 选区映射 | pass（既有 macOS + Linux 包测） | 同上；Linux 115 项 |
| 拖拽/跨容器 | pass（既有 macOS） | 同上 |
| 插入组件 | pass（既有 macOS + Linux） | 同上 |
| 右键样式/确定性编辑 | pass（既有 macOS + Linux） | 同上 |
| 撤销/重做 | pass（既有 macOS + Linux） | 同上 |
| 保存回写/重启 | pass（既有 macOS；Linux 包测覆盖 CAS/保存） | 同上 |
| 交互与移动宽度 | pass（既有 macOS） | 同上 |
| 未适配区域只读 | pass（既有 macOS + Linux） | 同上 |
| Magpie 真实生成项目编辑 | blocked | 需 darwin + Magpie 凭据路径 |

