//! 验证 PostgreSQL 与 SQLite 下 AI 模型的创建与增量更新行为是否一致。

use nuono_core::db::{CreateAiModel, UpdateAiModel};

#[tokio::test]
async fn model_patches_preserve_omitted_fields_and_support_explicit_clears() {
    let db = nuono_core::test_support::test_db(2).await;
    let first = db
        .create_ai_model(CreateAiModel {
            name: "original",
            provider: "fixture-provider",
            provider_type: "openai_compatible",
            model_id: "fixture-model",
            api_key: Some("fixture-key"),
            base_url: "https://example.invalid/v1",
            is_default: true,
        })
        .await
        .unwrap();
    assert!(first.enabled);
    let second = db
        .create_ai_model(CreateAiModel {
            name: "second",
            provider: "fixture-provider",
            provider_type: "responses",
            model_id: "second-model",
            api_key: None,
            base_url: "https://example.invalid/v1",
            is_default: false,
        })
        .await
        .unwrap();

    let unchanged = db
        .update_ai_model(first.id, UpdateAiModel::default())
        .await
        .unwrap();
    assert_eq!(unchanged.name, first.name);
    assert_eq!(unchanged.api_key.as_deref(), Some("fixture-key"));
    assert!(unchanged.enabled && unchanged.is_default);

    let edited = db
        .update_ai_model(
            first.id,
            UpdateAiModel {
                name: Some("renamed"),
                api_key: Some(""),
                enabled: Some(false),
                ..UpdateAiModel::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(edited.name, "renamed");
    assert_eq!(edited.api_key, None);
    assert!(!edited.enabled);
    assert_eq!(edited.provider, first.provider);
    assert_eq!(edited.provider_type, first.provider_type);
    assert_eq!(edited.model_id, first.model_id);
    assert_eq!(edited.base_url, first.base_url);
    assert!(edited.is_default);

    db.update_ai_model(
        second.id,
        UpdateAiModel {
            api_key: Some("replacement-key"),
            is_default: Some(true),
            ..UpdateAiModel::default()
        },
    )
    .await
    .unwrap();
    let selected = db.get_default_ai_model().await.unwrap().unwrap();
    assert_eq!(selected.id, second.id);
    assert_eq!(selected.api_key.as_deref(), Some("replacement-key"));
    assert!(!db.get_ai_model(first.id).await.unwrap().unwrap().is_default);
    db.shutdown().await.unwrap();
}
