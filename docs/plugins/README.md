# Nuono 插件开发指南

Nuono 插件采用**源码级插件机制**：每个插件是独立的 Rust 工程，与 Nuono 宿主一同静态编译和链接。宿主与插件之间没有 IPC 开销，独立工程则保持清晰的代码边界。

本机业务插件统一放在 `plugins/*/`，由各自的仓库独立维护，无需修改框架的 `Cargo.toml`。

> **注意**：插件与宿主运行在同一进程和地址空间中，权限相同。只安装受信任的插件代码。

## 专题规范

先按本页创建并测试插件，再按功能查阅详细要求：

| 功能 | 规范 |
| --- | --- |
| 配置字段、表单、群覆盖与保存 | [配置](CONFIGURATION.md) |
| 命令前缀、匹配规则与被动监听 | [消息路由](MESSAGE_ROUTING.md) |
| 图片、素材与文件 | [媒体](MEDIA.md) |
| 原生身份、主动发送与平台回执 | [投递](DELIVERY.md) |
| AI 工具、MCP、工作流、图片结果与嵌入 | [AI 能力](AI.md) |
| 自定义页面、Bridge 与插件图标 | [Dashboard 页面](WEB_PAGES.md) |
| 动态网页抓取 | [Obscura](OBSCURA.md) |
| 状态卡公开汇总 | [状态统计](STATUS.md) |

## 快速创建与上手

在框架根目录执行脚手架与管理命令：

```bash
# 1. 创建新插件工程
cargo plugin new my-plugin

# 2. 检查与测试
cargo plugin doctor plugins/my-plugin
cargo plugin check plugins/my-plugin
cargo plugin test plugins/my-plugin

# 3. 运行（自动加载并链接本机插件）
cargo run
```

开发说明：
- `doctor` / `check` / `test` 只检查和测试插件代码本身，无需启动宿主或连接数据库。
- 修改插件代码后重新执行 `cargo run`，框架会自动完成增量编译和应用工程组装。
- 插件默认共享框架的 `target/` 目录与增量编译缓存，避免重复构建通用依赖。
- 导出独立的生产程序时执行 `cargo run --release -- build --output dist/nuono`。

## 生命周期与数据库迁移

插件先完成声明/配置校验、存储登记、`on_load` 和入口注册，此时显示“已加载”，尚不接收业务请求。框架管理服务可访问且首个管理员设置完成后，才调用 `on_enable` 并放行受管任务和插件调用。运行期间手动启用或重建也遵循相同顺序；可通过 `ctx.activation_reason()` 获取来源。禁用插件不运行生命周期回调；存储声明仍由宿主统一登记。

`on_load` 只准备本地资源，`on_enable` 登记连接、轮询和推送任务，不等待外部服务连接成功。外部服务离线由插件报告“连接中／未连接／重试中”，不阻塞框架管理服务。插件默认启用策略通过 `service_config().enabled` 声明，只用于首次安装；后续以框架保存的开关为准，插件业务配置不要再定义同名启用开关。

```rust
fn migrations(&self) -> Vec<PluginMigrationForBackend> {
    let sql = "CREATE TABLE IF NOT EXISTS my_plugin_note (id TEXT PRIMARY KEY NOT NULL)";
    vec![PluginMigrationForBackend {
        version: 1, description: "创建便签表".into(),
        pg_sql: sql.into(), sqlite_sql: sql.into(),
    }]
}
async fn on_load(&mut self, ctx: &PluginContext) -> Result<()> {
    self.runtime.bind(ctx)
}
async fn on_enable(&mut self) -> Result<()> {
    let ctx = self.runtime.context()?;
    ctx.schedule("维护", Schedule::every(std::time::Duration::from_secs(60))?,
        || async { Ok(()) })?;
    Ok(())
}
```

上例使用 `PluginRuntime` 保存本轮上下文。迁移是纯声明，两个数据库方言都须提供；已发布版本不改写，只追加。宿主事务执行失败不会进入回调；迁移提交后的回调失败不会回滚 schema。

## 命令与自动监听

