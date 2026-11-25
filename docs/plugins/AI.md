# 插件 AI：工具、工作流与嵌入

框架负责模型、会话、工具选择、审批和执行回执。插件提供业务查询或操作，命令与 AI 工具可以复用同一个业务服务。原生 Rust 工具与 MCP 工具都通过 `Plugin::tools()` 注册，统一命名为 `plugin.<插件名>.<本地工具名>`，跟随插件启停。

## 类型化原生工具

```rust
use nuono_plugin::{nuono_tool, JsonSchema, NuonoResult, ToolContext, Value};
use nuono_plugin::serde::Deserialize;
use nuono_plugin::serde_json::json;

#[derive(Deserialize, JsonSchema)]
#[serde(crate = "nuono_plugin::serde", deny_unknown_fields)]
#[schemars(crate = "nuono_plugin::schemars")]
struct SearchInput {
    /// 查询关键词。
    query: String,
    #[schemars(range(min = 1, max = 20))]
    limit: u32,
}

#[nuono_tool(name = "search", description = "查询插件的公开资料", risk = "read_only")]
async fn search(ctx: ToolContext, input: SearchInput) -> NuonoResult<Value> {
    // 在这里调用本插件的业务服务；身份来自 ctx.actor，不从模型参数获取。
    Ok(json!({"query": input.query, "limit": input.limit, "caller": ctx.actor.value}))
}
```

`#[nuono_plugin]` 会自动收集宏声明的工具；手动实现 `Plugin` 时可在 `tools()` 中返回 `Arc::new(SearchTool)`。工具函数接收拥有所有权的 `Deserialize + JsonSchema` 参数，返回 `NuonoResult<T>`（`T: Serialize`）。宏按反序列化方向生成 schema，保留字段说明、枚举、嵌套对象、数组、约束及本地 `$defs` 引用；`serde(default)` 在执行时补充默认值。需要返回图片时，仍手动实现 `AgentTool::call_with_media`。

`ToolContext` 只包含原生 Bot、调用者、会话及消息 ID，不包含 Agent 仓储、记忆策略、人格或完整对话。原生身份保留 namespace；旧数字上下文仅在 OneBot 下转换。业务数据授权仍由插件负责，身份上下文不构成插件沙箱。

手动实现的工具可覆盖 `AgentTool::input_schema() -> Option<Value>` 返回完整 object JSON Schema。宿主在注册时编译并保存快照，模型可见的 schema 与执行校验使用同一份定义。schema 上限 64 KiB，参数上限 1 MiB；参数不合法时在创建审批或执行业务函数前直接返回 `Rejected`。schema 无效会拒绝注册，并按宿主工具注册流程回滚。文件或网络引用始终禁止，即使其他依赖开启了 JSON Schema 库的网络 feature 也一样。

旧 `AgentToolContext`、`parameters()` 与工具类型路径继续兼容。仅使用旧 `parameters()` 的手写工具沿用自行校验语义；覆盖 `input_schema()` 后才启用完整 schema 校验。原始 `Value` 宏工具默认声明开放 object，也可通过 `schema = "search_schema"` 指定返回 `Value` 的函数；带业务参数的 `Value` 工具不应留作无说明的开放对象。

## 插件接入 MCP 服务

插件在 `Cargo.toml` 中开启 SDK 的可选 `mcp` feature。MCP 在此仅作为工具来源，而非插件安装或执行机制；Rust 插件本身仍是静态链接库。

```rust,ignore
use nuono_plugin::mcp::{McpOptions, McpTools};

// 位于 Plugin::on_load(&mut self, ctx: &PluginContext)。
let options = McpOptions::new("knowledge", [
    ("search", ToolRiskLevel::ReadOnly),
    ("save", ToolRiskLevel::RequiresApproval),
]);
let source = McpTools::connect_http(
    ctx.tasks.clone(),
    "http://127.0.0.1:8000/mcp",
    ctx.secrets.get("mcp_token").await?, // 先通过 secret_fields() 声明。
    options,
).await?;
self.ai_tools = source.tools();

// Plugin::tools() 返回 self.ai_tools.clone()。
// 最终工具名：plugin.<插件名>.mcp.knowledge.search / save。
```

