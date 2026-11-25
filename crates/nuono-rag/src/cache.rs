use std::io::Write;

use serde::{
    Serialize,
    de::{DeserializeSeed, SeqAccess, Visitor},
};
use sha2::{Digest, Sha256};

use crate::{IndexBudget, RagError, Result};

/// 索引缓存的身份指纹，用于在复用缓存前核对当前语料与模型配置是否一致。
/// 采用借用字段仅用于内存比对，不影响调用方原有的索引落盘序列化格式。
#[derive(Debug, PartialEq)]
pub struct IndexIdentity<'a, M> {
    pub format_version: u32,
    pub rules_version: &'a str,
    pub corpus_sha256: &'a str,
    pub model: &'a M,
}

impl<M> Copy for IndexIdentity<'_, M> {}
impl<M> Clone for IndexIdentity<'_, M> {
    fn clone(&self) -> Self {
        *self
    }
}

pub fn validate_identity<M: PartialEq>(
    stored: IndexIdentity<'_, M>,
    current: IndexIdentity<'_, M>,
) -> Result<()> {
    if stored != current {
        return Err(RagError::StaleIndex);
    }
    Ok(())
}

pub struct VectorManifest<'a> {
    pub dimensions: usize,
    pub vector_bytes: usize,
    pub vectors_sha256: &'a str,
}

/// 先校验向量行列维度与内存占用，再校验数值合法性与包含维度信息的二进制 SHA-256 摘要。
pub fn validate_vector_manifest(
    vectors: &[Vec<f32>],
    rows: usize,
    manifest: VectorManifest<'_>,
    budget: &IndexBudget,
) -> Result<()> {
    let dimensions = vectors.first().map_or(0, Vec::len);
    let bytes = budget.vector_bytes(rows, dimensions)?;
    if vectors.len() != rows || dimensions != manifest.dimensions || bytes != manifest.vector_bytes
    {
        return Err(RagError::VectorBudget);
    }
    nuono_ai::embedding::validate_vectors(vectors, rows).map_err(|_| RagError::InvalidVectors)?;
    if vector_digest(vectors) != manifest.vectors_sha256 {
        return Err(RagError::InvalidVectors);
    }
    Ok(())
}

pub fn vector_digest(vectors: &[Vec<f32>]) -> String {
    let mut digest = Sha256::new();
    digest.update((vectors.len() as u64).to_le_bytes());
    digest.update((vectors.first().map_or(0, Vec::len) as u64).to_le_bytes());
    for vector in vectors {
        for value in vector {
            digest.update(value.to_bits().to_le_bytes());
        }
    }
    hex::encode(digest.finalize())
}

struct BoundedJson {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("index cache byte budget exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 在 JSON 序列化过程中实时限制写入字节数，超出上限时立即终止，避免分配超大内存缓冲区。
pub fn bounded_json(value: &impl Serialize, max_bytes: usize) -> Result<Vec<u8>> {
    let mut output = BoundedJson {
        bytes: Vec::new(),
        limit: max_bytes,
    };
    serde_json::to_writer(&mut output, value)?;
    Ok(output.bytes)
}

/// 在反序列化阶段流式校验向量数量、维度与总字节数上限。
pub struct VectorsSeed(pub IndexBudget);

impl<'de> DeserializeSeed<'de> for VectorsSeed {
    type Value = Vec<Vec<f32>>;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        decoder: D,
    ) -> std::result::Result<Self::Value, D::Error> {
        struct VectorSeed {
            dimensions: usize,
            remaining_values: usize,
        }
        impl<'de> DeserializeSeed<'de> for VectorSeed {
            type Value = Vec<f32>;
            fn deserialize<D: serde::Deserializer<'de>>(
                self,
                decoder: D,
            ) -> std::result::Result<Self::Value, D::Error> {
                decoder.deserialize_seq(self)
            }
        }
        impl<'de> Visitor<'de> for VectorSeed {
            type Value = Vec<f32>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a bounded finite vector")
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element::<f32>()? {
                    if values.len() >= self.dimensions
                        || values.len() >= self.remaining_values
                        || !value.is_finite()
                    {
                        return Err(serde::de::Error::custom(
                            "invalid vector dimensions, values, or budget",
                        ));
                    }
                    values.push(value);
                }
                Ok(values)
            }
        }
        struct Vectors(IndexBudget);
        impl<'de> Visitor<'de> for Vectors {
            type Value = Vec<Vec<f32>>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("bounded index vectors")
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut vectors = Vec::new();
                let mut total = 0usize;
                let max_values = self.0.max_vector_bytes / std::mem::size_of::<f32>();
                loop {
                    // 限制后续单行向量的最大可分配元素数，确保不会超出总向量数或总字节数上限。
                    let remaining_values = if vectors.len() < self.0.max_chunks {
                        max_values.saturating_sub(total)
                    } else {
                        0
                    };
                    let Some(vector) = seq.next_element_seed(VectorSeed {
                        dimensions: self.0.max_dimensions,
                        remaining_values,
                    })?
                    else {
                        break;
                    };
                    if vectors.len() >= self.0.max_chunks {
                        return Err(serde::de::Error::custom("vector storage budget exceeded"));
                    }
                    total += vector.len();
                    vectors.push(vector);
                }
                Ok(vectors)
            }
        }
        decoder.deserialize_seq(Vectors(self.0))
    }
}

