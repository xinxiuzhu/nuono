//! AI 模型提供商配置持久化（SeaORM 双后端：PostgreSQL / SQLite）。

use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
};

use super::Database;
use crate::error::{NuonoError, Result};

use crate::entity::{ai_models, global_configs};
use nuono_app::model_catalog::{AiModelRecord, ModelChange, validate_references};
pub use nuono_app::model_catalog::{CreateAiModel, UpdateAiModel};
use sea_orm::{DatabaseTransaction, DbBackend};

impl From<ai_models::Model> for AiModelRecord {
    fn from(row: ai_models::Model) -> Self {
        Self {
            id: row.id,
            name: row.name,
            provider: row.provider,
            provider_type: row.provider_type,
            model_id: row.model_id,
            api_key: row.api_key,
            base_url: row.base_url,
            is_default: row.is_default,
            enabled: row.enabled,
            created_at: crate::clock::load_time(row.created_at),
            updated_at: crate::clock::load_time(row.updated_at),
        }
    }
}
/// 获取模型目录写锁：即便模型表为空，也在 `nuono_global_configs` 中维护一行固定锁记录。
/// 全局加锁顺序固定为：模型目录 -> 运行时设置 -> 密钥（secrets）。
/// SQLite 的 `BEGIN IMMEDIATE` 已经在库级串行化写事务；PostgreSQL 则对该锁记录加行级排他锁。
pub(super) async fn lock_catalog(tx: &DatabaseTransaction, backend: DbBackend) -> Result<()> {
    const KEY: &str = "runtime.ai_models.lock.v1";
    global_configs::Entity::insert(global_configs::ActiveModel {
        key: Set(KEY.into()),
        value: Set("{}".into()),
        created_at_ms: Set(crate::clock::db_now().timestamp_millis()),
        updated_at: Set(crate::clock::db_now()),
        ..Default::default()
    })
    .on_conflict_do_nothing()
    .exec_without_returning(tx)
    .await?;
    let mut query = global_configs::Entity::find_by_id(KEY);
    if backend == DbBackend::Postgres {
        query = query.lock_exclusive();
    }
    query
        .one(tx)
        .await?
        .ok_or_else(|| NuonoError::Internal("模型目录锁不存在".into()))?;
    Ok(())
}
pub(super) async fn catalog(tx: &DatabaseTransaction) -> Result<Vec<AiModelRecord>> {
    if !super::migrations::is_ai_module_installed(tx).await? {
        return Ok(Vec::new());
    }
    Ok(ai_models::Entity::find()
        .all(tx)
        .await?
        .into_iter()
        .map(Into::into)
        .collect())
}
impl Database {
    #[tracing::instrument(skip(self), err(Debug))]
    pub async fn list_ai_models(&self) -> Result<Vec<crate::entity::ai_models::Model>> {
        if !self.is_ai_module_installed().await? {
            return Ok(Vec::new());
        }
        let rows = crate::entity::ai_models::Entity::find()
            .order_by_asc(crate::entity::ai_models::Column::Id)
            .all(self.orm())
            .await?;
        Ok(rows)
    }

    #[tracing::instrument(skip(self), err(Debug))]
    pub async fn get_ai_model(&self, id: i64) -> Result<Option<crate::entity::ai_models::Model>> {
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        let row = crate::entity::ai_models::Entity::find_by_id(id)
            .one(self.orm())
            .await?;
        Ok(row)
    }

    #[tracing::instrument(skip(self), err(Debug))]
    pub async fn get_default_ai_model(&self) -> Result<Option<crate::entity::ai_models::Model>> {
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        let row = crate::entity::ai_models::Entity::find()
            .filter(crate::entity::ai_models::Column::IsDefault.eq(true))
            .filter(crate::entity::ai_models::Column::Enabled.eq(true))
            .limit(1)
            .one(self.orm())
            .await?;
        Ok(row)
    }

