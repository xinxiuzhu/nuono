use std::collections::{BTreeMap, VecDeque};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock};
use tokio::sync::broadcast;
use tracing_subscriber::{
    Layer,
    filter::LevelFilter,
    fmt::{self, time::LocalTime},
    layer::Context,
    layer::SubscriberExt,
    reload,
    util::SubscriberInitExt,
};

mod console;
mod context;

/// 预留插件加载列表的名称列宽，分类标签和其他日志不参与对齐。
pub(crate) fn register_console_plugin_names<'a>(names: impl IntoIterator<Item = &'a str>) {
    console::register_plugin_names(&console::PLUGIN_NAMES, names);
}

/// 日志缓存容量（最近 500 条）。
const LOG_CACHE_SIZE: usize = 500;
const LOG_MESSAGE_MAX_BYTES: usize = 16 * 1024;
const LOG_FIELD_MAX_BYTES: usize = 8 * 1024;
const LOG_EVENT_MAX_BYTES: usize = 64 * 1024;
const LOG_FIELD_MAX_COUNT: usize = 64;
const REDACTED: &str = "[REDACTED]";

static PRIVATE_MESSAGE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"\[(?:RECV|SEND|接收|发送)\]\[[^\]\r\n]+\] (?:私聊|会话|消息)\([^\r\n]*")
        .expect("private message log pattern is valid")
});

static SENSITIVE_FIELD_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)(password|passwd|token|secret|api[_-]?key|authorization|cookie|session|webui[_-]?token|qr|credential)",
    )
    .expect("敏感字段正则应有效")
});
static BEARER_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)\b(Bearer|Basic)\s+[A-Za-z0-9._~+/=-]+").expect("认证方案正则应有效")
});
static ASSIGNMENT_SECRET_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?i)\b([\w.-]*(?:password|passwd|token|secret|api[_-]?key|authorization|cookie|session|webui[_-]?token|qr|credential)[\w.-]*)["']?\s*[:=]\s*(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\r\n]*)"#,
    )
    .expect("secret 赋值正则应有效")
});
static ANSI_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").expect("ANSI 正则应有效"));
static TEXT_FIELDS_JSON_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?is)\bfields_json\s*[:=].*$").expect("fields_json 正则应有效")
});
static TEXT_AUTH_HEADER_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)\b(authorization|cookie|set-cookie)\s*[:=][^\r\n]*")
        .expect("认证头正则应有效")
});
static TELEGRAM_TOKEN_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)(/bot|\bbot)(\d{5,}:[A-Za-z0-9_-]{20,})")
        .expect("Telegram token 正则应有效")
});
static URL_CREDENTIAL_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)(https?://)[^\s/@:]+:[^\s/@]+@").expect("URL credential 正则应有效")
});

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct RuntimeLogEvent {
    pub id: u64,
    pub level: String,
    pub bot_id: String,
    pub target: String,
    pub message: String,
    pub fields: BTreeMap<String, serde_json::Value>,
    #[serde(with = "nuono_domain::scalar::utc_millis")]
    #[schema(value_type = String)]
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub truncated: bool,
}

/// 全局日志广播通道（用于 WebSocket 实时推送）
static LOG_BROADCAST_TX: OnceLock<broadcast::Sender<RuntimeLogEvent>> = OnceLock::new();

/// 全局日志内存缓存（用于历史查询，不落地数据库）
static LOG_CACHE: OnceLock<LogCache> = OnceLock::new();

/// 运行时日志过滤器句柄（允许 WebUI 调整日志等级，无需重启）
static LOG_FILTER_RELOAD: OnceLock<
    reload::Handle<tracing_subscriber::EnvFilter, tracing_subscriber::Registry>,
> = OnceLock::new();

