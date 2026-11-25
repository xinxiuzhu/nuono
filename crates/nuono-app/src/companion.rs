//! 用户私有伴侣角色卡定义。角色卡的对话经历与长期记忆绑定在其不可变的 UUID 上，修改角色名称不会丢失记忆。
use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};

use crate::persona::{PersonaInput, PersonaRecord};

pub const MAX_COMPANION_CARDS: u64 = 32;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct CompanionCard {
    pub id: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub user_id: i64,
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    pub user_address: String,
    pub avatar_filename: Option<String>,
    pub portrait_filename: Option<String>,
    pub reference_audio_filename: Option<String>,
    pub reference_text: String,
    pub language: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub revision: i64,
    pub updated_at: String,
}

impl CompanionCard {
    /// 组装当前角色卡的系统提示词（在每轮对话开始前捕获快照，后续对角色卡的编辑仅对下一轮生效）。
    pub fn prompt(&self) -> String {
        let mut prompt = format!("你的角色名称是{}。\n{}", self.name, self.system_prompt);
        if !self.user_address.trim().is_empty() {
            prompt.push_str("\n用户希望被称呼为：");
            prompt.push_str(&self.user_address);
        }
        prompt
    }

    /// 转换为通用的角色卡快照结构（不启用全局语音提供方；
    /// 其中的素材文件名归属于用户私有伴侣素材库，与全局角色卡素材库隔离）。
    pub fn persona_snapshot(&self) -> PersonaRecord {
        PersonaRecord {
            id: self.id.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            system_prompt: self.prompt(),
            enabled: true,
            avatar_filename: self.avatar_filename.clone(),
            portrait_filename: self.portrait_filename.clone(),
            reference_audio_filename: self.reference_audio_filename.clone(),
            reference_text: self.reference_text.clone(),
            language: self.language.clone(),
            tts_provider: None,
            voice_id: None,
            revision: self.revision,
            updated_at: self.updated_at.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CompanionInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub user_address: String,
    #[serde(default)]
    pub reference_text: String,
    #[serde(default = "default_language")]
    pub language: String,
}

fn default_language() -> String {
    "zh".into()
}

impl Default for CompanionInput {
    fn default() -> Self {
        Self {
            name: "伴侣".into(),
            description: String::new(),
            system_prompt: "以温和、真诚、自然的方式交流，认真倾听，尊重用户的自主选择。不要编造共同经历或声称记得没有依据的事情。".into(),
            user_address: String::new(),
            reference_text: String::new(),
            language: default_language(),
        }
    }
}

impl CompanionInput {
    pub fn validate(&self) -> Result<()> {
        PersonaInput {
            name: self.name.clone(),
            description: self.description.clone(),
            system_prompt: self.system_prompt.clone(),
            reference_text: self.reference_text.clone(),
            language: self.language.clone(),
            ..Default::default()
        }
        .validate()?;
        if self.user_address.len() > 512 || self.user_address.contains('\0') {
            return Err(NuonoError::Validation("称呼过长或包含无效字符".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CompanionPreset {
    pub id: String,
    #[serde(flatten)]
    pub card: CompanionInput,
}

/// 框架内置的伴侣角色卡预设模板（与管理员配置的群聊全局角色卡完全独立）。
pub fn presets() -> Vec<CompanionPreset> {
    vec![
        CompanionPreset {
            id: "warm-companion".into(),
            card: CompanionInput {
                name: "小暖".into(),
                description: "温和、细心，愿意听你分享日常。".into(),
                ..Default::default()
            },
        },
        CompanionPreset {
            id: "steady-partner".into(),
            card: CompanionInput {
                name: "知行".into(),
                description: "平和、坦诚，与你一起理清想法。".into(),
                system_prompt: "你是沉稳、坦诚且有幽默感的对话伙伴。先理解用户的感受和想法，再给出适量、有依据的回应。尊重独立判断，不编造共同经历。".into(),
                ..Default::default()
            },
        },
    ]
}
