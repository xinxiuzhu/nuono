//! 个人专属陪伴角色卡持久化：保存在 `nuono_ai_personas` 表中（通过 `owner_user_id = Some(user_id)` 与公共角色卡区分）。
use nuono_app::{
    companion::{CompanionCard, CompanionInput, MAX_COMPANION_CARDS},
    persona::{PersonaAssetKind, validate_asset_filename, validate_persona_id},
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DbBackend, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect, Set,
};

use super::{Database, STATUS_ACTIVE};
use crate::{
    entity::{account_assistant as accounts, ai_personas as cards, users::orm as users},
    error::{NuonoError, Result},
};

fn to_companion_card(row: cards::Model) -> Result<CompanionCard> {
    let user_id = row
        .owner_user_id
        .ok_or_else(|| NuonoError::NotFound("角色卡不存在".into()))?;
    Ok(CompanionCard {
        id: row.id,
        user_id,
        name: row.name,
        description: row.description,
        system_prompt: row.system_prompt,
        user_address: row.user_address,
        avatar_filename: row.avatar_filename,
        portrait_filename: row.portrait_filename,
        reference_audio_filename: row.reference_audio_filename,
        reference_text: row.reference_text,
        language: row.language,
        revision: row.revision,
        updated_at: crate::clock::format_wire_ts(row.updated_at),
    })
}

fn input_model(user_id: i64, id: &str, input: CompanionInput) -> cards::ActiveModel {
    cards::ActiveModel {
        id: Set(id.to_owned()),
        owner_user_id: Set(Some(user_id)),
        name: Set(input.name),
        description: Set(input.description),
        system_prompt: Set(input.system_prompt),
        user_address: Set(input.user_address),
        enabled: Set(true),
        reference_text: Set(input.reference_text),
        language: Set(input.language),
        revision: Set(1),
        updated_at: Set(crate::clock::store_time(crate::clock::db_now())),
        ..Default::default()
    }
}

fn next_revision(expected: i64) -> Result<i64> {
    if expected < 1 {
        return Err(NuonoError::Validation("角色卡版本无效".into()));
    }
    expected
        .checked_add(1)
        .ok_or_else(|| NuonoError::Conflict("角色卡版本已达上限".into()))
}

impl Database {
    async fn lock_companion_owner(
        &self,
        tx: &sea_orm::DatabaseTransaction,
        user_id: i64,
    ) -> Result<()> {
        let mut query =
            users::Entity::find_by_id(user_id).filter(users::Column::Status.eq(STATUS_ACTIVE));
        if self.backend() == DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        if query.one(tx).await?.is_none() {
            return Err(NuonoError::NotFound("账号不存在或已停用".into()));
        }
        Ok(())
    }

    async fn select_companion_on(
        &self,
        tx: &sea_orm::DatabaseTransaction,
        user_id: i64,
        card_id: &str,
    ) -> Result<()> {
        let exists = accounts::Entity::find_by_id(user_id)
            .one(tx)
            .await?
            .is_some();
        let row = accounts::ActiveModel {
            user_id: Set(user_id),
            active_card_id: Set(Some(card_id.to_owned())),
            ..Default::default()
        };
        if exists {
            row.update(tx).await?;
        } else {
            row.insert(tx).await?;
        }
        Ok(())
    }

    pub async fn list_companion_cards(&self, user_id: i64) -> Result<Vec<CompanionCard>> {
        if !self.is_ai_module_installed().await? {
            return Ok(Vec::new());
        }
        cards::Entity::find()
            .filter(cards::Column::OwnerUserId.eq(user_id))
            .order_by_asc(cards::Column::UpdatedAt)
            .order_by_asc(cards::Column::Id)
            .all(self.orm())
            .await?
            .into_iter()
            .map(to_companion_card)
            .collect()
    }

    pub async fn companion_card(&self, user_id: i64, id: &str) -> Result<CompanionCard> {
        validate_persona_id(id)?;
        if !self.is_ai_module_installed().await? {
            return Err(NuonoError::NotFound("角色卡不存在".into()));
        }
        let row = cards::Entity::find_by_id(id)
            .filter(cards::Column::OwnerUserId.eq(user_id))
            .one(self.orm())
            .await?
            .ok_or_else(|| NuonoError::NotFound("角色卡不存在".into()))?;
        to_companion_card(row)
    }

    pub async fn active_companion_card(&self, user_id: i64) -> Result<Option<CompanionCard>> {
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        let Some(id) = accounts::Entity::find_by_id(user_id)
            .one(self.orm())
            .await?
            .and_then(|row| row.active_card_id)
        else {
            return Ok(None);
        };
        self.companion_card(user_id, &id).await.map(Some)
    }

