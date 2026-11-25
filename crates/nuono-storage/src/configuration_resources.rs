//! 插件结构化配置资源与业务表数据在同一数据库事务内原子提交。
use crate::{
    entity::plugins,
    error::{NuonoError, Result},
};
use nuono_app::configuration_resources::{ConfigurationResource, ValidatedConfigurationResource};
use sea_orm::{ConnectionTrait, DatabaseTransaction, DbBackend, EntityTrait, QuerySelect, Set};
async fn locked(tx: &DatabaseTransaction, plugin: &str) -> Result<plugins::Model> {
    let mut query = plugins::Entity::find_by_id(plugin);
    if tx.get_database_backend() == DbBackend::Postgres {
        query = query.lock_exclusive();
    }
    query
        .one(tx)
        .await?
        .ok_or_else(|| NuonoError::PluginNotFound(plugin.into()))
}
fn decode(
    values: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Option<ConfigurationResource>> {
    values
        .get(key)
        .map(|raw| {
            let text = raw.as_str().ok_or_else(|| {
                NuonoError::ConfigValidation("结构配置必须使用 JSON 文本字段".into())
            })?;
            Ok(serde_json::from_str(text)?)
        })
        .transpose()
}
pub(crate) async fn read_on(
    tx: &DatabaseTransaction,
    plugin: &str,
    key: &str,
) -> Result<Option<ConfigurationResource>> {
    let row = locked(tx, plugin).await?;
    let values = serde_json::from_str(&row.config_json)?;
    decode(&values, key)
}
pub(crate) async fn save_on(
    tx: &DatabaseTransaction,
    plugin: &str,
    write: &ValidatedConfigurationResource,
) -> Result<ConfigurationResource> {
    if plugin != write.plugin_id() {
        return Err(NuonoError::PermissionDenied(
            "不能写入其他插件的配置".into(),
        ));
    }
    let row = locked(tx, plugin).await?;
    let mut values: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&row.config_json)?;
    let current = decode(&values, write.key())?.unwrap_or(write.initial()?);
    write.validate_current(&current)?;
    if current.revision != write.expected_revision() {
        return Err(NuonoError::Conflict("配置资源已变化，请重新读取".into()));
    }
    let next = write.next()?;
    values.insert(
        write.key().into(),
        serde_json::Value::String(serde_json::to_string(&next)?),
    );
    let revision = row
        .revision
        .checked_add(1)
        .ok_or_else(|| NuonoError::Conflict("配置版本已耗尽".into()))?;
    let mut row: plugins::ActiveModel = row.into();
    row.config_json = Set(serde_json::to_string(&values)?);
    row.revision = Set(revision);
    row.updated_at_ms = Set(chrono::Utc::now().timestamp_millis());
    crate::plugins::save_configuration_on(tx, row).await?;
    crate::db::append_audit_on(tx,crate::db::AuditEvent{actor_user_id:None,actor_plugin_id:Some(plugin),action:"plugin.configuration.resource.update",result:"success",subject_user_id:None,group_id:None,operation_id:None,details:serde_json::json!({"resource":write.key(),"resource_revision":next.revision.to_string(),"revision":revision.to_string()})}).await?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuono_app::configuration_resources::ConfigurationResourceDefinition;
    fn definition(key: &str) -> ConfigurationResourceDefinition {
        ConfigurationResourceDefinition {
            key: key.into(),
            label: key.into(),
            description: String::new(),
            version: 1,
            default: serde_json::json!([]),
            max_bytes: 4096,
            validator: |v| {
                if v.as_array()
                    .is_some_and(|items| items.iter().all(serde_json::Value::is_string))
                {
                    Ok(())
                } else {
                    Err("expected string array".into())
                }
            },
        }
    }
    #[tokio::test]
    async fn resources_keep_independent_revisions_and_rollback_with_business_transaction() {
        let db = crate::test_support::test_db(4).await;
        db.sync_plugin("resource_probe", "1", &serde_json::json!({}))
            .await
            .unwrap();
        let a = definition("aliases");
        let b = definition("scoring");
        let tx = db.begin_write().await.unwrap();
        assert!(
            read_on(&tx, "resource_probe", "aliases")
                .await
                .unwrap()
                .is_none()
        );
        let first = save_on(
            &tx,
            "resource_probe",
            &a.validated_write("resource_probe", 0, serde_json::json!(["one"]))
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(first.revision, 1);
        tx.rollback().await.unwrap();
        let tx = db.begin_write().await.unwrap();
        assert!(
            read_on(&tx, "resource_probe", "aliases")
                .await
                .unwrap()
                .is_none()
        );
        save_on(
            &tx,
            "resource_probe",
            &a.validated_write("resource_probe", 0, serde_json::json!(["one"]))
                .unwrap(),
        )
        .await
        .unwrap();
        save_on(
            &tx,
            "resource_probe",
            &b.validated_write("resource_probe", 0, serde_json::json!(["rules"]))
                .unwrap(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let tx = db.begin_write().await.unwrap();
        assert!(
            save_on(
                &tx,
                "resource_probe",
                &a.validated_write("resource_probe", 0, serde_json::json!(["stale"]))
                    .unwrap()
            )
            .await
            .is_err()
        );
        assert!(
            save_on(
                &tx,
                "another_plugin",
                &a.validated_write("resource_probe", 1, serde_json::json!([]))
                    .unwrap()
            )
            .await
            .is_err()
        );
        assert_eq!(
            read_on(&tx, "resource_probe", "aliases")
                .await
                .unwrap()
                .unwrap()
                .value,
            serde_json::json!(["one"])
        );
        assert_eq!(
            read_on(&tx, "resource_probe", "scoring")
                .await
                .unwrap()
                .unwrap()
                .revision,
            1
        );
        tx.rollback().await.unwrap();
        assert!(
            a.validated_write("resource_probe", 1, serde_json::json!([3]))
                .is_err()
        );
        let saved = db.plugin_configuration("resource_probe").await.unwrap();
        assert_eq!(saved.revision, 3);
    }
    #[tokio::test]
    async fn concurrent_resource_cas_has_exactly_one_winner() {
        let db = crate::test_support::test_db(4).await;
        db.sync_plugin("resource_probe", "1", &serde_json::json!({}))
            .await
            .unwrap();
        let mut workers = Vec::new();
        for value in ["first", "second"] {
            let db = db.clone();
            workers.push(tokio::spawn(async move {
                let tx = db.begin_write().await.unwrap();
                let result = save_on(
                    &tx,
                    "resource_probe",
                    &definition("aliases")
                        .validated_write("resource_probe", 0, serde_json::json!([value]))
                        .unwrap(),
                )
                .await;
                if result.is_ok() {
                    tx.commit().await.unwrap();
                    true
                } else {
                    tx.rollback().await.unwrap();
                    false
                }
            }));
        }
        let mut winners = 0;
        for task in workers {
            winners += usize::from(task.await.unwrap());
        }
        assert_eq!(winners, 1);
    }
}
