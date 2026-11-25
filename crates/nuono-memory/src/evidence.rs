//! 记忆召回的来源追溯与细节级别定义；仅做数据结构与权限边界校验，不负责解析附件或访问模型与数据库。
use std::collections::{BTreeSet, HashMap};

use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};

use crate::{MemoryEntry, MemoryHit, MemoryScope};

pub const MAX_MEMORY_SOURCES: usize = 32;
pub const MAX_SOURCED_HITS: usize = 128;

/// 标识当前来源还能提供何种程度的原始细节（完整原文、摘要或大纲），不代表记忆本身的置信度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryDetail {
    Full,
    Summary,
    Outline,
}

/// 原始记录的生命周期状态：自然过期回收后仍可保留摘要，但用户主动删除的记录绝不能继续作为召回来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginalState {
    Available,
    Reclaimed,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MemoryMediaKind {
    Image,
    Audio { duration_ms: u32 },
}

/// 仅保存由宿主托管的多媒体素材 ID 与校验元数据，绝不接受本地文件路径或远程下载 URL。
///
/// `asset_id` 格式固定为 `asset_` 加 32 位小写十六进制字符。宿主在校验账号与角色作用域后生成该 ID 并负责鉴权读取；
/// 单纯持有或猜中 ID 本身并不会获得素材读取权限。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryMediaRef {
    pub asset_id: String,
    pub mime: String,
    pub sha256: String,
    pub byte_length: u64,
    pub media: MemoryMediaKind,
}

