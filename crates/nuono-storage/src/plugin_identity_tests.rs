use super::*;
use nuono_app::{
    account_merge::{
        AccountMergeContext, AccountMergeParticipant, AccountMergeRead, AccountMergeTransaction,
        register_account_merge_participant, unregister_account_merge_participant,
    },
    plugin_manifest::*,
};
use nuono_domain::{BotIdentity, IdentityScheme, Platform};
use sea_orm::Statement;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

fn native_id(kind: IdentityKind, value: &str, namespace: Option<&str>) -> IdentityRef {
    IdentityRef::new(
        Platform::OneBot,
        kind,
        IdentityScheme::Native,
        value,
        namespace,
    )
    .unwrap()
}
fn bot() -> BotIdentity {
    BotIdentity::new(
        "identity-test-bot",
        native_id(IdentityKind::Bot, "10000", None),
        None,
    )
    .unwrap()
}

#[tokio::test]
async fn stored_platform_names_restore_native_enums_including_custom_platforms() {
    let db = crate::test_support::test_db(2).await;
    for platform in [
        Platform::OneBot,
        Platform::Telegram,
        Platform::Discord,
        Platform::Feishu,
        Platform::QqOfficial,
        Platform::Other("custom-adapter".into()),
    ] {
        let identity = IdentityRef::new(
            platform,
            IdentityKind::User,
            IdentityScheme::Native,
            "001234567890123456789",
            Some("application-scope"),
        )
        .unwrap();
        let registered = db.resolve_plugin_identity(&identity).await.unwrap();
        assert_eq!(registered.identity, identity);
        assert_eq!(registered.bound_identities, vec![identity]);
        assert!(matches!(
            registered.account,
            IdentityAccountState::Shadow { .. }
        ));
        let loaded = db
            .plugin_identity_by_id(registered.identity_id)
            .await
            .unwrap();
        assert_eq!(loaded, registered);
    }
    db.shutdown().await.unwrap();
}

