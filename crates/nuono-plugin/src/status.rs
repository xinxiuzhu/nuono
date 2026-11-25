//! 在框架 `nuono状态` 卡片上展示的公开汇总指标。
//!
//! 这些指标会展示给任何有权限调用 `nuono状态` 的用户，因此严禁包含账号标识、
//! 密钥凭据、用户个人记录或特定群组的私有数据。

use chrono::{DateTime, Utc};

/// 单个插件在状态卡片上最多可展示的指标数量。
pub const MAX_PLUGIN_STATUS_METRICS: usize = 4;
/// 指标名称（label）的最大 Unicode 字符数。
pub const MAX_PLUGIN_STATUS_LABEL_CHARS: usize = 16;
/// 格式化后的指标值（value）的最大 Unicode 字符数。
pub const MAX_PLUGIN_STATUS_VALUE_CHARS: usize = 24;

/// 公开汇总指标查询请求（刻意不包含调用者身份信息，确保返回内容与调用者无关）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginStatusRequest {
    /// 生成整张状态卡片时的统一快照时间戳，便于各插件按同一时间基准统计当日汇总数据。
    pub generated_at: DateTime<Utc>,
}

/// 单条简短的纯文本公开指标（数值与单位由插件自行格式化）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PluginStatusMetric {
    pub label: String,
    pub value: String,
}

impl PluginStatusMetric {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

/// 在宿主渲染展示前，校验插件返回的全部状态指标。
///
/// 返回空列表属于合法情况（此时状态卡会隐藏该插件对应行）。指标名称必须唯一；
/// 纯空白字符串、包含控制字符或超出字符长度上限的指标会被整体拒绝。
pub fn validate_status_metrics(metrics: &[PluginStatusMetric]) -> crate::Result<()> {
    anyhow::ensure!(
        metrics.len() <= MAX_PLUGIN_STATUS_METRICS,
        "status metrics exceed the maximum of {MAX_PLUGIN_STATUS_METRICS}"
    );
    let mut labels = std::collections::HashSet::new();
    for metric in metrics {
        validate_text(&metric.label, MAX_PLUGIN_STATUS_LABEL_CHARS, "label")?;
        validate_text(&metric.value, MAX_PLUGIN_STATUS_VALUE_CHARS, "value")?;
        anyhow::ensure!(
            labels.insert(metric.label.trim()),
            "status metric labels must be unique"
        );
    }
    Ok(())
}

fn validate_text(value: &str, limit: usize, field: &str) -> crate::Result<()> {
    // 先按最大 UTF-8 字节数（limit * 4）做快速边界检查，防止超长恶意字符串耗费过多遍历开销。
    anyhow::ensure!(
        value.len() <= limit * 4
            && !value.trim().is_empty()
            && value.chars().count() <= limit
            && !value.chars().any(char::is_control),
        "status metric {field} is blank, contains controls or exceeds {limit} characters"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_metrics_accept_unicode_and_formatted_values() {
        assert!(validate_status_metrics(&[]).is_ok());
        assert!(
            validate_status_metrics(&[
                PluginStatusMetric::new("绑定账号", "2.9万"),
                PluginStatusMetric::new("今日签到", "1,234"),
            ])
            .is_ok()
        );
        assert!(
            validate_status_metrics(&[PluginStatusMetric::new(
                "中".repeat(MAX_PLUGIN_STATUS_LABEL_CHARS),
                "🙂".repeat(MAX_PLUGIN_STATUS_VALUE_CHARS),
            )])
            .is_ok()
        );
    }

    #[test]
    fn public_metrics_reject_ambiguous_or_over_budget_contributions() {
        for metric in [
            PluginStatusMetric::new(" ", "1"),
            PluginStatusMetric::new("Count", "\n"),
            PluginStatusMetric::new("Count\t", "1"),
            PluginStatusMetric::new("Count", "1\0"),
            PluginStatusMetric::new("中".repeat(MAX_PLUGIN_STATUS_LABEL_CHARS + 1), "1"),
            PluginStatusMetric::new("Count", "x".repeat(MAX_PLUGIN_STATUS_VALUE_CHARS + 1)),
        ] {
            assert!(validate_status_metrics(&[metric]).is_err());
        }
        assert!(
            validate_status_metrics(&[
                PluginStatusMetric::new("Count", "1"),
                PluginStatusMetric::new(" Count ", "2"),
            ])
            .is_err()
        );
        assert!(
            validate_status_metrics(&vec![
                PluginStatusMetric::new("Count", "1");
                MAX_PLUGIN_STATUS_METRICS + 1
            ])
            .is_err()
        );
    }
}
