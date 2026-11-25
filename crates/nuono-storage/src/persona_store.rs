//! AI 角色卡（Persona）存储层适配器：向应用层暴露无底层 ORM 依赖的角色卡与群绑定接口。
use crate::Database;
use nuono_app::persona::{
    PersonaAssetKind, PersonaBinding, PersonaBindingKey, PersonaInput, PersonaRecord, PersonaStore,
};
use nuono_error::Result;

#[async_trait::async_trait]
impl PersonaStore for Database {
    async fn list_ai_personas(&self) -> Result<Vec<PersonaRecord>> {
        Database::list_ai_personas(self).await
    }
    async fn get_ai_persona(&self, id: &str) -> Result<Option<PersonaRecord>> {
        Database::get_ai_persona(self, id).await
    }
    async fn create_ai_persona(&self, id: &str, input: PersonaInput) -> Result<PersonaRecord> {
        Database::create_ai_persona(self, id, input).await
    }
    async fn update_ai_persona(
        &self,
        id: &str,
        expected_revision: i64,
        input: PersonaInput,
    ) -> Result<PersonaRecord> {
        Database::update_ai_persona(self, id, expected_revision, input).await
    }
    async fn delete_ai_persona(&self, id: &str, expected_revision: i64) -> Result<()> {
        Database::delete_ai_persona(self, id, expected_revision).await
    }
    async fn set_persona_asset(
        &self,
        id: &str,
        kind: PersonaAssetKind,
        filename: Option<String>,
        expected_revision: i64,
    ) -> Result<PersonaRecord> {
        Database::set_persona_asset(self, id, kind, filename, expected_revision).await
    }
    async fn list_ai_persona_bindings(&self) -> Result<Vec<PersonaBinding>> {
        Database::list_ai_persona_bindings(self).await
    }
    async fn upsert_ai_persona_binding(&self, binding: PersonaBinding) -> Result<()> {
        Database::upsert_ai_persona_binding(self, binding).await
    }
    async fn remove_ai_persona_binding(&self, key: &PersonaBindingKey) -> Result<()> {
        Database::remove_ai_persona_binding(self, key).await
    }
    async fn resolve_ai_persona(&self, key: &PersonaBindingKey) -> Result<Option<PersonaRecord>> {
        Database::resolve_ai_persona(self, key).await
    }
}
