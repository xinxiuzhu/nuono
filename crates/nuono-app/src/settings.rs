//! 部署配置与运行时设置的数据结构定义，具体的持久化由数据库存储层和配置文件适配器实现。
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use validator::Validate;

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct DatabaseConfig {
    pub url: String,
    pub max_connections: u32,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: "sqlite://data/nuono.db?mode=rwc".into(),
            max_connections: 5,
        }
    }
}

use serde_json::Value;
/// 运行期配置的显式补丁。
///
/// `settings` 是普通设置的局部树（对存储值做深合并，只覆盖显式给出的字段）；
/// `secrets` 是 secret 键的显式目标值，`None` 表示删除该键。两个部分都只应包含
/// 请求明确给出的字段——这是显式补丁与「整棵快照 diff」的区别所在：显式提交的
/// 密码即使与环境变量相同也会落库，显式清除也总能删掉数据库里的旧值。
#[derive(Debug, Clone)]
pub struct RuntimeConfigPatch {
    pub settings: Value,
    pub secrets: Vec<(String, Option<String>)>,
}

/// 数据库中已保存的设置与受管密钥快照；环境变量等部署级覆盖项由调用方另行合并。
#[derive(Debug, Clone, Default)]
pub struct RuntimeConfigSnapshot {
    pub settings: Value,
    pub secrets: std::collections::HashMap<String, String>,
}
/// 将显式提交的普通设置补丁深合并到基准配置树中（密钥更新走独立语义，不在此合并）。
pub fn merge_runtime_values(base: &mut Value, patch: &Value) {
    if let (Some(base), Some(patch)) = (base.as_object_mut(), patch.as_object()) {
        for (key, value) in patch {
            if let Some(existing) = base.get_mut(key)
                && existing.is_object()
                && value.is_object()
            {
                merge_runtime_values(existing, value);
                continue;
            }
            base.insert(key.clone(), value.clone());
        }
    }
}
/// 拒绝在运行期热更新只能在启动时确定的部署配置项。
pub fn reject_startup_patch(settings: &Value) -> Result<(), nuono_error::NuonoError> {
    let Some(object) = settings.as_object() else {
        return Err(nuono_error::NuonoError::ConfigValidation(
            "运行设置补丁必须是对象".into(),
        ));
    };
    if object
        .get("log")
        .and_then(Value::as_object)
        .is_some_and(|log| log.contains_key("output"))
        || ["host", "port", "database", "build", "backup"]
            .iter()
            .any(|key| object.contains_key(*key))
        || object
            .get("security")
            .and_then(Value::as_object)
            .is_some_and(|s| s.contains_key("admin_auth_enabled"))
    {
        return Err(nuono_error::NuonoError::ConfigValidation(
            "启动配置请修改 nuono.toml 后重启".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct AIConfig {
    #[serde(default)]
    pub assistant: crate::assistant::AssistantSettings,
    #[serde(default)]
    pub output: crate::agent::output::OutputSettings,
    #[serde(default)]
    pub web: crate::web_settings::WebSettings,
    /// 全站 AI 总开关；关闭时网页对话、机器人自动回复与后台 AI 任务一并停用。
    pub enabled: bool,
    /// 机器人自动回复子开关（群聊 @机器人 / 私聊触发）；仅在 AI 总开关开启时生效。
    #[serde(default)]
    pub reply_enabled: bool,
    pub provider: String,
    pub model: String,
    pub api_key: Option<String>,
    pub endpoint: Option<String>,
    pub temperature: f32,
    pub max_tokens: u32,
    pub system_prompt: Option<String>,
    #[serde(default = "default_provider_router_enabled")]
    pub provider_router_enabled: bool,
    #[serde(default)]
    pub provider_fallback_chain: Vec<String>,
    #[serde(default = "default_provider_timeout_secs")]
    pub provider_timeout_secs: u64,
    #[serde(default = "default_provider_health_fuse_enabled")]
    pub provider_health_fuse_enabled: bool,
    #[serde(default = "default_provider_health_window_events")]
    pub provider_health_window_events: u64,
    #[serde(default = "default_provider_health_min_attempts")]
    pub provider_health_min_attempts: u64,
    #[serde(default = "default_provider_health_max_failure_rate")]
    pub provider_health_max_failure_rate: f32,
    #[serde(default)]
    pub model_alias: String,
    #[serde(default)]
    pub chat_model_alias: String,
    #[serde(default)]
    pub tool_model_alias: String,
    /// 是否开启持久化长期记忆（与常规对话上下文历史相互独立）。
    #[serde(default = "default_agent_memory_auto_extract_enabled")]
    pub agent_memory_enabled: bool,
    #[serde(default)]
    pub agent_memory_policy: crate::agent::memory::MemoryPolicy,
    #[serde(default = "default_agent_memory_auto_extract_enabled")]
    pub agent_memory_auto_extract_enabled: bool,
    #[serde(default = "default_agent_autonomy")]
    pub agent_autonomy: String,
    #[serde(default)]
    pub agent_allowed_platforms: Vec<String>,
    #[serde(default)]
    pub agent_denied_platforms: Vec<String>,
    /// 被管理员暂停使用 AI 的站点账号 ID 列表（同时阻止网页对话及其已绑定聊天平台身份的 AI 回复）。
    #[serde(default)]
    pub blocked_accounts: Vec<i64>,
    /// 被管理员暂停 AI 自动回复的内部群对象 ID 列表（对应 `nuono_groups.id`）。
    #[serde(default)]
    pub blocked_groups: Vec<i64>,
    #[serde(default)]
    pub agent_allowed_tools: Vec<String>,
    #[serde(default)]
    pub agent_denied_tools: Vec<String>,
    #[serde(default)]
    pub agent_allowed_plugins: Vec<String>,
    #[serde(default)]
    pub agent_denied_plugins: Vec<String>,
}

fn default_agent_autonomy() -> String {
    "reply_only".to_string()
}

fn default_provider_router_enabled() -> bool {
    true
}

fn default_provider_timeout_secs() -> u64 {
    30
}

fn default_provider_health_fuse_enabled() -> bool {
    true
}

fn default_provider_health_window_events() -> u64 {
    200
}

fn default_provider_health_min_attempts() -> u64 {
    3
}

fn default_provider_health_max_failure_rate() -> f32 {
    0.8
}

fn default_agent_memory_auto_extract_enabled() -> bool {
    true
}

impl Default for AIConfig {
    fn default() -> Self {
        Self {
            assistant: Default::default(),
            output: Default::default(),
            web: Default::default(),
            enabled: false,
            reply_enabled: false,
            provider: String::new(),
            model: String::new(),
            api_key: None,
            endpoint: None,
            temperature: 0.7,
            max_tokens: 2048,
            system_prompt: None,
            provider_router_enabled: default_provider_router_enabled(),
            provider_fallback_chain: Vec::new(),
            provider_timeout_secs: default_provider_timeout_secs(),
            provider_health_fuse_enabled: default_provider_health_fuse_enabled(),
            provider_health_window_events: default_provider_health_window_events(),
            provider_health_min_attempts: default_provider_health_min_attempts(),
            provider_health_max_failure_rate: default_provider_health_max_failure_rate(),
            model_alias: String::new(),
            chat_model_alias: String::new(),
            tool_model_alias: String::new(),
            agent_memory_enabled: true,
            agent_memory_policy: Default::default(),
            agent_memory_auto_extract_enabled: default_agent_memory_auto_extract_enabled(),
            agent_autonomy: default_agent_autonomy(),
            agent_allowed_platforms: Vec::new(),
            agent_denied_platforms: Vec::new(),
            blocked_accounts: Vec::new(),
            blocked_groups: Vec::new(),
            agent_allowed_tools: Vec::new(),
            agent_denied_tools: Vec::new(),
            agent_allowed_plugins: Vec::new(),
            agent_denied_plugins: Vec::new(),
        }
    }
}

impl AIConfig {
    /// 判断指定站点账号是否被管理员暂停使用 AI。
    pub fn is_account_blocked(&self, account_id: i64) -> bool {
        self.blocked_accounts.contains(&account_id)
    }

    /// 判断指定内部群对象是否被管理员暂停 AI 自动回复。
    pub fn is_group_blocked(&self, group_id: i64) -> bool {
        self.blocked_groups.contains(&group_id)
    }

    /// 机器人自动回复是否实际处于运行状态（需同时开启 AI 总开关与自动回复子开关）。
    pub fn reply_active(&self) -> bool {
        self.enabled && self.reply_enabled
    }

    /// 网页对话与跨端个人对话是否实际处于运行状态（需同时开启 AI 总开关与网页对话子开关）。
    pub fn assistant_active(&self) -> bool {
        self.enabled && self.assistant.enabled
    }

    /// 是否至少开启了一个需要真实调用模型的对外 AI 场景。
    pub fn any_live_channel_active(&self) -> bool {
        self.reply_active() || self.assistant_active()
    }

    pub fn agent_autonomy_level(&self) -> crate::agent::AutonomyLevel {
        match self.agent_autonomy.as_str() {
            "low_risk_tools" | "low" => crate::agent::AutonomyLevel::LowRiskTools,
            "approved_tools" | "approval" => crate::agent::AutonomyLevel::ApprovedTools,
            "admin_approved" | "admin" => crate::agent::AutonomyLevel::AdminApproved,
            _ => crate::agent::AutonomyLevel::ReplyOnly,
        }
    }

    pub fn to_ai_config(&self) -> Option<crate::ai::AiConfig> {
        self.enabled.then(|| self.model_config())
    }

    pub(crate) fn model_config(&self) -> crate::ai::AiConfig {
        crate::ai::AiConfig {
            provider: self.provider.clone(),
            provider_type: "openai_compatible".into(),
            api_key: self.api_key.clone().unwrap_or_default(),
            base_url: self.endpoint.clone().unwrap_or_default(),
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            context_length: 128000,
            system_prompt: self.system_prompt.clone(),
            extra: HashMap::new(),
        }
    }
}

/// 运行时配置校验器，在存储层的写入事务内执行最终有效配置校验。
#[async_trait::async_trait]
pub trait RuntimeSettingsValidator: Send + Sync {
    async fn validate(&self, candidate: RuntimeConfigSnapshot) -> nuono_error::Result<Value>;
    async fn validate_with_models(
        &self,
        candidate: RuntimeConfigSnapshot,
        models: &[crate::model_catalog::AiModelRecord],
    ) -> nuono_error::Result<Value> {
        crate::model_catalog::validate_references(&candidate.settings, models, false)?;
        let effective = self.validate(candidate).await?;
        crate::model_catalog::validate_references(&effective, models, true)?;
        Ok(effective)
    }
}
/// 运行时设置更新的前置条件检查，由存储层在锁定当前配置树后、合并补丁前执行。
pub trait RuntimeSettingsPrecondition: Send + Sync {
    fn check(&self, current: &Value) -> nuono_error::Result<()>;
    fn check_snapshot(&self, current: &RuntimeConfigSnapshot) -> nuono_error::Result<()> {
        self.check(&current.settings)
    }
}
#[async_trait::async_trait]
pub trait RuntimeSettingsStore: Send + Sync {
    async fn initialize(&self, seed: RuntimeConfigSnapshot) -> nuono_error::Result<()>;
    async fn load(&self) -> nuono_error::Result<RuntimeConfigSnapshot>;
    async fn update(
        &self,
        patch: &RuntimeConfigPatch,
        validator: &dyn RuntimeSettingsValidator,
    ) -> nuono_error::Result<Value>;
    /// 将模型目录变更、设置补丁合并与有效配置校验放在同一个数据库事务中原子执行。
    async fn change_model(
        &self,
        _change: crate::model_catalog::ModelChange<'_>,
        _patch: &RuntimeConfigPatch,
        _validator: &dyn RuntimeSettingsValidator,
    ) -> nuono_error::Result<(Value, Option<crate::model_catalog::AiModelRecord>)> {
        Err(nuono_error::NuonoError::Internal(
            "运行设置存储不支持模型原子修改".into(),
        ))
    }
    /// 带前置条件的更新：必须在合并与提交所在的同一个事务内完成条件检查。
    /// 不支持事务级原子检查的自定义存储实现应直接报错拒绝，而不要在事务外做有竞态风险的模拟检查。
    async fn update_conditional(
        &self,
        _patch: &RuntimeConfigPatch,
        _validator: &dyn RuntimeSettingsValidator,
        _precondition: &dyn RuntimeSettingsPrecondition,
    ) -> nuono_error::Result<Value> {
        Err(nuono_error::NuonoError::Internal(
            "运行设置存储不支持原子条件更新".into(),
        ))
    }
}
/// 运行时配置服务入口：由 `store` 负责事务与持久化，`validator` 负责结合部署级覆盖项校验最终生效的配置。
pub struct RuntimeSettingsService {
    store: std::sync::Arc<dyn RuntimeSettingsStore>,
    validator: std::sync::Arc<dyn RuntimeSettingsValidator>,
}
impl RuntimeSettingsService {
    pub fn new(
        store: std::sync::Arc<dyn RuntimeSettingsStore>,
        validator: std::sync::Arc<dyn RuntimeSettingsValidator>,
    ) -> Self {
        Self { store, validator }
    }
    pub async fn initialize(&self, seed: RuntimeConfigSnapshot) -> nuono_error::Result<Value> {
        self.store.initialize(seed).await?;
        self.validator.validate(self.store.load().await?).await
    }
    pub async fn update(&self, patch: &RuntimeConfigPatch) -> nuono_error::Result<Value> {
        reject_startup_patch(&patch.settings)?;
        self.store.update(patch, self.validator.as_ref()).await
    }
    pub async fn change_model(
        &self,
        change: crate::model_catalog::ModelChange<'_>,
        patch: &RuntimeConfigPatch,
    ) -> nuono_error::Result<(Value, Option<crate::model_catalog::AiModelRecord>)> {
        reject_startup_patch(&patch.settings)?;
        self.store
            .change_model(change, patch, self.validator.as_ref())
            .await
    }
    pub async fn update_conditional(
        &self,
        patch: &RuntimeConfigPatch,
        precondition: &dyn RuntimeSettingsPrecondition,
    ) -> nuono_error::Result<Value> {
        reject_startup_patch(&patch.settings)?;
        self.store
            .update_conditional(patch, self.validator.as_ref(), precondition)
            .await
    }
}

impl Default for RuntimeConfigPatch {
    fn default() -> Self {
        Self {
            settings: serde_json::json!({}),
            secrets: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AIConfig, reject_startup_patch};

    #[test]
    fn persistent_memory_defaults_on_but_respects_explicit_disable() {
        let default = AIConfig::default();
        assert!(default.agent_memory_enabled);
        let mut old_settings = serde_json::to_value(default).unwrap();
        old_settings
            .as_object_mut()
            .unwrap()
            .remove("agent_memory_enabled");
        let restored: AIConfig = serde_json::from_value(old_settings.clone()).unwrap();
        assert!(restored.agent_memory_enabled);
        old_settings["agent_memory_enabled"] = serde_json::json!(false);
        let opted_out: AIConfig = serde_json::from_value(old_settings).unwrap();
        assert!(!opted_out.agent_memory_enabled);
    }

    #[test]
    fn build_section_is_rejected_as_a_runtime_patch() {
        for build in [
            serde_json::json!({"incremental": true}),
            serde_json::json!({}),
            serde_json::Value::Null,
        ] {
            assert!(reject_startup_patch(&serde_json::json!({"build": build})).is_err());
        }
        assert!(reject_startup_patch(&serde_json::json!({"log": {"level": "DEBUG"}})).is_ok());
    }
}
