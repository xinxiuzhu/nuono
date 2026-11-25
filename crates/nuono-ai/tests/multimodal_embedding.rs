//! 使用固定测试桩验证多模态向量化的调用约束与输入校验，不测试真实模型效果。
use nuono_ai::embedding::*;
use nuono_error::Result;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};

fn model() -> EmbeddingModel {
    EmbeddingModel {
        id: 1,
        model: "fixture".into(),
        revision: "v1".into(),
    }
}
fn all() -> EmbeddingCapabilities {
    EmbeddingCapabilities {
        model: model(),
        modalities: BTreeSet::from([
            EmbeddingModality::Text,
            EmbeddingModality::Image,
            EmbeddingModality::Audio,
        ]),
    }
}
fn reference(mime: &str, bytes: &[u8]) -> EmbeddingAssetRef {
    EmbeddingAssetRef {
        asset_id: "asset_0123456789abcdef0123456789abcdef".into(),
        mime: mime.into(),
        sha256: hex::encode(Sha256::digest(bytes)),
        byte_length: bytes.len() as u64,
    }
}
fn media(mime: &str) -> EmbeddingMedia {
    let bytes = b"bounded fixture; not a real codec/model test".to_vec();
    EmbeddingMedia::new(reference(mime, &bytes), bytes).unwrap()
}

