//! 部署级服务能力门禁与平台角色信任开关的持久化（独立于插件当前的安装或卸载状态）。
use crate::{
    Database,
    db::{AuditEvent, append_audit_on},
    error::{NuonoError, Result},
};
use sea_orm::{ConnectionTrait, Statement};
const KEY: &str = "service.required_providers";
impl Database {
    pub async fn required_service_providers(&self) -> Result<Vec<String>> {
        self.get_global_config(KEY)
            .await?
            .map(serde_json::from_value)
            .transpose()
            .map(|value| value.unwrap_or_default())
            .map_err(Into::into)
    }
    /// 更新部署环境必需的服务提供商列表（仅限活跃的 `owner` 账号修改或移除服务门禁；插件卸载绝不会自动移除已登记的服务门禁要求）。
    pub async fn set_required_service_providers(
        &self,
        actor: i64,
        providers: &[String],
    ) -> Result<()> {
        self.write_deployment_policy(actor, providers, None).await
    }
    pub async fn set_deployment_policy(
        &self,
        actor: i64,
        providers: &[String],
        platform_roles_enabled: bool,
    ) -> Result<()> {
        self.write_deployment_policy(actor, providers, Some(platform_roles_enabled))
            .await
    }
    async fn write_deployment_policy(
        &self,
        actor: i64,
        providers: &[String],
        platform_roles_enabled: Option<bool>,
    ) -> Result<()> {
        if providers.iter().any(|id| {
            id.is_empty()
                || id.len() > 64
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        }) {
            return Err(NuonoError::Validation("服务策略插件标识无效".into()));
        }
        let tx = self.begin_write().await?;
        crate::group_authorizations::lock_accounts(&tx, &[actor]).await?;
        if tx
            .query_one_raw(Statement::from_sql_and_values(
                self.backend(),
                "SELECT id FROM nuono_users WHERE id=$1 AND role='owner' AND status='active'",
                [actor.into()],
            ))
            .await?
            .is_none()
        {
            return Err(NuonoError::PermissionDenied(
                "只有部署所有者可修改必需服务策略".into(),
            ));
        }
        let mut providers = providers.to_vec();
        providers.sort();
        providers.dedup();
        let now = crate::clock::db_now().timestamp_millis();
        tx.execute_raw(Statement::from_sql_and_values(self.backend(),"INSERT INTO nuono_global_configs(key,value_json,value_version,revision,created_at_ms,updated_at_ms) VALUES ($1,$2,1,1,$3,$3) ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json,revision=nuono_global_configs.revision+1,updated_at_ms=excluded.updated_at_ms",[KEY.into(),serde_json::to_string(&providers)?.into(),now.into()])).await?;
        if let Some(enabled) = platform_roles_enabled {
            tx.execute_raw(Statement::from_sql_and_values(self.backend(),"INSERT INTO nuono_global_configs(key,value_json,value_version,revision,created_at_ms,updated_at_ms) VALUES ('configuration.platform_roles_enabled',$1,1,1,$2,$2) ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json,revision=nuono_global_configs.revision+1,updated_at_ms=excluded.updated_at_ms",[enabled.to_string().into(),now.into()])).await?;
        }
        append_audit_on(&tx,AuditEvent{actor_user_id:Some(actor),actor_plugin_id:None,action:"service.policy_required",result:"success",subject_user_id:None,group_id:None,operation_id:None,details:serde_json::json!({"providers":providers,"platform_roles_enabled":platform_roles_enabled})}).await?;
        tx.commit().await?;
        Ok(())
    }
}
impl Database {
    pub async fn set_platform_role_policy(&self, actor: i64, enabled: bool) -> Result<()> {
        let tx = self.begin_write().await?;
        crate::group_authorizations::lock_accounts(&tx, &[actor]).await?;
        if tx
            .query_one_raw(Statement::from_sql_and_values(
                self.backend(),
                "SELECT id FROM nuono_users WHERE id=$1 AND role='owner' AND status='active'",
                [actor.into()],
            ))
            .await?
            .is_none()
        {
            return Err(NuonoError::PermissionDenied("需要部署所有者权限".into()));
        }
        let now = crate::clock::db_now().timestamp_millis();
        tx.execute_raw(Statement::from_sql_and_values(self.backend(),"INSERT INTO nuono_global_configs(key,value_json,value_version,revision,created_at_ms,updated_at_ms) VALUES ('configuration.platform_roles_enabled',$1,1,1,$2,$2) ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json,revision=nuono_global_configs.revision+1,updated_at_ms=excluded.updated_at_ms",[enabled.to_string().into(),now.into()])).await?;
        append_audit_on(
            &tx,
            AuditEvent {
                actor_user_id: Some(actor),
                actor_plugin_id: None,
                action: "configuration.platform_roles",
                result: "success",
                subject_user_id: None,
                group_id: None,
                operation_id: None,
                details: serde_json::json!({"enabled":enabled}),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}
