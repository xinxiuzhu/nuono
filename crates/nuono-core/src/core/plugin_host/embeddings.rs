//! 插件可选的显式向量化（Embedding）模型调用接口；不猜测默认模型，也不在启动时发起请求。
use crate::{
    db::Database,
    error::{NuonoError, Result},
    plugin::{PluginHttpClient, PluginTaskScope},
};
use nuono_plugin::embedding::{
    PluginEmbeddingBatch, PluginEmbeddingModel, PluginEmbeddingPort, PluginEmbeddings,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;

struct HostEmbeddings {
    db: Database,
    http: Result<PluginHttpClient>,
}

pub(super) fn for_plugin(
    db: Database,
    http: PluginHttpClient,
    scope: Arc<PluginTaskScope>,
) -> PluginEmbeddings {
    PluginEmbeddings::from_port(
        Arc::new(HostEmbeddings {
            db,
            http: http
                .without_redirects_and_retries()
                .map_err(NuonoError::from),
        }),
        scope,
    )
}

fn invalid() -> NuonoError {
    NuonoError::Validation("Embedding model configuration or response is invalid".into())
}

fn describe(model: &crate::entity::ai_models::Model) -> Result<PluginEmbeddingModel> {
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
    let url = nuono_ai_provider::embedding::endpoint(&model.base_url).map_err(|_| invalid())?;
    Ok(PluginEmbeddingModel {
        id: model.id,
        model: model.model_id.clone(),
        revision: hex::encode(Sha256::digest(
            serde_json::to_vec(&json!([model.id, model.model_id, url.as_str()]))
                .map_err(|_| invalid())?,
        )),
    })
}

#[async_trait::async_trait]
impl PluginEmbeddingPort for HostEmbeddings {
    async fn describe(&self, model_id: i64) -> Result<PluginEmbeddingModel> {
        let model = self.db.get_ai_model(model_id).await?.ok_or_else(invalid)?;
        describe(&model)
    }

    async fn embed(&self, model_id: i64, input: Vec<String>) -> Result<PluginEmbeddingBatch> {
        nuono_ai::embedding::validate_input(model_id, &input)?;
        static GATE: std::sync::LazyLock<Semaphore> =
            std::sync::LazyLock::new(|| Semaphore::new(2));
        let _permit = tokio::time::timeout(Duration::from_secs(5), GATE.acquire())
            .await
            .map_err(|_| NuonoError::Validation("Embedding service is busy".into()))?
            .map_err(|_| invalid())?;
        let model = self.db.get_ai_model(model_id).await?.ok_or_else(invalid)?;
        let selected = describe(&model)?;
        let key = model
            .api_key
            .as_deref()
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(invalid)?;
        let http = self.http.as_ref().map_err(|_| invalid())?;
        let vectors = nuono_ai_provider::embedding::embed_with_usage(
            http.raw(),
            &model.base_url,
            &selected.model,
            key,
            &input,
            Some(crate::ai::usage_tracking::tracking(
                &self.db,
                crate::ai::usage_tracking::model_context(&model, "plugin_embedding"),
            )),
        )
        .await
        .map_err(|_| invalid())?;
        if self.describe(model_id).await? != selected {
            return Err(NuonoError::Validation(
                "Embedding model changed during request".into(),
            ));
        }
        Ok(PluginEmbeddingBatch {
            model: selected,
            vectors,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> crate::entity::ai_models::Model {
        crate::entity::ai_models::Model {
            id: 7,
            name: "Embedding".into(),
            provider: "Test provider".into(),
            provider_type: "openai_compatible".into(),
            model_id: "test-embedding".into(),
            api_key: Some("private-key".into()),
            base_url: "https://example.com/v1".into(),
            is_default: false,
            enabled: true,
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn host_embedding_identity_tracks_endpoint_and_model_without_credentials() {
        let mut model = model();
        let selected = describe(&model).unwrap();
        assert_eq!(selected.id, 7);
        assert_eq!(selected.model, "test-embedding");
        assert!(
            !serde_json::to_string(&selected)
                .unwrap()
                .contains("private-key")
        );

        model.base_url.push_str("/chat/completions");
        model.api_key = Some("rotated-private-key".into());
        assert_eq!(describe(&model).unwrap(), selected);

        model.model_id = "different-model".into();
        assert_ne!(describe(&model).unwrap().revision, selected.revision);
        model.model_id = selected.model.clone();
        model.base_url = "https://other.example.com/v1".into();
        assert_ne!(describe(&model).unwrap().revision, selected.revision);
    }

    #[test]
    fn host_embeddings_require_an_enabled_supported_selection() {
        let mut model = model();
        model.enabled = false;
        assert!(describe(&model).is_err());
        model.enabled = true;
        model.provider_type = "anthropic".into();
        assert!(describe(&model).is_err());
        model.provider_type = "responses".into();
        assert!(describe(&model).is_ok());
        model.model_id = " ".into();
        assert!(describe(&model).is_err());
    }
}
