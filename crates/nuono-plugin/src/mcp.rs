//! 可选的外部 MCP 工具接入支持。
//!
//! 插件必须显式指定要引入的工具名称并为其分配宿主风险等级（[`ToolRiskLevel`]）；
//! 宿主始终保有最终的工具注册、审批与执行控制权。远端 MCP 服务器的系统指令（instructions）、采样（sampling）与反问（elicitation）不会注入到 Agent 上下文中。
use crate::agent::ToolInputSchema;
use crate::{
    AgentTool, AgentToolContext, AgentToolImage, AgentToolOutput, NuonoError, NuonoResult,
    PluginTaskScope, ToolRiskLevel, Value,
};
use rmcp::{
    ClientServiceExt, RoleClient,
    model::{CallToolRequestParams, ContentBlock, PaginatedRequestParams},
    service::{Peer, RunningServiceCancellationToken},
    transport::IntoTransport,
};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
    time::Duration,
};

pub use rmcp;
pub use rmcp::ClientLifecycleMode;

const MAX_CATALOG_TOOLS: usize = 256;
const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
const MAX_RESULT_BYTES: usize = 2 * 1024 * 1024;

/// MCP 工具白名单与风险等级配置（风险等级完全由宿主侧插件代码显式指定，绝不采信远端服务器返回的注解）。
#[derive(Debug, Clone)]
pub struct McpOptions {
    pub server_id: String,
    pub tools: BTreeMap<String, ToolRiskLevel>,
    pub timeout: Duration,
    pub lifecycle: ClientLifecycleMode,
}

impl McpOptions {
    pub fn new(
        server_id: impl Into<String>,
        tools: impl IntoIterator<Item = (impl Into<String>, ToolRiskLevel)>,
    ) -> Self {
        Self {
            server_id: server_id.into(),
            tools: tools
                .into_iter()
                .map(|(name, risk)| (name.into(), risk))
                .collect(),
            timeout: Duration::from_secs(30),
            lifecycle: ClientLifecycleMode::Auto {
                preferred_versions: vec![rmcp::model::ProtocolVersion::V_2026_07_28],
                legacy_version: None,
            },
        }
    }

    fn validate(&self) -> NuonoResult<()> {
        let valid_name = |name: &str| {
            !name.is_empty()
                && name.len() <= 64
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        };
        if !valid_name(&self.server_id)
            || self.tools.is_empty()
            || self.tools.len() > MAX_CATALOG_TOOLS
            || self.timeout.is_zero()
            || self.timeout > Duration::from_secs(180)
            || self.tools.keys().any(|name| {
                name.is_empty()
                    || name.len() > 128
                    || !name
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
            })
        {
            return Err(invalid("MCP 服务标识、工具选择或超时无效"));
        }
        Ok(())
    }
}

fn invalid(message: &str) -> NuonoError {
    NuonoError::Validation(message.into())
}

struct Session {
    peer: Peer<RoleClient>,
    scope: Arc<PluginTaskScope>,
    cancellation: Option<RunningServiceCancellationToken>,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(cancellation) = self.cancellation.take() {
            cancellation.cancel();
        }
    }
}

/// 单次插件激活期间捕获的 MCP 工具目录快照（若远端工具定义变更，需重载插件以刷新目录）。
pub struct McpTools {
    tools: Vec<Arc<dyn AgentTool>>,
}

impl std::fmt::Debug for McpTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTools")
            .field("tool_count", &self.tools.len())
            .finish()
    }
}

impl McpTools {
    pub fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.tools.clone()
    }

    /// 连接由插件代码显式配置的 MCP 传输通道。
    /// 目标端点或子进程参数绝不能由大模型动态传入；调用工具时也不会向远端转发任何会话身份或宿主密钥。
    pub async fn connect<T, E, A>(
        scope: Arc<PluginTaskScope>,
        transport: T,
        options: McpOptions,
    ) -> NuonoResult<Self>
    where
        T: IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        options.validate()?;
        if scope.is_closed() {
            return Err(invalid("插件已停用"));
        }
        let service = tokio::time::timeout(
            options.timeout,
            ().serve_with_lifecycle(transport, options.lifecycle.clone()),
        )
        .await
        .map_err(|_| invalid("MCP 连接超时"))?
        .map_err(|_| invalid("MCP 连接或协议协商失败"))?;
        let session = Arc::new(Session {
            peer: service.peer().clone(),
            scope: scope.clone(),
            cancellation: Some(service.cancellation_token()),
        });
        if let Err(service) = scope.bind_mcp_service(service) {
            let _ = service.cancel().await;
            return Err(invalid("插件已停用或 MCP 连接配额已满"));
        }
        let catalog = tokio::time::timeout(options.timeout, discover(&session.peer))
            .await
            .map_err(|_| invalid("MCP 工具发现超时"))??;
        let mut selected = options.tools.clone();
        let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
        for tool in catalog {
            let Some(risk) = selected.remove(tool.name.as_ref()) else {
                continue;
            };
            let schema = ToolInputSchema::compile(Value::Object((*tool.input_schema).clone()))?;
            tools.push(Arc::new(McpTool {
                local_name: format!("mcp.{}.{}", options.server_id, tool.name),
                remote_name: tool.name.into_owned(),
                description: tool.description.map(|v| v.into_owned()).unwrap_or_default(),
                schema,
                risk,
                timeout: options.timeout,
                session: session.clone(),
            }));
        }
        if !selected.is_empty() {
            return Err(invalid("MCP 服务未提供全部已选择工具"));
        }
        if scope.is_closed() {
            return Err(invalid("插件已停用"));
        }
        Ok(Self { tools })
    }

    /// 连接已配置的 MCP Streamable HTTP 端点（含显式配置的本机服务）；可选的 Bearer 令牌由插件提供（通常读取自 `PluginContext::secrets`）。
    pub async fn connect_http(
        scope: Arc<PluginTaskScope>,
        endpoint: &str,
        bearer_token: Option<String>,
        options: McpOptions,
    ) -> NuonoResult<Self> {
        options.validate()?;
        let url = url::Url::parse(endpoint).map_err(|_| invalid("MCP URL 无效"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid("MCP URL 必须是不含账号或 fragment 的 HTTP(S) 地址"));
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(options.timeout)
            .build()
            .map_err(|_| invalid("MCP HTTP 客户端初始化失败"))?;
        let mut config =
            rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
                endpoint.to_owned(),
            )
            .max_concurrent_requests(4)
            .max_sse_event_size(MAX_RESULT_BYTES)
            .reinit_on_expired_session(false);
        if let Some(token) = bearer_token {
            config = config.auth_header(token);
        }
        Self::connect(
            scope,
            rmcp::transport::StreamableHttpClientTransport::with_client(client, config),
            options,
        )
        .await
    }

    pub async fn connect_stdio(
        scope: Arc<PluginTaskScope>,
        mut command: tokio::process::Command,
        options: McpOptions,
    ) -> NuonoResult<Self> {
        options.validate()?;
        if scope.is_closed() {
            return Err(invalid("插件已停用"));
        }
        command.kill_on_drop(true);
        let transport = rmcp::transport::TokioChildProcess::new(command)
            .map_err(|_| invalid("MCP 子进程启动失败"))?;
        Self::connect(scope, transport, options).await
    }
}