    /// 在与角色卡切换相同的账号行锁保护下，为该账号幂等创建唯一的初始默认陪伴角色卡。
    pub async fn ensure_default_companion_card(&self, user_id: i64) -> Result<CompanionCard> {
        self.ensure_ai_module().await?;
        let tx = self.begin_write().await?;
        self.lock_companion_owner(&tx, user_id).await?;
        if let Some(id) = accounts::Entity::find_by_id(user_id)
            .one(&tx)
            .await?
            .and_then(|row| row.active_card_id)
        {
            let card = cards::Entity::find_by_id(id)
                .filter(cards::Column::OwnerUserId.eq(user_id))
                .one(&tx)
                .await?
                .ok_or_else(|| NuonoError::NotFound("当前角色卡不存在".into()))?;
            tx.commit().await?;
            return to_companion_card(card);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let card = input_model(user_id, &id, CompanionInput::default())
            .insert(&tx)
            .await?;
        self.select_companion_on(&tx, user_id, &id).await?;
        tx.commit().await?;
        to_companion_card(card)
    }

    pub async fn create_companion_card(
        &self,
        user_id: i64,
        input: CompanionInput,
    ) -> Result<CompanionCard> {
        input.validate()?;
        self.ensure_ai_module().await?;
        let tx = self.begin_write().await?;
        self.lock_companion_owner(&tx, user_id).await?;
        if cards::Entity::find()
            .filter(cards::Column::OwnerUserId.eq(user_id))
            .count(&tx)
            .await?
            >= MAX_COMPANION_CARDS
        {
            return Err(NuonoError::Validation(format!(
                "最多保存 {MAX_COMPANION_CARDS} 张角色卡"
            )));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let card = input_model(user_id, &id, input).insert(&tx).await?;
        let current = accounts::Entity::find_by_id(user_id)
            .one(&tx)
            .await?
            .and_then(|row| row.active_card_id);
        if current.is_none() {
            self.select_companion_on(&tx, user_id, &id).await?;
        }
        tx.commit().await?;
        to_companion_card(card)
    }

    pub async fn activate_companion_card(&self, user_id: i64, id: &str) -> Result<CompanionCard> {
        validate_persona_id(id)?;
        self.ensure_ai_module().await?;
        let tx = self.begin_write().await?;
        self.lock_companion_owner(&tx, user_id).await?;
        let card = cards::Entity::find_by_id(id)
            .filter(cards::Column::OwnerUserId.eq(user_id))
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::NotFound("角色卡不存在".into()))?;
        self.select_companion_on(&tx, user_id, id).await?;
        tx.commit().await?;
        to_companion_card(card)
    }

    pub async fn update_companion_card(
        &self,
        user_id: i64,
        id: &str,
        expected_revision: i64,
        input: CompanionInput,
    ) -> Result<CompanionCard> {
        validate_persona_id(id)?;
        input.validate()?;
        let revision = next_revision(expected_revision)?;
        self.ensure_ai_module().await?;
        let tx = self.begin_write().await?;
        self.lock_companion_owner(&tx, user_id).await?;
        let current = cards::Entity::find_by_id(id)
            .filter(cards::Column::OwnerUserId.eq(user_id))
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::NotFound("角色卡不存在".into()))?;
        if current.revision != expected_revision {
            return Err(NuonoError::Conflict(
                "角色卡已更新，请重新读取后再保存".into(),
            ));
        }
        let mut model = input_model(user_id, id, input);
        model.revision = Set(revision);
        let card = model.update(&tx).await?;
        tx.commit().await?;
        to_companion_card(card)
    }

    pub async fn set_companion_asset(
        &self,
        user_id: i64,
        id: &str,
        kind: PersonaAssetKind,
        filename: Option<String>,
        expected_revision: i64,
    ) -> Result<CompanionCard> {
        validate_persona_id(id)?;
        if let Some(filename) = &filename {
            validate_asset_filename(kind, filename)?;
        }
        let revision = next_revision(expected_revision)?;
        self.ensure_ai_module().await?;
        let tx = self.begin_write().await?;
        self.lock_companion_owner(&tx, user_id).await?;
        let current = cards::Entity::find_by_id(id)
            .filter(cards::Column::OwnerUserId.eq(user_id))
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::NotFound("角色卡不存在".into()))?;
        if current.revision != expected_revision {
            return Err(NuonoError::Conflict(
                "角色卡已更新，请重新读取后再保存".into(),
            ));
        }
        let mut model = cards::ActiveModel {
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
        let card = model.update(&tx).await?;
        tx.commit().await?;
        to_companion_card(card)
    }
}

#[cfg(test)]
#[path = "../../tests/unit/db/companion.rs"]
mod tests;
