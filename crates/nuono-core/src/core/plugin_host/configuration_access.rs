//! 已安装插件的配置访问控制：整合完整平台原生身份、宿主临时授权令牌与带版本条件写入（CAS）。
use crate::{
    config::PluginConfigManager,
    db::Database,
    error::{NuonoError, Result},
};
use nuono_app::{
    configuration::*,
    plugin_identity::{CanonicalGroup, IdentityAccess},
};
use nuono_storage::plugin_overrides::OverrideTarget;
use std::sync::Arc;
use tokio::sync::RwLock;

pub(super) async fn actor_for_group(
    db: &Database,
    authority: &PlatformRoleAuthority,
    verifiers: &RwLock<Vec<Arc<dyn PlatformRoleVerifier>>>,
    caller: &nuono_app::auth::CallerIdentity,
    group_id: i64,
) -> Result<ConfigurationActor> {
    use nuono_app::configuration::*;
    use sea_orm::{ConnectionTrait, Statement};
    let actor = ConfigurationActor::from_caller(caller);
    if matches!(actor.mode, ConfigurationMode::Administrator) {
        return Ok(actor);
    }
    let enabled = db
        .get_global_config("configuration.platform_roles_enabled")
        .await?
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    authority.set_enabled(enabled);
    if !enabled {
        return Ok(actor);
    }
    let group = db
        .group_by_id(group_id)
        .await?
        .ok_or_else(|| crate::error::NuonoError::NotFound("群不存在".into()))?;
    let rows=db.orm().query_all_raw(Statement::from_sql_and_values(db.backend(),
            "SELECT id,revision,native_user_id,identity_kind FROM nuono_platform_identities WHERE user_id=$1 AND platform=$2 AND namespace=$3 AND verified_at_ms IS NOT NULL ORDER BY id",
            [caller.user_id.into(),group.platform.clone().into(),group.namespace.clone().into()])).await?;
    let providers = verifiers.read().await.clone();
    for row in rows {
        let identity_kind: String = row.try_get("", "identity_kind")?;
        let scheme: serde_json::Value = nuono_domain::scalar::parse_json(&identity_kind)?;
        let scheme = scheme
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("json:{scheme}"));
        if !group.conversation_kind.ends_with(&format!(":{scheme}")) {
            continue;
        }
        let subject = PlatformRoleSubject {
            identity_id: row.try_get("", "id")?,
            identity_revision: row.try_get("", "revision")?,
            group_id,
            platform: group.platform.clone(),
            namespace: group.namespace.clone(),
            native_user_id: row.try_get("", "native_user_id")?,
            conversation_kind: group.conversation_kind.clone(),
            native_parent_id: group.native_parent_id.clone(),
            native_group_id: group.native_group_id.clone(),
        };
        for provider in &providers {
            if let Ok(proof) = authority.verify(provider.as_ref(), subject.clone()).await {
                return Ok(ConfigurationActor {
                    user_id: caller.user_id,
                    mode: ConfigurationMode::VerifiedPlatformRole(Box::new(proof)),
                });
            }
        }
    }
    Ok(actor)
}

