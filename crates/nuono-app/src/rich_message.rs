//! 经校验的富消息内容结构，供 SDK 发送规划与平台适配器编解码共用。
pub use nuono_domain::{ButtonAction, ButtonOne, ButtonPermission, ButtonStyle};
use nuono_error::{NuonoError, Result};
use serde_json::{Map, Value};

#[derive(Debug, Clone)]
pub struct TemplateMarkdown {
    pub template_id: String,
    pub params: Map<String, Value>,
    pub fallback: String,
}

#[derive(Debug, Clone, Default)]
pub struct RichMessage {
    pub text: String,
    pub markdown: Option<String>,
    pub template: Option<TemplateMarkdown>,
    pub buttons: Vec<Vec<ButtonOne>>,
    pub keyboard: Option<Value>,
    pub attachments: Vec<Value>,
}

fn invalid(message: impl Into<String>) -> NuonoError {
    NuonoError::MessageFormat(message.into())
}

/// 支持传入纯文本、单个消息段或消息段数组；不允许把任意 JSON 对象当作消息文本。
pub fn segments(value: &Value) -> Result<Vec<Value>> {
    match value {
        Value::String(text) => Ok(vec![
            serde_json::json!({"type":"text","data":{"text":text}}),
        ]),
        Value::Array(items) => Ok(items.clone()),
        Value::Object(object) if object.get("type").and_then(Value::as_str).is_some() => {
            Ok(vec![value.clone()])
        }
        _ => Err(invalid("消息必须为文本、消息段或消息段数组")),
    }
}

impl RichMessage {
    pub fn from_segments(value: &Value) -> Result<Option<Self>> {
        let items = segments(value)?;
        let mut result = Self::default();
        let mut body = Vec::new();
        let mut rich = false;
        for item in items {
            let kind = item["type"]
                .as_str()
                .ok_or_else(|| invalid("消息段缺少 type"))?;
            let data = item.get("data").unwrap_or(&item);
            match kind {
                "text" => {
                    let text = data["text"]
                        .as_str()
                        .ok_or_else(|| invalid("文本段缺少 text"))?;
                    result.text.push_str(text);
                    body.push(text.to_owned());
                }
                "markdown" => {
                    let text = data
                        .as_str()
                        .or_else(|| data["content"].as_str())
                        .or_else(|| data["data"].as_str())
                        .ok_or_else(|| invalid("Markdown 缺少 content"))?;
                    result.markdown.get_or_insert_default();
                    body.push(text.into());
                    rich = true;
                }
                "template_markdown" => {
                    if result.template.is_some() {
                        return Err(invalid("一条消息只允许一个 Markdown 模板"));
                    }
                    let template_id = data["template_id"]
                        .as_str()
                        .ok_or_else(|| invalid("Markdown 模板缺少 template_id"))?
                        .to_owned();
                    if template_id.trim().is_empty() || template_id.len() > 128 {
                        return Err(invalid("Markdown 模板 ID 无效"));
                    }
                    let params = data
                        .get("params")
                        .or_else(|| data.get("data"))
                        .or_else(|| data.get("para"))
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    if params.len() > 32
                        || params.iter().any(|(k, v)| {
                            k.is_empty()
                                || k.len() > 128
                                || !(v.is_string()
                                    || v.as_array().is_some_and(|a| a.iter().all(Value::is_string)))
                        })
                    {
                        return Err(invalid("Markdown 模板参数必须为字符串或字符串数组"));
                    }
                    result.template = Some(TemplateMarkdown {
                        template_id,
                        params,
                        fallback: data["fallback"].as_str().unwrap_or_default().into(),
                    });
                    rich = true;
                }
                "buttons" => {
                    let rows = data.get("buttons").unwrap_or(data);
                    let rows: Vec<Vec<ButtonOne>> = serde_json::from_value(rows.clone())
                        .map_err(|e| invalid(format!("按钮格式无效：{e}")))?;
                    result.buttons.extend(rows);
                    rich = true;
                }
                "keyboard" => {
                    if result.keyboard.is_some() {
                        return Err(invalid("一条消息只允许一个按钮模板"));
                    }
                    let id = data["id"]
                        .as_str()
                        .filter(|id| !id.trim().is_empty() && id.len() <= 128)
                        .ok_or_else(|| invalid("原始 keyboard 仅接受已审核模板 id"))?;
                    result.keyboard = Some(serde_json::json!({"id":id}));
                    rich = true;
                }
                _ => result.attachments.push(item),
            }
        }
        if !rich {
            return Ok(None);
        }
        if result.template.is_some() && result.markdown.is_some() {
            return Err(invalid("Markdown 正文与模板不能同时发送"));
        }
        if result.markdown.is_some() {
            result.markdown = Some(body.join(""));
        }
        if result.template.is_some() && !result.text.is_empty() {
            return Err(invalid("模板消息的正文应通过模板参数提供"));
        }
        if result.keyboard.is_some() && !result.buttons.is_empty() {
            return Err(invalid("静态按钮模板与动态按钮不能在同一条消息中发送"));
        }
        if result.buttons.len() > 5
            || result
                .buttons
                .iter()
                .any(|row| row.is_empty() || row.len() > 5)
        {
            return Err(invalid("按钮最多 5 行，每行 1–5 个"));
        }
        for button in result.buttons.iter().flatten() {
            if button.text.trim().is_empty()
                || button.text.chars().count() > 80
                || button.data.len() > 8192
                || button.ttl_secs == 0
                || button.ttl_secs > 3600
                || button.specify_user_ids.len() > 100
                || button.specify_role_ids.len() > 100
            {
                return Err(invalid("按钮文案、数据、有效期或权限列表超出范围"));
            }
            if matches!(button.action, ButtonAction::Jump) {
                let link =
                    url::Url::parse(&button.data).map_err(|_| invalid("跳转按钮需要有效 URL"))?;
                if !matches!(link.scheme(), "https" | "http") {
                    return Err(invalid("跳转按钮仅支持 http/https URL"));
                }
            }
        }
        Ok(Some(result))
    }

