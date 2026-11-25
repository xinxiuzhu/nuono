//! 面向开发者的可读控制台日志格式化器；文件日志与 WebSocket 广播仍保持各自原有格式。
use super::{LOG_MESSAGE_MAX_BYTES, LogVisitor, redact_text, truncate_utf8};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{
    Arc, LazyLock, RwLock,
    atomic::{AtomicUsize, Ordering},
};
use tracing::{Event, Subscriber};
use tracing_subscriber::{
    fmt::{FmtContext, FormatEvent, FormatFields, format::Writer},
    registry::LookupSpan,
};
use unicode_width::UnicodeWidthStr;

pub(super) static PLUGIN_NAMES: LazyLock<Arc<PluginNames>> =
    LazyLock::new(|| Arc::new(PluginNames::default()));

#[derive(Default)]
pub(super) struct PluginNames {
    width: AtomicUsize,
    names: RwLock<BTreeMap<String, String>>,
}

fn plugin_key(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '-' | '_'))
        .flat_map(char::to_lowercase)
        .collect()
}

impl PluginNames {
    fn display(&self, name: &str) -> Option<String> {
        self.names
            .read()
            .expect("plugin names lock poisoned")
            .get(&plugin_key(name.split("::").next().unwrap_or(name)))
            .cloned()
    }
}

#[derive(Default)]
pub(super) struct ConsoleFormat {
    pub include_target: bool,
    pub include_location: bool,
    pub plugins: Arc<PluginNames>,
}

pub(super) fn register_plugin_names<'a>(
    plugins: &PluginNames,
    sources: impl IntoIterator<Item = &'a str>,
) {
    let mut names = plugins.names.write().expect("plugin names lock poisoned");
    let mut longest = 0;
    for source in sources {
        let name = one_line(&redact_text(source));
        longest = longest.max(name.width());
        names.insert(plugin_key(source), name);
    }
    // 列宽属于本次加载清单，后续批次不会被历史上的长名字撑开。
    plugins.width.store(longest, Ordering::Relaxed);
}

#[derive(Debug)]
enum FlowSummary<'a> {
    Message {
        label: &'static str,
        body: Cow<'a, str>,
    },
    Handled(String),
}

impl<'a> FlowSummary<'a> {
    fn from_event(
        meta: &tracing::Metadata<'_>,
        visitor: &LogVisitor,
        message: &'a str,
    ) -> Option<Self> {
        if meta.target() != "message_flow"
            || (*meta.level() != tracing::Level::INFO
                && !(matches!(*meta.level(), tracing::Level::WARN | tracing::Level::ERROR)
                    && visitor.fields.contains_key("send_state")))
        {
            return None;
        }
        let field = |name| visitor.fields.get(name).and_then(serde_json::Value::as_str);
        if message.starts_with("[HANDLED] ") {
            let trigger = field("trigger")?;
            let stopped = visitor.fields.get("propagation_stopped")?.as_bool()?;
            let propagation = if stopped {
                "停止后续处理"
            } else {
                "继续后续处理"
            };
            return Some(Self::Handled(format!("已处理「{trigger}」，{propagation}")));
        }
        let (label, prefix, legacy_prefix) = match field("direction")? {
            "receive" => ("消息/接收", "[接收]", "[RECV]"),
            "send" => ("消息/发送", "[发送]", "[SEND]"),
            _ => return None,
        };
        if let Some(body) = message
            .strip_prefix(prefix)
            .or_else(|| message.strip_prefix(legacy_prefix))
        {
            let body = compact_message_body(body, label);
            return Some(Self::Message {
                label,
                body: Cow::Owned(body),
            });
        }
        // 整条私聊摘要已被脱敏时，只从独立字段补回平台和会话身份。
        if message == "[私人消息正文已隐藏]" {
            let scene = match field("scene")? {
                "private" => "私聊",
                "conversation" => "会话",
                "reply" => "消息",
                _ => return None,
            };
            let platform = field("platform")?;
            let conversation = field("conversation_id")?;
            return Some(Self::Message {
                label,
                body: Cow::Owned(format!(
                    "{} · {scene}({conversation}) · {message}",
                    platform_name(platform)
                )),
            });
        }
        None
    }

    fn source(&self) -> Option<&'static str> {
        match self {
            Self::Message { label, .. } => Some(label),
            Self::Handled(_) => None,
        }
    }

    fn body(&self) -> &str {
        match self {
            Self::Message { body, .. } => body,
            Self::Handled(body) => body,
        }
    }

    fn duplicates(&self, field: &str) -> bool {
        match self {
            Self::Message { .. } => matches!(
                field,
                "direction"
                    | "platform"
                    | "scene"
                    | "conversation_id"
                    | "sender_id"
                    | "message_id"
                    | "content_length"
                    | "truncated"
                    | "send_state"
                    | "request_id"
            ),
            Self::Handled(_) => matches!(field, "trigger" | "trigger_type" | "propagation_stopped"),
        }
    }
}

fn write_styled(
    writer: &mut Writer<'_>,
    style: &str,
    text: std::fmt::Arguments<'_>,
) -> std::fmt::Result {
    if writer.has_ansi_escapes() {
        write!(writer, "\x1b[{style}m{text}\x1b[0m")
    } else {
        writer.write_fmt(text)
    }
}

