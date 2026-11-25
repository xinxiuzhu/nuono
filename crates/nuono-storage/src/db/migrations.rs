//! 框架核心与可选模块迁移执行台账：统一校验 PostgreSQL 与 SQLite 双端建表 SQL 的归一化校验和。

use nuono_app::plugin_manifest::migration_checksum;
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, SqliteTransactionMode,
    Statement, TransactionOptions, TransactionTrait,
};

use crate::error::{NuonoError, Result};

const MIGRATION_LOCK_ID: i64 = 0x4e55_4f4e_4f4d_4947;
const FRAMEWORK_MIGRATIONS: &[(&str, &str, &str)] = &[(
    "m0001_baseline",
    include_str!("../../migrations/m0001_baseline.sql"),
    include_str!("../../migrations_sqlite/m0001_baseline.sql"),
)];
const AI_MODULE_MIGRATIONS: &[(&str, &str, &str)] = &[(
    "ai_m0001_baseline",
    include_str!("../../migrations/ai_m0001_baseline.sql"),
    include_str!("../../migrations_sqlite/ai_m0001_baseline.sql"),
)];

/// 供数据库初始化与跨数据库可移植迁移使用的框架核心双端不可变建表 SQL 定义。
pub fn framework_migration_sources() -> Vec<(&'static str, &'static str, &'static str)> {
    FRAMEWORK_MIGRATIONS.to_vec()
}

/// 按需安装的 AI/Agent/陪伴可选模块双端不可变建表 SQL 定义。
pub fn ai_module_migration_sources() -> Vec<(&'static str, &'static str, &'static str)> {
    AI_MODULE_MIGRATIONS.to_vec()
}

pub(crate) fn framework_checksum(index: usize) -> Result<String> {
    let (name, postgres, sqlite) = FRAMEWORK_MIGRATIONS[index];
    migration_checksum(
        (index + 1) as i64,
        name,
        &[("postgres", postgres), ("sqlite", sqlite)],
    )
}

pub(crate) fn ai_module_checksum(index: usize) -> Result<String> {
    let (name, postgres, sqlite) = AI_MODULE_MIGRATIONS[index];
    migration_checksum(
        (index + 1) as i64,
        name,
        &[("postgres", postgres), ("sqlite", sqlite)],
    )
}

