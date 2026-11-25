//! 宿主侧记忆向量化（Embedding）能力装配。不设默认模型，也不在启动时自动发起请求。
use crate::{
    db::Database,
    error::{NuonoError, Result},
};
use nuono_ai::embedding::{EmbeddingBatch, EmbeddingModel, EmbeddingPort};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;

struct MemoryEmbeddings {
    db: Database,
    http: Result<reqwest::Client>,
}

pub(crate) fn for_memory(db: Database) -> Arc<dyn EmbeddingPort> {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| invalid());
    Arc::new(MemoryEmbeddings { db, http })
}

fn invalid() -> NuonoError {
    NuonoError::Validation("记忆 Embedding 模型配置或响应无效".into())
}

fn describe(model: &crate::entity::ai_models::Model) -> Result<EmbeddingModel> {
    if !model.enabled
        || model.model_id.trim().is_empty()
        || model.model_id.len() > 256
        || !matches!(
            model.provider_type.as_str(),
            "openai_compatible" | "responses"
        )
    {
        return Err(invalid());
    }
    let endpoint =
        nuono_ai_provider::embedding::endpoint(&model.base_url).map_err(|_| invalid())?;
    Ok(EmbeddingModel {
        id: model.id,
        model: model.model_id.clone(),
        revision: hex::encode(Sha256::digest(
            serde_json::to_vec(&serde_json::json!([
                model.id,
                model.model_id,
                endpoint.as_str()
            ]))
            .map_err(|_| invalid())?,
        )),
    })
}

#[async_trait::async_trait]
impl EmbeddingPort for MemoryEmbeddings {
    async fn describe(&self, model_id: i64) -> Result<EmbeddingModel> {
        describe(&self.db.get_ai_model(model_id).await?.ok_or_else(invalid)?)
    }
    async fn embed(&self, model_id: i64, input: Vec<String>) -> Result<EmbeddingBatch> {
        nuono_ai::embedding::validate_input(model_id, &input)?;
        static GATE: std::sync::LazyLock<Semaphore> =
            std::sync::LazyLock::new(|| Semaphore::new(2));
        let _permit = tokio::time::timeout(Duration::from_secs(5), GATE.acquire())
            .await
            .map_err(|_| invalid())?
            .map_err(|_| invalid())?;
        let model = self.db.get_ai_model(model_id).await?.ok_or_else(invalid)?;
        let selected = describe(&model)?;
        let key = model
            .api_key
            .as_deref()
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(invalid)?;
        let vectors = nuono_ai_provider::embedding::embed_with_usage(
            self.http.as_ref().map_err(|_| invalid())?,
            &model.base_url,
            &model.model_id,
            key,
            &input,
            Some(crate::ai::usage_tracking::tracking(
                &self.db,
                crate::ai::usage_tracking::model_context(&model, "memory_embedding"),
            )),
        )
        .await
        .map_err(|_| invalid())?;
        if self.describe(model_id).await? != selected {
            return Err(invalid());
        }
        Ok(EmbeddingBatch {
            model: selected,
            vectors,
        })
    }
}