/// 供 Serde `#[serde(deserialize_with = ...)]` 使用的默认反序列化函数（上限为 2048 行、4096 维、总计 16 MiB）。
pub fn deserialize_vectors<'de, D: serde::Deserializer<'de>>(
    decoder: D,
) -> std::result::Result<Vec<Vec<f32>>, D::Error> {
    VectorsSeed(IndexBudget::default()).deserialize(decoder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_borrows_models_without_requiring_model_clone_or_copy() {
        #[derive(Debug, PartialEq)]
        struct Model(String);
        let model = Model("model-revision-a".into());
        let identity = IndexIdentity {
            format_version: 1,
            rules_version: "v1",
            corpus_sha256: "corpus-a",
            model: &model,
        };
        let copied = identity;
        validate_identity(identity, copied).unwrap();
        validate_identity(identity, identity).unwrap();
        let second_copy = identity;
        validate_identity(second_copy, copied).unwrap();
    }

    #[test]
    fn cache_identity_and_vector_integrity_reject_stale_or_corrupt_data() {
        let identity = IndexIdentity {
            format_version: 1,
            rules_version: "utf8-v1",
            corpus_sha256: "source-a",
            model: &"model-a",
        };
        validate_identity(identity, identity).unwrap();
        for changed in [
            IndexIdentity {
                format_version: 2,
                ..identity
            },
            IndexIdentity {
                rules_version: "utf8-v2",
                ..identity
            },
            IndexIdentity {
                corpus_sha256: "source-b",
                ..identity
            },
            IndexIdentity {
                model: &"model-b",
                ..identity
            },
        ] {
            assert!(matches!(
                validate_identity(identity, changed),
                Err(RagError::StaleIndex)
            ));
        }
        let vectors = vec![vec![1.0, 0.0], vec![0.0, 1.0]];
        let digest = vector_digest(&vectors);
        let manifest = || VectorManifest {
            dimensions: 2,
            vector_bytes: 16,
            vectors_sha256: &digest,
        };
        validate_vector_manifest(&vectors, 2, manifest(), &IndexBudget::default()).unwrap();
        let mut corrupted = vectors.clone();
        corrupted[0][0] = 0.5;
        assert!(matches!(
            validate_vector_manifest(&corrupted, 2, manifest(), &IndexBudget::default()),
            Err(RagError::InvalidVectors)
        ));
        assert!(matches!(
            validate_vector_manifest(&vectors, 3, manifest(), &IndexBudget::default()),
            Err(RagError::VectorBudget)
        ));
        assert!(matches!(
            validate_vector_manifest(
                &vectors,
                2,
                manifest(),
                &IndexBudget {
                    max_vector_bytes: 15,
                    ..IndexBudget::default()
                }
            ),
            Err(RagError::VectorBudget)
        ));
        assert!(
            IndexBudget::default()
                .vector_bytes(usize::MAX, usize::MAX)
                .is_err()
        );
    }

    #[test]
    fn untrusted_json_is_bounded_during_parsing_and_serialization() {
        let budget = IndexBudget {
            max_chunks: 2,
            max_dimensions: 2,
            max_vector_bytes: 16,
            ..IndexBudget::default()
        };
        let decode = |input: &str| {
            VectorsSeed(budget).deserialize(&mut serde_json::Deserializer::from_str(input))
        };
        assert_eq!(
            decode("[[1,0],[0,1]]").unwrap(),
            vec![vec![1.0, 0.0], vec![0.0, 1.0]]
        );
        for input in ["[[1,0,0]]", "[[1,0],[0,1],[1]]", "[[],[],[]]", "[[1e100]]"] {
            assert!(decode(input).is_err(), "{input}");
        }
        let tight = IndexBudget {
            max_vector_bytes: 8,
            ..budget
        };
        assert!(
            VectorsSeed(tight)
                .deserialize(&mut serde_json::Deserializer::from_str("[[1,0],[1]]"))
                .is_err()
        );
        assert_eq!(bounded_json(&"abcd", 6).unwrap(), b"\"abcd\"");
        assert!(bounded_json(&"abcd", 5).is_err());
    }
}