    pub async fn create_ai_model(&self, input: CreateAiModel<'_>) -> Result<ai_models::Model> {
        self.change_model_record(ModelChange::Create {
            input,
            enabled: true,
        })
        .await?
        .ok_or_else(|| NuonoError::Internal("创建模型未返回记录".into()))
    }
    pub async fn update_ai_model(
        &self,
        id: i64,
        input: UpdateAiModel<'_>,
    ) -> Result<ai_models::Model> {
        self.change_model_record(ModelChange::Update { id, patch: input })
            .await?
            .ok_or_else(|| NuonoError::Internal("更新模型未返回记录".into()))
    }
    pub async fn delete_ai_model(&self, id: i64) -> Result<()> {
        self.change_model_record(ModelChange::Delete { id }).await?;
        Ok(())
    }
    async fn change_model_record(
        &self,
        change: ModelChange<'_>,
    ) -> Result<Option<ai_models::Model>> {
        self.ensure_ai_module().await?;
        let tx = self.begin_write().await?;
        lock_catalog(&tx, self.backend()).await?;
        let row = apply_change(&tx, change).await?;
        if let Some(settings) = global_configs::Entity::find_by_id("runtime.settings.v1")
            .one(&tx)
            .await?
        {
            let settings = serde_json::from_str(&settings.value)?;
            validate_references(&settings, &catalog(&tx).await?, false)
                .map_err(reference_conflict)?;
        }
        tx.commit().await?;
        Ok(row)
    }
}
pub(super) fn reference_conflict(error: NuonoError) -> NuonoError {
    match error {
        NuonoError::ConfigValidation(message) => NuonoError::Conflict(message),
        other => other,
    }
}
/// 在调用方已持有模型目录锁的写事务内执行模型增删改（调用方负责在提交前校验模型引用完整性）。
pub(super) async fn apply_change(
    tx: &DatabaseTransaction,
    change: ModelChange<'_>,
) -> Result<Option<ai_models::Model>> {
    let mut row = match change {
        ModelChange::Delete { id } => {
            if ai_models::Entity::delete_by_id(id)
                .exec(tx)
                .await?
                .rows_affected
                == 0
            {
                return Err(NuonoError::NotFound(format!("模型 #{id}")));
            }
            return Ok(None);
        }
        ModelChange::Create { input, enabled } => ai_models::ActiveModel {
            name: Set(input.name.into()),
            provider: Set(input.provider.into()),
            provider_type: Set(input.provider_type.into()),
            model_id: Set(input.model_id.into()),
            api_key: Set(input.api_key.map(str::to_owned)),
            base_url: Set(input.base_url.into()),
            is_default: Set(input.is_default),
            enabled: Set(enabled),
            created_at: Set(crate::clock::store_time(Some(crate::clock::db_now()))),
            ..Default::default()
        },
        ModelChange::Update { id, patch } => {
            let existing = ai_models::Entity::find_by_id(id)
                .one(tx)
                .await?
                .ok_or_else(|| NuonoError::NotFound(format!("模型 #{id}")))?;
            let mut row: ai_models::ActiveModel = existing.into();
            if let Some(v) = patch.name {
                row.name = Set(v.into());
            }
            if let Some(v) = patch.provider {
                row.provider = Set(v.into());
            }
            if let Some(v) = patch.provider_type {
                row.provider_type = Set(v.into());
            }
            if let Some(v) = patch.model_id {
                row.model_id = Set(v.into());
            }
            if let Some(v) = patch.api_key {
                row.api_key = Set((!v.is_empty()).then(|| v.into()));
            }
            if let Some(v) = patch.base_url {
                row.base_url = Set(v.into());
            }
            if let Some(v) = patch.is_default {
                row.is_default = Set(v);
            }
            if let Some(v) = patch.enabled {
                row.enabled = Set(v);
            }
            row
        }
    };
    if row.name.as_ref().trim().is_empty() || row.model_id.as_ref().trim().is_empty() {
        return Err(NuonoError::Validation("模型名称和模型 ID 不能为空".into()));
    }
    if !matches!(
        row.provider_type.as_ref().as_str(),
        "openai_compatible" | "responses"
    ) {
        return Err(NuonoError::Validation(
            "请选择 Chat Completions 或 Responses 协议".into(),
        ));
    }
    if *row.is_default.as_ref() {
        ai_models::Entity::update_many()
            .set(ai_models::ActiveModel {
                is_default: Set(false),
                ..Default::default()
            })
            .filter(ai_models::Column::IsDefault.eq(true))
            .exec(tx)
            .await?;
        // 即使当前行原本就是默认模型也要显式置回 true：上方的全表重置语句会把当前行一并改为 false。
        row.is_default = Set(true);
    }
    row.updated_at = Set(crate::clock::store_time(Some(crate::clock::db_now())));
    Ok(Some(if row.id.is_not_set() {
        row.insert(tx).await?
    } else {
        row.update(tx).await?
    }))
}
