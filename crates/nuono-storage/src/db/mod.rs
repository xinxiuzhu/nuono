mod agent;
mod agent_runs;
mod ai_conversations;
mod ai_models;
#[cfg(test)]
#[path = "../../tests/unit/db/ai_usage_shutdown_tests.rs"]
mod ai_usage_shutdown_tests;
pub use ai_models::{CreateAiModel, UpdateAiModel};
mod ai_personas;
pub(crate) mod assistant;
mod companion;
mod companion_data;
mod companion_memory;
mod companion_retention;
mod connection;
pub use assistant::{
    AssistantAccount, AssistantReservation, AssistantTurnAdmission, AssistantTurnRequest,
};
pub use companion_retention::{CompanionRetentionBatch, CompanionRetentionScope};
mod inspection;
mod management;
mod message_history;
pub(crate) mod migrations;
pub use message_history::MessageHistoryStats;
pub use migrations::{ai_module_migration_sources, framework_migration_sources};
mod policy;
pub use inspection::{
    BrowseFilter, BrowsePage, BrowseRequest, ColumnInspection, FilterOperator, MAX_BROWSE_LIMIT,
    MAX_EXPORT_LIMIT, TableInspection, TableOwner, TableSchema,
};
pub use management::{MutationOperation, MutationRequest, MutationResult, TableManagement};
pub use policy::{DatabaseColumnPolicy, DatabaseFieldType, DatabaseTablePolicy};
mod sqlite_lock;
pub use sqlite_lock::{SqliteDatabaseLock, sqlite_database_path};
mod notifications;
mod password_work;
mod plugins;
mod registration_otp_challenges;
mod runtime_settings;
pub(crate) mod secrets;
pub(crate) use secrets::set_secret_on;
mod personal_profile;
pub(crate) mod platform_identities;
pub use platform_identities::{
    AccountPlatformIdentityView, PlatformBindingIssued, PlatformBindingView, PlatformIdentityView,
};
pub(crate) mod account_state;
mod user_tokens;
pub(crate) mod users;
pub use account_state::{AccountSessionView, AuditEvent, append_audit_on};
pub(crate) mod account_merge;
pub use account_merge::{
    AccountMergeExecution, AccountMergeService, AccountMergeView, MergeCredentials,
    MergeExecutionOptions,
};

pub use crate::entity::users::{ACCOUNT_KIND_REGISTERED, ACCOUNT_KIND_SHADOW};
pub use crate::identity_resolver::{IdentityResolver, ResolvedIdentity};
pub use registration_otp_challenges::{
    IssuedRegistrationOtp, REGISTRATION_OTP_LENGTH, REGISTRATION_OTP_TTL_MINUTES,
    RegistrationOtpIssueOutcome, RegistrationOtpVerification,
};
pub use secrets::SECRET_KEYS;
pub use user_tokens::{IssuedToken, TOKEN_CHANGE_EMAIL, TOKEN_RESET_PASSWORD, TOKEN_VERIFY_EMAIL};
pub use users::{NewUser, ROLE_ADMIN, ROLE_OWNER, ROLE_USER, STATUS_ACTIVE, STATUS_DISABLED};

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, EntityTrait,
    PaginatorTrait, QueryFilter, SqliteTransactionMode, Statement, TransactionOptions,
    TransactionTrait,
};

use crate::entity;

use crate::config::DatabaseConfig;
use crate::error::{NuonoError, Result};

/// 在调用方事务内保存框架设置或单例状态，保留创建时间并递增修订号。
pub(crate) async fn set_global_config_on(
    db: &impl ConnectionTrait,
    key: &str,
    value: &serde_json::Value,
) -> Result<()> {
    if key.is_empty() || key.len() > 256 || key.contains('\0') {
        return Err(NuonoError::Validation(
            "设置键必须为1至256字节且不能含NUL".into(),
        ));
    }
    let json = serde_json::to_string(value)?;
    nuono_domain::scalar::parse_json(&json)?;
    let now = crate::clock::db_now().timestamp_millis();
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "INSERT INTO nuono_global_configs (key,value_json,value_version,revision,created_at_ms,updated_at_ms) VALUES ($1,$2,1,1,$3,$3) ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json, revision=nuono_global_configs.revision+1, updated_at_ms=excluded.updated_at_ms",
        [key.into(), json.into(), now.into()],
    ))
    .await?;
    Ok(())
}