static LOG_ID: AtomicU64 = AtomicU64::new(1);
static ACTIVE_FILTER: OnceLock<RwLock<String>> = OnceLock::new();
static LOG_FILTER_UPDATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 本进程实际使用的文件输出配置，关闭输出后仍可查看该目录中的历史日志。
#[derive(Debug, Clone)]
pub(crate) struct FileLogSettings {
    pub enabled: bool,
    pub retention_days: u64,
    pub directory: PathBuf,
}

static FILE_LOG_SETTINGS: OnceLock<FileLogSettings> = OnceLock::new();

pub(crate) fn file_log_settings() -> Option<FileLogSettings> {
    FILE_LOG_SETTINGS.get().cloned()
}

const NOISY_LOG_TARGETS: &[&str] = &[
    "sqlx",
    "tower_http",
    "hyper",
    "h2",
    "reqwest",
    "rustls",
    "sea_orm_migration",
    "bpi_rs",
];

/// 订阅供 WebSocket 日志查看器使用的实时广播。
///
/// 广播容量固定为 1024，慢订阅者会收到 lag 错误并丢失旧事件；需要补历史时应从
/// [`get_recent_logs`] 读取最多 500 条进程内缓存。该接口要求先成功调用 [`init`]。
pub fn log_broadcast_subscribe() -> broadcast::Receiver<RuntimeLogEvent> {
    LOG_BROADCAST_TX
        .get()
        .expect("telemetry not initialized")
        .subscribe()
}

/// 获取最近 N 条缓存日志（用于历史查询）
pub fn get_recent_logs(n: usize) -> Vec<RuntimeLogEvent> {
    LOG_CACHE.get().map(|c| c.get_recent(n)).unwrap_or_default()
}

pub fn validate_filter(level: &str) -> Result<String, String> {
    let normalized = normalize_filter(level);
    tracing_subscriber::EnvFilter::try_new(&normalized)
        .map_err(|e| format!("无效日志等级: {e}"))?;
    Ok(normalized)
}

pub fn current_filter() -> String {
    ACTIVE_FILTER
        .get()
        .and_then(|filter| filter.read().ok().map(|filter| filter.clone()))
        .unwrap_or_else(|| "info".to_string())
}

pub async fn acquire_filter_update_lock() -> tokio::sync::MutexGuard<'static, ()> {
    LOG_FILTER_UPDATE.lock().await
}

pub async fn update_filter_serialized(level: &str) -> Result<(), String> {
    let _guard = acquire_filter_update_lock().await;
    reload_filter(level)
}

pub fn reload_filter(level: &str) -> Result<(), String> {
    let normalized = validate_filter(level)?;
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .parse(&normalized)
        .map_err(|e| format!("无效日志等级: {e}"))?;

    let handle = LOG_FILTER_RELOAD
        .get()
        .ok_or_else(|| "日志系统尚未初始化".to_string())?;

    handle
        .reload(filter)
        .map_err(|e| format!("更新日志等级失败: {}", e))?;
    if let Some(active) = ACTIVE_FILTER.get() {
        *active
            .write()
            .map_err(|_| "日志等级状态锁已损坏".to_string())? = level.trim().to_string();
    }
    Ok(())
}

pub fn normalize_filter(level: &str) -> String {
    let trimmed = level.trim();
    let mut filter = if trimmed.is_empty() {
        "info".to_string()
    } else if trimmed.contains(',') || trimmed.contains('=') {
        trimmed.to_string()
    } else {
        trimmed.to_ascii_lowercase()
    };

    let lower = filter.to_ascii_lowercase();
    for target in NOISY_LOG_TARGETS {
        if !filter_mentions_target(&lower, target) {
            filter.push_str(&format!(",{}=warn", target));
        }
    }

    filter
}

fn filter_mentions_target(filter: &str, target: &str) -> bool {
    filter
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .any(|part| part == target || part.starts_with(&format!("{}=", target)))
}

/// 进程内日志环形缓存（参考 AstrBot LogBroker）。
///
/// 写满后淘汰最旧记录，查询返回按时间从旧到新排列的最近 N 条。锁使用标准库
/// `Mutex`，因为 tracing layer 的 `on_event` 是同步回调且临界区仅包含内存操作。
#[derive(Clone)]
pub struct LogCache {
    inner: Arc<Mutex<VecDeque<RuntimeLogEvent>>>,
    capacity: usize,
}