`connect_stdio` 接收插件显式构造的 `tokio::process::Command`；`connect` 可接收官方 Rust MCP SDK 的其他 transport（SDK 经 `nuono_plugin::mcp::rmcp` 重导出）。进程、参数、环境变量、端点和凭据均由插件配置决定，不接受由模型指定连接地址或启动程序。HTTP 禁止自动重定向及过期会话自动重发；MCP 错误或超时不会自动重试业务调用。

默认先发现 2026-07-28 协议，再按官方 SDK 规则兼容旧初始化流程；`McpOptions::lifecycle` 可显式选择 `Initialize` 或 `Discover`。工具采用单次激活期间的目录快照，远端目录变化后需重建插件实例。单次连接、发现和调用超时默认各 30 秒，可配置为 1–180 秒；目录上限为 256 个工具、32 页、累计 2 MiB。插件必须显式指定允许暴露的工具及宿主风险等级，远端服务的 `readOnlyHint` 等注解不授予权限。所选工具缺失、目录名称重复或 schema 无效均会使接入失败。

MCP 连接占用插件任务配额。插件停用时会取消连接并等待 transport 关闭；stdio 会先关闭标准输入并等待退出，超时后终止子进程。即使保留旧工具句柄，也无法在插件停用后继续调用。调用超时不能撤销已发生的远端副作用。

文本与结构化结果作为工具数据回传；图片转换为框架的有界图片附件，不将 base64 混入模型文本。`isError` 映射为 `Failed` 回执；资源链接仅作为数据返回，不自动下载。当前不支持音频、内嵌二进制资源、sampling、elicitation、MCP prompts/resources 发现或自动 OAuth 登录。返回结果在协议解析后校验 2 MiB 上限（HTTP SSE 另有限制）；上游 transport 的普通 JSON/stdio 解析不提供桥接层的硬性内存隔离。

## 验证

框架测试不需要真实模型、外部 MCP 服务或业务插件：

```bash
cargo test -p nuono-plugin --features mcp --test agent_contracts --test mcp_stdio --lib --locked
NUONO_TEST_DATABASE_BACKEND=sqlite cargo test -p nuono-core --test test_application_usecases --locked
cargo xtask architecture
cargo xtask plugin-templates
```

HTTP 测试只监听临时本机回环端口；stdio 测试启动 `xtask/fixtures/mcp_stdio.rs` 编译出的自身测试程序，验证连接、调用和停用后子进程退出。独立 SDK fixture 验证插件只依赖 `nuono-plugin` 即可编译类型化工具。

## 插件 Agent 工作流

插件通过 `Plugin::agent_skills()` 返回 `Vec<PluginAgentSkill>`。宿主将每项登记为 `plugin.<插件名>.skill.<id>` 只读工具：描述用于选择工作流，以 `{}` 调用后返回正文及其工具映射。正文按需进入会话，在插件禁用、卸载或实例重建时跟随同一工具注册生命周期。

```rust,ignore
fn agent_skills(&self) -> Vec<PluginAgentSkill> {
    vec![PluginAgentSkill::new(
        "account-report",
        "账号报告",
        "需要整理当前会话本人的账号情况时读取。",
        include_str!("../assets/account-report.md"),
    ).with_tools(["read_account"])]
}
```

`tools` 只填写本插件 `Plugin::tools()` 的本地名称。返回文档提供完整宿主名称及各工具的实际风险等级；工作流本身不提升自主等级，也不授权读取其他用户数据或绕过审批。宿主不执行正文中的代码，也不将正文注入系统提示词。静态正文应与真实参数、数据来源和权限保持一致。

