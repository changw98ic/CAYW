# PR55 发布准备门禁

本轮从 `ee94e9c155d17af469887066627fa0bc50c11c16` 继续修复模型凭据编辑、配置输入及安装引导，并补充此前默认 feature CI 不会执行的验收。本文件说明可复跑门禁与证据边界，不把新增测试代码当作通过记录。实际结果以 [PR55](https://github.com/changw98ic/CAYW/pull/55) 对应候选提交的 Actions 日志和 artifact 为准。

## 候选提交和平台

- CI checkout 显式使用 PR 的 head SHA，不再把 GitHub 临时 merge SHA 写作 PR head 的执行证据。
- 默认 macOS 和 Ubuntu 工作区门禁保留 fmt、check all-targets、test、clippy `-D warnings`，新增 WebUI Node 回归测试。
- macOS 产品和发行包门禁独立运行。发行包要求实际 runner 为 Darwin arm64；其他架构没有因此获得验收。
- Ubuntu 结果仍仅表示 Rust 工作区验证，不是 Linux 桌面产品支持。

## 显式产品 feature 与恢复

`macOS / product features / crash recovery` job 执行：

```sh
cargo check -p cayw-runtime --all-targets --locked --features plugins,strongflow,pi-compat,remote
cargo test -p cayw-runtime --locked --features plugins,strongflow,pi-compat,remote
cargo clippy -p cayw-runtime --all-targets --locked --features plugins,strongflow,pi-compat,remote -- -D warnings
cargo test -p cayw-runtime --locked \
  --features plugins,strongflow,pi-compat,remote,fault-injection \
  --test crash_recovery_matrix --test crash_reconciliation \
  --test refactor_process_recovery --test worker_panic \
  --test content_recovery --test result_externalization --test result_sources
cargo clippy -p cayw-runtime --all-targets --locked --features plugins,strongflow,pi-compat,remote,fault-injection -- -D warnings
```

显式启用 Pi 所需 feature 后，`pi_compat_runtime`、`pi_mcp_combo`、`optional_service_cleanup`、`workspace_optional_services` 不再依赖默认 feature 或其他 crate 的 dev-dependency 偶然启用。故障注入仅用于测试，发行包不启用它。

## 发行包安装和升级

`macOS arm64 / package / install / upgrade` job 执行：

```sh
python3 scripts/test-install-cli.py
sh scripts/package-cli.sh "$RUNNER_TEMP/cayw-release"
python3 scripts/validate-release-install.py "$RUNNER_TEMP/cayw-release" "$RUNNER_TEMP/cayw-release-evidence"
```

- 第一步是合成程序的安装失败/中断回归，不能替代后两步真实发行二进制验证。
- 打包从干净候选提交构建四个程序：`cayw`、`cayw-client`、`cayw-tui`、`cayw-webui`。独立 headless-client workspace 也以 `--locked` 构建。
- 实际包的 Runtime 仍只启用 `plugins`。可选产品组合通过不等于这些组件已随包分发，Pi/remote/StrongFlow 不在该标准包的声明范围内。
- 安装验收使用新建 HOME、CAYW_HOME、workspace 和合成数据。不得使用真实用户配置、真实 API key 或付费模型调用。
- Actions artifact 保存发行包、`BUILD.json`、校验和及安装验收记录，保留 7 天。这不是发布 GitHub Release，也不执行部署或合并。

## 尚不能由这些门禁证明的范围

- 未授权的真实 Provider/付费调用不运行；离线 fixture 不能替代真实服务质量验收。
- 不声称全部 Cargo feature 组合都已覆盖。
- 本地生成和安装成功不等于外部下载后 Gatekeeper、签名、公证已验收。
- 首发平台之外的桌面兼容性不在本次范围内。
