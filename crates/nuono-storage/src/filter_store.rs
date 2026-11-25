use crate::{Database, error::Result};
use nuono_app::filter::FilterStore;
use serde_json::Value;

#[async_trait::async_trait]
impl FilterStore for Database {
    async fn is_blacklisted(&self, kind: &str, id: &str) -> Result<bool> {
        Database::is_blacklisted(self, kind, id).await
    }
    async fn is_group_enabled(&self, group: &str) -> Result<bool> {
        Database::is_group_enabled(self, group).await
    }
    async fn get_filter_rules(&self, kind: &str) -> Result<Vec<Value>> {
        Database::get_filter_rules(self, kind).await
    }
    async fn add_log(&self, level: &str, bot: &str, message: &str) -> Result<()> {
        Database::add_log(self, level, bot, message).await
    }
}
