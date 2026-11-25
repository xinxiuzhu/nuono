//! 登录账号的持久化。
//!
//! 密码只以带版本标识的 bcrypt 哈希存储，本模块是唯一的写入入口，因此不存在明文落库的路径。
//! 首个账号由首启设置向导创建，角色为 `owner`；后续账号（含通过邮箱注册的用户）
//! 复用同一张表，用 `role` 区分权限。

use sea_orm::{
    ActiveValue::NotSet, ColumnTrait, Condition, ConnectionTrait, DbBackend, EntityTrait,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, Set, Statement, sea_query::Expr,
};

#[cfg(test)]
use sea_orm::sea_query::ExprTrait;

use super::Database;
#[cfg(test)]
pub(super) use super::password_work::{
    hash_password_fixture as hash_password, verify_password_fixture as verify_password,
};
use crate::entity::users;
use crate::error::{NuonoError, Result};

/// 账号角色的落库文本值。
///
/// 语义与授权规则都在 [`nuono_app::auth::Role`]，这里只是它的文本投影，供 SQL 过滤和存量
/// 调用点使用。值由枚举派生而不是各写一份字面量 —— 两处独立的 `"admin"` 迟早漂移，
/// 而漂移的表现是授权静默失效，不是编译错误。
///
/// 新代码请直接用 [`nuono_app::auth::Role`]；需要文本时调 `Role::as_str()`。
pub const ROLE_OWNER: &str = nuono_app::auth::Role::Owner.as_str();
pub const ROLE_ADMIN: &str = nuono_app::auth::Role::Admin.as_str();
pub const ROLE_USER: &str = nuono_app::auth::Role::User.as_str();

/// 正式账号只有 active/disabled；未验证注册只存在于 account_challenges。
pub const STATUS_ACTIVE: &str = "active";
pub const STATUS_DISABLED: &str = "disabled";

/// 创建账号所需的输入。密码为明文，仅在本结构体内短暂存在，落库前即哈希。
#[derive(Debug, Clone)]
pub struct NewUser {
    pub username: String,
    pub email: Option<String>,
    pub password: String,
    pub role: String,
    pub status: String,
    /// 邮箱是否视为已确认归属。
    ///
    /// 有邮箱时必须已验证；匿名注册在 account_challenges 验证成功后才建号。
    /// 无邮箱的管理员建号可填 false。
    pub email_verified: bool,
}

/// 在给定连接（池或事务）上插入一行 `users` 并取回完整 Model。
///
/// 不用 `RETURNING`：它不是两个后端都可用的方言（SQLite 需要额外 feature），插入后
/// 按主键回读一次既拿到数据库生成的默认值，也与后端无关。首启/建号都是低频路径，
/// 多一次主键查询的代价可以忽略。ORM 行（`orm::Model`，含隐藏的 `totp_last_counter`）
/// 在这里收敛成公开的 `users::Model`，调用方与 JSON 契约不感知隐藏列。
pub(super) async fn insert_user<C>(conn: &C, am: users::orm::ActiveModel) -> Result<users::Model>
where
    C: ConnectionTrait,
{
    let id = users::orm::Entity::insert(am)
        .exec(conn)
        .await
        .map_err(account_unique_conflict)?
        .last_insert_id;
    users::orm::Entity::find_by_id(id)
        .one(conn)
        .await?
        .map(users::Model::from)
        .ok_or_else(|| NuonoError::DatabaseQuery(format!("账号插入后回读失败: id={id}")))
}

pub(super) fn account_unique_conflict(error: sea_orm::DbErr) -> NuonoError {
    if matches!(
        error.sql_err(),
        Some(sea_orm::SqlErr::UniqueConstraintViolation(_))
    ) {
        NuonoError::Conflict("用户名或邮箱已经被使用".into())
    } else {
        error.into()
    }
}

pub(super) fn new_user_active_model(
    username: &str,
    email: Option<&str>,
    email_verified: bool,
    password_hash: &str,
    role: &str,
    status: &str,
    now: impl Into<chrono::DateTime<chrono::Utc>>,
) -> users::orm::ActiveModel {
    let now = now.into();
    users::orm::ActiveModel {
        id: NotSet,
        account_kind: Set(users::ACCOUNT_KIND_REGISTERED.to_owned()),
        username: Set(Some(username.to_string())),
        username_key: Set(Some(username.to_ascii_lowercase())),
        auth_epoch: NotSet,
        revision: NotSet,
        merged_into_user_id: NotSet,
        email: Set(email.filter(|_| email_verified).map(str::to_owned)),
        email_key: Set(email
            .filter(|_| email_verified)
            .map(|value| value.trim().to_ascii_lowercase())),
        email_verified_at_ms: Set(email
            .filter(|_| email_verified)
            .map(|_| now.timestamp_millis())),
        password_hash: Set(Some(password_hash.to_string())),
        // enroll 之前 TOTP 三元组为空；totp_enabled 列 NOT NULL，显式落 false。
        totp_secret: NotSet,
        totp_enabled: Set(false),
        totp_recovery_json: NotSet,
        role: Set(role.to_string()),
        status: Set(status.to_string()),
        last_login_at_ms: NotSet,
        created_at_ms: Set(now.timestamp_millis()),
        updated_at_ms: Set(now.timestamp_millis()),
        // 重放保护计数由数据库默认 NULL 起，插入时不需要显式值。
        totp_last_counter: NotSet,
        display_name: Set(String::new()),
        birthday: Set(String::new()),
        gender: Set(String::new()),
        bio: Set(String::new()),
        use_for_ai: Set(false),
    }
}