命令、规则和 `on_event` 消息回调通过 `service_config()` 中的命令前缀统一路由。命令声明写业务名称，例如 `帮助`，配置 `ww` 后发送 `ww帮助`；正文通过 `Event::command_text()` 读取，原文和消息段仍通过 `text()`、`raw()`、`message()` 读取。没有有效非空前缀时允许直接匹配指令，但不能接收其他插件已认领的消息。

链接解析等被动功能独立声明：

```rust
fn service_config(&self) -> Plugins {
    Plugins::new(self.name()).with_prefix(vec!["url".into()])
        .with_auto_listen_enabled(true)
}
fn message_listener(&self) -> Option<PluginMessageListener> {
    Some(PluginMessageListener::new("链接和分享卡片")
        .with_text_pattern(r"(?i)https?://").expect("static pattern")
        .with_segment_types(["json"]))
}
async fn on_message_listen(&self, event: &Event) -> Result<PluginResponse> {
    // 使用原始事件处理范围内的消息。
    Ok(PluginResponse::Continue)
}
```

文本正则和消息段类型按任一匹配，空范围拒绝加载。监听只接收无显式前缀归属的新消息，仍检查权限、区域、黑白名单、全局和群插件开关。独立开关 `auto_listen_enabled` 是框架保留键，支持群级继承覆盖。未声明监听能力时不能通过文件或 API 新增能力；声明能力但未指定默认开关时关闭。

宿主在 `on_event` 与 `on_message_listen` 之间选择一个回调。`Continue` 仍继续其他插件和后续触发器，`Stop`、`Reply`、`WaitReply` 沿用原传播语义；命令/规则级 `priority` 和 `block` 保留，插件整体没有优先级。详细路由、等待回复和迁移规则见 [插件消息路由](MESSAGE_ROUTING.md)。

## 配置、任务与数据

`Plugin::config_fields()` 是唯一业务配置声明入口，默认值、表单和校验来自同一声明；通过 `ctx.config_handle` / `LiveConfig<T>` 读取有效快照，`on_config_changed` 更新派生客户端。全局、个人和群的配置差异统一存入数据库，旧 TOML 只作为显式导入材料。顶层 `password` 配置字段保存为受管密钥引用；独立凭据通过 `secret_fields()` 和 `ctx.secrets` 管理，普通配置响应不回显明文。详见 [配置表单](CONFIGURATION.md)。

频繁读取静态声明的类型化配置可用 `LiveConfig<T>::snapshot()` 复用 `Arc<T>`；宿主发布新值后自动重新解析。原 `get()` 仍逐次返回独立值，适用于需要拥有字段或解码依赖外部状态的情况。一次命令先取一次快照，后续字段读取共用它，避免各字段跨配置版本。

后台任务通过 `ctx.spawn` 或 `ctx.tasks` 绑定插件生命周期，在宿主允许业务运行且本插件激活成功前保持等待，在禁用/卸载时取消。`on_load` 不得等待普通后台任务完成，否则会与启动放行相互等待。有限的本地初始化优先直接 `.await`；确需受管并发辅助时使用 `ctx.tasks.spawn_initialization` / `try_spawn_initialization`，这些任务仍受取消和配额约束，不能用于外连、轮询或推送。不要自行创建永久任务或通过 EventBus 绕过消息路由。数据库使用 `ctx.db` 的共享 SeaORM 端口，实体模块使用 `use nuono_plugin::orm as sea_orm`，事务用 `ctx.db.begin()`，不要另建 SQLx 连接池。数据库业务需验证 PostgreSQL 和 SQLite。

普通持久状态优先使用 `ctx.state("preferences", PluginStateScope::global())?`，宿主统一管理插件 ID、记录唯一性、JSON 大小和更新时间。用户、群和频道状态从领域事件生成作用域。需要关系查询或事务约束时，按业务实体建表并通过 `Plugin::database_tables()` 声明归属。

每个插件作用域最多接收 256 个普通异步后台任务，`ctx.tasks.try_spawn` 满载、停用或 runtime 不可用时返回 `false`；`ctx.spawn` 记录拒绝日志。定时任务与插件 EventBus 回调共用这些名额，绘图、浏览器沿用各自配额。EventBus 回调通过受管任务执行，停用或取消发布者会停止等待中的回调，panic 与失败会记录后继续其他订阅。取消等待关闭不表示旧工作已退出，实例重建必须等待同一个实际退出屏障。取消不能强停同步代码、已开始的阻塞工作，或撤销既有外部副作用。