/// 批量查询调用者在给定候选群中具备已验证平台管理员身份的群 ID 集合。
///
/// 相比逐群调用 [`actor_for_group`]，本函数将全局开关、调用者的已验证平台身份与
/// verifier 列表各只读取一次，并跳过平台/命名空间不匹配的候选群，彻底消除列表页 N+1 查询。
pub(super) async fn verified_platform_role_groups(
    db: &Database,
    authority: &PlatformRoleAuthority,
    verifiers: &RwLock<Vec<Arc<dyn PlatformRoleVerifier>>>,
    caller: &nuono_app::auth::CallerIdentity,
    groups: &[nuono_storage::groups::Group],
) -> Result<std::collections::HashSet<i64>> {
    use sea_orm::{ConnectionTrait, Statement};
    use std::collections::{HashMap, HashSet};

    let mut verified = HashSet::new();
    if groups.is_empty()
        || matches!(
            ConfigurationActor::from_caller(caller).mode,
            ConfigurationMode::Administrator
        )
    {
        return Ok(verified);
    }

    let enabled = db
        .get_global_config("configuration.platform_roles_enabled")
        .await?
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    authority.set_enabled(enabled);
    if !enabled {
        return Ok(verified);
    }

    let providers = verifiers.read().await.clone();
    if providers.is_empty() {
        return Ok(verified);
    }

    let rows = db
        .orm()
        .query_all_raw(Statement::from_sql_and_values(
            db.backend(),
            "SELECT id,revision,platform,namespace,native_user_id,identity_kind \
             FROM nuono_platform_identities \
             WHERE user_id=$1 AND verified_at_ms IS NOT NULL \
             ORDER BY id",
            [caller.user_id.into()],
        ))
        .await?;
    if rows.is_empty() {
        return Ok(verified);
    }

    struct VerifiedIdentityRow {
        id: i64,
        revision: i64,
        native_user_id: String,
        scheme_suffix: String,
    }
    let mut by_scope: HashMap<(String, String), Vec<VerifiedIdentityRow>> = HashMap::new();
    for row in rows {
        let identity_kind: String = row.try_get("", "identity_kind")?;
        let scheme: serde_json::Value = nuono_domain::scalar::parse_json(&identity_kind)?;
        let scheme = scheme
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("json:{scheme}"));
        let platform: String = row.try_get("", "platform")?;
        let namespace: String = row.try_get("", "namespace")?;
        by_scope
            .entry((platform, namespace))
            .or_default()
            .push(VerifiedIdentityRow {
                id: row.try_get("", "id")?,
                revision: row.try_get("", "revision")?,
                native_user_id: row.try_get("", "native_user_id")?,
                scheme_suffix: format!(":{scheme}"),
            });
    }

    for group in groups {
        let Some(identities) = by_scope.get(&(group.platform.clone(), group.namespace.clone()))
        else {
            continue;
        };
        'group: for identity in identities {
            if !group.conversation_kind.ends_with(&identity.scheme_suffix) {
                continue;
            }
            let subject = PlatformRoleSubject {
                identity_id: identity.id,
                identity_revision: identity.revision,
                group_id: group.id,
                platform: group.platform.clone(),
                namespace: group.namespace.clone(),
                native_user_id: identity.native_user_id.clone(),
                conversation_kind: group.conversation_kind.clone(),
                native_parent_id: group.native_parent_id.clone(),
                native_group_id: group.native_group_id.clone(),
            };
            for provider in &providers {
                if authority
                    .verify(provider.as_ref(), subject.clone())
                    .await
                    .is_ok()
                {
                    verified.insert(group.id);
                    break 'group;
                }
            }
        }
    }

    Ok(verified)
}
pub(super) struct Access {
    pub db: Database,
    pub manager: Arc<PluginConfigManager>,
    pub plugin: String,
    pub scope: Arc<nuono_plugin::PluginTaskScope>,
    pub authority: PlatformRoleAuthority,
    pub verifiers: Arc<RwLock<Vec<Arc<dyn PlatformRoleVerifier>>>>,
}
impl Access {
    fn require_running(&self) -> Result<()> {
        if self.scope.is_closed() {
            return Err(NuonoError::PermissionDenied("插件已停用".into()));
        }
        Ok(())
    }

