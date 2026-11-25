//! 管理对话使用的模型选择与 Provider 构造。
//!
//! 显式模型 ID 允许测试已启用的独立模型，即使全局 AI 尚未开启。未找到该 ID
//! 或数据库读取失败时明确报错；别名路由选择一个模型，无法提供密钥时再用
//! 全局配置。这与 Agent 的候选列表、逐个尝试及健康熔断策略分别维护。

use crate::ai::{OpenAiCompatibleProvider, parse_provider_type};
use crate::config::{AIConfig, ConfigManager};
use crate::db::Database;
use crate::entity::ai_models::Model;
use crate::error::{NuonoError, Result};

/// 为一次管理对话构造 Provider，并返回本次请求使用的模型名。
///
/// 只读取配置与模型记录，不发送 AI 请求，也不依赖 HTTP 请求或服务状态类型。
pub async fn build_chat_provider(
    db: &Database,
    config: &ConfigManager,
    model_id: Option<i64>,
    model_override: Option<&str>,
) -> Result<(OpenAiCompatibleProvider, String)> {
    if let Some(mid) = model_id {
        let m = db
            .get_ai_model(mid)
            .await?
            .ok_or_else(|| NuonoError::NotFound(format!("模型 #{mid}")))?;
        {
            if !m.enabled {
                return Err(NuonoError::Conflict("该模型已禁用".into()));
            }
            let context = crate::ai::usage_tracking::model_context(&m, "model_test");
            let api_key = m.api_key.unwrap_or_default();
            if api_key.is_empty() {
                return Err(NuonoError::Conflict("模型未配置 API Key".into()));
            }
            let provider_type = parse_provider_type(&m.provider_type);
            let mut provider = OpenAiCompatibleProvider::new(&api_key, &m.base_url, provider_type)
                .map_err(|e| NuonoError::ConfigValidation(e.to_string()))?;
            provider = provider.model(&m.model_id);
            provider =
                provider.with_usage_observer(crate::ai::usage_tracking::tracking(db, context));
            let model_name = model_override.unwrap_or(&m.model_id).to_string();
            return Ok((provider, model_name));
        }
    }

    let cfg = config.get().await;
    if !cfg.ai.enabled {
        return Err(NuonoError::Conflict("AI 功能未启用".into()));
    }
    if cfg.ai.provider_router_enabled
        && let Some(m) = select_routed_model(db, &cfg.ai.chat_model_alias, &cfg.ai).await?
    {
        let context = crate::ai::usage_tracking::model_context(&m, "model_test");
        let api_key = m.api_key.unwrap_or_default();
        if !api_key.is_empty() {
            let provider_type = parse_provider_type(&m.provider_type);
            let mut provider = OpenAiCompatibleProvider::new(&api_key, &m.base_url, provider_type)
                .map_err(|e| NuonoError::ConfigValidation(e.to_string()))?;
            provider = provider.model(&m.model_id);
            provider =
                provider.with_usage_observer(crate::ai::usage_tracking::tracking(db, context));
            let model_name = model_override.unwrap_or(&m.model_id).to_string();
            return Ok((provider, model_name));
        }
    }
    let ai_cfg = cfg
        .ai
        .to_ai_config()
        .ok_or_else(|| NuonoError::Conflict("AI 未配置或缺少 API Key".into()))?;
    let provider = OpenAiCompatibleProvider::from_config(&ai_cfg)
        .map_err(|e| NuonoError::ConfigValidation(e.to_string()))?
        .with_usage_observer(crate::ai::usage_tracking::tracking(
            db,
            crate::ai::usage_tracking::legacy_context(&cfg.ai, "model_test"),
        ));
    let model_name = model_override.unwrap_or(&ai_cfg.model).to_string();
    Ok((provider, model_name))
}

async fn select_routed_model(
    db: &Database,
    scenario_alias: &str,
    ai: &AIConfig,
) -> Result<Option<Model>> {
    let aliases = route_aliases(scenario_alias, ai);
    if scenario_alias.trim().is_empty()
        && ai.model_alias.trim().is_empty()
        && let Some(model) = db.get_default_ai_model().await?.filter(|m| m.enabled)
    {
        return Ok(Some(model));
    }

    let models = db.list_ai_models().await?;
    for alias in aliases {
        if let Some(model) = models
            .iter()
            .find(|model| model.enabled && model_matches_route_alias(model, &alias))
        {
            return Ok(Some(model.clone()));
        }
    }
    db.get_default_ai_model().await
}

fn route_aliases(scenario_alias: &str, ai: &AIConfig) -> Vec<String> {
    let mut aliases = Vec::new();
    for value in std::iter::once(scenario_alias)
        .chain(std::iter::once(ai.model_alias.as_str()))
        .chain(ai.provider_fallback_chain.iter().map(String::as_str))
    {
        let value = value.trim();
        if !value.is_empty() {
            aliases.push(value.to_string());
        }
    }
    aliases
}

