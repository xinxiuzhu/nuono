//! 用户个人级与规范群组级插件配置差异覆盖项（overrides）的统一事务存储。
use crate::{
    Database,
    entity::{plugin_overrides, plugins, users},
    error::{NuonoError, Result},
};
use nuono_app::configuration::{ConfigurationActor, ConfigurationMode};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DbBackend, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    Set, TransactionTrait,
};
use serde_json::{Map, Value};

pub use plugin_overrides::Model as Override;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideTarget {
    User(i64),
    Group(i64),
}

pub use nuono_app::configuration::OverridePatch;

fn query(plugin: &str, target: OverrideTarget) -> sea_orm::Select<plugin_overrides::Entity> {
    let query =
        plugin_overrides::Entity::find().filter(plugin_overrides::Column::PluginId.eq(plugin));
    match target {
        OverrideTarget::User(id) => query.filter(plugin_overrides::Column::UserId.eq(id)),
        OverrideTarget::Group(id) => query.filter(plugin_overrides::Column::GroupId.eq(id)),
    }
}

impl Database {
    pub async fn plugin_configuration_snapshot(
        &self,
        plugin: &str,
        target: Option<OverrideTarget>,
    ) -> Result<(
        plugins::Model,
        crate::plugins::PluginConfiguration,
        Option<Override>,
    )> {
        let tx = if self.backend() == DbBackend::Postgres {
            self.orm()
                .begin_with_config(
                    Some(sea_orm::IsolationLevel::RepeatableRead),
                    Some(sea_orm::AccessMode::ReadOnly),
                )
                .await?
        } else {
            self.orm().begin().await?
        };
        let policy = plugins::Entity::find_by_id(plugin)
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::PluginNotFound(plugin.into()))?;
        let mut values = serde_json::from_str(&policy.config_json)?;
        crate::plugin_global_settings::hydrate_secrets(&tx, plugin, &mut values).await?;
        let configuration = crate::plugins::PluginConfiguration {
            values,
            service: serde_json::from_str(&policy.service_config_json)?,
            revision: policy.revision,
            config_version: policy.config_version,
        };
        let mut row = match target {
            Some(target) => query(plugin, target).one(&tx).await?,
            None => None,
        };
        let permitted = match target {
            Some(OverrideTarget::User(_)) => policy.allow_user_overrides,
            Some(OverrideTarget::Group(_)) => policy.allow_group_overrides,
            None => false,
        };
        if permitted && let Some(row) = &mut row {
            let mut values = serde_json::from_str(&row.config_patch_json)?;
            crate::plugin_global_settings::hydrate_secrets(&tx, plugin, &mut values).await?;
            row.config_patch_json = serde_json::Value::Object(values).to_string();
        }
        tx.commit().await?;
        Ok((policy, configuration, row))
    }

    pub async fn plugin_override(
        &self,
        plugin: &str,
        target: OverrideTarget,
    ) -> Result<Option<Override>> {
        Ok(query(plugin, target).one(self.orm()).await?)
    }

    pub async fn save_plugin_override(
        &self,
        actor: &ConfigurationActor,
        plugin: &str,
        target: OverrideTarget,
        patch: OverridePatch,
    ) -> Result<Option<Override>> {
        self.save_plugin_override_with_secrets(actor, plugin, target, patch, &[])
            .await
    }
    pub async fn save_plugin_override_with_secrets(
        &self,
        actor: &ConfigurationActor,
        plugin: &str,
        target: OverrideTarget,
        patch: OverridePatch,
        secret_fields: &[String],
    ) -> Result<Option<Override>> {
        let actor_user_id = actor.user_id;
        if patch.expected_revision < 0
            || patch.config_version <= 0
            || (patch.inherit_enabled && patch.set_enabled.is_some())
        {
            return Err(NuonoError::Validation("覆盖版本或开关操作无效".into()));
        }
        for key in patch.remove_config.iter().chain(&patch.remove_service) {
            if patch.set_config.contains_key(key) || patch.set_service.contains_key(key) {
                return Err(NuonoError::Validation(format!(
                    "字段 {key} 不能同时设置与恢复继承"
                )));
            }
        }
        let tx = self.begin_write().await?;
        let mut ids = vec![actor_user_id];
        if let OverrideTarget::User(id) = target {
            ids.push(id);
        }
        ids.sort_unstable();
        ids.dedup();
        let mut accounts = users::orm::Entity::find()
            .filter(users::orm::Column::Id.is_in(ids))
            .order_by_asc(users::orm::Column::Id);
        if self.backend() == DbBackend::Postgres {
            accounts = accounts.lock_exclusive();
        }
        let accounts = accounts.all(&tx).await?;
        let current_actor = accounts
            .iter()
            .find(|user| user.id == actor_user_id && user.status == "active")
            .ok_or_else(|| NuonoError::PermissionDenied("账号不可用".into()))?;
        let verified_role;
        let mode = match &actor.mode {
            ConfigurationMode::VerifiedIdentity {
                access,
                group,
                role,
            } => {
                if access.active_registered_user_id()? != Some(actor_user_id)
                    || target != OverrideTarget::Group(group.id)
                {
                    return Err(NuonoError::PermissionDenied(
                        "配置身份或群对象不匹配".into(),
                    ));
                }
                crate::plugin_identity::require_identity_on(&tx, access).await?;
                crate::plugin_identity::require_group_on(&tx, group).await?;
                verified_role = if matches!(current_actor.role.as_str(), "owner" | "admin") {
                    ConfigurationMode::Administrator
                } else if let Some(role) = role {
                    ConfigurationMode::VerifiedPlatformRole(role.clone())
                } else {
                    ConfigurationMode::Account
                };
                &verified_role
            }
            mode => mode,
        };
        let administrator = matches!(mode, ConfigurationMode::Administrator)
            && matches!(current_actor.role.as_str(), "owner" | "admin");
        if matches!(mode, ConfigurationMode::Administrator) && !administrator {
            return Err(NuonoError::PermissionDenied("管理员权限已撤销".into()));
        }
        match target {
            OverrideTarget::User(id) => {
                if (!administrator && id != actor_user_id)
                    || !accounts
                        .iter()
                        .any(|user| user.id == id && user.status == "active")
                {
                    return Err(NuonoError::PermissionDenied(
                        "无权修改该账号的个人配置".into(),
                    ));
                }
            }
            OverrideTarget::Group(id) if !administrator => {
                if let ConfigurationMode::VerifiedPlatformRole(proof) = mode {
                    crate::plugin_group_access::require_platform_role_on(
                        &tx,
                        actor_user_id,
                        id,
                        proof,
                    )
                    .await?;
                } else {
                    crate::group_authorizations::lock_group_configuration_grant(
                        &tx,
                        actor_user_id,
                        id,
                        plugin,
                    )
                    .await?;
                }
            }
            OverrideTarget::Group(_) => {}
        }
        let mut policy = plugins::Entity::find_by_id(plugin);
        if self.backend() == DbBackend::Postgres {
            policy = policy.lock_exclusive();
        }
        let policy = policy
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::PluginNotFound(plugin.into()))?;
        if policy.plugin_role == nuono_app::plugin_role::PluginRole::Service.as_str() {
            return Err(NuonoError::PermissionDenied(
                "底层服务插件不支持个人或群级配置覆盖".into(),
            ));
        }
        if policy.config_version != patch.config_version {
            return Err(NuonoError::Conflict(
                "插件配置声明版本已改变，请刷新".into(),
            ));
        }
        let permitted = match target {
            OverrideTarget::User(_) => policy.allow_user_overrides,
            OverrideTarget::Group(_) => policy.allow_group_overrides,
        };
        if !permitted {
            return Err(NuonoError::PermissionDenied(
                "管理员未开放此类配置覆盖".into(),
            ));
        }
        let mut selected = query(plugin, target);
        if self.backend() == DbBackend::Postgres {
            selected = selected.lock_exclusive();
        }
        let old = selected.one(&tx).await?;
        if old.as_ref().map(|row| row.id) != patch.expected_id
            || old.as_ref().map_or(0, |row| row.revision) != patch.expected_revision
        {
            return Err(NuonoError::Conflict("插件覆盖版本冲突，请刷新".into()));
        }
        let parse = |raw: &str| {
            serde_json::from_str::<Map<String, Value>>(raw)
                .map_err(|_| NuonoError::ConfigValidation("已保存的覆盖不是 JSON 对象".into()))
        };
        let mut config = old
            .as_ref()
            .map(|row| parse(&row.config_patch_json))
            .transpose()?
            .unwrap_or_default();
        crate::plugin_global_settings::hydrate_secrets(&tx, plugin, &mut config).await?;
        let mut service = old
            .as_ref()
            .map(|row| parse(&row.service_patch_json))
            .transpose()?
            .unwrap_or_default();
        for key in patch.remove_config {
            config.remove(&key);
        }
        for key in patch.remove_service {
            service.remove(&key);
        }
        config.extend(patch.set_config);
        service.extend(patch.set_service);
        let scope = match target {
            OverrideTarget::User(id) => format!("user.{id}."),
            OverrideTarget::Group(id) => format!("group.{id}."),
        };
        crate::plugin_global_settings::store_scoped_secrets(
            &tx,
            plugin,
            &scope,
            &mut config,
            secret_fields,
        )
        .await?;
        let enabled = if patch.inherit_enabled {
            None
        } else {
            patch
                .set_enabled
                .or(old.as_ref().and_then(|row| row.enabled))
        };
        let saved = if enabled.is_none() && config.is_empty() && service.is_empty() {
            if let Some(old) = old {
                plugin_overrides::Entity::delete_by_id(old.id)
                    .exec(&tx)
                    .await?;
            }
            None
        } else {
            let now = chrono::Utc::now().timestamp_millis();
            let (user_id, group_id) = match target {
                OverrideTarget::User(id) => (Some(id), None),
                OverrideTarget::Group(id) => (None, Some(id)),
            };
            let mut next = plugin_overrides::ActiveModel {
                plugin_id: Set(plugin.into()),
                user_id: Set(user_id),
                group_id: Set(group_id),
                enabled: Set(enabled),
                config_patch_json: Set(Value::Object(config).to_string()),
                service_patch_json: Set(Value::Object(service).to_string()),
                revision: Set(patch
                    .expected_revision
                    .checked_add(1)
                    .ok_or_else(|| NuonoError::Conflict("覆盖版本已耗尽".into()))?),
                created_at_ms: Set(old.as_ref().map_or(now, |row| row.created_at_ms)),
                updated_at_ms: Set(now),
                ..Default::default()
            };
            if let Some(old) = old {
                next.id = Set(old.id);
                Some(next.update(&tx).await?)
            } else {
                Some(next.insert(&tx).await?)
            }
        };
        if let OverrideTarget::Group(id) = target
            && !administrator
        {
            if let ConfigurationMode::VerifiedPlatformRole(proof) = mode {
                proof.validate_current()?;
            } else {
                crate::group_authorizations::lock_group_configuration_grant(
                    &tx,
                    actor_user_id,
                    id,
                    plugin,
                )
                .await?;
            }
        }
        crate::plugin_global_settings::purge_unused_plugin_secrets_on(&tx, plugin).await?;
        crate::db::append_audit_on(&tx, crate::db::AuditEvent {
            actor_user_id:Some(actor_user_id),actor_plugin_id:None,action:"plugin.override.update",result:"success",
            subject_user_id:match target { OverrideTarget::User(id)=>Some(id), _=>None },
            group_id:match target { OverrideTarget::Group(id)=>Some(id), _=>None },
            operation_id:None, details:serde_json::json!({"plugin_id":plugin,"previous_revision":patch.expected_revision.to_string(),"revision":saved.as_ref().map(|row|row.revision.to_string())}),
        }).await?;
        tx.commit().await?;
        Ok(saved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn account(db: &Database, name: &str, role: &str) -> i64 {
        db.create_user(crate::db::NewUser {
            username: name.into(),
            email: None,
            password: "a-valid-password-123".into(),
            role: role.into(),
            status: "active".into(),
            email_verified: false,
        })
        .await
        .unwrap()
        .id
    }
    async fn fixture() -> (Database, i64, i64, i64) {
        let db = crate::test_support::test_db(4).await;
        let root = account(&db, "root", "owner").await;
        let user = account(&db, "member", "user").await;
        db.sync_plugin("settings_probe", "1", &serde_json::json!({}))
            .await
            .unwrap();
        db.save_plugin_policy(
            root,
            "settings_probe",
            crate::plugin_global_settings::PluginPolicyPatch {
                expected_revision: 1,
                allow_user_overrides: Some(true),
                allow_group_overrides: Some(true),
                enabled: None,
            },
        )
        .await
        .unwrap();
        let group = db
            .ensure_group(
                &crate::groups::GroupIdentity {
                    platform: "qq".into(),
                    namespace: "".into(),
                    conversation_kind: "group:numeric".into(),
                    native_parent_id: "".into(),
                    native_group_id: "123".into(),
                },
                "test",
            )
            .await
            .unwrap();
        (db, root, user, group.id)
    }
    fn patch() -> OverridePatch {
        OverridePatch {
            config_version: 1,
            set_config: serde_json::json!({"greeting":"hello"})
                .as_object()
                .unwrap()
                .clone(),
            ..Default::default()
        }
    }
    #[tokio::test]
    async fn personal_ownership_cas_delete_and_recreation_reject_stale_edits() {
        let (db, root, user, _) = fixture().await;
        let actor = ConfigurationActor {
            user_id: user,
            mode: ConfigurationMode::Account,
        };
        assert!(
            db.save_plugin_override(
                &actor,
                "settings_probe",
                OverrideTarget::User(root),
                patch()
            )
            .await
            .is_err()
        );
        let row = db
            .save_plugin_override(
                &actor,
                "settings_probe",
                OverrideTarget::User(user),
                patch(),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            db.save_plugin_override(
                &actor,
                "settings_probe",
                OverrideTarget::User(user),
                patch()
            )
            .await
            .is_err()
        );
        let remove = OverridePatch {
            expected_id: Some(row.id),
            expected_revision: row.revision,
            config_version: 1,
            remove_config: vec!["greeting".into()],
            ..Default::default()
        };
        assert!(
            db.save_plugin_override(
                &actor,
                "settings_probe",
                OverrideTarget::User(user),
                remove.clone()
            )
            .await
            .unwrap()
            .is_none()
        );
        let replacement = db
            .save_plugin_override(
                &actor,
                "settings_probe",
                OverrideTarget::User(user),
                patch(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_ne!(row.id, replacement.id);
        assert!(
            db.save_plugin_override(&actor, "settings_probe", OverrideTarget::User(user), remove)
                .await
                .is_err()
        );
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn revoked_source_does_not_remove_other_source_and_recheck_blocks_save() {
        let (db, root, user, group) = fixture().await;
        let actor = ConfigurationActor {
            user_id: user,
            mode: ConfigurationMode::Account,
        };
        let source = |key: &str| crate::group_authorizations::ManualGroupGrant {
            user_id: user,
            group_id: group,
            plugin_id: Some("settings_probe".into()),
            source_key: key.into(),
            valid_from_ms: 0,
            expires_at_ms: None,
        };
        assert!(
            db.save_plugin_override(
                &actor,
                "settings_probe",
                OverrideTarget::Group(group),
                patch()
            )
            .await
            .is_err()
        );
        let a = db
            .grant_group_configuration(root, &source("a"))
            .await
            .unwrap();
        let b = db
            .grant_group_configuration(root, &source("b"))
            .await
            .unwrap();
        db.revoke_group_configuration(root, group, a.id)
            .await
            .unwrap();
        let row = db
            .save_plugin_override(
                &actor,
                "settings_probe",
                OverrideTarget::Group(group),
                patch(),
            )
            .await
            .unwrap()
            .unwrap();
        db.revoke_group_configuration(root, group, b.id)
            .await
            .unwrap();
        let edit = OverridePatch {
            expected_id: Some(row.id),
            expected_revision: row.revision,
            ..patch()
        };
        assert!(
            db.save_plugin_override(&actor, "settings_probe", OverrideTarget::Group(group), edit)
                .await
                .is_err()
        );
        assert!(
            db.plugin_override("settings_probe", OverrideTarget::Group(group))
                .await
                .unwrap()
                .is_some()
        );
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn concurrent_revocation_serializes_before_or_after_the_whole_configuration_write() {
        use sea_orm::ConnectionTrait;
        let (db, root, user, group) = fixture().await;
        let actor = ConfigurationActor {
            user_id: user,
            mode: ConfigurationMode::Account,
        };
        let grant = db
            .grant_group_configuration(
                root,
                &crate::group_authorizations::ManualGroupGrant {
                    user_id: user,
                    group_id: group,
                    plugin_id: Some("settings_probe".into()),
                    source_key: "concurrent".into(),
                    valid_from_ms: 0,
                    expires_at_ms: None,
                },
            )
            .await
            .unwrap();
        let (save, revoke) = tokio::join!(
            db.save_plugin_override(
                &actor,
                "settings_probe",
                OverrideTarget::Group(group),
                patch()
            ),
            db.revoke_group_configuration(root, group, grant.id)
        );
        revoke.unwrap();
        let audit=db.orm().query_all_raw(sea_orm::Statement::from_sql_and_values(db.backend(),"SELECT id,action FROM nuono_audit_events WHERE group_id=$1 AND action IN ('plugin.override.update','authorization.revoke') ORDER BY id",[group.into()])).await.unwrap();
        let actions = audit
            .into_iter()
            .map(|row| row.try_get::<String>("", "action").unwrap())
            .collect::<Vec<_>>();
        if save.is_ok() {
            assert_eq!(
                actions,
                vec!["plugin.override.update", "authorization.revoke"]
            );
        } else {
            assert_eq!(actions, vec!["authorization.revoke"]);
            assert!(
                db.plugin_override("settings_probe", OverrideTarget::Group(group))
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        assert!(
            !db.has_group_configuration_grant(user, group, Some("settings_probe"))
                .await
                .unwrap()
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn configuration_migration_validates_every_target_before_commit() {
        let (db, _, user, _) = fixture().await;
        let actor = ConfigurationActor {
            user_id: user,
            mode: ConfigurationMode::Account,
        };
        let row = db
            .save_plugin_override(
                &actor,
                "settings_probe",
                OverrideTarget::User(user),
                patch(),
            )
            .await
            .unwrap()
            .unwrap();
        let fail = db
            .upgrade_plugin_configuration("settings_probe", 2, &[], |_, target, values, _| {
                if matches!(
                    target,
                    nuono_app::config_schema::ConfigurationTarget::User(_)
                ) {
                    Err(NuonoError::ConfigValidation("bad user config".into()))
                } else {
                    Ok(values.clone())
                }
            })
            .await;
        assert!(fail.is_err());
        assert_eq!(
            db.plugin_policy("settings_probe")
                .await
                .unwrap()
                .config_version,
            1
        );
        assert_eq!(
            db.plugin_override("settings_probe", OverrideTarget::User(user))
                .await
                .unwrap()
                .unwrap()
                .revision,
            row.revision
        );
        db.upgrade_plugin_configuration("settings_probe", 2, &[], |_, _, values, _| {
            let mut values = values.clone();
            if let Some(value) = values.remove("greeting") {
                values.insert("welcome".into(), value);
            }
            Ok(values)
        })
        .await
        .unwrap();
        let saved = db
            .plugin_override("settings_probe", OverrideTarget::User(user))
            .await
            .unwrap()
            .unwrap();
        assert!(saved.config_patch_json.contains("welcome"));
        assert_eq!(
            db.plugin_policy("settings_probe")
                .await
                .unwrap()
                .config_version,
            2
        );
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn secrets_are_separate_atomic_and_snapshot_resolves_them() {
        let (db, root, user, _) = fixture().await;
        let mut global = db.plugin_configuration("settings_probe").await.unwrap();
        global.values.insert("token".into(), "private-token".into());
        db.save_plugin_configuration_as(Some(root), "settings_probe", &global, &["token".into()])
            .await
            .unwrap();
        let policy = db.plugin_policy("settings_probe").await.unwrap();
        assert!(!policy.config_json.contains("private-token"));
        assert!(policy.config_json.contains("$secret"));
        let (_, global, _) = db
            .plugin_configuration_snapshot("settings_probe", Some(OverrideTarget::User(user)))
            .await
            .unwrap();
        assert_eq!(global.values["token"], "private-token");
        let actor = ConfigurationActor {
            user_id: user,
            mode: ConfigurationMode::Account,
        };
        let patch = OverridePatch {
            set_config: serde_json::json!({"token":"personal-token"})
                .as_object()
                .unwrap()
                .clone(),
            ..patch()
        };
        db.save_plugin_override_with_secrets(
            &actor,
            "settings_probe",
            OverrideTarget::User(user),
            patch,
            &["token".into()],
        )
        .await
        .unwrap();
        let row = db
            .plugin_override("settings_probe", OverrideTarget::User(user))
            .await
            .unwrap()
            .unwrap();
        assert!(!row.config_patch_json.contains("personal-token"));
        let (_, _, row) = db
            .plugin_configuration_snapshot("settings_probe", Some(OverrideTarget::User(user)))
            .await
            .unwrap();
        assert!(row.unwrap().config_patch_json.contains("personal-token"));
        db.shutdown().await.unwrap();
    }
}
