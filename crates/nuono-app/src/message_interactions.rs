//! 每个机器人对显式富消息的偏好；普通文本仍保持普通文本。
use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkdownMode {
    #[default]
    Auto,
    Plain,
    Native,
    Template,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ButtonMode {
    #[default]
    Auto,
    Text,
    Native,
    Template,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackMode {
    #[default]
    Text,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MessagePolicy {
    pub markdown: MarkdownMode,
    pub buttons: ButtonMode,
    pub fallback: FallbackMode,
    pub button_columns: u8,
    pub markdown_template_id: String,
    pub markdown_template_key: String,
    pub button_template_id: String,
    pub onebot_extensions: bool,
}

impl Default for MessagePolicy {
    fn default() -> Self {
        Self {
            markdown: MarkdownMode::Auto,
            buttons: ButtonMode::Auto,
            fallback: FallbackMode::Text,
            button_columns: 2,
            markdown_template_id: String::new(),
            markdown_template_key: "content".into(),
            button_template_id: String::new(),
            onebot_extensions: false,
        }
    }
}

impl MessagePolicy {
    pub fn validate(&self) -> Result<()> {
        if !(1..=5).contains(&self.button_columns) {
            return invalid("每行按钮数必须是 1–5 的整数");
        }
        validate_identifier(&self.markdown_template_id, "Markdown 模板 ID", 128, true)?;
        validate_identifier(
            &self.markdown_template_key,
            "Markdown 正文参数名",
            64,
            false,
        )?;
        validate_identifier(&self.button_template_id, "按钮模板 ID", 128, true)?;
        if self.markdown == MarkdownMode::Template && self.markdown_template_id.is_empty() {
            return invalid("使用 Markdown 模板时必须填写模板 ID");
        }
        if self.buttons == ButtonMode::Template && self.button_template_id.is_empty() {
            return invalid("使用按钮模板时必须填写模板 ID");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageRoutePolicy {
    pub platform: String,
    pub instance_id: String,
    pub bot_self_id: String,
    #[serde(flatten)]
    pub policy: MessagePolicy,
}

impl<'de> Deserialize<'de> for MessageRoutePolicy {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct RouteVisitor;
        impl<'de> serde::de::Visitor<'de> for RouteVisitor {
            type Value = MessageRoutePolicy;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an exact bot route with message policy fields")
            }

            fn visit_map<M>(self, mut map: M) -> std::result::Result<Self::Value, M::Error>
            where
                M: serde::de::MapAccess<'de>,
            {
                let mut fields = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if fields.contains_key(&key) {
                        return Err(serde::de::Error::custom(format!("duplicate field `{key}`")));
                    }
                    fields.insert(key, map.next_value::<serde_json::Value>()?);
                }
                fn route_field<E: serde::de::Error>(
                    fields: &mut serde_json::Map<String, serde_json::Value>,
                    name: &'static str,
                ) -> std::result::Result<String, E> {
                    let value = fields.remove(name).ok_or_else(|| E::missing_field(name))?;
                    String::deserialize(value).map_err(E::custom)
                }

                let platform = route_field::<M::Error>(&mut fields, "platform")?;
                let instance_id = route_field::<M::Error>(&mut fields, "instance_id")?;
                let bot_self_id = route_field::<M::Error>(&mut fields, "bot_self_id")?;
                // `#[serde(flatten)]` 不会自动继承内嵌结构的 `deny_unknown_fields`；
                // 此处手动剥离路由字段后将剩余字段交给 `MessagePolicy` 反序列化，以确保严格校验未知字段、类型与默认值。
                let policy = MessagePolicy::deserialize(serde_json::Value::Object(fields))
                    .map_err(serde::de::Error::custom)?;
                Ok(MessageRoutePolicy {
                    platform,
                    instance_id,
                    bot_self_id,
                    policy,
                })
            }
        }
        deserializer.deserialize_map(RouteVisitor)
    }
}

impl MessageRoutePolicy {
    pub fn validate(&self) -> Result<()> {
        validate_identifier(&self.platform, "平台标识", 256, false)?;
        validate_identifier(&self.instance_id, "实例标识", 256, false)?;
        validate_identifier(&self.bot_self_id, "机器人原生 ID", 256, false)?;
        self.policy.validate()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MessageInteractionSettings {
    pub default: MessagePolicy,
    pub routes: Vec<MessageRoutePolicy>,
}

impl MessageInteractionSettings {
    pub fn resolve(&self, platform: &str, instance_id: &str, bot_self_id: &str) -> MessagePolicy {
        self.routes
            .iter()
            .find(|route| {
                route.platform == platform
                    && route.instance_id == instance_id
                    && route.bot_self_id == bot_self_id
            })
            .map(|route| &route.policy)
            .unwrap_or(&self.default)
            .clone()
    }

    pub fn validate(&self) -> Result<()> {
        self.default.validate()?;
        if self.routes.len() > 128 {
            return invalid("机器人消息覆盖设置不能超过 128 项");
        }
        let mut seen = BTreeSet::new();
        for route in &self.routes {
            route.validate()?;
            if !seen.insert((&route.platform, &route.instance_id, &route.bot_self_id)) {
                return invalid("机器人消息覆盖设置包含重复的精确路由");
            }
        }
        Ok(())
    }
}

fn validate_identifier(value: &str, label: &str, maximum: usize, allow_empty: bool) -> Result<()> {
    if (!allow_empty && value.is_empty())
        || value.len() > maximum
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return invalid(&format!(
            "{label}长度、空白或控制字符无效（最多 {maximum} 字节）"
        ));
    }
    Ok(())
}

fn invalid<T>(message: &str) -> Result<T> {
    Err(NuonoError::ConfigValidation(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn route(platform: &str, instance_id: &str, bot_self_id: &str) -> MessageRoutePolicy {
        MessageRoutePolicy {
            platform: platform.into(),
            instance_id: instance_id.into(),
            bot_self_id: bot_self_id.into(),
            policy: MessagePolicy {
                markdown: MarkdownMode::Plain,
                ..MessagePolicy::default()
            },
        }
    }

    #[test]
    fn defaults_preserve_auto_and_legacy_configuration() {
        let settings: MessageInteractionSettings = serde_json::from_value(json!({})).unwrap();
        assert_eq!(settings, MessageInteractionSettings::default());
        assert_eq!(settings.default.markdown, MarkdownMode::Auto);
        assert_eq!(settings.default.buttons, ButtonMode::Auto);
        assert!(!settings.default.onebot_extensions);
        settings.validate().unwrap();
    }

    #[test]
    fn resolution_preserves_all_three_exact_native_strings() {
        let settings = MessageInteractionSettings {
            routes: vec![route("onebot_v11", "instance/甲", "00042")],
            ..MessageInteractionSettings::default()
        };
        assert_eq!(
            settings
                .resolve("onebot_v11", "instance/甲", "00042")
                .markdown,
            MarkdownMode::Plain
        );
        for (platform, instance, bot) in [
            ("onebot_v11", "instance/甲", "42"),
            ("onebot_v12", "instance/甲", "00042"),
            ("onebot_v11", "instance/乙", "00042"),
        ] {
            assert_eq!(settings.resolve(platform, instance, bot), settings.default);
        }
        let mut copy = settings.resolve("onebot_v11", "instance/甲", "00042");
        copy.markdown_template_id = "owned copy".into();
        assert!(settings.routes[0].policy.markdown_template_id.is_empty());
    }

    #[test]
    fn duplicate_routes_invalid_values_and_excess_overrides_reject() {
        let mut settings = MessageInteractionSettings {
            routes: vec![route("onebot_v11", "a", "1"), route("onebot_v11", "a", "1")],
            ..MessageInteractionSettings::default()
        };
        assert!(settings.validate().is_err());
        settings.routes = (0..129)
            .map(|index| route("onebot_v11", "a", &index.to_string()))
            .collect();
        assert!(settings.validate().is_err());
        settings.routes = vec![route("onebot_v11", "a", "\n")];
        assert!(settings.validate().is_err());
        settings.routes.clear();
        for columns in [0, 6] {
            settings.default.button_columns = columns;
            assert!(settings.validate().is_err());
        }
        for (key, value) in [
            ("markdown", json!("html")),
            ("buttons", json!("off")),
            ("fallback", json!("silent")),
            ("extra", json!(true)),
        ] {
            let mut policy = serde_json::to_value(MessagePolicy::default()).unwrap();
            policy[key] = value;
            assert!(serde_json::from_value::<MessagePolicy>(policy).is_err());
        }
    }

    #[test]
    fn template_modes_require_explicit_ids_and_keep_parameter_name() {
        let mut policy = MessagePolicy {
            markdown: MarkdownMode::Template,
            ..MessagePolicy::default()
        };
        assert!(policy.validate().is_err());
        policy.markdown_template_id = "approved-markdown".into();
        policy.markdown_template_key = "正文".into();
        policy.validate().unwrap();
        policy.buttons = ButtonMode::Template;
        assert!(policy.validate().is_err());
        policy.button_template_id = "approved-buttons".into();
        policy.validate().unwrap();
        policy.markdown_template_key = "".into();
        assert!(policy.validate().is_err());
    }

    #[test]
    fn route_policy_round_trips_flattened_policy_fields() {
        let original = route("qq_official", "config:qq_official", "app-id");
        let value = serde_json::to_value(&original).unwrap();
        assert_eq!(value["markdown"], "plain");
        assert!(value.get("policy").is_none());
        assert_eq!(
            serde_json::from_value::<MessageRoutePolicy>(value.clone()).unwrap(),
            original
        );
        let mut unknown = value;
        unknown["extra"] = json!(true);
        assert!(serde_json::from_value::<MessageRoutePolicy>(unknown).is_err());
    }

    #[test]
    fn route_policy_defaults_stay_shared_and_invalid_flat_fields_reject() {
        let base = json!({
            "platform": "onebot_v11", "instance_id": "实例/甲", "bot_self_id": "00042"
        });
        let parsed: MessageRoutePolicy = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(parsed.policy, MessagePolicy::default());
        assert_eq!(parsed.bot_self_id, "00042");
        for key in ["platform", "instance_id", "bot_self_id"] {
            let mut missing = base.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(serde_json::from_value::<MessageRoutePolicy>(missing).is_err());
        }
        for (key, value) in [
            ("platform", json!(42)),
            ("bot_self_id", json!(null)),
            ("button_columns", json!(2.0)),
            ("button_columns", json!("2")),
            ("onebot_extensions", json!(null)),
            ("markdown", json!("html")),
            ("buttons", json!("off")),
            ("policy", json!({"markdown": "plain"})),
            ("extra", json!({"nested": true})),
        ] {
            let mut invalid = base.clone();
            invalid[key] = value;
            assert!(
                serde_json::from_value::<MessageRoutePolicy>(invalid.clone()).is_err(),
                "{key}"
            );
            assert!(
                serde_json::from_value::<MessageInteractionSettings>(json!({"routes": [invalid]}))
                    .is_err(),
                "nested route: {key}"
            );
        }
    }

    #[test]
    fn route_policy_rejects_duplicate_identity_and_policy_keys() {
        for input in [
            r#"{"platform":"qq","platform":"discord","instance_id":"a","bot_self_id":"1"}"#,
            r#"{"platform":"qq","instance_id":"a","bot_self_id":"1","markdown":"plain","markdown":"native"}"#,
            r#"{"platform":"qq","instance_id":"a","bot_self_id":"1","button_columns":2,"button_columns":3}"#,
        ] {
            let error = serde_json::from_str::<MessageRoutePolicy>(input).unwrap_err();
            assert!(error.to_string().contains("duplicate field"), "{error}");
        }
    }
}
