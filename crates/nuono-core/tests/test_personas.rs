//! 验证人设（Persona）在 PostgreSQL 与 SQLite 后端下的持久化行为。
use nuono_app::persona::{PersonaAssetKind, PersonaBinding, PersonaBindingKey, PersonaInput};
use nuono_core::test_support;

const FIRST: &str = "11111111-1111-4111-8111-111111111111";
const SECOND: &str = "22222222-2222-4222-8222-222222222222";
fn input(name: &str) -> PersonaInput {
    PersonaInput {
        name: name.into(),
        system_prompt: format!("{name}的性格"),
        ..Default::default()
    }
}
fn binding(persona: &str) -> PersonaBinding {
    PersonaBinding {
        platform: "onebot".into(),
        bot_id: "1".into(),
        group_id: "2".into(),
        namespace: None,
        persona_id: persona.into(),
    }
}

#[tokio::test]
async fn persona_crud_cas_and_asset_updates_preserve_other_values() {
    let db = test_support::test_db(3).await;
    let first = db.create_ai_persona(FIRST, input("小菲比")).await.unwrap();
    assert_eq!(first.revision, 1);
    assert_eq!(db.list_ai_personas().await.unwrap().len(), 1);
    let avatar = format!("{SECOND}.png");
    let updated = db
        .set_persona_asset(
            FIRST,
            PersonaAssetKind::Avatar,
            Some(avatar.clone()),
            first.revision,
        )
        .await
        .unwrap();
    assert_eq!(updated.avatar_filename.as_deref(), Some(avatar.as_str()));
    assert!(
        db.update_ai_persona(FIRST, first.revision, input("陈旧编辑"))
            .await
            .is_err()
    );
    let named = db
        .update_ai_persona(FIRST, updated.revision, input("更新角色"))
        .await
        .unwrap();
    assert_eq!(named.avatar_filename, updated.avatar_filename);
    assert!(
        db.set_persona_asset(
            FIRST,
            PersonaAssetKind::Portrait,
            Some("../outside.png".into()),
            named.revision
        )
        .await
        .is_err()
    );
    assert_eq!(db.get_ai_persona(FIRST).await.unwrap().unwrap(), named);
    assert!(db.delete_ai_persona(FIRST, first.revision).await.is_err());
    db.delete_ai_persona(FIRST, named.revision).await.unwrap();
    assert!(db.get_ai_persona(FIRST).await.unwrap().is_none());
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn persona_binding_exact_tuple_disabled_fallback_and_delete_protection() {
    let db = test_support::test_db(3).await;
    db.create_ai_persona(FIRST, input("角色 A")).await.unwrap();
    db.create_ai_persona(SECOND, input("角色 B")).await.unwrap();
    let a = binding(FIRST);
    db.upsert_ai_persona_binding(a.clone()).await.unwrap();
    let mut b = binding(SECOND);
    b.platform = "telegram".into();
    db.upsert_ai_persona_binding(b.clone()).await.unwrap();
    assert_eq!(
        db.resolve_ai_persona(&a.key()).await.unwrap().unwrap().id,
        FIRST
    );
    assert_eq!(
        db.resolve_ai_persona(&b.key()).await.unwrap().unwrap().id,
        SECOND
    );
    assert!(
        db.resolve_ai_persona(&PersonaBindingKey {
            bot_id: "other".into(),
            ..a.key()
        })
        .await
        .unwrap()
        .is_none()
    );
    assert!(db.delete_ai_persona(FIRST, 1).await.is_err());
    let mut disabled = input("角色 A");
    disabled.enabled = false;
    let record = db.update_ai_persona(FIRST, 1, disabled).await.unwrap();
    assert!(db.resolve_ai_persona(&a.key()).await.unwrap().is_none());
    assert_eq!(db.list_ai_persona_bindings().await.unwrap().len(), 2);
    db.remove_ai_persona_binding(&a.key()).await.unwrap();
    db.delete_ai_persona(FIRST, record.revision).await.unwrap();
    assert_eq!(
        db.resolve_ai_persona(&b.key()).await.unwrap().unwrap().id,
        SECOND
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn persona_namespaced_bindings_and_upsert_are_independent() {
    let db = test_support::test_db(3).await;
    db.create_ai_persona(FIRST, input("A")).await.unwrap();
    db.create_ai_persona(SECOND, input("B")).await.unwrap();
    let mut first = binding(FIRST);
    first.platform = "qq_official".into();
    first.bot_id = "app".into();
    first.group_id = "group-openid".into();
    first.namespace = Some("app-a".into());
    let mut second = first.clone();
    second.namespace = Some("app-b".into());
    second.persona_id = SECOND.into();
    db.upsert_ai_persona_binding(first.clone()).await.unwrap();
    db.upsert_ai_persona_binding(second.clone()).await.unwrap();
    first.persona_id = SECOND.into();
    db.upsert_ai_persona_binding(first.clone()).await.unwrap();
    assert_eq!(db.list_ai_persona_bindings().await.unwrap().len(), 2);
    db.remove_ai_persona_binding(&first.key()).await.unwrap();
    assert!(db.resolve_ai_persona(&first.key()).await.unwrap().is_none());
    assert_eq!(
        db.resolve_ai_persona(&second.key())
            .await
            .unwrap()
            .unwrap()
            .id,
        SECOND
    );
    let mut invalid = second;
    invalid.namespace = None;
    assert!(db.upsert_ai_persona_binding(invalid).await.is_err());
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_persona_binding_and_deletion_never_leave_a_dangling_binding() {
    let db = test_support::test_db(3).await;
    db.create_ai_persona(FIRST, input("A")).await.unwrap();
    let (deleted, bound) = tokio::join!(
        db.delete_ai_persona(FIRST, 1),
        db.upsert_ai_persona_binding(binding(FIRST))
    );
    assert!(deleted.is_ok() || bound.is_ok());
    let exists = db.get_ai_persona(FIRST).await.unwrap().is_some();
    let bindings = db.list_ai_persona_bindings().await.unwrap();
    if bindings.iter().any(|b| b.persona_id == FIRST) {
        assert!(exists);
        assert!(deleted.is_err());
    }
    if !exists {
        assert!(bindings.is_empty());
        assert!(bound.is_err());
    }
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn stale_unbinding_never_removes_a_reassigned_persona() {
    let db = test_support::test_db(3).await;
    db.create_ai_persona(FIRST, input("A")).await.unwrap();
    db.create_ai_persona(SECOND, input("B")).await.unwrap();
    let first = binding(FIRST);
    let key = first.key();
    db.upsert_ai_persona_binding(first).await.unwrap();
    db.upsert_ai_persona_binding(binding(SECOND)).await.unwrap();
    assert!(db.remove_ai_persona_binding_if(&key, FIRST).await.is_err());
    assert_eq!(
        db.resolve_ai_persona(&key).await.unwrap().unwrap().id,
        SECOND
    );
    db.remove_ai_persona_binding_if(&key, SECOND).await.unwrap();
    assert!(db.resolve_ai_persona(&key).await.unwrap().is_none());
    db.remove_ai_persona_binding_if(&key, SECOND).await.unwrap();
    db.shutdown().await.unwrap();
}
