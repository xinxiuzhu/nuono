use std::collections::BTreeMap;

use crate::{Chunk, RagError, Result};

/// 单篇文档的最佳匹配分块结果；具体的展示与分页由调用方控制。
#[derive(Clone, Debug, PartialEq)]
pub struct SearchHit<'a> {
    pub document_id: &'a str,
    pub score: f64,
    pub matched_section: usize,
}

/// 精确余弦相似度检索，按文档 ID 去重（保留最高分分块），平局时按文档 ID 确定性排序。
pub fn cosine_search<'a>(
    chunks: &'a [Chunk],
    vectors: &[Vec<f32>],
    query: &[f32],
    limit: usize,
    threshold: f64,
) -> Result<Vec<SearchHit<'a>>> {
    if limit == 0 || !threshold.is_finite() || !(-1.0..=1.0).contains(&threshold) {
        return Err(RagError::InvalidQuery);
    }
    if vectors.len() != chunks.len() {
        return Err(RagError::InvalidVectors);
    }
    let scores = cosine_scores(vectors, query)?;
    let mut best = BTreeMap::<&str, (f64, usize)>::new();
    for (chunk, score) in chunks.iter().zip(scores) {
        if score >= threshold {
            let entry = best.entry(&chunk.id).or_insert((score, chunk.section));
            if score > entry.0 {
                *entry = (score, chunk.section);
            }
        }
    }
    let mut best = best.into_iter().collect::<Vec<_>>();
    best.sort_by(|(a, (sa, _)), (b, (sb, _))| sb.total_cmp(sa).then_with(|| a.cmp(b)));
    Ok(best
        .into_iter()
        .take(limit)
        .map(|(document_id, (score, matched_section))| SearchHit {
            document_id,
            score,
            matched_section,
        })
        .collect())
}

/// 按输入顺序计算每行向量与查询向量的精确余弦相似度。
/// 语料向量与查询向量均须为有限浮点数且范数非零；调用方需在调用前自行校验向量配额。
pub fn cosine_scores(vectors: &[Vec<f32>], query: &[f32]) -> Result<Vec<f64>> {
    nuono_ai::embedding::validate_vectors(vectors, vectors.len())
        .map_err(|_| RagError::InvalidVectors)?;
    let query_norm = query
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    if query.len() != vectors[0].len()
        || query.iter().any(|value| !value.is_finite())
        || query_norm <= f64::EPSILON
    {
        return Err(RagError::InvalidQueryVector);
    }
    let mut scores = Vec::with_capacity(vectors.len());
    for vector in vectors {
        let norm = vector
            .iter()
            .map(|value| f64::from(*value).powi(2))
            .sum::<f64>()
            .sqrt();
        let product = query
            .iter()
            .zip(vector)
            .map(|(q, v)| f64::from(*q) * f64::from(*v))
            .sum::<f64>();
        let score = (product / (query_norm * norm)).clamp(-1.0, 1.0);
        if !score.is_finite() {
            return Err(RagError::InvalidSimilarity);
        }
        scores.push(score);
    }
    Ok(scores)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: &str, section: usize) -> Chunk {
        Chunk {
            id: id.into(),
            kind: "text".into(),
            title: id.into(),
            subtitle: "".into(),
            source_key: "test".into(),
            section,
            sections: 2,
            text: "text".into(),
            embedding_text: "text".into(),
            metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn analytical_cosine_scores_deduplicate_documents_and_keep_deterministic_ties() {
        let chunks = [chunk("a", 1), chunk("a", 2), chunk("b", 1), chunk("c", 1)];
        let vectors = vec![
            vec![0.0, 1.0],
            vec![1.0, 0.0],
            vec![1.0, 0.0],
            vec![-1.0, 0.0],
        ];
        let result = cosine_search(&chunks, &vectors, &[1.0, 0.0], 4, -1.0).unwrap();
        assert_eq!(
            result,
            [
                SearchHit {
                    document_id: "a",
                    score: 1.0,
                    matched_section: 2
                },
                SearchHit {
                    document_id: "b",
                    score: 1.0,
                    matched_section: 1
                },
                SearchHit {
                    document_id: "c",
                    score: -1.0,
                    matched_section: 1
                }
            ]
        );
        assert_eq!(
            cosine_search(&chunks, &vectors, &[1.0, 0.0], 1, 0.5)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn malformed_query_and_vectors_are_rejected_without_substitute_values() {
        let chunks = [chunk("a", 1)];
        let vectors = vec![vec![1.0, 0.0]];
        for query in [vec![0.0, 0.0], vec![f32::NAN, 0.0], vec![1.0]] {
            assert!(matches!(
                cosine_search(&chunks, &vectors, &query, 1, 0.0),
                Err(RagError::InvalidQueryVector)
            ));
        }
        assert!(cosine_search(&chunks, &vectors, &[1.0, 0.0], 1, f64::NAN).is_err());
        assert!(matches!(
            cosine_search(&chunks, &[vec![0.0, 0.0]], &[1.0, 0.0], 1, 0.0),
            Err(RagError::InvalidVectors)
        ));
        assert!(matches!(
            cosine_search(&chunks, &[], &[1.0, 0.0], 1, 0.0),
            Err(RagError::InvalidVectors)
        ));
    }
}
