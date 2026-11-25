use crate::core::event_bus::{Event, EventBus, EventType};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tracing::info;

/// 配置变更类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConfigChangeType {
    /// 插件启用状态变更
    PluginEnabled,
    /// 服务启用状态变更
    ServiceEnabled,
    /// 插件配置变更
    PluginConfig,
    /// 全局配置变更
    GlobalConfig,
}

/// 配置变更事件数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigChangeEvent {
    /// 变更类型
    pub change_type: ConfigChangeType,
    /// 目标名称（插件名/服务名/配置键）
    pub target: String,
    /// 旧值
    pub old_value: Option<Value>,
    /// 新值
    pub new_value: Value,
    /// 变更时间
    pub timestamp: DateTime<Local>,
    /// 变更者（用户 ID）
    pub changed_by: Option<String>,
    /// 变更原因
    pub reason: Option<String>,
}

impl ConfigChangeEvent {
    pub fn new(
        change_type: ConfigChangeType,
        target: impl Into<String>,
        old_value: Option<Value>,
        new_value: Value,
    ) -> Self {
        Self {
            change_type,
            target: target.into(),
            old_value,
            new_value,
            timestamp: Local::now(),
            changed_by: None,
            reason: None,
        }
    }

    pub fn with_changed_by(mut self, user_id: impl Into<String>) -> Self {
        self.changed_by = Some(user_id.into());
        self
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

/// 审计事件类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AuditEventType {
    /// 登录
    Login,
    /// 登出
    Logout,
    /// 配置变更
    ConfigChange,
    /// 插件启用/禁用
    PluginToggle,
    /// 服务启用/禁用
    ServiceToggle,
    /// 安全规则变更
    SecurityRuleChange,
    /// 订阅变更
    SubscriptionChange,
    /// 手动发送消息
    ManualMessage,
    /// 插件安装/卸载
    PluginInstall,
    /// 数据库操作
    DatabaseOperation,
    /// 系统操作
    SystemOperation,
}

/// 审计事件数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// 事件类型
    pub event_type: AuditEventType,
    /// 操作描述
    pub description: String,
    /// 操作目标
    pub target: Option<String>,
    /// 操作详情
    pub details: Option<Value>,
    /// 操作时间
    pub timestamp: DateTime<Local>,
    /// 操作者（用户 ID）
    pub operator: Option<String>,
    /// 操作来源（IP 地址）
    pub source_ip: Option<String>,
    /// 操作结果
    pub result: AuditResult,
}

/// 审计结果
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AuditResult {
    /// 成功
    Success,
    /// 失败
    Failure,
    /// 部分成功
    PartialSuccess,
}

impl AuditEvent {
    pub fn new(event_type: AuditEventType, description: impl Into<String>) -> Self {
        Self {
            event_type,
            description: description.into(),
            target: None,
            details: None,
            timestamp: Local::now(),
            operator: None,
            source_ip: None,
            result: AuditResult::Success,
        }
    }

    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn with_operator(mut self, operator: impl Into<String>) -> Self {
        self.operator = Some(operator.into());
        self
    }

    pub fn with_source_ip(mut self, ip: impl Into<String>) -> Self {
        self.source_ip = Some(ip.into());
        self
    }

    pub fn with_result(mut self, result: AuditResult) -> Self {
        self.result = result;
        self
    }
}

/// 配置变更管理器
pub struct ConfigChangeManager {
    event_bus: Arc<EventBus>,
}

impl ConfigChangeManager {
    pub fn new(event_bus: Arc<EventBus>) -> Self {
        Self { event_bus }
    }

    /// 发布配置变更事件
    pub async fn emit_config_change(&self, event: ConfigChangeEvent) {
        let event_type = match event.change_type {
            ConfigChangeType::PluginEnabled => EventType::PluginEnabledChanged,
            ConfigChangeType::ServiceEnabled => EventType::ServiceEnabledChanged,
            ConfigChangeType::PluginConfig => EventType::PluginConfigChanged,
            ConfigChangeType::GlobalConfig => EventType::GlobalConfigChanged,
        };

        let data = serde_json::to_value(&event).unwrap_or_default();
        let bus_event = Event::new(event_type, 0, data);

        info!(
            change_type = ?event.change_type,
            target = %event.target,
            "配置变更事件已发布"
        );

        self.event_bus.publish(bus_event).await;
    }

    /// 发布审计事件
    pub async fn emit_audit_event(&self, event: AuditEvent) {
        let data = serde_json::to_value(&event).unwrap_or_default();
        let bus_event = Event::new(EventType::AuditEvent, 0, data);

        info!(
            event_type = ?event.event_type,
            description = %event.description,
            operator = ?event.operator,
            result = ?event.result,
            "审计事件已发布"
        );

        self.event_bus.publish(bus_event).await;
    }

