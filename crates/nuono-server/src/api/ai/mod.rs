//! AI 管理台与运行诊断 HTTP 路由，按业务领域拆分为独立子模块。

mod chat;
mod config;
mod memories;
mod runs;
mod tools;

#[cfg(test)]
use crate::api::input::{ApiJson as Json, ApiPath as Path, ApiQuery as Query};
#[cfg(test)]
use axum::extract::State;
#[cfg(test)]
use axum::response::IntoResponse;
#[cfg(test)]
use chat::present_conversation_content;
#[cfg(test)]
use config::{UpdateAiConfigPayload, build_ai_config_patch, mask_api_key};
#[cfg(test)]
use memories::{
    CreateMemoryPayload, MemoryGraphQuery, MemoryQuery, create_memory, delete_memory, get_memory,
    get_memory_graph, list_memories,
};
#[cfg(test)]
use nuono_core::db::AgentMemoryRecord;
#[cfg(test)]
use runs::{AgentRunResponse, AgentRunSummaryResponse, authorize_run_operator};

use utoipa_axum::{router::OpenApiRouter, routes};

use crate::AppState;

#[cfg(test)]
#[path = "../../../tests/unit/api/ai/output.rs"]
mod output_tests;
#[cfg(test)]
#[path = "../../../tests/unit/api/ai/privacy.rs"]
mod privacy_tests;

pub(super) fn private_scope(key: &str) -> bool {
    key.starts_with(nuono_app::agent::ACCOUNT_CHAT_PREFIX)
}

