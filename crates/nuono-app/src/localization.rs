//! 多语言偏好解析与存储接口（仅决定展示语言，不影响命令路由与会话身份）。
//! 用户和会话级覆盖由存储层持久化，系统默认语言由宿主运行时动态提供。

use nuono_domain::{BotIdentity, ConversationRef, IdentityKind, IdentityRef};
use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt,
    str::FromStr,
    sync::{Arc, RwLock},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Locale {
    #[default]
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en")]
    En,
    #[serde(rename = "ja")]
    Ja,
    #[serde(rename = "ko")]
    Ko,
}

impl Locale {
    pub const ALL: [Self; 4] = [Self::ZhCn, Self::En, Self::Ja, Self::Ko];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::En => "en",
            Self::Ja => "ja",
            Self::Ko => "ko",
        }
    }

    pub const fn native_name(self) -> &'static str {
        match self {
            Self::ZhCn => "简体中文",
            Self::En => "English",
            Self::Ja => "日本語",
            Self::Ko => "한국어",
        }
    }

    /// 显式设置只接受四个规范值；缺失表示继承。
    pub fn parse_explicit(value: &str) -> Result<Self> {
        match value {
            "zh-CN" => Ok(Self::ZhCn),
            "en" => Ok(Self::En),
            "ja" => Ok(Self::Ja),
            "ko" => Ok(Self::Ko),
            _ => Err(NuonoError::Validation(
                "语言必须为 zh-CN、en、ja 或 ko；恢复继承请清除语言覆盖".into(),
            )),
        }
    }

    /// 解析标准语言标签提示（如 `language[-script][-region]`），仅用于辅助归一化，不触发自动语言切换。
    /// 不支持的文字体系、变体或格式错误的标签均返回 `None`（例如繁体中文 `zh-TW`/`zh-Hant` 不会静默回退为简体中文 `zh-CN`）。
    pub fn from_language_tag(value: &str) -> Option<Self> {
        let tag = value.trim().replace('_', "-").to_ascii_lowercase();
        let parts: Vec<_> = tag.split('-').collect();
        let (locale, script) = match *parts.first()? {
            "zh" => (Self::ZhCn, "hans"),
            "en" => (Self::En, "latn"),
            "ja" => (Self::Ja, "jpan"),
            "ko" => (Self::Ko, "kore"),
            _ => return None,
        };
        let mut rest = &parts[1..];
        if rest.first().is_some_and(|part| part.len() == 4) {
            if rest[0] != script {
                return None;
            }
            rest = &rest[1..];
        }
        match rest {
            [] => Some(locale),
            [region] if locale == Self::ZhCn => matches!(*region, "cn" | "sg").then_some(locale),
            [region]
                if (region.len() == 2 && region.bytes().all(|c| c.is_ascii_alphabetic()))
                    || (region.len() == 3 && region.bytes().all(|c| c.is_ascii_digit())) =>
            {
                Some(locale)
            }
            _ => None,
        }
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Locale {
    type Err = NuonoError;
    fn from_str(value: &str) -> Result<Self> {
        Self::parse_explicit(value)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocaleSource {
    Explicit,
    User,
    Conversation,
    #[default]
    System,
}

/// 单次请求解析出的语言偏好快照（翻译文案缺失时的词条级回退由文案目录另行处理）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocaleContext {
    pub locale: Locale,
    pub source: LocaleSource,
}

/// 语言偏好的作用范围：绑定到具体机器人实例及完整的平台原生身份（含命名空间、会话类型与可选父级频道），绝不仅凭昵称或裸群号匹配。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LocaleScope {
    User {
        bot: BotIdentity,
        user: IdentityRef,
    },
    Conversation {
        bot: BotIdentity,
        conversation: ConversationRef,
    },
}

impl LocaleScope {
    pub fn user(bot: BotIdentity, user: IdentityRef) -> Result<Self> {
        let scope = Self::User { bot, user };
        scope.validate()?;
        Ok(scope)
    }

    pub fn conversation(bot: BotIdentity, conversation: ConversationRef) -> Result<Self> {
        let scope = Self::Conversation { bot, conversation };
        scope.validate()?;
        Ok(scope)
    }

    pub fn validate(&self) -> Result<()> {
        // 领域类型的字段是公开的，因此通过一次序列化/反序列化触发完整的字段校验，
        // 并核对跨字段平台一致性，防止调用方绕过构造函数直接拼装非法对象。
        let checked: Self = serde_json::from_value(serde_json::to_value(self)?)?;
        let valid = match &checked {
            Self::User { bot, user } => {
                *user.kind() == IdentityKind::User && bot.platform() == user.platform()
            }
            Self::Conversation { bot, conversation } => bot.platform() == conversation.platform(),
        };
        if !valid {
            return Err(NuonoError::Validation(
                "语言偏好的机器人与用户或会话身份不一致".into(),
            ));
        }
        Ok(())
    }

    /// 生成用于计算存储键的规范化 JSON（不含昵称等易变展示字段）。
    pub fn canonical_json(&self) -> Result<String> {
        self.validate()?;
        Ok(serde_json::to_string(self)?)
    }

    pub fn canonical_key(&self) -> Result<String> {
        Ok(format!(
            "v1:{}",
            hex::encode(Sha256::digest(self.canonical_json()?.as_bytes()))
        ))
    }

    pub const fn source(&self) -> LocaleSource {
        match self {
            Self::User { .. } => LocaleSource::User,
            Self::Conversation { .. } => LocaleSource::Conversation,
        }
    }

    pub fn bot(&self) -> &BotIdentity {
        match self {
            Self::User { bot, .. } | Self::Conversation { bot, .. } => bot,
        }
    }
}

#[async_trait::async_trait]
pub trait LocalePreferenceStore: Send + Sync {
    async fn get_locale_preference(&self, scope: &LocaleScope) -> Result<Option<Locale>>;
    /// 传入 `None` 表示移除当前作用域的语言覆盖并恢复继承（不得将当前继承来的语言值写死到存储中）。
    async fn set_locale_preference(
        &self,
        scope: &LocaleScope,
        locale: Option<Locale>,
    ) -> Result<()>;
}

/// 系统默认语言提供方接口（宿主可通过此接口实时读取最新系统配置，供后台主动推送等无入站事件场景使用）。
#[async_trait::async_trait]
pub trait SystemLocaleProvider: Send + Sync {
    async fn system_locale(&self) -> Result<Locale>;
}

/// 面向嵌入式运行时与单元测试的内存语言偏好存储（各实例状态相互独立）。
#[derive(Default)]
pub struct MemoryLocalePreferenceStore {
    values: RwLock<HashMap<String, Locale>>,
}

#[async_trait::async_trait]
impl LocalePreferenceStore for MemoryLocalePreferenceStore {
    async fn get_locale_preference(&self, scope: &LocaleScope) -> Result<Option<Locale>> {
        let key = scope.canonical_key()?;
        Ok(self
            .values
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .copied())
    }

    async fn set_locale_preference(
        &self,
        scope: &LocaleScope,
        locale: Option<Locale>,
    ) -> Result<()> {
        let key = scope.canonical_key()?;
        let mut values = self.values.write().unwrap_or_else(|e| e.into_inner());
        match locale {
            Some(locale) => {
                values.insert(key, locale);
            }
            None => {
                values.remove(&key);
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct LocaleService {
    store: Arc<dyn LocalePreferenceStore>,
    system: Arc<RwLock<Locale>>,
    system_provider: Option<Arc<dyn SystemLocaleProvider>>,
}

impl Default for LocaleService {
    fn default() -> Self {
        Self::new(Arc::new(MemoryLocalePreferenceStore::default()))
    }
}

impl LocaleService {
    pub fn new(store: Arc<dyn LocalePreferenceStore>) -> Self {
        Self {
            store,
            system: Arc::new(RwLock::new(Locale::default())),
            system_provider: None,
        }
    }

    pub fn with_system_locale_provider(mut self, provider: Arc<dyn SystemLocaleProvider>) -> Self {
        self.system_provider = Some(provider);
        self
    }

    /// 设置内存回退的系统默认语言（若已通过 `with_system_locale_provider` 注入宿主配置提供方，则始终以宿主提供方为准）。
    pub fn set_system_locale(&self, locale: Locale) {
        *self.system.write().unwrap_or_else(|e| e.into_inner()) = locale;
    }

    pub async fn system_locale(&self) -> Result<Locale> {
        match &self.system_provider {
            Some(provider) => provider.system_locale().await,
            None => Ok(*self.system.read().unwrap_or_else(|e| e.into_inner())),
        }
    }

    pub async fn get_preference(&self, scope: &LocaleScope) -> Result<Option<Locale>> {
        scope.validate()?;
        self.store.get_locale_preference(scope).await
    }

    /// 设置或清除指定作用域的语言偏好（调用方需在业务层先行完成权限校验）。
    pub async fn set_preference(&self, scope: &LocaleScope, locale: Option<Locale>) -> Result<()> {
        scope.validate()?;
        self.store.set_locale_preference(scope, locale).await
    }

    pub async fn resolve(
        &self,
        explicit: Option<Locale>,
        user: Option<&LocaleScope>,
        conversation: Option<&LocaleScope>,
    ) -> Result<LocaleContext> {
        for (scope, expected) in [
            (user, LocaleSource::User),
            (conversation, LocaleSource::Conversation),
        ] {
            if let Some(scope) = scope {
                scope.validate()?;
                if scope.source() != expected {
                    return Err(NuonoError::Validation("语言偏好作用域类型不匹配".into()));
                }
            }
        }
        if let (Some(user), Some(conversation)) = (user, conversation)
            && user.bot() != conversation.bot()
        {
            return Err(NuonoError::Validation(
                "语言偏好的用户与会话不属于同一机器人".into(),
            ));
        }
        if let Some(locale) = explicit {
            return Ok(LocaleContext {
                locale,
                source: LocaleSource::Explicit,
            });
        }
        for scope in [user, conversation].into_iter().flatten() {
            if let Some(locale) = self.store.get_locale_preference(scope).await? {
                return Ok(LocaleContext {
                    locale,
                    source: scope.source(),
                });
            }
        }
        Ok(LocaleContext {
            locale: self.system_locale().await?,
            source: LocaleSource::System,
        })
    }

    /// 解析主动推送目标的语言偏好（仅根据接收方用户或目标会话解析，不受最后一次发言人或订阅创建者语言的影响）。
    pub async fn resolve_target(
        &self,
        explicit: Option<Locale>,
        target: &LocaleScope,
    ) -> Result<LocaleContext> {
        match target {
            LocaleScope::User { .. } => self.resolve(explicit, Some(target), None).await,
            LocaleScope::Conversation { .. } => self.resolve(explicit, None, Some(target)).await,
        }
    }
}

#[cfg(test)]
#[path = "localization_tests.rs"]
mod tests;
