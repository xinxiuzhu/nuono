use super::*;
use crate::conversation_store::{DIRECTION_INBOUND, ROLE_USER};
use crate::entity::{audit_events, messages};
use sea_orm::{ActiveModelTrait, EntityTrait, Set};

#[tokio::test]
async fn baseline_preserves_history_and_accepts_long_native_ids() {
    let db = crate::test_support::test_db(2).await;
    let now_ms = crate::clock::db_now().timestamp_millis();
    let original = messages::ActiveModel {
        platform: Set("onebot".into()),
        bot_instance_id: Set(Some("original-bot".into())),
        native_message_id: Set(Some("original-message".into())),
        native_sender_id: Set(Some("original-user".into())),
        native_group_id: Set(Some("original-group".into())),
        direction: Set(DIRECTION_INBOUND.into()),
        role: Set(ROLE_USER.into()),
        text_content: Set("keep the original history".into()),
        content_blocks_json: Set("[]".into()),
        raw_payload_json: Set(Some("keep original raw history".into())),
        occurred_at_ms: Set(now_ms),
        created_at_ms: Set(now_ms),
        ..Default::default()
    }
    .insert(db.orm())
    .await
    .unwrap();
    let original_command = audit_events::ActiveModel {
        category: Set(crate::audit::CATEGORY_COMMAND.into()),
        occurred_at_ms: Set(now_ms),
        action: Set("nuono状态".into()),
        bot_id: Set(Some("original-bot".into())),
        actor_label: Set(Some("original-user".into())),
        result: Set("ok".into()),
        details_json: Set("{}".into()),
        ..Default::default()
    }
    .insert(db.orm())
    .await
    .unwrap();
    let settings = serde_json::json!({"retained":true});
    db.set_global_config("native-history-fixture", settings.clone())
        .await
        .unwrap();
    let native = messages::ActiveModel {
        platform: Set("qq_official".into()),
        bot_instance_id: Set(Some("native-bot-".repeat(8))),
        native_message_id: Set(Some("native-message-".repeat(8))),
        native_sender_id: Set(Some("native-user-".repeat(8))),
        native_group_id: Set(Some("native-group-".repeat(8))),
        direction: Set(DIRECTION_INBOUND.into()),
        role: Set(ROLE_USER.into()),
        text_content: Set(String::new()),
        content_blocks_json: Set("[]".into()),
        occurred_at_ms: Set(now_ms),
        created_at_ms: Set(now_ms),
        ..Default::default()
    }
    .insert(db.orm())
    .await
    .unwrap();
    let native_command = audit_events::ActiveModel {
        category: Set(crate::audit::CATEGORY_COMMAND.into()),
        occurred_at_ms: Set(now_ms),
        action: Set("nuono状态".into()),
        bot_id: Set(native.bot_instance_id.clone()),
        actor_label: Set(native.native_sender_id.clone()),
        result: Set("ok".into()),
        details_json: Set("{}".into()),
        ..Default::default()
    }
    .insert(db.orm())
    .await
    .unwrap();
    run_framework(db.orm()).await.unwrap();
    for retained in [&original, &native] {
        assert_eq!(
            messages::Entity::find_by_id(retained.id)
                .one(db.orm())
                .await
                .unwrap()
                .as_ref(),
            Some(retained)
        );
    }
    for retained in [&original_command, &native_command] {
        assert_eq!(
            audit_events::Entity::find_by_id(retained.id)
                .one(db.orm())
                .await
                .unwrap()
                .as_ref(),
            Some(retained)
        );
    }
    assert_eq!(
        db.get_global_config("native-history-fixture")
            .await
            .unwrap(),
        Some(settings)
    );
    db.shutdown().await.unwrap();
}
