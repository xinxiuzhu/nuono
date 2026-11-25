# 插件通过 SDK 使用 Obscura

`nuono_plugin::browser` 提供执行网页 JavaScript 后抓取 HTML、文本和 JSON 求值结果的能力。底层将 [Obscura](https://github.com/h4ckf0r0day/obscura/tree/a754dcebe2476713cf30e1b9f9428ee9fbfb4e99) 的 `obscura-browser`、`obscura-js` 作为普通 Rust 库统一链接到应用，固定 revision 为 `a754dcebe2476713cf30e1b9f9428ee9fbfb4e99`。本接入没有 CLI 子进程、CDP 服务、Chromium 或外部浏览器协议，也不启用 Obscura 的 render、stealth 功能。

这是轻量网页执行和提取入口。网页所需的 Web API、反爬策略和页面逻辑仍可能超出引擎支持范围；真实站点需要消费者逐站测试。SDK 不提供截图、PDF、交互式浏览器会话或完整 Chrome 兼容承诺。

## 启用与调用

业务插件保持自己的独立 Cargo workspace，在现有 SDK 依赖上启用 feature：

```toml
[dependencies]
nuono-plugin = { path = "../../crates/nuono-plugin", features = ["obscura"] }
```

插件安装并参与统一应用构建时，Cargo 会将该 feature 合并到宿主共用的同一个 SDK。修改后需重新构建并启动应用；在 Dashboard 重建插件实例不会重新编译源码。没有任何扩展启用该 feature 时，不构建也不链接 V8，`browser::is_available()` 返回 `false`，调用 `fetch` 返回明确的未启用错误。

通过插件上下文获取绑定本插件生命周期的句柄：

```rust
use nuono_plugin::{PluginContext, Result};
use nuono_plugin::browser::{BrowserContent, BrowserRequest, BrowserWaitUntil};
use std::time::Duration;

async fn resolve(ctx: &PluginContext, url: &str) -> Result<nuono_plugin::browser::BrowserPage> {
    let mut request = BrowserRequest::new(url);
    request.wait_until = BrowserWaitUntil::DomContentLoaded;
    request.timeout = Duration::from_secs(15);
    request.settle = Duration::from_millis(300);
    request.content = BrowserContent::Html;
    request.evaluation = Some(
        "({ title: document.title, canonical: document.querySelector('link[rel=canonical]')?.href ?? null })"
            .into(),
    );
    ctx.browser().fetch(request).await
}
```

`BrowserPage` 的 `url` 是最终页面 URL，`title` 是页面标题，`content` 按请求返回 HTML、文本或空字符串。`evaluation` 为可选的 `serde_json::Value`；未请求求值时为 `None`，正常返回 JavaScript `null` 时为 `Some(Value::Null)`。脚本异常、求值超时、导航失败及输出超限返回 `Err`。

SDK 会同步 history 地址变更，并在页面稳定和求值后有界跟进延迟跳转；求值只执行一次，其结果保留，标题和正文从最终文档提取。为避免上游调试日志输出完整代理凭据、URL 或页面脚本，浏览器工作线程关闭上游 tracing 输出，向调用者返回不含敏感参数的阶段错误。

URL 解析插件可优先走站点公开 API 或普通 HTTP，确认需要执行页面脚本时再调用此入口。是否降级由插件业务规则决定；引擎报错时不会自动改用其他协议，也不会静默返回未执行脚本的 HTTP 内容。若需兼容未启用 Obscura 的构建，可在选择解析策略时检查 `browser::is_available()`；实际抓取产生的错误仍应保留上下文并向调用方报告。

## 请求参数

所有参数都是 `BrowserRequest` 的公开字段；`BrowserRequest::new(url)` 提供默认值。

| 字段 | 默认值与含义 |
| --- | --- |
| `url: String` | 起始 HTTP/HTTPS URL。 |
| `wait_until` | `BrowserWaitUntil::Load`；也支持 `DomContentLoaded`、`NetworkIdle0`。网络空闲不保证业务内容已经就绪。 |
| `timeout: Duration` | 30 秒，最多 120 秒；限制调用者等待，包括排队、导航、settle 和求值。 |
| `settle: Duration` | 250 毫秒，最多 10 秒；导航后继续推进页面事件循环，给定时器和异步页面变化留出时间。 |
| `content` | `BrowserContent::Html`；`Text` 提取文本，`None` 不返回正文。 |
| `evaluation: Option<String>` | 默认不求值；指定同步页面 JavaScript 表达式，返回 JSON 值。返回 Promise 会报错；异步状态应先在导航/settle 阶段推进，再读取结果。 |
| `user_agent: Option<String>` | 默认使用引擎 UA；可显式覆盖。 |
| `proxy: Option<String>` | 默认未指定代理；按底层引擎支持的代理 URL 配置。 |
| `cookies: Vec<String>` | 默认空；按起始 URL 写入的 `Set-Cookie` 格式种子，例如 `session=abc; Path=/`。 |
| `allow_private_network: bool` | 默认 `false`，沿用上游私网访问限制。 |
| `max_output_bytes: usize` | 默认 2 MiB，最多 8 MiB，合计 URL、标题、正文与序列化求值结果的 UTF-8 字节数，超限报错、不截断；不是网络下载量或进程内存上限。 |

每次 `fetch` 创建独立页面与 cookie 状态，不在不同调用间共享登录会话。需要登录状态的消费者应在每次请求中显式提供自身管理的 cookie；不要把用户输入直接当作高权限 cookie 或代理配置。SDK 在整个进程内限制 1 个正在执行的抓取和最多 8 个排队请求，超过容量立即报错。调用者取消等待不会提前释放仍在执行的工作所占配额。

## 初始化、生命周期与运行边界

Nuono 宿主启动入口已在创建 Tokio runtime 前调用 `browser::initialize()`。普通插件只使用 `ctx.browser()`，无需自行初始化。独立程序或 `harness = false` 的集成测试必须从同步 `main` 初始化 V8，然后才创建 Tokio runtime；不要把首次初始化放进 `#[tokio::main]`、`#[tokio::test]` 或插件回调：

```rust
fn main() -> nuono_plugin::Result<()> {
    nuono_plugin::browser::initialize()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async {
        // 在 runtime 内创建 PluginTaskScope，然后使用
        // PluginBrowser::for_scope(Arc::new(scope)) 发起请求。
        Ok(())
    })
}
```

SDK 将引擎执行放在受插件任务作用域管理的工作线程中。插件禁用、卸载或实例重建会拒绝新请求，并等待已启动任务退出。调用方到达 deadline 或取消 future 仅停止等待，不会强制终止工作线程。求值阶段使用 V8 终止机制，但同步导航路径和部分 Rust op 不保证立即中断；上游同步导航 watchdog 默认约 31 秒（可通过环境配置调整），线程真正退出前仍占用配额，插件停用阶段也会继续等待。静态库接入不具备 Obscura CLI 的进程级强退兜底。

已知兼容边界：

- 当前固定 revision 在 debug 构建创建初始 iframe 时，`op_initial_frame` 可能重入 `op_register_document_realm`，触发不可展开的 panic 并终止进程；SDK 的 `catch_unwind` 无法将此类 abort 转为普通错误。目标站点应先在独立测试程序中验证，未通过验证的完整页面不要直接接入宿主。这不代表 release 构建必然崩溃，也不构成 release 下任意页面的兼容承诺。
- 当前公开接口只返回最终 HTML/文本和单次同步求值结果，尚未暴露上游 fetch/XHR 响应捕获与请求拦截能力。

网页 JavaScript 通过 V8 在宿主进程内运行；源码插件与宿主同权限，本接口也不是 OS 沙箱。默认沿用 Obscura 的出站访问保护，阻止 loopback、私网和 link-local 目标。仅在已知本地服务测试场景显式设置 `allow_private_network = true`。上游还支持进程环境变量 `OBSCURA_ALLOW_PRIVATE_NETWORK=1`，它会覆盖默认网络限制；部署时应将其视为整个进程的私网访问授权。代理和操作系统网络策略也由部署环境管理。

V8 是原生依赖，开启 feature 的首次构建可能需要下载与目标平台匹配的 V8 预编译归档，并明显增加编译时间与产物体积。构建机需要访问相应依赖下载源；离线构建须预先准备 Cargo/V8 缓存。支持范围取决于固定 revision 的 Obscura、`rusty_v8` 和目标系统，不能将一个系统或 CPU 架构的可执行程序直接复制到其他目标使用。WASM Dashboard 不构建此能力。

请优先在目标系统和 CPU 架构上原生构建。此 revision 的跨架构 snapshot 生成需要额外的 cross-mksnapshot 工具；上游缺少该工具时会警告并回退到宿主架构 snapshot，这种产物不能视为可运行的交叉编译结果。release 必须保留 `panic = "unwind"`，Nuono 当前配置已满足。

## 验证命令

框架 smoke fixture 位于 `xtask/fixtures/obscura_smoke.rs`，使用真实 V8 和本地 HTTP 服务，覆盖 JavaScript 修改 DOM、定时器、HTML/文本/JSON 输出、异常、超时、输出上限、cookie 隔离、并发及任务退出，无需外网站点或数据库。它不代替 URL 解析插件在实际站点上的应用测试。

在框架根目录执行：

```bash
cargo test -p nuono-plugin --features obscura --test obscura_smoke --locked
cargo xtask architecture
cargo xtask plugin-templates
```

消费者启用 feature 并写入调用后，在框架根目录验证本机 URL 解析插件，再启动统一应用进行真实站点测试：

```bash
cargo run -- plugin check plugins/nuono-urlresolver
cargo run -- plugin test plugins/nuono-urlresolver
cargo run
```