fn platform_name(platform: &str) -> &str {
    match platform {
        "onebot" | "onebot_v11" | "OneBot v11" => "OneBot v11",
        "qq" | "qq_official" => "QQ 官方",
        "telegram" => "Telegram",
        "discord" => "Discord",
        "feishu" => "飞书",
        _ => platform,
    }
}

fn compact_message_body(body: &str, direction: &str) -> String {
    if let Some((platform, text)) = body.strip_prefix('[').and_then(|s| s.split_once("] ")) {
        let (separator, replacement) = if direction == "消息/接收" {
            (" <= ", " · ")
        } else {
            (" => ", "：")
        };
        format!(
            "{} · {}",
            platform_name(platform),
            text.replacen(separator, replacement, 1)
        )
    } else {
        body.to_owned()
    }
}

fn source_label(target: &str) -> Option<&'static str> {
    let core = matches!(
        target.split("::").next().unwrap_or(target),
        "nuono_server"
            | "nuono_core"
            | "nuono_plugin"
            | "nuono_app"
            | "nuono_tooling"
            | "nuono_domain"
            | "nuono_error"
            | "nuono_ai_provider"
            | "nuono_storage"
    ) || target.starts_with("nuono_adapter_")
        || target.starts_with("nuono::");
    if !core && !target.starts_with("sea_orm") && !matches!(target, "message_flow" | "onebot_api") {
        return None;
    }
    if target == "nuono::startup" || target.starts_with("nuono_server::bootstrap") {
        Some("核心/启动")
    } else if target == "nuono::http" {
        Some("核心/服务")
    } else if target == "message_flow" {
        Some("核心/分发")
    } else if target == "nuono::plugins"
        || target.contains("::plugin_host")
        || target.contains("::trigger_registry")
    {
        Some("核心/插件")
    } else if target.starts_with("nuono_storage") || target.starts_with("sea_orm") {
        Some("核心/数据库")
    } else if target.starts_with("nuono_core::config") {
        Some("核心/配置")
    } else if target.contains("::scheduler") {
        Some("核心/定时任务")
    } else if target == "onebot_api"
        || target.contains("::connection")
        || target.contains("::bot_session")
    {
        Some("核心/连接")
    } else if target.contains("::runtime_stages")
        || target.contains("::event_kernel")
        || target.contains("::handler")
    {
        Some("核心/分发")
    } else if matches!(
        target.split("::").next().unwrap_or(target),
        "nuono_server"
            | "nuono_core"
            | "nuono_plugin"
            | "nuono_app"
            | "nuono_tooling"
            | "nuono_domain"
            | "nuono_error"
            | "nuono_ai_provider"
    ) || target.starts_with("nuono_adapter_")
        || target.starts_with("nuono::")
    {
        Some("核心")
    } else {
        None
    }
}

fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// 仅用于展示：绝不要装饰存储在 JSON 中的消息或广播事件。
fn event_icon(target: &str, level: &tracing::Level, message: &str) -> Option<&'static str> {
    let message = message.trim_start();
    // 若消息开头已带有 Emoji 或符号前缀，则不再重复叠加状态图标。
    if matches!(
        message.chars().next(),
        Some('\u{2600}'..='\u{27ff}' | '\u{1f000}'..='\u{1faff}')
    ) {
        return None;
    }
    match *level {
        tracing::Level::ERROR => return Some("❌"),
        tracing::Level::WARN => return Some("⚠️"),
        _ => {}
    }
    if !matches!(*level, tracing::Level::INFO | tracing::Level::DEBUG) {
        return None;
    }
    // 连接模块还会输出普通报文帧与业务日志，仅为其明确的生命周期状态消息添加图标。
    match target {
        "nuono_core::core::connection" if message.starts_with("[BotConnected]") => Some("🔗"),
        "nuono_core::core::connection" if message.starts_with("[BotDisconnected]") => Some("🔌"),
        "nuono_core::core::bot_session" if message == "Bot 会话已注册" => Some("🔗"),
        "nuono_core::core::bot_session" if message == "Bot 会话已注销" => Some("🔌"),
        "nuono_server::ws"
            if (message.starts_with("收到") && message.contains("OneBot WebSocket 连接"))
                || message == "[LogViewer] WebSocket 连接已建立" =>
        {
            Some("🔗")
        }
        "nuono_server::ws" if message == "[LogViewer] WebSocket 连接已关闭" => Some("🔌"),
        "nuono_server::ws" if message.starts_with("拒绝") => Some("⚠️"),
        "nuono_adapter_discord::gateway"
            if message.starts_with("Connecting to Discord gateway:")
                || message.starts_with("Discord: READY as ") =>
        {
            Some("🔗")
        }
        "nuono_adapter_discord::gateway" if message == "Discord: Gateway connection ended" => {
            Some("🔌")
        }
        "nuono_adapter_telegram::adapter" if message == "Telegram polling started" => Some("🔗"),
        "nuono_adapter_telegram::adapter" if message == "Telegram polling stopped" => Some("🔌"),
        _ => None,
    }
}