impl LogCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            capacity,
        }
    }

    pub fn push(&self, log: RuntimeLogEvent) {
        let mut deque = self.inner.lock().unwrap();
        if deque.len() >= self.capacity {
            deque.pop_front();
        }
        deque.push_back(log);
    }

    pub fn get_recent(&self, n: usize) -> Vec<RuntimeLogEvent> {
        let deque = self.inner.lock().unwrap();
        let start = deque.len().saturating_sub(n);
        deque.iter().skip(start).cloned().collect()
    }
}

/// 将 tracing 日志事件转换为统一 JSON 格式并广播到 WebSocket
struct LogBroadcastLayer;

impl<S> Layer<S> for LogBroadcastLayer
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let target = event.metadata().target();
        // 过滤掉数据库/ORM 内部查询日志、HTTP 中间件日志等噪音，
        // 这些日志对前端查看器没有价值，但保留在文件/控制台中
        if target.starts_with("sqlx")
            || target.starts_with("tower_http")
            || target.starts_with("hyper")
            || target.starts_with("h2")
            || target.starts_with("reqwest")
        {
            return;
        }

        let mut visitor = LogVisitor::default();
        event.record(&mut visitor);
        visitor.fallback_message(event.metadata());

        let log_entry = visitor.into_event(event.metadata());

        if let Some(cache) = LOG_CACHE.get() {
            cache.push(log_entry.clone());
        }
        if let Some(tx) = LOG_BROADCAST_TX.get() {
            let _ = tx.send(log_entry);
        }
    }
}

#[derive(Default)]
struct LogVisitor {
    message: String,
    bot_id: Option<String>,
    has_message: bool,
    fields: BTreeMap<String, serde_json::Value>,
    truncated: bool,
}

fn truncate_utf8(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_string(), false);
    }
    let mut end = max_bytes.min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}…", &value[..end]), true)
}

pub(crate) fn redact_text(value: &str) -> String {
    let value = ANSI_RE.replace_all(value, "");
    let value = PRIVATE_MESSAGE_RE.replace_all(&value, "[私人消息正文已隐藏]");
    // Debug 格式可能把任意字段内的 JSON 再次转义，无法可靠地分辨值边界。
    // 这类文本含敏感字段时整体隐藏，避免只遮住第一个单词而泄漏剩余凭据。
    if (value.contains("\\\"") || value.contains("\\'")) && SENSITIVE_FIELD_RE.is_match(&value) {
        return "[包含转义敏感字段的日志已隐藏]".to_string();
    }
    // Text formatter 会对 fields_json 再加引号/转义，不能安全地按普通 key=value 拆分。
    // 保守隐藏该字段及其余部分；JSON formatter 的对应字段由递归 JSON 脱敏保留结构。
    let value = TEXT_FIELDS_JSON_RE.replace_all(&value, "fields_json=[REDACTED]");
    let value = TEXT_AUTH_HEADER_RE.replace_all(&value, "$1=[REDACTED]");
    let value = BEARER_RE.replace_all(&value, "$1 [REDACTED]");
    let value = ASSIGNMENT_SECRET_RE.replace_all(&value, "$1=[REDACTED]");
    let value = TELEGRAM_TOKEN_RE
        .replace_all(&value, "$1[REDACTED]")
        .into_owned();
    URL_CREDENTIAL_RE
        .replace_all(&value, "$1[REDACTED]@")
        .into_owned()
}

