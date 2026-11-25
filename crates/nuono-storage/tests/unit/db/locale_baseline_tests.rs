use super::*;
use crate::entity::plugins;
use nuono_app::localization::{Locale, LocalePreferenceStore, LocaleScope};
use nuono_domain::{BotIdentity, IdentityKind, IdentityRef, IdentityScheme, Platform};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

fn scope() -> LocaleScope {
    let identity = |kind, id: &str| {
        IdentityRef::new(
            Platform::OneBot,
            kind,
            IdentityScheme::Native,
            id,
            None::<String>,
        )
        .unwrap()
    };
    LocaleScope::user(
        BotIdentity::new(
            "migration-instance",
            identity(IdentityKind::Bot, "bot"),
            None,
        )
        .unwrap(),
        identity(IdentityKind::User, "user"),
    )
    .unwrap()
}

#[tokio::test]
async fn baseline_creates_locale_preferences_and_preserves_records() {
    let db = crate::test_support::test_db(2).await;
    db.sync_plugin("existing", "1.2.3", &serde_json::json!({"keep":true}))
        .await
        .unwrap();
    db.set_plugin_enabled("existing", false).await.unwrap();
    db.set_global_config(
        "runtime.settings.v1",
        serde_json::json!({"core_commands":{"disabled_commands":["nuono重启"]}}),
    )
    .await
    .unwrap();
    let original_plugin = plugins::Entity::find()
        .filter(plugins::Column::PluginId.eq("existing"))
        .one(db.orm())
        .await
        .unwrap()
        .unwrap();
    let original_settings = db.get_global_config("runtime.settings.v1").await.unwrap();
    assert_eq!(db.get_locale_preference(&scope()).await.unwrap(), None);
    assert_eq!(
        plugins::Entity::find()
            .filter(plugins::Column::PluginId.eq("existing"))
            .one(db.orm())
            .await
            .unwrap(),
        Some(original_plugin)
    );
    assert_eq!(
        db.get_global_config("runtime.settings.v1").await.unwrap(),
        original_settings
    );

    db.set_locale_preference(&scope(), Some(Locale::Ja))
        .await
        .unwrap();
    run_framework(db.orm()).await.unwrap();
    assert_eq!(
        db.get_locale_preference(&scope()).await.unwrap(),
        Some(Locale::Ja)
    );
    db.shutdown().await.unwrap();
}
