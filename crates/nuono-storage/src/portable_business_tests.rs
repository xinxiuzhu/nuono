//! 跨数据库可移植迁移业务语义测试：验证在 SQLite 与 PostgreSQL 之间迁移后，不仅底层标量字节一致，账号、会话、配置覆盖与插件状态等上层业务语义也完全保持不变。
use crate::{
    Database,
    config::DatabaseConfig,
    db::{MergeCredentials, NewUser},
    portable,
};
use nuono_app::{
    configuration::{ConfigurationActor, ConfigurationMode},
    plugin_data::{PluginDataStore, PluginStateCollection, PluginStateScope},
    plugin_manifest::PluginStorageDeclaration,
};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde_json::json;
use sha2::{Digest, Sha256};
const PASSWORD: &str = "Portable-fixture-password-123!";
async fn account(db: &Database, name: &str, role: &str) -> crate::entity::users::Model {
    db.create_user(NewUser {
        username: name.into(),
        email: None,
        password: PASSWORD.into(),
        role: role.into(),
        status: "active".into(),
        email_verified: true,
    })
    .await
    .unwrap()
}
async fn session(db: &Database, user: &crate::entity::users::Model, token: &str) {
    db.create_session_for_verified_user(
        user,
        &format!(
            "auth_session_{}",
            hex::encode(Sha256::digest(token.as_bytes()))
        ),
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
}
async fn copy_to(source: &Database, target_url: &str) {
    let schema = portable::registered_schema(source.orm()).await.unwrap();
    let image = portable::export(source.orm(), &schema).await.unwrap();
    let mut options = sea_orm::ConnectOptions::new(target_url);
    options.max_connections(1).min_connections(1);
    if target_url.starts_with("sqlite:") {
        options.map_sqlx_sqlite_opts(|options| options.foreign_keys(true).create_if_missing(true));
    }
    let target = sea_orm::Database::connect(options).await.unwrap();
    let declared = portable::Schema::from_image(&image).unwrap();
    portable::import(&target, &declared, &image).await.unwrap();
    portable::verify(&target, &declared, &image).await.unwrap();
    target.close().await.unwrap();
}
#[tokio::test]
#[ignore = "requires an isolated PostgreSQL admin configured by NUONO_TEST_DATABASE_URL"]
async fn sqlite_postgres_sqlite_preserves_unbind_merge_grants_and_reinstallation() {
    let folder =
        std::env::temp_dir().join(format!("nuono-business-transfer-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&folder).unwrap();
    let source_cfg = DatabaseConfig {
        url: format!("sqlite://{}?mode=rwc", folder.join("source.db").display()),
        max_connections: 3,
    };
    let returned_cfg = DatabaseConfig {
        url: format!("sqlite://{}?mode=rwc", folder.join("returned.db").display()),
        max_connections: 3,
    };
    let admin_url = std::env::var("NUONO_TEST_DATABASE_URL").expect("isolated PostgreSQL URL");
    let pg_name = format!("nuono_test_{}", uuid::Uuid::new_v4().simple());
    let admin = sea_orm::Database::connect(&admin_url).await.unwrap();
    admin
        .execute_unprepared(&format!("CREATE DATABASE \"{pg_name}\""))
        .await
        .unwrap();
    let mut pg_url = url::Url::parse(&admin_url).unwrap();
    pg_url.set_path(&format!("/{pg_name}"));
    let pg_cfg = DatabaseConfig {
        url: pg_url.to_string(),
        max_connections: 3,
    };
    let source = Database::from_config(&source_cfg).await.unwrap();
    let owner = account(&source, "transfer-owner", "owner").await;
    assert_eq!(owner.id, 1);
    let red = account(&source, "transfer-red", "user").await;
    let blue = account(&source, "transfer-blue", "user").await;
    session(&source, &red, "red-session").await;
    session(&source, &blue, "blue-session").await;
    let group = source
        .ensure_group(
            &crate::groups::GroupIdentity {
                platform: "onebot".into(),
                namespace: "".into(),
                conversation_kind: "group:native".into(),
                native_parent_id: "".into(),
                native_group_id: "876543".into(),
            },
            "Transfer",
        )
        .await
        .unwrap();
    for plugin in ["portable_example", "portable_billing"] {
        source.sync_plugin(plugin, "1", &json!({})).await.unwrap();
        source
            .register_plugin_storage(
                plugin,
                "1",
                &PluginStorageDeclaration::from_migrations(plugin, vec![]).unwrap(),
            )
            .await
            .unwrap();
    }
    let policy = source.plugin_policy("portable_example").await.unwrap();
    source
        .save_plugin_policy(
            owner.id,
            "portable_example",
            crate::plugin_global_settings::PluginPolicyPatch {
                expected_revision: policy.revision,
                allow_user_overrides: Some(true),
                allow_group_overrides: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let actor = ConfigurationActor {
        user_id: owner.id,
        mode: ConfigurationMode::Administrator,
    };
    for (target, text) in [
        (
            crate::plugin_overrides::OverrideTarget::User(red.id),
            "个人值",
        ),
        (
            crate::plugin_overrides::OverrideTarget::Group(group.id),
            "群值",
        ),
    ] {
        source
            .save_plugin_override(
                &actor,
                "portable_example",
                target,
                crate::plugin_overrides::OverridePatch {
                    config_version: 1,
                    set_config: serde_json::Map::from_iter([("greeting".into(), json!(text))]),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    let now = chrono::Utc::now().timestamp_millis();
    let manual = source
        .grant_group_configuration(
            owner.id,
            &crate::group_authorizations::ManualGroupGrant {
                user_id: red.id,
                group_id: group.id,
                plugin_id: Some("portable_example".into()),
                source_key: "manual-revoked".into(),
                valid_from_ms: now - 1,
                expires_at_ms: Some(now + 3_600_000),
            },
        )
        .await
        .unwrap();
    source
        .revoke_group_configuration(owner.id, group.id, manual.id)
        .await
        .unwrap();
    source
        .set_plugin_grant_delegation(
            owner.id,
            "portable_billing",
            &nuono_app::group_authorization::PluginGrantDelegation {
                all_plugins: false,
                plugin_ids: vec!["portable_example".into()],
            },
        )
        .await
        .unwrap();
    let tx = PluginDataStore::begin_for_plugin(&source, "portable_billing")
        .await
        .unwrap();
    let paid = tx
        .issue_group_grant(&nuono_app::group_authorization::GroupGrantRequest {
            user_id: red.id,
            group_id: group.id,
            plugin_id: Some("portable_example".into()),
            source_key: "order-one".into(),
            valid_from_ms: now - 1,
            expires_at_ms: Some(now + 60_000),
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let bytes = vec![0, 1, 255, 4];
    source.orm().execute_raw(Statement::from_sql_and_values(DbBackend::Sqlite,"INSERT INTO nuono_runtime_secrets(key,owner_plugin_id,value,revision,created_at_ms,updated_at_ms) VALUES ('plugin.portable_example.binary','portable_example',$1,1,$2,$2)",[bytes.clone().into(),now.into()])).await.unwrap();
    let collection =
        PluginStateCollection::new("portable_example", "portable", PluginStateScope::global())
            .unwrap();
    PluginDataStore::state_set(
        &source,
        &collection,
        "numbers",
        1,
        r#"{"big":9007199254740993,"zero":0,"false":false,"null":null}"#.into(),
    )
    .await
    .unwrap();
    source.orm().execute_raw(Statement::from_sql_and_values(DbBackend::Sqlite,"INSERT INTO nuono_platform_identities(platform,namespace,identity_kind,native_user_id,user_id,verified_at_ms,revision,created_at_ms,updated_at_ms) VALUES ('onebot','','\"numeric\"','123456',$1,$2,1,$2,$2)",[red.id.into(),now.into()])).await.unwrap();
    source
        .mark_plugin_code_absent("portable_example")
        .await
        .unwrap();
    copy_to(&source, &pg_cfg.url).await;
    let pg = Database::from_config(&pg_cfg).await.unwrap();
    assert!(
        pg.authenticate_session("red-session")
            .await
            .unwrap()
            .is_some()
    );
    let identity = pg.platform_identities(red.id).await.unwrap().pop().unwrap();
    pg.unbind_platform_identity_in_session(
        red.id,
        identity.id.parse().unwrap(),
        identity.revision.parse().unwrap(),
        "red-session",
    )
    .await
    .unwrap();
    assert!(
        pg.plugin_override(
            "portable_example",
            crate::plugin_overrides::OverrideTarget::User(red.id)
        )
        .await
        .unwrap()
        .is_some()
    );
    let tx = PluginDataStore::begin_for_plugin(&pg, "portable_billing")
        .await
        .unwrap();
    tx.renew_group_grant(paid.id, paid.revision, Some(now + 120_000))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let job = pg
        .prepare_account_merge(
            blue.id,
            "blue-session",
            MergeCredentials {
                source_username: "transfer-red",
                source_password: PASSWORD,
                source_second_factor: None,
                target_password: PASSWORD,
                target_second_factor: None,
            },
        )
        .await
        .unwrap();
    pg.confirm_account_merge(
        blue.id,
        "blue-session",
        &job.id,
        job.revision.parse().unwrap(),
        &job.plan_digest,
    )
    .await
    .unwrap();
    assert!(
        pg.authenticate_session("red-session")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        pg.authenticate_session("blue-session")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        pg.group_configuration_grants(group.id)
            .await
            .unwrap()
            .iter()
            .all(|grant| grant.user_id == blue.id)
    );
    assert!(
        pg.group_configuration_grants(group.id)
            .await
            .unwrap()
            .iter()
            .find(|grant| grant.id == manual.id)
            .unwrap()
            .revoked_at_ms
            .is_some()
    );
    copy_to(&pg, &returned_cfg.url).await;
    let returned = Database::from_config(&returned_cfg).await.unwrap();
    returned
        .register_plugin_storage(
            "portable_example",
            "1",
            &PluginStorageDeclaration::from_migrations("portable_example", vec![]).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        returned
            .plugin_override(
                "portable_example",
                crate::plugin_overrides::OverrideTarget::User(blue.id)
            )
            .await
            .unwrap()
            .unwrap()
            .config_patch_json,
        r#"{"greeting":"个人值"}"#
    );
    assert!(
        returned
            .plugin_override(
                "portable_example",
                crate::plugin_overrides::OverrideTarget::Group(group.id)
            )
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        returned
            .find_user_by_id(red.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "merged"
    );
    // 解绑平台身份会在 nuono_users 中新建一条 shadow 占位用户（占用 blue.id + 1），
    // 回迁 SQLite 后新注册账号的自增 ID 应从 blue.id + 2 继续递增。
    assert_eq!(
        account(&returned, "transfer-red", "user").await.id,
        blue.id + 2
    );
    let row = returned
        .orm()
        .query_one_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT value FROM nuono_runtime_secrets WHERE key='plugin.portable_example.binary'",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.try_get::<Vec<u8>>("", "value").unwrap(), bytes);
    assert_eq!(
        PluginDataStore::state_get(&returned, &collection, "numbers")
            .await
            .unwrap()
            .unwrap()
            .value["big"]
            .as_i64(),
        Some(9007199254740993)
    );
    returned.shutdown().await.unwrap();
    pg.shutdown().await.unwrap();
    source.shutdown().await.unwrap();
    admin
        .execute_unprepared(&format!("DROP DATABASE \"{pg_name}\""))
        .await
        .unwrap();
    admin.close().await.unwrap();
    std::fs::remove_dir_all(folder).unwrap();
}
