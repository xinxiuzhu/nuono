//! 验证插件私有密钥读取会复用当前业务事务的宿主运行时与数据库连接。
use std::time::Duration;

use nuono_plugin::{
    Database, PluginSecretStore,
    orm::{ConnectionTrait, Statement},
};

#[test]
fn private_secret_transaction_reads_preserve_pending_writes_namespace_and_rollback() {
    let host_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("secret-transaction-host")
        .enable_all()
        .build()
        .unwrap();
    let caller_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (host, db, secrets, other) = host_runtime.block_on(async {
        // 连接池大小设为 1，这样一旦密钥读取脱离当前事务去抢新连接就会立刻阻塞暴露问题。
        let host = nuono_core::test_support::test_db(1).await;
        for plugin in ["secret-tx-fixture", "other-fixture"] {
            host.sync_plugin(plugin, "1", &serde_json::json!({}))
                .await
                .unwrap();
        }
        let db = Database::new(host.clone());
        db.execute_unprepared("CREATE TABLE private_secret_tx_fixture (id BIGINT PRIMARY KEY)")
            .await
            .unwrap();
        let secrets = PluginSecretStore::new(host.clone(), "secret-tx-fixture", vec![]);
        let other = PluginSecretStore::new(host.clone(), "other-fixture", vec![]);
        (host, db, secrets, other)
    });
    caller_runtime.block_on(async {
        let tx = db.begin().await.unwrap();
        assert!(
            secrets
                .get_private_in_transaction(&tx, "source")
                .await
                .unwrap()
                .is_none()
        );
        tx.execute_unprepared("INSERT INTO private_secret_tx_fixture VALUES (1)")
            .await
            .unwrap();
        secrets
            .set_private_in_transaction(&tx, "source", "synthetic-value")
            .await
            .unwrap();
        let value = tokio::time::timeout(
            Duration::from_secs(2),
            secrets.get_private_in_transaction(&tx, "source"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            value.as_deref() == Some("synthetic-value"),
            "transaction must read its pending write"
        );
        secrets
            .set_private_in_transaction(&tx, "copy", value.as_deref().unwrap())
            .await
            .unwrap();
        assert!(
            other
                .get_private_in_transaction(&tx, "source")
                .await
                .unwrap()
                .is_none()
        );
        for key in ["", "../source", "plugin.other-fixture.source"] {
            assert!(secrets.get_private_in_transaction(&tx, key).await.is_err());
        }
        tx.commit().await.unwrap();
        assert!(secrets.get_private("source").await.unwrap().as_deref() == Some("synthetic-value"));
        assert!(secrets.get_private("copy").await.unwrap().as_deref() == Some("synthetic-value"));

        let tx = db.begin().await.unwrap();
        tx.execute_unprepared("INSERT INTO private_secret_tx_fixture VALUES (2)")
            .await
            .unwrap();
        secrets
            .set_private_in_transaction(&tx, "source", "synthetic-replacement")
            .await
            .unwrap();
        assert!(
            secrets
                .get_private_in_transaction(&tx, "source")
                .await
                .unwrap()
                .as_deref()
                == Some("synthetic-replacement")
        );
        secrets
            .set_private_in_transaction(&tx, "copy", " ")
            .await
            .unwrap();
        assert!(
            secrets
                .get_private_in_transaction(&tx, "copy")
                .await
                .unwrap()
                .is_none()
        );
        tx.rollback().await.unwrap();
        assert!(secrets.get_private("source").await.unwrap().as_deref() == Some("synthetic-value"));
        assert!(secrets.get_private("copy").await.unwrap().as_deref() == Some("synthetic-value"));

        let tx = db.begin().await.unwrap();
        tx.execute_unprepared("INSERT INTO private_secret_tx_fixture VALUES (3)")
            .await
            .unwrap();
        secrets
            .set_private_in_transaction(&tx, "source", "synthetic-dropped")
            .await
            .unwrap();
        assert!(
            secrets
                .get_private_in_transaction(&tx, "source")
                .await
                .unwrap()
                .as_deref()
                == Some("synthetic-dropped")
        );
        drop(tx);
        let value = tokio::time::timeout(Duration::from_secs(2), secrets.get_private("source"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            value.as_deref() == Some("synthetic-value"),
            "dropped transaction must roll back"
        );
        let row = db
            .query_one_raw(Statement::from_string(
                db.get_database_backend(),
                "SELECT COUNT(*) AS n FROM private_secret_tx_fixture".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<i64>("", "n").unwrap(), 1);
    });
    host_runtime.block_on(async {
        host.shutdown().await.unwrap();
    });
}
