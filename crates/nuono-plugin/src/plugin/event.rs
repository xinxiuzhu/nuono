use super::PluginResponse;
use crate::core::RuntimeBotRoute;
use serde_json::Value;

/// 类型安全的事件包装器
///
/// 封装 `&serde_json::Value`，提供类型安全的访问方法和便捷的回复方法。
///
/// # 示例
///
/// ```rust
/// use nuono_plugin::{async_trait, Event, Plugin, PluginResponse, Result};
/// # struct PingPlugin;
///
/// #[async_trait]
/// impl Plugin for PingPlugin {
/// #     fn name(&self) -> &str { "PingPlugin" }
/// #     fn version(&self) -> &str { "1.0.0" }
/// #     fn description(&self) -> &str { "事件回复示例" }
///     async fn on_event(&self, event: &Event) -> Result<PluginResponse> {
///         match event.command_text() {
///             "ping" => Ok(event.reply_text("pong!")),
///             "help" => Ok(event.reply_text("帮助信息...")),
///             _ => Ok(PluginResponse::Continue),
///         }
///     }
/// }
/// # let raw = nuono_plugin::serde_json::json!({
/// #     "post_type": "message", "message_type": "private",
/// #     "user_id": 42, "raw_message": "demo ping"
/// # });
/// # let event = Event::new(&raw).with_command_context("demo", "ping");
/// # let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
/// # let response = runtime.block_on(PingPlugin.on_event(&event)).unwrap();
/// # let PluginResponse::Reply(reply) = response else { panic!("应回复 pong"); };
/// # assert_eq!(reply["params"]["user_id"], 42);
/// # assert_eq!(reply["params"]["message"][0]["data"]["text"], "pong!");
/// # assert_eq!(event.text(), "demo ping");
/// ```
#[derive(Debug)]
pub struct Event<'a> {
    inner: &'a Value,
    domain: Option<&'a crate::domain::DomainEvent>,
    command_text: Option<&'a str>,
    command_prefix: &'a str,
    locale: crate::LocaleContext,
}

impl<'a> Event<'a> {
    /// 从 `&Value` 创建 Event wrapper
    pub fn new(value: &'a Value) -> Self {
        Self {
            inner: value,
            domain: None,
            command_text: None,
            command_prefix: "",
            locale: crate::LocaleContext::default(),
        }
    }

