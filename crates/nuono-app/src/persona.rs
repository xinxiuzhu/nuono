//! AI 角色卡定义、群/频道会话精确绑定以及 TTS 语音合成抽象接口。
use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersonaRecord {
    pub id: String,
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    pub enabled: bool,
    pub avatar_filename: Option<String>,
    pub portrait_filename: Option<String>,
    pub reference_audio_filename: Option<String>,
    pub reference_text: String,
    pub language: String,
    pub tts_provider: Option<String>,
    pub voice_id: Option<String>,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub revision: i64,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PersonaInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
    #[serde(default)]
    pub reference_text: String,
    #[serde(default = "language_default")]
    pub language: String,
    #[serde(default)]
    pub tts_provider: Option<String>,
    #[serde(default)]
    pub voice_id: Option<String>,
}
fn enabled_default() -> bool {
    true
}
fn language_default() -> String {
    "zh".into()
}
impl Default for PersonaInput {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            system_prompt: String::new(),
            enabled: true,
            reference_text: String::new(),
            language: language_default(),
            tts_provider: None,
            voice_id: None,
        }
    }
}
impl PersonaInput {
    pub fn validate(&self) -> Result<()> {
        bounded(&self.name, "角色名称", 512, false)?;
        bounded(&self.description, "角色说明", 4096, true)?;
        bounded(&self.system_prompt, "性格设定", 65536, true)?;
        bounded(&self.reference_text, "参考音频文本", 16384, true)?;
        bounded(&self.language, "语言", 32, false)?;
        for (value, label, max) in [
            (&self.tts_provider, "TTS 服务标识", 64),
            (&self.voice_id, "音色标识", 256),
        ] {
            if let Some(value) = value {
                bounded(value, label, max, false)?;
            }
        }
        Ok(())
    }
}
fn bounded(value: &str, label: &str, max: usize, empty: bool) -> Result<()> {
    if value.len() > max || (!empty && value.trim().is_empty()) || value.contains('\0') {
        return Err(NuonoError::Validation(format!(
            "{label}为空、过长或包含无效字符"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PersonaBindingKey {
    pub platform: String,
    pub bot_id: String,
    pub group_id: String,
    pub namespace: Option<String>,
}
impl PersonaBindingKey {
    pub fn validate(&self) -> Result<()> {
        if !matches!(
            self.platform.as_str(),
            "onebot" | "telegram" | "discord" | "feishu" | "qq_official"
        ) {
            return Err(NuonoError::Validation("不支持的角色绑定平台".into()));
        }
        bounded(&self.bot_id, "机器人标识", 512, false)?;
        bounded(&self.group_id, "群或频道标识", 512, false)?;
        if let Some(namespace) = &self.namespace {
            bounded(namespace, "平台命名空间", 512, false)?;
        }
        if matches!(self.platform.as_str(), "feishu" | "qq_official") && self.namespace.is_none() {
            return Err(NuonoError::Validation(
                "此平台的角色绑定需要应用命名空间".into(),
            ));
        }
        Ok(())
    }
    /// 对绑定四元组做 SHA-256 哈希以生成规范唯一键（原始平台 ID 仍保留在数据行中供前端展示）。
    pub fn canonical_key(&self) -> String {
        hex::encode(Sha256::digest(
            serde_json::to_vec(&(
                &self.platform,
                &self.bot_id,
                &self.group_id,
                &self.namespace,
            ))
            .expect("binding tuple serializes"),
        ))
    }
    pub fn from_context(ctx: &crate::agent::AgentContext) -> Option<Self> {
        use nuono_domain::ConversationRef;
        let key = if let Some(native) = &ctx.native {
            let group = match native.conversation.as_ref()? {
                ConversationRef::Group { group } => group,
                ConversationRef::Channel { channel, .. } => channel,
                ConversationRef::Direct { .. } | _ => return None,
            };
            Self {
                platform: native.bot.platform().to_string(),
                bot_id: native.bot.account_id.value().to_string(),
                group_id: group.value().to_string(),
                namespace: group
                    .namespace()
                    .as_ref()
                    .or(native.bot.account_id.namespace().as_ref())
                    .map(ToString::to_string),
            }
        } else {
            Self {
                platform: ctx.platform.clone(),
                bot_id: ctx.bot_id.to_string(),
                group_id: ctx
                    .channel_id
                    .clone()
                    .or_else(|| ctx.group_id.map(|id| id.to_string()))?,
                namespace: None,
            }
        };
        // 自定义平台或缺少命名空间的旧上下文直接返回 None 并回退到全局 AI 提示词；
        // 只有管理员显式创建角色绑定时才会将校验错误直接返回给调用方。
        key.validate().ok()?;
        Some(key)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PersonaBinding {
    pub platform: String,
    pub bot_id: String,
    pub group_id: String,
    pub namespace: Option<String>,
    pub persona_id: String,
}
impl PersonaBinding {
    pub fn key(&self) -> PersonaBindingKey {
        PersonaBindingKey {
            platform: self.platform.clone(),
            bot_id: self.bot_id.clone(),
            group_id: self.group_id.clone(),
            namespace: self.namespace.clone(),
        }
    }
    pub fn validate(&self) -> Result<()> {
        self.key().validate()?;
        validate_persona_id(&self.persona_id)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PersonaAssetKind {
    Avatar,
    Portrait,
    ReferenceAudio,
}
pub fn validate_persona_id(id: &str) -> Result<()> {
    let valid = id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        });
    if valid {
        Ok(())
    } else {
        Err(NuonoError::Validation("角色标识必须是小写 UUID".into()))
    }
}
pub fn validate_asset_filename(kind: PersonaAssetKind, filename: &str) -> Result<()> {
    let Some((id, extension)) = filename.rsplit_once('.') else {
        return Err(NuonoError::Validation("角色素材文件名无效".into()));
    };
    validate_persona_id(id)?;
    let valid = match kind {
        PersonaAssetKind::Avatar | PersonaAssetKind::Portrait => {
            matches!(extension, "png" | "jpg" | "jpeg" | "webp")
        }
        PersonaAssetKind::ReferenceAudio => {
            matches!(extension, "wav" | "mp3" | "ogg" | "flac" | "m4a")
        }
    };
    if valid {
        Ok(())
    } else {
        Err(NuonoError::Validation("角色素材格式不支持".into()))
    }
}

/// 任务启动时捕获的角色卡快照（任务执行期间固定使用该版本及其素材，不受后续管理员编辑角色卡的影响）。
pub type PersonaSnapshot = PersonaRecord;

#[async_trait::async_trait]
pub trait PersonaStore: Send + Sync {
    async fn list_ai_personas(&self) -> Result<Vec<PersonaRecord>>;
    async fn get_ai_persona(&self, id: &str) -> Result<Option<PersonaRecord>>;
    async fn create_ai_persona(&self, id: &str, input: PersonaInput) -> Result<PersonaRecord>;
    async fn update_ai_persona(
        &self,
        id: &str,
        expected_revision: i64,
        input: PersonaInput,
    ) -> Result<PersonaRecord>;
    async fn delete_ai_persona(&self, id: &str, expected_revision: i64) -> Result<()>;
    async fn set_persona_asset(
        &self,
        id: &str,
        kind: PersonaAssetKind,
        filename: Option<String>,
        expected_revision: i64,
    ) -> Result<PersonaRecord>;
    async fn list_ai_persona_bindings(&self) -> Result<Vec<PersonaBinding>>;
    async fn upsert_ai_persona_binding(&self, binding: PersonaBinding) -> Result<()>;
    async fn remove_ai_persona_binding(&self, key: &PersonaBindingKey) -> Result<()>;
    async fn resolve_ai_persona(&self, key: &PersonaBindingKey) -> Result<Option<PersonaRecord>>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersonaAssetRef {
    pub persona_id: String,
    pub filename: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsRequest {
    pub text: String,
    pub reference_audio: Option<PersonaAssetRef>,
    pub reference_text: String,
    pub language: String,
    pub voice_id: Option<String>,
}
#[derive(Debug, Clone)]
pub struct TtsAudio {
    pub bytes: Vec<u8>,
    pub content_type: String,
    pub extension: String,
}
#[derive(Debug, thiserror::Error)]
pub enum TtsError {
    #[error("尚未配置 TTS 服务")]
    NotConfigured,
    #[error("TTS 请求无效：{0}")]
    InvalidRequest(String),
    #[error("TTS 合成失败：{0}")]
    Provider(String),
}
#[async_trait::async_trait]
pub trait TtsProvider: Send + Sync {
    fn provider_name(&self) -> &str;
    async fn synthesize(&self, request: TtsRequest) -> std::result::Result<TtsAudio, TtsError>;
}
pub struct UnconfiguredTtsProvider;
#[async_trait::async_trait]
impl TtsProvider for UnconfiguredTtsProvider {
    fn provider_name(&self) -> &str {
        "unconfigured"
    }
    async fn synthesize(&self, _: TtsRequest) -> std::result::Result<TtsAudio, TtsError> {
        Err(TtsError::NotConfigured)
    }
}
impl PersonaRecord {
    pub fn tts_request(&self, text: impl Into<String>) -> TtsRequest {
        TtsRequest {
            text: text.into(),
            reference_audio: self.reference_audio_filename.as_ref().map(|filename| {
                PersonaAssetRef {
                    persona_id: self.id.clone(),
                    filename: filename.clone(),
                }
            }),
            reference_text: self.reference_text.clone(),
            language: self.language.clone(),
            voice_id: self.voice_id.clone(),
        }
    }
}
