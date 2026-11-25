use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{IndexBudget, RagError, Result};

/// 切分前的原始文档内容及调用方附带的元数据。
#[derive(Clone, Debug)]
pub struct Document {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub subtitle: String,
    pub source_key: String,
    pub text: String,
    pub metadata: Value,
}

/// 按 UTF-8 字符边界切分出的文档分块，`section` 分块序号从 1 开始。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chunk {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub subtitle: String,
    pub source_key: String,
    pub section: usize,
    pub sections: usize,
    pub text: String,
    pub embedding_text: String,
    pub metadata: Value,
}

/// 按 UTF-8 字符边界无损、无重叠地切分文本。
pub fn split_text(input: &str, max_bytes: usize, max_chunks: usize) -> Result<Vec<String>> {
    if max_bytes < 4 {
        return Err(RagError::ChunkHeaderBudget);
    }
    if input.contains('\0') {
        return Err(RagError::InvalidText);
    }
    if max_chunks == 0 {
        return Err(RagError::DocumentChunkBudget);
    }
    if input.is_empty() {
        return Ok(vec![String::new()]);
    }
    let mut result = Vec::new();
    let mut start = 0;
    while start < input.len() {
        let mut end = start.saturating_add(max_bytes).min(input.len());
        while !input.is_char_boundary(end) {
            end -= 1;
        }
        if result.len() >= max_chunks {
            return Err(RagError::DocumentChunkBudget);
        }
        result.push(input[start..end].to_string());
        start = end;
    }
    Ok(result)
}

/// 仅当整篇文档切分出的所有分块都满足语料库配额限制时，才将它们一并追加到分块列表中。
pub fn append_document(
    chunks: &mut Vec<Chunk>,
    document: Document,
    budget: &IndexBudget,
) -> Result<()> {
    if document.id.len() > 1024
        || document.title.len() > 2048
        || document.subtitle.len() > 2048
        || document.source_key.len() > 1024
    {
        return Err(RagError::DocumentIdentifiers);
    }
    let header = format!(
        "{}\n{}\n{}\n",
        document.title,
        document.subtitle,
        serde_json::to_string(&document.metadata)?
    );
    let room = budget
        .chunk_bytes
        .checked_sub(header.len())
        .ok_or(RagError::ChunkHeaderBudget)?;
    let sections = split_text(&document.text, room, budget.max_chunks)?;
    let total = sections.len();
    if chunks
        .len()
        .checked_add(total)
        .is_none_or(|count| count > budget.max_chunks)
    {
        return Err(RagError::CorpusChunkBudget);
    }
    let mut pending = Vec::with_capacity(total);
    for (index, text) in sections.into_iter().enumerate() {
        let embedding_text = format!("{header}{text}");
        if embedding_text.trim().is_empty() || embedding_text.len() > budget.chunk_bytes {
            return Err(RagError::InvalidEmbeddingText);
        }
        pending.push(Chunk {
            id: document.id.clone(),
            kind: document.kind.clone(),
            title: document.title.clone(),
            subtitle: document.subtitle.clone(),
            source_key: document.source_key.clone(),
            section: index + 1,
            sections: total,
            text,
            embedding_text,
            metadata: document.metadata.clone(),
        });
    }
    chunks.extend(pending);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document(text: &str) -> Document {
        Document {
            id: "a".into(),
            kind: "text".into(),
            title: "标题".into(),
            subtitle: "".into(),
            source_key: "source-v1".into(),
            text: text.into(),
            metadata: json!({"public":true}),
        }
    }

    #[test]
    fn complete_utf8_text_and_metadata_fit_each_embedding_page() {
        let text = "中文🙂AB".repeat(30);
        let budget = IndexBudget {
            chunk_bytes: 48,
            ..IndexBudget::default()
        };
        let mut chunks = Vec::new();
        append_document(&mut chunks, document(&text), &budget).unwrap();
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.text.as_str())
                .collect::<String>(),
            text
        );
        assert!(chunks.len() > 1);
        for (page, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.section, page + 1);
            assert_eq!(chunk.sections, chunks.len());
            assert!(chunk.embedding_text.len() <= 48);
            assert!(chunk.embedding_text.contains("{\"public\":true}"));
        }
        // 现有的上层缓存结构依赖这些扁平字段以及从 1 开始的分块序号。
        let value = serde_json::to_value(&chunks[0]).unwrap();
        assert_eq!(value["source_key"], "source-v1");
        assert_eq!(value["section"], 1);
        assert_eq!(serde_json::from_value::<Chunk>(value).unwrap(), chunks[0]);
    }

    #[test]
    fn rejected_documents_never_partially_append_to_the_corpus() {
        let mut chunks = Vec::new();
        let budget = IndexBudget {
            chunk_bytes: 48,
            max_chunks: 1,
            ..IndexBudget::default()
        };
        append_document(&mut chunks, document("ok"), &budget).unwrap();
        let original = chunks.clone();
        assert!(matches!(
            append_document(&mut chunks, document("more"), &budget),
            Err(RagError::CorpusChunkBudget)
        ));
        assert_eq!(chunks, original);
        assert!(matches!(
            split_text("bad\0text", 8, 5),
            Err(RagError::InvalidText)
        ));
        assert!(matches!(
            split_text("🙂🙂", 4, 1),
            Err(RagError::DocumentChunkBudget)
        ));
        assert!(matches!(
            split_text("a", 3, 1),
            Err(RagError::ChunkHeaderBudget)
        ));
    }
}
