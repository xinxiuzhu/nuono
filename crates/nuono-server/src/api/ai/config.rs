//! AI 运行配置读取与补丁更新接口。

use axum::extract::State;
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::api::ApiResponse;

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::ai::AiConfigResponse)]
pub struct AiConfigResponse {
    /// AI 回复呈现策略；包含 default 与按完整机器人/会话身份匹配的 overrides。
    pub output: serde_json::Value,
    pub provider: String,
    pub model: String,
    /// 已打码的密钥；完整值不会回显。
    pub api_key: String,
    pub endpoint: Option<String>,
    pub temperature: f32,
    pub max_tokens: u32,
    pub system_prompt: Option<String>,
    /// 全站 AI 总开关。
    pub enabled: bool,
    /// 机器人自动回复子开关。
    pub reply_enabled: bool,
    pub provider_router_enabled: bool,
    pub provider_fallback_chain: Vec<String>,
    pub provider_timeout_secs: u64,
    pub provider_health_fuse_enabled: bool,
    pub provider_health_window_events: u64,
    pub provider_health_min_attempts: u64,
    pub provider_health_max_failure_rate: f32,
    pub model_alias: String,
    pub chat_model_alias: String,
    pub tool_model_alias: String,
    pub agent_memory_enabled: bool,
    pub agent_memory_policy: serde_json::Value,
    pub agent_memory_auto_extract_enabled: bool,
    pub agent_autonomy: String,
    pub agent_allowed_platforms: Vec<String>,
    pub agent_denied_platforms: Vec<String>,
    pub agent_allowed_tools: Vec<String>,
    pub agent_denied_tools: Vec<String>,
    pub agent_allowed_plugins: Vec<String>,
    pub agent_denied_plugins: Vec<String>,
}

pub(super) fn mask_api_key(key: &str) -> String {
    if key.chars().count() <= 8 {
        "****".to_string()
    } else {
        let head: String = key.chars().take(4).collect();
        let tail: String = key
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("{head}****{tail}")
    }
}

#[utoipa::path(
    get,
    path = "/config",
    operation_id = "ai_get_ai_config",
    tag = "ai",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<AiConfigResponse>))
)]
pub(super) async fn get_ai_config(
    State(state): State<AppState>,
) -> crate::api::ApiReply<AiConfigResponse> {
    let cfg = state.config.get().await;
    let ai = cfg.ai;
    ApiResponse::ok(AiConfigResponse {
        output: serde_json::to_value(ai.output).expect("output settings serialize"),
        provider: ai.provider,
        model: ai.model,
        api_key: ai.api_key.as_deref().map(mask_api_key).unwrap_or_default(),
        endpoint: ai.endpoint,
        temperature: ai.temperature,
        max_tokens: if ai.max_tokens == 0 {
            2048
        } else {
            ai.max_tokens
        },
        system_prompt: ai.system_prompt,
        enabled: ai.enabled,
        reply_enabled: ai.reply_enabled,
        provider_router_enabled: ai.provider_router_enabled,
        provider_fallback_chain: ai.provider_fallback_chain,
        provider_timeout_secs: ai.provider_timeout_secs,
        provider_health_fuse_enabled: ai.provider_health_fuse_enabled,
        provider_health_window_events: ai.provider_health_window_events,
        provider_health_min_attempts: ai.provider_health_min_attempts,
        provider_health_max_failure_rate: ai.provider_health_max_failure_rate,
        model_alias: ai.model_alias,
        chat_model_alias: ai.chat_model_alias,
        tool_model_alias: ai.tool_model_alias,
        agent_memory_enabled: ai.agent_memory_enabled,
        agent_memory_policy: serde_json::to_value(ai.agent_memory_policy)
            .expect("policy serializes"),
        agent_memory_auto_extract_enabled: ai.agent_memory_auto_extract_enabled,
        agent_autonomy: if ai.agent_autonomy.is_empty() {
            "reply_only".to_string()
        } else {
            ai.agent_autonomy.clone()
        },
        agent_allowed_platforms: ai.agent_allowed_platforms,
        agent_denied_platforms: ai.agent_denied_platforms,
        agent_allowed_tools: ai.agent_allowed_tools,
        agent_denied_tools: ai.agent_denied_tools,
        agent_allowed_plugins: ai.agent_allowed_plugins,
        agent_denied_plugins: ai.agent_denied_plugins,
    })
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::ai::UpdateAiConfigPayload)]
pub struct UpdateAiConfigPayload {
    /// 显式更新输出策略；mode 为 complete/native/segmented，省略字段保留存储值。
    pub output: Option<serde_json::Value>,
    pub provider: Option<String>,
    pub model: Option<String>,
    #[schema(write_only, format = Password)]
    pub api_key: Option<String>,
    pub endpoint: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub system_prompt: Option<String>,
    pub enabled: Option<bool>,
    pub reply_enabled: Option<bool>,
    pub provider_router_enabled: Option<bool>,
    pub provider_fallback_chain: Option<Vec<String>>,
    pub provider_timeout_secs: Option<u64>,
    pub provider_health_fuse_enabled: Option<bool>,
    pub provider_health_window_events: Option<u64>,
    pub provider_health_min_attempts: Option<u64>,
    pub provider_health_max_failure_rate: Option<f32>,
    pub model_alias: Option<String>,
    pub chat_model_alias: Option<String>,
    pub tool_model_alias: Option<String>,
    pub agent_memory_enabled: Option<bool>,
    pub agent_memory_policy: Option<serde_json::Value>,
    pub agent_memory_auto_extract_enabled: Option<bool>,
    pub agent_autonomy: Option<String>,
    pub agent_allowed_platforms: Option<Vec<String>>,
    pub agent_denied_platforms: Option<Vec<String>>,
    pub agent_allowed_tools: Option<Vec<String>>,
    pub agent_denied_tools: Option<Vec<String>>,
    pub agent_allowed_plugins: Option<Vec<String>>,
    pub agent_denied_plugins: Option<Vec<String>>,
}

