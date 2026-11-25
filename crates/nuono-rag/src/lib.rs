//! 文档分块、受限索引缓存及关键词/语义混合检索。
//!
//! 调用方负责传入文本或文档及可选的已确认嵌入向量。模型选择、权限校验、
//! 任务生命周期、数据源加载与缓存原子落盘均由调用方负责；本 crate 不直接访问数据库或外部模型服务。
//!
//! ```
//! use nuono_rag::{Document, IndexBudget, append_document, cosine_search};
//!
//! let mut chunks = Vec::new();
//! append_document(&mut chunks, Document {
//!     id: "opening-hours".into(),
//!     kind: "faq".into(),
//!     title: "Opening hours".into(),
//!     subtitle: String::new(),
//!     source_key: "faq-v1".into(),
//!     text: "The library opens at 09:00.".into(),
//!     metadata: serde_json::json!({"source": "public FAQ"}),
//! }, &IndexBudget::default())?;
//!
//! // 在实际应用中，这些向量来自其显式选定的 embedding 提供方。
//! let vectors = vec![vec![1.0, 0.0]];
//! let query_vector = [1.0, 0.0];
//! let hits = cosine_search(&chunks, &vectors, &query_vector, 3, 0.5)?;
//! assert_eq!(hits[0].document_id, "opening-hours");
//! assert_eq!(hits[0].score, 1.0);
//! # Ok::<(), nuono_rag::RagError>(())
//! ```

mod cache;
mod chunk;
mod hybrid;
mod lexical;
mod vector;

pub use cache::{
    IndexIdentity, VectorManifest, VectorsSeed, bounded_json, deserialize_vectors,
    validate_identity, validate_vector_manifest, vector_digest,
};
pub use chunk::{Chunk, Document, append_document, split_text};
pub use hybrid::{HybridHit, HybridOptions, HybridScore, hybrid_rank, hybrid_rank_optional};
pub use lexical::{Bm25Index, LexicalBudget, LexicalScore};
pub use vector::{SearchHit, cosine_scores, cosine_search};

/// 索引全局配额限制（含每个分块向量化输入中拼接的元数据头部）。
#[derive(Clone, Copy, Debug)]
pub struct IndexBudget {
    pub chunk_bytes: usize,
    pub max_chunks: usize,
    pub max_dimensions: usize,
    pub max_vector_bytes: usize,
    pub max_json_bytes: usize,
}

impl Default for IndexBudget {
    fn default() -> Self {
        Self {
            chunk_bytes: 4096,
            max_chunks: 2048,
            max_dimensions: 4096,
            max_vector_bytes: 16 * 1024 * 1024,
            max_json_bytes: 32 * 1024 * 1024,
        }
    }
}

impl IndexBudget {
    /// 在接收向量批次前，校验总行数、向量维度及总内存占用是否超出预算。
    pub fn vector_bytes(&self, rows: usize, dimensions: usize) -> Result<usize> {
        if rows == 0
            || rows > self.max_chunks
            || dimensions == 0
            || dimensions > self.max_dimensions
        {
            return Err(RagError::VectorBudget);
        }
        rows.checked_mul(dimensions)
            .and_then(|values| values.checked_mul(std::mem::size_of::<f32>()))
            .filter(|bytes| *bytes <= self.max_vector_bytes)
            .ok_or(RagError::VectorBudget)
    }
}

pub type Result<T> = std::result::Result<T, RagError>;

#[derive(Debug, thiserror::Error)]
pub enum RagError {
    #[error("document identifiers exceed their byte budget")]
    DocumentIdentifiers,
    #[error("document metadata leaves insufficient space for text")]
    ChunkHeaderBudget,
    #[error("document text contains a NUL character")]
    InvalidText,
    #[error("document exceeds the chunk count budget")]
    DocumentChunkBudget,
    #[error("corpus exceeds the chunk count budget")]
    CorpusChunkBudget,
    #[error("embedding text is empty or exceeds the chunk byte budget")]
    InvalidEmbeddingText,
    #[error("vector shape or size exceeds the index budget")]
    VectorBudget,
    #[error("index vectors or their digest are invalid")]
    InvalidVectors,
    #[error("query vector has an invalid dimension, nonfinite value, or zero norm")]
    InvalidQueryVector,
    #[error("query limit or threshold is invalid")]
    InvalidQuery,
    #[error("cosine similarity is nonfinite")]
    InvalidSimilarity,
    #[error("lexical index exceeds its row, text, or token budget")]
    LexicalBudget,
    #[error("lexical query exceeds its byte or token budget")]
    LexicalQueryBudget,
    #[error("hybrid scores, weights, or relevance thresholds are invalid")]
    InvalidHybridScore,
    #[error("index format, rules, corpus, or model identity has changed")]
    StaleIndex,
    #[error("index cache could not be serialized within its byte budget: {0}")]
    CacheSerialization(#[from] serde_json::Error),
}