pub(crate) async fn run_framework(conn: &DatabaseConnection) -> Result<bool> {
    let transaction = migration_transaction(conn).await?;
    let names_sql = match conn.get_database_backend() {
        DbBackend::Sqlite => {
            "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*'"
        }
        DbBackend::Postgres => {
            "SELECT table_name AS name FROM information_schema.tables WHERE table_schema='public' AND table_type='BASE TABLE'"
        }
        backend => return Err(unsupported_backend(backend)),
    };
    let tables = transaction
        .query_all_raw(Statement::from_string(
            conn.get_database_backend(),
            names_sql,
        ))
        .await?
        .into_iter()
        .map(|r| r.try_get::<String>("", "name"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let has_ledger = tables.iter().any(|name| name == "nuono_schema_migrations");
    if !has_ledger && !tables.is_empty() {
        return Err(old_schema());
    }
    let mut applied_core = std::collections::BTreeMap::new();
    let mut applied_ai = std::collections::BTreeMap::new();
    if has_ledger {
        let rows = transaction
            .query_all_raw(Statement::from_string(
                conn.get_database_backend(),
                "SELECT module,version,description,checksum FROM nuono_schema_migrations ORDER BY module,version",
            ))
            .await
            .map_err(|_| old_schema())?;
        for row in rows {
            let module = row
                .try_get::<String>("", "module")
                .map_err(|_| old_schema())?;
            let version = row
                .try_get::<i64>("", "version")
                .map_err(|_| old_schema())?;
            match module.as_str() {
                "core" => {
                    applied_core.insert(version, row);
                }
                "ai" => {
                    applied_ai.insert(version, row);
                }
                _ => {
                    return Err(NuonoError::DatabaseConnection(
                        "数据库包含未知框架模块版本".into(),
                    ));
                }
            }
        }
        if applied_core.len() > FRAMEWORK_MIGRATIONS.len()
            || applied_core
                .keys()
                .any(|v| *v <= 0 || *v as usize > FRAMEWORK_MIGRATIONS.len())
            || applied_ai.len() > AI_MODULE_MIGRATIONS.len()
            || applied_ai
                .keys()
                .any(|v| *v <= 0 || *v as usize > AI_MODULE_MIGRATIONS.len())
        {
            return Err(NuonoError::DatabaseConnection(
                "数据库结构版本高于或不匹配当前程序".into(),
            ));
        }
        let operation = transaction
            .query_one_raw(Statement::from_string(
                conn.get_database_backend(),
                "SELECT maintenance_operation FROM nuono_deployment WHERE id=1",
            ))
            .await
            .map_err(|_| old_schema())?
            .map(|row| row.try_get::<Option<String>>("", "maintenance_operation"))
            .transpose()?
            .flatten();
        if operation.is_some() {
            return Err(NuonoError::DatabaseConnection(
                "数据库处于持久维护状态；必须使用原操作日志恢复，不能启动普通宿主".into(),
            ));
        }
    }
    for (index, (name, postgres, sqlite)) in FRAMEWORK_MIGRATIONS.iter().enumerate() {
        let version = i64::try_from(index + 1).map_err(|e| NuonoError::Internal(e.to_string()))?;
        let digest = framework_checksum(index)?;
        if let Some(row) = applied_core.get(&version) {
            if row.try_get::<String>("", "description")? != *name
                || row.try_get::<String>("", "checksum")? != digest
            {
                return Err(NuonoError::DatabaseConnection(format!(
                    "框架已应用版本 {version} 的声明被修改；请使用匹配程序，未发布的开发库可在停止宿主后清理重建"
                )));
            }
            continue;
        }
        if applied_core.keys().any(|v| *v > version) {
            return Err(NuonoError::DatabaseConnection(
                "框架迁移账本存在版本空洞".into(),
            ));
        }
        transaction
            .execute_unprepared(match conn.get_database_backend() {
                DbBackend::Postgres => postgres,
                DbBackend::Sqlite => sqlite,
                _ => unreachable!(),
            })
            .await?;
        transaction.execute_raw(Statement::from_sql_and_values(conn.get_database_backend(),
            "INSERT INTO nuono_schema_migrations(module,version,description,checksum,applied_at_ms) VALUES('core',$1,$2,$3,$4)",
            [version.into(),(*name).into(),digest.into(),chrono::Utc::now().timestamp_millis().into()])).await?;
    }
    let ai_installed = if applied_ai.is_empty() {
        false
    } else {
        for (index, (name, postgres, sqlite)) in AI_MODULE_MIGRATIONS.iter().enumerate() {
            let version =
                i64::try_from(index + 1).map_err(|e| NuonoError::Internal(e.to_string()))?;
            let digest = ai_module_checksum(index)?;
            if let Some(row) = applied_ai.get(&version) {
                if row.try_get::<String>("", "description")? != *name
                    || row.try_get::<String>("", "checksum")? != digest
                {
                    return Err(NuonoError::DatabaseConnection(format!(
                        "AI 模块已应用版本 {version} 的声明被修改；请使用匹配程序，未发布的开发库可在停止宿主后清理重建"
                    )));
                }
                continue;
            }
            if applied_ai.keys().any(|v| *v > version) {
                return Err(NuonoError::DatabaseConnection(
                    "AI 模块迁移账本存在版本空洞".into(),
                ));
            }
            transaction
                .execute_unprepared(match conn.get_database_backend() {
                    DbBackend::Postgres => postgres,
                    DbBackend::Sqlite => sqlite,
                    _ => unreachable!(),
                })
                .await?;
            transaction.execute_raw(Statement::from_sql_and_values(conn.get_database_backend(),
                "INSERT INTO nuono_schema_migrations(module,version,description,checksum,applied_at_ms) VALUES('ai',$1,$2,$3,$4)",
                [version.into(),(*name).into(),digest.into(),chrono::Utc::now().timestamp_millis().into()])).await?;
        }
        true
    };
    transaction.execute_raw(Statement::from_sql_and_values(conn.get_database_backend(),
        "INSERT INTO nuono_deployment(id,deployment_id,created_at_ms) VALUES(1,$1,$2) ON CONFLICT(id) DO NOTHING",
        [uuid::Uuid::new_v4().to_string().into(),chrono::Utc::now().timestamp_millis().into()])).await?;
    transaction.commit().await?;
    Ok(ai_installed)
}

pub(crate) async fn ensure_ai_module(conn: &DatabaseConnection) -> Result<()> {
    let transaction = migration_transaction(conn).await?;
    let rows = transaction
        .query_all_raw(Statement::from_string(
            conn.get_database_backend(),
            "SELECT version,description,checksum FROM nuono_schema_migrations WHERE module='ai' ORDER BY version",
        ))
        .await?;
    let mut applied = std::collections::BTreeMap::new();
    for row in rows {
        applied.insert(row.try_get::<i64>("", "version")?, row);
    }
    if applied.len() > AI_MODULE_MIGRATIONS.len()
        || applied
            .keys()
            .any(|v| *v <= 0 || *v as usize > AI_MODULE_MIGRATIONS.len())
    {
        return Err(NuonoError::DatabaseConnection(
            "AI 模块结构版本高于或不匹配当前程序".into(),
        ));
    }
    for (index, (name, postgres, sqlite)) in AI_MODULE_MIGRATIONS.iter().enumerate() {
        let version = i64::try_from(index + 1).map_err(|e| NuonoError::Internal(e.to_string()))?;
        let digest = ai_module_checksum(index)?;
        if let Some(row) = applied.get(&version) {
            if row.try_get::<String>("", "description")? != *name
                || row.try_get::<String>("", "checksum")? != digest
            {
                return Err(NuonoError::DatabaseConnection(format!(
                    "AI 模块已应用版本 {version} 的声明被修改"
                )));
            }
            continue;
        }
        transaction
            .execute_unprepared(match conn.get_database_backend() {
                DbBackend::Postgres => postgres,
                DbBackend::Sqlite => sqlite,
                _ => unreachable!(),
            })
            .await?;
        transaction.execute_raw(Statement::from_sql_and_values(conn.get_database_backend(),
            "INSERT INTO nuono_schema_migrations(module,version,description,checksum,applied_at_ms) VALUES('ai',$1,$2,$3,$4)",
            [version.into(),(*name).into(),digest.into(),chrono::Utc::now().timestamp_millis().into()])).await?;
    }
    transaction.commit().await?;
    Ok(())
}

pub(crate) async fn is_ai_module_installed<C: ConnectionTrait>(conn: &C) -> Result<bool> {
    let row = conn
        .query_one_raw(Statement::from_string(
            conn.get_database_backend(),
            "SELECT 1 AS present FROM nuono_schema_migrations WHERE module='ai' LIMIT 1",
        ))
        .await?;
    Ok(row.is_some())
}

fn old_schema() -> NuonoError {
    NuonoError::DatabaseConnection(
        "检测到未发布的旧开发数据库；请停止宿主并删除旧开发库，再从空库初始化首发结构".into(),
    )
}

pub(crate) async fn migration_transaction(
    conn: &DatabaseConnection,
) -> Result<DatabaseTransaction> {
    let transaction = match conn.get_database_backend() {
        DbBackend::Postgres => conn.begin().await?,
        DbBackend::Sqlite => {
            conn.begin_with_options(TransactionOptions {
                isolation_level: None,
                access_mode: None,
                sqlite_transaction_mode: Some(SqliteTransactionMode::Immediate),
            })
            .await?
        }
        backend => return Err(unsupported_backend(backend)),
    };
    if conn.get_database_backend() == DbBackend::Postgres {
        transaction
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT pg_advisory_xact_lock($1)",
                [MIGRATION_LOCK_ID.into()],
            ))
            .await?;
    }
    Ok(transaction)
}