fn model_matches_route_alias(model: &Model, alias: &str) -> bool {
    nuono_app::ai::model_route_matches(
        model.id,
        &model.name,
        &model.model_id,
        &model.provider,
        alias,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NuonoConfig;

    fn config(ai: AIConfig) -> ConfigManager {
        ConfigManager::for_test(
            NuonoConfig {
                ai,
                ..NuonoConfig::default()
            },
            std::env::temp_dir().join("nuono-ai-provider-test.toml"),
        )
    }

    fn legacy_ai() -> AIConfig {
        AIConfig {
            enabled: true,
            api_key: Some("test-key".to_string()),
            model: "legacy-model".to_string(),
            provider_router_enabled: false,
            ..AIConfig::default()
        }
    }

    async fn model(db: &Database, name: &str, key: Option<&str>, default: bool) -> Model {
        db.create_ai_model(crate::db::CreateAiModel {
            name,
            provider: "test-provider",
            provider_type: "openai_compatible",
            model_id: &format!("{name}-model"),
            api_key: key,
            base_url: "https://example.invalid/v1",
            is_default: default,
        })
        .await
        .unwrap()
    }

    async fn disable(db: &Database, id: i64) {
        db.update_ai_model(
            id,
            crate::db::UpdateAiModel {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn explicit_model_can_be_tested_with_ai_off_but_cannot_bypass_model_validation() {
        let db = crate::test_support::test_db(2).await;
        let cfg = config(AIConfig::default());
        let ready = model(&db, "ready", Some("test-key"), false).await;
        let no_key = model(&db, "no-key", None, false).await;
        let disabled = model(&db, "disabled", Some("test-key"), false).await;
        disable(&db, disabled.id).await;

        let (_, selected) = build_chat_provider(&db, &cfg, Some(ready.id), None)
            .await
            .unwrap();
        assert_eq!(selected, "ready-model");
        let (_, selected) = build_chat_provider(&db, &cfg, Some(ready.id), Some("override"))
            .await
            .unwrap();
        assert_eq!(selected, "override");
        assert!(matches!(
            build_chat_provider(&db, &cfg, Some(no_key.id), None).await,
            Err(NuonoError::Conflict(error)) if error == "模型未配置 API Key"
        ));
        assert!(matches!(
            build_chat_provider(&db, &cfg, Some(disabled.id), None).await,
            Err(NuonoError::Conflict(error)) if error == "该模型已禁用"
        ));
        assert!(matches!(
            build_chat_provider(&db, &cfg, None, None).await,
            Err(NuonoError::Conflict(error)) if error == "AI 功能未启用"
        ));
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn explicit_model_never_silently_tests_another_provider() {
        let db = crate::test_support::test_db(2).await;
        let cfg = config(legacy_ai());
        assert!(matches!(
            build_chat_provider(&db, &cfg, Some(i64::MAX), None).await,
            Err(NuonoError::NotFound(_))
        ));
        db.shutdown().await.unwrap();
        assert!(matches!(
            build_chat_provider(&db, &cfg, Some(i64::MAX), None).await,
            Err(error) if error.kind() == "DatabaseQuery"
        ));
    }

    #[tokio::test]
    async fn stable_model_id_supports_responses_and_survives_rename() {
        let db = crate::test_support::test_db(2).await;
        let saved = db
            .create_ai_model(crate::db::CreateAiModel {
                name: "advanced",
                provider: "custom",
                provider_type: "responses",
                model_id: "reasoner",
                api_key: Some("test-key"),
                base_url: "https://example.invalid/v1",
                is_default: true,
            })
            .await
            .unwrap();
        let mut ai = legacy_ai();
        ai.provider_router_enabled = true;
        ai.chat_model_alias = format!("id:{}", saved.id);
        db.update_ai_model(
            saved.id,
            crate::db::UpdateAiModel {
                name: Some("renamed"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let (provider, name) = build_chat_provider(&db, &config(ai), None, None)
            .await
            .unwrap();
        assert_eq!(name, "reasoner");
        assert_eq!(provider.provider_type(), crate::ai::ProviderType::Responses);
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn chat_routing_keeps_alias_priority_default_and_single_selection_fallback() {
        let db = crate::test_support::test_db(2).await;
        let scenario = model(&db, "scenario", Some("test-key"), false).await;
        disable(&db, scenario.id).await;
        let common = model(&db, "common", Some("test-key"), false).await;
        let _fallback = model(&db, "fallback", Some("test-key"), false).await;
        let default = model(&db, "default", Some("test-key"), true).await;
        let mut ai = legacy_ai();
        ai.provider_router_enabled = true;
        ai.chat_model_alias = " SCENARIO ".to_string();
        ai.model_alias = " COMMON-model ".to_string();
        ai.provider_fallback_chain = vec![" ".to_string(), "fallback".to_string()];

        // 场景匹配被禁用时先用通用别名，别名支持模型名、模型 ID 和提供商名。
        assert_eq!(
            select_routed_model(&db, &ai.chat_model_alias, &ai)
                .await
                .unwrap()
                .unwrap()
                .id,
            common.id
        );
        assert!(model_matches_route_alias(&common, "TEST-PROVIDER"));
        assert!(!model_matches_route_alias(&common, "openai_compatible"));
        let (_, selected) = build_chat_provider(&db, &config(ai.clone()), None, None)
            .await
            .unwrap();
        assert_eq!(selected, "common-model");

        ai.chat_model_alias = "unknown".to_string();
        ai.model_alias.clear();
        ai.provider_fallback_chain.clear();
        assert_eq!(
            select_routed_model(&db, &ai.chat_model_alias, &ai)
                .await
                .unwrap()
                .unwrap()
                .id,
            default.id
        );
        assert_eq!(
            select_routed_model(&db, "", &ai).await.unwrap().unwrap().id,
            default.id
        );

        let _no_key = model(&db, "no-key", None, false).await;
        ai.chat_model_alias = "no-key".to_string();
        ai.model_alias = "common".to_string();
        // 管理对话只选一个匹配：缺少密钥后使用旧配置，不遍历下一个别名。
        let (_, selected) = build_chat_provider(&db, &config(ai), None, None)
            .await
            .unwrap();
        assert_eq!(selected, "legacy-model");
        db.shutdown().await.unwrap();
    }
}
