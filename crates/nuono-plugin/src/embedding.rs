//! 供插件构建公共知识库索引时显式调用的宿主向量嵌入（Embedding）模型接口。
use std::{sync::Arc, time::Duration};

use crate::{NuonoError, NuonoResult, PluginTaskScope};

pub use nuono_ai::embedding::{
    EmbeddingBatch as PluginEmbeddingBatch, EmbeddingModel as PluginEmbeddingModel,
    EmbeddingPort as PluginEmbeddingPort, validate_input, validate_vectors,
};

#[derive(Clone, Default)]
pub struct PluginEmbeddings {
    port: Option<Arc<dyn PluginEmbeddingPort>>,
    scope: Option<Arc<PluginTaskScope>>,
}

impl std::fmt::Debug for PluginEmbeddings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginEmbeddings")
            .field("configured", &self.port.is_some())
            .finish_non_exhaustive()
    }
}

impl PluginEmbeddings {
    pub fn from_port(port: Arc<dyn PluginEmbeddingPort>, scope: Arc<PluginTaskScope>) -> Self {
        Self {
            port: Some(port),
            scope: Some(scope),
        }
    }

    pub async fn describe(&self, model_id: i64) -> NuonoResult<PluginEmbeddingModel> {
        if model_id <= 0 || self.scope.as_ref().is_some_and(|scope| scope.is_closed()) {
            return Err(NuonoError::Validation(
                "Embedding model is unset or plugin is disabled".into(),
            ));
        }
        self.port
            .as_ref()
            .ok_or_else(|| {
                NuonoError::Validation("Host embedding capability is unavailable".into())
            })?
            .describe(model_id)
            .await
    }

    /// 仅在插件显式调用此方法时才会向模型发送文本计算向量；宿主加载插件时绝不会自动在后台触发索引构建。
    pub async fn embed(
        &self,
        model_id: i64,
        input: Vec<String>,
    ) -> NuonoResult<PluginEmbeddingBatch> {
        validate_input(model_id, &input)?;
        let rows = input.len();
        let port = self.port.clone().ok_or_else(|| {
            NuonoError::Validation("Host embedding capability is unavailable".into())
        })?;
        let scope = self
            .scope
            .as_ref()
            .ok_or_else(|| NuonoError::Validation("Plugin task scope is unavailable".into()))?;
        let (mut send, receive) = tokio::sync::oneshot::channel();
        if !scope.try_spawn("public-knowledge-embedding", async move {
            let result = tokio::select! {
                biased;
                _ = send.closed() => return,
                result = tokio::time::timeout(Duration::from_secs(40), port.embed(model_id, input)) =>
                    result.unwrap_or_else(|_| Err(NuonoError::Validation("Embedding request timed out".into()))),
            };
            let _ = send.send(result);
        }) {
            return Err(NuonoError::Validation(
                "Plugin is disabled, its background task capacity is full, or its runtime is unavailable"
                    .into(),
            ));
        }
        let result = receive
            .await
            .map_err(|_| NuonoError::Validation("Embedding request was cancelled".into()))??;
        validate_vectors(&result.vectors, rows)?;
        if scope.is_closed() || result.model.id != model_id {
            return Err(NuonoError::Validation(
                "Embedding model or plugin scope changed".into(),
            ));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    struct PendingPort {
        entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        active: Arc<AtomicUsize>,
    }

    #[crate::async_trait]
    impl PluginEmbeddingPort for PendingPort {
        async fn describe(&self, model_id: i64) -> NuonoResult<PluginEmbeddingModel> {
            Ok(PluginEmbeddingModel {
                id: model_id,
                model: "synthetic-test-model".into(),
                revision: "synthetic-test-revision".into(),
            })
        }

        async fn embed(
            &self,
            _model_id: i64,
            _input: Vec<String>,
        ) -> NuonoResult<PluginEmbeddingBatch> {
            struct ActiveGuard(Arc<AtomicUsize>);
            impl Drop for ActiveGuard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::SeqCst);
                }
            }
            self.active.fetch_add(1, Ordering::SeqCst);
            let _guard = ActiveGuard(self.active.clone());
            if let Some(entered) = self.entered.lock().unwrap().take() {
                let _ = entered.send(());
            }
            std::future::pending::<NuonoResult<PluginEmbeddingBatch>>().await
        }
    }

    #[tokio::test]
    async fn scope_shutdown_cancels_embedding_and_reaps_the_pending_port() {
        let scope = Arc::new(PluginTaskScope::new("synthetic-embedding"));
        let (entered, ready) = tokio::sync::oneshot::channel();
        let active = Arc::new(AtomicUsize::new(0));
        let capability = PluginEmbeddings::from_port(
            Arc::new(PendingPort {
                entered: Mutex::new(Some(entered)),
                active: active.clone(),
            }),
            scope.clone(),
        );
        let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(
                capability.embed(7, vec!["public test text".into()]),
                async {
                    ready.await.expect("port entered before cancellation");
                    assert_eq!(active.load(Ordering::SeqCst), 1);
                    scope.shutdown().await;
                }
            )
        })
        .await
        .expect("scope shutdown must promptly finish the capability call");
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert!(scope.is_closed());
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(!scope.try_spawn("closed", async { panic!("closed scope cannot run work") }));
    }
}