    pub fn content(&self) -> String {
        self.template
            .as_ref()
            .map(|t| t.fallback.clone())
            .or_else(|| self.markdown.clone())
            .unwrap_or_else(|| self.text.clone())
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct RichCapabilities {
    pub markdown: bool,
    pub template_markdown: bool,
    pub buttons: bool,
    pub button_template: bool,
    pub callbacks: bool,
    pub requires_button_permission: bool,
    pub extension_only: bool,
    /// 是否支持在同一条原生 Markdown 消息中内嵌图片。
    pub inline_images: bool,
}

pub fn capabilities(platform: &str) -> RichCapabilities {
    let qq = matches!(platform, "qq" | "qq_official");
    let native = qq || matches!(platform, "telegram" | "discord" | "feishu");
    RichCapabilities {
        markdown: native,
        template_markdown: qq,
        buttons: native,
        button_template: qq,
        callbacks: native,
        requires_button_permission: qq,
        extension_only: matches!(platform, "onebot" | "onebot_v11" | "onebot11"),
        inline_images: qq,
    }
}

pub struct MessagePlan {
    pub segments: Vec<Value>,
    pub degradations: Vec<String>,
}

/// 按钮回调数据属于应用内部状态，降级为纯文本时绝不能当作指令或可读文本直接展示出来。
pub fn fallback_options(rows: &[Vec<ButtonOne>]) -> String {
    rows.iter()
        .flatten()
        .map(|button| {
            if button.action == ButtonAction::Callback {
                format!("{}（需要支持交互按钮的平台）", button.text)
            } else if button.data == button.text {
                button.text.clone()
            } else {
                format!("{}：{}", button.text, button.data)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 在真正发起网络请求前处理平台不支持的富消息能力（降级或报错）；网络传输失败绝不会触发降级重发。
pub fn plan(
    platform: &str,
    policy: &crate::message_interactions::MessagePolicy,
    message: &Value,
) -> Result<MessagePlan> {
    use crate::message_interactions::{ButtonMode, FallbackMode, MarkdownMode};
    policy.validate()?;
    let original = segments(message)?;
    let Some(mut rich) = RichMessage::from_segments(message)? else {
        return Ok(MessagePlan {
            segments: original,
            degradations: Vec::new(),
        });
    };
    let cap = capabilities(platform);
    let inline_images = cap.inline_images
        && !rich.attachments.is_empty()
        && rich.template.is_none()
        && matches!(policy.markdown, MarkdownMode::Auto | MarkdownMode::Native)
        && rich.attachments.iter().all(|part| part["type"] == "image");
    if inline_images {
        if rich.attachments.len() > 10 {
            return Err(invalid("一条富消息最多包含 10 张内嵌图片"));
        }
        for image in &rich.attachments {
            crate::media::image_source(image)?;
            if image["data"]
                .get("reuse")
                .is_some_and(|reuse| !matches!(reuse.as_str(), Some("transient" | "persistent")))
            {
                return Err(invalid("图片 reuse 只接受 transient 或 persistent"));
            }
        }
    }
    if !rich.attachments.is_empty() && (cap.markdown || cap.buttons) && !inline_images {
        if policy.fallback == FallbackMode::Error {
            return Err(NuonoError::NotImplemented(
                "当前平台不支持富消息与附件组合；请分开发送".into(),
            ));
        }
        if rich.keyboard.is_some() {
            return Err(NuonoError::NotImplemented(
                "按钮模板与附件请分开发送；固定模板没有可推导的文字选项".into(),
            ));
        }
        if rich
            .template
            .as_ref()
            .is_some_and(|template| template.fallback.is_empty())
        {
            return Err(invalid("Markdown 模板与附件组合需要可读 fallback"));
        }
        let mut text = rich.content();
        if !rich.buttons.is_empty() {
            text.push_str("\n可选操作：\n");
            text.push_str(&fallback_options(&rich.buttons));
        }
        let mut output = Vec::new();
        if !text.is_empty() {
            output.push(serde_json::json!({"type":"text","data":{"text":text}}));
        }
        output.extend(rich.attachments);
        return Ok(MessagePlan {
            segments: output,
            degradations: vec!["当前平台不支持富消息与附件组合，已使用文字并保留附件".into()],
        });
    }
    let extension = cap.extension_only && policy.onebot_extensions;
    let mut output = Vec::new();
    let mut degradations = Vec::new();
    let mut lose = |reason: &str| -> Result<()> {
        if policy.fallback == FallbackMode::Error {
            return Err(NuonoError::NotImplemented(reason.into()));
        }
        degradations.push(reason.into());
        Ok(())
    };
    let md_available = cap.markdown || extension;
    let mut md = rich.markdown.take();
    if inline_images {
        // 保持正文中各段落的原有顺序：如果把所有文本合并到一起，会导致图片与对应的说明文字错位。
        // 按钮的组装仍然走统一的宿主处理流程。
        output.extend(
            original
                .into_iter()
                .filter_map(|mut part| match part["type"].as_str() {
                    Some("text") => {
                        let text = part["data"]["text"].clone();
                        part["type"] = Value::String("markdown".into());
                        part["data"] = serde_json::json!({"content":text});
                        Some(part)
                    }
                    Some("markdown" | "image") => Some(part),
                    _ => None,
                }),
        );
        if !output.iter().any(|part| part["type"] == "markdown") {
            output.insert(
                0,
                serde_json::json!({"type":"markdown","data":{"content":""}}),
            );
        }
        rich.attachments.clear();
    } else if let Some(template) = rich.template.take() {
        if (cap.template_markdown || extension) && policy.markdown != MarkdownMode::Plain {
            output.push(serde_json::json!({"type":"template_markdown","data":{"template_id":template.template_id,"params":template.params,"fallback":template.fallback}}));
        } else {
            if policy.markdown != MarkdownMode::Plain {
                lose("当前平台或设置不支持 Markdown 模板，已使用文字回退")?;
            }
            if template.fallback.is_empty() {
                return Err(invalid("不支持的 Markdown 模板必须提供可读 fallback"));
            }
            rich.text = template.fallback;
        }
    } else if let Some(content) = md.take() {
        match policy.markdown {
            MarkdownMode::Auto|MarkdownMode::Native if md_available => output.push(serde_json::json!({"type":"markdown","data":{"content":content}})),
            MarkdownMode::Template if cap.template_markdown || extension => output.push(serde_json::json!({"type":"template_markdown","data":{"template_id":policy.markdown_template_id,"params":{policy.markdown_template_key.clone():content},"fallback":content}})),
            MarkdownMode::Plain => { rich.text=content; },
            _ => { lose("当前平台不支持所选 Markdown 形式，已使用文字回退")?; rich.text=content; }
        }
    }
    let has_native_body = !output.is_empty();
    if !has_native_body && !rich.text.is_empty() {
        output.push(serde_json::json!({"type":"text","data":{"text":rich.text}}));
    }
    if !rich.buttons.is_empty() {
        // QQ 的自定义按钮键盘需要向平台单独申请权限，因此 Auto 模式下默认采取保守策略（降级为文字）。
        let supported = (cap.buttons || extension)
            && (policy.buttons != ButtonMode::Auto || !cap.requires_button_permission);
        let dynamic_template = policy.buttons == ButtonMode::Template;
        if supported && matches!(policy.buttons, ButtonMode::Auto | ButtonMode::Native) {
            if output.is_empty() {
                output.push(serde_json::json!({"type":"text","data":{"text":"请选择操作。"}}));
            }
            output.push(serde_json::json!({"type":"buttons","data":{"buttons":rich.buttons}}));
        } else {
            if policy.buttons != ButtonMode::Text {
                lose(if dynamic_template {
                    "固定按钮模板不能绑定动态操作，已使用文字选项"
                } else {
                    "当前平台或账号设置未启用原生按钮，已使用文字选项"
                })?;
            }
            let choices = fallback_options(&rich.buttons);
            output.push(serde_json::json!({"type":"text","data":{"text":format!("\n可选操作：\n{choices}")}}));
        }
    }
    if let Some(keyboard) = rich.keyboard {
        if (cap.button_template || extension) && policy.buttons != ButtonMode::Text {
            if output.is_empty() {
                output.push(serde_json::json!({"type":"text","data":{"text":"请选择操作。"}}));
            }
            output.push(serde_json::json!({"type":"keyboard","data":keyboard}));
        } else {
            return Err(NuonoError::NotImplemented(
                "该平台不支持按钮模板；静态模板没有可推导的文字选项".into(),
            ));
        }
    }
    output.extend(rich.attachments);
    if output.is_empty() {
        return Err(invalid("富消息没有可发送内容"));
    }
    Ok(MessagePlan {
        segments: output,
        degradations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn qq_inline_images_preserve_body_order_and_do_not_bypass_button_policy() {
        let message = json!([
            {"type":"text","data":{"text":"前言"}},
            {"type":"image","data":{"file":"base64://image-a","reuse":"persistent"}},
            {"type":"markdown","data":{"content":"**中间**"}},
            {"type":"image","data":{"file":"base64://image-b"}},
            {"type":"buttons","data":{"buttons":[[ButtonOne::callback("确认","private-data")]]}}
        ]);
        let policy = crate::message_interactions::MessagePolicy {
            buttons: crate::message_interactions::ButtonMode::Native,
            fallback: crate::message_interactions::FallbackMode::Error,
            ..Default::default()
        };
        let prepared = plan("qq_official", &policy, &message).unwrap();
        assert!(prepared.degradations.is_empty());
        assert_eq!(
            prepared
                .segments
                .iter()
                .map(|part| part["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["markdown", "image", "markdown", "image", "buttons"]
        );
        assert_eq!(prepared.segments[1], message[1]);
        assert_eq!(prepared.segments[3], message[3]);
        let conservative = plan("qq_official", &Default::default(), &message).unwrap();
        assert!(
            conservative
                .segments
                .iter()
                .all(|part| part["type"] != "buttons")
        );
        assert!(
            !serde_json::to_string(&conservative.segments)
                .unwrap()
                .contains("private-data")
        );
        let plain = plan(
            "qq_official",
            &crate::message_interactions::MessagePolicy {
                markdown: crate::message_interactions::MarkdownMode::Plain,
                ..Default::default()
            },
            &message,
        )
        .unwrap();
        assert!(plain.segments.iter().all(|part| part["type"] != "markdown"));
        assert_eq!(
            plain
                .segments
                .iter()
                .filter(|part| part["type"] == "image")
                .count(),
            2
        );
    }

    #[test]
    fn qq_inline_image_validation_and_unsupported_attachments_stay_in_preflight() {
        let valid = json!([
            {"type":"markdown","data":{"content":"正文"}},
            {"type":"image","data":{"file":"file:///tmp/image.png"}}
        ]);
        let policy = crate::message_interactions::MessagePolicy {
            fallback: crate::message_interactions::FallbackMode::Error,
            ..Default::default()
        };
        for bad in [
            json!({"type":"image","data":{}}),
            json!({"type":"image","data":{"file":"x","reuse":true}}),
            json!({"type":"video","data":{"file":"x"}}),
        ] {
            let mut message = valid.clone();
            message[1] = bad;
            assert!(plan("qq_official", &policy, &message).is_err());
        }
    }
    #[test]
    fn explicit_plain_text_uses_template_fallback_even_when_automatic_fallback_is_disabled() {
        let policy = crate::message_interactions::MessagePolicy {
            markdown: crate::message_interactions::MarkdownMode::Plain,
            fallback: crate::message_interactions::FallbackMode::Error,
            ..Default::default()
        };
        let prepared = plan(
            "telegram",
            &policy,
            &json!({"type":"template_markdown","data":{"template_id":"approved","params":{"content":"body"},"fallback":"可读正文"}}),
        )
        .unwrap();
        assert_eq!(
            prepared.segments,
            vec![json!({"type":"text","data":{"text":"可读正文"}})]
        );
        assert!(prepared.degradations.is_empty());
    }
    #[test]
    fn mixed_media_falls_back_before_network_without_losing_attachments() {
        let message = json!([{"type":"image","data":{"file":"base64://aGVsbG8="}},{"type":"markdown","data":{"content":"正文"}},{"type":"buttons","data":{"buttons":[[ButtonOne::command("帮助","ww帮助")]]}}]);
        let prepared = plan("telegram", &Default::default(), &message).unwrap();
        assert_eq!(prepared.degradations.len(), 1);
        assert!(
            prepared
                .segments
                .iter()
                .all(|segment| segment["type"] != "buttons" && segment["type"] != "markdown")
        );
        assert_eq!(prepared.segments[1], message[0]);
        assert!(
            prepared.segments[0]["data"]["text"]
                .as_str()
                .unwrap()
                .contains("ww帮助")
        );
        assert!(
            plan(
                "telegram",
                &crate::message_interactions::MessagePolicy {
                    fallback: crate::message_interactions::FallbackMode::Error,
                    ..Default::default()
                },
                &message
            )
            .is_err()
        );
    }
    #[test]
    fn platform_preference_is_not_a_grant_and_fallback_never_discloses_callback_data() {
        let message = json!([{"type":"markdown","data":{"content":"**body**"}},{"type":"buttons","data":{"buttons":[[ButtonOne::callback_json("确认","id",json!({"private":"hidden-state"}))]]}}]);
        let prepared = plan("onebot", &Default::default(), &message).unwrap();
        assert_eq!(prepared.degradations.len(), 2);
        assert!(
            !json!(prepared.segments)
                .to_string()
                .contains("hidden-state")
        );
        assert!(
            plan(
                "onebot",
                &crate::message_interactions::MessagePolicy {
                    fallback: crate::message_interactions::FallbackMode::Error,
                    ..Default::default()
                },
                &message
            )
            .is_err()
        );
    }
    #[test]
    fn explicit_markdown_template_and_button_only_messages_are_validated() {
        let policy = crate::message_interactions::MessagePolicy {
            markdown: crate::message_interactions::MarkdownMode::Template,
            markdown_template_id: "approved".into(),
            ..Default::default()
        };
        let result = plan(
            "qq_official",
            &policy,
            &json!({"type":"markdown","data":{"content":"正文"}}),
        )
        .unwrap();
        assert_eq!(result.segments[0]["data"]["params"]["content"], "正文");
        let result = plan(
            "telegram",
            &Default::default(),
            &json!({"type":"buttons","data":{"buttons":[[ButtonOne::command("帮助","帮助")]]}}),
        )
        .unwrap();
        assert_eq!(result.segments[0]["type"], "text");
        assert_eq!(result.segments[1]["type"], "buttons");
    }
    #[test]
    fn rich_segments_preserve_text_order_and_reject_arbitrary_objects() {
        let rich = RichMessage::from_segments(&json!([{"type":"text","data":{"text":"a"}},{"type":"markdown","data":{"content":"**b**"}},{"type":"text","data":{"text":"c"}}])).unwrap().unwrap();
        assert_eq!(rich.content(), "a**b**c");
        assert!(segments(&json!({"password":"never stringify"})).is_err());
    }
    #[test]
    fn template_fallback_is_explicit_and_raw_keyboard_cannot_inject_buttons() {
        let rich = RichMessage::from_segments(
            &json!({"type":"template_markdown","data":{"template_id":"1","params":{"x":"secret"}}}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(rich.content(), "");
        assert!(
            RichMessage::from_segments(&json!({"type":"keyboard","data":{"content":{"rows":[]}}}))
                .is_err()
        );
        assert!(
            RichMessage::from_segments(&json!([
                {"type":"keyboard","data":{"id":"approved"}},
                {"type":"buttons","data":{"buttons":[[ButtonOne::command("帮助","帮助")]]}}
            ]))
            .is_err()
        );
    }
}