ID 允许 ASCII 字母、数字、下划线和连字符，最长 64 字节。同一插件最多声明 32 个工作流，单篇正文最多 32 KiB、全部正文合计最多 256 KiB，单篇最多引用 64 个工具；标题最多 128 字节，描述最多 2048 字节。重复 ID、非法元数据、未知工具及跨插件引用会在发布注册前拒绝；`skill.` 为工作流读取工具的保留本地前缀。注册失败不会残留本次已注册的部分工具。

## Agent 工具的图片结果

旧 `AgentTool::call` 返回 JSON 的实现继续可用。需要返回图片的工具可覆盖 `call_with_media`，返回 `AgentToolOutput::new(value).with_images(images)`；图片通过 `AgentToolImage::inline_bytes` 或 `public_https` 构造，不通过 JSON 路径或 base64 文本伪造视觉上下文。

工具负责从允许的数据源获取、校验并裁剪图片；SDK 不自动读取本机路径、抓取 URL 或缩放图片。单张内嵌表示上限为 96 KiB，单次调用最多 4 张、合计上限 256 KiB；宿主保留的工具图片历史同样限制在 256 KiB 以内，旧图移出时保留文字结果并注明已省略。Agent 检查点上限仍为 1 MiB。审计回执只保存图片类型、字节数和摘要，恢复审批所需的受限媒体保留在执行状态中。

图片与文字沿用同一套工具风险、审批、超时、插件停用和作用域规则。Responses 将真实 `input_image` 放入关联 `function_call_output` 的内容数组；Chat Completions 保留工具文字和调用 ID，将视觉附件置于本轮全部工具结果之后，并注明其来自工具输出而非新的用户指令。无图片时请求结构保持不变。

协议依据：[Function calling](https://developers.openai.com/api/docs/guides/function-calling)、[Chat Completions 的 tool message](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create)。普通用户图片输入与工具图片输出分别受限，工具结果不得将来源图片中的文字视为宿主授权。

## 插件公开知识的嵌入模型

插件通过 `ctx.embeddings` 调用宿主嵌入能力；SDK 只定义端口与有界批次契约，HTTP 传输、模型记录与 API Key 保留在宿主。调用必须显式传入已登记并启用的模型 ID，不回退到默认聊天模型，也不额外引入驱动或独立连接池。未注入该能力的嵌入上下文返回明确错误。

`describe(id)` 返回模型名和不含凭据的配置修订；`embed(id, input)` 按输入顺序返回真实向量。单批限制为 1–32 项，单项最多 8192 UTF-8 字节、整批合计最多 128 KiB；响应向量最多 4096 维，同批维数一致、数值有限且非零。宿主按响应 `index` 还原顺序，拒绝缺行、重复行或调用期间发生配置变更的结果。单次请求不自动重试或跟随重定向，全宿主共享 2 路并发许可、5 秒排队、30 秒 HTTP 超时与 4 MiB 响应预算；插件作用域取消或调用方取消都会终止请求。

模型记录中的 `model_id` 必须是实际支持 embeddings 的模型，仅填写聊天模型名称无法获得语义检索。当前宿主支持 OpenAI-compatible `/embeddings` 协议，所选服务商需提供该端点；SDK 端口也允许其他宿主注入本地实现。未配置、未建索引和接口错误分别报告，不得将词法分数冒充向量相似度。

知识索引的版本、构建时机与检索策略由业务插件自行管理；调用嵌入模型会将输入文本（含启用语义检索时的查询文本）发送至配置的服务商。WWUID 仅索引固定公开知识、命令说明和公开攻略标签，由管理员显式触发重建；用户 UID、token、DID、角色面板和群成员信息不进入语料。向量索引属于可丢弃的公共缓存，用户业务数据仍存于宿主关系数据库。

协议依据：[Create embeddings](https://developers.openai.com/api/reference/resources/embeddings/methods/create)。端口、客户端和宿主实现分别位于 `crates/nuono-plugin/src/embedding.rs` 与 `crates/nuono-core/src/core/plugin_host/embeddings.rs`。