impl<S, N> FormatEvent<S, N> for ConsoleFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let meta = event.metadata();
        let mut visitor = LogVisitor::default();
        event.record(&mut visitor);
        visitor.fallback_message(meta);
        let (message, _) = truncate_utf8(&redact_text(&visitor.message), LOG_MESSAGE_MAX_BYTES);
        let flow = FlowSummary::from_event(meta, &visitor, &message);
        let flow_source = flow.as_ref().and_then(FlowSummary::source);
        let plugin = ["plugin", "plugin_name", "plugin_target"]
            .into_iter()
            .find_map(|key| {
                visitor
                    .fields
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())
            });
        let plugin = plugin.map(|name| {
            self.plugins
                .display(name)
                .unwrap_or_else(|| name.to_owned())
        });
        let bridged = meta.target() == "nuono_plugin::host_log";
        let core_source = (!bridged).then(|| source_label(meta.target())).flatten();
        let plugin_source = if core_source.is_none() {
            plugin
                .clone()
                .or_else(|| self.plugins.display(meta.target()))
        } else {
            None
        };
        let plugin_module = plugin_source.as_ref().and_then(|plugin| {
            let target = if bridged {
                visitor
                    .fields
                    .get("plugin_target")
                    .and_then(serde_json::Value::as_str)?
            } else {
                meta.target()
            };
            let module = target.split("::").next().unwrap_or(target);
            (*meta.level() != tracing::Level::INFO && plugin_key(module) != plugin_key(plugin))
                .then_some(module)
        });
        let source = flow_source
            .map(str::to_owned)
            .or_else(|| core_source.map(str::to_owned))
            .or_else(|| {
                plugin_source.as_ref().map(|name| match plugin_module {
                    Some(module) => format!("插件/{name}/{module}"),
                    None => format!("插件/{name}"),
                })
            })
            .unwrap_or_else(|| {
                format!(
                    "外部/{}",
                    meta.target().split("::").next().unwrap_or(meta.target())
                )
            });
        let source = one_line(&source);
        let source_style = if flow_source.is_some() {
            "34"
        } else if plugin_source.is_some() {
            "35"
        } else {
            "36"
        };
        let timestamp = chrono::Local::now().format("%m-%d %H:%M:%S");
        let level = meta.level().as_str();
        let color = match *meta.level() {
            tracing::Level::ERROR => "31",
            tracing::Level::WARN => "33",
            tracing::Level::INFO => "32",
            tracing::Level::DEBUG => "36",
            _ => "90",
        };
        write_styled(&mut writer, "2", format_args!("{timestamp}"))?;
        writer.write_char(' ')?;
        write_styled(&mut writer, color, format_args!("[{level}]"))?;
        let level_padding = tracing::Level::ERROR
            .as_str()
            .len()
            .saturating_sub(level.len());
        write!(writer, "{:level_padding$} ", "")?;
        write_styled(&mut writer, source_style, format_args!("[{source}]"))?;
        writer.write_char(' ')?;
        if let Some(plugin) = &plugin
            && plugin_source.is_none()
        {
            let plugin = one_line(plugin);
            write_styled(&mut writer, "2", format_args!("{plugin}"))?;
            if meta.target() == "nuono::plugins" {
                // 只为插件加载列表对齐名称，分类标签和业务日志保持紧凑。
                let padding = self
                    .plugins
                    .width
                    .load(Ordering::Relaxed)
                    .saturating_sub(plugin.width());
                write!(writer, "{:padding$} ", "")?;
            } else if flow_source == Some("消息/发送") {
                writer.write_str(" → ")?;
            } else {
                writer.write_str(" · ")?;
            }
        }
        if let Some(icon) = event_icon(meta.target(), meta.level(), &message) {
            write!(writer, "{icon} ")?;
        }
        let body = flow.as_ref().map(FlowSummary::body).unwrap_or(&message);
        let body = one_line(body);
        let separator = if flow_source == Some("消息/接收") {
            ": "
        } else {
            "："
        };
        if flow_source.is_some()
            && let Some((context, content)) = body.split_once(separator)
        {
            write_styled(&mut writer, "2", format_args!("{context}{separator}"))?;
            writer.write_str(content)?;
        } else {
            writer.write_str(&body)?;
        }
        if flow_source == Some("消息/发送") {
            let state = visitor
                .fields
                .get("send_state")
                .and_then(serde_json::Value::as_str);
            let label = match state {
                Some("confirmed") => "接口已确认",
                Some("submitted") => "请求已发出",
                Some("queued") => "请求已入队",
                Some("failed") => "发送失败",
                Some("timeout") => "回执超时，结果未确认",
                Some("unconfirmed" | "cancelled") => "发送结果未确认",
                _ => "",
            };
            if !label.is_empty() {
                let style = if matches!(*meta.level(), tracing::Level::WARN | tracing::Level::ERROR)
                {
                    color
                } else {
                    "2"
                };
                write_styled(&mut writer, style, format_args!("（{label}）"))?;
            }
        }
        if flow.is_none()
            && let Some(bot_id) = visitor.bot_id
        {
            write_styled(
                &mut writer,
                "2",
                format_args!(" bot_id={}", one_line(&bot_id)),
            )?;
        }
        for (name, value) in &visitor.fields {
            if name == "plugin"
                || name == "plugin_name"
                || name == "plugin_target"
                || flow.as_ref().is_some_and(|flow| flow.duplicates(name))
            {
                continue;
            }
            if *meta.level() == tracing::Level::INFO
                && meta.target() == "nuono::plugins"
                && matches!(
                    name.as_str(),
                    "duration_ms" | "installed" | "skipped" | "enabled" | "disabled"
                )
            {
                continue;
            }
            if matches!(name.as_str(), "error" | "reason" | "cause") {
                write!(writer, " {name}={value}")?;
            } else {
                write_styled(&mut writer, "2", format_args!(" {name}={value}"))?;
            }
        }
        if self.include_target {
            write_styled(
                &mut writer,
                "2",
                format_args!(" target={}", one_line(meta.target())),
            )?;
        }
        if self.include_location
            && let Some(file) = meta.file()
        {
            write_styled(&mut writer, "2", format_args!(" at {}", one_line(file)))?;
            if let Some(line) = meta.line() {
                write_styled(&mut writer, "2", format_args!(":{line}"))?;
            }
        }
        writeln!(writer)
    }
}

