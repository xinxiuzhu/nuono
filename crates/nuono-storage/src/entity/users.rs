//! 登录账号实体。
//!
//! `password_hash` 是带版本标识的 bcrypt 哈希，不接受明文；写入路径见 `db::users`。
//!
//! ORM 是持久字段的唯一映射；公开 Model 只向现有认证服务提供便利视图。

/// 账号类型：交互懒创建的占位账号（无法登录 Web）。
pub const ACCOUNT_KIND_SHADOW: &str = "shadow";
/// 账号类型：已完成注册/转正的正式账号。
pub const ACCOUNT_KIND_REGISTERED: &str = "registered";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    pub id: i64,
    pub account_kind: String,
    pub auth_epoch: i64,
    pub username: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub password_hash: String,
    /// TOTP secret（base32 明文，可逆验码所需）；NULL = 未开启两步验证
    pub totp_secret: Option<String>,
    pub totp_enabled: bool,
    /// 恢复码 SHA-256 列表（JSON 数组文本）；用掉一个删一个
    pub totp_recovery: Option<String>,
    pub role: String,
    pub status: String,
    pub last_login_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// 首发账号表：区分占位账号与正式账号，保留唯一规范键、凭据版本、墓碑状态与 UTC 毫秒。
pub mod orm {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "nuono_users")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub account_kind: String,
        pub username: Option<String>,
        pub username_key: Option<String>,
        pub auth_epoch: i64,
        pub revision: i64,
        pub merged_into_user_id: Option<i64>,
        pub email: Option<String>,
        pub email_key: Option<String>,
        pub email_verified_at_ms: Option<i64>,
        pub password_hash: Option<String>,
        pub role: String,
        pub status: String,
        pub totp_secret: Option<Vec<u8>>,
        pub totp_enabled: bool,
        pub totp_recovery_json: String,
        /// TOTP 重放保护：该账号最近一次通过校验的时间步。
        pub totp_last_counter: Option<i64>,
        pub last_login_at_ms: Option<i64>,
        pub display_name: String,
        pub birthday: String,
        pub gender: String,
        pub bio: String,
        pub use_for_ai: bool,
        pub created_at_ms: i64,
        pub updated_at_ms: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

impl From<orm::Model> for Model {
    fn from(m: orm::Model) -> Self {
        Model {
            id: m.id,
            account_kind: m.account_kind,
            auth_epoch: m.auth_epoch,
            username: m.username.unwrap_or_default(),
            email: m.email,
            email_verified: m.email_verified_at_ms.is_some(),
            password_hash: m.password_hash.unwrap_or_default(),
            totp_secret: m
                .totp_secret
                .and_then(|value| String::from_utf8(value).ok()),
            totp_enabled: m.totp_enabled,
            totp_recovery: (m.totp_recovery_json != "[]").then_some(m.totp_recovery_json),
            role: m.role,
            status: m.status,
            last_login_at: m
                .last_login_at_ms
                .and_then(chrono::DateTime::from_timestamp_millis),
            created_at: chrono::DateTime::from_timestamp_millis(m.created_at_ms)
                .unwrap_or_default(),
            updated_at: chrono::DateTime::from_timestamp_millis(m.updated_at_ms)
                .unwrap_or_default(),
        }
    }
}

impl From<Model> for orm::ActiveModel {
    fn from(m: Model) -> Self {
        let account_kind = if m.account_kind.is_empty() {
            ACCOUNT_KIND_REGISTERED.to_owned()
        } else {
            m.account_kind
        };
        let is_shadow = account_kind == ACCOUNT_KIND_SHADOW;
        orm::ActiveModel {
            id: sea_orm::ActiveValue::Set(m.id),
            account_kind: sea_orm::ActiveValue::Set(account_kind),
            username_key: sea_orm::ActiveValue::Set(
                (!is_shadow).then(|| m.username.to_ascii_lowercase()),
            ),
            username: sea_orm::ActiveValue::Set((!is_shadow).then_some(m.username)),
            auth_epoch: sea_orm::ActiveValue::Set(m.auth_epoch),
            revision: sea_orm::ActiveValue::NotSet,
            merged_into_user_id: sea_orm::ActiveValue::NotSet,
            email_key: sea_orm::ActiveValue::Set(
                m.email
                    .as_ref()
                    .map(|value| value.trim().to_ascii_lowercase()),
            ),
            email_verified_at_ms: sea_orm::ActiveValue::Set(
                m.email.as_ref().map(|_| m.updated_at.timestamp_millis()),
            ),
            email: sea_orm::ActiveValue::Set(m.email),
            password_hash: sea_orm::ActiveValue::Set((!is_shadow).then_some(m.password_hash)),
            role: sea_orm::ActiveValue::Set(m.role),
            status: sea_orm::ActiveValue::Set(m.status),
            totp_secret: sea_orm::ActiveValue::Set(m.totp_secret.map(String::into_bytes)),
            totp_enabled: sea_orm::ActiveValue::Set(m.totp_enabled),
            totp_recovery_json: sea_orm::ActiveValue::Set(
                m.totp_recovery.unwrap_or_else(|| "[]".into()),
            ),
            // `Model` 没有该列：置 NotSet，更新时保留库中旧值、插入时为 NULL。
            totp_last_counter: sea_orm::ActiveValue::NotSet,
            last_login_at_ms: sea_orm::ActiveValue::Set(
                m.last_login_at.map(|value| value.timestamp_millis()),
            ),
            display_name: sea_orm::ActiveValue::NotSet,
            birthday: sea_orm::ActiveValue::NotSet,
            gender: sea_orm::ActiveValue::NotSet,
            bio: sea_orm::ActiveValue::NotSet,
            use_for_ai: sea_orm::ActiveValue::NotSet,
            created_at_ms: sea_orm::ActiveValue::Set(m.created_at.timestamp_millis()),
            updated_at_ms: sea_orm::ActiveValue::Set(m.updated_at.timestamp_millis()),
        }
    }
}

impl nuono_app::auth::AccountIdentity for crate::entity::users::Model {
    fn account_id(&self) -> i64 {
        self.id
    }
    fn account_username(&self) -> &str {
        &self.username
    }
    fn account_role(&self) -> &str {
        &self.role
    }
}