fn sanitize_value(name: &str, value: serde_json::Value) -> (serde_json::Value, bool) {
    if matches!(
        name,
        "tokens" | "prompt_tokens" | "completion_tokens" | "total_tokens"
    ) && value.is_number()
    {
        return (value, false);
    }
    if SENSITIVE_FIELD_RE.is_match(name) {
        return (serde_json::Value::String(REDACTED.to_string()), false);
    }
    match value {
        serde_json::Value::String(value) => {
            let redacted = redact_text(&value);
            let (value, truncated) = truncate_utf8(&redacted, LOG_FIELD_MAX_BYTES);
            (serde_json::Value::String(value), truncated)
        }
        serde_json::Value::Object(values) => {
            let mut output = serde_json::Map::new();
            let mut truncated = values.len() > LOG_FIELD_MAX_COUNT;
            for (key, value) in values.into_iter().take(LOG_FIELD_MAX_COUNT) {
                let (value, value_truncated) = sanitize_value(&key, value);
                truncated |= value_truncated;
                output.insert(key, value);
            }
            (serde_json::Value::Object(output), truncated)
        }
        serde_json::Value::Array(values) => {
            let mut truncated = values.len() > LOG_FIELD_MAX_COUNT;
            let output = values
                .into_iter()
                .take(LOG_FIELD_MAX_COUNT)
                .enumerate()
                .map(|(index, value)| {
                    let (value, value_truncated) = sanitize_value(&index.to_string(), value);
                    truncated |= value_truncated;
                    value
                })
                .collect();
            (serde_json::Value::Array(output), truncated)
        }
        other => (other, false),
    }
}

/// 文件查看与实时日志共用字段、文本脱敏规则。残缺 JSON 不作为纯文本回显。
pub fn sanitize_file_line(line: &str) -> (String, bool) {
    let line = ANSI_RE.replace_all(line, "");
    let trimmed = line.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            return ("[日志 JSON 不完整或无效，内容已隐藏]".to_string(), true);
        };
        fn unpack_fields(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(fields) => {
                    for (key, value) in fields {
                        if key == "fields_json"
                            && let Some(raw) = value.as_str()
                        {
                            *value = serde_json::from_str(raw).unwrap_or_else(|_| {
                                serde_json::Value::String(REDACTED.to_string())
                            });
                        }
                        unpack_fields(value);
                    }
                }
                serde_json::Value::Array(values) => values.iter_mut().for_each(unpack_fields),
                _ => {}
            }
        }
        unpack_fields(&mut value);
        let (value, truncated) = sanitize_value("", value);
        (value.to_string(), truncated)
    } else {
        (redact_text(&line), false)
    }
}

impl LogVisitor {
    fn explicit_plugin(&self) -> Option<&str> {
        ["plugin", "plugin_name"].into_iter().find_map(|name| {
            self.fields
                .get(name)
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
        })
    }

    fn inherit_plugin_context(&mut self) {
        if self.explicit_plugin().is_none()
            && let Some(plugin) = nuono_plugin::core::log_context::current_plugin()
        {
            // 即使第三方依赖的日志字段已占满配额，也要优先保留 plugin 归属字段。
            if self.fields.len() >= LOG_FIELD_MAX_COUNT {
                self.fields.pop_last();
                self.truncated = true;
            }
            self.insert_field("plugin", plugin.into());
        }
    }

    fn fallback_message(&mut self, meta: &tracing::Metadata<'_>) {
        self.inherit_plugin_context();
        if !self.has_message {
            self.message = format!("[{}] {}", meta.target(), meta.name());
        }
    }

    fn push_field(&mut self, field: &tracing::field::Field, value: serde_json::Value) {
        if matches!(field.name(), "bot_id" | "message") {
            return;
        }
        if field.name() == "fields_json"
            && let Some(raw) = value.as_str()
        {
            if raw.len() > LOG_EVENT_MAX_BYTES {
                self.truncated = true;
                self.insert_field(
                    field.name(),
                    serde_json::Value::String(REDACTED.to_string()),
                );
                return;
            }
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(raw) {
                let (parsed, truncated) = sanitize_value(field.name(), parsed);
                self.truncated |= truncated;
                self.insert_field("fields", parsed);
                return;
            }
            self.truncated = true;
            self.insert_field(
                field.name(),
                serde_json::Value::String(REDACTED.to_string()),
            );
            return;
        }
        self.insert_field(field.name(), value);
    }