    async fn require(&self, actor: &IdentityAccess, group: &CanonicalGroup) -> Result<()> {
        self.require_running()?;
        let tx = self.db.begin_write().await?;
        nuono_storage::plugin_identity::require_identity_on(&tx, actor).await?;
        nuono_storage::plugin_identity::require_group_on(&tx, group).await?;
        tx.commit().await?;
        Ok(())
    }
    async fn actor(
        &self,
        access: &IdentityAccess,
        group: &CanonicalGroup,
    ) -> Result<ConfigurationActor> {
        let user_id = access
            .active_registered_user_id()?
            .ok_or_else(|| NuonoError::PermissionDenied("请先绑定框架账号后配置群功能".into()))?;
        let user = self
            .db
            .find_user_by_id(user_id)
            .await?
            .filter(|u| u.status == "active")
            .ok_or_else(|| NuonoError::PermissionDenied("账号不可用".into()))?;
        actor_for_group(
            &self.db,
            &self.authority,
            &self.verifiers,
            &nuono_app::auth::CallerIdentity::from_user(&user),
            group.id,
        )
        .await
    }
}
#[async_trait::async_trait]
impl PluginConfigurationAccess for Access {
    async fn configuration_resource_snapshot(
        &self,
        key: &str,
    ) -> Result<nuono_app::configuration_resources::ConfigurationResource> {
        self.require_running()?;
        let definition = self.manager.resource_definition(&self.plugin, key).await?;
        let snapshot = self.manager.read_latest(&self.plugin).await?;
        let raw = snapshot
            .values
            .get(key)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| NuonoError::ConfigValidation("结构配置字段缺失".into()))?;
        let value = serde_json::from_str(raw)?;
        definition.check(&value)?;
        Ok(value)
    }

    async fn configuration_resource_definition(
        &self,
        key: &str,
    ) -> Result<nuono_app::configuration_resources::ConfigurationResourceDefinition> {
        self.require_running()?;
        self.manager.resource_definition(&self.plugin, key).await
    }
    async fn configuration_resources_committed(&self) -> Result<()> {
        // 即使插件在并发关闭中，数据库里已提交的配置版本依然是权威有效的。
        self.manager.read_latest(&self.plugin).await.map(|_| ())
    }

    async fn verify_group_role(
        &self,
        candidates: GroupConfigurationCandidates,
    ) -> Result<Option<PlatformRoleProof>> {
        self.require_running()?;
        self.authority
            .set_enabled(candidates.platform_roles_enabled);
        if !candidates.platform_roles_enabled {
            return Ok(None);
        }
        let providers = self.verifiers.read().await.clone();
        for subject in candidates.subjects {
            for provider in &providers {
                if let Ok(proof) = self
                    .authority
                    .verify(provider.as_ref(), subject.clone())
                    .await
                {
                    return Ok(Some(proof));
                }
            }
        }
        Ok(None)
    }

    async fn group_configuration_for_task(
        &self,
        group: &CanonicalGroup,
    ) -> Result<PluginConfigurationSnapshot> {
        self.require_running()?;
        let tx = self.db.begin_write().await?;
        nuono_storage::plugin_identity::require_group_on(&tx, group).await?;
        tx.commit().await?;
        let (policy, saved, row) = self
            .db
            .plugin_configuration_snapshot(&self.plugin, Some(OverrideTarget::Group(group.id)))
            .await?;
        let override_id = row.as_ref().map(|r| r.id);
        let revision = row.as_ref().map_or(0, |r| r.revision);
        let config_version = policy.config_version;
        let resolved = crate::service::plugin_configuration::resolve_snapshot(
            &self.manager,
            &self.plugin,
            Some(OverrideTarget::Group(group.id)),
            policy,
            saved,
            row,
        )
        .await?;
        let tx = self.db.begin_write().await?;
        nuono_storage::plugin_identity::require_group_on(&tx, group).await?;
        tx.commit().await?;
        Ok(PluginConfigurationSnapshot {
            values: resolved.values.as_ref().clone(),
            enabled: resolved.enabled,
            can_configure: false,
            override_id,
            revision,
            config_version,
        })
    }

    async fn group_configuration(
        &self,
        access: &IdentityAccess,
        group: &CanonicalGroup,
    ) -> Result<PluginConfigurationSnapshot> {
        self.require(access, group).await?;
        let (policy, saved, row) = self
            .db
            .plugin_configuration_snapshot(&self.plugin, Some(OverrideTarget::Group(group.id)))
            .await?;
        let override_id = row.as_ref().map(|r| r.id);
        let revision = row.as_ref().map_or(0, |r| r.revision);
        let config_version = policy.config_version;
        let can_configure = if policy.allow_group_overrides {
            match self.actor(access, group).await {
                Ok(actor) => crate::service::group_plugins::ensure_group_settings_writable(
                    &self.db,
                    group.id,
                    Some(&self.plugin),
                    &actor,
                )
                .await
                .is_ok(),
                Err(NuonoError::PermissionDenied(_)) => false,
                Err(error) => return Err(error),
            }
        } else {
            false
        };
        let resolved = crate::service::plugin_configuration::resolve_snapshot(
            &self.manager,
            &self.plugin,
            Some(OverrideTarget::Group(group.id)),
            policy,
            saved,
            row,
        )
        .await?;
        self.require(access, group).await?;
        Ok(PluginConfigurationSnapshot {
            values: resolved.values.as_ref().clone(),
            enabled: resolved.enabled,
            can_configure,
            override_id,
            revision,
            config_version,
        })
    }
    async fn save_group_configuration(
        &self,
        access: &IdentityAccess,
        group: &CanonicalGroup,
        patch: OverridePatch,
    ) -> Result<PluginConfigurationSnapshot> {
        self.require(access, group).await?;
        let actor = self.actor(access, group).await?;
        let role = match actor.mode {
            ConfigurationMode::VerifiedPlatformRole(proof) => Some(proof),
            _ => None,
        };
        let actor = ConfigurationActor {
            user_id: actor.user_id,
            mode: ConfigurationMode::VerifiedIdentity {
                access: Box::new(access.clone()),
                group: Box::new(group.clone()),
                role,
            },
        };
        crate::service::plugin_configuration::save(
            &self.db,
            &self.manager,
            &actor,
            &self.plugin,
            OverrideTarget::Group(group.id),
            patch,
        )
        .await?;
        self.group_configuration(access, group).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuono_domain::{ConversationRef, IdentityKind, IdentityRef, IdentityScheme, Platform};
    use sea_orm::{ConnectionTrait, Statement};
    use serde_json::json;
    #[tokio::test]
    async fn sdk_group_config_reads_defaults_checks_grants_and_rejects_unbound_or_stale_writes() {
        let db = nuono_storage::test_support::test_db(4).await;
        let account = |name: &str, role: &str| crate::db::NewUser {
            username: name.into(),
            email: None,
            password: "configuration-sdk-tests".into(),
            role: role.into(),
            status: "active".into(),
            email_verified: false,
        };
        let root = db
            .create_user(account("config_root", "owner"))
            .await
            .unwrap();
        let user = db
            .create_user(account("config_member", "user"))
            .await
            .unwrap();
        let path = std::env::temp_dir().join(format!("nuono-config-sdk-{}", uuid::Uuid::new_v4()));
        let manager = Arc::new(PluginConfigManager::with_database(path.clone(), db.clone()));
        let schema = crate::config::plugin_config::plugin_fields_to_schema(&[
            nuono_plugin::PluginConfigField::text("greeting", "Greeting", "", "default")
                .group_scoped(),
            nuono_plugin::PluginConfigField::text("global_only", "Global", "", "fixed"),
        ])
        .unwrap();
        manager.register("sdk_config_probe", schema).await.unwrap();
        db.save_plugin_policy(
            root.id,
            "sdk_config_probe",
            nuono_storage::plugin_global_settings::PluginPolicyPatch {
                expected_revision: 1,
                allow_user_overrides: None,
                allow_group_overrides: Some(true),
                enabled: None,
            },
        )
        .await
        .unwrap();
        let native = |kind, value: &str| {
            IdentityRef::new(
                Platform::OneBot,
                kind,
                IdentityScheme::Native,
                value,
                None::<String>,
            )
            .unwrap()
        };
        let user_identity = native(IdentityKind::User, "50000");
        let guest = db.resolve_plugin_identity(&user_identity).await.unwrap();
        let group = db
            .resolve_plugin_group(&ConversationRef::Group {
                group: native(IdentityKind::Group, "60000"),
            })
            .await
            .unwrap()
            .unwrap();
        let scope = Arc::new(nuono_plugin::PluginTaskScope::new("sdk_config_probe"));
        let service = Arc::new(Access {
            db: db.clone(),
            manager,
            plugin: "sdk_config_probe".into(),
            scope: scope.clone(),
            authority: Default::default(),
            verifiers: Default::default(),
        });
        let sdk = nuono_plugin::db::Database::for_plugin(db.clone(), "sdk_config_probe")
            .with_configuration_access(service);
        let values = sdk.group_configuration(&guest, &group).await.unwrap();
        assert_eq!(values.values["greeting"], "default");
        assert!(!values.can_configure);
        assert_eq!(
            sdk.group_configuration_for_task(&group)
                .await
                .unwrap()
                .values,
            values.values
        );
        let patch = || OverridePatch {
            config_version: 1,
            set_config: json!({"greeting":"group"}).as_object().unwrap().clone(),
            ..Default::default()
        };
        assert!(
            sdk.save_group_configuration(&guest, &group, patch())
                .await
                .is_err()
        );
        db.orm().execute_raw(Statement::from_sql_and_values(db.backend(),"UPDATE nuono_platform_identities SET user_id=$1,verified_at_ms=1,revision=revision+1 WHERE id=$2",[user.id.into(),guest.identity_id.into()])).await.unwrap();
        let access = sdk.resolve_identity(&user_identity).await.unwrap();
        assert!(
            sdk.save_group_configuration(&access, &group, patch())
                .await
                .is_err()
        );
        let grant = db
            .grant_group_configuration(
                root.id,
                &nuono_storage::group_authorizations::ManualGroupGrant {
                    user_id: user.id,
                    group_id: group.id,
                    plugin_id: Some("sdk_config_probe".into()),
                    source_key: "sdk-config-grant".into(),
                    valid_from_ms: 0,
                    expires_at_ms: None,
                },
            )
            .await
            .unwrap();
        assert!(
            sdk.group_configuration(&access, &group)
                .await
                .unwrap()
                .can_configure
        );
        let mut invalid = patch();
        invalid.set_config = json!({"global_only":"changed"})
            .as_object()
            .unwrap()
            .clone();
        assert!(
            sdk.save_group_configuration(&access, &group, invalid)
                .await
                .is_err()
        );
        let saved = sdk
            .save_group_configuration(&access, &group, patch())
            .await
            .unwrap();
        assert_eq!(saved.values["greeting"], "group");
        assert!(
            sdk.save_group_configuration(&access, &group, patch())
                .await
                .is_err()
        );
        let collection = nuono_plugin::db::PluginStateCollection::new(
            "sdk_config_probe",
            "receipts",
            nuono_plugin::db::PluginStateScope::global(),
        )
        .unwrap();
        let tx = sdk.begin().await.unwrap();
        tx.require_group_configuration(&access, &group)
            .await
            .unwrap();
        tx.state_create(&collection, "allowed", 1, json!({}), None)
            .await
            .unwrap()
            .unwrap();
        tx.commit().await.unwrap();
        let tx = sdk.begin().await.unwrap();
        tx.require_group_configuration(&access, &group)
            .await
            .unwrap();
        tx.state_create(&collection, "must_rollback", 1, json!({}), None)
            .await
            .unwrap()
            .unwrap();
        // 模拟业务操作进行期间，临时配置授权令牌刚好过期失效的场景。
        tx.execute(
            "UPDATE nuono_group_authorizations SET expires_at_ms=1 WHERE id=$1",
            vec![nuono_plugin::db::PluginSqlValue::Integer(grant.id)],
        )
        .await
        .unwrap();
        assert!(tx.commit().await.is_err());
        assert!(
            sdk.state_record(&collection, "must_rollback")
                .await
                .unwrap()
                .is_none()
        );
        db.revoke_group_configuration(root.id, group.id, grant.id)
            .await
            .unwrap();
        let tx = sdk.begin().await.unwrap();
        assert!(
            tx.require_group_configuration(&access, &group)
                .await
                .is_err()
        );
        tx.rollback().await.unwrap();
        assert!(!sdk.is_administrator(&access).await.unwrap());
        let root_native = native(IdentityKind::User, "50001");
        let root_guest = db.resolve_plugin_identity(&root_native).await.unwrap();
        db.orm().execute_raw(Statement::from_sql_and_values(db.backend(),"UPDATE nuono_platform_identities SET user_id=$1,verified_at_ms=1,revision=revision+1 WHERE id=$2",[root.id.into(),root_guest.identity_id.into()])).await.unwrap();
        let root_access = sdk.resolve_identity(&root_native).await.unwrap();
        assert!(sdk.is_administrator(&root_access).await.unwrap());
        let tx = sdk.begin().await.unwrap();
        tx.require_administrator(&root_access).await.unwrap();
        tx.execute(
            "UPDATE nuono_users SET role='user' WHERE id=$1",
            vec![nuono_plugin::db::PluginSqlValue::Integer(root.id)],
        )
        .await
        .unwrap();
        assert!(tx.commit().await.is_err());
        db.unbind_platform_identity(user.id, access.identity_id, access.revision)
            .await
            .unwrap();
        let update = OverridePatch {
            expected_id: saved.override_id,
            expected_revision: saved.revision,
            ..patch()
        };
        assert!(
            sdk.save_group_configuration(&access, &group, update)
                .await
                .is_err()
        );
        assert_eq!(
            sdk.group_configuration_for_task(&group)
                .await
                .unwrap()
                .values["greeting"],
            "group"
        );
        scope.shutdown().await;
        assert!(sdk.group_configuration_for_task(&group).await.is_err());
        db.shutdown().await.unwrap();
        if path.exists() {
            tokio::fs::remove_dir_all(path).await.unwrap();
        }
    }
}