pub(super) async fn public_memory(
    state: &AppState,
    id: i64,
) -> nuono_core::error::Result<Option<nuono_core::entity::agent_memories::Model>> {
    Ok(state
        .db
        .get_agent_memory(id)
        .await?
        .filter(|row| !private_scope(&row.scope_key)))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(config::get_ai_config))
        .routes(routes!(config::update_ai_config))
        .routes(routes!(chat::chat))
        .routes(routes!(chat::chat_stream))
        .routes(routes!(chat::list_conversations))
        .routes(routes!(chat::delete_conversation))
        .routes(routes!(tools::list_tool_catalog))
        .routes(routes!(tools::list_tool_receipts))
        .routes(routes!(tools::list_tool_approvals))
        .routes(routes!(tools::get_tool_approval))
        .routes(routes!(tools::decide_tool_approval))
        .routes(routes!(runs::list_agent_runs))
        .routes(routes!(runs::get_agent_run))
        .routes(routes!(runs::steer_agent_run))
        .routes(routes!(runs::cancel_agent_run))
        .routes(routes!(runs::collect_agent_children))
        .routes(routes!(runs::resume_agent_approval))
        .routes(routes!(memories::list_memories))
        .routes(routes!(memories::get_memory_graph))
        .routes(routes!(memories::maintain_memories))
        .routes(routes!(memories::pin_memory))
        .routes(routes!(memories::create_memory))
        .routes(routes!(memories::purge_expired_memories))
        .routes(routes!(
            memories::get_memory,
            memories::update_memory,
            memories::delete_memory
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_mask_handles_unicode_without_exposing_short_keys() {
        for key in ["", "sk-short", "密钥", "🔑🔑🔑🔑🔑🔑🔑🔑"] {
            assert_eq!(mask_api_key(key), "****");
        }
        assert_eq!(mask_api_key("sk-1234567890"), "sk-1****7890");
        assert_eq!(mask_api_key("密钥🔑abcdef秘密尾"), "密钥🔑a****f秘密尾");
    }

    fn payload(json: serde_json::Value) -> UpdateAiConfigPayload {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn task_api_requires_runtime_capability_even_for_authenticated_readers() {
        use nuono_core::auth::{CallerIdentity, Role};
        let reader = CallerIdentity::new(3, "reader", Role::User);
        let admin = CallerIdentity::new(2, "admin", Role::Admin);
        let owner = CallerIdentity::new(1, "owner", Role::Owner);
        for enabled in [false, true] {
            assert!(authorize_run_operator(enabled, Some(&reader)).is_err());
            assert!(authorize_run_operator(enabled, Some(&admin)).is_ok());
            assert!(authorize_run_operator(enabled, Some(&owner)).is_ok());
        }
        assert!(authorize_run_operator(true, None).is_err());
        assert!(authorize_run_operator(false, None).is_ok());
    }

    #[test]
    fn api_key_patch_only_carries_explicit_unmasked_values() {
        // 显式提交明文 key：进 secrets 补丁（即使与环境变量同值也会落库）。
        let patch = build_ai_config_patch(payload(serde_json::json!({ "api_key": "sk-live" })));
        assert_eq!(
            patch.secrets,
            vec![("ai.api_key".to_string(), Some("sk-live".to_string()))]
        );

        // GET 回显打码值原样回传 = 未修改；留空 = 保持不变。两者都不进补丁。
        let masked =
            build_ai_config_patch(payload(serde_json::json!({ "api_key": "sk-ab****cd" })));
        assert!(masked.secrets.is_empty());
        let empty = build_ai_config_patch(payload(serde_json::json!({ "api_key": "" })));
        assert!(empty.secrets.is_empty());

        // api_key 永不进入普通设置树。
        assert!(!patch.settings.to_string().contains("sk-live"));
    }

    #[test]
    fn ai_patch_preserves_field_normalization_semantics() {
        let patch = build_ai_config_patch(payload(serde_json::json!({
            "temperature": 5.0,
            "max_tokens": 0,
            "provider_timeout_secs": 0,
            "provider_health_window_events": 1,
            "provider_health_min_attempts": 500,
            "provider_health_max_failure_rate": 2.0,
            "agent_autonomy": "yolo",
            "agent_allowed_tools": [" A ", "", "B"],
            "model_alias": "  main  "
        })));
        let ai = &patch.settings["ai"];
        assert_eq!(ai["temperature"], serde_json::json!(2.0));
        assert_eq!(ai["max_tokens"], serde_json::json!(1));
        assert_eq!(ai["provider_timeout_secs"], serde_json::json!(1));
        assert_eq!(ai["provider_health_window_events"], serde_json::json!(50));
        assert_eq!(ai["provider_health_min_attempts"], serde_json::json!(100));
        assert_eq!(
            ai["provider_health_max_failure_rate"],
            serde_json::json!(1.0)
        );
        assert_eq!(ai["agent_autonomy"], serde_json::json!("reply_only"));
        assert_eq!(ai["agent_allowed_tools"], serde_json::json!(["A", "B"]));
        assert_eq!(ai["model_alias"], serde_json::json!("main"));
        assert!(patch.secrets.is_empty());
    }

    #[test]
    fn empty_ai_payload_produces_settings_scoped_empty_patch() {
        // 空请求 = 无变化：补丁只含空的 ai 节，不改写其他运行设置。
        let patch = build_ai_config_patch(payload(serde_json::json!({})));
        assert!(patch.secrets.is_empty());
        assert_eq!(patch.settings, serde_json::json!({ "ai": {} }));
    }

    #[test]
    fn memory_switch_patch_preserves_auto_capture_preference() {
        for enabled in [false, true] {
            let patch = build_ai_config_patch(payload(serde_json::json!({
                "agent_memory_enabled": enabled
            })));
            assert_eq!(
                patch.settings,
                serde_json::json!({
                    "ai": {"agent_memory_enabled": enabled}
                })
            );
            assert!(patch.secrets.is_empty());
        }
    }

    #[test]
    fn conversation_api_only_decodes_the_internal_context_role() {
        use nuono_app::agent::{AgentContext, input::USER_CONTEXT_ROLE};
        let ctx = AgentContext::from_runtime_event(
            1,
            &serde_json::json!({
                "post_type": "message", "message_type": "group",
                "user_id": 2, "group_id": 3, "message_id": "fixture-message",
                "raw_message": "这是发言内容"
            }),
        )
        .unwrap();
        let envelope = ctx.history_content("这是发言内容");
        let (role, content) =
            present_conversation_content(USER_CONTEXT_ROLE.into(), envelope.clone());
        assert_eq!(role, "user");
        assert!(content.contains("这是发言内容"));
        assert!(content.starts_with("[消息信息:"));
        assert!(!content.contains("\"version\""));
        assert_eq!(
            present_conversation_content("user".into(), envelope.clone()),
            ("user".into(), envelope)
        );
        assert_eq!(
            present_conversation_content(USER_CONTEXT_ROLE.into(), "旧记录".into()),
            ("user".into(), "旧记录".into())
        );
    }
    #[test]
    fn memory_policy_patch_is_explicit_and_preserves_other_ai_settings() {
        let patch = build_ai_config_patch(payload(
            serde_json::json!({"agent_memory_policy":{"organization_enabled":true}}),
        ));
        assert_eq!(
            patch.settings,
            serde_json::json!({"ai":{"agent_memory_policy":{"organization_enabled":true}}})
        );
        assert!(patch.secrets.is_empty());
    }
}