/// 构造一条占位用户（`shadow`）的新增模型：无登录凭据、角色固定 `user`，支持按需封禁。
pub(crate) fn new_shadow_user_active_model(
    display_name: &str,
    now_ms: i64,
) -> users::orm::ActiveModel {
    let safe_name: String = display_name
        .chars()
        .filter(|ch| *ch != '\0')
        .take(100)
        .collect();
    users::orm::ActiveModel {
        id: NotSet,
        account_kind: Set(users::ACCOUNT_KIND_SHADOW.to_owned()),
        username: Set(None),
        username_key: Set(None),
        auth_epoch: NotSet,
        revision: NotSet,
        merged_into_user_id: NotSet,
        email: Set(None),
        email_key: Set(None),
        email_verified_at_ms: Set(None),
        password_hash: Set(None),
        totp_secret: NotSet,
        totp_enabled: Set(false),
        totp_recovery_json: NotSet,
        role: Set(ROLE_USER.to_owned()),
        status: Set(STATUS_ACTIVE.to_owned()),
        last_login_at_ms: NotSet,
        created_at_ms: Set(now_ms),
        updated_at_ms: Set(now_ms),
        totp_last_counter: NotSet,
        display_name: Set(safe_name),
        birthday: Set(String::new()),
        gender: Set(String::new()),
        bio: Set(String::new()),
        use_for_ai: Set(false),
    }
}

impl Database {
    /// 已注册正式账号总数。为 0 表示尚未完成首启设置，此时设置向导开放（占位账号不计入）。
    pub async fn user_count(&self) -> Result<u64> {
        self.timed("user_count", "nuono_users", async {
            let count = users::orm::Entity::find()
                .filter(users::orm::Column::AccountKind.eq(users::ACCOUNT_KIND_REGISTERED))
                .count(self.orm())
                .await?;
            Ok(count)
        })
        .await
    }

    /// 按规范用户名查账号，登录与唯一约束使用相同的 ASCII 小写键。
    pub async fn find_user_by_username(&self, username: &str) -> Result<Option<users::Model>> {
        self.timed("find_user_by_username", "nuono_users", async {
            Ok(users::orm::Entity::find()
                .filter(users::orm::Column::UsernameKey.eq(username.trim().to_ascii_lowercase()))
                .one(self.orm())
                .await?
                .map(users::Model::from))
        })
        .await
    }

    pub async fn find_user_by_id(&self, id: i64) -> Result<Option<users::Model>> {
        self.timed("find_user_by_id", "nuono_users", async {
            Ok(users::orm::Entity::find_by_id(id)
                .one(self.orm())
                .await?
                .map(users::Model::from))
        })
        .await
    }

    /// 按邮箱查账号。邮箱统一小写存储，这里也按小写匹配。
    pub async fn find_user_by_email(&self, email: &str) -> Result<Option<users::Model>> {
        let email = email.trim().to_ascii_lowercase();
        self.timed("find_user_by_email", "nuono_users", async {
            Ok(users::orm::Entity::find()
                .filter(users::orm::Column::EmailKey.eq(email.trim().to_ascii_lowercase()))
                .one(self.orm())
                .await?
                .map(users::Model::from))
        })
        .await
    }

    /// 创建账号，密码在此哈希。用户名或邮箱重复时返回错误而不是静默覆盖。
    pub async fn create_user(&self, input: NewUser) -> Result<users::Model> {
        if !matches!(input.status.as_str(), STATUS_ACTIVE | STATUS_DISABLED)
            || !matches!(input.role.as_str(), ROLE_OWNER | ROLE_ADMIN | ROLE_USER)
        {
            return Err(NuonoError::ConfigValidation(
                "正式账号角色或状态无效".into(),
            ));
        }
        if input
            .email
            .as_ref()
            .is_some_and(|email| !email.trim().is_empty())
            && !input.email_verified
        {
            return Err(NuonoError::ConfigValidation(
                "未验证邮箱必须先完成账号验证请求".into(),
            ));
        }
        let username = input.username.trim().to_string();
        if username.is_empty() {
            return Err(NuonoError::ConfigValidation("用户名不能为空".into()));
        }
        let email = input
            .email
            .map(|e| e.trim().to_ascii_lowercase())
            .filter(|e| !e.is_empty());

        if self.find_user_by_username(&username).await?.is_some() {
            return Err(NuonoError::Conflict(format!("用户名已存在: {username}")));
        }
        if let Some(email) = email.as_deref()
            && self.find_user_by_email(email).await?.is_some()
        {
            return Err(NuonoError::Conflict(format!("邮箱已被注册: {email}")));
        }

        let password_hash = self.password_work.hash(&input.password).await?;
        let now = crate::clock::db_now();

        self.timed("create_user", "nuono_users", async {
            let am = new_user_active_model(
                &username,
                email.as_deref(),
                input.email_verified,
                &password_hash,
                &input.role,
                &input.status,
                now,
            );
            insert_user(self.orm(), am).await
        })
        .await
    }

