use crate::{
    Database,
    error::{NuonoError, Result},
    plugin_kv::{PluginKVStore, RecordSet, state_value},
};
use nuono_app::plugin_data::*;
use sea_orm::{ConnectionTrait, Statement};
#[async_trait::async_trait]
impl PluginDataStore for Database {
    async fn is_administrator(
        &self,
        access: &nuono_app::plugin_identity::IdentityAccess,
    ) -> Result<bool> {
        let tx = self.begin_write().await?;
        match crate::plugin_group_access::require_administrator_on(&tx, access).await {
            Ok(()) => {
                tx.commit().await?;
                Ok(true)
            }
            Err(NuonoError::PermissionDenied(_)) => {
                tx.rollback().await?;
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    async fn state_record(
        &self,
        collection: &PluginStateCollection,
        key: &str,
    ) -> Result<Option<PluginStateRecord>> {
        validate_state_key(key)?;
        RecordSet::managed(self.orm(), collection)
            .find(key)
            .await?
            .as_ref()
            .map(crate::plugin_kv::record)
            .transpose()
    }
    async fn state_create(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        value_version: i32,
        value: serde_json::Value,
        expires_at_ms: Option<i64>,
    ) -> Result<Option<PluginStateRecord>> {
        validate_state_key(key)?;
        let tx = self.begin_write().await?;
        let result = RecordSet::managed(&tx, collection)
            .create(key, value_version, value, expires_at_ms)
            .await?;
        tx.commit().await?;
        Ok(result)
    }
    async fn state_compare_exchange(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        expected: &PluginStateStamp,
        value_version: i32,
        value: serde_json::Value,
        expires_at_ms: Option<i64>,
    ) -> Result<Option<PluginStateRecord>> {
        validate_state_key(key)?;
        let tx = self.begin_write().await?;
        let result = RecordSet::managed(&tx, collection)
            .compare_exchange(key, expected, value_version, value, expires_at_ms)
            .await?;
        tx.commit().await?;
        Ok(result)
    }
    async fn state_delete_if(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        expected: &PluginStateStamp,
    ) -> Result<bool> {
        validate_state_key(key)?;
        let tx = self.begin_write().await?;
        let result = RecordSet::managed(&tx, collection)
            .delete_if(key, expected)
            .await?;
        tx.commit().await?;
        Ok(result)
    }
    async fn state_prune_expired(
        &self,
        collection: &PluginStateCollection,
        limit: u32,
    ) -> Result<u64> {
        let tx = self.begin_write().await?;
        let result = RecordSet::managed(&tx, collection)
            .prune_expired(limit)
            .await?;
        tx.commit().await?;
        Ok(result)
    }

    async fn find_group(
        &self,
        conversation: &nuono_domain::ConversationRef,
    ) -> Result<Option<nuono_app::plugin_identity::CanonicalGroup>> {
        self.find_plugin_group(conversation).await
    }

    async fn resolve_identity(
        &self,
        identity: &nuono_domain::IdentityRef,
    ) -> Result<nuono_app::plugin_identity::IdentityAccess> {
        self.resolve_plugin_identity(identity).await
    }
    async fn identity_access_by_id(
        &self,
        id: i64,
    ) -> Result<nuono_app::plugin_identity::IdentityAccess> {
        self.plugin_identity_by_id(id).await
    }
    async fn resolve_group(
        &self,
        conversation: &nuono_domain::ConversationRef,
    ) -> Result<Option<nuono_app::plugin_identity::CanonicalGroup>> {
        self.resolve_plugin_group(conversation).await
    }
    async fn require_identity(
        &self,
        access: &nuono_app::plugin_identity::IdentityAccess,
    ) -> Result<()> {
        self.require_plugin_identity(access).await
    }

    async fn require_group(
        &self,
        group: &nuono_app::plugin_identity::CanonicalGroup,
    ) -> Result<()> {
        let tx = self.begin_write().await?;
        crate::plugin_identity::require_group_on(&tx, group).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn has_active_entitlement(
        &self,
        subject_type: &str,
        subject_id: i64,
        feature_key: &str,
    ) -> Result<bool> {
        Database::has_active_entitlement(self, subject_type, subject_id, feature_key).await
    }
    fn backend(&self) -> PluginDatabaseBackend {
        match Database::backend(self) {
            sea_orm::DbBackend::Postgres => PluginDatabaseBackend::Postgres,
            _ => PluginDatabaseBackend::Sqlite,
        }
    }
    async fn orm_execute(
        &self,
        statement: Statement,
    ) -> std::result::Result<sea_orm::ExecResult, sea_orm::DbErr> {
        self.orm().execute_raw(statement).await
    }
    async fn orm_query(
        &self,
        statement: Statement,
    ) -> std::result::Result<Vec<sea_orm::QueryResult>, sea_orm::DbErr> {
        self.orm().query_all_raw(statement).await
    }
    async fn orm_execute_unprepared(
        &self,
        sql: &str,
    ) -> std::result::Result<sea_orm::ExecResult, sea_orm::DbErr> {
        self.orm().execute_unprepared(sql).await
    }
    async fn begin(&self) -> Result<Box<dyn PluginDataTransaction>> {
        Ok(Box::new(PluginTransaction {
            transaction: self.begin_write().await?,
            backend: Database::backend(self),
            issuer_plugin_id: None,
            configuration_checks: Default::default(),
            administrator_checks: Default::default(),
        }))
    }
    async fn begin_for_plugin(&self, plugin_id: &str) -> Result<Box<dyn PluginDataTransaction>> {
        Ok(Box::new(PluginTransaction {
            transaction: self.begin_write().await?,
            backend: Database::backend(self),
            issuer_plugin_id: Some(plugin_id.to_owned()),
            configuration_checks: Default::default(),
            administrator_checks: Default::default(),
        }))
    }
    async fn get_secret(&self, key: &str) -> Result<Option<String>> {
        Database::get_secret(self, key).await
    }
    async fn set_secret(&self, key: &str, value: &str) -> Result<()> {
        Database::set_secret(self, key, value).await
    }
    async fn delete_secret(&self, key: &str) -> Result<()> {
        Database::delete_secret(self, key).await
    }
    async fn kv_get(&self, namespace: &str, key: &str) -> Result<Option<String>> {
        PluginKVStore::new(self.clone(), namespace).get(key).await
    }
    async fn kv_set(&self, namespace: &str, key: &str, value: &str) -> Result<()> {
        PluginKVStore::new(self.clone(), namespace)
            .set(key, value)
            .await
    }
    async fn kv_delete(&self, namespace: &str, key: &str) -> Result<bool> {
        PluginKVStore::new(self.clone(), namespace)
            .delete(key)
            .await
    }
    async fn kv_list(&self, namespace: &str) -> Result<Vec<(String, String)>> {
        PluginKVStore::new(self.clone(), namespace).list().await
    }
    async fn kv_clear(&self, namespace: &str) -> Result<u64> {
        PluginKVStore::new(self.clone(), namespace).clear().await
    }
    async fn state_get(
        &self,
        collection: &PluginStateCollection,
        key: &str,
    ) -> Result<Option<PluginStateValue>> {
        validate_state_key(key)?;
        RecordSet::managed(self.orm(), collection)
            .find(key)
            .await?
            .as_ref()
            .map(state_value)
            .transpose()
    }
    async fn state_set(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        value_version: i32,
        value_json: String,
    ) -> Result<()> {
        validate_state_key(key)?;
        validate_state_value(value_version, &value_json)?;
        let tx = self.begin_write().await?;
        RecordSet::managed(&tx, collection)
            .save(key, value_json, value_version)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    async fn state_delete(&self, collection: &PluginStateCollection, key: &str) -> Result<bool> {
        validate_state_key(key)?;
        let tx = self.begin_write().await?;
        let deleted = RecordSet::managed(&tx, collection).delete(key).await?;
        tx.commit().await?;
        Ok(deleted)
    }
    async fn state_list(
        &self,
        collection: &PluginStateCollection,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<(String, PluginStateValue)>> {
        if !(1..=100).contains(&limit) {
            return Err(NuonoError::InvalidInput(
                "插件状态每页条数须为 1–100".into(),
            ));
        }
        if let Some(key) = after {
            validate_state_key(key)?;
        }
        RecordSet::managed(self.orm(), collection)
            .list(after, Some(u64::from(limit)))
            .await?
            .into_iter()
            .map(|row| {
                let value = state_value(&row)?;
                Ok((row.record_key, value))
            })
            .collect()
    }
    async fn migrate(&self, namespace: &str, migrations: &[PluginMigration]) -> Result<()> {
        self.run_plugin_migrations(namespace, migrations).await
    }
    async fn migrate_backends(
        &self,
        namespace: &str,
        migrations: &[PluginMigrationForBackend],
    ) -> Result<()> {
        self.run_plugin_migrations_for_backend(namespace, migrations)
            .await
    }

    async fn execute(&self, sql: &str, values: Vec<PluginSqlValue>) -> Result<u64> {
        Ok(self
            .orm()
            .execute_raw(statement(Database::backend(self), sql, values))
            .await?
            .rows_affected())
    }
    async fn query_json(
        &self,
        sql: &str,
        values: Vec<PluginSqlValue>,
    ) -> Result<Vec<serde_json::Value>> {
        self.orm()
            .query_all_raw(statement(Database::backend(self), sql, values))
            .await?
            .into_iter()
            .map(|row| {
                row.try_get::<String>("", "data")
                    .map_err(Into::into)
                    .and_then(|value| serde_json::from_str(&value).map_err(Into::into))
            })
            .collect()
    }
    async fn query_rows(
        &self,
        sql: &str,
        values: Vec<PluginSqlValue>,
    ) -> Result<Vec<serde_json::Value>> {
        self.orm()
            .query_all_raw(statement(Database::backend(self), sql, values))
            .await?
            .into_iter()
            .map(|row| {
                <sea_orm::JsonValue as sea_orm::FromQueryResult>::from_query_result(&row, "")
                    .map_err(Into::into)
            })
            .collect()
    }
}

fn validate_state_value(value_version: i32, value_json: &str) -> Result<()> {
    if value_version <= 0 {
        return Err(NuonoError::InvalidInput(
            "插件状态的值版本必须为正整数".into(),
        ));
    }
    if value_json.len() > 64 * 1024 {
        return Err(NuonoError::InvalidInput(
            "插件状态 JSON 不得超过 64 KiB".into(),
        ));
    }
    // 写入前先用与读取侧完全相同的 JSON 解码器（含嵌套深度上限检查）做校验。
    nuono_domain::scalar::parse_json(value_json)?;
    Ok(())
}

fn validate_state_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > 200 || key.trim() != key || key.chars().any(char::is_control) {
        return Err(NuonoError::InvalidInput(
            "插件状态键须为 1–200 字节，且首尾无空白、全键无控制字符".into(),
        ));
    }
    Ok(())
}

struct PluginTransaction {
    transaction: sea_orm::DatabaseTransaction,
    backend: sea_orm::DbBackend,
    issuer_plugin_id: Option<String>,
    configuration_checks: tokio::sync::Mutex<
        Vec<(
            nuono_app::plugin_identity::IdentityAccess,
            nuono_app::plugin_identity::CanonicalGroup,
            Option<nuono_app::configuration::PlatformRoleProof>,
        )>,
    >,
    administrator_checks: tokio::sync::Mutex<Vec<nuono_app::plugin_identity::IdentityAccess>>,
}
impl PluginTransaction {
    fn require_collection(&self, collection: &PluginStateCollection) -> Result<()> {
        if self
            .issuer_plugin_id
            .as_deref()
            .is_some_and(|id| id != collection.plugin_id())
        {
            return Err(NuonoError::PermissionDenied(
                "状态集合不属于当前插件".into(),
            ));
        }
        Ok(())
    }

    fn grant_issuer(&self) -> Result<crate::group_authorizations::Issuer<'_>> {
        self.issuer_plugin_id
            .as_deref()
            .map(crate::group_authorizations::Issuer::Plugin)
            .ok_or_else(|| NuonoError::PermissionDenied("事务未绑定插件签发身份".into()))
    }
}
#[async_trait::async_trait]
impl PluginDataTransaction for PluginTransaction {
    async fn configuration_resource(
        &self,
        key: &str,
    ) -> Result<Option<nuono_app::configuration_resources::ConfigurationResource>> {
        let plugin = self
            .issuer_plugin_id
            .as_deref()
            .ok_or_else(|| NuonoError::PermissionDenied("事务未绑定插件配置身份".into()))?;
        crate::configuration_resources::read_on(&self.transaction, plugin, key).await
    }
    async fn save_configuration_resource(
        &self,
        write: &nuono_app::configuration_resources::ValidatedConfigurationResource,
    ) -> Result<nuono_app::configuration_resources::ConfigurationResource> {
        let plugin = self
            .issuer_plugin_id
            .as_deref()
            .ok_or_else(|| NuonoError::PermissionDenied("事务未绑定插件配置身份".into()))?;
        crate::configuration_resources::save_on(&self.transaction, plugin, write).await
    }

    async fn require_administrator(
        &self,
        access: &nuono_app::plugin_identity::IdentityAccess,
    ) -> Result<()> {
        crate::plugin_group_access::require_administrator_on(&self.transaction, access).await?;
        let mut checks = self.administrator_checks.lock().await;
        if !checks.contains(access) {
            checks.push(access.clone());
        }
        Ok(())
    }
    async fn prepare_group_configuration(
        &self,
        access: &nuono_app::plugin_identity::IdentityAccess,
        group: &nuono_app::plugin_identity::CanonicalGroup,
    ) -> Result<nuono_app::configuration::GroupConfigurationCandidates> {
        self.grant_issuer()?;
        crate::plugin_group_access::prepare_on(&self.transaction, access, group).await
    }
    async fn require_group_configuration(
        &self,
        access: &nuono_app::plugin_identity::IdentityAccess,
        group: &nuono_app::plugin_identity::CanonicalGroup,
        proof: Option<nuono_app::configuration::PlatformRoleProof>,
    ) -> Result<()> {
        let plugin = self
            .issuer_plugin_id
            .as_deref()
            .ok_or_else(|| NuonoError::PermissionDenied("事务未绑定插件".into()))?;
        crate::plugin_group_access::require_on(
            &self.transaction,
            plugin,
            access,
            group,
            proof.as_ref(),
        )
        .await?;
        let mut checks = self.configuration_checks.lock().await;
        checks.retain(|(a, g, _)| a.identity_id != access.identity_id || g.id != group.id);
        checks.push((access.clone(), group.clone(), proof));
        Ok(())
    }

    async fn state_record(
        &self,
        collection: &PluginStateCollection,
        key: &str,
    ) -> Result<Option<PluginStateRecord>> {
        validate_state_key(key)?;
        self.require_collection(collection)?;
        RecordSet::managed(&self.transaction, collection)
            .find(key)
            .await?
            .as_ref()
            .map(crate::plugin_kv::record)
            .transpose()
    }
    async fn state_create(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        value_version: i32,
        value: serde_json::Value,
        expires_at_ms: Option<i64>,
    ) -> Result<Option<PluginStateRecord>> {
        validate_state_key(key)?;
        self.require_collection(collection)?;
        RecordSet::managed(&self.transaction, collection)
            .create(key, value_version, value, expires_at_ms)
            .await
    }
    async fn state_compare_exchange(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        expected: &PluginStateStamp,
        value_version: i32,
        value: serde_json::Value,
        expires_at_ms: Option<i64>,
    ) -> Result<Option<PluginStateRecord>> {
        validate_state_key(key)?;
        self.require_collection(collection)?;
        RecordSet::managed(&self.transaction, collection)
            .compare_exchange(key, expected, value_version, value, expires_at_ms)
            .await
    }
    async fn state_delete_if(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        expected: &PluginStateStamp,
    ) -> Result<bool> {
        validate_state_key(key)?;
        self.require_collection(collection)?;
        RecordSet::managed(&self.transaction, collection)
            .delete_if(key, expected)
            .await
    }
    async fn state_prune_expired(
        &self,
        collection: &PluginStateCollection,
        limit: u32,
    ) -> Result<u64> {
        self.require_collection(collection)?;
        RecordSet::managed(&self.transaction, collection)
            .prune_expired(limit)
            .await
    }

    async fn identity_access_by_id(
        &self,
        id: i64,
    ) -> Result<nuono_app::plugin_identity::IdentityAccess> {
        crate::plugin_identity::identity_by_id_on(&self.transaction, id).await
    }

    async fn require_identity(
        &self,
        access: &nuono_app::plugin_identity::IdentityAccess,
    ) -> Result<()> {
        crate::plugin_identity::require_identity_on(&self.transaction, access).await
    }
    async fn require_group(
        &self,
        group: &nuono_app::plugin_identity::CanonicalGroup,
    ) -> Result<()> {
        crate::plugin_identity::require_group_on(&self.transaction, group).await
    }

    async fn lock_accounts(&self, ids: &[i64]) -> Result<()> {
        crate::group_authorizations::lock_accounts(&self.transaction, ids).await
    }
    async fn group_grants(
        &self,
        group_id: i64,
    ) -> Result<Vec<nuono_app::group_authorization::GroupAuthorization>> {
        self.grant_issuer()?;
        crate::group_authorizations::plugin_sources_on(
            &self.transaction,
            self.issuer_plugin_id.as_deref().unwrap_or_default(),
            group_id,
        )
        .await
    }
    async fn issue_group_grant(
        &self,
        request: &nuono_app::group_authorization::GroupGrantRequest,
    ) -> Result<nuono_app::group_authorization::GroupAuthorization> {
        crate::group_authorizations::issue_on(&self.transaction, self.grant_issuer()?, request)
            .await
    }
    async fn renew_group_grant(
        &self,
        id: i64,
        revision: i64,
        expires_at_ms: Option<i64>,
    ) -> Result<nuono_app::group_authorization::GroupAuthorization> {
        crate::group_authorizations::change_on(
            &self.transaction,
            self.grant_issuer()?,
            id,
            Some(revision),
            crate::group_authorizations::Change::Renew(expires_at_ms),
        )
        .await
    }
    async fn revoke_group_grant(
        &self,
        id: i64,
        revision: i64,
    ) -> Result<nuono_app::group_authorization::GroupAuthorization> {
        crate::group_authorizations::change_on(
            &self.transaction,
            self.grant_issuer()?,
            id,
            Some(revision),
            crate::group_authorizations::Change::Revoke,
        )
        .await
    }
    async fn state_get(
        &self,
        collection: &PluginStateCollection,
        key: &str,
    ) -> Result<Option<PluginStateValue>> {
        validate_state_key(key)?;
        RecordSet::managed(&self.transaction, collection)
            .find(key)
            .await?
            .as_ref()
            .map(state_value)
            .transpose()
    }
    async fn state_set(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        value_version: i32,
        value_json: String,
    ) -> Result<()> {
        validate_state_key(key)?;
        validate_state_value(value_version, &value_json)?;
        RecordSet::managed(&self.transaction, collection)
            .save(key, value_json, value_version)
            .await
    }

    async fn assistant_grant(
        &self,
        user_id: i64,
    ) -> Result<Option<nuono_app::assistant::AssistantGrant>> {
        crate::db::assistant::grant_on(&self.transaction, self.backend, user_id).await
    }
    async fn set_assistant_grant(
        &self,
        user_id: i64,
        grant: Option<nuono_app::assistant::AssistantGrant>,
    ) -> Result<()> {
        crate::db::assistant::set_grant_on(&self.transaction, self.backend, user_id, grant).await
    }

    async fn upsert_entitlement(
        &self,
        input: &nuono_app::entitlement::EntitlementUpsert,
    ) -> Result<nuono_app::entitlement::EntitlementRecord> {
        self.grant_issuer()?;
        crate::entitlements::upsert_entitlement_on(&self.transaction, input).await
    }

    async fn revoke_entitlement(
        &self,
        subject_type: &str,
        subject_id: i64,
        feature_key: &str,
        source: &str,
    ) -> Result<bool> {
        self.grant_issuer()?;
        crate::entitlements::revoke_entitlement_on(
            &self.transaction,
            subject_type,
            subject_id,
            feature_key,
            source,
        )
        .await
    }

    fn backend(&self) -> PluginDatabaseBackend {
        if self.backend == sea_orm::DbBackend::Postgres {
            PluginDatabaseBackend::Postgres
        } else {
            PluginDatabaseBackend::Sqlite
        }
    }
    async fn get_secret(&self, key: &str) -> Result<Option<String>> {
        use crate::entity::runtime_secrets;
        use sea_orm::EntityTrait;
        let value = runtime_secrets::Entity::find_by_id(key)
            .one(&self.transaction)
            .await?
            .map(|row| crate::db::secrets::decode_secret(row.value))
            .transpose()?;
        Ok(value.filter(|value| !value.trim().is_empty()))
    }
    async fn set_secret(&self, key: &str, value: &str) -> Result<()> {
        crate::db::set_secret_on(&self.transaction, key, value).await
    }
    async fn orm_execute(
        &self,
        statement: Statement,
    ) -> std::result::Result<sea_orm::ExecResult, sea_orm::DbErr> {
        self.transaction.execute_raw(statement).await
    }
    async fn orm_query(
        &self,
        statement: Statement,
    ) -> std::result::Result<Vec<sea_orm::QueryResult>, sea_orm::DbErr> {
        self.transaction.query_all_raw(statement).await
    }
    async fn orm_execute_unprepared(
        &self,
        sql: &str,
    ) -> std::result::Result<sea_orm::ExecResult, sea_orm::DbErr> {
        self.transaction.execute_unprepared(sql).await
    }
    async fn execute(&self, sql: &str, values: Vec<PluginSqlValue>) -> Result<u64> {
        Ok(self
            .transaction
            .execute_raw(statement(self.backend, sql, values))
            .await?
            .rows_affected())
    }
    async fn query_json(
        &self,
        sql: &str,
        values: Vec<PluginSqlValue>,
    ) -> Result<Vec<serde_json::Value>> {
        self.transaction
            .query_all_raw(statement(self.backend, sql, values))
            .await?
            .into_iter()
            .map(|row| {
                row.try_get::<String>("", "data")
                    .map_err(Into::into)
                    .and_then(|value| serde_json::from_str(&value).map_err(Into::into))
            })
            .collect()
    }
    async fn query_rows(
        &self,
        sql: &str,
        values: Vec<PluginSqlValue>,
    ) -> Result<Vec<serde_json::Value>> {
        self.transaction
            .query_all_raw(statement(self.backend, sql, values))
            .await?
            .into_iter()
            .map(|row| {
                <sea_orm::JsonValue as sea_orm::FromQueryResult>::from_query_result(&row, "")
                    .map_err(Into::into)
            })
            .collect()
    }
    async fn commit(self: Box<Self>) -> Result<()> {
        for access in self.administrator_checks.into_inner() {
            crate::plugin_group_access::require_administrator_on(&self.transaction, &access)
                .await?;
        }
        for (access, group, proof) in self.configuration_checks.into_inner() {
            crate::plugin_group_access::require_on(
                &self.transaction,
                self.issuer_plugin_id.as_deref().unwrap_or_default(),
                &access,
                &group,
                proof.as_ref(),
            )
            .await?;
        }
        self.transaction.commit().await.map_err(Into::into)
    }
    async fn rollback(self: Box<Self>) -> Result<()> {
        self.transaction.rollback().await.map_err(Into::into)
    }
}
fn statement(backend: sea_orm::DbBackend, sql: &str, values: Vec<PluginSqlValue>) -> Statement {
    let plugin_backend = match backend {
        sea_orm::DbBackend::Postgres => PluginDatabaseBackend::Postgres,
        _ => PluginDatabaseBackend::Sqlite,
    };
    let rewritten = rewrite_sql_placeholders(sql, plugin_backend);
    let values: Vec<sea_orm::Value> = values
        .into_iter()
        .map(|v| match v {
            PluginSqlValue::Null => sea_orm::Value::String(None),
            PluginSqlValue::NullBool => sea_orm::Value::Bool(None),
            PluginSqlValue::NullInteger => sea_orm::Value::BigInt(None),
            PluginSqlValue::NullReal => sea_orm::Value::Double(None),
            PluginSqlValue::NullBytes => sea_orm::Value::Bytes(None),
            PluginSqlValue::Bool(v) => v.into(),
            PluginSqlValue::Integer(v) => v.into(),
            PluginSqlValue::Real(v) => v.into(),
            PluginSqlValue::Text(v) => v.into(),
            PluginSqlValue::Bytes(v) => v.into(),
            _ => sea_orm::Value::String(None),
        })
        .collect();
    Statement::from_sql_and_values(backend, rewritten.as_ref(), values)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn plugin_transactions_commit_explicitly_and_roll_back_on_drop() {
        let db = crate::test_support::test_db(2).await;
        PluginDataStore::execute(
            &db,
            "CREATE TABLE plugin_tx_fixture (id BIGINT PRIMARY KEY)",
            vec![],
        )
        .await
        .unwrap();
        let pg = Database::backend(&db) == sea_orm::DbBackend::Postgres;
        let insert = if pg {
            "INSERT INTO plugin_tx_fixture VALUES ($1)"
        } else {
            "INSERT INTO plugin_tx_fixture VALUES (?)"
        };
        let count = if pg {
            "SELECT json_build_object('count',COUNT(*))::text AS data FROM plugin_tx_fixture"
        } else {
            "SELECT json_object('count',COUNT(*)) AS data FROM plugin_tx_fixture"
        };
        let tx = PluginDataStore::begin(&db).await.unwrap();
        tx.execute(insert, vec![PluginSqlValue::Integer(1)])
            .await
            .unwrap();
        assert_eq!(tx.query_json(count, vec![]).await.unwrap()[0]["count"], 1);
        tx.commit().await.unwrap();
        let tx = PluginDataStore::begin(&db).await.unwrap();
        tx.execute(insert, vec![PluginSqlValue::Integer(2)])
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        let tx = PluginDataStore::begin(&db).await.unwrap();
        tx.execute(insert, vec![PluginSqlValue::Integer(3)])
            .await
            .unwrap();
        drop(tx);
        assert_eq!(
            PluginDataStore::query_json(&db, count, vec![])
                .await
                .unwrap()[0]["count"],
            1
        );
        let tx = PluginDataStore::begin(&db).await.unwrap();
        tx.execute(insert, vec![PluginSqlValue::Integer(4)])
            .await
            .unwrap();
        assert!(
            tx.execute(insert, vec![PluginSqlValue::Integer(1)])
                .await
                .is_err()
        );
        tx.rollback().await.unwrap();
        assert_eq!(
            PluginDataStore::query_json(&db, count, vec![])
                .await
                .unwrap()[0]["count"],
            1
        );
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn plugin_sql_port_preserves_bound_values_on_both_backends() {
        let db = crate::test_support::test_db(2).await;
        db.run_plugin_migrations_for_backend("port-test", &[PluginMigrationForBackend {
            version: 1, description: "plugin port fixture".into(),
            pg_sql: "CREATE TABLE port_test_fixture (id BIGINT PRIMARY KEY, content TEXT NOT NULL)".into(),
            sqlite_sql: "CREATE TABLE port_test_fixture (id INTEGER PRIMARY KEY, content TEXT NOT NULL) STRICT".into(),
        }]).await.unwrap();
        let content = "bound ' value 中文";
        let pg = Database::backend(&db) == sea_orm::DbBackend::Postgres;
        let sql = if pg {
            "INSERT INTO port_test_fixture (id,content) VALUES ($1,$2)"
        } else {
            "INSERT INTO port_test_fixture (id,content) VALUES (?,?)"
        };
        assert_eq!(
            PluginDataStore::execute(
                &db,
                sql,
                vec![
                    PluginSqlValue::Integer(1),
                    PluginSqlValue::Text(content.into())
                ]
            )
            .await
            .unwrap(),
            1
        );
        let sql = if pg {
            "SELECT json_build_object('id',id,'content',content)::text AS data FROM port_test_fixture WHERE id=$1"
        } else {
            "SELECT json_object('id',id,'content',content) AS data FROM port_test_fixture WHERE id=?"
        };
        assert_eq!(
            PluginDataStore::query_json(&db, sql, vec![PluginSqlValue::Integer(1)])
                .await
                .unwrap(),
            vec![serde_json::json!({"id":1,"content":content})]
        );
        // 验证当前数据库后端上方言无关的 `$1` 占位符自动改写以及 `query_rows` 查询结果。
        let rows = PluginDataStore::query_rows(
            &db,
            "SELECT id, content FROM port_test_fixture WHERE id = $1",
            vec![PluginSqlValue::Integer(1)],
        )
        .await
        .unwrap();
        assert_eq!(rows, vec![serde_json::json!({"id": 1, "content": content})]);
        db.shutdown().await.unwrap();
    }
}
