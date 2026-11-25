//! 基于宿主数据库接口实现的 SeaORM 原生连接与事务适配（`ConnectionTrait`）。
use super::{Database, PluginDataStore, PluginDataTransaction, PluginDatabaseBackend, Transaction};
use sea_orm::{ConnectionTrait, DbBackend, DbErr, ExecResult, QueryResult, Statement};
fn backend(value: PluginDatabaseBackend) -> DbBackend {
    match value {
        PluginDatabaseBackend::Postgres => DbBackend::Postgres,
        PluginDatabaseBackend::Sqlite => DbBackend::Sqlite,
        _ => DbBackend::Sqlite,
    }
}
#[async_trait::async_trait]
impl ConnectionTrait for Database {
    fn get_database_backend(&self) -> DbBackend {
        backend(self.backend())
    }
    async fn execute_raw(&self, statement: Statement) -> Result<ExecResult, DbErr> {
        PluginDataStore::orm_execute(self, statement).await
    }
    async fn execute_unprepared(&self, sql: &str) -> Result<ExecResult, DbErr> {
        PluginDataStore::orm_execute_unprepared(self, sql).await
    }
    async fn query_one_raw(&self, statement: Statement) -> Result<Option<QueryResult>, DbErr> {
        Ok(PluginDataStore::orm_query(self, statement)
            .await?
            .into_iter()
            .next())
    }
    async fn query_all_raw(&self, statement: Statement) -> Result<Vec<QueryResult>, DbErr> {
        PluginDataStore::orm_query(self, statement).await
    }
}
#[async_trait::async_trait]
impl ConnectionTrait for Transaction {
    fn get_database_backend(&self) -> DbBackend {
        backend(self.backend)
    }
    async fn execute_raw(&self, statement: Statement) -> Result<ExecResult, DbErr> {
        PluginDataTransaction::orm_execute(self, statement).await
    }
    async fn execute_unprepared(&self, sql: &str) -> Result<ExecResult, DbErr> {
        PluginDataTransaction::orm_execute_unprepared(self, sql).await
    }
    async fn query_one_raw(&self, statement: Statement) -> Result<Option<QueryResult>, DbErr> {
        Ok(PluginDataTransaction::orm_query(self, statement)
            .await?
            .into_iter()
            .next())
    }
    async fn query_all_raw(&self, statement: Statement) -> Result<Vec<QueryResult>, DbErr> {
        PluginDataTransaction::orm_query(self, statement).await
    }
}
