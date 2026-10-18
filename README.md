# CAYW

**Code As Your Wish — Rust-native modular Agent Runtime**

> 开发初期 / Early development
>
> 本项目目前处于规划与初始化阶段。本仓库仅包含项目说明；下述内核、模块和工具策略均为设计目标，尚未实现，也不代表已有可用功能。
>
> CAYW is in early development. This repository currently contains project documentation only. The runtime, modules, and tool policies below are planned, not implemented features.

## 项目方向

CAYW 计划构建 Rust 原生、独立的模块化 Agent Runtime 内核，逐步替换 WinWinCode 对 Codex 内核的依赖。内核以 Headless Core 为中心，界面、远程服务和工作流能力按需组合。

项目承接 [WinWinCode](https://github.com/changw98ic/winwincode) 和 [CodePatchBay](https://github.com/changw98ic/codepatchbay) 的后续开发方向。当前初始化不复制旧项目代码，不创建空的架构骨架。

## 计划中的模块边界

以下是待验证的职责划分，不是当前实现。

| 模块 | 规划职责 |
| --- | --- |
| Headless Core | 独立运行的 Rust 原生 Agent 内核，不依赖图形界面。 |
| TUI / WebUI | 可插拔的交互界面，按使用场景选择。 |
| Server（可选） | 按需提供远程接入与服务能力，不作为本地运行的必选依赖。 |
| Device | 管理设备侧的执行进程与资源，包括进程生命周期和资源回收。 |
| StrongFlow / JEv / Fusion / Observer（可选） | 按需接入的扩展模块；具体职责、接口和启用方式待设计验证。 |

## CodeMode 与工具能力

CodeMode 计划作为统一的工具编排入口。其编排语言与执行方案尚未选定。

默认不向 Agent 暴露 Bash、Shell、Node 或 Python 通用执行入口。出现真实能力缺口时，按具体任务申请开放所需能力，并限定用途和范围。

这一默认策略针对 Agent 的工具暴露。项目构建和测试所需的工具链仍应按实际任务需要使用；需要额外执行能力时，按任务申请，避免阻塞必要的构建与测试。

## 初期工作

先验证独立内核的必要能力与模块边界，再确定 CodeMode 语言和实现方案。实现进度与可运行方式将在对应能力实际完成后补充。

## 许可证

许可证尚未选定，当前初始化不添加许可证文件。