直接使用 `tracing` 记录日志即可：宿主回调和上述受管任务会自动标注插件来源，包含调用的第三方库。普通 INFO 保持插件主标签，警告、错误和调试日志补充依赖模块。不要为每个依赖配置插件名映射；裸 `tokio::spawn` 或自行创建的线程不继承此上下文，应使用 SDK 受管入口。日志归属仅用于观察，不是身份或权限凭据。

## 自定义 Dashboard 页面

实现 `Plugin::web_pages()`，将独立构建的 HTML、JS、CSS、WASM 通过 `PluginWebAsset` 嵌入插件，即可在官方 Dashboard 的插件详情添加按钮，也可声明 Dock 入口。页面经公开 Bridge 调用本插件 API，支持主题与语言同步；官方前端保持单一构建。页面资产随源码插件静态链接，更新后需重建并重启宿主。声明、Leptos 接入、权限与生命周期见 [插件 Dashboard 页面](WEB_PAGES.md)。

`ctx.account_conversations` 提供按插件和网站账号隔离的文字对话，`recent` 读取近期消息，`reply` 通过宿主 Agent 生成回复。插件应从已验证的网页调用者或已验证的外部身份映射确定账号，并在每次调用前校验该账号的访问权限。宿主按 `account_chat` 入口应用 Agent 策略，使用该账号的上下文和个人记忆，执行仅回复模式。

## 状态卡公开统计

`nuono状态` 在状态卡底部展示已启用插件主动提供的汇总指标。实现可选的 `Plugin::status_metrics(&PluginStatusRequest)`，返回最多 4 个 `PluginStatusMetric::new("标签", "值")`；默认空列表隐藏该插件。插件无需引入 `image` 或自行绘图。这个回调面向所有用户，仅能提供适合公开的汇总数据，不能返回绑定账号、凭据或私人内容。完整接口、示例和执行预算见 [插件状态统计](STATUS.md)。

## 测试

AI 工具可通过 `#[nuono_tool]` 使用类型化参数自动生成完整 JSON Schema，或手动实现 `AgentTool`。SDK 的可选 `mcp` feature 将已选择的 MCP 工具接入同一权限和插件生命周期。接口与示例见 [插件 AI 工具与 MCP](AI.md)。

单元测试使用 `nuono-plugin-test` 的 `TestBot` 和事件构造工具，默认无需数据库或运行中的宿主。监听范围可用 `PluginMessageListener::matches(&Event)` 本地验证；宿主集成测试负责前缀归属、权限、群覆盖和传播。测试不应默认访问真实机器人、模型或平台，外部 smoke 需显式选择。

完整的教程、接口参考和可执行自动监听示例位于与本仓库并列的 `nuono-docs` 项目，其中 `scripts/check-auto-listener.py --nuono-root ../Nuono --offline` 在临时工程验证示例，不安装插件、不启动宿主。

脚手架生成同一条可配置的“问候”指令；文档站的问候教程逐步加入参数、配置、按用户保存和清除称呼。小型状态直接使用 `ctx.state`，不为教学示例添加业务表。

`nuono-plugin-test::TestHost` 为 `on_load` 提供内存共享状态与临时目录，`TestBot::call_command_with` 接收显式的 `Event` 和 `MatchData`，保留原生领域身份。这样可以在不启动宿主、不连接数据库的情况下测试配置绑定、状态隔离、实例重建、清除和错误分支。

测试宿主不实现 SQL、事务、迁移、KV、密钥或权限策略，不支持的存储操作报错。它不自动执行生命周期；启动受管任务的测试应显式关闭任务作用域。HTTP 客户端仍是 SDK 客户端，不拦截插件自行发出的外部请求。实际数据库持久化与平台门控仍由独立集成测试验证。

## 富消息与按钮

生成 Markdown、模板或按钮时使用 SDK 的 `Bot::send_markdown`、`send_template_markdown` 和 `send_options`，平台策略由 Dashboard「消息与交互」管理。普通文字不会自动变成 Markdown；命令按钮复用现有前缀、权限、触发保护与 `block`，回复按钮绑定实际等待代次。