    pub fn with_domain(value: &'a Value, domain: Option<&'a crate::domain::DomainEvent>) -> Self {
        Self {
            inner: value,
            domain,
            command_text: None,
            command_prefix: "",
            locale: crate::LocaleContext::default(),
        }
    }
    pub fn domain_event(&self) -> Option<&'a crate::domain::DomainEvent> {
        self.domain
    }

    /// 此消息的不可变语言快照。手动创建的事件默认使用中文。
    pub fn locale(&self) -> crate::Locale {
        self.locale.locale
    }

    pub fn locale_context(&self) -> crate::LocaleContext {
        self.locale
    }

    /// 为事件附加宿主已解析的语言上下文快照（不会修改底层原始 JSON 或路由信息）。
    pub fn with_locale_context(mut self, locale: crate::LocaleContext) -> Self {
        self.locale = locale;
        self
    }

    /// 提取当前发言用户的完整原生身份语言偏好作用域（绝不根据昵称/显示名拼凑身份）。
    pub fn user_locale_scope(&self) -> crate::NuonoResult<Option<crate::LocaleScope>> {
        let Some(domain) = self.native_domain_event()? else {
            return Ok(None);
        };
        domain
            .actor
            .as_ref()
            .map(|actor| crate::LocaleScope::user(domain.bot.clone(), actor.clone()))
            .transpose()
    }

    /// 提取当前会话（群聊/频道/私聊会话）的语言偏好作用域；可在保存后台推送目标时一并保存，并在实际发送前重新查询最新语言偏好。
    pub fn conversation_locale_scope(&self) -> crate::NuonoResult<Option<crate::LocaleScope>> {
        let Some(domain) = self.native_domain_event()? else {
            return Ok(None);
        };
        domain
            .conversation
            .as_ref()
            .map(|conversation| {
                crate::LocaleScope::conversation(domain.bot.clone(), conversation.clone())
            })
            .transpose()
    }

    /// 提取供后台异步投递使用的语言偏好作用域：私聊目标使用接收方个人的语言偏好，
    /// 群聊或频道则使用群组/频道级语言偏好（不随当时操作订阅的个人而变）。
    /// 建议与 `RuntimeMessageTarget` 分开独立存储，以确保订阅目标的唯一键保持稳定。
    pub fn delivery_locale_scope(&self) -> anyhow::Result<Option<crate::LocaleScope>> {
        let Some(domain) = self.native_domain_event()? else {
            return Ok(None);
        };
        match domain.conversation.as_ref() {
            Some(crate::domain::ConversationRef::Direct { participant, .. }) => Ok(Some(
                crate::LocaleScope::user(domain.bot.clone(), participant.clone())?,
            )),
            Some(conversation) => Ok(Some(crate::LocaleScope::conversation(
                domain.bot.clone(),
                conversation.clone(),
            )?)),
            None => Ok(None),
        }
    }

    /// 获取底层的完整 runtime JSON Value
    pub fn raw(&self) -> &'a Value {
        self.inner
    }

    /// 获取原平台适配器保留的原始事件负载。
    pub fn raw_event(&self) -> Option<&'a Value> {
        self.inner.get("raw")
    }

    /// 获取平台名。
    pub fn platform(&self) -> &str {
        self.inner
            .get("platform")
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    /// 获取可用于主动调用的精确字符串 runtime route。
    pub fn route(&self) -> Option<RuntimeBotRoute> {
        self.inner
            .get("route")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
    }

    /// 取得当前会话的主动推送目标，保存后可在定时任务中使用。
    /// 缺少路由、会话类型或原生 ID 时返回错误，不默认发送到私聊。
    pub fn message_target(&self) -> anyhow::Result<crate::RuntimeMessageTarget> {
        let route = self
            .route()
            .ok_or_else(|| anyhow::anyhow!("事件缺少机器人路由"))?;
        let required = |key| {
            exact_string(self.inner.get(key)).ok_or_else(|| anyhow::anyhow!("事件缺少 {key}"))
        };
        let target = match self.inner.get("message_type").and_then(Value::as_str) {
            Some("private") => crate::RuntimeMessageTarget::private(route, required("user_id")?),
            Some("group") => crate::RuntimeMessageTarget::group(route, required("group_id")?),
            Some("channel") => crate::RuntimeMessageTarget::channel(
                route,
                required("channel_id")?,
                exact_string(self.inner.get("guild_id")).or_else(|| self.group_id_str()),
            ),
            _ => return Err(anyhow::anyhow!("此事件没有可推送的消息会话")),
        };
        target.validate()?;
        Ok(target)
    }

    /// 获取原生字符串机器人 ID，保留旧 `self_id()` 数字访问器不变。
    pub fn bot_self_id(&self) -> Option<String> {
        self.inner
            .get("route")
            .and_then(|route| exact_string(route.get("bot_self_id")))
            .or_else(|| exact_string(self.inner.get("self_id")))
    }

    /// 获取原生字符串用户 ID，保留旧 `user_id()` 数字访问器不变。
    pub fn user_id_str(&self) -> Option<String> {
        exact_string(self.inner.get("user_id"))
    }

    /// 获取原生字符串群组 ID，保留旧 `group_id()` 数字访问器不变。
    pub fn group_id_str(&self) -> Option<String> {
        exact_string(self.inner.get("group_id"))
    }

    /// 获取原生字符串消息 ID，保留旧 `message_id()` 数字访问器不变。
    pub fn message_id_str(&self) -> Option<String> {
        exact_string(self.inner.get("message_id"))
    }

    /// 获取消息文本内容
    pub fn text(&self) -> &str {
        self.inner
            .get("raw_message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    }

    /// 宿主已移除插件入口前缀的正文。原始文本、事件与附件保持不变。
    /// 手动构造的事件或等待回复未附路由时返回原始文本。
    pub fn command_text(&self) -> &str {
        self.command_text.unwrap_or_else(|| self.text())
    }

    /// 本条消息已识别的插件前缀；普通消息和等待回复为空。
    pub fn command_prefix(&self) -> &str {
        self.command_prefix
    }

    /// 为事件附加消息路由上下文，不改写原始 JSON。
    pub fn with_command_context(mut self, prefix: &'a str, text: &'a str) -> Self {
        self.command_prefix = prefix;
        self.command_text = Some(text);
        self
    }

    /// 附加无前缀正文，适用于构造独立的处理器测试事件。
    pub fn with_command_text(self, text: &'a str) -> Self {
        self.with_command_context("", text)
    }

    /// 获取发送者 user_id
    pub fn user_id(&self) -> i64 {
        self.inner
            .get("user_id")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    }

    /// 获取群号（私聊时返回 None）
    pub fn group_id(&self) -> Option<i64> {
        self.inner.get("group_id").and_then(|v| v.as_i64())
    }

    /// 获取机器人自身 ID
    pub fn self_id(&self) -> i64 {
        self.inner
            .get("self_id")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    }

    /// 获取消息类型 ("group" / "private")
    pub fn message_type(&self) -> &str {
        self.inner
            .get("message_type")
            .and_then(|v| v.as_str())
            .unwrap_or("private")
    }

    /// 获取消息 ID
    pub fn message_id(&self) -> i64 {
        self.inner
            .get("message_id")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    }

    /// 是否为群消息
    pub fn is_group(&self) -> bool {
        self.message_type() == "group"
    }

    /// 是否为私聊消息
    pub fn is_private(&self) -> bool {
        self.message_type() == "private"
    }

    /// 获取事件类型 ("message" / "notice" / "request" / "meta_event")
    pub fn post_type(&self) -> &str {
        self.inner
            .get("post_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    }

    /// notice 事件的种类，如 `poke`、`group_increase`、`essence`、`group_msg_emoji_like`。
    /// 仅 `post_type() == "notice"` 时有值。
    ///
    /// 注意 OneBot v11 把戳一戳包在 `notice_type == "notify"` 里、细分靠 `sub_type`，
    /// 匹配时两个字段要一起看。
    pub fn notice_type(&self) -> Option<&str> {
        self.inner.get("notice_type").and_then(Value::as_str)
    }

    /// request 事件的种类：`friend`（好友申请）或 `group`（加群申请/邀请）。
    /// 仅 `post_type() == "request"` 时有值。
    pub fn request_type(&self) -> Option<&str> {
        self.inner.get("request_type").and_then(Value::as_str)
    }

    /// meta 事件的种类：`lifecycle` 或 `heartbeat`。
    pub fn meta_event_type(&self) -> Option<&str> {
        self.inner.get("meta_event_type").and_then(Value::as_str)
    }

    /// 事件子类型。语义随 `post_type` 变化：message 的 normal/anonymous，
    /// notice 的 poke/essence 等，request 的 add/invite。
    pub fn sub_type(&self) -> &str {
        self.inner
            .get("sub_type")
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    /// 获取发送者角色 ("owner" / "admin" / "member")
    pub fn sender_role(&self) -> &str {
        self.inner
            .get("sender")
            .and_then(|s| s.get("role"))
            .and_then(|v| v.as_str())
            .unwrap_or("member")
    }

    /// 获取发送者昵称
    pub fn sender_nick(&self) -> &str {
        self.inner
            .get("sender")
            .and_then(|s| s.get("nickname"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
    }

    /// 获取完整的 sender 对象
    pub fn sender(&self) -> Option<&Value> {
        self.inner.get("sender")
    }

    /// 获取原始消息段列表
    pub fn message(&self) -> Option<&Value> {
        self.inner.get("message")
    }

    /// 回复富消息：文本、单个消息段或消息段数组（`&str`/`String`/
    /// `MessageSegment`/`Vec<MessageSegment>` 均可 `into`），自动识别群聊/私聊。
    ///
    /// 这是 `reply_text` / `reply_image` 的通用形式，多段消息（图文混排）用它。
    pub fn reply(&self, message: impl Into<crate::api::bot::SendableMessage>) -> PluginResponse {
        let PluginResponse::Reply(mut action) = PluginResponse::text(self.inner, "") else {
            unreachable!("text reply constructs a reply action")
        };
        action["params"]["message"] = message.into().to_json();
        PluginResponse::Reply(action)
    }

    /// 回复纯文本消息（自动识别群聊/私聊）
    pub fn reply_text(&self, msg: impl Into<String>) -> PluginResponse {
        PluginResponse::text(self.inner, msg)
    }

    /// 回复 @某人 + 文本（仅群聊有效，私聊降级为普通文本）
    pub fn reply_at(&self, msg: impl Into<String>) -> PluginResponse {
        PluginResponse::at_text(self.inner, msg)
    }

    /// 回复图片
    pub fn reply_image(&self, file_url: impl Into<String>) -> PluginResponse {
        PluginResponse::image(self.inner, file_url)
    }

    /// 发送 OneBot action（如 set_group_ban 等）
    pub fn action(&self, action: &str, params: Value) -> PluginResponse {
        PluginResponse::action(action, params)
    }

    /// 禁言用户（单位：秒）
    pub fn ban_user(&self, user_id: i64, duration_secs: i32) -> Option<PluginResponse> {
        self.group_id().map(|gid| {
            PluginResponse::action(
                "set_group_ban",
                serde_json::json!({
                    "group_id": gid,
                    "user_id": user_id,
                    "duration": duration_secs
                }),
            )
        })
    }

    /// 解除禁言
    pub fn unban_user(&self, user_id: i64) -> Option<PluginResponse> {
        self.ban_user(user_id, 0)
    }

    /// 踢出用户
    pub fn kick_user(&self, user_id: i64, reject: bool) -> Option<PluginResponse> {
        self.group_id().map(|gid| {
            PluginResponse::action(
                "set_group_kick",
                serde_json::json!({
                    "group_id": gid,
                    "user_id": user_id,
                    "reject_add_request": reject
                }),
            )
        })
    }
}

fn exact_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

impl<'a> std::ops::Deref for Event<'a> {
    type Target = Value;

    fn deref(&self) -> &Self::Target {
        self.inner
    }
}

impl<'a> AsRef<Value> for Event<'a> {
    fn as_ref(&self) -> &Value {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::Event;
    use serde_json::json;
    #[test]
    fn rich_reply_preserves_native_group_private_and_channel_targets() {
        for (raw, key, id) in [
            (
                json!({"message_type":"group","group_id":"group-openid","user_id":"actor"}),
                "group_id",
                "group-openid",
            ),
            (
                json!({"message_type":"private","user_id":"0009007199254740999"}),
                "user_id",
                "0009007199254740999",
            ),
            (
                json!({"message_type":"channel","channel_id":"channel-id","guild_id":"guild-id","user_id":"actor"}),
                "channel_id",
                "channel-id",
            ),
        ] {
            let response = Event::new(&raw).reply(crate::api::MessageSegment::markdown("正文"));
            let super::PluginResponse::Reply(action) = response else {
                panic!("reply action")
            };
            assert_eq!(action["params"][key], id);
            assert!(action["params"]["message"].is_array());
        }
    }

    #[test]
    fn command_context_does_not_rewrite_raw_event_or_segments() {
        let raw = json!({"raw_message":"URL解析 AbC [图片]", "message":[
            {"type":"text", "data":{"text":"URL解析 AbC"}},
            {"type":"image", "data":{"file":"image-id"}}
        ]});
        let event = Event::new(&raw).with_command_context("url", "解析 AbC [图片]");
        assert_eq!(event.command_prefix(), "url");
        assert_eq!(event.command_text(), "解析 AbC [图片]");
        assert_eq!(event.text(), "URL解析 AbC [图片]");
        assert_eq!(event.raw(), &raw);
        assert_eq!(event.message(), Some(&raw["message"]));
        assert_eq!(Event::new(&raw).command_text(), event.text());
    }

    #[test]
    fn exposes_notice_request_and_sub_type_fields() {
        let poke = json!({
            "post_type": "notice",
            "notice_type": "notify",
            "sub_type": "poke",
            "target_id": 123,
            "user_id": 456
        });
        let event = Event::new(&poke);
        assert_eq!(event.post_type(), "notice");
        assert_eq!(event.notice_type(), Some("notify"));
        assert_eq!(event.sub_type(), "poke");
        assert_eq!(event.request_type(), None);

        let request = json!({
            "post_type": "request",
            "request_type": "group",
            "sub_type": "add",
            "flag": "abc"
        });
        let event = Event::new(&request);
        assert_eq!(event.request_type(), Some("group"));
        assert_eq!(event.sub_type(), "add");
        assert_eq!(event.notice_type(), None);

        let heartbeat = json!({"post_type": "meta_event", "meta_event_type": "heartbeat"});
        let event = Event::new(&heartbeat);
        assert_eq!(event.meta_event_type(), Some("heartbeat"));
        assert_eq!(event.sub_type(), "");
    }

    #[test]
    fn bot_self_id_string_accessor_preserves_numeric_runtime_ids() {
        let value = json!({
            "self_id": 123456789,
            "route": { "bot_self_id": 123456789 }
        });
        let event = Event::new(&value);
        assert_eq!(event.bot_self_id().as_deref(), Some("123456789"));
    }

    #[test]
    fn exposes_native_string_ids_route_platform_and_raw_event() {
        let value = json!({
            "platform": "discord",
            "self_id": 0,
            "user_id": "user-snowflake",
            "group_id": "channel-snowflake",
            "message_id": "message-snowflake",
            "raw": {"discord": true},
            "route": {
                "platform": "discord",
                "instance_id": "discord",
                "bot_self_id": "application-snowflake"
            }
        });
        let event = Event::new(&value);

        assert_eq!(event.platform(), "discord");
        assert_eq!(
            event.bot_self_id().as_deref(),
            Some("application-snowflake")
        );
        assert_eq!(event.user_id_str().as_deref(), Some("user-snowflake"));
        assert_eq!(event.group_id_str().as_deref(), Some("channel-snowflake"));
        assert_eq!(event.message_id_str().as_deref(), Some("message-snowflake"));
        assert_eq!(event.raw_event(), Some(&json!({"discord": true})));
        assert_eq!(event.route().unwrap().instance_id, "discord");
        assert_eq!(event.user_id(), 0);
    }
}
