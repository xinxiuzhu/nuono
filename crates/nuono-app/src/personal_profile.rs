//! 用户自填的可选个人资料（与管理员分配的全局角色卡设定相互独立）。
use chrono::{NaiveDate, Utc};
use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PersonalProfile {
    pub display_name: String,
    /// `YYYY-MM-DD` 格式的生日日期；未填写时为空字符串。
    pub birthday: String,
    /// 用户自定义填写的性别描述；未填写时为空字符串。
    pub gender: String,
    pub bio: String,
    pub use_for_ai: bool,
}

impl PersonalProfile {
    pub fn validated(mut self) -> Result<Self> {
        for (value, max, label) in [
            (&mut self.display_name, 40, "昵称"),
            (&mut self.gender, 40, "性别"),
            (&mut self.bio, 500, "个人简介"),
        ] {
            *value = value.trim().to_owned();
            if value.chars().count() > max || value.chars().any(|c| c.is_control() && c != '\n') {
                return Err(NuonoError::InvalidInput(format!(
                    "{label}过长或包含无效字符"
                )));
            }
        }
        self.birthday = self.birthday.trim().to_owned();
        if !self.birthday.is_empty() {
            let date = NaiveDate::parse_from_str(&self.birthday, "%Y-%m-%d")
                .map_err(|_| NuonoError::InvalidInput("请填写有效生日".into()))?;
            if date.format("%Y-%m-%d").to_string() != self.birthday
                || date > Utc::now().date_naive()
            {
                return Err(NuonoError::InvalidInput(
                    "生日不能晚于今天，请使用 YYYY-MM-DD 格式".into(),
                ));
            }
        }
        Ok(self)
    }

    /// 生成供模型参考的个人资料 JSON 字符串（仅作对话背景，不替代角色卡或系统指令）。
    pub fn ai_context(&self) -> Option<String> {
        if !self.use_for_ai
            || [
                self.display_name.as_str(),
                &self.birthday,
                &self.gender,
                &self.bio,
            ]
            .iter()
            .all(|v| v.is_empty())
        {
            return None;
        }
        Some(
            serde_json::json!({"personal_profile": {
                "display_name": self.display_name, "birthday": self.birthday,
                "gender": self.gender, "bio": self.bio,
            }})
            .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn personal_profile_validates_optional_facts_and_opt_in() {
        let profile = PersonalProfile {
            display_name: "  星星  ".into(),
            birthday: "2000-02-29".into(),
            ..Default::default()
        }
        .validated()
        .unwrap();
        assert_eq!(profile.display_name, "星星");
        assert!(profile.ai_context().is_none());
        assert!(
            PersonalProfile {
                use_for_ai: true,
                ..profile.clone()
            }
            .ai_context()
            .unwrap()
            .contains("2000-02-29")
        );
        for birthday in ["2001-02-29", "2999-01-01", "2000-2-1"] {
            assert!(
                PersonalProfile {
                    birthday: birthday.into(),
                    ..profile.clone()
                }
                .validated()
                .is_err()
            );
        }
        assert!(
            PersonalProfile {
                bio: "字".repeat(501),
                ..profile
            }
            .validated()
            .is_err()
        );
        assert!(
            serde_json::from_value::<PersonalProfile>(serde_json::json!({"user_id": 2})).is_err()
        );
    }
}