    /// 记录插件启用状态变更
    pub async fn plugin_enabled_changed(
        &self,
        plugin_name: &str,
        old_value: Option<bool>,
        new_value: bool,
        changed_by: Option<&str>,
    ) {
        let event = ConfigChangeEvent::new(
            ConfigChangeType::PluginEnabled,
            plugin_name,
            old_value.map(serde_json::Value::Bool),
            serde_json::Value::Bool(new_value),
        )
        .with_changed_by(changed_by.unwrap_or("system"));

        self.emit_config_change(event).await;
    }

    /// 记录服务启用状态变更
    pub async fn service_enabled_changed(
        &self,
        plugin_name: &str,
        service_name: &str,
        old_value: Option<bool>,
        new_value: bool,
        changed_by: Option<&str>,
    ) {
        let target = format!("{}.{}", plugin_name, service_name);
        let event = ConfigChangeEvent::new(
            ConfigChangeType::ServiceEnabled,
            target,
            old_value.map(serde_json::Value::Bool),
            serde_json::Value::Bool(new_value),
        )
        .with_changed_by(changed_by.unwrap_or("system"));

        self.emit_config_change(event).await;
    }

    /// 记录插件配置变更
    pub async fn plugin_config_changed(
        &self,
        plugin_name: &str,
        config_key: &str,
        old_value: Option<Value>,
        new_value: Value,
        changed_by: Option<&str>,
    ) {
        let target = format!("{}.{}", plugin_name, config_key);
        let event =
            ConfigChangeEvent::new(ConfigChangeType::PluginConfig, target, old_value, new_value)
                .with_changed_by(changed_by.unwrap_or("system"));

        self.emit_config_change(event).await;
    }

    /// 记录全局配置变更
    pub async fn global_config_changed(
        &self,
        config_key: &str,
        old_value: Option<Value>,
        new_value: Value,
        changed_by: Option<&str>,
    ) {
        let event = ConfigChangeEvent::new(
            ConfigChangeType::GlobalConfig,
            config_key,
            old_value,
            new_value,
        )
        .with_changed_by(changed_by.unwrap_or("system"));

        self.emit_config_change(event).await;
    }

    /// 记录登录事件
    pub async fn login_event(&self, user_id: &str, ip: &str, success: bool) {
        let event = AuditEvent::new(
            AuditEventType::Login,
            if success {
                "用户登录成功"
            } else {
                "用户登录失败"
            },
        )
        .with_operator(user_id)
        .with_source_ip(ip)
        .with_result(if success {
            AuditResult::Success
        } else {
            AuditResult::Failure
        });

        self.emit_audit_event(event).await;
    }

    /// 记录插件操作事件
    pub async fn plugin_operation(
        &self,
        plugin_name: &str,
        operation: &str,
        operator: &str,
        success: bool,
    ) {
        let event = AuditEvent::new(
            AuditEventType::PluginToggle,
            format!("插件{}: {}", operation, plugin_name),
        )
        .with_target(plugin_name)
        .with_operator(operator)
        .with_result(if success {
            AuditResult::Success
        } else {
            AuditResult::Failure
        });

        self.emit_audit_event(event).await;
    }

    /// 记录手动消息发送
    pub async fn manual_message_sent(&self, operator: &str, target: &str, message_preview: &str) {
        let event = AuditEvent::new(
            AuditEventType::ManualMessage,
            format!("手动发送消息到 {}", target),
        )
        .with_target(target)
        .with_operator(operator)
        .with_details(serde_json::json!({
            "message_preview": message_preview
        }));

        self.emit_audit_event(event).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_config_change_event_creation() {
        let event = ConfigChangeEvent::new(
            ConfigChangeType::PluginEnabled,
            "test_plugin",
            Some(json!(false)),
            json!(true),
        )
        .with_changed_by("admin")
        .with_reason("测试启用");

        assert_eq!(event.change_type, ConfigChangeType::PluginEnabled);
        assert_eq!(event.target, "test_plugin");
        assert_eq!(event.old_value, Some(json!(false)));
        assert_eq!(event.new_value, json!(true));
        assert_eq!(event.changed_by, Some("admin".to_string()));
        assert_eq!(event.reason, Some("测试启用".to_string()));
    }

    #[test]
    fn test_audit_event_creation() {
        let event = AuditEvent::new(AuditEventType::Login, "用户登录成功")
            .with_operator("user123")
            .with_source_ip("192.168.1.1")
            .with_result(AuditResult::Success);

        assert_eq!(event.event_type, AuditEventType::Login);
        assert_eq!(event.description, "用户登录成功");
        assert_eq!(event.operator, Some("user123".to_string()));
        assert_eq!(event.source_ip, Some("192.168.1.1".to_string()));
        assert_eq!(event.result, AuditResult::Success);
    }
}