pub use nuono_app::plugin_data::{PluginMigration, PluginMigrationForBackend};

pub use nuono_app::agent::store::{
    AgentMemoryRecord, AgentMemoryUpdateRecord, AgentToolApprovalRecord, AgentToolReceiptRecord,
};

/// 共享数据库连接（SeaORM 双后端：PostgreSQL / SQLite，服务器唯一真相源）
#[derive(Clone, Debug)]
pub struct Database {
    /// SeaORM 持有唯一驱动连接池；公开接口不暴露驱动池。
    conn: DatabaseConnection,
    backend: DbBackend,
    sqlite_lock: Option<SqliteDatabaseLock>,
    /// 可选 AI 模块（15 张 AI/Agent/陪伴表）是否已安装到本地库。
    ai_module_ready: Arc<AtomicBool>,
    /// 有界、尽力写入的消息历史；所有 clone 共享准入与关闭状态。
    history: Arc<message_history::HistoryWriter>,
    pub(crate) ai_usage: Arc<crate::ai_usage::UsageWriter>,
    pub(crate) activity: Arc<crate::activity::ActivityWriter>,
    /// 密码哈希与校验等 CPU 密集型任务受独立的硬性并发准入池限制，所有 Database 克隆共享同一限流池。
    password_work: Arc<password_work::PasswordWorkPool>,
    /// 进程内有界 LRU 平台身份解析缓存，与所有 Database 克隆共享并在合并/转正/解绑后实时失效。
    pub(crate) identity_cache: Arc<crate::identity_resolver::IdentityCache>,
    shutdown_completed: Arc<tokio::sync::Mutex<bool>>,
    /// 测试基建专用：PG 时为测试数据库名（shutdown 时 DROP），SQLite 时为数据库
    /// 文件路径（shutdown 时删除文件）。
    test_db_name: Option<String>,
}

impl Database {
    /// SeaORM 连接（当前后端）。跨后端 DAO 一律走这里。
    pub fn orm(&self) -> &DatabaseConnection {
        &self.conn
    }

    /// 当前后端方言。
    pub fn backend(&self) -> DbBackend {
        self.backend
    }

    /// 检查可选 AI/Agent/陪伴模块表是否已安装。
    pub async fn is_ai_module_installed(&self) -> Result<bool> {
        if self.ai_module_ready.load(Ordering::Acquire) {
            return Ok(true);
        }
        let installed = migrations::is_ai_module_installed(&self.conn).await?;
        if installed {
            self.ai_module_ready.store(true, Ordering::Release);
        }
        Ok(installed)
    }

    /// 按需原子安装可选 AI/Agent/陪伴模块表（幂等、并发安全）。
    pub async fn ensure_ai_module(&self) -> Result<()> {
        if self.ai_module_ready.load(Ordering::Acquire) {
            return Ok(());
        }
        migrations::ensure_ai_module(&self.conn).await?;
        self.ai_module_ready.store(true, Ordering::Release);
        Ok(())
    }

    /// 开启一个写事务。
    ///
    /// PostgreSQL：普通 `BEGIN`（并发控制交给 PG 的 MVCC / advisory lock）。
    /// SQLite：真正**数据库级** `BEGIN IMMEDIATE`——在 BEGIN 语句本身就拿写锁，
    /// 不是进程内 mutex，跨连接、跨进程都互斥。
    pub async fn begin_write(&self) -> Result<DatabaseTransaction> {
        match self.backend {
            DbBackend::Postgres => Ok(self.conn.begin().await?),
            DbBackend::Sqlite => Ok(self
                .conn
                .begin_with_options(TransactionOptions {
                    isolation_level: None,
                    access_mode: None,
                    sqlite_transaction_mode: Some(SqliteTransactionMode::Immediate),
                })
                .await?),
            _ => Err(NuonoError::NotImplemented(format!(
                "begin_write 不支持后端 {:?}",
                self.backend
            ))),
        }
    }

