# 贡献指南

感谢参与 Nuono 的开发。无论是修正文档错别字、改进报错提示，还是新增功能或优化性能，都欢迎贡献。

提交代码前，请先阅读本指南。

---

## 本地开发准备

### 1. 基础环境
- **Rust 1.100.0**：由项目根目录的 `rust-toolchain.toml` 固定，建议使用 [rustup](https://rustup.rs/) 安装。
- Linux 裸机构建基础依赖：`build-essential pkg-config libssl-dev ca-certificates curl`。

用以下命令确认工具链就绪：
```bash
rustup toolchain install 1.100.0 --profile minimal --component clippy --component rustfmt
cargo --version  # 应输出 1.100.0 对应版本
```

### 2. 本地运行后端
根 workspace 的默认成员是提供源码入口的 `nuono-tooling`，直接运行即可：

```bash
cargo run
```

### 3. 前端（Dashboard）开发
Dashboard 源码位于独立仓库 [nuono-dashboard](https://github.com/xinxiuzhu/nuono-dashboard)。
- 本仓库只提交编译后的静态产物 `nuono-hub/dist/`，仅做后端开发时无需安装前端工具链。
- 修改前端代码后，在前端仓库执行 `trunk build --release`，再将生成的 `dist/` 完整替换本仓库的 `nuono-hub/dist/`。

---

## 提交前自测清单

创建 Pull Request 之前，请在本地运行以下检查，以便通过 CI：

```bash
# 1. 代码格式化检查
cargo fmt --all -- --check

# 2. 静态分析与规范检查
cargo clippy --workspace --all-targets --all-features --locked -- \
  -D clippy::correctness -D clippy::suspicious

# 3. 运行单元测试（使用 SQLite，无需配置外部数据库）
NUONO_TEST_DATABASE_BACKEND=sqlite cargo test --workspace --all-targets --all-features --locked

# 4. 文档构建检查
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features --locked
```

### 简化 HTTP 接口

只做服务调用和结果包装的接口使用 `ApiResponse::from_result(service_call(...).await)`，复用既有成功信封、错误分类和脱敏。需要权限检查、结果投影或提交后刷新提示的接口保留显式流程；不要用通用 CRUD 宏隐藏这些业务差异。

### 独立单元测试的存放位置

`nuono-core`、`nuono-server`、`nuono-storage`、`nuono-plugin` 和 `nuono-tooling` 的独立单元测试文件放在各包的 `tests/unit/`，内部沿用 `src/` 的目录层级。生产模块保留原来的 `#[cfg(test)]` 模块入口，通过 `#[path = "…"]` 引入测试文件。例如 `src/foo.rs` 可以写：

```rust
#[cfg(test)]
#[path = "../tests/unit/foo/tests.rs"]
mod tests;
```

这仍是库内部的单元测试，`use super::*`、私有成员访问、测试名称和 `cargo test -p <包名> --lib` 用法不变。测试子模块的相对位置也应一起保留。`tests/unit/` 不设 `main.rs`，避免被 Cargo 当作单独的集成测试入口；原有直接位于 `tests/` 下的集成测试不受影响。

这样普通构建不会因修改独立测试而改变框架生产源码指纹或重编宿主。不要按文件名包含 `test` 来忽略源码：参与普通库编译的 `test_support` 等模块继续留在 `src/` 并纳入指纹。小型内联测试可以保留；修改其所在生产文件仍会按 Cargo 规则触发编译。迁移 SQL 的结构指纹保持独立、完整。

---

## 注释与文案

注释写给下一个修改这段代码的人。代码已经表达了「做什么」，注释只补充代码无法表达的部分。

### 注释：写「为什么」，不写「是什么」

```rust
// ❌ 纯复述，下一行代码自己会说：
// 检查是否被封禁
if level == PermissionLevel::Banned { ... }

// ✅ 解释代码看不出来的动机（示例取自 src/api/auth.rs）：
// 不能整体 `clear()`：那会连同生效中的 ban 一起清掉，攻击者只要让失败记录
// 来自足够多的 IP，就能随时重置整张表——包括针对自己的锁定。
```

没有信息量的注释只会增加噪音。删除前先判断：去掉它，读者是否会误解或踩坑？不会则删除。

### 这些内容必须写进注释

- **不变量与前提**：尤其是 fail-closed 的方向及理由——「未登记路由默认最严能力」这类设计，不写明原因就可能被当成疏漏而“修掉”。参考 `middleware.rs` 的 `route_capability` 注释。
- **有意的取舍**：说明为什么“不做”某事（如「不重编码，因此不得绕过认证公开此接口」）。后人无法从代码中读出被否决的方案。
- **外部依据**：RFC 条目、平台文档行为或关联的 Issue。例如 `auth.rs` 对 TOTP 重放保护引用 RFC 6238 §5.2 的注释。
- **历史边界**：代码维持现状的原因（如「拒绝以 `-` 开头的值，避免被 git 解析为命令行选项」）。

### 注释随代码活

- 修改行为时必须同步更新注释。**过时的注释比没有注释更危险**，会误导后续维护者。
- 占位、未完成或已知妥协统一使用 `TODO:` 前缀写明缺失项，不要写成普通注释。
- 能靠命名表达清楚的逻辑（如 `is_trigger_command_disabled`）无需额外注释。

### 用户可见文案

错误提示、日志与 Dashboard 文案面向使用者而非开发者：

- 说明**发生了什么**和**可以怎么做**：「登录失败次数过多，请稍后再试」优于「429 Too Many Requests」。
- 内部细节（SQL 错误、文件路径、错误栈）写入日志，不进入用户提示——现有统一错误信封已做脱敏，新增文案不要绕过它。
- 同一事件的失败措辞保持一致；部分文案受契约测试约束（如 CSRF 校验提示），修改文案需同步更新测试。
- 前端 UI 文案全部走 `locales/` 目录，改动需同步全部语种，`check_i18n.py` 会强制校验完整性。

---

## 插件开发约定

- Nuono 仓库不包含业务插件；`plugins/` 目录用于存放本机插件源码。
- 插件是独立的 Cargo 工程，通过脚手架创建：
  ```bash
  cargo run -- plugin new my-plugin
  ```
- 插件由各自独立仓库维护，请勿将本机业务插件源码或 `target/` 产物提交到框架仓库。详见 [插件开发指南](docs/plugins/README.md)。

---

## 提交规范与 Pull Request

清晰的 PR 便于维护者评审和合并：

1. **保持变更聚焦**：一个 PR 只解决一个明确的问题，避免将功能变更与大规模格式调整或重构混在一起。
2. **填写清晰的 PR 描述**：
   - 简要说明改动的原因和思路。
   - 附上本地测试命令或验证结果。
   - 涉及 UI 变动时附上截图。
3. **注意信息安全**：不要在代码、Issue 或 PR 讨论中包含真实的 Token、密码、私钥、内部日志或用户隐私数据。
