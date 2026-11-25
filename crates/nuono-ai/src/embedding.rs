//! 为知识库索引提供职责清晰、边界受控的向量化（Embedding）接口。
mod multimodal;
pub use multimodal::{
    EmbeddingAssetRef, EmbeddingCapabilities, EmbeddingInput, EmbeddingMedia, EmbeddingModality,
    MAX_AUDIO_BYTES, MAX_AUDIO_DURATION_MS, MAX_IMAGE_BYTES, MAX_INPUTS, MAX_MEDIA_BATCH_BYTES,
    embed_multimodal, validate_multimodal_input,
};

use nuono_error::{NuonoError, Result};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EmbeddingModel {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub id: i64,
    pub model: String,
    /// 由宿主生成的端点与模型配置修订标识，不含任何敏感凭据。
    pub revision: String,
}

#[derive(Clone, Debug)]
pub struct EmbeddingBatch {
    pub model: EmbeddingModel,
    /// 按输入顺序一一对应的非零有限浮点向量，所有向量维度一致。
    pub vectors: Vec<Vec<f32>>,
}

/// 由宿主提供的向量化模型调用接口，调用方不会接触到模型的 API Key。
#[async_trait::async_trait]
pub trait EmbeddingPort: Send + Sync {
    async fn describe(&self, model_id: i64) -> Result<EmbeddingModel>;
    async fn embed(&self, model_id: i64, input: Vec<String>) -> Result<EmbeddingBatch>;

    /// 显式声明模型支持的输入模态；现有的纯文本服务商默认仅声明 `Text`。
    async fn capabilities(&self, model_id: i64) -> Result<EmbeddingCapabilities> {
        Ok(EmbeddingCapabilities::text_only(
            self.describe(model_id).await?,
        ))
    }

    /// 可扩展的多模态向量化入口。默认仅转发至纯文本接口，遇到图片或音频时直接返回不支持。
    /// 调用方应优先使用 `embed_multimodal`，以统一校验模型能力、输入限制以及返回的模型标识。
    async fn embed_inputs(
        &self,
        model_id: i64,
        inputs: Vec<EmbeddingInput>,
    ) -> Result<EmbeddingBatch> {
        let capabilities = EmbeddingCapabilities::text_only(self.describe(model_id).await?);
        validate_multimodal_input(model_id, &inputs, &capabilities)?;
        let text = inputs
            .into_iter()
            .map(|input| match input {
                EmbeddingInput::Text(text) => Ok(text),
                _ => Err(NuonoError::Validation(
                    "Embedding media is unsupported by this provider".into(),
                )),
            })
            .collect::<Result<Vec<_>>>()?;
        self.embed(model_id, text).await
    }
}

pub fn validate_input(model_id: i64, input: &[String]) -> Result<()> {
    if model_id <= 0
        || input.is_empty()
        || input.len() > 32
        || input
            .iter()
            .any(|text| text.trim().is_empty() || text.len() > 8192 || text.contains('\0'))
        || input.iter().map(String::len).sum::<usize>() > 128 * 1024
    {
        return Err(NuonoError::Validation(
            "Embedding input exceeds the bounded batch contract".into(),
        ));
    }
    Ok(())
}

pub fn validate_vectors(vectors: &[Vec<f32>], rows: usize) -> Result<()> {
    let dimensions = vectors.first().map_or(0, Vec::len);
    if rows == 0
        || vectors.len() != rows
        || !(1..=4096).contains(&dimensions)
        || vectors.iter().any(|vector| {
            vector.len() != dimensions
                || vector.iter().any(|value| !value.is_finite())
                || vector
                    .iter()
                    .map(|value| f64::from(*value).powi(2))
                    .sum::<f64>()
                    <= f64::EPSILON
        })
    {
        return Err(NuonoError::Validation(
            "Embedding response has invalid rows or vectors".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_and_vector_batches_reject_invalid_contracts() {
        let inputs = [
            (0, vec!["text".into()], false),
            (7, Vec::new(), false),
            (7, vec![" ".into()], false),
            (7, vec!["a\0b".into()], false),
            (7, vec!["a".repeat(8193)], false),
            (7, vec!["text".into(); 33], false),
            (7, vec!["a".repeat(8192); 17], false),
            (7, vec!["public test text".into()], true),
        ];
        for (model_id, input, expected) in inputs {
            assert_eq!(validate_input(model_id, &input).is_ok(), expected);
        }
        let vectors = [
            (Vec::new(), 0, false),
            (vec![vec![1.0]], 2, false),
            (vec![Vec::new()], 1, false),
            (vec![vec![0.0]], 1, false),
            (vec![vec![f32::NAN]], 1, false),
            (vec![vec![f32::INFINITY]], 1, false),
            (vec![vec![1.0; 4097]], 1, false),
            (vec![vec![1.0], vec![1.0, 2.0]], 2, false),
            (vec![vec![1.0, 2.0], vec![2.0, 1.0]], 2, true),
        ];
        for (vectors, rows, expected) in vectors {
            assert_eq!(validate_vectors(&vectors, rows).is_ok(), expected);
        }
    }
}
