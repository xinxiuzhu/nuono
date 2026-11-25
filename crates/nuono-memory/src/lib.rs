//! 有作用域的长期记忆策略、证据校验与有界回忆排序。
//!
//! 调用方提供允许的作用域、源记录与已确认的语义分数。
//! 数据库访问、模型选择、授权与提示组装仍由宿主负责。
//! 排序绝不会仅因为某条记录最近、置顶或关联到相关记录，
//! 就让一条无关记录通过准入。
pub mod evidence;
pub use evidence::{
    MemoryDetail, MemoryMediaKind, MemoryMediaRef, MemorySource, OriginalState, SourcedMemoryHit,
    attach_sources,
};

use chrono::{DateTime, Utc};
use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub id: i64,
    pub scope_type: String,
    pub scope_key: String,
    pub memory_key: String,
    pub content: String,
    pub tags_json: Option<String>,
    pub enabled: bool,
    #[serde(with = "nuono_domain::scalar::optional_utc_millis")]
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    pub created_by: Option<String>,
    pub updated_by: Option<String>,
    #[serde(with = "nuono_domain::scalar::utc_millis")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "nuono_domain::scalar::utc_millis")]
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryPolicy {
    pub forgetting_enabled: bool,
    pub organization_enabled: bool,
    pub half_life_days: u32,
    pub idle_secs: u64,
    pub interval_secs: u64,
    pub batch_size: u32,
    /// 宿主显式指定的向量化模型 ID；为 `None` 时仅使用关键词（BM25）检索。
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[serde(default)]
    pub embedding_model_id: Option<i64>,
    /// 仅靠语义相似度召回时所需的最低余弦相似度阈值。
    pub semantic_threshold: f64,
}
impl Default for MemoryPolicy {
    fn default() -> Self {
        Self {
            forgetting_enabled: false,
            organization_enabled: false,
            half_life_days: 30,
            idle_secs: 300,
            interval_secs: 3600,
            batch_size: 64,
            embedding_model_id: None,
            semantic_threshold: 0.5,
        }
    }
}
impl MemoryPolicy {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !(1..=3650).contains(&self.half_life_days)
            || !(30..=86400).contains(&self.idle_secs)
            || !(300..=604800).contains(&self.interval_secs)
            || !(2..=128).contains(&self.batch_size)
        {
            return Err(
                "记忆半衰期须为 1–3650 天，空闲时间 30–86400 秒，整理间隔 300–604800 秒，每批 2–128 条"
                    .into(),
            );
        }
        if self.embedding_model_id.is_some_and(|id| id <= 0)
            || !self.semantic_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.semantic_threshold)
            || self.semantic_threshold == 0.0
        {
            return Err("记忆 embedding 模型 ID 须为正数，语义门槛须大于 0 且不超过 1".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryProfile {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub memory_id: i64,
    pub pinned: bool,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub recall_count: i64,
    #[serde(with = "nuono_domain::scalar::optional_utc_millis")]
    #[serde(default)]
    pub last_recalled_at: Option<DateTime<Utc>>,
}
impl MemoryProfile {
    pub fn for_memory(memory: &MemoryEntry) -> Self {
        let tags: Vec<String> = memory
            .tags_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        Self {
            memory_id: memory.id,
            pinned: tags.iter().any(|t| t == "explicit" || t == "pinned"),
            recall_count: 0,
            last_recalled_at: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct MemoryScope {
    pub scope_type: String,
    pub scope_key: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryTopic {
    pub name: String,
    pub summary: String,
    #[serde(with = "nuono_domain::scalar::i64_strings")]
    pub source_ids: Vec<i64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryLink {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub from_id: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub to_id: i64,
    pub relation: String,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryGraph {
    pub topics: Vec<MemoryTopic>,
    pub links: Vec<MemoryLink>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryGraphRecord {
    pub scope: MemoryScope,
    pub source_fingerprint: String,
    pub source_limit: u32,
    pub graph: MemoryGraph,
    #[serde(with = "nuono_domain::scalar::utc_millis")]
    pub updated_at: DateTime<Utc>,
}

pub fn fingerprint(memories: &[MemoryEntry]) -> String {
    let mut rows = memories.to_vec();
    rows.sort_by_key(|m| m.id);
    hex::encode(Sha256::digest(
        serde_json::to_vec(&rows).expect("memory records serialize"),
    ))
}

impl MemoryGraph {
    pub fn validate(&self, memories: &[MemoryEntry]) -> Result<()> {
        let ids: BTreeSet<_> = memories.iter().map(|m| m.id).collect();
        if ids.len() != memories.len()
            || memories.first().is_some_and(|source| {
                memories.iter().any(|memory| {
                    memory.scope_type != source.scope_type || memory.scope_key != source.scope_key
                })
            })
        {
            return Err(NuonoError::Validation(
                "整理输入包含重复编号或跨作用域记录".into(),
            ));
        }
        let mut covered = BTreeSet::new();
        if self.topics.is_empty() || self.topics.len() > 32 || self.links.len() > 128 {
            return Err(NuonoError::Validation("整理结果主题或关联数量无效".into()));
        }
        for topic in &self.topics {
            if topic.name.trim().is_empty()
                || topic.name.chars().count() > 80
                || topic.summary.trim().is_empty()
                || topic.summary.chars().count() > 1000
                || topic.source_ids.is_empty()
                || topic.source_ids.len() > 128
                || topic.source_ids.iter().any(|id| !ids.contains(id))
            {
                return Err(NuonoError::Validation(
                    "整理结果包含无效主题或无来源的内容".into(),
                ));
            }
            covered.extend(topic.source_ids.iter().copied());
        }
        for link in &self.links {
            if link.from_id == link.to_id
                || !ids.contains(&link.from_id)
                || !ids.contains(&link.to_id)
                || !["related", "supports", "conflicts", "updates"]
                    .contains(&link.relation.as_str())
                || link.reason.trim().is_empty()
                || link.reason.chars().count() > 300
            {
                return Err(NuonoError::Validation("整理结果包含无效关联".into()));
            }
        }
        if covered != ids {
            return Err(NuonoError::Validation("整理结果遗漏了输入记忆".into()));
        }
        Ok(())
    }
}

pub fn strength(
    memory: &MemoryEntry,
    profile: &MemoryProfile,
    policy: &MemoryPolicy,
    now: DateTime<Utc>,
) -> f64 {
    if profile.pinned || !policy.forgetting_enabled {
        return 1.0;
    }
    let last = profile
        .last_recalled_at
        .unwrap_or(memory.updated_at)
        .max(memory.updated_at);
    let days = (now - last).num_seconds().max(0) as f64 / 86400.0;
    // 限制被召回次数的加成上限，防止高频召回让记忆强度无限增长。
    let half_life = f64::from(policy.half_life_days.max(1))
        * (1.0 + profile.recall_count.clamp(0, 10) as f64 * 0.1);
    0.5_f64.powf(days / half_life)
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryMaintenanceResult {
    pub status: String,
    pub scope: Option<MemoryScope>,
    pub topics: usize,
    pub links: usize,
    #[serde(default)]
    pub indexed_memories: usize,
}
impl MemoryMaintenanceResult {
    pub fn skipped(status: &str) -> Self {
        Self {
            status: status.into(),
            scope: None,
            topics: 0,
            links: 0,
            indexed_memories: 0,
        }
    }
}

/// 单次召回用于构建提示材料的语料硬上限。
#[derive(Debug, Clone, Copy)]
pub struct MemoryRecallBudget {
    pub lexical: nuono_rag::LexicalBudget,
    pub max_results: usize,
    pub max_output_bytes: usize,
}
impl Default for MemoryRecallBudget {
    fn default() -> Self {
        Self {
            lexical: nuono_rag::LexicalBudget::default(),
            max_results: 32,
            max_output_bytes: 64 * 1024,
        }
    }
}

/// 分别给出证据与生命周期先验，便于解释单次命中。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemoryScore {
    pub lexical: nuono_rag::LexicalScore,
    /// 已确认的原始余弦；`None` 表示该记录无语义证据。
    pub semantic: Option<f64>,
    pub relevance: f64,
    pub strength: f64,
    pub pinned: bool,
    pub score: f64,
}
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryHit {
    pub row_index: usize,
    pub memory_id: i64,
    pub score: MemoryScore,
}

/// 在应用任何生命周期先验之前，先按作用域、状态、过期与相关性过滤。
///
/// 语义分数必须对应该批记录内容与同一查询；
/// 宿主负责该身份检查。缺失分数时仍保留词法准入资格。
#[allow(clippy::too_many_arguments)]
pub fn rank_memories(
    candidates: &[MemoryEntry],
    allowed_scopes: &[MemoryScope],
    profiles: &HashMap<i64, MemoryProfile>,
    query: &str,
    semantic_scores: Option<&HashMap<i64, f64>>,
    policy: &MemoryPolicy,
    now: DateTime<Utc>,
    limit: usize,
    budget: &MemoryRecallBudget,
) -> Result<Vec<MemoryHit>> {
    policy.validate().map_err(NuonoError::Validation)?;
    if limit == 0 || allowed_scopes.is_empty() || query.trim().is_empty() {
        return Ok(Vec::new());
    }
    if candidates.len() > budget.lexical.max_rows
        || query.len() > budget.lexical.max_query_bytes
        || budget.max_results == 0
        || budget.max_output_bytes == 0
    {
        return Err(NuonoError::Validation(
            "记忆召回超出候选、查询或输出预算".into(),
        ));
    }
    let allowed: BTreeSet<_> = allowed_scopes
        .iter()
        .map(|scope| (scope.scope_type.as_str(), scope.scope_key.as_str()))
        .collect();
    let mut ids = BTreeSet::new();
    let mut rows = Vec::new();
    let mut documents = Vec::new();
    for (index, memory) in candidates.iter().enumerate() {
        if !memory.enabled
            || memory.expires_at.is_some_and(|expiry| expiry <= now)
            || !allowed.contains(&(memory.scope_type.as_str(), memory.scope_key.as_str()))
        {
            continue;
        }
        if !ids.insert(memory.id) {
            return Err(NuonoError::Validation("记忆召回包含重复 ID".into()));
        }
        let bytes = memory
            .memory_key
            .len()
            .checked_add(memory.content.len())
            .and_then(|size| size.checked_add(1))
            .filter(|size| *size <= budget.lexical.max_document_bytes)
            .ok_or_else(|| NuonoError::Validation("单条记忆超出检索文本预算".into()))?;
        let mut text = String::with_capacity(bytes);
        text.push_str(&memory.memory_key);
        text.push(' ');
        text.push_str(&memory.content);
        rows.push(index);
        documents.push(text);
    }
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let index = nuono_rag::Bm25Index::with_budget(&documents, budget.lexical)
        .map_err(|error| NuonoError::Validation(error.to_string()))?;
    let lexical = index
        .scores(query)
        .map_err(|error| NuonoError::Validation(error.to_string()))?;
    // 纯标点或停用词等无实际语义的查询直接返回空，
    // 避免仅凭模型返回的向量就召回无关记忆。
    if lexical.iter().all(|score| score.query_terms == 0) {
        return Ok(Vec::new());
    }
    let mut semantic = Vec::with_capacity(rows.len());
    let mut confirmed = false;
    for row in &rows {
        let value = semantic_scores.and_then(|scores| scores.get(&candidates[*row].id));
        if let Some(value) = value {
            if !value.is_finite() || !(-1.0..=1.0).contains(value) {
                return Err(NuonoError::Validation("记忆语义分数无效".into()));
            }
            confirmed = true;
        }
        semantic.push(value.copied());
    }
    let options = nuono_rag::HybridOptions {
        min_semantic_similarity: policy.semantic_threshold,
        ..Default::default()
    };
    let hits = nuono_rag::hybrid_rank_optional(
        &lexical,
        confirmed.then_some(semantic.as_slice()),
        rows.len(),
        &options,
    )
    .map_err(|error| NuonoError::Validation(error.to_string()))?;
    let mut ranked = Vec::with_capacity(hits.len());
    for hit in hits {
        let row = rows[hit.row_index];
        let memory = &candidates[row];
        let profile = profiles
            .get(&memory.id)
            .filter(|profile| profile.memory_id == memory.id)
            .cloned()
            .unwrap_or_else(|| MemoryProfile::for_memory(memory));
        let strength = strength(memory, &profile, policy, now);
        // 记忆强度和置顶标记仅在通过相关性门槛后做小幅加权调整，
        // 本身不会让无关记录产生相关性。
        let score =
            hit.score.relevance * (0.8 + 0.2 * strength + if profile.pinned { 0.05 } else { 0.0 });
        ranked.push(MemoryHit {
            row_index: row,
            memory_id: memory.id,
            score: MemoryScore {
                lexical: hit.score.lexical,
                semantic: semantic_scores
                    .and_then(|scores| scores.get(&memory.id))
                    .copied(),
                relevance: hit.score.relevance,
                strength,
                pinned: profile.pinned,
                score,
            },
        });
    }
    ranked.sort_by(|a, b| {
        b.score
            .score
            .total_cmp(&a.score.score)
            .then(
                candidates[b.row_index]
                    .updated_at
                    .cmp(&candidates[a.row_index].updated_at),
            )
            .then(b.memory_id.cmp(&a.memory_id))
    });
    let mut used = 0_usize;
    ranked.retain(|hit| {
        let memory = &candidates[hit.row_index];
        let bytes = memory.memory_key.len()
            + memory.content.len()
            + memory.scope_type.len()
            + memory.scope_key.len()
            + 96;
        let Some(next) = used.checked_add(bytes) else {
            return false;
        };
        if next > budget.max_output_bytes {
            return false;
        }
        used = next;
        true
    });
    ranked.truncate(limit.min(budget.max_results));
    Ok(ranked)
}
