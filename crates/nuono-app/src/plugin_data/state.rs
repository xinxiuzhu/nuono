//! 插件共享状态的地址与值契约。

use nuono_domain::{DomainEvent, IdentityKind};
use nuono_error::{NuonoError, Result};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// 由宿主领域身份生成的插件状态作用域。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginStateScope(String);

impl PluginStateScope {
    /// 插件内跨机器人共享的作用域。
    pub fn global() -> Self {
        Self("global".into())
    }

    /// 按框架账号划分状态，跨网页和已验证的平台身份共享。
    pub fn account(user_id: i64) -> Result<Self> {
        if user_id <= 0 {
            return Err(NuonoError::InvalidInput("账号 ID 必须为正整数".into()));
        }
        Ok(Self(format!("account:{user_id}")))
    }

    /// 按完整机器人身份和当前用户划分状态。
    ///
    /// # Errors
    /// 事件缺少同平台的用户身份时返回错误。
    pub fn user(event: &DomainEvent) -> Result<Self> {
        let actor = event
            .actor
            .as_ref()
            .filter(|actor| {
                *actor.kind() == IdentityKind::User && actor.platform() == event.bot.platform()
            })
            .ok_or_else(|| NuonoError::InvalidInput("插件用户状态需要原生用户身份".into()))?;
        Self::from_identity("user", &(&event.bot, actor))
    }

    /// 按完整机器人身份和当前私聊、群或频道划分状态。
    ///
    /// # Errors
    /// 事件缺少有效的同平台会话时返回错误。
    pub fn conversation(event: &DomainEvent) -> Result<Self> {
        let conversation = event
            .conversation
            .as_ref()
            .filter(|conversation| conversation.platform() == event.bot.platform())
            .ok_or_else(|| NuonoError::InvalidInput("插件会话状态需要原生会话身份".into()))?;
        conversation
            .validate()
            .map_err(|error| NuonoError::InvalidInput(error.to_string()))?;
        Self::from_identity("conversation", &(&event.bot, conversation))
    }

    fn from_identity(kind: &str, identity: &impl Serialize) -> Result<Self> {
        let bytes = serde_json::to_vec(identity)?;
        Ok(Self(format!(
            "{kind}:v1:{}",
            hex::encode(Sha256::digest(bytes))
        )))
    }
}

/// 宿主传给状态仓储的插件、集合与作用域。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginStateCollection {
    plugin_id: String,
    name: String,
    scope: PluginStateScope,
}

impl PluginStateCollection {
    /// 绑定一个已注册插件的状态集合。
    ///
    /// # Errors
    /// 插件 ID 或集合名不符合持久化命名规则时返回错误。
    pub fn new(plugin_id: &str, name: &str, scope: PluginStateScope) -> Result<Self> {
        if plugin_id.is_empty()
            || plugin_id.len() > 64
            || nuono_domain::deployment::is_reserved_plugin_name(plugin_id)
            || !plugin_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(NuonoError::InvalidInput("插件状态需要有效的插件 ID".into()));
        }
        if name.len() > 64
            || !name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(NuonoError::InvalidInput(
                "插件状态集合必须以小写字母开头，限 64 字节的小写字母、数字和下划线".into(),
            ));
        }
        Ok(Self {
            plugin_id: plugin_id.into(),
            name: name.into(),
            scope,
        })
    }

    /// 此集合所属的稳定插件 ID。
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// 插件定义的稳定集合名。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 仓储使用的规范作用域键。
    pub fn scope_key(&self) -> &str {
        &self.scope.0
    }
}

/// 一个 JSON 值和插件定义的格式版本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginStateValue {
    /// 取值必须为正整数。升级时用它选择对应的数据结构。
    pub value_version: i32,
    /// 一个完整 JSON 值。
    pub value: Value,
}

/// 状态记录的乐观锁戳记（包含实例周期号 `generation` 与递增版本号 `revision`，确保某个键被删除又重建后不会误匹配旧戳记）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PluginStateStamp {
    pub generation: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub revision: i64,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PluginStateRecord {
    pub value_version: i32,
    pub value: Value,
    pub generation: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub revision: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub created_at_ms: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub updated_at_ms: i64,
    #[serde(default, with = "nuono_domain::scalar::optional_i64_string")]
    pub expires_at_ms: Option<i64>,
}
impl PluginStateRecord {
    pub fn stamp(&self) -> PluginStateStamp {
        PluginStateStamp {
            generation: self.generation.clone(),
            revision: self.revision,
        }
    }
}