#[cfg(test)]
mod tests {
    use super::super::RuntimeLogEvent;
    use super::*;
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };
    use tracing_subscriber::prelude::*;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Capture {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct RuntimeCapture(Arc<Mutex<Vec<RuntimeLogEvent>>>);

    impl<S: Subscriber> tracing_subscriber::Layer<S> for RuntimeCapture {
        fn on_event(&self, event: &Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            let mut visitor = LogVisitor::default();
            event.record(&mut visitor);
            visitor.fallback_message(event.metadata());
            self.0
                .lock()
                .unwrap()
                .push(visitor.into_event(event.metadata()));
        }
    }

    #[tokio::test]
    async fn shared_dependencies_keep_their_calling_plugin_in_console_json_and_live_events() {
        use nuono_plugin::core::log_context;
        use tracing::instrument::WithSubscriber;
        let console = Capture::default();
        let json = Capture::default();
        let live = RuntimeCapture::default();
        let console_writer = console.clone();
        let json_writer = json.clone();
        let subscriber = tracing_subscriber::registry()
            // 即使全局过滤器关闭了回调所在的 INFO 级别 span，插件归属标识仍须保留。
            .with(tracing_subscriber::EnvFilter::new("warn"))
            .with(tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .event_format(ConsoleFormat::default())
                .with_writer(move || console_writer.clone()))
            .with(tracing_subscriber::fmt::layer().json()
                .with_ansi(false)
                .map_event_format(super::super::context::WithPluginContext::json)
                .with_writer(move || json_writer.clone()))
            .with(live.clone());
        let dispatcher = tracing::Dispatch::new(subscriber);
        let request = |plugin: &'static str| {
            log_context::scope(Some(plugin.into()), async move {
                tokio::task::yield_now().await;
                tracing::warn!(target: "bpi_rs::client", request = plugin, "请求失败");
            })
            .with_subscriber(dispatcher.clone())
        };
        tokio::join!(request("DemoPlugin"), request("AnotherPlugin"));
        tracing::dispatcher::with_default(&dispatcher, || {
            tracing::warn!(target: "bpi_rs::client", "未绑定插件的请求");
        });

        let text = console.text();
        for plugin in ["DemoPlugin", "AnotherPlugin"] {
            let line = text
                .lines()
                .find(|line| line.contains(&format!("request=\"{plugin}\"")))
                .unwrap();
            assert!(line.contains(&format!("[插件/{plugin}/bpi_rs]")), "{line}");
        }
        let unbound = text
            .lines()
            .find(|line| line.contains("未绑定插件的请求"))
            .unwrap();
        assert!(unbound.contains("[外部/bpi_rs]"), "{unbound}");
        let records: Vec<serde_json::Value> = json
            .text()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 3);
        for record in &records[..2] {
            assert_eq!(record["target"], "bpi_rs::client");
            assert_eq!(record["fields"]["plugin"], record["fields"]["request"]);
        }
        assert!(records[2]["fields"].get("plugin").is_none());
        let live = live.0.lock().unwrap();
        assert_eq!(live.len(), 3);
        for event in &live[..2] {
            assert_eq!(event.target, "bpi_rs::client");
            assert_eq!(event.fields["plugin"], event.fields["request"]);
        }
        assert!(!live[2].fields.contains_key("plugin"));
    }

    #[test]
    fn info_stays_compact_and_host_supervision_keeps_its_source() {
        let output = Capture::default();
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .event_format(ConsoleFormat::default())
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            nuono_plugin::core::log_context::scope_blocking(Some("DemoPlugin".into()), || {
                tracing::info!(target: "bpi_rs::client", "请求完成");
                tracing::debug!(target: "bpi_rs::client", "请求细节");
                tracing::info!(target: "demo_plugin::download", "下载已排队");
                tracing::warn!(target: "nuono_core::core::plugin_host::diagnostics", plugin = "OtherPlugin", "插件慢调用");
            });
        });
        let text = output.text();
        let rows: Vec<_> = text.lines().collect();
        assert!(rows[0].contains("[插件/DemoPlugin] 请求完成"));
        assert!(rows[1].contains("[插件/DemoPlugin/bpi_rs] 请求细节"));
        assert!(rows[2].contains("[插件/DemoPlugin] 下载已排队"));
        assert!(rows[3].contains("[核心/插件] OtherPlugin ·"));
        assert!(!rows[3].contains("DemoPlugin"));
    }

    #[test]
    fn file_context_preserves_span_fields_and_explicit_owner() {
        let json = Capture::default();
        let text = Capture::default();
        let json_writer = json.clone();
        let text_writer = text.clone();
        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_ansi(false)
                    .map_event_format(super::super::context::WithPluginContext::json)
                    .with_writer(move || json_writer.clone()),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .map_event_format(super::super::context::WithPluginContext::text)
                    .with_writer(move || text_writer.clone()),
            );
        tracing::subscriber::with_default(subscriber, || {
            nuono_plugin::core::log_context::scope_blocking(Some("DemoPlugin".into()), || {
                let span = tracing::info_span!("request", trace_id = "trace-fixture");
                let _entered = span.enter();
                tracing::info!(target: "bpi_rs::client", attempt = 2_u64, "request details");
                tracing::info!(target: "nuono_plugin::host_log", plugin = "ExplicitPlugin", plugin_target = "native.module", "bridged details");
            });
        });
        let records: Vec<serde_json::Value> = json
            .text()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records[0]["fields"]["plugin"], "DemoPlugin");
        assert_eq!(records[0]["fields"]["attempt"], 2);
        assert_eq!(records[0]["target"], "bpi_rs::client");
        assert_eq!(records[0]["span"]["trace_id"], "trace-fixture");
        assert_eq!(records[1]["fields"]["plugin"], "ExplicitPlugin");
        let text = text.text();
        let rows: Vec<_> = text.lines().collect();
        assert!(rows[0].contains("plugin=\"DemoPlugin\""));
        assert!(rows[1].contains("ExplicitPlugin"));
        assert!(!rows[1].contains("DemoPlugin"));
    }

    #[test]
    fn plugin_origin_is_distinct_from_host_supervision_and_preserves_failure_details() {
        let output = Capture::default();
        let writer = output.clone();
        let format = ConsoleFormat::default();
        register_plugin_names(&format.plugins, ["DemoPlugin", "BillingDemo"]);
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .event_format(format)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "demo_plugin::download", "视频下载已排队");
            tracing::info!(target: "billing_demo", "扣费完成");
            tracing::warn!(target: "nuono_core::core::plugin_host::diagnostics",
                plugin = "DemoPlugin", callback = "on_message", error = "调用超时", "插件回调失败");
            tracing::info!(target: "nuono_plugin::host_log", plugin = "DemoPlugin",
                plugin_target = "download.internal", "媒体已准备好");
            tracing::warn!(target: "message_flow", plugin = "DemoPlugin", direction = "send",
                platform = "onebot_v11", send_state = "timeout", request_id = "request-1",
                error = "等待平台回执超时", "[发送][onebot_v11] 群(100) => [视频]");
        });
        let text = output.text();
        let rows: Vec<_> = text.lines().collect();
        assert!(rows[0].contains("[插件/DemoPlugin] 视频下载已排队"));
        assert!(rows[1].contains("[插件/BillingDemo] 扣费完成"));
        assert!(rows[2].contains("[核心/插件] DemoPlugin ·"));
        assert!(rows[2].contains("调用超时") && rows[2].contains("on_message"));
        assert!(rows[3].contains("[插件/DemoPlugin] 媒体已准备好"));
        assert!(rows[4].contains("[消息/发送] DemoPlugin →"));
        assert!(rows[4].contains("结果未确认") && rows[4].contains("等待平台回执超时"));
        assert!(!rows[4].contains("接口已确认"));
    }

    #[test]
    fn plugin_list_width_is_local_to_each_batch() {
        let names = PluginNames::default();
        register_plugin_names(&names, ["LongPluginName", "短"]);
        register_plugin_names(&names, ["短"]);
        assert_eq!(names.width.load(Ordering::Relaxed), 2);
        assert_eq!(
            names.display("long_plugin_name::download").as_deref(),
            Some("LongPluginName")
        );
    }

    #[test]
    fn compact_messages_preserve_arrows_in_content_and_plugin_internal_module_names() {
        let capture = Capture::default();
        let writer = capture.clone();
        let format = ConsoleFormat::default();
        register_plugin_names(&format.plugins, ["DemoPlugin"]);
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .event_format(format)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "message_flow", direction = "receive", "[接收][onebot_v11] 群(1) <= 小明(2): a => b <= c");
            tracing::info!(target: "message_flow", direction = "send", "[发送][onebot_v11] 群(1) => a <= b => c");
            tracing::info!(target: "demo_plugin::handler", "插件业务处理");
        });
        let output = capture.text();
        let rows: Vec<_> = output.lines().collect();
        assert!(rows[0].ends_with("小明(2): a => b <= c"));
        assert!(rows[1].ends_with("群(1)：a <= b => c"));
        assert!(rows[2].contains("[插件/DemoPlugin]"));
    }

    #[test]
    fn chat_console_is_concise_while_json_and_runtime_keep_diagnostic_fields() {
        use crate::core::message_log::{inbound_summary, log_receive, log_send, outbound_summary};
        use serde_json::json;

        let received = inbound_summary(
            &json!({
                "post_type":"message", "message_type":"group", "group_id":100,
                "user_id":7, "sender":{"card":"小明"}, "message_id":"m-1",
                "raw_message":"你好",
            }),
            400,
        )
        .unwrap();
        let sent = outbound_summary(
            "onebot_v11",
            &json!({"action":"send_group_msg", "params":{"group_id":100,"message":"收到"}}),
            400,
        )
        .unwrap();
        let private = inbound_summary(
            &json!({
                "post_type":"message", "message_type":"private", "platform":"discord",
                "channel_id":"dm-7", "user_id":7, "raw_message":"不会输出的私人正文",
            }),
            400,
        )
        .unwrap();
        for ansi in [false, true] {
            let console = Capture::default();
            let console_writer = console.clone();
            let json = Capture::default();
            let json_writer = json.clone();
            let runtime = RuntimeCapture::default();
            let subscriber = tracing_subscriber::registry()
                .with(
                    tracing_subscriber::fmt::layer()
                        .event_format(ConsoleFormat::default())
                        .with_ansi(ansi)
                        .with_writer(move || console_writer.clone()),
                )
                .with(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .with_ansi(false)
                        .with_writer(move || json_writer.clone()),
                )
                .with(runtime.clone());
            tracing::subscriber::with_default(subscriber, || {
                log_receive("42", &received);
                log_send("42", &sent);
                tracing::info!(target: "message_flow", bot_id = "42", plugin = "BetaPlugin",
                    trigger_type = "EventCommand", trigger = "自定义", propagation_stopped = true,
                    "[HANDLED] EventCommand 自定义，停止后续处理");
                log_receive("42", &private);
            });
            let text = console.text();
            assert_eq!(text.contains('\x1b'), ansi);
            if ansi {
                assert!(text.starts_with("\x1b[2m"), "时间应弱化显示");
                assert!(text.contains("\x1b[34m[消息/接收]\x1b[0m"));
                assert!(text.contains("\x1b[34m[消息/发送]\x1b[0m"));
                assert!(text.contains("\x1b[36m[核心/分发]\x1b[0m"));
            }
            let plain = super::super::ANSI_RE.replace_all(&text, "");
            let rows: Vec<_> = plain.lines().map(|line| &line[15..]).collect();
            assert_eq!(
                rows,
                [
                    "[INFO]  [消息/接收] OneBot v11 · 群(100) · 小明(7): 你好",
                    "[INFO]  [消息/发送] OneBot v11 · 群(100)：收到（请求已发出）",
                    "[INFO]  [核心/分发] BetaPlugin · 已处理「自定义」，停止后续处理",
                    "[INFO]  [消息/接收] Discord · 私聊(dm-7) · [私人消息正文已隐藏]",
                ]
            );
            assert!(!plain.contains("不会输出的私人正文"));
            let records: Vec<serde_json::Value> = json
                .text()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(records.len(), 4);
            assert_eq!(records[0]["target"], "message_flow");
            assert_eq!(records[0]["fields"]["message"], received.line);
            assert_eq!(records[0]["fields"]["bot_id"], "42");
            assert_eq!(records[0]["fields"]["direction"], "receive");
            assert_eq!(records[0]["fields"]["platform"], "onebot_v11");
            assert_eq!(records[0]["fields"]["conversation_id"], "100");
            assert_eq!(records[0]["fields"]["sender_id"], "7");
            assert_eq!(records[0]["fields"]["message_id"], "m-1");
            assert_eq!(records[0]["fields"]["content_length"], 2);
            assert_eq!(records[0]["fields"]["truncated"], false);
            assert_eq!(records[1]["fields"]["message"], sent.line);
            assert_eq!(
                records[2]["fields"]["message"],
                "[HANDLED] EventCommand 自定义，停止后续处理"
            );
            assert_eq!(records[2]["fields"]["trigger_type"], "EventCommand");
            assert_eq!(records[2]["fields"]["propagation_stopped"], true);
            let runtime = runtime.0.lock().unwrap();
            assert_eq!(runtime[0].message, received.line);
            assert_eq!(runtime[0].bot_id, "42");
            assert_eq!(runtime[0].fields["message_id"], "m-1");
            assert_eq!(runtime[1].fields["direction"], "send");
            assert_eq!(runtime[2].fields["trigger"], "自定义");
            assert_eq!(runtime[3].fields["conversation_id"], "dm-7");
            assert!(
                !serde_json::to_string(&*runtime)
                    .unwrap()
                    .contains("不会输出的私人正文")
            );
        }
    }

    #[test]
    fn compact_chat_keeps_errors_unknown_fields_and_debug_details() {
        let console = Capture::default();
        let writer = console.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .event_format(ConsoleFormat::default())
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "message_flow", bot_id = "42", direction = "receive",
                platform = "onebot_v11", duration_ms = 12, error = "附加说明",
                "[接收][onebot_v11] 群(100) <= 小明(7): 你好");
            tracing::warn!(target: "message_flow", bot_id = "42", direction = "send",
                message_id = "m-1", error = "连接已关闭",
                "[发送][onebot_v11] 群(100) => 重试");
            tracing::error!(target: "message_flow", bot_id = "42", direction = "send",
                message_id = "m-1", error = "连接已关闭", "消息发送失败");
            tracing::debug!(target: "message_flow", bot_id = "42", direction = "receive",
                message_id = "m-1", "[接收][onebot_v11] 群(100) <= 小明(7): 你好");
            tracing::trace!(target: "message_flow", bot_id = "42", direction = "send",
                message_id = "m-1", "[发送][onebot_v11] 群(100) => 收到");
            tracing::info!(target: "message_flow", bot_id = "42", direction = "receive",
                message_id = "m-1", "其他消息流事件");
            tracing::info!(target: "plugin_custom", bot_id = "42", direction = "send",
                message_id = "m-1", "插件自己的日志");
        });
        let text = console.text();
        let rows: Vec<_> = text.lines().collect();
        assert_eq!(rows.len(), 7);
        assert!(rows[0].contains("[消息/接收] OneBot v11"));
        assert!(rows[0].contains("duration_ms=12") && rows[0].contains("error=\"附加说明\""));
        assert!(!rows[0].contains("direction=") && !rows[0].contains("bot_id="));
        for row in &rows[1..] {
            assert!(
                row.contains("bot_id=42") && row.contains("message_id=\"m-1\""),
                "{row}"
            );
            assert!(row.contains("direction="), "{row}");
        }
        assert!(rows[1].contains("error=\"连接已关闭\""));
        assert!(rows[2].contains("error=\"连接已关闭\""));
        assert!(rows[3].contains("[DEBUG] [核心/分发]"));
        assert!(rows[6].contains("[外部/plugin_custom]"));
    }

    #[test]
    fn console_aligns_only_plugin_list_and_preserves_compact_sources_and_chinese() {
        let names = [
            "AlphaNN",
            "BetaPlugin",
            "中文插件",
            "Cafe\u{301}",
            "小👩‍💻",
            "NuonoUrlResolver",
        ];
        let message = "管理员账号初始化检查完成（耗时 0.00 秒） Agent 遗留任务检查…";
        for ansi in [false, true] {
            let output = Capture::default();
            let writer = output.clone();
            let format = ConsoleFormat::default();
            register_plugin_names(&format.plugins, names);
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(ansi)
                .event_format(format)
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                tracing::info!(target: "nuono::startup", "{message}");
                tracing::info!(target: "sea_orm_migration::migrator::exec", "{message}");
                for name in names {
                    tracing::info!(target: "nuono::plugins", plugin = name, "{message}");
                }
                tracing::info!(target: "nuono::plugins", plugin_target = "BetaPlugin", "{message}");
                tracing::info!(target: "LongerRuntimePluginName", "{message}");
                tracing::info!(target: "nuono::plugins", plugin = "BetaPlugin", "{message}");
                tracing::info!(target: "nuono::http", "{message}");
                tracing::info!(target: "beta_plugin", plugin = "BetaPlugin", "{message}");
                tracing::info!(target: "nuono::startup", "{message}");
            });
            let text = output.text();
            assert_eq!(text.contains('\x1b'), ansi);
            let plain = super::super::ANSI_RE.replace_all(&text, "");
            let rows: Vec<_> = plain
                .lines()
                .map(|line| line.split_once("[INFO]  ").unwrap().1)
                .collect();
            assert_eq!(rows.len(), names.len() + 8);
            assert_eq!(rows[0], format!("[核心/启动] {message}"));
            assert_eq!(rows[1], format!("[核心/数据库] {message}"));
            for (index, name) in names.into_iter().enumerate() {
                let padded_name = rows[index + 2]
                    .strip_prefix("[核心/插件] ")
                    .unwrap()
                    .strip_suffix(message)
                    .expect("中文正文及原有空格应保持原样")
                    .strip_suffix(' ')
                    .unwrap();
                assert_eq!(padded_name.trim_end(), name);
                assert_eq!(padded_name.width(), 16, "插件 {name} 未按最长名称对齐");
            }
            assert_eq!(rows[3], format!("[核心/插件] BetaPlugin       {message}"));
            assert_eq!(rows[names.len() + 2], rows[3]);
            assert_eq!(
                rows[names.len() + 3],
                format!("[外部/LongerRuntimePluginName] {message}")
            );
            assert_eq!(rows[names.len() + 4], rows[3]);
            assert_eq!(rows[names.len() + 5], format!("[核心/服务] {message}"));
            assert_eq!(
                rows[names.len() + 6],
                format!("[插件/BetaPlugin] {message}")
            );
            assert_eq!(rows[names.len() + 7], rows[0]);
        }
    }

    #[test]
    fn icons_and_compact_sources_only_change_console_presentation() {
        let console = Capture::default();
        let console_writer = console.clone();
        let json = Capture::default();
        let json_writer = json.clone();
        let runtime = RuntimeCapture::default();
        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .event_format(ConsoleFormat {
                        include_target: false,
                        include_location: false,
                        ..Default::default()
                    })
                    .with_ansi(false)
                    .with_writer(move || console_writer.clone()),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_ansi(false)
                    .with_writer(move || json_writer.clone()),
            )
            .with(runtime.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "nuono::startup", phase = "listen", "HTTP 服务已就绪");
            tracing::info!(target: "nuono_core::core::connection", bot_id = 42_u64, "[BotConnected] Bot 连接已接入");
            tracing::info!(target: "nuono_core::core::connection", bot_id = 42_u64, "[BotDisconnected] Bot 连接已断开");
            tracing::warn!(target: "nuono::startup", attempt = 2_u64, "连接重试");
            tracing::error!(target: "demo_plugin", ok = false, "媒体下载失败");
            tracing::info!(target: "nuono_core::core::connection", "🔗 新的 WebSocket 连接已建立");
            tracing::warn!(target: "nuono::startup", "⚠️ 已有警告图标");
            tracing::info!(target: "nuono_core::core::connection", bytes = 128_u64, "收到 WebSocket 文本帧");
            tracing::info!(target: "demo_plugin", plugin = "短插件🙂", "媒体下载完成");
        });
        let console_text = console.text();
        let lines: Vec<_> = console_text.lines().collect();
        let expected = [
            (
                "INFO",
                "nuono::startup",
                "HTTP 服务已就绪",
                "[INFO]  [核心/启动] HTTP 服务已就绪 phase=\"listen\"",
            ),
            (
                "INFO",
                "nuono_core::core::connection",
                "[BotConnected] Bot 连接已接入",
                "[INFO]  [核心/连接] 🔗 [BotConnected] Bot 连接已接入 bot_id=42",
            ),
            (
                "INFO",
                "nuono_core::core::connection",
                "[BotDisconnected] Bot 连接已断开",
                "[INFO]  [核心/连接] 🔌 [BotDisconnected] Bot 连接已断开 bot_id=42",
            ),
            (
                "WARN",
                "nuono::startup",
                "连接重试",
                "[WARN]  [核心/启动] ⚠️ 连接重试 attempt=2",
            ),
            (
                "ERROR",
                "demo_plugin",
                "媒体下载失败",
                "[ERROR] [外部/demo_plugin] ❌ 媒体下载失败 ok=false",
            ),
            (
                "INFO",
                "nuono_core::core::connection",
                "🔗 新的 WebSocket 连接已建立",
                "[INFO]  [核心/连接] 🔗 新的 WebSocket 连接已建立",
            ),
            (
                "WARN",
                "nuono::startup",
                "⚠️ 已有警告图标",
                "[WARN]  [核心/启动] ⚠️ 已有警告图标",
            ),
            (
                "INFO",
                "nuono_core::core::connection",
                "收到 WebSocket 文本帧",
                "[INFO]  [核心/连接] 收到 WebSocket 文本帧 bytes=128",
            ),
            (
                "INFO",
                "demo_plugin",
                "媒体下载完成",
                "[INFO]  [插件/短插件🙂] 媒体下载完成",
            ),
        ];
        let json_records: Vec<serde_json::Value> = json
            .text()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let runtime_records = runtime.0.lock().unwrap();
        assert_eq!(lines.len(), expected.len());
        assert_eq!(json_records.len(), expected.len());
        assert_eq!(runtime_records.len(), expected.len());
        for (index, (level, target, message, rendered)) in expected.into_iter().enumerate() {
            assert_eq!(&lines[index][15..], rendered);
            assert_eq!(json_records[index]["level"], level);
            assert_eq!(json_records[index]["target"], target);
            assert_eq!(json_records[index]["fields"]["message"], message);
            let runtime = &runtime_records[index];
            assert_eq!(runtime.level, level);
            assert_eq!(runtime.target, target);
            assert_eq!(runtime.message, message);
        }
        assert_eq!(runtime_records[1].bot_id, "42");
        assert_eq!(json_records[1]["fields"]["bot_id"], 42);
        assert_eq!(runtime_records[3].fields["attempt"], 2);
        assert_eq!(runtime_records[4].fields["ok"], false);
        assert_eq!(runtime_records[8].fields["plugin"], "短插件🙂");
    }

    #[test]
    fn console_is_short_while_json_retains_plugin_fields_and_span_context() {
        let console = Capture::default();
        let console_writer = console.clone();
        let json = Capture::default();
        let json_writer = json.clone();
        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .event_format(ConsoleFormat {
                        include_target: false,
                        include_location: false,
                        ..Default::default()
                    })
                    .with_ansi(false)
                    .with_writer(move || console_writer.clone()),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_ansi(false)
                    .with_writer(move || json_writer.clone()),
            );
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("init", phase = "startup");
            let _entered = span.enter();
            tracing::info!(target: "nuono::plugins", plugin = "NuonoUrlResolver", count = 2, "就绪（0.03 秒）");
        });
        let text = console.text();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("[INFO]  [核心/插件] NuonoUrlResolver 就绪"));
        assert!(text.contains("就绪（0.03 秒） count=2"));
        assert!(!text.contains("init:") && !text.contains("phase="));
        assert!(!text.contains('\x1b'));
        assert!(
            chrono::NaiveDateTime::parse_from_str(
                &format!("2000-{}", &text[..14]),
                "%Y-%m-%d %H:%M:%S"
            )
            .is_ok()
        );
        let record: serde_json::Value = serde_json::from_str(json.text().trim()).unwrap();
        assert_eq!(record["fields"]["plugin"], "NuonoUrlResolver");
        assert_eq!(record["fields"]["count"], 2);
        assert_eq!(record["span"]["name"], "init");
        assert_eq!(record["span"]["phase"], "startup");
    }

    #[test]
    fn console_redacts_credentials_and_escapes_multiline_controls() {
        let output = Capture::default();
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .event_format(ConsoleFormat {
                include_target: true,
                include_location: true,
                ..Default::default()
            })
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(target: "nuono::startup", api_key = "test-credential", attempts = 2,
                "发生错误\n请检查 '配置' https://user:test-pass@example.test/x");
        });
        let text = output.text();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("[WARN]  [核心/启动] ⚠️ 发生错误"));
        assert!(text.contains("\\n请检查 '配置'"));
        assert!(text.contains("attempts=2"));
        assert!(text.contains("target=nuono::startup") && text.contains(" at "));
        assert!(!text.contains("test-credential") && !text.contains("test-pass"));
        assert!(text.contains("[REDACTED]"));
    }
}