    /// 从配置创建数据库连接并执行迁移（按 URL scheme 自动选择 PG / SQLite）。
    #[tracing::instrument(skip(cfg))]
    pub async fn from_config(cfg: &DatabaseConfig) -> Result<Self> {
        let handles = connection::connect_and_migrate(cfg).await?;

        let ai_module_ready = Arc::new(AtomicBool::new(handles.ai_installed));
        tracing::debug!(backend = ?handles.backend, ai_installed = handles.ai_installed, "数据库初始化完成");
        Ok(Self {
            ai_usage: crate::ai_usage::UsageWriter::start(
                handles.conn.clone(),
                ai_module_ready.clone(),
            ),
            activity: crate::activity::ActivityWriter::start(handles.conn.clone()),
            history: message_history::HistoryWriter::start(handles.conn.clone()),
            password_work: password_work::PasswordWorkPool::new(),
            identity_cache: crate::identity_resolver::IdentityCache::new(),
            ai_module_ready,
            conn: handles.conn,
            backend: handles.backend,
            sqlite_lock: handles.sqlite_lock,
            shutdown_completed: Arc::new(tokio::sync::Mutex::new(false)),
            test_db_name: None,
        })
    }

    /// 测试基建专用：标记该实例关联的测试数据库（PG 名 / SQLite 文件路径），
    /// `shutdown` 时一并清理。
    pub fn mark_test_db(&mut self, name: String) {
        self.test_db_name = Some(name);
    }

