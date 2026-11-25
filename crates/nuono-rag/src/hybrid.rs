use crate::{LexicalScore, RagError, Result};

/// 在结合关键词匹配度和时间新旧程度、访问频率或重要度加权之前，先按最低相关度阈值过滤。
/// 单纯的先验权重绝不会让不相关的记录通过筛选。
#[derive(Clone, Copy, Debug)]
pub struct HybridOptions {
    pub lexical_weight: f64,
    pub semantic_weight: f64,
    pub min_lexical_relevance: f64,
    pub min_lexical_coverage: f64,
    pub min_semantic_similarity: f64,
}

impl Default for HybridOptions {
    fn default() -> Self {
        Self {
            lexical_weight: 0.45,
            semantic_weight: 0.55,
            min_lexical_relevance: 0.12,
            min_lexical_coverage: 0.30,
            min_semantic_similarity: 0.55,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HybridScore {
    pub lexical: LexicalScore,
    /// 原始余弦相似度（包含负值）；在纯关键词检索模式下为 `None`。
    pub semantic: Option<f64>,
    /// 将关键词相关度与非负余弦相似度按权重融合后的最终相关度，范围固定在 `0.0..=1.0`。
    pub relevance: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HybridHit {
    pub row_index: usize,
    pub score: HybridScore,
}

/// 综合关键词匹配分数与可选的余弦相似度对文本行进行混合排序。
/// 输入的 `lexical` 数组必须来自同一次 `Bm25Index::scores` 调用。空查询或纯停用词查询即使余弦相似度很高也不会返回任何结果。
/// `semantic` 传 `None` 时退化为纯关键词检索；传入的每个余弦值都会作为已确认的语义相似度参与计算（若部分记录尚未生成向量缓存，请改用 `hybrid_rank_optional`）。
/// 分数完全相同时保持输入顺序；若需要跨数据源的稳定平局排序，调用方可预先按 ID 排列输入行。
pub fn hybrid_rank(
    lexical: &[LexicalScore],
    semantic: Option<&[f64]>,
    limit: usize,
    options: &HybridOptions,
) -> Result<Vec<HybridHit>> {
    let semantic = semantic.map(|scores| scores.iter().copied().map(Some).collect::<Vec<_>>());
    hybrid_rank_optional(lexical, semantic.as_deref(), limit, options)
}

/// 支持部分行缺失语义向量的混合排序。未命中向量缓存的行传 `None`，直接使用其关键词得分，不受其他已向量化行的影响。
/// 已确认的负余弦相似度仍保持为 `Some(负值)` 并参与加权，不会被误判为缓存未命中。
pub fn hybrid_rank_optional(
    lexical: &[LexicalScore],
    semantic: Option<&[Option<f64>]>,
    limit: usize,
    options: &HybridOptions,
) -> Result<Vec<HybridHit>> {
    if limit == 0 || !valid_options(options) {
        return Err(RagError::InvalidHybridScore);
    }
    if semantic.is_some_and(|scores| {
        scores.len() != lexical.len()
            || scores
                .iter()
                .flatten()
                .any(|score| !score.is_finite() || !(-1.0..=1.0).contains(score))
    }) {
        return Err(RagError::InvalidHybridScore);
    }
    let query_shape = lexical
        .first()
        .map(|score| (score.query_terms, score.query_exact_terms));
    let mut hits = Vec::new();
    for (row_index, lexical) in lexical.iter().enumerate() {
        if !valid_lexical(lexical)
            || Some((lexical.query_terms, lexical.query_exact_terms)) != query_shape
        {
            return Err(RagError::InvalidHybridScore);
        }
        if lexical.query_terms == 0 {
            continue;
        }
        let cosine = semantic.and_then(|scores| scores[row_index]);
        let lexical_relevant = options.lexical_weight > 0.0
            && lexical.matched_exact_terms > 0
            && lexical.coverage >= options.min_lexical_coverage
            && lexical.relevance >= options.min_lexical_relevance;
        let semantic_relevant = options.semantic_weight > 0.0
            && cosine.is_some_and(|score| score >= options.min_semantic_similarity);
        if !lexical_relevant && !semantic_relevant {
            continue;
        }
        let relevance = match cosine {
            Some(cosine) => {
                (options.lexical_weight * lexical.relevance
                    + options.semantic_weight * cosine.max(0.0))
                    / (options.lexical_weight + options.semantic_weight)
            }
            None => lexical.relevance,
        };
        hits.push(HybridHit {
            row_index,
            score: HybridScore {
                lexical: *lexical,
                semantic: cosine,
                relevance,
            },
        });
    }
    hits.sort_by(|a, b| {
        b.score
            .relevance
            .total_cmp(&a.score.relevance)
            .then(a.row_index.cmp(&b.row_index))
    });
    hits.truncate(limit);
    Ok(hits)
}

fn valid_options(options: &HybridOptions) -> bool {
    options.lexical_weight.is_finite()
        && options.semantic_weight.is_finite()
        && (0.0..=1.0).contains(&options.lexical_weight)
        && (0.0..=1.0).contains(&options.semantic_weight)
        && options.lexical_weight + options.semantic_weight > 0.0
        && [
            options.min_lexical_relevance,
            options.min_lexical_coverage,
            options.min_semantic_similarity,
        ]
        .into_iter()
        .all(|threshold| threshold.is_finite() && threshold > 0.0 && threshold <= 1.0)
}

fn valid_lexical(score: &LexicalScore) -> bool {
    score.bm25.is_finite()
        && score.bm25 >= 0.0
        && score.coverage.is_finite()
        && (0.0..=1.0).contains(&score.coverage)
        && score.relevance.is_finite()
        && (0.0..=1.0).contains(&score.relevance)
        && score.matched_terms <= score.query_terms
        && score.query_exact_terms <= score.query_terms
        && score.matched_exact_terms <= score.query_exact_terms
        && score.matched_exact_terms <= score.matched_terms
        && (score.matched_terms != 0
            || (score.bm25 == 0.0 && score.relevance == 0.0 && score.coverage == 0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Bm25Index, cosine_scores};

    #[test]
    fn fixed_memory_fixture_recalls_old_synonyms_exact_names_and_numbers() {
        // 调用方已选定归属作用域及当前有效的事实列表。
        // 测试中刻意将较早的记录放在前面，验证检索不会只盲目挑选最新记录。
        let texts = [
            "2019 年我在杭州租了第一间公寓。",
            "林小满的电话号码末四位是 3142。",
            "林小雨的电话号码末四位是 2718。",
            "我喜欢 EarlGrey，不喜欢 coffee。",
            "昨天的项目会议讨论了预算。",
        ];
        let index = Bm25Index::new(&texts).unwrap();
        let options = HybridOptions::default();
        let exact = hybrid_rank(&index.scores("林小满 3142").unwrap(), None, 1, &options).unwrap();
        assert_eq!(exact[0].row_index, 1);
        assert_eq!(exact[0].score.lexical.coverage, 1.0);
        let english = hybrid_rank(
            &index.scores("EARLGREY preference").unwrap(),
            None,
            2,
            &options,
        )
        .unwrap();
        assert_eq!(english[0].row_index, 3);

        // 用手工构造的向量模拟同义改写查询（字面上没有任何“公寓/租”关键词重合）。
        let vectors = vec![
            vec![1.0, 0.0],
            vec![0.0, 1.0],
            vec![0.0, 1.0],
            vec![0.0, 1.0],
            vec![0.0, 1.0],
        ];
        let semantic = cosine_scores(&vectors, &[1.0, 0.0]).unwrap();
        let lexical = index.scores("最早居住的城市").unwrap();
        assert_eq!(lexical[0].matched_exact_terms, 0);
        let synonym = hybrid_rank(&lexical, Some(&semantic), 3, &options).unwrap();
        assert_eq!(synonym.len(), 1);
        assert_eq!(synonym[0].row_index, 0);
        assert_eq!(synonym[0].score.semantic, Some(1.0));
    }

    #[test]
    fn unrelated_weak_and_empty_queries_do_not_create_candidates_for_priors() {
        let index = Bm25Index::new(&["青岛 海边", "我喜欢 EarlGrey", "项目会议预算"]).unwrap();
        let options = HybridOptions::default();
        for query in ["火星轨道周期", "", "what is the", "请问", "今天如何"] {
            let lexical = index.scores(query).unwrap();
            let semantic = if query == "火星轨道周期" {
                vec![0.2; 3]
            } else {
                vec![1.0; 3]
            };
            assert!(
                hybrid_rank(&lexical, Some(&semantic), 10, &options)
                    .unwrap()
                    .is_empty()
            );
        }
        let lexical = index
            .scores("青岛 成都 上海 西安 深圳 广州 天津 宁波")
            .unwrap();
        assert!(
            hybrid_rank(&lexical, None, 10, &options)
                .unwrap()
                .is_empty()
        );
        // 多字查询仅撞上单个汉字（1-gram）而未命中双字词组（2-gram）时，不予召回。
        let lexical = index.scores("岛屿").unwrap();
        assert!(lexical[0].matched_terms > 0);
        assert_eq!(lexical[0].matched_exact_terms, 0);
        assert!(
            hybrid_rank(&lexical, None, 10, &options)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn missing_semantics_and_stable_ties_are_explicit() {
        let index = Bm25Index::new(&["用户喜欢乌龙茶", "用户喜欢乌龙茶", "用户喜欢咖啡"]).unwrap();
        let lexical = index.scores("乌龙茶").unwrap();
        let options = HybridOptions::default();
        let first = hybrid_rank(&lexical, None, 10, &options).unwrap();
        assert_eq!(
            first.iter().map(|hit| hit.row_index).collect::<Vec<_>>(),
            [0, 1]
        );
        assert_eq!(first[0].score.relevance, first[0].score.lexical.relevance);
        assert_eq!(first, hybrid_rank(&lexical, None, 10, &options).unwrap());
        let partial = hybrid_rank(&lexical, Some(&[-1.0, -1.0, 0.2]), 10, &options).unwrap();
        assert_eq!(
            partial.iter().map(|hit| hit.row_index).collect::<Vec<_>>(),
            [0, 1]
        );
    }

    #[test]
    fn partial_cache_does_not_lower_missing_rows_or_discard_true_negative_cosines() {
        let index = Bm25Index::new(&["用户喜欢乌龙茶", "用户喜欢乌龙茶", "用户喜欢咖啡"]).unwrap();
        let lexical = index.scores("乌龙茶").unwrap();
        let options = HybridOptions::default();
        let baseline = hybrid_rank_optional(&lexical, None, 10, &options).unwrap();
        let partial =
            hybrid_rank_optional(&lexical, Some(&[None, Some(0.1), Some(0.2)]), 10, &options)
                .unwrap();
        assert_eq!(partial[0].row_index, 0);
        assert_eq!(partial[0].score, baseline[0].score);
        assert_eq!(partial[0].score.semantic, None);
        let negative =
            hybrid_rank_optional(&lexical, Some(&[Some(-1.0), None, Some(0.2)]), 10, &options)
                .unwrap();
        let confirmed_negative = negative.iter().find(|hit| hit.row_index == 0).unwrap();
        assert_eq!(confirmed_negative.score.semantic, Some(-1.0));
        assert_eq!(
            confirmed_negative.score.relevance,
            lexical[0].relevance * options.lexical_weight
        );
        assert!(hybrid_rank_optional(&lexical, Some(&[None]), 1, &options).is_err());
        assert!(
            hybrid_rank_optional(&lexical, Some(&[None, Some(f64::NAN), None]), 1, &options)
                .is_err()
        );
    }

    #[test]
    fn malformed_finite_ranges_shapes_and_thresholds_fail_closed() {
        let index = Bm25Index::new(&["oolong tea"]).unwrap();
        let lexical = index.scores("oolong").unwrap();
        for semantic in [vec![], vec![f64::NAN], vec![f64::INFINITY], vec![1.1]] {
            assert!(hybrid_rank(&lexical, Some(&semantic), 1, &HybridOptions::default()).is_err());
        }
        for options in [
            HybridOptions {
                lexical_weight: f64::NAN,
                ..Default::default()
            },
            HybridOptions {
                lexical_weight: 0.0,
                semantic_weight: 0.0,
                ..Default::default()
            },
            HybridOptions {
                min_lexical_relevance: 0.0,
                ..Default::default()
            },
            HybridOptions {
                min_semantic_similarity: -1.0,
                ..Default::default()
            },
        ] {
            assert!(hybrid_rank(&lexical, None, 1, &options).is_err());
        }
        let mut invalid = lexical;
        invalid[0].relevance = f64::NAN;
        assert!(hybrid_rank(&invalid, None, 1, &HybridOptions::default()).is_err());
    }
}
