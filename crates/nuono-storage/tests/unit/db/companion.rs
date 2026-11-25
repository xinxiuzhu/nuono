use super::*;

async fn account(db: &Database, name: &str) -> i64 {
    db.create_user(crate::db::NewUser {
        username: name.into(),
        email: None,
        password: "test-password-with-length".into(),
        role: "user".into(),
        status: "active".into(),
        email_verified: true,
    })
    .await
    .unwrap()
    .id
}

#[tokio::test]
async fn identities_are_private_and_switching_keeps_the_original_card() {
    let db = crate::test_support::test_db(2).await;
    let alice = account(&db, "companion-alice").await;
    let bob = account(&db, "companion-bob").await;
    let first = db.ensure_default_companion_card(alice).await.unwrap();
    let same = db.ensure_default_companion_card(alice).await.unwrap();
    assert_eq!(first.id, same.id);
    let second = db
        .create_companion_card(
            alice,
            CompanionInput {
                name: "另一位伙伴".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        db.active_companion_card(alice).await.unwrap().unwrap().id,
        first.id
    );
    assert!(db.companion_card(bob, &first.id).await.is_err());
    assert!(db.activate_companion_card(bob, &first.id).await.is_err());
    assert!(
        db.update_companion_card(bob, &first.id, first.revision, CompanionInput::default())
            .await
            .is_err()
    );
    assert!(
        db.set_companion_asset(
            bob,
            &first.id,
            PersonaAssetKind::Avatar,
            None,
            first.revision
        )
        .await
        .is_err()
    );
    assert!(db.list_companion_cards(bob).await.unwrap().is_empty());
    db.activate_companion_card(alice, &second.id).await.unwrap();
    assert_eq!(
        db.active_companion_card(alice).await.unwrap().unwrap().id,
        second.id
    );
    db.activate_companion_card(alice, &first.id).await.unwrap();
    assert_eq!(
        db.active_companion_card(alice).await.unwrap().unwrap(),
        first
    );
    assert_eq!(db.list_companion_cards(alice).await.unwrap().len(), 2);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn settings_and_assets_share_revision_without_changing_identity() {
    let db = crate::test_support::test_db(2).await;
    let user = account(&db, "companion-edit").await;
    let original = db.ensure_default_companion_card(user).await.unwrap();
    let filename = format!("{}.png", uuid::Uuid::new_v4());
    let with_asset = db
        .set_companion_asset(
            user,
            &original.id,
            PersonaAssetKind::Avatar,
            Some(filename.clone()),
            original.revision,
        )
        .await
        .unwrap();
    assert!(
        db.update_companion_card(
            user,
            &original.id,
            original.revision,
            CompanionInput::default()
        )
        .await
        .is_err()
    );
    let edited = db
        .update_companion_card(
            user,
            &original.id,
            with_asset.revision,
            CompanionInput {
                name: "新名字".into(),
                user_address: "朋友".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(edited.id, original.id);
    assert_eq!(edited.avatar_filename, Some(filename));
    assert!(edited.prompt().contains("朋友"));
    assert_eq!(edited.revision, original.revision + 2);
    assert_eq!(
        db.active_companion_card(user).await.unwrap().unwrap().id,
        original.id
    );
    assert!(
        db.set_companion_asset(
            user,
            &original.id,
            PersonaAssetKind::Avatar,
            Some("../escape.png".into()),
            edited.revision
        )
        .await
        .is_err()
    );
    db.shutdown().await.unwrap();
}
