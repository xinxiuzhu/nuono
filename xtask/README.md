# Nuono 开发工具

在 Nuono 根目录使用 `cargo xtask`。工具是独立 Rust workspace，不加入应用或插件的生产依赖图；首次执行会编译，随后复用根目录 `target/` 缓存。工具链沿用根目录 `rust-toolchain.toml`，依赖由本目录的 `Cargo.lock` 锁定。

| 命令 | 检查或操作 |
| --- | --- |
| `cargo xtask architecture` | crate 依赖方向、源码职责边界 |
| `cargo xtask plugin-templates` | 独立生成的插件模板、SDK 依赖边界和单元测试 |
| `cargo xtask source-startup` | 插件与适配器安装组合、重编译、单程序导出与重启验证 |
| `cargo xtask source-startup --profile release` | 使用 release 构建配置检查源码与二进制启动 |
| `cargo xtask registry validate` | 校验 `registry/entries/` 下的全部扩展登记 |
| `cargo xtask registry build --out <目录>` | 生成 `extensions-v2.json` 与 `catalog.json` |
| `cargo xtask registry import-issue --issue-body-file <文件>` | 解析并写入审核通过的扩展提交 Issue |

`NUONO_CARGO_OFFLINE=1` 可让需要嵌套构建的检查使用已缓存的依赖，`CARGO_NET_OFFLINE=true` 可让整个 Cargo 调用离线。

工具自身的测试与格式检查：

```bash
cargo test --manifest-path xtask/Cargo.toml --target-dir target --locked
cargo fmt --manifest-path xtask/Cargo.toml --all -- --check
cargo clippy --manifest-path xtask/Cargo.toml --target-dir target --all-targets --locked -- -D warnings
```

`fixtures/` 保留宿主与 SDK 测试使用的精简测试素材（`source_plugin.rs`、`mcp_stdio.rs`、`obscura_smoke.rs`、`adapter-host/`）。