    fn insert_field(&mut self, name: &str, value: serde_json::Value) {
        if self.fields.len() >= LOG_FIELD_MAX_COUNT {
            self.truncated = true;
            return;
        }
        let (value, truncated) = sanitize_value(name, value);
        self.truncated |= truncated;
        self.fields.insert(name.to_string(), value);
    }

    fn into_event(mut self, meta: &tracing::Metadata<'_>) -> RuntimeLogEvent {
        self.inherit_plugin_context();
        let message = redact_text(&self.message);
        let (message, message_truncated) = truncate_utf8(&message, LOG_MESSAGE_MAX_BYTES);
        self.truncated |= message_truncated;

        let bot_id = self.bot_id.unwrap_or_else(|| "system".to_string());
        let (bot_id, bot_truncated) = truncate_utf8(&redact_text(&bot_id), LOG_FIELD_MAX_BYTES);
        let (target, target_truncated) = truncate_utf8(meta.target(), LOG_FIELD_MAX_BYTES);
        self.truncated |= bot_truncated || target_truncated;

        let mut event = RuntimeLogEvent {
            id: LOG_ID.fetch_add(1, Ordering::Relaxed),
            level: meta.level().to_string(),
            bot_id,
            target,
            message,
            fields: self.fields,
            timestamp: crate::clock::db_now(),
            truncated: self.truncated,
        };

        while serde_json::to_vec(&event).map_or(0, |bytes| bytes.len()) > LOG_EVENT_MAX_BYTES {
            let Some(key) = event.fields.keys().next_back().cloned() else {
                break;
            };
            event.fields.remove(&key);
            event.truncated = true;
        }
        event
    }
}

impl tracing::field::Visit for LogVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let val = format!("{value:?}");
        let val = if val.starts_with('"') && val.ends_with('"') && val.len() >= 2 {
            val[1..val.len() - 1].to_string()
        } else {
            val
        };

        match field.name() {
            "bot_id" => self.bot_id = Some(redact_text(&val)),
            "message" => {
                self.has_message = true;
                self.message = val;
            }
            _ => self.push_field(field, serde_json::Value::String(val)),
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "bot_id" => self.bot_id = Some(redact_text(value)),
            "message" => {
                self.has_message = true;
                self.message = value.to_string();
            }
            _ => self.push_field(field, serde_json::Value::String(value.to_string())),
        }
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        if field.name() == "bot_id" {
            self.bot_id = Some(value.to_string());
        } else {
            self.push_field(field, serde_json::Value::Number(value.into()));
        }
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if field.name() == "bot_id" {
            self.bot_id = Some(value.to_string());
        } else {
            self.push_field(field, serde_json::Value::Number(value.into()));
        }
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.push_field(field, serde_json::Value::Bool(value));
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        let value = serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null);
        self.push_field(field, value);
    }
}

/// 日志输出格式
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LogFormat {
    /// 人类可读的紧凑格式
    #[default]
    Compact,
    /// 人类可读的漂亮格式（带颜色）
    Pretty,
    /// 机器可读的 JSON 格式（适合日志收集）
    Json,
}

/// 日志配置
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// 日志级别
    pub level: String,
    /// 是否输出到控制台
    pub console_output: bool,
    /// 控制台输出格式
    pub console_format: LogFormat,
    /// 是否输出到文件
    pub file_output: bool,
    /// 日志文件目录
    pub file_dir: PathBuf,
    /// 文件日志格式
    pub file_format: LogFormat,
    /// 是否包含源码位置（文件名、行号）
    pub include_location: bool,
    /// 是否包含目标模块名
    pub include_target: bool,
    /// 日志保留天数（默认 7 天）
    pub retention_days: u64,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info,sqlx=warn,tower_http=warn,hyper=warn,h2=warn,reqwest=warn".to_string(),
            console_output: true,
            console_format: LogFormat::Pretty,
            file_output: true,
            file_dir: PathBuf::from("logs"),
            file_format: LogFormat::Json,
            include_location: true,
            include_target: true,
            retention_days: 7,
        }
    }
}