async fn discover(peer: &Peer<RoleClient>) -> NuonoResult<Vec<rmcp::model::Tool>> {
    let mut tools = Vec::new();
    let mut cursor = None;
    let mut cursors = HashSet::new();
    let mut names = HashSet::new();
    let mut bytes = 0;
    for _ in 0..32 {
        let page = peer
            .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
            .await
            .map_err(|_| invalid("MCP 工具发现失败"))?;
        bytes += serde_json::to_vec(&page)?.len();
        if bytes > MAX_CATALOG_BYTES || tools.len() + page.tools.len() > MAX_CATALOG_TOOLS {
            return Err(invalid("MCP 工具目录超过限制"));
        }
        for tool in page.tools {
            if !names.insert(tool.name.to_string()) {
                return Err(invalid("MCP 工具名称重复"));
            }
            tools.push(tool);
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(tools);
        }
        if !cursors.insert(cursor.clone()) {
            return Err(invalid("MCP 工具分页游标重复"));
        }
    }
    Err(invalid("MCP 工具分页超过限制"))
}

struct McpTool {
    local_name: String,
    remote_name: String,
    description: String,
    schema: ToolInputSchema,
    risk: ToolRiskLevel,
    timeout: Duration,
    session: Arc<Session>,
}

#[crate::async_trait]
impl AgentTool for McpTool {
    fn name(&self) -> &str {
        &self.local_name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn risk_level(&self) -> ToolRiskLevel {
        self.risk
    }
    fn timeout(&self) -> Duration {
        self.timeout
    }
    fn input_schema(&self) -> Option<Value> {
        Some(self.schema.value().clone())
    }
    async fn call(&self, ctx: AgentToolContext, input: Value) -> NuonoResult<Value> {
        Ok(self.call_with_media(ctx, input).await?.value)
    }
    async fn call_with_media(
        &self,
        _: AgentToolContext,
        input: Value,
    ) -> NuonoResult<AgentToolOutput> {
        self.schema.validate(&input)?;
        if self.session.scope.is_closed() || self.session.peer.is_transport_closed() {
            return Err(invalid("MCP 连接已停止，请重建插件实例"));
        }
        let mut params = CallToolRequestParams::new(self.remote_name.clone());
        params.arguments = input.as_object().cloned();
        let result = tokio::time::timeout(self.timeout, self.session.peer.call_tool(params))
            .await
            .map_err(|_| invalid("MCP 工具超时；外部操作可能仍在进行，请勿盲目重试"))?
            .map_err(|_| invalid("MCP 工具请求失败或需要尚未支持的交互"))?;
        convert_result(result)
    }
}

fn convert_result(result: rmcp::model::CallToolResult) -> NuonoResult<AgentToolOutput> {
    if serde_json::to_vec(&result)?.len() > MAX_RESULT_BYTES {
        return Err(invalid("MCP 工具结果超过限制"));
    }
    let mut images = Vec::new();
    let mut content = Vec::new();
    for item in result.content {
        match item {
            ContentBlock::Image(image) => {
                images.push(AgentToolImage::inline_data_uri(format!(
                    "data:{};base64,{}",
                    image.mime_type, image.data
                ))?);
            }
            ContentBlock::Text(text) => {
                content.push(serde_json::json!({"type":"text", "text":text.text}))
            }
            ContentBlock::ResourceLink(link) => content.push(serde_json::to_value(link)?),
            ContentBlock::Resource(resource) => {
                let value = serde_json::to_value(resource)?;
                if value["resource"].get("blob").is_some() {
                    return Err(invalid("MCP 二进制资源暂不支持"));
                }
                content.push(value);
            }
            ContentBlock::Audio(_) => return Err(invalid("MCP 音频结果暂不支持")),
            _ => return Err(invalid("MCP 结果内容类型暂不支持")),
        }
    }
    let value =
        serde_json::json!({"content":content,"structuredContent":result.structured_content});
    if result.is_error == Some(true) {
        return Err(invalid(&format!("MCP 工具执行失败：{}", value)));
    }
    AgentToolOutput::new(value).with_images(images)
}

#[cfg(test)]
#[path = "../tests/unit/mcp/tests.rs"]
mod tests;