fn unsupported_backend(backend: DbBackend) -> NuonoError {
    NuonoError::NotImplemented(format!("迁移不支持后端 {backend:?}"))
}

#[cfg(test)]
#[path = "../../tests/unit/db/locale_baseline_tests.rs"]
mod locale_baseline_tests;

#[cfg(test)]
#[path = "../../tests/unit/db/message_history_baseline_tests.rs"]
mod message_history_baseline_tests;

#[cfg(test)]
#[path = "../../tests/unit/db/sqlite_baseline_tests.rs"]
mod sqlite_baseline_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn old_development_schema_is_rejected_without_changes() {
        let connection = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        connection.execute_unprepared("CREATE TABLE users (id INTEGER PRIMARY KEY, note TEXT); INSERT INTO users VALUES (1, 'retain')").await.unwrap();
        assert!(run_framework(&connection).await.is_err());
        let row = connection
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT note FROM users WHERE id = 1",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<String>("", "note").unwrap(), "retain");
        let created = connection
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT name FROM sqlite_master WHERE name LIKE 'nuono_%'",
            ))
            .await
            .unwrap();
        assert!(
            created.is_empty(),
            "refusal must not create a parallel core schema"
        );
        connection.close().await.unwrap();
    }

    #[tokio::test]
    async fn sqlite_baseline_enforces_boolean_and_identity_constraints() {
        let connection = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        run_framework(&connection).await.unwrap();
        assert!(
            connection
                .execute_unprepared(
                    "INSERT INTO nuono_plugins(plugin_id, enabled,created_at_ms,updated_at_ms) VALUES ('invalid', 2,1,1)"
                )
                .await
                .is_err()
        );
        connection
            .execute_unprepared(
                "INSERT INTO nuono_plugins(plugin_id,enabled,created_at_ms,updated_at_ms) VALUES ('valid',1,1,1)",
            )
            .await
            .unwrap();
        connection.execute_unprepared("INSERT INTO nuono_users(id, username, username_key, password_hash, created_at_ms, updated_at_ms) VALUES (9007199254740993, 'large', 'large', 'test-hash', 1763164800000, 1763164800000)").await.unwrap();
        let id = connection
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT id FROM nuono_users WHERE username_key = 'large'",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<i64>("", "id")
            .unwrap();
        assert_eq!(id, 9_007_199_254_740_993);
        connection.execute_unprepared("DELETE FROM nuono_users WHERE username_key = 'large'; INSERT INTO nuono_users(username, username_key, password_hash, created_at_ms, updated_at_ms) VALUES ('next', 'next', 'test-hash', 1763164800000, 1763164800000)").await.unwrap();
        let next = connection
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT id FROM nuono_users WHERE username_key = 'next'",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<i64>("", "id")
            .unwrap();
        assert!(next > id, "deleted account numbers must not be reused");
        connection.close().await.unwrap();
    }

    #[tokio::test]
    async fn framework_baseline_preserves_plugin_registration_and_groups() {
        let db = crate::test_support::test_db(2).await;
        db.sync_plugin("existing", "1.2.3", &serde_json::json!({"retained":true}))
            .await
            .unwrap();
        db.set_plugin_enabled("existing", false).await.unwrap();
        db.orm().execute_unprepared("INSERT INTO nuono_groups(platform,conversation_kind,native_group_id,created_at_ms,updated_at_ms) VALUES('test','group','keep',1,1); INSERT INTO nuono_plugin_overrides(plugin_id,group_id,config_patch_json,created_at_ms,updated_at_ms) VALUES('existing',1,'{}',1,1)").await.unwrap();
        let schema = crate::portable::registered_schema(db.orm()).await.unwrap();
        let before = crate::portable::export(db.orm(), &schema).await.unwrap();
        run_framework(db.orm()).await.unwrap();
        crate::portable::verify(db.orm(), &schema, &before)
            .await
            .unwrap();
        assert_eq!(
            db.get_plugin_slow_threshold("existing").await.unwrap(),
            None
        );
        db.set_plugin_slow_threshold("existing", 45_000)
            .await
            .unwrap();
        run_framework(db.orm()).await.unwrap();
        assert_eq!(
            db.get_plugin_slow_threshold("existing").await.unwrap(),
            Some(45_000)
        );
        let ledger = db
            .orm()
            .query_all_raw(Statement::from_string(
                db.backend(),
                "SELECT version FROM nuono_schema_migrations ORDER BY version",
            ))
            .await
            .unwrap();
        assert_eq!(
            ledger
                .iter()
                .map(|row| row.try_get::<i64>("", "version").unwrap())
                .collect::<Vec<_>>(),
            [1_i64]
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn framework_baseline_creates_embedding_cache_and_preserves_records() {
        use nuono_ai::embedding::EmbeddingModel;
        use nuono_app::agent::store::{AgentMemoryRecord, MemoryEmbeddingRecord};
        use sha2::{Digest, Sha256};

        // test_db 总是创建独立的 nuono_test_<uuid> 临时测试库或文件，绝不会连接正式应用数据库或读取真实用户配置。
        let db = crate::test_support::test_db(2).await;
        let original = db
            .upsert_agent_memory(AgentMemoryRecord {
                scope_type: "user".into(),
                scope_key: "migration-fixture".into(),
                memory_key: "preference".into(),
                content: "I prefer EarlGrey tea.".into(),
                tags_json: Some("[\"pinned\"]".into()),
                enabled: true,
                expires_at: None,
                actor: Some("migration-fixture".into()),
            })
            .await
            .unwrap()
            .0;
        let model = EmbeddingModel {
            id: 42,
            model: "fixture-embedding".into(),
            revision: "migration-v1".into(),
        };
        let cache = MemoryEmbeddingRecord {
            memory_id: original.id,
            model: model.clone(),
            content_fingerprint: hex::encode(Sha256::digest(original.content.as_bytes())),
            vector: vec![1.0, 0.0],
        };
        assert_eq!(
            db.save_memory_embeddings(std::slice::from_ref(&cache))
                .await
                .unwrap(),
            1
        );
        let read = db.memory_embeddings(&[original.id], &model).await.unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].content_fingerprint, cache.content_fingerprint);
        assert_eq!(read[0].vector, cache.vector);

        // 重复执行框架迁移应是幂等空操作，不会改动已写入的记忆或向量缓存。
        run_framework(db.orm()).await.unwrap();
        assert_eq!(
            db.memory_embeddings(&[original.id], &model).await.unwrap()[0].vector,
            cache.vector
        );
        assert_eq!(
            db.get_agent_memory(original.id).await.unwrap().unwrap(),
            original
        );
        db.shutdown().await.unwrap();
    }

    #[test]
    fn postgres_baselines_enforce_bigint_for_timestamps_and_integer_pks() {
        for (module, sources) in [("core", FRAMEWORK_MIGRATIONS), ("ai", AI_MODULE_MIGRATIONS)] {
            for (name, postgres_sql, _) in sources {
                let mut current_table: Option<String> = None;
                for raw_line in postgres_sql.lines() {
                    let line = raw_line.trim();
                    if line.is_empty() || line.starts_with("--") {
                        continue;
                    }
                    if let Some(rest) = line.strip_prefix("CREATE TABLE ") {
                        let table = rest
                            .split(|c: char| c.is_whitespace() || c == '(')
                            .next()
                            .expect("CREATE TABLE 缺少表名");
                        current_table = Some(table.to_owned());
                        continue;
                    }
                    if line.starts_with(");") {
                        current_table = None;
                        continue;
                    }
                    let Some(table) = current_table.as_deref() else {
                        continue;
                    };
                    let upper = line.to_ascii_uppercase();
                    if upper.starts_with("CHECK")
                        || upper.starts_with("UNIQUE")
                        || upper.starts_with("PRIMARY KEY")
                        || upper.starts_with("FOREIGN KEY")
                        || upper.starts_with("CONSTRAINT")
                    {
                        continue;
                    }
                    let mut parts = line.split_whitespace();
                    let Some(col_raw) = parts.next() else {
                        continue;
                    };
                    let Some(ty_raw) = parts.next() else {
                        continue;
                    };
                    let col = col_raw.trim_matches('"');
                    let ty = ty_raw.trim_end_matches(',').to_ascii_uppercase();
                    let is_timestamp = col.ends_with("_at_ms")
                        || col.ends_with("_at")
                        || col.ends_with("_until")
                        || col == "timestamp";
                    if is_timestamp {
                        assert_eq!(
                            ty, "BIGINT",
                            "{module}/{name} 表 {table} 时间列 {col} 必须为 BIGINT，实际为 {ty}"
                        );
                    }
                    if col == "id"
                        && matches!(
                            ty.as_str(),
                            "INTEGER" | "INT" | "INT4" | "SERIAL" | "SMALLINT"
                        )
                    {
                        panic!("{module}/{name} 表 {table} 整型主键 id 必须为 BIGINT，实际为 {ty}");
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn fresh_database_applies_core_and_ai_baselines_with_64bit_timestamps() {
        let db = crate::test_support::test_db(2).await;
        db.ensure_ai_module().await.unwrap();
        assert!(db.is_ai_module_installed().await.unwrap());

        // 验证超过 32 位有符号整型上限的 13 位毫秒时间戳与大主键在当前后端（SQLite / PostgreSQL）均可正常落库与回读。
        let ts_ms: i64 = 1_795_540_000_000;
        let large_id: i64 = 3_000_000_000;
        db.orm()
            .execute_raw(Statement::from_sql_and_values(
                db.backend(),
                "INSERT INTO nuono_groups (id, platform, namespace, conversation_kind, native_parent_id, native_group_id, display_name, enabled, revision, created_at_ms, updated_at_ms) VALUES ($1, 'onebot', '', 'group:native', '', 'p0-bigint-group', 'P0', TRUE, 1, $2, $2)",
                [large_id.into(), ts_ms.into()],
            ))
            .await
            .expect("写入 64 位群 ID 与 13 位毫秒时间戳失败");
        let row = db
            .orm()
            .query_one_raw(Statement::from_sql_and_values(
                db.backend(),
                "SELECT id, created_at_ms FROM nuono_groups WHERE id = $1",
                [large_id.into()],
            ))
            .await
            .unwrap()
            .expect("应能读回刚写入的 64 位记录");
        assert_eq!(row.try_get::<i64>("", "id").unwrap(), large_id);
        assert_eq!(row.try_get::<i64>("", "created_at_ms").unwrap(), ts_ms);
        db.shutdown().await.unwrap();
    }
}
