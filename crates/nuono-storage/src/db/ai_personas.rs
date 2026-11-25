//! AI 角色卡（Persona）目录与群组精确绑定持久化。
//! 角色卡的文本配置与媒体资源变更均受乐观锁版本号（`revision`）保护；外键约束阻止删除仍绑定在群组上的角色卡。
use super::Database;
use crate::entity::{ai_persona_bindings, ai_personas};
use crate::error::{NuonoError, Result};
use nuono_app::persona::{
    PersonaAssetKind, PersonaBinding, PersonaBindingKey, PersonaInput, PersonaRecord,
    validate_asset_filename, validate_persona_id,
};
use sea_orm::sea_query::{OnConflict, Query};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DbBackend, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect, Set,
};

impl From<ai_personas::Model> for PersonaRecord {
    fn from(row: ai_personas::Model) -> Self {
        Self {
            id: row.id,
            name: row.name,
            description: row.description,
            system_prompt: row.system_prompt,
            enabled: row.enabled,
            avatar_filename: row.avatar_filename,
            portrait_filename: row.portrait_filename,
            reference_audio_filename: row.reference_audio_filename,
            reference_text: row.reference_text,
            language: row.language,
            tts_provider: row.tts_provider,
            voice_id: row.voice_id,
            revision: row.revision,
            updated_at: crate::clock::format_wire_ts(row.updated_at),
        }
    }
}

impl From<ai_persona_bindings::Model> for PersonaBinding {
    fn from(row: ai_persona_bindings::Model) -> Self {
        Self {
            platform: row.platform,
            bot_id: row.bot_id,
            group_id: row.group_id,
            namespace: row.namespace,
            persona_id: row.persona_id,
        }
    }
}

fn revision_after(expected: i64) -> Result<i64> {
    if expected < 1 {
        return Err(NuonoError::Validation("角色版本无效".into()));
    }
    expected
        .checked_add(1)
        .ok_or_else(|| NuonoError::Conflict("角色版本已达上限".into()))
}

fn write_error(error: sea_orm::DbErr) -> NuonoError {
    match error.sql_err() {
        Some(sea_orm::SqlErr::UniqueConstraintViolation(_)) => {
            NuonoError::Conflict("角色或群绑定已存在".into())
        }
        Some(sea_orm::SqlErr::ForeignKeyConstraintViolation(_)) => {
            NuonoError::Conflict("角色的群绑定已发生变化，请刷新后重试".into())
        }
        _ => error.into(),
    }
}

impl Database {
    async fn locked_persona(
        &self,
        id: &str,
        tx: &sea_orm::DatabaseTransaction,
    ) -> Result<ai_personas::Model> {
        let mut query =
            ai_personas::Entity::find_by_id(id).filter(ai_personas::Column::OwnerUserId.is_null());
        if self.backend() == DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        query
            .one(tx)
            .await?
            .ok_or_else(|| NuonoError::NotFound("角色不存在".into()))
    }

