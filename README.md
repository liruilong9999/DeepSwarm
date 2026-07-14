# DeepSwarm

DeepSwarm 是使用 Rust 实现、面向 DeepSeek API(应用程序编程接口) 的多智能体测试与评估 Harness(测试框架)。

## 开发检查

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

设计契约见 [`docs/设计.md`](docs/设计.md)，官方 API 手册是只读资料，不应修改。

