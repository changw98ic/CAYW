# 冻结旧实现的样本生成器

这两个文件是 2026-10-29 在 `e738dae1af73bf0ad3fe15f729a4557a2d9f4d24` 上实际运行的原件。它们不属于当前 Cargo 测试目标，正常检查不会执行它们或覆盖固定样本。

| 文件 | SHA-256 |
| --- | --- |
| `golden_events.rs` | `6cd7192217289633393a794e900305bc80e4b015f13d6db2629ba8055f727bfb` |
| `v3_sample.rs` | `fb8709be94208b97a18e3b7c19a1c296c88b9738cfae143effb23f12e90d66fa` |

复跑时，从该提交创建新的空目录副本，将生成器分别复制到副本的 `crates/cayw-agent/tests/golden_events.rs` 和 `crates/cayw-core/tests/v3_sample.rs`，再在副本执行：

```sh
cargo +1.95.0 test -p cayw-agent --test golden_events --locked -- --ignored generate_golden_events
cargo +1.95.0 test -p cayw-core --test v3_sample --locked -- --ignored generate_v3_sample
```

生成器会写副本内的 `tests/fixtures/`。不要在当前树执行，不要用生成结果替换仓库中的历史样本。

JSON 使用固定时间戳，可以比较生成字节。SQLite 通过公开 Agent、SessionStore 和工具调用路径写入，包含生成时的 UUID 和时钟值；复跑可重建同样的合法结构，不承诺相同数据库字节。历史原件的字节身份由样本 README 中的哈希固定。

源文件保留当时的内容，包括未用于本轮证据的旧 reader 断言。当前读取测试已独立修正：时间戳只要求为正，不承诺系统时钟单调。生成器不是当前读取行为的实现。