    /// 首启向导的原子入口：把「零用户检查 + 创建 owner」串成临界区。
    ///
    /// 中间件与 handler 各自的 `user_count() == 0` 检查之间没有事务，两个并发
    /// `POST /api/setup` 用不同用户名会双双建出 owner，破坏「只能成功一次」的
    /// 承诺（也会影响「最后一个 owner」保护的语义）。返回 `None` 表示锁内复查
    /// 发现已有账号——调用方按已初始化处理。
    ///
    /// 锁的实现按后端分：PostgreSQL 用事务级 advisory lock；SQLite 用
    /// [`Database::begin_write`]（immediate 事务，拿写锁后其余写事务进不来），
    /// 锁内复查在两个后端都成立。
    pub async fn create_first_owner(&self, input: NewUser) -> Result<Option<users::Model>> {
        const SETUP_WIZARD_LOCK_KEY: i64 = 0x6E75_6F6E_6F02;

        let username = input.username.trim().to_string();
        if username.is_empty() {
            return Err(NuonoError::ConfigValidation("用户名不能为空".into()));
        }
        let email = input
            .email
            .map(|e| e.trim().to_ascii_lowercase())
            .filter(|e| !e.is_empty());
        nuono_app::auth::validate_username(&username).map_err(NuonoError::Validation)?;
        nuono_app::auth::validate_password(&input.password).map_err(NuonoError::Validation)?;
        if let Some(email) = &email {
            nuono_app::auth::validate_email(email).map_err(NuonoError::Validation)?;
        }
        let password_hash = self.password_work.hash(&input.password).await?;

        self.timed("create_first_owner", "nuono_users", async {
            let tx = self.begin_write().await?;

            // advisory lock 是 PostgreSQL 专属；SQLite 的 immediate 事务本身就是库级写锁。
            if self.backend() == DbBackend::Postgres {
                tx.execute_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT pg_advisory_xact_lock($1)",
                    [SETUP_WIZARD_LOCK_KEY.into()],
                ))
                .await?;
            }

            let count = users::orm::Entity::find()
                .filter(users::orm::Column::AccountKind.eq(users::ACCOUNT_KIND_REGISTERED))
                .count(&tx)
                .await?;
            if count > 0 {
                tx.rollback().await?;
                return Ok(None);
            }

            let now = crate::clock::db_now();
            let mut am = new_user_active_model(
                &username,
                email.as_deref(),
                input.email_verified,
                &password_hash,
                ROLE_OWNER,
                STATUS_ACTIVE,
                now,
            );
            // 若 ID 1 尚未被占位用户占用，首个初始化的 root（owner）账号固定使用 ID 1。
            if users::orm::Entity::find_by_id(1).one(&tx).await?.is_none() {
                am.id = Set(1);
            }
            let owner = insert_user(&tx, am).await?;
            super::append_audit_on(
                &tx,
                super::AuditEvent {
                    actor_user_id: Some(owner.id),
                    actor_plugin_id: None,
                    action: "account.root_initialized",
                    result: "success",
                    subject_user_id: Some(owner.id),
                    group_id: None,
                    operation_id: None,
                    details: serde_json::json!({}),
                },
            )
            .await?;
            if self.backend() == DbBackend::Postgres {
                let sequence = tx
                    .query_one_raw(Statement::from_string(
                        DbBackend::Postgres,
                        "SELECT pg_get_serial_sequence('nuono_users','id') AS name",
                    ))
                    .await?
                    .ok_or_else(|| NuonoError::Internal("账号 ID 序列不存在".into()))?
                    .try_get::<String>("", "name")?;
                let quoted = sequence
                    .split('.')
                    .map(|part| format!("\"{}\"", part.trim_matches('"').replace('"', "\"\"")))
                    .collect::<Vec<_>>()
                    .join(".");
                tx.execute_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    format!(
                        "SELECT setval($1::regclass,GREATEST(last_value,1),TRUE) FROM {quoted}"
                    ),
                    [sequence.into()],
                ))
                .await?;
            }
            tx.commit().await?;
            Ok(Some(owner))
        })
        .await
    }

    /// 按用户名或邮箱查账号。
    ///
    /// 用户名优先：用户名的字符集不含 `@`（见 `validate_username`），所以带 `@` 的输入
    /// 只可能是邮箱，两种标识不会互相遮蔽。
    pub async fn find_user_by_identifier(&self, identifier: &str) -> Result<Option<users::Model>> {
        let identifier = identifier.trim();
        if identifier.is_empty() {
            return Ok(None);
        }
        if let Some(user) = self.find_user_by_username(identifier).await? {
            return Ok(Some(user));
        }
        if identifier.contains('@') {
            return self.find_user_by_email(identifier).await;
        }
        Ok(None)
    }

    /// 校验密码。`identifier` 可以是用户名或邮箱。
    ///
    /// 账号不存在、状态非 active、或密码不匹配都返回 `None`——三种失败返回同一结果，
    /// 避免调用方无意间把「账号是否存在」暴露给攻击者。
    pub async fn verify_user_password(
        &self,
        identifier: &str,
        password: &str,
    ) -> Result<Option<users::Model>> {
        let user = self.find_user_by_identifier(identifier).await?;
        let verified = self
            .password_work
            .verify_login(
                password,
                user.as_ref()
                    .filter(|user| {
                        user.status == STATUS_ACTIVE
                            && user.account_kind == users::ACCOUNT_KIND_REGISTERED
                            && !user.password_hash.is_empty()
                    })
                    .map(|user| user.password_hash.as_str()),
            )
            .await?;
        if !verified.valid {
            return Ok(None);
        }
        let Some(mut user) = user else {
            return Ok(None);
        };
        if user.account_kind != users::ACCOUNT_KIND_REGISTERED
            || user.status != STATUS_ACTIVE
            || user.password_hash.is_empty()
        {
            return Ok(None);
        }
        if let Some(password_hash) = verified.upgraded_hash {
            let now = crate::clock::db_now();
            match users::orm::Entity::update_many()
                .col_expr(
                    users::orm::Column::PasswordHash,
                    Expr::value(password_hash.clone()),
                )
                .col_expr(
                    users::orm::Column::UpdatedAtMs,
                    Expr::value(now.timestamp_millis()),
                )
                .filter(users::orm::Column::Id.eq(user.id))
                .filter(users::orm::Column::AccountKind.eq(users::ACCOUNT_KIND_REGISTERED))
                .filter(users::orm::Column::PasswordHash.eq(user.password_hash.clone()))
                .filter(users::orm::Column::Status.eq(STATUS_ACTIVE))
                .exec(self.orm())
                .await
            {
                Ok(result) if result.rows_affected == 1 => {
                    user.password_hash = password_hash;
                    user.updated_at = now.into();
                }
                Ok(_) => return Ok(None),
                Err(error) => {
                    tracing::warn!(error = %error, user_id = user.id, "升级旧密码哈希失败")
                }
            }
        }

        Ok(Some(user))
    }

    /// 更新密码。调用方负责先校验旧密码或确认重置凭据。
    #[cfg(test)]
    pub(crate) async fn set_user_password(&self, id: i64, new_password: &str) -> Result<()> {
        let password_hash = self.password_work.hash(new_password).await?;

        self.timed("set_user_password", "nuono_users", async {
            let result = users::orm::Entity::update_many()
                .col_expr(users::orm::Column::PasswordHash, Expr::value(password_hash))
                .col_expr(
                    users::orm::Column::AuthEpoch,
                    Expr::col(users::orm::Column::AuthEpoch).add(1),
                )
                .col_expr(
                    users::orm::Column::UpdatedAtMs,
                    Expr::value(crate::clock::db_now().timestamp_millis()),
                )
                .filter(users::orm::Column::Id.eq(id))
                .exec(self.orm())
                .await?;
            if result.rows_affected == 0 {
                return Err(NuonoError::NotFound(format!("账号不存在: {id}")));
            }
            Ok(())
        })
        .await
    }

    /// 列出已注册正式账号，按 id 升序。不返回 `password_hash` 之外的过滤，调用方负责脱敏。
    pub async fn list_users(&self) -> Result<Vec<users::Model>> {
        self.timed("list_users", "nuono_users", async {
            let rows = users::orm::Entity::find()
                .filter(users::orm::Column::AccountKind.eq(users::ACCOUNT_KIND_REGISTERED))
                .filter(users::orm::Column::Status.is_not_in(["deleted", "merged"]))
                .order_by_asc(users::orm::Column::Id)
                .all(self.orm())
                .await?;
            Ok(rows.into_iter().map(users::Model::from).collect())
        })
        .await
    }

    /// 主 owner 的用户名，供管理面显示实例归属人。这里只找 `owner`，与 `admin` 角色无关。
    ///
    /// 取 id 最小的 `owner` 角色账号：首启向导创建的那个。没有 owner 时返回 `None`，
    /// 调用方据此判断是否仍未初始化，而不是回退到硬编码的 `admin`。
    pub async fn primary_owner_username(&self) -> Result<Option<String>> {
        self.timed("primary_owner", "nuono_users", async {
            let username = users::orm::Entity::find()
                .select_only()
                .column(users::orm::Column::Username)
                .filter(users::orm::Column::Role.eq(ROLE_OWNER))
                .order_by_asc(users::orm::Column::Id)
                .into_tuple::<String>()
                .one(self.orm())
                .await?;
            Ok(username)
        })
        .await
    }

    /// 当前 `owner` 角色的账号数。
    ///
    /// 只服务一个判断：降级某个 owner 之后，站点是否还剩得下人来管。因此返回计数而不是列表 ——
    /// 调用方需要的是「还剩几个」，拿到列表就会有人顺手拿它做别的过滤，判据就散了。
    pub async fn owner_count(&self) -> Result<u64> {
        self.timed("owner_count", "nuono_users", async {
            let count = users::orm::Entity::find()
                .filter(users::orm::Column::Role.eq(ROLE_OWNER))
                .count(self.orm())
                .await?;
            Ok(count)
        })
        .await
    }

    /// 所有 `owner`/`admin` 的邮箱（去空去重），认领申请提交时通知值班员用。
    ///
    /// 空邮箱直接过滤掉而不是留给调用方判断：发不出的地址混在列表里只会刷投递失败日志。
    pub async fn list_staff_emails(&self) -> Result<Vec<String>> {
        self.timed("list_staff_emails", "nuono_users", async {
            let emails = users::orm::Entity::find()
                .select_only()
                .column(users::orm::Column::Email)
                .filter(users::orm::Column::Role.is_in([ROLE_OWNER, ROLE_ADMIN]))
                .into_tuple::<Option<String>>()
                .all(self.orm())
                .await?;
            let mut emails: Vec<String> = emails
                .into_iter()
                .flatten()
                .map(|e| e.trim().to_string())
                .filter(|e| !e.is_empty())
                .collect();
            emails.sort();
            emails.dedup();
            Ok(emails)
        })
        .await
    }

    /// 所有 `owner`/`admin` 的账号 id，站内通知（如认领申请提醒）发给值班员用。
    pub async fn list_staff_ids(&self) -> Result<Vec<i64>> {
        self.timed("list_staff_ids", "nuono_users", async {
            let ids = users::orm::Entity::find()
                .select_only()
                .column(users::orm::Column::Id)
                .filter(users::orm::Column::Role.is_in([ROLE_OWNER, ROLE_ADMIN]))
                .into_tuple::<i64>()
                .all(self.orm())
                .await?;
            Ok(ids)
        })
        .await
    }

    /// 测试辅助方法：直接修改账号角色，用于在存储层测试中模拟并发撤销管理员权限的场景。
    #[cfg(test)]
    pub(crate) async fn set_user_role(&self, id: i64, role: nuono_app::auth::Role) -> Result<()> {
        self.timed("set_user_role", "nuono_users", async {
            users::orm::Entity::update_many()
                .col_expr(
                    users::orm::Column::Role,
                    Expr::value(role.as_str().to_string()),
                )
                .filter(users::orm::Column::Id.eq(id))
                .exec(self.orm())
                .await?;
            Ok(())
        })
        .await
    }

    /// 写入 TOTP secret（enroll 阶段，`totp_enabled` 仍为 false）。
    ///
    /// secret 是 base32 明文：验码必须可逆，不能哈希。威胁模型见 `auth::totp` 模块文档。
    #[cfg(test)]
    pub(crate) async fn set_totp_secret(&self, id: i64, secret: Option<&str>) -> Result<()> {
        self.timed("set_totp_secret", "nuono_users", async {
            users::orm::Entity::update_many()
                .col_expr(
                    users::orm::Column::TotpSecret,
                    Expr::value(secret.map(|value| value.as_bytes().to_vec())),
                )
                .col_expr(
                    users::orm::Column::UpdatedAtMs,
                    Expr::value(crate::clock::db_now().timestamp_millis()),
                )
                .filter(users::orm::Column::Id.eq(id))
                .exec(self.orm())
                .await?;
            Ok(())
        })
        .await
    }

    /// 写入两步验证开关与恢复码哈希列表（JSON 数组文本）。
    ///
    /// 关闭时调用方传 `(false, None)` 同时清空恢复码；恢复码只存 SHA-256，
    /// 明文只在生成的那一次返回给用户。
    #[cfg(test)]
    pub(crate) async fn set_totp_state(
        &self,
        id: i64,
        enabled: bool,
        recovery_hashes: Option<&[String]>,
    ) -> Result<()> {
        self.timed("set_totp_state", "nuono_users", async {
            let hashes = recovery_hashes
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| NuonoError::DatabaseQuery(format!("序列化恢复码失败: {e}")))?;
            let mut update = users::orm::Entity::update_many()
                .col_expr(users::orm::Column::TotpEnabled, Expr::value(enabled))
                .col_expr(
                    users::orm::Column::AuthEpoch,
                    Expr::col(users::orm::Column::AuthEpoch).add(1),
                )
                .col_expr(
                    users::orm::Column::TotpRecoveryJson,
                    Expr::value(hashes.unwrap_or_else(|| "[]".into())),
                )
                .col_expr(
                    users::orm::Column::UpdatedAtMs,
                    Expr::value(crate::clock::db_now().timestamp_millis()),
                );
            // 关闭两步验证时清空重放保护计数：重新开启后从全新窗口开始，
            // 旧记录不应让新验证器的码被误判为重放。开启时保持原值不动。
            if !enabled {
                update = update.col_expr(
                    users::orm::Column::TotpLastCounter,
                    Expr::value(Option::<i64>::None),
                );
            }
            update
                .filter(users::orm::Column::Id.eq(id))
                .exec(self.orm())
                .await?;
            Ok(())
        })
        .await
    }

    /// 消费一个 TOTP 时间步（重放保护，RFC 6238 §5.2）。
    ///
    /// 命中步必须严格大于该账号已记录的最近成功值；条件 UPDATE 即 CAS，两个并发
    /// 请求带同一个码时只有一个拿到 `true`，另一个按验证码错误处理。
    pub async fn consume_totp_step(&self, id: i64, step: u64) -> Result<bool> {
        self.timed("consume_totp_step", "nuono_users", async {
            let result = users::orm::Entity::update_many()
                .col_expr(
                    users::orm::Column::TotpLastCounter,
                    Expr::value(step as i64),
                )
                .col_expr(
                    users::orm::Column::UpdatedAtMs,
                    Expr::value(crate::clock::db_now().timestamp_millis()),
                )
                .filter(users::orm::Column::Id.eq(id))
                .filter(users::orm::Column::AccountKind.eq(users::ACCOUNT_KIND_REGISTERED))
                .filter(users::orm::Column::Status.eq(STATUS_ACTIVE))
                .filter(
                    Condition::any()
                        .add(users::orm::Column::TotpLastCounter.is_null())
                        .add(users::orm::Column::TotpLastCounter.lt(step as i64)),
                )
                .exec(self.orm())
                .await?;
            Ok(result.rows_affected == 1)
        })
        .await
    }

    /// 消费一个恢复码：哈希命中则从列表删除并返回 true，否则 false。
    ///
    /// 恢复码是一次性的；命中但更新失败会返回错误（而不是静默吞掉），
    /// 避免同一码被两次使用都「成功」。
    pub async fn consume_recovery_code(&self, id: i64, code_hash: &str) -> Result<bool> {
        self.timed("consume_recovery_code", "nuono_users", async {
            let raw = users::orm::Entity::find()
                .select_only()
                .column(users::orm::Column::TotpRecoveryJson)
                .filter(users::orm::Column::Id.eq(id))
                .filter(users::orm::Column::AccountKind.eq(users::ACCOUNT_KIND_REGISTERED))
                .filter(users::orm::Column::Status.eq(STATUS_ACTIVE))
                .into_tuple::<Option<String>>()
                .one(self.orm())
                .await?
                .flatten();
            let Some(raw) = raw else {
                return Ok(false);
            };
            let mut hashes: Vec<String> = serde_json::from_str(&raw)
                .map_err(|e| NuonoError::DatabaseQuery(format!("解析恢复码失败: {e}")))?;
            let Some(pos) = hashes.iter().position(|h| h == code_hash) else {
                return Ok(false);
            };
            hashes.remove(pos);
            // 回写带旧值条件（CAS）：两个并发请求带同一个恢复码时会读到同一份
            // 列表，无条件回写会让双方都成功。rows_affected == 0 说明已被并发消费。
            let result = users::orm::Entity::update_many()
                .col_expr(
                    users::orm::Column::TotpRecoveryJson,
                    Expr::value(serde_json::to_string(&hashes).map_err(|e| {
                        NuonoError::DatabaseQuery(format!("序列化恢复码失败: {e}"))
                    })?),
                )
                .filter(users::orm::Column::Id.eq(id))
                .filter(users::orm::Column::AccountKind.eq(users::ACCOUNT_KIND_REGISTERED))
                .filter(users::orm::Column::Status.eq(STATUS_ACTIVE))
                .filter(users::orm::Column::TotpRecoveryJson.eq(&raw))
                .exec(self.orm())
                .await?;
            Ok(result.rows_affected == 1)
        })
        .await
    }

    /// 更换账号邮箱，并标记为已验证。
    ///
    /// 只应在消费 `change_email` 令牌后调用：新地址收到并点击了链接，本身即完成验证。
    /// 邮箱唯一性由数据库的部分唯一索引兜底，这里先做一次显式检查以返回可读错误。
    #[cfg(test)]
    pub(crate) async fn set_user_email(&self, id: i64, email: &str) -> Result<()> {
        let email = email.trim().to_string();
        if email.is_empty() {
            return Err(NuonoError::ConfigValidation("邮箱不能为空".to_string()));
        }

        if let Some(existing) = self.find_user_by_email(&email).await?
            && existing.id != id
        {
            return Err(NuonoError::ConfigValidation(
                "该邮箱已被其他账号使用".to_string(),
            ));
        }

        self.timed("set_user_email", "nuono_users", async {
            let result = users::orm::Entity::update_many()
                .col_expr(users::orm::Column::Email, Expr::value(email.clone()))
                .col_expr(
                    users::orm::Column::EmailKey,
                    Expr::value(email.to_ascii_lowercase()),
                )
                .col_expr(
                    users::orm::Column::EmailVerifiedAtMs,
                    Expr::value(crate::clock::db_now().timestamp_millis()),
                )
                .col_expr(
                    users::orm::Column::UpdatedAtMs,
                    Expr::value(crate::clock::db_now().timestamp_millis()),
                )
                .filter(users::orm::Column::Id.eq(id))
                .exec(self.orm())
                .await?;
            if result.rows_affected == 0 {
                return Err(NuonoError::NotFound(format!("账号不存在: {id}")));
            }
            Ok(())
        })
        .await
    }

    /// 根据账号不可变主键（通过用户名解析）撤销该账号的所有持久化登录会话。
    #[cfg(test)]
    pub(crate) async fn revoke_user_sessions(&self, username: &str) -> Result<u64> {
        let Some(user) = self.find_user_by_username(username).await? else {
            return Ok(0);
        };
        let tx = self.begin_write().await?;
        let count = super::account_state::revoke_sessions_on(&tx, user.id).await?;
        tx.commit().await?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> Database {
        crate::test_support::test_db(2).await
    }

    fn owner(username: &str, password: &str) -> NewUser {
        NewUser {
            username: username.to_string(),
            email: Some("Owner@Example.COM".to_string()),
            password: password.to_string(),
            role: ROLE_OWNER.to_string(),
            status: STATUS_ACTIVE.to_string(),
            // 对齐首启向导：owner 邮箱由操作者直接录入。
            email_verified: true,
        }
    }

    #[tokio::test]
    async fn first_user_creation_gates_setup_and_hashes_password() {
        let db = test_db().await;
        assert_eq!(db.user_count().await.unwrap(), 0);

        let created = db
            .create_user(owner("root", "correct-horse"))
            .await
            .unwrap();
        assert_eq!(db.user_count().await.unwrap(), 1);
        assert_eq!(created.role, ROLE_OWNER);

        // 密码不得以明文落库。
        assert_ne!(created.password_hash, "correct-horse");
        assert!(created.password_hash.starts_with("sha256-bcrypt$"));
        assert!(!created.password_hash.contains("correct-horse"));

        // 邮箱统一小写，便于唯一性判断。
        assert_eq!(created.email.as_deref(), Some("owner@example.com"));

        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn database_write_paths_enforce_the_shared_password_policy() {
        let db = test_db().await;

        let short = db
            .create_user(owner("short", "too-short"))
            .await
            .unwrap_err();
        assert!(short.to_string().contains("10–72 个字符"));

        let user = db
            .create_user(owner("root", "correct-horse"))
            .await
            .unwrap();
        let too_long = "你".repeat(nuono_app::auth::MAX_PASSWORD_LEN + 1);
        let error = db.set_user_password(user.id, &too_long).await.unwrap_err();
        assert!(error.to_string().contains("10–72 个字符"));

        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn duplicate_username_or_email_is_rejected() {
        let db = test_db().await;
        db.create_user(owner("root", "correct-horse"))
            .await
            .unwrap();

        let dup_name = db.create_user(owner("root", "another-pass")).await;
        assert!(dup_name.is_err(), "重复用户名应被拒绝");

        let mut other = owner("second", "another-pass");
        other.email = Some("owner@example.com".to_string());
        let dup_email = db.create_user(other).await;
        assert!(dup_email.is_err(), "重复邮箱应被拒绝");

        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn password_verification_rejects_wrong_password_and_inactive_status() {
        let db = test_db().await;
        db.create_user(owner("root", "correct-horse"))
            .await
            .unwrap();

        assert!(
            db.verify_user_password("root", "correct-horse")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            db.verify_user_password("root", "wrong")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.verify_user_password("nobody", "correct-horse")
                .await
                .unwrap()
                .is_none(),
            "不存在的账号不应通过校验"
        );

        db.prepare_registration("buyer", "buyer@example.com", "buyer-pass-1")
            .await
            .unwrap();
        assert!(db.find_user_by_username("buyer").await.unwrap().is_none());
        assert!(
            db.verify_user_password("buyer", "buyer-pass-1")
                .await
                .unwrap()
                .is_none()
        );

        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn login_accepts_email_as_identifier() {
        let db = test_db().await;
        db.create_user(owner("root", "correct-horse"))
            .await
            .unwrap();

        // 邮箱可作为登录标识，且大小写不敏感（落库时已小写）。
        assert!(
            db.verify_user_password("owner@example.com", "correct-horse")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            db.verify_user_password("Owner@Example.COM", "correct-horse")
                .await
                .unwrap()
                .is_some()
        );

        // 邮箱通道同样受密码与状态约束，不是绕过口。
        assert!(
            db.verify_user_password("owner@example.com", "wrong")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.verify_user_password("nobody@example.com", "correct-horse")
                .await
                .unwrap()
                .is_none()
        );

        db.prepare_registration("buyer", "buyer@example.com", "buyer-pass-1")
            .await
            .unwrap();
        assert!(
            db.verify_user_password("buyer@example.com", "buyer-pass-1")
                .await
                .unwrap()
                .is_none(),
            "未验证账号走邮箱通道也不应能登录"
        );

        db.shutdown().await.unwrap();
    }

    async fn set_stored_password_hash(db: &Database, id: i64, hash: &str) {
        users::orm::Entity::update_many()
            .col_expr(
                users::orm::Column::PasswordHash,
                Expr::value(hash.to_string()),
            )
            .filter(users::orm::Column::Id.eq(id))
            .exec(db.orm())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn login_migrates_legacy_bcrypt_hash() {
        let db = test_db().await;
        let user = db
            .create_user(owner("legacy", "correct-horse"))
            .await
            .unwrap();
        let legacy_hash = bcrypt::hash("correct-horse", bcrypt::DEFAULT_COST).unwrap();
        set_stored_password_hash(&db, user.id, &legacy_hash).await;

        let verified = db
            .verify_user_password("legacy", "correct-horse")
            .await
            .unwrap()
            .unwrap();
        let migrated = db.find_user_by_id(user.id).await.unwrap().unwrap();
        assert!(migrated.password_hash.starts_with("sha256-bcrypt$"));
        assert_eq!(verified.password_hash, migrated.password_hash);
        assert!(
            db.create_session_for_verified_user(
                &verified,
                &format!("auth_session_{}", hex::encode([7_u8; 32])),
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .await
            .unwrap()
        );
        assert!(!bcrypt::verify("correct-horse", &migrated.password_hash).unwrap_or(false));

        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn login_does_not_migrate_legacy_hash_when_bcrypt_truncated_the_input() {
        let db = test_db().await;
        let original = "你".repeat(30);
        let user = db
            .create_user(owner("legacy-long", "correct-horse"))
            .await
            .unwrap();
        let legacy_hash = bcrypt::hash(&original, bcrypt::DEFAULT_COST).unwrap();
        set_stored_password_hash(&db, user.id, &legacy_hash).await;

        assert!(
            db.verify_user_password("legacy-long", &original)
                .await
                .unwrap()
                .is_some()
        );
        let stored = db.find_user_by_id(user.id).await.unwrap().unwrap();
        assert_eq!(stored.password_hash, legacy_hash);

        db.shutdown().await.unwrap();
    }

    #[test]
    fn password_hash_preserves_long_multilingual_input() {
        let password = "你".repeat(30);
        assert!(password.chars().count() < 72);
        assert!(password.len() > 72);
        let hash = hash_password(&password).unwrap();
        assert!(verify_password(&password, &hash));
        assert!(!verify_password(&(password + "x"), &hash));
    }

    #[tokio::test]
    async fn password_change_invalidates_old_password() {
        let db = test_db().await;
        let user = db
            .create_user(owner("root", "correct-horse"))
            .await
            .unwrap();

        db.set_user_password(user.id, "brand-new-pass")
            .await
            .unwrap();

        assert!(
            db.verify_user_password("root", "correct-horse")
                .await
                .unwrap()
                .is_none(),
            "旧密码应失效"
        );
        assert!(
            db.verify_user_password("root", "brand-new-pass")
                .await
                .unwrap()
                .is_some()
        );

        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn registration_allocates_an_account_only_after_mailbox_proof() {
        let db = test_db().await;
        db.create_user(owner("root", "correct-horse"))
            .await
            .unwrap();
        let request = db
            .prepare_registration("newcomer", "newcomer@example.com", "register-pass")
            .await
            .unwrap();
        assert!(
            db.find_user_by_username("newcomer")
                .await
                .unwrap()
                .is_none()
        );
        let (_, issued) = db
            .issue_registration_otp(&request.request_id, 10, "test-pepper")
            .await
            .unwrap();
        let proof = issued.unwrap();
        assert!(matches!(
            db.verify_registration_otp("newcomer@example.com", &proof.code, "test-pepper")
                .await
                .unwrap(),
            super::super::RegistrationOtpVerification::Verified { .. }
        ));
        let user = db.find_user_by_username("newcomer").await.unwrap().unwrap();
        assert_eq!(user.status, STATUS_ACTIVE);
        assert!(user.email_verified);
        assert!(
            db.verify_user_password("newcomer", "register-pass")
                .await
                .unwrap()
                .is_some()
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn verification_requests_cannot_resurrect_disabled_accounts() {
        let db = test_db().await;
        db.create_user(owner("root", "correct-horse"))
            .await
            .unwrap();
        let banned = db
            .create_user(NewUser {
                username: "banned".into(),
                email: Some("banned@example.com".into()),
                password: "some-password".into(),
                role: ROLE_USER.into(),
                status: STATUS_DISABLED.into(),
                email_verified: true,
            })
            .await
            .unwrap();
        assert!(
            db.prepare_registration("banned", "banned@example.com", "new-password")
                .await
                .is_err()
        );
        assert_eq!(
            db.find_user_by_id(banned.id).await.unwrap().unwrap().status,
            STATUS_DISABLED
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn changing_email_rejects_an_address_owned_by_another_account() {
        let db = test_db().await;
        db.create_user(owner("root", "correct-horse"))
            .await
            .unwrap();
        let second = db
            .create_user(NewUser {
                username: "second".to_string(),
                email: Some("second@example.com".to_string()),
                password: "second-pass".to_string(),
                role: ROLE_USER.to_string(),
                status: STATUS_ACTIVE.to_string(),
                email_verified: true,
            })
            .await
            .unwrap();

        // owner 已占用 owner@example.com。
        let conflict = db.set_user_email(second.id, "owner@example.com").await;
        assert!(conflict.is_err(), "抢占他人邮箱应被拒绝");

        db.set_user_email(second.id, "moved@example.com")
            .await
            .unwrap();
        let moved = db.find_user_by_id(second.id).await.unwrap().unwrap();
        assert_eq!(moved.email.as_deref(), Some("moved@example.com"));
        assert!(moved.email_verified, "新地址点击链接即视为已验证");

        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn revoking_sessions_only_removes_the_target_users_sessions() {
        let db = test_db().await;
        let root = db
            .create_user(owner("root", "correct-horse-battery"))
            .await
            .unwrap();
        let other = db
            .create_user(NewUser {
                email: Some("other@example.com".into()),
                ..owner("other", "correct-horse-battery")
            })
            .await
            .unwrap();
        for (user, token) in [(&root, "aaa"), (&root, "bbb"), (&other, "ccc")] {
            let key = format!(
                "auth_session_{}",
                hex::encode(super::super::account_state::token_hash(token))
            );
            db.create_session_for_verified_user(
                user,
                &key,
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .await
            .unwrap();
        }
        db.set_global_config("admin_theme", serde_json::json!("dark"))
            .await
            .unwrap();
        assert_eq!(db.revoke_user_sessions("root").await.unwrap(), 2);
        assert!(db.authenticate_session("aaa").await.unwrap().is_none());
        assert!(db.authenticate_session("bbb").await.unwrap().is_none());
        assert!(db.authenticate_session("ccc").await.unwrap().is_some());
        assert!(db.get_global_config("admin_theme").await.unwrap().is_some());
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn totp_state_and_recovery_codes_roundtrip() {
        let db = test_db().await;
        let user = owner("totp-owner", "correct-horse-battery");
        let id = db.create_user(user).await.unwrap().id;

        // enroll 阶段：secret 落库但未启用
        let secret = nuono_app::auth::generate_secret();
        db.set_totp_secret(id, Some(&secret)).await.unwrap();
        let stored = db.find_user_by_id(id).await.unwrap().unwrap();
        assert_eq!(stored.totp_secret.as_deref(), Some(secret.as_str()));
        assert!(!stored.totp_enabled);

        // confirm：启用 + 恢复码哈希
        let codes = nuono_app::auth::generate_recovery_codes();
        let hashes: Vec<String> = codes
            .iter()
            .map(|code| nuono_app::auth::recovery_code_hash(code))
            .collect();
        db.set_totp_state(id, true, Some(&hashes)).await.unwrap();
        let stored = db.find_user_by_id(id).await.unwrap().unwrap();
        assert!(stored.totp_enabled);

        // 恢复码一次性 + 大小写折叠
        assert!(db.consume_recovery_code(id, &hashes[0]).await.unwrap());
        assert!(
            !db.consume_recovery_code(id, &hashes[0]).await.unwrap(),
            "恢复码必须一次性"
        );
        let folded = nuono_app::auth::recovery_code_hash(&codes[1].to_lowercase());
        assert!(
            db.consume_recovery_code(id, &folded).await.unwrap(),
            "小写输入应命中同一码"
        );

        // 关闭：清空 secret 与恢复码
        db.set_totp_state(id, false, None).await.unwrap();
        db.set_totp_secret(id, None).await.unwrap();
        let stored = db.find_user_by_id(id).await.unwrap().unwrap();
        assert!(!stored.totp_enabled);
        assert!(stored.totp_recovery.is_none());
        assert!(stored.totp_secret.is_none());

        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn totp_step_consumption_is_cas_across_concurrent_claims() {
        let db = test_db().await;
        let id = db
            .create_user(owner("totp-cas", "correct-horse"))
            .await
            .unwrap()
            .id;

        // 同一步的两个并发消费只能有一个赢；之后更高的步仍可消费。
        let (a, b) = tokio::join!(db.consume_totp_step(id, 42), db.consume_totp_step(id, 42));
        assert_eq!(
            [a.unwrap(), b.unwrap()]
                .into_iter()
                .filter(|won| *won)
                .count(),
            1,
            "同一时间步并发消费只能成功一次"
        );
        assert!(
            !db.consume_totp_step(id, 42).await.unwrap(),
            "同步重放必须被拒"
        );
        assert!(
            db.consume_totp_step(id, 43).await.unwrap(),
            "更高步应被接受"
        );
        assert!(
            !db.consume_totp_step(id, 43).await.unwrap(),
            "低值或同值不得覆盖已记录步"
        );

        // 关闭并重新开启两步验证后，重放计数从全新窗口开始。
        db.set_totp_state(id, false, None).await.unwrap();
        assert!(
            db.consume_totp_step(id, 42).await.unwrap(),
            "重开后旧窗口不应再限制新验证器"
        );

        db.shutdown().await.unwrap();
    }

    /// 与 OTP / 令牌并发用例同一套防御：首启是幂等语义，瞬时错误重试不改变结论。
    async fn create_first_owner_with_retry(db: &Database, name: &str) -> Option<users::Model> {
        for attempt in 0..3 {
            match db
                .create_first_owner(NewUser {
                    username: name.to_string(),
                    email: Some(format!("{name}@example.com")),
                    password: "correct-horse".to_string(),
                    role: ROLE_OWNER.to_string(),
                    status: STATUS_ACTIVE.to_string(),
                    email_verified: true,
                })
                .await
            {
                Ok(outcome) => return outcome,
                Err(e) if attempt < 2 => {
                    tracing::warn!(error = %e, attempt, "首启创建瞬时失败，稍后重试");
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                }
                Err(e) => panic!("首启创建连续失败: {e}"),
            }
        }
        unreachable!()
    }

    #[tokio::test]
    async fn concurrent_setup_wizards_initialize_exactly_once() {
        let db = test_db().await;

        // 两个并发首启请求用不同用户名：锁内复查必须保证只有一个 owner 被建出。
        let (a, b) = tokio::join!(
            create_first_owner_with_retry(&db, "wizard-a"),
            create_first_owner_with_retry(&db, "wizard-b")
        );
        let winners: Vec<_> = [a, b].into_iter().flatten().collect();
        assert_eq!(winners.len(), 1, "并发首启只能成功一次");
        assert_eq!(winners[0].id, 1);
        assert_eq!(db.user_count().await.unwrap(), 1);
        assert_eq!(
            db.primary_owner_username().await.unwrap().unwrap(),
            winners[0].username,
            "保存的 owner 必须与唯一成功的请求一致"
        );

        // 已初始化后再调用：返回 None，且不再新增账号。
        let again = db
            .create_first_owner(NewUser {
                username: "wizard-c".to_string(),
                email: None,
                password: "correct-horse".to_string(),
                role: ROLE_OWNER.to_string(),
                status: STATUS_ACTIVE.to_string(),
                email_verified: true,
            })
            .await
            .unwrap();
        assert!(again.is_none(), "已初始化后首启入口必须关闭");
        assert_eq!(db.user_count().await.unwrap(), 1);

        db.shutdown().await.unwrap();
    }
}