async fn account(db: &Database, name: &str) -> users::Model {
    db.create_user(crate::db::NewUser {
        username: name.into(),
        email: None,
        password: "identity-tests-password".into(),
        role: "user".into(),
        status: "active".into(),
        email_verified: false,
    })
    .await
    .unwrap()
}
async fn candidate(
    db: &Database,
    user: &users::Model,
    identity: &IdentityRef,
    session: &str,
) -> crate::db::PlatformBindingView {
    let key = format!(
        "auth_session_{}",
        hex::encode(Sha256::digest(session.as_bytes()))
    );
    db.create_session_for_verified_user(
        user,
        &key,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let issued = db
        .begin_platform_binding(user.id, bot(), session, None)
        .await
        .unwrap();
    assert!(
        db.observe_platform_binding(
            issued.command.strip_prefix("绑定 ").unwrap(),
            &bot(),
            identity
        )
        .await
        .unwrap()
    );
    db.platform_binding(user.id, &issued.request.id, session)
        .await
        .unwrap()
}
async fn bind(db: &Database, user: &users::Model, identity: &IdentityRef, session: &str) {
    let candidate = candidate(db, user, identity, session).await;
    db.confirm_platform_binding_with_claim(
        user.id,
        &candidate.id,
        session,
        candidate.candidate.unwrap().revision.parse().unwrap(),
        candidate.data_claim_digest.as_deref(),
    )
    .await
    .unwrap();
}
#[tokio::test]
async fn identity_snapshots_distinguish_shadow_disabled_and_all_bound_identity_changes() {
    let db = crate::test_support::test_db(3).await;
    let user = account(&db, "identity_owner").await;
    let qq = native_id(IdentityKind::User, "20000", None);
    let other = native_id(IdentityKind::User, "20000", Some("other-app"));
    let shadow = db.resolve_plugin_identity(&qq).await.unwrap();
    assert!(shadow.active_user_id().unwrap() > 0);
    assert_eq!(shadow.active_registered_user_id().unwrap(), None);
    assert_eq!(
        db.resolve_plugin_identity(&qq).await.unwrap().identity_id,
        shadow.identity_id
    );
    assert_ne!(
        db.resolve_plugin_identity(&other)
            .await
            .unwrap()
            .identity_id,
        shadow.identity_id
    );
    bind(&db, &user, &qq, "identity-main").await;
    assert!(db.require_plugin_identity(&shadow).await.is_err());
    let first = db.resolve_plugin_identity(&qq).await.unwrap();
    assert_eq!(first.active_user_id().unwrap(), user.id);
    assert_eq!(first.active_registered_user_id().unwrap(), Some(user.id));
    bind(&db, &user, &other, "identity-other").await;
    assert!(db.require_plugin_identity(&first).await.is_err());
    let refreshed = db.resolve_plugin_identity(&qq).await.unwrap();
    assert_eq!(refreshed.bound_identities.len(), 2);
    let second = db.resolve_plugin_identity(&other).await.unwrap();
    db.unbind_platform_identity(user.id, second.identity_id, second.revision)
        .await
        .unwrap();
    assert!(db.require_plugin_identity(&refreshed).await.is_err());
    let unbound = db.resolve_plugin_identity(&other).await.unwrap();
    assert_eq!(unbound.active_registered_user_id().unwrap(), None);
    assert_ne!(unbound.active_user_id().unwrap(), user.id);
    db.orm()
        .execute_raw(Statement::from_sql_and_values(
            db.backend(),
            "UPDATE nuono_users SET status='disabled' WHERE id=$1",
            [user.id.into()],
        ))
        .await
        .unwrap();
    let disabled = db.resolve_plugin_identity(&qq).await.unwrap();
    assert!(matches!(
        disabled.account,
        IdentityAccountState::Account { .. }
    ));
    assert!(disabled.active_user_id().is_err());
    assert!(db.require_plugin_identity(&disabled).await.is_err());
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn canonical_group_excludes_delivery_bot_but_preserves_namespace_and_revision() {
    let db = crate::test_support::test_db(2).await;
    let conversation = ConversationRef::Group {
        group: native_id(IdentityKind::Group, "30000", None),
    };
    assert!(db.find_plugin_group(&conversation).await.unwrap().is_none());
    let a = db
        .resolve_plugin_group(&conversation)
        .await
        .unwrap()
        .unwrap();
    let b = db
        .resolve_plugin_group(&conversation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a, b);
    assert_eq!(
        db.find_plugin_group(&conversation).await.unwrap(),
        Some(a.clone())
    );
    let other = ConversationRef::Group {
        group: native_id(IdentityKind::Group, "30000", Some("tenant-two")),
    };
    assert_ne!(
        a.id,
        db.resolve_plugin_group(&other).await.unwrap().unwrap().id
    );
    let tx = db.begin_write().await.unwrap();
    require_group_on(&tx, &a).await.unwrap();
    tx.commit().await.unwrap();
    db.orm()
        .execute_raw(Statement::from_sql_and_values(
            db.backend(),
            "UPDATE nuono_groups SET revision=revision+1 WHERE id=$1",
            [a.id.into()],
        ))
        .await
        .unwrap();
    let tx = db.begin_write().await.unwrap();
    assert!(require_group_on(&tx, &a).await.is_err());
    tx.rollback().await.unwrap();
    db.shutdown().await.unwrap();
}

const PLUGIN: &str = "shadow_claim_probe";
const TABLE: &str = "shadow_claim_probe_players";
struct ShadowClaimMerge(Arc<AtomicU8>);
#[async_trait::async_trait]
impl AccountMergeParticipant for ShadowClaimMerge {
    fn version(&self) -> i64 {
        1
    }
    async fn preflight(
        &self,
        c: &AccountMergeContext,
        data: &dyn AccountMergeRead,
    ) -> Result<Value> {
        let rows = data
            .query(Statement::from_sql_and_values(
                data.backend(),
                format!("SELECT id,revision FROM {TABLE} WHERE user_id=$1 ORDER BY id"),
                [c.source_user_id.into()],
            ))
            .await?;
        let revisions = rows
            .iter()
            .map(|r| {
                Ok((
                    r.try_get::<i64>("", "id")?.to_string(),
                    r.try_get::<i64>("", "revision")?.to_string(),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(json!({"summary":"占位玩家资料并入正式账号","rows":revisions}))
    }
    async fn apply(
        &self,
        c: &AccountMergeContext,
        _: &Value,
        tx: &dyn AccountMergeTransaction,
    ) -> Result<()> {
        tx.execute(Statement::from_sql_and_values(
            tx.backend(),
            format!("UPDATE {TABLE} SET user_id=$1,revision=revision+1 WHERE user_id=$2"),
            [c.target_user_id.into(), c.source_user_id.into()],
        ))
        .await?;
        if self.0.load(Ordering::Relaxed) == 1 {
            return Err(NuonoError::Conflict("injected merge failure".into()));
        }
        Ok(())
    }
}
async fn register_claim(db: &Database) {
    let columns = "id BIGINT PRIMARY KEY, user_id BIGINT NOT NULL REFERENCES nuono_users(id), revision BIGINT NOT NULL";
    let pg = format!("CREATE TABLE {TABLE} ({columns});");
    let sqlite = format!(
        "CREATE TABLE {TABLE} ({}) STRICT;",
        columns.replace("BIGINT", "INTEGER")
    );
    let mut manifest = PortableManifest::empty(PLUGIN);
    manifest.postgres_schema = pg.clone();
    manifest.sqlite_schema = sqlite.clone();
    manifest.merge_participant_version = Some(1);
    manifest.account_references.push(AccountReference {
        table: TABLE.into(),
        column: "user_id".into(),
        merge_policy: AccountMergePolicy::FollowAccount,
    });
    db.register_plugin_storage(
        PLUGIN,
        "1",
        &PluginStorageDeclaration {
            manifest,
            versions: vec![PluginStorageVersion {
                version: 1,
                description: "shadow merge test".into(),
                postgres_sql: pg,
                sqlite_sql: sqlite,
            }],
        },
    )
    .await
    .unwrap();
}
#[tokio::test]
async fn branch_c_binding_requires_preview_and_rolls_back_plugin_and_shadow_together() {
    let db = crate::test_support::test_db(3).await;
    let user = account(&db, "claim_owner").await;
    let identity = native_id(IdentityKind::User, "40000", None);
    let shadow = db.resolve_plugin_identity(&identity).await.unwrap();
    let shadow_user_id = shadow.active_user_id().unwrap();
    register_claim(&db).await;
    db.orm()
        .execute_raw(Statement::from_sql_and_values(
            db.backend(),
            format!("INSERT INTO {TABLE}(id,user_id,revision) VALUES(1,$1,1)"),
            [shadow_user_id.into()],
        ))
        .await
        .unwrap();
    let session_key = format!(
        "auth_session_{}",
        hex::encode(Sha256::digest(b"claim-session"))
    );
    db.create_session_for_verified_user(
        &user,
        &session_key,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let issued = db
        .begin_platform_binding(user.id, bot(), "claim-session", None)
        .await
        .unwrap();
    assert!(
        db.observe_platform_binding(
            issued.command.strip_prefix("绑定 ").unwrap(),
            &bot(),
            &identity
        )
        .await
        .unwrap()
    );
    assert!(
        db.platform_binding(user.id, &issued.request.id, "claim-session")
            .await
            .is_err(),
        "未注册合并参与者时不得绕过占位数据合并"
    );
    let mode = Arc::new(AtomicU8::new(0));
    register_account_merge_participant(PLUGIN, Arc::new(ShadowClaimMerge(mode.clone()))).unwrap();
    let preview = db
        .platform_binding(user.id, &issued.request.id, "claim-session")
        .await
        .unwrap();
    let revision = preview
        .candidate
        .as_ref()
        .unwrap()
        .revision
        .parse()
        .unwrap();
    assert!(preview.data_claim_digest.is_some());
    assert!(
        db.confirm_platform_binding(user.id, &preview.id, "claim-session", revision)
            .await
            .is_err()
    );
    db.orm()
        .execute_raw(Statement::from_string(
            db.backend(),
            format!("UPDATE {TABLE} SET revision=revision+1 WHERE id=1"),
        ))
        .await
        .unwrap();
    assert!(
        db.confirm_platform_binding_with_claim(
            user.id,
            &preview.id,
            "claim-session",
            revision,
            preview.data_claim_digest.as_deref()
        )
        .await
        .is_err()
    );
    let current = db
        .platform_binding(user.id, &preview.id, "claim-session")
        .await
        .unwrap();
    mode.store(1, Ordering::Relaxed);
    assert!(
        db.confirm_platform_binding_with_claim(
            user.id,
            &current.id,
            "claim-session",
            revision,
            current.data_claim_digest.as_deref()
        )
        .await
        .is_err()
    );
    assert_eq!(
        db.resolve_plugin_identity(&identity)
            .await
            .unwrap()
            .active_user_id()
            .unwrap(),
        shadow_user_id
    );
    mode.store(0, Ordering::Relaxed);
    db.confirm_platform_binding_with_claim(
        user.id,
        &current.id,
        "claim-session",
        revision,
        current.data_claim_digest.as_deref(),
    )
    .await
    .unwrap();
    let bound = db.resolve_plugin_identity(&identity).await.unwrap();
    assert_eq!(bound.active_user_id().unwrap(), user.id);
    assert_eq!(bound.active_registered_user_id().unwrap(), Some(user.id));
    let row = db
        .orm()
        .query_one_raw(Statement::from_string(
            db.backend(),
            format!("SELECT user_id FROM {TABLE} WHERE id=1"),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.try_get::<i64>("", "user_id").unwrap(), user.id);
    unregister_account_merge_participant(PLUGIN);
    db.shutdown().await.unwrap();
}