    pub async fn list_ai_personas(&self) -> Result<Vec<PersonaRecord>> {
        if !self.is_ai_module_installed().await? {
            return Ok(Vec::new());
        }
        Ok(ai_personas::Entity::find()
            .filter(ai_personas::Column::OwnerUserId.is_null())
            .order_by_asc(ai_personas::Column::Name)
            .order_by_asc(ai_personas::Column::Id)
            .all(self.orm())
            .await?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    pub async fn get_ai_persona(&self, id: &str) -> Result<Option<PersonaRecord>> {
        validate_persona_id(id)?;
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        Ok(ai_personas::Entity::find_by_id(id)
            .filter(ai_personas::Column::OwnerUserId.is_null())
            .one(self.orm())
            .await?
            .map(Into::into))
    }

    pub async fn create_ai_persona(&self, id: &str, input: PersonaInput) -> Result<PersonaRecord> {
        validate_persona_id(id)?;
        input.validate()?;
        self.ensure_ai_module().await?;
        let row = ai_personas::ActiveModel {
            id: Set(id.to_owned()),
            owner_user_id: Set(None),
            name: Set(input.name),
            description: Set(input.description),
            system_prompt: Set(input.system_prompt),
            user_address: Set(String::new()),
            enabled: Set(input.enabled),
            avatar_filename: Set(None),
            portrait_filename: Set(None),
            reference_audio_filename: Set(None),
            reference_text: Set(input.reference_text),
            language: Set(input.language),
            tts_provider: Set(input.tts_provider),
            voice_id: Set(input.voice_id),
            revision: Set(1),
            updated_at: Set(crate::clock::store_time(crate::clock::db_now())),
        }
        .insert(self.orm())
        .await
        .map_err(write_error)?;
        Ok(row.into())
    }

    pub async fn update_ai_persona(
        &self,
        id: &str,
        expected_revision: i64,
        input: PersonaInput,
    ) -> Result<PersonaRecord> {
        validate_persona_id(id)?;
        input.validate()?;
        self.ensure_ai_module().await?;
        let revision = revision_after(expected_revision)?;
        let tx = self.begin_write().await?;
        let current = self.locked_persona(id, &tx).await?;
        if current.revision != expected_revision {
            return Err(NuonoError::Conflict("角色已被修改，请刷新后重试".into()));
        }
        let row = ai_personas::ActiveModel {
            id: Set(id.to_owned()),
            name: Set(input.name),
            description: Set(input.description),
            system_prompt: Set(input.system_prompt),
            enabled: Set(input.enabled),
            reference_text: Set(input.reference_text),
            language: Set(input.language),
            tts_provider: Set(input.tts_provider),
            voice_id: Set(input.voice_id),
            revision: Set(revision),
            updated_at: Set(crate::clock::store_time(crate::clock::db_now())),
            ..Default::default()
        }
        .update(&tx)
        .await
        .map_err(write_error)?;
        tx.commit().await?;
        Ok(row.into())
    }

    pub async fn delete_ai_persona(&self, id: &str, expected_revision: i64) -> Result<()> {
        validate_persona_id(id)?;
        revision_after(expected_revision)?;
        self.ensure_ai_module().await?;
        let tx = self.begin_write().await?;
        let current = self.locked_persona(id, &tx).await?;
        if current.revision != expected_revision {
            return Err(NuonoError::Conflict("角色已被修改，请刷新后重试".into()));
        }
        if ai_persona_bindings::Entity::find()
            .filter(ai_persona_bindings::Column::PersonaId.eq(id))
            .count(&tx)
            .await?
            > 0
        {
            return Err(NuonoError::Conflict(
                "请先解除此角色的群绑定，再删除角色".into(),
            ));
        }
        ai_personas::Entity::delete_by_id(id)
            .exec(&tx)
            .await
            .map_err(write_error)?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_persona_asset(
        &self,
        id: &str,
        kind: PersonaAssetKind,
        filename: Option<String>,
        expected_revision: i64,
    ) -> Result<PersonaRecord> {
        validate_persona_id(id)?;
        if let Some(filename) = &filename {
            validate_asset_filename(kind, filename)?;
        }
        let revision = revision_after(expected_revision)?;
        self.ensure_ai_module().await?;
        let tx = self.begin_write().await?;
        let current = self.locked_persona(id, &tx).await?;
        if current.revision != expected_revision {
            return Err(NuonoError::Conflict("角色已被修改，请刷新后重试".into()));
        }
        let mut model = ai_personas::ActiveModel {
            id: Set(id.to_owned()),
            revision: Set(revision),
            updated_at: Set(crate::clock::store_time(crate::clock::db_now())),
            ..Default::default()
        };
        match kind {
            PersonaAssetKind::Avatar => model.avatar_filename = Set(filename),
            PersonaAssetKind::Portrait => model.portrait_filename = Set(filename),
            PersonaAssetKind::ReferenceAudio => model.reference_audio_filename = Set(filename),
        }
        let row = model.update(&tx).await.map_err(write_error)?;
        tx.commit().await?;
        Ok(row.into())
    }

    pub async fn list_ai_persona_bindings(&self) -> Result<Vec<PersonaBinding>> {
        if !self.is_ai_module_installed().await? {
            return Ok(Vec::new());
        }
        Ok(ai_persona_bindings::Entity::find()
            .order_by_asc(ai_persona_bindings::Column::Platform)
            .order_by_asc(ai_persona_bindings::Column::BotId)
            .order_by_asc(ai_persona_bindings::Column::GroupId)
            .order_by_asc(ai_persona_bindings::Column::TargetKey)
            .all(self.orm())
            .await?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    pub async fn upsert_ai_persona_binding(&self, binding: PersonaBinding) -> Result<()> {
        binding.validate()?;
        self.ensure_ai_module().await?;
        let tx = self.begin_write().await?;
        // 对目标角色卡加锁，配合外键 ON DELETE RESTRICT 使「绑定群组」与「删除角色卡」严格串行化。
        self.locked_persona(&binding.persona_id, &tx).await?;
        let target_key = binding.key().canonical_key();
        ai_persona_bindings::Entity::insert(ai_persona_bindings::ActiveModel {
            target_key: Set(target_key),
            platform: Set(binding.platform),
            bot_id: Set(binding.bot_id),
            group_id: Set(binding.group_id),
            namespace: Set(binding.namespace),
            persona_id: Set(binding.persona_id),
        })
        .on_conflict(
            OnConflict::column(ai_persona_bindings::Column::TargetKey)
                .update_column(ai_persona_bindings::Column::PersonaId)
                .to_owned(),
        )
        .exec(&tx)
        .await
        .map_err(write_error)?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn remove_ai_persona_binding(&self, key: &PersonaBindingKey) -> Result<()> {
        key.validate()?;
        if !self.is_ai_module_installed().await? {
            return Ok(());
        }
        ai_persona_bindings::Entity::delete_by_id(key.canonical_key())
            .exec(self.orm())
            .await?;
        Ok(())
    }

    /// 按管理员界面当前看到的绑定关系执行条件解绑：若期间该群组已被并发改绑到其他角色卡，则拒绝删除并提示刷新。
    pub async fn remove_ai_persona_binding_if(
        &self,
        key: &PersonaBindingKey,
        expected_persona_id: &str,
    ) -> Result<()> {
        key.validate()?;
        validate_persona_id(expected_persona_id)?;
        if !self.is_ai_module_installed().await? {
            return Ok(());
        }
        let target_key = key.canonical_key();
        let tx = self.begin_write().await?;
        let mut query = ai_persona_bindings::Entity::find_by_id(&target_key);
        if self.backend() == DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        if let Some(current) = query.one(&tx).await? {
            if current.persona_id != expected_persona_id {
                return Err(NuonoError::Conflict(
                    "群组已分配给其他角色，请刷新后重试".into(),
                ));
            }
            let deleted = ai_persona_bindings::Entity::delete_many()
                .filter(ai_persona_bindings::Column::TargetKey.eq(target_key))
                .filter(ai_persona_bindings::Column::PersonaId.eq(expected_persona_id))
                .exec(&tx)
                .await?;
            if deleted.rows_affected != 1 {
                return Err(NuonoError::Conflict(
                    "群组角色分配已发生变化，请刷新后重试".into(),
                ));
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn resolve_ai_persona(
        &self,
        key: &PersonaBindingKey,
    ) -> Result<Option<PersonaRecord>> {
        key.validate()?;
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        // 使用单条带子查询的 SQL 在同一快照内完成绑定关系查找与已启用角色卡的解析。
        let mut binding = Query::select();
        binding
            .column(ai_persona_bindings::Column::PersonaId)
            .from(ai_persona_bindings::Entity)
            .and_where(ai_persona_bindings::Column::TargetKey.eq(key.canonical_key()))
            .and_where(ai_persona_bindings::Column::Platform.eq(&key.platform))
            .and_where(ai_persona_bindings::Column::BotId.eq(&key.bot_id))
            .and_where(ai_persona_bindings::Column::GroupId.eq(&key.group_id));
        if let Some(namespace) = &key.namespace {
            binding.and_where(ai_persona_bindings::Column::Namespace.eq(namespace));
        } else {
            binding.and_where(ai_persona_bindings::Column::Namespace.is_null());
        }
        Ok(ai_personas::Entity::find()
            .filter(ai_personas::Column::OwnerUserId.is_null())
            .filter(ai_personas::Column::Enabled.eq(true))
            .filter(ai_personas::Column::Id.in_subquery(binding.to_owned()))
            .one(self.orm())
            .await?
            .map(Into::into))
    }
}
