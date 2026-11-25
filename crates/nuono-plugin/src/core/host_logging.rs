use serde::{Deserialize, Serialize};
use serde_json::Value;

const HOST_LOG_TARGET: &str = "nuono_plugin::host_log";

/// 插件宿主日志桥支持的日志级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum HostLogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// 由插件发出并交由宿主统一分发的结构化日志记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostLogRecord {
    pub level: HostLogLevel,
    pub target: String,
    /// 宿主分配的插件名；日志 target 可以继续表示插件内部模块。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub fields: Value,
}

impl HostLogRecord {
    pub fn new(level: HostLogLevel, target: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level,
            target: target.into(),
            plugin: None,
            message: message.into(),
            fields: Value::Null,
        }
    }

    pub fn with_fields(mut self, fields: Value) -> Self {
        self.fields = fields;
        self
    }

    /// 附加明确的插件来源。
    pub fn with_plugin(mut self, plugin: Option<String>) -> Self {
        self.plugin = plugin;
        self
    }
}

/// 连接到宿主 `tracing` 订阅器的进程内日志桥。
///
/// 在静态链接的二进制中，插件与宿主共享同一套 `tracing` 实现；
/// 本日志桥会在预先捕获的宿主订阅器上下文中发出日志事件。
#[derive(Clone)]
pub struct HostLogBridge {
    dispatch: tracing::Dispatch,
    emit_fn: fn(&HostLogRecord),
}

impl std::fmt::Debug for HostLogBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostLogBridge").finish_non_exhaustive()
    }
}

impl HostLogBridge {
    pub fn capture_current() -> Self {
        Self {
            dispatch: tracing::dispatcher::get_default(Clone::clone),
            emit_fn: emit_record,
        }
    }

    pub fn emit(&self, mut record: HostLogRecord) {
        record.plugin = record.plugin.or_else(super::log_context::current_plugin);
        tracing::dispatcher::with_default(&self.dispatch, || (self.emit_fn)(&record));
    }
}

fn emit_record(record: &HostLogRecord) {
    let plugin = record.plugin.as_deref();
    let plugin_target = record.target.as_str();
    let message = record.message.as_str();
    let fields_json = record.fields.to_string();

    match record.level {
        HostLogLevel::Trace => tracing::event!(
            target: HOST_LOG_TARGET,
            tracing::Level::TRACE,
            plugin,
            plugin_target,
            fields_json,
            "{message}"
        ),
        HostLogLevel::Debug => tracing::event!(
            target: HOST_LOG_TARGET,
            tracing::Level::DEBUG,
            plugin,
            plugin_target,
            fields_json,
            "{message}"
        ),
        HostLogLevel::Info => tracing::event!(
            target: HOST_LOG_TARGET,
            tracing::Level::INFO,
            plugin,
            plugin_target,
            fields_json,
            "{message}"
        ),
        HostLogLevel::Warn => tracing::event!(
            target: HOST_LOG_TARGET,
            tracing::Level::WARN,
            plugin,
            plugin_target,
            fields_json,
            "{message}"
        ),
        HostLogLevel::Error => tracing::event!(
            target: HOST_LOG_TARGET,
            tracing::Level::ERROR,
            plugin,
            plugin_target,
            fields_json,
            "{message}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::{Layer, layer::Context, prelude::*};

    #[derive(Debug, PartialEq, Eq)]
    struct CapturedEvent {
        level: tracing::Level,
        target: String,
        plugin: String,
        plugin_target: String,
        message: String,
        fields: String,
    }

    #[derive(Default)]
    struct EventVisitor {
        plugin: String,
        plugin_target: String,
        message: String,
        fields: String,
    }

    impl Visit for EventVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            match field.name() {
                "plugin" => self.plugin = format!("{value:?}").trim_matches('"').into(),
                "plugin_target" => {
                    self.plugin_target = format!("{value:?}").trim_matches('"').into()
                }
                "message" => self.message = format!("{value:?}").trim_matches('"').into(),
                "fields_json" => self.fields = format!("{value:?}"),
                _ => {}
            }
        }
    }

    struct CaptureLayer(Arc<Mutex<Vec<CapturedEvent>>>);

    impl<S> Layer<S> for CaptureLayer
    where
        S: tracing::Subscriber,
    {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut visitor = EventVisitor::default();
            event.record(&mut visitor);
            self.0.lock().unwrap().push(CapturedEvent {
                level: *event.metadata().level(),
                target: event.metadata().target().to_string(),
                plugin: visitor.plugin,
                plugin_target: visitor.plugin_target,
                message: visitor.message,
                fields: visitor.fields,
            });
        }
    }

    #[test]
    fn bridge_uses_captured_host_dispatch_for_all_levels_and_fields() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(CaptureLayer(captured.clone()));
        let bridge = tracing::subscriber::with_default(subscriber, HostLogBridge::capture_current);

        let levels = [
            HostLogLevel::Trace,
            HostLogLevel::Debug,
            HostLogLevel::Info,
            HostLogLevel::Warn,
            HostLogLevel::Error,
        ];
        for level in levels {
            bridge.emit(
                HostLogRecord::new(level, "native.adapter", format!("{level:?}"))
                    .with_fields(serde_json::json!({"attempt": 2, "ok": true})),
            );
        }

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 5);
        assert_eq!(captured[0].level, tracing::Level::TRACE);
        assert_eq!(captured[1].level, tracing::Level::DEBUG);
        assert_eq!(captured[2].level, tracing::Level::INFO);
        assert_eq!(captured[3].level, tracing::Level::WARN);
        assert_eq!(captured[4].level, tracing::Level::ERROR);
        assert!(captured.iter().all(|event| event.target == HOST_LOG_TARGET));
        assert!(
            captured
                .iter()
                .all(|event| event.plugin_target == "native.adapter")
        );
        assert!(
            captured
                .iter()
                .all(|event| event.fields.contains("attempt"))
        );
        assert_eq!(captured[2].message, "Info");
    }

    #[test]
    fn bridge_keeps_dependency_target_and_inherits_only_missing_ownership() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(CaptureLayer(captured.clone()));
        let bridge = tracing::subscriber::with_default(subscriber, HostLogBridge::capture_current);
        crate::core::log_context::scope_blocking(Some("DemoPlugin".into()), || {
            bridge.emit(HostLogRecord::new(
                HostLogLevel::Warn,
                "bpi_rs::client",
                "request failed",
            ));
            bridge.emit(
                HostLogRecord::new(HostLogLevel::Warn, "another::client", "explicit owner")
                    .with_plugin(Some("OtherPlugin".into())),
            );
        });
        let records = captured.lock().unwrap();
        assert_eq!(records[0].plugin, "DemoPlugin");
        assert_eq!(records[0].plugin_target, "bpi_rs::client");
        assert_eq!(records[1].plugin, "OtherPlugin");
    }
}