/// 仅接受按天生成的 YYYY-MM-DD.log 文件名。
pub(crate) fn is_log_file_name(name: &str) -> bool {
    let Some(date) = name.strip_suffix(".log") else {
        return false;
    };
    date.len() == 10
        && chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .is_ok_and(|parsed| parsed.format("%Y-%m-%d").to_string() == date)
}

/// 清理超过 retention_days 天的日志文件
fn clean_old_logs(dir: &PathBuf, retention_days: u64) {
    if retention_days == 0 {
        return;
    }
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(retention_days * 86400);

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            // 与日志列表使用同一命名规则，只清理 Nuono 生成的日志。
            if !path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_log_file_name)
            {
                continue;
            }
            if let Ok(meta) = entry.metadata()
                && let Ok(modified) = meta.modified()
                && modified < cutoff
            {
                let _ = std::fs::remove_file(&path);
                tracing::debug!(path = %path.display(), "删除过期日志文件");
            }
        }
    }
}

/// tracing 后台 writer 的生命周期守卫。
///
/// 主程序应持有到退出流程结束，drop 时会刷新尚未写出的文件日志。
pub struct TelemetryGuard {
    _file_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
}

/// 初始化 Nuono 的 tracing 基础设施。
///
/// 这是进程级一次性初始化：全局 sender/cache/reload handle 使用 `OnceLock`，而
/// subscriber 的 `.init()` 也要求未安装过全局 subscriber。调用方应只初始化一次并
/// 持有返回的 [`TelemetryGuard`] 至 shutdown 完成，否则非阻塞文件 writer 可能尚未
/// 刷新。
///
/// 支持：
/// - 控制台彩色输出（开发友好）
/// - 文件滚动日志（按天切割）
/// - JSON 格式输出（对接 ELK/Loki）
/// - 结构化字段
/// - 自动清理过期日志
///
/// # 示例
/// ```
/// use nuono_core::telemetry::{self, LogConfig};
///
/// telemetry::init(LogConfig::default());
/// ```
pub fn init(mut cfg: LogConfig) -> TelemetryGuard {
    cfg.file_dir = std::path::absolute(&cfg.file_dir).expect("无法确定日志目录的绝对路径");
    // 确保日志目录存在；日志文件本体未脱敏（脱敏发生在读取出口），目录按 0700
    // 创建/收紧，挡住同机其他用户直接读文件。
    if cfg.file_output {
        crate::core::paths::restrict_private_dir(&cfg.file_dir);
        clean_old_logs(&cfg.file_dir, cfg.retention_days);
    }
    // 接受部署时配置的目录软链接，但之后固定在它当时指向的实际目录。
    cfg.file_dir = cfg.file_dir.canonicalize().unwrap_or(cfg.file_dir);
    let _ = FILE_LOG_SETTINGS.set(FileLogSettings {
        enabled: cfg.file_output,
        retention_days: cfg.retention_days,
        directory: cfg.file_dir.clone(),
    });

    // 构建 EnvFilter，支持模块级控制
    // 例如：RUST_LOG=nuono=debug,tower_http=warn,sqlx=error
    let env_filter = match std::env::var("RUST_LOG") {
        Ok(level) if !level.trim().is_empty() => {
            let filter = tracing_subscriber::EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .parse_lossy(normalize_filter(&level));
            let _ = ACTIVE_FILTER.set(RwLock::new(level.trim().to_string()));
            filter
        }
        _ => {
            let _ = ACTIVE_FILTER.set(RwLock::new(cfg.level.clone()));
            tracing_subscriber::EnvFilter::new(normalize_filter(&cfg.level))
        }
    };
    let (env_filter, reload_handle) = reload::Layer::new(env_filter);
    let _ = LOG_FILTER_RELOAD.set(reload_handle);

    let console_layer = if cfg.console_output {
        Some(match cfg.console_format {
            LogFormat::Pretty => fmt::layer()
                .with_target(cfg.include_target)
                .with_file(cfg.include_location)
                .with_line_number(cfg.include_location)
                .with_timer(LocalTime::rfc_3339())
                .with_ansi(false)
                .map_event_format(context::WithPluginContext::text)
                .boxed(),
            LogFormat::Compact => fmt::layer()
                .event_format(console::ConsoleFormat {
                    include_target: cfg.include_target,
                    include_location: cfg.include_location,
                    plugins: Arc::clone(&console::PLUGIN_NAMES),
                })
                .with_ansi(
                    std::io::stdout().is_terminal()
                        && std::env::var_os("NO_COLOR").is_none()
                        && std::env::var("TERM").as_deref() != Ok("dumb"),
                )
                .boxed(),
            LogFormat::Json => fmt::layer()
                .json()
                .with_target(cfg.include_target)
                .with_file(cfg.include_location)
                .with_line_number(cfg.include_location)
                .with_timer(LocalTime::rfc_3339())
                // JSON 不应该带 ANSI 颜色
                .with_ansi(false)
                .map_event_format(context::WithPluginContext::json)
                .boxed(),
        })
    } else {
        None
    };

    // WebSocket 实时日志广播层
    let (log_tx, _log_rx) = broadcast::channel(1024);
    let _ = LOG_BROADCAST_TX.set(log_tx);
    let _ = LOG_CACHE.set(LogCache::new(LOG_CACHE_SIZE));

    let broadcast_layer = LogBroadcastLayer;

    // 文件日志层按天滚动，文件名格式为 YYYY-MM-DD.log
    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(console_layer)
        .with(broadcast_layer);

    if cfg.file_output {
        let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_suffix("log")
            .build(&cfg.file_dir)
            .expect("无法创建日志文件");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        let file_layer = match cfg.file_format {
            LogFormat::Json => fmt::layer()
                .json()
                .with_writer(non_blocking)
                .with_target(true)
                .with_file(true)
                .with_line_number(true)
                .with_timer(LocalTime::rfc_3339())
                .with_ansi(false)
                .map_event_format(context::WithPluginContext::json)
                .boxed(),
            _ => fmt::layer()
                .with_writer(non_blocking)
                .with_target(true)
                .with_file(true)
                .with_line_number(true)
                .with_timer(LocalTime::rfc_3339())
                .with_ansi(false)
                .map_event_format(context::WithPluginContext::text)
                .boxed(),
        };

        registry.with(file_layer).init();
        TelemetryGuard {
            _file_guard: Some(guard),
        }
    } else {
        registry.init();
        TelemetryGuard { _file_guard: None }
    }
}