#[derive(Default)]
struct TextPort {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl EmbeddingPort for TextPort {
    async fn describe(&self, _: i64) -> Result<EmbeddingModel> {
        Ok(model())
    }
    async fn embed(&self, _: i64, input: Vec<String>) -> Result<EmbeddingBatch> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(EmbeddingBatch {
            model: model(),
            vectors: vec![vec![1.0, 0.5]; input.len()],
        })
    }
}
struct MediaPort;
#[async_trait::async_trait]
impl EmbeddingPort for MediaPort {
    async fn describe(&self, _: i64) -> Result<EmbeddingModel> {
        Ok(model())
    }
    async fn embed(&self, _: i64, _: Vec<String>) -> Result<EmbeddingBatch> {
        unreachable!("fixture uses the replaceable media port")
    }
    async fn capabilities(&self, _: i64) -> Result<EmbeddingCapabilities> {
        Ok(all())
    }
    async fn embed_inputs(&self, _: i64, inputs: Vec<EmbeddingInput>) -> Result<EmbeddingBatch> {
        Ok(EmbeddingBatch {
            model: model(),
            vectors: inputs
                .iter()
                .map(|input| match input.modality() {
                    EmbeddingModality::Text => vec![1.0, 0.1],
                    EmbeddingModality::Image => vec![0.1, 1.0],
                    EmbeddingModality::Audio => vec![0.5, 0.5],
                })
                .collect(),
        })
    }
}
#[test]
fn text_port_is_compatible_and_rejects_media_before_calling_legacy_embed() {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let port = TextPort::default();
            let old = port.embed(1, vec!["text".into()]).await.unwrap();
            let new = embed_multimodal(&port, 1, vec![EmbeddingInput::Text("text".into())])
                .await
                .unwrap();
            assert_eq!(old.vectors, new.vectors);
            assert_eq!(
                port.capabilities(1).await.unwrap().modalities,
                BTreeSet::from([EmbeddingModality::Text])
            );
            let error = embed_multimodal(&port, 1, vec![EmbeddingInput::Image(media("image/png"))])
                .await
                .unwrap_err();
            assert!(error.to_string().contains("unsupported"));
            assert!(
                port.embed_inputs(
                    1,
                    vec![EmbeddingInput::Audio {
                        media: media("audio/wav"),
                        duration_ms: 10
                    }]
                )
                .await
                .is_err()
            );
            assert_eq!(port.calls.load(Ordering::Relaxed), 2);
        });
}
#[test]
fn replaceable_port_returns_one_checked_vector_per_explicit_input() {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let batch = embed_multimodal(
                &MediaPort,
                1,
                vec![
                    EmbeddingInput::Text("text".into()),
                    EmbeddingInput::Image(media("image/png")),
                    EmbeddingInput::Audio {
                        media: media("audio/wav"),
                        duration_ms: 1000,
                    },
                ],
            )
            .await
            .unwrap();
            assert_eq!(
                batch.vectors,
                [vec![1.0, 0.1], vec![0.1, 1.0], vec![0.5, 0.5]]
            );
        });
}
#[test]
fn asset_contract_rejects_paths_urls_data_uris_hashes_and_byte_mismatch() {
    let bytes = b"fixture".to_vec();
    for id in [
        "/private/photo.png",
        "https://example.test/image.png",
        "data:image/png;base64,AA==",
        "asset_name",
        "asset_0123456789abcdef0123456789abcdeg",
    ] {
        let mut candidate = reference("image/png", &bytes);
        candidate.asset_id = id.into();
        assert!(EmbeddingMedia::new(candidate, bytes.clone()).is_err());
    }
    for hash in ["not-a-hash".into(), "f".repeat(64), "z".repeat(64)] {
        let mut candidate = reference("image/png", &bytes);
        candidate.sha256 = hash;
        assert!(EmbeddingMedia::new(candidate, bytes.clone()).is_err());
    }
    let mut candidate = reference("image/png", &bytes);
    candidate.byte_length += 1;
    assert!(EmbeddingMedia::new(candidate, bytes.clone()).is_err());
    let mut candidate = reference("image/png", &bytes);
    candidate.byte_length = MAX_IMAGE_BYTES as u64 + 1;
    assert!(EmbeddingMedia::new(candidate, bytes).is_err());
    assert!(!format!("{:?}", media("image/png")).contains("real codec"));
}
#[test]
fn modality_mime_duration_and_batch_budgets_are_bounded() {
    let caps = all();
    for inputs in [
        vec![],
        vec![EmbeddingInput::Text("x".into()); MAX_INPUTS + 1],
        vec![EmbeddingInput::Text("x".repeat(8193))],
        vec![EmbeddingInput::Text("x".repeat(8192)); 17],
        vec![EmbeddingInput::Image(media("audio/wav"))],
        vec![EmbeddingInput::Audio {
            media: media("image/png"),
            duration_ms: 100,
        }],
        vec![EmbeddingInput::Audio {
            media: media("audio/wav"),
            duration_ms: 0,
        }],
        vec![EmbeddingInput::Audio {
            media: media("audio/wav"),
            duration_ms: MAX_AUDIO_DURATION_MS + 1,
        }],
    ] {
        assert!(validate_multimodal_input(1, &inputs, &caps).is_err());
    }
    assert!(validate_multimodal_input(2, &[EmbeddingInput::Text("x".into())], &caps).is_err());
    let bytes = vec![1; MAX_AUDIO_BYTES + 1];
    let audio = EmbeddingMedia::new(reference("audio/wav", &bytes), bytes).unwrap();
    assert!(
        validate_multimodal_input(
            1,
            &[EmbeddingInput::Audio {
                media: audio,
                duration_ms: 1
            }],
            &caps
        )
        .is_err()
    );
    let bytes = vec![1; MAX_IMAGE_BYTES];
    let image = EmbeddingMedia::new(reference("image/png", &bytes), bytes).unwrap();
    assert!(
        validate_multimodal_input(
            1,
            &[
                EmbeddingInput::Image(image.clone()),
                EmbeddingInput::Image(image.clone())
            ],
            &caps
        )
        .is_ok()
    );
    assert!(
        validate_multimodal_input(
            1,
            &[
                EmbeddingInput::Image(image.clone()),
                EmbeddingInput::Image(image),
                EmbeddingInput::Image(media("image/png"))
            ],
            &caps
        )
        .is_err()
    );
}

enum InvalidResponse {
    Model,
    Rows,
}
struct MalformedPort(InvalidResponse);
#[async_trait::async_trait]
impl EmbeddingPort for MalformedPort {
    async fn describe(&self, _: i64) -> Result<EmbeddingModel> {
        Ok(model())
    }
    async fn embed(&self, _: i64, _: Vec<String>) -> Result<EmbeddingBatch> {
        let mut returned_model = model();
        let mut vectors = vec![vec![1.0]];
        match self.0 {
            InvalidResponse::Model => returned_model.revision = "changed".into(),
            InvalidResponse::Rows => vectors.clear(),
        }
        Ok(EmbeddingBatch {
            model: returned_model,
            vectors,
        })
    }
}
#[test]
fn checked_entry_rejects_changed_model_and_bad_vector_rows() {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            for kind in [InvalidResponse::Model, InvalidResponse::Rows] {
                assert!(
                    embed_multimodal(
                        &MalformedPort(kind),
                        1,
                        vec![EmbeddingInput::Text("test".into())]
                    )
                    .await
                    .is_err()
                );
            }
        });
    assert!(
        !format!("{:?}", EmbeddingInput::Text("private marker".into())).contains("private marker")
    );
}