impl MemoryMediaRef {
    /// 校验素材引用的 ID 格式及元数据配额限制（不读取原始文件，也不授予访问权限）。
    ///
    /// # Errors
    /// 当素材 ID、SHA-256 摘要、MIME 类型、字节大小或音频时长超出限制时返回错误。
    pub fn validate(&self) -> Result<()> {
        let valid_id = self.asset_id.strip_prefix("asset_").is_some_and(|id| {
            id.len() == 32
                && id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        let (max_bytes, valid_mime) = match self.media {
            MemoryMediaKind::Image => (
                32 * 1024 * 1024,
                matches!(
                    self.mime.as_str(),
                    "image/png" | "image/jpeg" | "image/webp"
                ),
            ),
            MemoryMediaKind::Audio { duration_ms } => {
                if !(1..=300_000).contains(&duration_ms) {
                    return Err(invalid("音频时长必须为 1–300000 毫秒"));
                }
                (
                    25 * 1024 * 1024,
                    matches!(
                        self.mime.as_str(),
                        "audio/wav" | "audio/mpeg" | "audio/ogg" | "audio/flac" | "audio/webm"
                    ),
                )
            }
        };
        if !valid_id
            || !valid_mime
            || self.sha256.len() != 64
            || !self.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || self.byte_length == 0
            || self.byte_length > max_bytes
        {
            return Err(invalid("记忆媒体引用无效或超过边界"));
        }
        Ok(())
    }
}

/// 单条可追溯的记忆来源引用（经多轮对话整理合并后的记忆可关联多个来源）。
///
/// `source_id` 由宿主分配（例如 `turn:<uuid>`），不接受 URL 或本地文件路径。
/// 从数据库反序列化出的结构仍须经过 `validate` 或 `attach_sources` 校验后，才可进入召回结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemorySource {
    pub source_id: String,
    pub scope: MemoryScope,
    pub detail: MemoryDetail,
    pub original: OriginalState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<MemoryMediaRef>,
}

impl MemorySource {
    /// 校验来源的作用域归属与生命周期状态；已主动删除的来源禁止再次用于记忆召回。
    ///
    /// # Errors
    /// 当来源跨越作用域、ID 或媒体元数据无效、状态为已删除，或原件已回收却仍声明为完整细节时返回错误。
    pub fn validate(&self, expected_scope: &MemoryScope) -> Result<()> {
        if self.scope != *expected_scope || !valid_scope(expected_scope) {
            return Err(invalid("记忆来源不属于请求作用域"));
        }
        if self.source_id.is_empty()
            || self.source_id.len() > 200
            || !self
                .source_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'-' | b'_'))
            || self.source_id.contains("::")
        {
            return Err(invalid("记忆来源需要稳定的宿主记录 ID"));
        }
        if self.original == OriginalState::Deleted {
            return Err(invalid("已删除来源不得重新用于记忆召回"));
        }
        if self.original == OriginalState::Reclaimed && self.detail == MemoryDetail::Full {
            return Err(invalid("已回收原件不能声明完整细节"));
        }
        if let Some(media) = &self.media {
            media.validate()?;
        }
        Ok(())
    }

    /// 仅在原始记录仍可用（`Available`）时返回待宿主鉴权解析的媒体引用。
    ///
    /// 返回 `Some` 仅代表引用存在，不代表已获读取授权，宿主仍须核查归属权限与底层文件；
    /// 原件已回收（`Reclaimed`）时返回 `Ok(None)`，已删除（`Deleted`）或跨作用域则直接报错拒绝。
    ///
    /// # Errors
    /// 来源本身未通过 `validate` 校验时直接返回错误，防止通过媒体查询绕过作用域或删除状态检查。
    pub fn readable_media(&self, expected_scope: &MemoryScope) -> Result<Option<&MemoryMediaRef>> {
        self.validate(expected_scope)?;
        Ok(if self.original == OriginalState::Available {
            self.media.as_ref()
        } else {
            None
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourcedMemoryHit {
    pub hit: MemoryHit,
    pub scope: MemoryScope,
    pub sources: Vec<MemorySource>,
}

/// 在完成记忆相关性排序后，为每条命中结果挂载同一作用域下的来源列表（不修改原有得分，也不虚构新证据）。
///
/// 单条记忆可关联多轮对话来源。若出现缺失来源、重复来源、跨作用域、行号与记忆 ID 不匹配或包含已删除来源，
/// 均采取严格失败（fail-closed）策略拒绝整批结果，绝不将无来源记录冒充为可追溯记忆。
///
/// # Errors
/// 任一命中结果或关联来源未通过边界校验时，拒绝整批数据并返回错误。
pub fn attach_sources(
    scope: &MemoryScope,
    candidates: &[MemoryEntry],
    hits: Vec<MemoryHit>,
    sources: &HashMap<i64, Vec<MemorySource>>,
) -> Result<Vec<SourcedMemoryHit>> {
    if !valid_scope(scope) || hits.len() > MAX_SOURCED_HITS {
        return Err(invalid("记忆召回作用域或结果数量无效"));
    }
    let mut seen_hits = BTreeSet::new();
    hits.into_iter()
        .map(|hit| {
            let memory = candidates
                .get(hit.row_index)
                .ok_or_else(|| invalid("记忆命中行号无效"))?;
            if memory.id != hit.memory_id
                || !seen_hits.insert(memory.id)
                || memory.scope_type != scope.scope_type
                || memory.scope_key != scope.scope_key
            {
                return Err(invalid("记忆命中编号、作用域或唯一性无效"));
            }
            let evidence = sources
                .get(&memory.id)
                .ok_or_else(|| invalid("记忆缺少来源"))?;
            if evidence.is_empty() || evidence.len() > MAX_MEMORY_SOURCES {
                return Err(invalid("记忆来源数量必须为 1–32"));
            }
            let mut ids = BTreeSet::new();
            for source in evidence {
                source.validate(scope)?;
                if !ids.insert(&source.source_id) {
                    return Err(invalid("记忆包含重复来源"));
                }
            }
            Ok(SourcedMemoryHit {
                hit,
                scope: scope.clone(),
                sources: evidence.clone(),
            })
        })
        .collect()
}

fn valid_scope(scope: &MemoryScope) -> bool {
    !scope.scope_type.trim().is_empty()
        && scope.scope_type.len() <= 64
        && !scope.scope_key.trim().is_empty()
        && scope.scope_key.len() <= 512
        && !scope.scope_type.chars().any(char::is_control)
        && !scope.scope_key.chars().any(char::is_control)
}

fn invalid(message: &str) -> NuonoError {
    NuonoError::Validation(message.into())
}