/// 快速初始化：开发环境默认配置
pub fn init_dev() -> TelemetryGuard {
    init(LogConfig {
        level: std::env::var("RUST_LOG").unwrap_or_else(|_| "debug".to_string()),
        console_format: LogFormat::Pretty,
        file_output: false,
        ..Default::default()
    })
}

/// 快速初始化：生产环境默认配置
pub fn init_prod() -> TelemetryGuard {
    init(LogConfig {
        level: std::env::var("RUST_LOG").unwrap_or_else(|_| {
            "info,sqlx=warn,tower_http=warn,hyper=warn,h2=warn,reqwest=warn".to_string()
        }),
        console_format: LogFormat::Compact,
        file_output: true,
        file_format: LogFormat::Json,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::{
        LogVisitor, RuntimeLogEvent, normalize_filter, redact_text, sanitize_value, truncate_utf8,
        validate_filter,
    };
    use serde_json::json;
    use tracing_subscriber::prelude::*;

    #[test]
    fn normalizes_plain_level_with_noisy_targets() {
        let filter = normalize_filter("INFO");

        assert!(filter.starts_with("info"));
        assert!(filter.contains("sqlx=warn"));
        assert!(filter.contains("tower_http=warn"));
    }

    #[test]
    fn accepts_presets_and_rejects_invalid_filters() {
        assert!(validate_filter("DEBUG").is_ok());
        assert!(validate_filter("info,nuono_core=trace").is_ok());
        assert!(validate_filter("this is not a filter[").is_err());
    }

    #[test]
    fn bilibili_request_details_are_opt_in_but_warnings_and_business_logs_remain_visible() {
        for (filter, details) in [("info", false), ("info,bpi_rs=debug", true)] {
            let captured = std::sync::Arc::new(std::sync::Mutex::new(None::<RuntimeLogEvent>));
            let subscriber = tracing_subscriber::registry()
                .with(tracing_subscriber::EnvFilter::new(normalize_filter(filter)))
                .with(TestLayer(captured.clone()));
            tracing::subscriber::with_default(subscriber, || {
                tracing::info!(target: "bpi_rs::request", "sending Bilibili request");
                assert_eq!(
                    captured.lock().unwrap().take().is_some(),
                    details,
                    "{filter}"
                );
                tracing::debug!(target: "bpi_rs::request", "request details");
                assert_eq!(
                    captured.lock().unwrap().take().is_some(),
                    details,
                    "{filter}"
                );
                tracing::warn!(target: "bpi_rs::request", "request failed");
                assert!(captured.lock().unwrap().take().is_some(), "{filter}");
                tracing::info!(target: "demo_plugin", "视频下载已排队");
                assert!(captured.lock().unwrap().take().is_some(), "{filter}");
            });
        }
    }

    #[test]
    fn redacts_secret_fields_and_common_token_text() {
        let (secret, truncated) = sanitize_value("api_token", json!("abcdef"));
        assert_eq!(secret, json!("[REDACTED]"));
        assert!(!truncated);

        let (nested, nested_truncated) =
            sanitize_value("fields", json!({"password": "open-sesame", "attempt": 2}));
        assert_eq!(nested["password"], json!("[REDACTED]"));
        assert_eq!(nested["attempt"], json!(2));
        assert!(!nested_truncated);

        let text = redact_text(
            "Authorization: Bearer abc.def password=hunter2 access_token=public-looking client_secret=hidden /bot123456:abcdefghijklmnopqrstuvwxyzABCDEFGHI/getMe",
        );
        assert!(!text.contains("abc.def"));
        assert!(!text.contains("hunter2"));
        assert!(!text.contains("public-looking"));
        assert!(!text.contains("hidden"));
        assert!(!text.contains("123456:"));
        assert!(!redact_text("https://user:password@example.test/x").contains("user:password"));
        assert!(text.contains("[REDACTED]"));
    }

    #[test]
    fn truncates_on_utf8_boundaries() {
        let (value, truncated) = truncate_utf8("消息日志", 5);
        assert!(truncated);
        assert_eq!(value, "消…");
    }

    #[test]
    fn visitor_preserves_scalar_field_types() {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None::<RuntimeLogEvent>));
        let subscriber = tracing_subscriber::registry().with(TestLayer(captured.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(count = 3_u64, ok = true, message = "done");
        });
        let event = captured.lock().unwrap().clone().unwrap();
        assert_eq!(event.fields.get("count"), Some(&json!(3)));
        assert_eq!(event.fields.get("ok"), Some(&json!(true)));
        assert_eq!(event.message, "done");
    }

    struct TestLayer(std::sync::Arc<std::sync::Mutex<Option<RuntimeLogEvent>>>);

    impl<S> tracing_subscriber::Layer<S> for TestLayer
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut visitor = LogVisitor::default();
            event.record(&mut visitor);
            visitor.fallback_message(event.metadata());
            *self.0.lock().unwrap() = Some(visitor.into_event(event.metadata()));
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/telemetry/privacy.rs"]
mod privacy_tests;
