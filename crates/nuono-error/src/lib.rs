#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    clippy::unwrap_used,
    clippy::exhaustive_enums,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::allow_attributes_without_reason
)]

use thiserror::Error;

/// 包装 `anyhow::Error`，在保留应用层错误说明的同时透传底层错误链。
#[derive(Debug)]
pub struct ErrorCause(anyhow::Error);

impl std::fmt::Display for ErrorCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for ErrorCause {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

/// Nuono 统一错误类型
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum NuonoError {
    /// 数据库连接失败
    #[error("数据库连接失败: {0}")]
    DatabaseConnection(String),

    /// 数据库查询失败
    #[error("数据库查询失败: {0}")]
    DatabaseQuery(String),

    /// 数据库查询失败，保留原始数据库错误供调用方诊断。
    #[cfg(feature = "database")]
    #[error("数据库查询失败: {0}")]
    DatabaseQuerySource(#[source] sea_orm::DbErr),

    /// 网络连接失败
    #[error("网络连接失败: {0}")]
    Connection(String),

    /// WebSocket 错误
    #[error("WebSocket 错误: {0}")]
    WebSocket(String),

    /// HTTP 请求错误
    #[error("HTTP 请求错误: {0}")]
    Http(String),

    /// URL 解析错误
    #[error("URL 解析错误: {0}")]
    Url(#[from] url::ParseError),

    /// OneBot 协议错误
    #[error("OneBot 协议错误: {0}")]
    OneBot(String),

    /// 平台适配器错误
    #[error("平台适配器错误: {0}")]
    Platform(String),

    /// OneBot 消息解析失败
    #[error("OneBot 消息解析失败: {0}")]
    MessageParse(String),

    /// OneBot 事件处理失败
    #[error("OneBot 事件处理失败: {0}")]
    EventProcessing(String),

    /// 插件加载失败
    #[error("插件加载失败: {plugin} - {reason}")]
    PluginLoad { plugin: String, reason: String },

    /// 插件初始化失败
    #[error("插件初始化失败: {0}")]
    PluginInit(String),

    /// 插件执行失败
    #[error("插件执行失败: {plugin} - {reason}")]
    PluginExecution { plugin: String, reason: String },

    /// 插件未找到
    #[error("插件未找到: {0}")]
    PluginNotFound(String),

    /// 触发器注册失败
    #[error("触发器注册失败: {0}")]
    TriggerRegistration(String),

    /// 权限不足
    #[error("权限不足: {0}")]
    PermissionDenied(String),

    /// 权限验证失败
    #[error("权限验证失败: {0}")]
    PermissionValidation(String),

    /// 用户未授权
    #[error("用户未授权: {0}")]
    Unauthorized(String),

    /// 配置加载失败
    #[error("配置加载失败: {0}")]
    ConfigLoad(String),

    /// 配置验证失败
    #[error("配置验证失败: {0}")]
    ConfigValidation(String),

    /// 环境变量缺失
    #[error("环境变量缺失: {0}")]
    MissingEnvVar(String),

    /// 消息发送失败
    #[error("消息发送失败: {0}")]
    MessageSend(String),

    /// 消息格式错误
    #[error("消息格式错误: {0}")]
    MessageFormat(String),

    /// 资源未找到
    #[error("未找到: {0}")]
    NotFound(String),

    /// 消息通道未设置
    #[error("消息通道未设置")]
    MessageChannelNotSet,

    /// 当前消息上下文未设置
    #[error("当前消息上下文未设置，无法确定发送目标")]
    MessageContextNotSet,

    /// 无法获取当前消息上下文
    #[error("无法获取当前消息上下文")]
    MessageContextLockError,

    /// JSON 解析错误
    #[error("JSON 解析错误: {0}")]
    JsonParse(#[from] serde_json::Error),

    /// 反序列化错误
    #[error("反序列化错误: {0}")]
    Deserialization(String),

    /// 序列化错误
    #[error("序列化错误: {0}")]
    Serialization(String),

    /// 正则表达式错误
    #[error("正则表达式错误: {0}")]
    Regex(#[from] regex::Error),

    /// 文件读取错误
    #[error("文件读取错误: {path} - {reason}")]
    FileRead { path: String, reason: String },

    /// 文件写入错误
    #[error("文件写入错误: {path} - {reason}")]
    FileWrite { path: String, reason: String },

    /// 目录不存在
    #[error("目录不存在: {0}")]
    DirectoryNotFound(String),

    /// 参数验证失败
    #[error("参数验证失败: {0}")]
    Validation(String),

    /// 资源状态冲突
    #[error("资源状态冲突: {0}")]
    Conflict(String),

    /// 并发或队列容量已满，调用方可稍后重试。
    #[error("服务繁忙: {0}")]
    Overloaded(String),

    /// 无效输入
    #[error("无效输入: {0}")]
    InvalidInput(String),

    /// 除数为零
    #[error("除数为零")]
    DivisionByZero,

    /// 内部错误
    #[error("内部错误: {0}")]
    Internal(String),

    /// 功能未实现
    #[error("功能未实现: {0}")]
    NotImplemented(String),

    /// 超时
    #[error("操作超时: {0}")]
    Timeout(String),

    /// 通用错误
    #[error("{0}")]
    Other(String),

    /// 通用错误，保留应用上下文和原始原因。
    #[error("{0}")]
    OtherSource(#[source] ErrorCause),
}

/// Nuono 结果类型别名
pub type Result<T> = std::result::Result<T, NuonoError>;

impl NuonoError {
    /// 错误类别名（不含参数与内部细节）。
    ///
    /// 用于结构化 trace 等对外观察面：只暴露「哪一类错误」，完整技术错误仍留在
    /// 受保护的运行日志里。新增变体时必须同步补进这个 match。
    pub fn kind(&self) -> &'static str {
        match self {
            NuonoError::DatabaseConnection(_) => "DatabaseConnection",
            NuonoError::DatabaseQuery(_) => "DatabaseQuery",
            #[cfg(feature = "database")]
            NuonoError::DatabaseQuerySource(_) => "DatabaseQuery",
            NuonoError::Connection(_) => "Connection",
            NuonoError::WebSocket(_) => "WebSocket",
            NuonoError::Http(_) => "Http",
            NuonoError::Url(_) => "Url",
            NuonoError::OneBot(_) => "OneBot",
            NuonoError::Platform(_) => "Platform",
            NuonoError::MessageParse(_) => "MessageParse",
            NuonoError::EventProcessing(_) => "EventProcessing",
            NuonoError::PluginLoad { .. } => "PluginLoad",
            NuonoError::PluginInit(_) => "PluginInit",
            NuonoError::PluginExecution { .. } => "PluginExecution",
            NuonoError::PluginNotFound(_) => "PluginNotFound",
            NuonoError::TriggerRegistration(_) => "TriggerRegistration",
            NuonoError::PermissionDenied(_) => "PermissionDenied",
            NuonoError::PermissionValidation(_) => "PermissionValidation",
            NuonoError::Unauthorized(_) => "Unauthorized",
            NuonoError::ConfigLoad(_) => "ConfigLoad",
            NuonoError::ConfigValidation(_) => "ConfigValidation",
            NuonoError::MissingEnvVar(_) => "MissingEnvVar",
            NuonoError::MessageSend(_) => "MessageSend",
            NuonoError::MessageFormat(_) => "MessageFormat",
            NuonoError::NotFound(_) => "NotFound",
            NuonoError::MessageChannelNotSet => "MessageChannelNotSet",
            NuonoError::MessageContextNotSet => "MessageContextNotSet",
            NuonoError::MessageContextLockError => "MessageContextLockError",
            NuonoError::JsonParse(_) => "JsonParse",
            NuonoError::Deserialization(_) => "Deserialization",
            NuonoError::Serialization(_) => "Serialization",
            NuonoError::Regex(_) => "Regex",
            NuonoError::FileRead { .. } => "FileRead",
            NuonoError::FileWrite { .. } => "FileWrite",
            NuonoError::DirectoryNotFound(_) => "DirectoryNotFound",
            NuonoError::Validation(_) => "Validation",
            NuonoError::Conflict(_) => "Conflict",
            NuonoError::Overloaded(_) => "Overloaded",
            NuonoError::InvalidInput(_) => "InvalidInput",
            NuonoError::DivisionByZero => "DivisionByZero",
            NuonoError::Internal(_) => "Internal",
            NuonoError::NotImplemented(_) => "NotImplemented",
            NuonoError::Timeout(_) => "Timeout",
            NuonoError::Other(_) => "Other",
            NuonoError::OtherSource(_) => "Other",
        }
    }
}

/// 从 anyhow::Error 转换
impl From<anyhow::Error> for NuonoError {
    fn from(err: anyhow::Error) -> Self {
        NuonoError::OtherSource(ErrorCause(err))
    }
}

/// 从 SeaORM 错误转换（DAO 经 `db.orm()` / `db.begin_write()` 直接写 `.await?`）。
#[cfg(feature = "database")]
impl From<sea_orm::DbErr> for NuonoError {
    fn from(err: sea_orm::DbErr) -> Self {
        NuonoError::DatabaseQuerySource(err)
    }
}

/// 转换为分类错误字符串（供 HTTP API 使用）
pub fn to_api_error(err: &NuonoError) -> String {
    match err {
        NuonoError::DatabaseConnection(_) | NuonoError::DatabaseQuery(_) => {
            format!("[DB] {}", err)
        }
        #[cfg(feature = "database")]
        NuonoError::DatabaseQuerySource(_) => format!("[DB] {}", err),
        NuonoError::Connection(_) | NuonoError::WebSocket(_) | NuonoError::Http(_) => {
            format!("[NET] {}", err)
        }
        NuonoError::PermissionDenied(_)
        | NuonoError::PermissionValidation(_)
        | NuonoError::Unauthorized(_) => {
            format!("[AUTH] {}", err)
        }
        NuonoError::PluginNotFound(_) => {
            format!("[PLUGIN] {}", err)
        }
        _ => err.to_string(),
    }
}