    /// 检查数据库连接是否正常
    pub async fn ping(&self) -> Result<bool> {
        let stmt = Statement::from_string(self.backend, "SELECT 1");
        match self.conn.query_one_raw(stmt).await {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    /// 获取指定表的行数。表名来自内部白名单，不接受外部输入。
    pub async fn get_table_count(&self, table: &str) -> Result<u64> {
        let sql = format!("SELECT COUNT(*) AS cnt FROM \"{table}\"");
        let count: i64 = self
            .conn
            .query_one_raw(Statement::from_string(self.backend, sql))
            .await?
            .ok_or_else(|| NuonoError::DatabaseQuery("COUNT 查询无结果行".to_string()))?
            .try_get("", "cnt")?;
        Ok(count as u64)
    }

    async fn timed<T>(
        &self,
        op: &'static str,
        table: &'static str,
        fut: impl std::future::Future<Output = T>,
    ) -> T {
        let start = std::time::Instant::now();
        let result = fut.await;
        let duration = start.elapsed();
        metrics::histogram!("nuono_db_query_duration_seconds", "operation" => op, "table" => table)
            .record(duration.as_secs_f64());
        result
    }

    /// 单后端插件迁移无法提供跨数据库（PostgreSQL / SQLite）可移植表结构，非空调用将直接报错拒绝。
    pub async fn run_plugin_migrations(
        &self,
        _plugin_id: &str,
        migrations: &[PluginMigration],
    ) -> Result<()> {
        if migrations.is_empty() {
            return Ok(());
        }
        Err(NuonoError::NotImplemented(
            "单后端插件迁移已移除，请登记双端 SQL 和完整存储清单".into(),
        ))
    }

    /// 将仅包含 `CREATE TABLE` 的双后端插件迁移声明适配并登记到统一的不可变插件迁移台账中。
    pub async fn run_plugin_migrations_for_backend(
        &self,
        plugin_id: &str,
        migrations: &[PluginMigrationForBackend],
    ) -> Result<()> {
        let declaration = nuono_app::plugin_manifest::PluginStorageDeclaration::from_migrations(
            plugin_id,
            migrations.to_vec(),
        )?;
        self.register_plugin_storage(plugin_id, "development", &declaration)
            .await
    }

    #[tracing::instrument(skip(self), fields(bot_id = %bot_id), err(Debug))]
    pub async fn update_bot_status(&self, bot_id: &str, status: &str) -> Result<()> {
        self.timed("update_bot_status", "nuono_bot_instances", async {
            crate::connections::upsert_reverse_ws_status(self, bot_id, status).await
        })
        .await
    }

    #[tracing::instrument(skip(self), fields(bot_id = %bot_id), err(Debug))]
    pub async fn update_bot_nickname(&self, bot_id: &str, nickname: &str) -> Result<()> {
        self.timed("update_bot_nickname", "nuono_bot_instances", async {
            crate::connections::update_bot_nickname(self, bot_id, nickname).await
        })
        .await
    }

    #[tracing::instrument(skip(self), fields(bot_id = %bot_id), err(Debug))]
    pub async fn delete_bot(&self, bot_id: &str) -> Result<()> {
        self.timed("delete_bot", "nuono_bot_instances", async {
            crate::connections::delete_reverse_ws_bot(self, bot_id).await
        })
        .await
    }

    #[tracing::instrument(skip(self, message), fields(bot_id = %bot_id, level), err(Debug))]
    pub async fn add_log(&self, level: &str, bot_id: &str, message: &str) -> Result<()> {
        self.timed("add_log", "logs", async {
            // 直接输出到 tracing，由 LogBroadcastLayer 统一捕获到内存缓存和 WebSocket
            match level {
                "ERROR" => tracing::error!(bot_id = %bot_id, "{}", message),
                "WARN" => tracing::warn!(bot_id = %bot_id, "{}", message),
                "DEBUG" => tracing::debug!(bot_id = %bot_id, "{}", message),
                "TRACE" => tracing::trace!(bot_id = %bot_id, "{}", message),
                _ => tracing::info!(bot_id = %bot_id, "{}", message),
            }
            Ok(())
        })
        .await
    }

    /// 尝试将消息历史加入有界后台写入队列，绝不等待队列或创建逐条任务。
    ///
    /// 成功只表示内存准入，不保证落盘。队列满、payload 超限或关闭时立即返回错误，
    /// 按固定 reason 累计指标；业务流程不应因此重试消息。shutdown 后禁止新准入，
    /// 不再回退到直接 INSERT。保留 async 签名供现有调用者使用，内部没有等待点。
    pub async fn save_message_history(
        &self,
        bot_id: &str,
        msg: &serde_json::Value,
        direction: &str,
    ) -> Result<()> {
        self.history.try_record(bot_id, msg, direction)
    }

    /// 消息历史的准入、落盘和丢弃计数，以及当前已准入字符串 payload 字节数。
    /// 字节数不包含结构体、分配器开销或 ORM 暂存，并非整个进程的精确堆占用。
    pub fn message_history_stats(&self) -> MessageHistoryStats {
        self.history.stats()
    }

    /// 按本地自然日与指定时间窗口聚合消息趋势（自动补齐空桶，避免前端再按 UTC 猜网格）。
    #[tracing::instrument(skip(self), err(Debug))]
    pub async fn message_trend_rows(
        &self,
        range: crate::statistics::MessageTrendRange,
        tz_offset_minutes: i32,
    ) -> Result<Vec<crate::statistics::MessageTrend>> {
        self.message_trend_rows_at(range, tz_offset_minutes, *crate::clock::db_now())
            .await
    }

    pub(crate) async fn message_trend_rows_at(
        &self,
        range: crate::statistics::MessageTrendRange,
        tz_offset_minutes: i32,
        now_utc: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<crate::statistics::MessageTrend>> {
        use crate::statistics::MessageTrendRange;
        use chrono::{Duration, FixedOffset, Timelike};

        self.timed("get_message_trend", "nuono_messages", async {
            let clamped_minutes = tz_offset_minutes.clamp(-720, 840);
            let offset = FixedOffset::east_opt(clamped_minutes * 60)
                .ok_or_else(|| NuonoError::Validation("时区偏移无效".to_string()))?;
            let local_now = now_utc.with_timezone(&offset);
            let today = local_now.date_naive();
            let to_utc_ms = |date: chrono::NaiveDate| -> Result<i64> {
                let naive = date
                    .and_hms_opt(0, 0, 0)
                    .ok_or_else(|| NuonoError::Validation("日期边界无效".to_string()))?;
                // FixedOffset 不存在夏令时跳变，单点映射必然唯一。
                let local_dt = naive
                    .and_local_timezone(offset)
                    .single()
                    .ok_or_else(|| NuonoError::Validation("本地时区换算失败".to_string()))?;
                Ok(local_dt.with_timezone(&chrono::Utc).timestamp_millis())
            };

            let (start_ms, end_ms, step_ms, labels): (i64, i64, i64, Vec<String>) = match range {
                MessageTrendRange::Yesterday => {
                    let date = today
                        .pred_opt()
                        .ok_or_else(|| NuonoError::Validation("日期范围无效".to_string()))?;
                    let start = to_utc_ms(date)?;
                    let end = to_utc_ms(today)?;
                    let labels = (0..24)
                        .map(|hour| format!("{} {hour:02}:00", date.format("%Y-%m-%d")))
                        .collect();
                    (start, end, 3_600_000, labels)
                }
                MessageTrendRange::Today => {
                    let start = to_utc_ms(today)?;
                    let current_hour = local_now.hour() as i64;
                    let end = start + (current_hour + 1) * 3_600_000;
                    let labels = (0..=current_hour)
                        .map(|hour| format!("{} {hour:02}:00", today.format("%Y-%m-%d")))
                        .collect();
                    (start, end, 3_600_000, labels)
                }
                MessageTrendRange::Week => {
                    let start_date = today - Duration::days(6);
                    let next_day = today
                        .succ_opt()
                        .ok_or_else(|| NuonoError::Validation("日期范围无效".to_string()))?;
                    let start = to_utc_ms(start_date)?;
                    let end = to_utc_ms(next_day)?;
                    let labels = (0..7)
                        .map(|day| (start_date + Duration::days(day)).format("%Y-%m-%d").to_string())
                        .collect();
                    (start, end, 86_400_000, labels)
                }
                MessageTrendRange::Month => {
                    let start_date = today - Duration::days(29);
                    let next_day = today
                        .succ_opt()
                        .ok_or_else(|| NuonoError::Validation("日期范围无效".to_string()))?;
                    let start = to_utc_ms(start_date)?;
                    let end = to_utc_ms(next_day)?;
                    let labels = (0..30)
                        .map(|day| (start_date + Duration::days(day)).format("%Y-%m-%d").to_string())
                        .collect();
                    (start, end, 86_400_000, labels)
                }
            };

            let sql = match self.backend {
                DbBackend::Postgres => {
                    "SELECT ((occurred_at_ms - $1) / $3)::BIGINT AS bucket, \
                            COALESCE(SUM(CASE WHEN direction = 'inbound' THEN 1 ELSE 0 END), 0)::BIGINT AS received, \
                            COALESCE(SUM(CASE WHEN direction = 'outbound' THEN 1 ELSE 0 END), 0)::BIGINT AS sent \
                     FROM nuono_messages \
                     WHERE occurred_at_ms >= $1 AND occurred_at_ms < $2 \
                     GROUP BY bucket ORDER BY bucket ASC"
                }
                DbBackend::Sqlite => {
                    "SELECT CAST(((occurred_at_ms - ?1) / ?3) AS INTEGER) AS bucket, \
                            COALESCE(SUM(CASE WHEN direction = 'inbound' THEN 1 ELSE 0 END), 0) AS received, \
                            COALESCE(SUM(CASE WHEN direction = 'outbound' THEN 1 ELSE 0 END), 0) AS sent \
                     FROM nuono_messages \
                     WHERE occurred_at_ms >= ?1 AND occurred_at_ms < ?2 \
                     GROUP BY bucket ORDER BY bucket ASC"
                }
                other => {
                    return Err(NuonoError::NotImplemented(format!(
                        "消息趋势不支持后端 {other:?}"
                    )));
                }
            };
            let rows = self
                .conn
                .query_all_raw(Statement::from_sql_and_values(
                    self.backend,
                    sql,
                    [start_ms.into(), end_ms.into(), step_ms.into()],
                ))
                .await?;
            let mut counts = vec![(0_i64, 0_i64); labels.len()];
            for row in rows {
                let bucket: i64 = row.try_get("", "bucket")?;
                let received: i64 = row.try_get("", "received")?;
                let sent: i64 = row.try_get("", "sent")?;
                if let Ok(index) = usize::try_from(bucket)
                    && let Some(slot) = counts.get_mut(index)
                {
                    *slot = (received, sent);
                }
            }
            Ok(labels
                .into_iter()
                .zip(counts)
                .map(|(time, (received, sent))| crate::statistics::MessageTrend {
                    time,
                    received,
                    sent,
                })
                .collect())
        })
        .await
    }

    #[tracing::instrument(skip(self), fields(target_type, target_id), err(Debug))]
    pub async fn is_blacklisted(&self, target_type: &str, target_id: &str) -> Result<bool> {
        self.timed("is_blacklisted", "nuono_filter_rules", async {
            let filter_type = crate::security::blacklist_filter_type(target_type);
            let count = entity::filter_rules::Entity::find()
                .filter(entity::filter_rules::Column::FilterType.eq(filter_type))
                .filter(entity::filter_rules::Column::TargetId.eq(target_id))
                .filter(entity::filter_rules::Column::Enabled.eq(true))
                .count(self.orm())
                .await?;
            Ok(count > 0)
        })
        .await
    }

    #[tracing::instrument(skip(self), err(Debug))]
    pub async fn get_global_config(&self, key: &str) -> Result<Option<serde_json::Value>> {
        self.timed("get_global_config", "nuono_global_configs", async {
            let row = entity::global_configs::Entity::find()
                .filter(entity::global_configs::Column::Key.eq(key))
                .one(self.orm())
                .await?;

            row.map(|r| nuono_domain::scalar::parse_json(&r.value).map_err(Into::into))
                .transpose()
        })
        .await
    }

    #[tracing::instrument(skip(self), err(Debug))]
    pub async fn delete_global_config(&self, key: &str) -> Result<()> {
        self.timed("delete_global_config", "nuono_global_configs", async {
            entity::global_configs::Entity::delete_many()
                .filter(entity::global_configs::Column::Key.eq(key))
                .exec(self.orm())
                .await?;
            Ok(())
        })
        .await
    }

    #[tracing::instrument(skip(self, value), err(Debug))]
    pub async fn set_global_config(&self, key: &str, value: serde_json::Value) -> Result<()> {
        self.timed(
            "set_global_config",
            "nuono_global_configs",
            set_global_config_on(self.orm(), key, &value),
        )
        .await
    }

    /// 检查是否已设置任何管理 Token
    pub async fn has_any_auth_token(&self) -> Result<bool> {
        let count = entity::global_configs::Entity::find()
            .filter(entity::global_configs::Column::Key.starts_with("auth_token_"))
            .count(self.orm())
            .await?;
        Ok(count > 0)
    }

    #[tracing::instrument(skip(self), fields(group_id = %group_id), err(Debug))]
    pub async fn is_group_enabled(&self, group_id: &str) -> Result<bool> {
        self.timed("is_group_enabled", "nuono_groups", async {
            let group = self
                .ensure_group(&crate::groups::GroupIdentity::onebot(group_id), "")
                .await?;
            Ok(group.enabled && nuono_app::entitlement::is_group_active(group.id).await)
        })
        .await
    }

    #[tracing::instrument(skip(self), fields(filter_type), err(Debug))]
    pub async fn get_filter_rules(&self, filter_type: &str) -> Result<Vec<serde_json::Value>> {
        self.timed("get_filter_rules", "nuono_filter_rules", async {
            let rows = entity::filter_rules::Entity::find()
                .filter(entity::filter_rules::Column::FilterType.eq(filter_type))
                .filter(entity::filter_rules::Column::Enabled.eq(true))
                .all(self.orm())
                .await?;

            Ok(rows
                .into_iter()
                .map(|r| {
                    serde_json::json!({
                        "pattern": r.pattern,
                        "match_mode": r.match_mode,
                    })
                })
                .collect())
        })
        .await
    }

    /// 统一格式化时间戳为字符串
    pub fn fmt_ts(ts: &Option<chrono::NaiveDateTime>) -> String {
        ts.map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "N/A".to_string())
    }

    /// 查询插件是否启用；不存在的插件默认视为启用。
    pub async fn is_plugin_enabled(&self, name: &str) -> Result<bool> {
        let enabled = entity::plugins::Entity::find()
            .filter(entity::plugins::Column::PluginId.eq(name))
            .one(self.orm())
            .await?
            .map(|m| m.enabled);
        Ok(enabled.unwrap_or(true))
    }

    /// 关闭历史准入，等待已有历史与用量写入尝试完成，然后关闭共享连接池。
    ///
    /// 历史属于 best-effort：写失败、超时及故障退避期间的记录计数后丢弃，不重试
    /// 可能已经提交的 INSERT。多个 clone 的关闭请求串行；等待者被取消不丢失 handle。
    pub async fn shutdown(&self) -> Result<()> {
        let mut completed = self.shutdown_completed.lock().await;
        if *completed {
            return Ok(());
        }
        self.history.close();
        self.ai_usage.shutdown().await?;
        self.activity.shutdown().await?;
        self.history.shutdown().await?;

        self.conn.close_by_ref().await?;
        if let Some(lock) = &self.sqlite_lock {
            lock.release()?;
        }
        if let Some(name) = self.test_db_name.as_deref() {
            match self.backend {
                DbBackend::Sqlite => {
                    crate::test_support::remove_sqlite_test_files(name);
                }
                _ => {
                    crate::test_support::drop_test_db(name).await;
                }
            }
        }

        *completed = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> Database {
        crate::test_support::test_db(2).await
    }

    #[tokio::test]
    async fn only_one_concurrent_tool_approval_claim_succeeds() {
        let db = test_db().await;
        let id = db
            .create_agent_tool_approval(AgentToolApprovalRecord {
                run_id: None,
                context_json: None,
                session_id: "test-session".to_string(),
                bot_id: 1,
                platform: "test".to_string(),
                message_type: "private".to_string(),
                user_id: 2,
                group_id: None,
                channel_id: None,
                message_id: None,
                tool_name: "test.tool".to_string(),
                risk_level: "RequiresApproval".to_string(),
                input_json: Some("{}".to_string()),
                reason: Some("test".to_string()),
            })
            .await
            .unwrap();
        db.decide_agent_tool_approval(id, true, Some("test".to_string()), None)
            .await
            .unwrap();

        // 同一套瞬时错误防御：claim 是 CAS、幂等，重试不改变「一个赢家」。
        async fn claim_with_retry(
            db: &Database,
            id: i64,
        ) -> Option<crate::entity::agent_tool_approvals::Model> {
            for attempt in 0..3 {
                match db.claim_agent_tool_approval_for_execution(id).await {
                    Ok(outcome) => return outcome,
                    Err(e) if attempt < 2 => {
                        tracing::warn!(error = %e, attempt, "claim 瞬时失败，稍后重试");
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    }
                    Err(e) => panic!("claim 连续失败: {e}"),
                }
            }
            unreachable!()
        }

        let (first, second) = tokio::join!(claim_with_retry(&db, id), claim_with_retry(&db, id));
        let claims = [first, second];

        assert_eq!(claims.iter().filter(|claim| claim.is_some()).count(), 1);
        assert!(claims.iter().flatten().all(|row| row.status == "executing"));
        assert!(
            db.claim_agent_tool_approval_for_execution(id)
                .await
                .unwrap()
                .is_none()
        );

        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn single_backend_migrations_are_rejected() {
        let db = test_db().await;
        assert!(
            db.run_plugin_migrations(
                "old",
                &[PluginMigration {
                    version: 1,
                    description: "legacy".into(),
                    sql: "CREATE TABLE unowned(id BIGINT PRIMARY KEY)".into()
                }]
            )
            .await
            .is_err()
        );
        db.shutdown().await.unwrap();
    }
}

mod account_security;

mod account_provisioning;