/// 把 AI 设置请求转成显式补丁：请求给出的字段才落库。
///
/// `ai.api_key` 是 secret，走 `runtime_secrets`；GET 回显打码值（含 `****`），原样回传
/// 或留空表示保持不变，该接口不提供显式清除（清除请用 `/config/secrets`）。其余字段
/// 沿用原有规范化语义：temperature/max_tokens/provider 超时与熔断参数夹取、模型别名
/// trim、自治等级白名单、策略名单去空白去重。
pub(super) fn build_ai_config_patch(
    body: UpdateAiConfigPayload,
) -> nuono_core::db::RuntimeConfigPatch {
    let UpdateAiConfigPayload {
        output,
        provider,
        model,
        api_key,
        endpoint,
        temperature,
        max_tokens,
        system_prompt,
        enabled,
        reply_enabled,
        provider_router_enabled,
        provider_fallback_chain,
        provider_timeout_secs,
        provider_health_fuse_enabled,
        provider_health_window_events,
        provider_health_min_attempts,
        provider_health_max_failure_rate,
        model_alias,
        chat_model_alias,
        tool_model_alias,
        agent_memory_enabled,
        agent_memory_policy,
        agent_memory_auto_extract_enabled,
        agent_autonomy,
        agent_allowed_platforms,
        agent_denied_platforms,
        agent_allowed_tools,
        agent_denied_tools,
        agent_allowed_plugins,
        agent_denied_plugins,
    } = body;

    let mut secrets: Vec<(String, Option<String>)> = Vec::new();
    if let Some(v) = api_key
        && !v.contains("****")
        && !v.is_empty()
    {
        secrets.push(("ai.api_key".into(), Some(v)));
    }

    let mut ai = serde_json::Map::new();
    let mut insert = |key: &str, value: serde_json::Value| {
        ai.insert(key.to_string(), value);
    };
    if let Some(v) = provider {
        insert("provider", serde_json::json!(v));
    }
    if let Some(v) = model {
        insert("model", serde_json::json!(v));
    }
    if let Some(v) = endpoint {
        insert("endpoint", serde_json::json!(v));
    }
    if let Some(v) = temperature {
        insert("temperature", serde_json::json!(v.clamp(0.0, 2.0)));
    }
    if let Some(v) = max_tokens {
        insert("max_tokens", serde_json::json!(v.max(1)));
    }
    if let Some(v) = system_prompt {
        insert("system_prompt", serde_json::json!(v));
    }
    if let Some(v) = enabled {
        insert("enabled", serde_json::json!(v));
    }
    if let Some(v) = reply_enabled {
        insert("reply_enabled", serde_json::json!(v));
    }
    if let Some(v) = provider_router_enabled {
        insert("provider_router_enabled", serde_json::json!(v));
    }
    if let Some(v) = provider_fallback_chain {
        insert(
            "provider_fallback_chain",
            serde_json::json!(normalize_policy_list(v)),
        );
    }
    if let Some(v) = provider_timeout_secs {
        insert("provider_timeout_secs", serde_json::json!(v.clamp(1, 300)));
    }
    if let Some(v) = provider_health_fuse_enabled {
        insert("provider_health_fuse_enabled", serde_json::json!(v));
    }
    if let Some(v) = provider_health_window_events {
        insert(
            "provider_health_window_events",
            serde_json::json!(v.clamp(50, 1000)),
        );
    }
    if let Some(v) = provider_health_min_attempts {
        insert(
            "provider_health_min_attempts",
            serde_json::json!(v.clamp(1, 100)),
        );
    }
    if let Some(v) = provider_health_max_failure_rate {
        insert(
            "provider_health_max_failure_rate",
            serde_json::json!(v.clamp(0.0, 1.0)),
        );
    }
    if let Some(v) = model_alias {
        insert("model_alias", serde_json::json!(v.trim()));
    }
    if let Some(v) = chat_model_alias {
        insert("chat_model_alias", serde_json::json!(v.trim()));
    }
    if let Some(v) = tool_model_alias {
        insert("tool_model_alias", serde_json::json!(v.trim()));
    }

    if let Some(v) = agent_memory_enabled {
        insert("agent_memory_enabled", serde_json::json!(v));
    }
    if let Some(v) = output {
        insert("output", v);
    }
    if let Some(v) = agent_memory_policy {
        insert("agent_memory_policy", v);
    }
    if let Some(v) = agent_memory_auto_extract_enabled {
        insert("agent_memory_auto_extract_enabled", serde_json::json!(v));
    }
    if let Some(v) = agent_autonomy {
        insert(
            "agent_autonomy",
            serde_json::json!(match v.as_str() {
                "low_risk_tools" | "approved_tools" | "admin_approved" => v,
                _ => "reply_only".to_string(),
            }),
        );
    }
    if let Some(v) = agent_allowed_platforms {
        insert(
            "agent_allowed_platforms",
            serde_json::json!(normalize_policy_list(v)),
        );
    }
    if let Some(v) = agent_denied_platforms {
        insert(
            "agent_denied_platforms",
            serde_json::json!(normalize_policy_list(v)),
        );
    }
    if let Some(v) = agent_allowed_tools {
        insert(
            "agent_allowed_tools",
            serde_json::json!(normalize_policy_list(v)),
        );
    }
    if let Some(v) = agent_denied_tools {
        insert(
            "agent_denied_tools",
            serde_json::json!(normalize_policy_list(v)),
        );
    }
    if let Some(v) = agent_allowed_plugins {
        insert(
            "agent_allowed_plugins",
            serde_json::json!(normalize_policy_list(v)),
        );
    }
    if let Some(v) = agent_denied_plugins {
        insert(
            "agent_denied_plugins",
            serde_json::json!(normalize_policy_list(v)),
        );
    }

    nuono_core::db::RuntimeConfigPatch {
        settings: serde_json::json!({ "ai": serde_json::Value::Object(ai) }),
        secrets,
    }
}

#[utoipa::path(
    post,
    path = "/config",
    operation_id = "ai_update_ai_config",
    tag = "ai",
    request_body = UpdateAiConfigPayload,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::Empty>))
)]
pub(super) async fn update_ai_config(
    State(state): State<AppState>,
    crate::api::input::ApiJson(body): crate::api::input::ApiJson<UpdateAiConfigPayload>,
) -> crate::api::ApiReply<()> {
    match state.config.update_patch(build_ai_config_patch(body)).await {
        Ok(()) => ApiResponse::ok(()),
        Err(error) => ApiResponse::from_err(&error),
    }
}

pub(super) fn normalize_policy_list(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}
