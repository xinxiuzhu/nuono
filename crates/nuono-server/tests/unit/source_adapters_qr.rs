use super::*;
use crate::bot_instances::BotInstanceRuntimeManager;

#[tokio::test]
async fn qr_save_uses_verified_secrets_and_retries_return_one_instance() {
    let fixture = crate::api::contract_tests::Fixture::new(false).await;
    let mut manager = BotInstanceRuntimeManager::new(
        fixture.state.db.clone(),
        Arc::new(AdapterManager::new()),
        fixture.state.manager.event_kernel.clone(),
    );
    let provider = crate::bot_qr_login::tests::provider();
    let mut registry = nuono_adapter::AdapterRegistry::default();
    registry.register(provider.clone()).unwrap();
    manager.source.registry = Arc::new(registry);
    let manager = Arc::new(manager);
    let login = manager.qr_logins.start(7, provider).await.unwrap();
    let request = CreateBotInstanceRequest {
        name: "QR instance".into(),
        adapter_type: "qr_fixture".into(),
        enabled: false,
        config: json!({"account":"browser-account","token":"browser-token"}),
        ..Default::default()
    };
    assert!(
        manager
            .qr_logins
            .complete(7, &login.id, request.clone(), manager.clone())
            .await
            .is_err()
    );
    manager.qr_logins.poll(7, &login.id).await.unwrap();
    assert!(
        manager
            .qr_logins
            .complete(8, &login.id, request.clone(), manager.clone())
            .await
            .is_err()
    );
    let mut wrong = request.clone();
    wrong.adapter_type = "another_adapter".into();
    assert!(
        manager
            .qr_logins
            .complete(7, &login.id, wrong, manager.clone())
            .await
            .is_err()
    );
    let first = manager
        .qr_logins
        .complete(7, &login.id, request.clone(), manager.clone())
        .await
        .unwrap();
    let again = manager
        .qr_logins
        .complete(7, &login.id, request, manager.clone())
        .await
        .unwrap();
    assert_eq!(first.id, again.id);
    assert_eq!(first.config["account"], "fixture-account");
    assert!(
        !serde_json::to_string(&first)
            .unwrap()
            .contains("synthetic-qr-secret")
    );
    let record = bot_instances::get(&fixture.state.db, &first.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.secrets["token"], "synthetic-qr-secret");
    assert_eq!(manager.list().await.unwrap().len(), 1);
    manager.qr_logins.cancel(7, &login.id).await.unwrap();
    assert!(
        manager.get(&first.id).await.unwrap().is_some(),
        "Closing a saved QR session never deletes its instance"
    );

    // 未填写实例名称时，扫码完成应自动回填平台官方昵称。
    let login_auto = manager
        .qr_logins
        .start(7, crate::bot_qr_login::tests::provider())
        .await
        .unwrap();
    manager.qr_logins.poll(7, &login_auto.id).await.unwrap();
    let auto_named = manager
        .qr_logins
        .complete(
            7,
            &login_auto.id,
            CreateBotInstanceRequest {
                name: "   ".into(),
                adapter_type: "qr_fixture".into(),
                enabled: false,
                config: json!({}),
                ..Default::default()
            },
            manager.clone(),
        )
        .await
        .unwrap();
    assert_eq!(auto_named.name, "小小");
    manager.shutdown_all().await;
}
