//! AI 模型目录变更指令与配置引用完整性校验（与数据库及 HTTP 层解耦）。
use crate::settings::AIConfig;
use nuono_error::{NuonoError, Result};
use serde_json::Value;

// 模型结构包含 API 密钥，刻意不派生 Debug 与 Serialize 以防凭据外泄。
pub struct CreateAiModel<'a> {
    pub name: &'a str,
    pub provider: &'a str,
    pub provider_type: &'a str,
    pub model_id: &'a str,
    pub api_key: Option<&'a str>,
    pub base_url: &'a str,
    pub is_default: bool,
}
#[derive(Default)]
pub struct UpdateAiModel<'a> {
    pub name: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub provider_type: Option<&'a str>,
    pub model_id: Option<&'a str>,
    pub api_key: Option<&'a str>,
    pub base_url: Option<&'a str>,
    pub is_default: Option<bool>,
    pub enabled: Option<bool>,
}
pub enum ModelChange<'a> {
    Create {
        input: CreateAiModel<'a>,
        enabled: bool,
    },
    Update {
        id: i64,
        patch: UpdateAiModel<'a>,
    },
    Delete {
        id: i64,
    },
}
#[derive(Clone)]
pub struct AiModelRecord {
    pub id: i64,
    pub name: String,
    pub provider: String,
    pub provider_type: String,
    pub model_id: String,
    pub api_key: Option<String>,
    pub base_url: String,
    pub is_default: bool,
    pub enabled: bool,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}
impl AiModelRecord {
    fn ready(&self) -> bool {
        self.enabled
            && matches!(
                self.provider_type.as_str(),
                "openai_compatible" | "responses"
            )
            && self
                .api_key
                .as_deref()
                .is_some_and(|key| !key.trim().is_empty())
    }
}

/// 校验 AI 设置中引用的模型 ID（`id:<编号>` 及向量模型 ID）在模型目录中均真实存在且已就绪。
/// 即便当前运行时存在环境变量覆盖，也不允许将指向无效模型的悬空 ID 引用写入配置；传统按名称/提供方别名的引用保持原有回退逻辑。
pub fn validate_references(
    settings: &Value,
    models: &[AiModelRecord],
    effective: bool,
) -> Result<()> {
    let Some(value) = settings.get("ai") else {
        return Ok(());
    };
    let ai: AIConfig = serde_json::from_value(value.clone())
        .map_err(|error| NuonoError::ConfigValidation(error.to_string()))?;
    for reference in [&ai.model_alias, &ai.chat_model_alias, &ai.tool_model_alias]
        .into_iter()
        .chain(ai.provider_fallback_chain.iter())
    {
        if let Some(id) = reference.trim().strip_prefix("id:") {
            let id: i64 = id
                .parse()
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(|| NuonoError::ConfigValidation("模型编号无效".into()))?;
            require_ready(models, id)?;
        }
    }
    if let Some(id) = ai.agent_memory_policy.embedding_model_id {
        require_ready(models, id)?;
    }
    if effective
        && ai.any_live_channel_active()
        && ai.provider_router_enabled
        && ai
            .api_key
            .as_deref()
            .is_none_or(|key| key.trim().is_empty())
        && !models.iter().any(AiModelRecord::ready)
    {
        return Err(NuonoError::ConfigValidation(
            "请先添加至少一个已启用且配置了密钥的模型".into(),
        ));
    }
    Ok(())
}
fn require_ready(models: &[AiModelRecord], id: i64) -> Result<()> {
    if models.iter().any(|model| model.id == id && model.ready()) {
        return Ok(());
    }
    Err(NuonoError::ConfigValidation(format!(
        "模型 #{id} 不存在、未启用、缺少密钥或协议不受支持；请先更换或移除相关绑定"
    )))
}
