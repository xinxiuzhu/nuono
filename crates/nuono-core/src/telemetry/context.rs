//! 在保持标准文件日志格式不变的前提下，自动附加上下文中的当前执行插件标识。
use super::{LOG_FIELD_MAX_BYTES, LogVisitor, redact_text, truncate_utf8};
use std::fmt;
use tracing::{Event, Subscriber};
use tracing_subscriber::{
    fmt::{FmtContext, FormatEvent, FormatFields, format::Writer},
    registry::LookupSpan,
};

enum Output {
    Json,
    Text,
}

pub(super) struct WithPluginContext<F> {
    inner: F,
    output: Output,
}

impl<F> WithPluginContext<F> {
    pub(super) fn json(inner: F) -> Self {
        Self {
            inner,
            output: Output::Json,
        }
    }

    pub(super) fn text(inner: F) -> Self {
        Self {
            inner,
            output: Output::Text,
        }
    }
}

impl<S, N, F> FormatEvent<S, N> for WithPluginContext<F>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let Some(plugin) = nuono_plugin::core::log_context::current_plugin() else {
            return self.inner.format_event(ctx, writer, event);
        };
        let mut visitor = LogVisitor::default();
        event.record(&mut visitor);
        if visitor.explicit_plugin().is_some() {
            return self.inner.format_event(ctx, writer, event);
        }
        let (plugin, _) = truncate_utf8(&redact_text(&plugin), LOG_FIELD_MAX_BYTES);
        // 标准 JSON 格式化器会保留 span 字段与源码位置；仅当处于插件作用域内时，才在写入非阻塞文件流前额外注入 plugin 字段。
        let mut line = String::new();
        self.inner
            .format_event(ctx, Writer::new(&mut line), event)?;
        match self.output {
            Output::Json => {
                let mut record: serde_json::Value =
                    serde_json::from_str(&line).map_err(|_| fmt::Error)?;
                let fields = record
                    .get_mut("fields")
                    .and_then(serde_json::Value::as_object_mut)
                    .ok_or(fmt::Error)?;
                fields.insert("plugin".into(), plugin.into());
                writeln!(writer, "{record}")
            }
            Output::Text => writeln!(writer, "{} plugin={plugin:?}", line.trim_end_matches('\n')),
        }
    }
}
