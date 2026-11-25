use crate::api::button::ButtonOne;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;

/// 消息段类型枚举。
///
/// [`MessageSegment::to_json`] 是发往 OneBot 实现的权威编码：手写每个段的确切
/// 线上形状，包括派生 serde 表达不了的细节——`at_all` 实际是 `at` + `qq:"all"`、
/// `dice`/`rps` 的空 `data`、`node` 内嵌段数组的递归编码。派生 serde（snake_case）
/// 只服务于结构化存储与回读，不要直接 `serde_json::to_value` 后上协议。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MessageSegment {
    /// 文本消息
    Text { text: String },
    /// 图片消息
    Image {
        /// 图片数据，可以是 base64、file://、url://
        file: String,
        /// 图片 URL（可选）
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    /// 与原生富消息组合发送时可复用的图片消息段。
    ReusableImage { file: String },
    /// @某人（兼容数字 QQ ID）。原生字符串身份使用 [`Self::AtUser`]。
    At { qq: i64 },
    /// 按平台原生用户 ID 提及，保留 openid、长数字和前导零。
    AtUser { user_id: String },
    /// @全体成员
    AtAll,
    /// 表情
    Face { id: i64 },
    /// 语音
    Record {
        /// 语音数据
        file: String,
        /// 语音 URL（可选）
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// 是否使用魔法
        #[serde(skip_serializing_if = "Option::is_none")]
        magic: Option<bool>,
    },
    /// 视频
    Video {
        /// 视频数据
        file: String,
        /// 视频 URL（可选）
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    /// 回复消息
    Reply {
        /// 回复的消息 ID
        id: i64,
    },
    /// 转发消息（合并转发）
    Forward { id: String },
    /// 单个转发节点
    Node {
        /// 自定义发送人
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// 自定义发送人 UIN
        #[serde(skip_serializing_if = "Option::is_none")]
        uin: Option<i64>,
        /// 消息内容
        content: Vec<MessageSegment>,
    },
    /// 按钮组
    Buttons { buttons: Vec<Vec<ButtonOne>> },
    /// Markdown 消息
    Markdown { data: String },
    /// 模板 Markdown（支持 OneBot v11 扩展）
    TemplateMarkdown {
        template_id: String,
        params: HashMap<String, String>,
    },
    /// 文件
    File {
        /// 文件数据
        file: String,
        /// 文件名
        name: String,
    },
    /// 戳一戳
    Poke { target_id: i64 },
    /// 礼物
    Gift { target_id: i64, gift_id: i64 },
    /// JSON 卡片消息，`data` 为已序列化的 JSON 字符串
    Json { data: String },
    /// XML 卡片消息
    Xml { data: String },
    /// 音乐分享：`qq`/`163`/`xm` 平台歌曲或 `custom` 自定义卡片
    Music {
        music_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// custom 专用：点击跳转
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// custom 专用：音频直链
        #[serde(skip_serializing_if = "Option::is_none")]
        audio: Option<String>,
        /// custom 专用：标题
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// 链接分享
    Share {
        url: String,
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        image: Option<String>,
    },
    /// 推荐好友/群
    Contact {
        /// "qq" 或 "group"
        contact_type: String,
        id: i64,
    },
    /// 位置
    Location {
        lat: f64,
        lon: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    /// 骰子（点数由 QQ 客户端随机）
    Dice,
    /// 猜拳（石头剪刀布）
    Rps,
    /// 键盘（QQ 官方机器人扩展），负载原样透传
    Keyboard { data: Value },
}

impl MessageSegment {
    /// 创建与富消息组合发送时可由平台缓存复用的图片段。
    pub fn reusable_image(file: impl Into<String>) -> Self {
        Self::ReusableImage { file: file.into() }
    }
    /// 创建文本消息
    pub fn text(text: impl Into<String>) -> Self {
        MessageSegment::Text { text: text.into() }
    }

    /// 创建图片消息
    pub fn image(file: impl Into<String>) -> Self {
        MessageSegment::Image {
            file: file.into(),
            url: None,
        }
    }

    /// 创建带 URL 的图片消息
    pub fn image_with_url(file: impl Into<String>, url: impl Into<String>) -> Self {
        MessageSegment::Image {
            file: file.into(),
            url: Some(url.into()),
        }
    }

    /// 从文件路径创建图片消息
    pub fn image_from_path(path: impl AsRef<std::path::Path>) -> Self {
        MessageSegment::Image {
            file: format!("file://{}", path.as_ref().display()),
            url: None,
        }
    }

    /// @某人
    pub fn at(qq: i64) -> Self {
        MessageSegment::At { qq }
    }

    /// 提及平台原生用户；ID 应来自当前平台事件，不转换为数字或稳定哈希。
    ///
    /// 编码为兼容 `at` 段，由适配器选择平台格式。QQ 官方群聊当前会降级为用户标签；
    /// Telegram 需要有效的正数用户 ID，无法使用 openid 或 `all`。
    pub fn at_user(user_id: impl Into<String>) -> Self {
        MessageSegment::AtUser {
            user_id: user_id.into(),
        }
    }

    /// @全体成员
    pub fn at_all() -> Self {
        MessageSegment::AtAll
    }

    /// 表情
    pub fn face(id: i64) -> Self {
        MessageSegment::Face { id }
    }

    /// 语音消息
    pub fn record(file: impl Into<String>) -> Self {
        MessageSegment::Record {
            file: file.into(),
            url: None,
            magic: None,
        }
    }

    /// 视频消息
    pub fn video(file: impl Into<String>) -> Self {
        MessageSegment::Video {
            file: file.into(),
            url: None,
        }
    }

    /// 回复消息
    pub fn reply(id: i64) -> Self {
        MessageSegment::Reply { id }
    }

    /// 转发消息
    pub fn forward(id: impl Into<String>) -> Self {
        MessageSegment::Forward { id: id.into() }
    }

    /// 单个转发节点
    pub fn node(content: Vec<MessageSegment>) -> Self {
        MessageSegment::Node {
            name: None,
            uin: None,
            content,
        }
    }

    /// 带信息的转发节点
    pub fn node_with_info(name: impl Into<String>, uin: i64, content: Vec<MessageSegment>) -> Self {
        MessageSegment::Node {
            name: Some(name.into()),
            uin: Some(uin),
            content,
        }
    }

    /// 按钮组
    pub fn buttons(buttons: Vec<Vec<ButtonOne>>) -> Self {
        MessageSegment::Buttons { buttons }
    }

    /// Markdown 消息
    pub fn markdown(data: impl Into<String>) -> Self {
        MessageSegment::Markdown { data: data.into() }
    }

    /// 模板 Markdown
    pub fn template_markdown(
        template_id: impl Into<String>,
        params: HashMap<String, String>,
    ) -> Self {
        MessageSegment::TemplateMarkdown {
            template_id: template_id.into(),
            params,
        }
    }

    /// 文件消息
    pub fn file(file: impl Into<String>, name: impl Into<String>) -> Self {
        MessageSegment::File {
            file: file.into(),
            name: name.into(),
        }
    }

    /// 戳一戳
    pub fn poke(target_id: i64) -> Self {
        MessageSegment::Poke { target_id }
    }

    /// 礼物
    pub fn gift(target_id: i64, gift_id: i64) -> Self {
        MessageSegment::Gift { target_id, gift_id }
    }

    /// JSON 卡片。入参通常是 `json!(...)` 的序列化结果。
    pub fn json_card(data: impl Into<String>) -> Self {
        MessageSegment::Json { data: data.into() }
    }

    /// 便捷构造：直接传 `Value`，内部序列化成字符串。
    pub fn json_from_value(data: &Value) -> Self {
        MessageSegment::Json {
            data: data.to_string(),
        }
    }

    /// XML 卡片
    pub fn xml(data: impl Into<String>) -> Self {
        MessageSegment::Xml { data: data.into() }
    }

    /// 平台音乐分享（`qq`/`163`/`xm`）
    pub fn music(music_type: impl Into<String>, id: impl Into<String>) -> Self {
        MessageSegment::Music {
            music_type: music_type.into(),
            id: Some(id.into()),
            url: None,
            audio: None,
            title: None,
        }
    }

    /// 自定义音乐卡片
    pub fn custom_music(
        url: impl Into<String>,
        audio: impl Into<String>,
        title: impl Into<String>,
    ) -> Self {
        MessageSegment::Music {
            music_type: "custom".into(),
            id: None,
            url: Some(url.into()),
            audio: Some(audio.into()),
            title: Some(title.into()),
        }
    }

    /// 链接分享
    pub fn share(url: impl Into<String>, title: impl Into<String>) -> Self {
        MessageSegment::Share {
            url: url.into(),
            title: title.into(),
            content: None,
            image: None,
        }
    }

    /// 推荐联系人/群
    pub fn contact(contact_type: impl Into<String>, id: i64) -> Self {
        MessageSegment::Contact {
            contact_type: contact_type.into(),
            id,
        }
    }

    /// 位置分享
    pub fn location(lat: f64, lon: f64, title: impl Into<String>) -> Self {
        MessageSegment::Location {
            lat,
            lon,
            title: Some(title.into()),
            content: None,
        }
    }

    /// 骰子
    pub fn dice() -> Self {
        MessageSegment::Dice
    }

    /// 猜拳
    pub fn rps() -> Self {
        MessageSegment::Rps
    }

    /// 键盘（QQ 官方机器人扩展）
    pub fn keyboard(data: Value) -> Self {
        MessageSegment::Keyboard { data }
    }

    /// 获取消息段的原始文本（如果存在）
    pub fn get_text(&self) -> Option<&str> {
        match self {
            MessageSegment::Text { text } => Some(text.as_str()),
            _ => None,
        }
    }

    /// 该段是否属于需要宿主统一降级/交互处理的富消息。
    ///
    /// 媒体段和普通文本都不是富消息，调用方可据此跳过整条 JSON 编码。
    pub fn is_rich(&self) -> bool {
        matches!(
            self,
            MessageSegment::Markdown { .. }
                | MessageSegment::TemplateMarkdown { .. }
                | MessageSegment::Buttons { .. }
                | MessageSegment::Keyboard { .. }
        )
    }

    /// 消费自身转换为 OneBot v11 线上 JSON。
    ///
    /// 与 [`Self::to_json`] 的线上形状完全一致，但媒体字段按所有权移动，避免
    /// Base64/文件内容在编码时被深拷贝；其余段类型回退到借用版。
    pub fn into_json(self) -> Value {
        use serde_json::Map;
        let segment = |kind: &str, data: Map<String, Value>| {
            let mut segment = Map::with_capacity(2);
            segment.insert("type".into(), Value::String(kind.into()));
            segment.insert("data".into(), Value::Object(data));
            Value::Object(segment)
        };
        match self {
            MessageSegment::ReusableImage { file } => {
                let mut data = Map::with_capacity(2);
                data.insert("file".into(), Value::String(file));
                data.insert("reuse".into(), Value::String("persistent".into()));
                segment("image", data)
            }
            MessageSegment::Text { text } => {
                let mut data = Map::with_capacity(1);
                data.insert("text".into(), Value::String(text));
                segment("text", data)
            }
            MessageSegment::Image { file, url } => {
                let mut data = Map::with_capacity(2);
                data.insert("file".into(), Value::String(file));
                if let Some(url) = url {
                    data.insert("url".into(), Value::String(url));
                }
                segment("image", data)
            }
            MessageSegment::Record { file, url, magic } => {
                let mut data = Map::with_capacity(3);
                data.insert("file".into(), Value::String(file));
                if let Some(url) = url {
                    data.insert("url".into(), Value::String(url));
                }
                if let Some(magic) = magic {
                    data.insert("magic".into(), Value::Bool(magic));
                }
                segment("record", data)
            }
            MessageSegment::Video { file, url } => {
                let mut data = Map::with_capacity(2);
                data.insert("file".into(), Value::String(file));
                if let Some(url) = url {
                    data.insert("url".into(), Value::String(url));
                }
                segment("video", data)
            }
            MessageSegment::File { file, name } => {
                let mut data = Map::with_capacity(2);
                data.insert("file".into(), Value::String(file));
                data.insert("name".into(), Value::String(name));
                segment("file", data)
            }
            other => other.to_json(),
        }
    }

    /// 转换为 OneBot v11 线上 JSON。
    ///
    /// 每个段的 `data` 形状以此处为准（对照 NapCat 文档）；`buttons` 与
    /// `template_markdown` 属 QQ 官方扩展，沿用派生形状。
    pub fn to_json(&self) -> Value {
        match self {
            MessageSegment::ReusableImage { file } => {
                json!({"type":"image","data":{"file":file,"reuse":"persistent"}})
            }
            MessageSegment::Text { text } => {
                json!({"type": "text", "data": {"text": text}})
            }
            MessageSegment::Image { file, url } => {
                let mut data = json!({"file": file});
                if let (Some(url), Some(obj)) = (url, data.as_object_mut()) {
                    obj.insert("url".into(), json!(url));
                }
                json!({"type": "image", "data": data})
            }
            // @全体成员在线上就是 qq="all"，没有独立的 at_all 段类型。
            MessageSegment::AtAll => json!({"type": "at", "data": {"qq": "all"}}),
            MessageSegment::At { qq } => json!({"type": "at", "data": {"qq": qq}}),
            MessageSegment::AtUser { user_id } => {
                json!({"type": "at", "data": {"qq": user_id}})
            }
            MessageSegment::Face { id } => json!({"type": "face", "data": {"id": id}}),
            MessageSegment::Record { file, url, magic } => {
                let mut data = json!({"file": file});
                if let Some(obj) = data.as_object_mut() {
                    if let Some(url) = url {
                        obj.insert("url".into(), json!(url));
                    }
                    if let Some(magic) = magic {
                        obj.insert("magic".into(), json!(magic));
                    }
                }
                json!({"type": "record", "data": data})
            }
            MessageSegment::Video { file, url } => {
                let mut data = json!({"file": file});
                if let (Some(url), Some(obj)) = (url, data.as_object_mut()) {
                    obj.insert("url".into(), json!(url));
                }
                json!({"type": "video", "data": data})
            }
            MessageSegment::Reply { id } => json!({"type": "reply", "data": {"id": id}}),
            MessageSegment::Forward { id } => {
                json!({"type": "forward", "data": {"id": id}})
            }
            // node 的 content 内嵌段数组，必须递归走同一条编码路径。
            MessageSegment::Node { name, uin, content } => {
                let mut data =
                    json!({"content": content.iter().map(Self::to_json).collect::<Vec<_>>()});
                if let Some(obj) = data.as_object_mut() {
                    if let Some(name) = name {
                        obj.insert("name".into(), json!(name));
                    }
                    if let Some(uin) = uin {
                        obj.insert("uin".into(), json!(uin));
                    }
                }
                json!({"type": "node", "data": data})
            }
            MessageSegment::Buttons { .. } | MessageSegment::TemplateMarkdown { .. } => {
                serde_json::to_value(self).unwrap_or(Value::Null)
            }
            MessageSegment::Markdown { data } => {
                json!({"type": "markdown", "data": {"content": data}})
            }
            MessageSegment::File { file, name } => {
                json!({"type": "file", "data": {"file": file, "name": name}})
            }
            MessageSegment::Poke { target_id } => {
                json!({"type": "poke", "data": {"qq": target_id}})
            }
            MessageSegment::Gift { target_id, gift_id } => {
                json!({"type": "gift", "data": {"qq": target_id, "id": gift_id}})
            }
            MessageSegment::Json { data } => json!({"type": "json", "data": {"data": data}}),
            MessageSegment::Xml { data } => json!({"type": "xml", "data": {"data": data}}),
            MessageSegment::Music {
                music_type,
                id,
                url,
                audio,
                title,
            } => {
                let mut data = json!({"type": music_type});
                if let Some(obj) = data.as_object_mut() {
                    if let Some(id) = id {
                        obj.insert("id".into(), json!(id));
                    }
                    if let Some(url) = url {
                        obj.insert("url".into(), json!(url));
                    }
                    if let Some(audio) = audio {
                        obj.insert("audio".into(), json!(audio));
                    }
                    if let Some(title) = title {
                        obj.insert("title".into(), json!(title));
                    }
                }
                json!({"type": "music", "data": data})
            }
            MessageSegment::Share {
                url,
                title,
                content,
                image,
            } => {
                let mut data = json!({"url": url, "title": title});
                if let Some(obj) = data.as_object_mut() {
                    if let Some(content) = content {
                        obj.insert("content".into(), json!(content));
                    }
                    if let Some(image) = image {
                        obj.insert("image".into(), json!(image));
                    }
                }
                json!({"type": "share", "data": data})
            }
            MessageSegment::Contact { contact_type, id } => {
                json!({"type": "contact", "data": {"type": contact_type, "id": id}})
            }
            MessageSegment::Location {
                lat,
                lon,
                title,
                content,
            } => {
                let mut data = json!({"lat": lat, "lon": lon});
                if let Some(obj) = data.as_object_mut() {
                    if let Some(title) = title {
                        obj.insert("title".into(), json!(title));
                    }
                    if let Some(content) = content {
                        obj.insert("content".into(), json!(content));
                    }
                }
                json!({"type": "location", "data": data})
            }
            // 骰子/猜拳没有可变字段，但 data 键本身要存在。
            MessageSegment::Dice => json!({"type": "dice", "data": {}}),
            MessageSegment::Rps => json!({"type": "rps", "data": {}}),
            MessageSegment::Keyboard { data } => {
                json!({"type": "keyboard", "data": data})
            }
        }
    }
}

/// 消息构建器
#[derive(Debug, Clone)]
pub struct MessageBuilder {
    segments: Vec<MessageSegment>,
}

impl MessageBuilder {
    /// 创建新的消息构建器
    pub fn new() -> Self {
        Self {
            segments: Vec::new(),
        }
    }

    /// 添加文本
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.segments.push(MessageSegment::text(text));
        self
    }

    /// 添加图片
    pub fn image(mut self, file: impl Into<String>) -> Self {
        self.segments.push(MessageSegment::image(file));
        self
    }

    /// @某人
    pub fn at(mut self, qq: i64) -> Self {
        self.segments.push(MessageSegment::at(qq));
        self
    }

    /// 添加平台原生用户提及。
    pub fn at_user(mut self, user_id: impl Into<String>) -> Self {
        self.segments.push(MessageSegment::at_user(user_id));
        self
    }

    /// @全体成员
    pub fn at_all(mut self) -> Self {
        self.segments.push(MessageSegment::at_all());
        self
    }

    /// 添加表情
    pub fn face(mut self, id: i64) -> Self {
        self.segments.push(MessageSegment::face(id));
        self
    }

    /// 添加回复
    pub fn reply(mut self, id: i64) -> Self {
        self.segments.push(MessageSegment::reply(id));
        self
    }

    /// 添加按钮
    pub fn buttons(mut self, buttons: Vec<Vec<ButtonOne>>) -> Self {
        self.segments.push(MessageSegment::buttons(buttons));
        self
    }

    /// 添加 Markdown
    pub fn markdown(mut self, data: impl Into<String>) -> Self {
        self.segments.push(MessageSegment::markdown(data));
        self
    }

    /// 添加自定义消息段
    pub fn segment(mut self, segment: MessageSegment) -> Self {
        self.segments.push(segment);
        self
    }

    /// 批量添加消息段
    pub fn segments(mut self, segments: Vec<MessageSegment>) -> Self {
        self.segments.extend(segments);
        self
    }

    /// 构建消息
    pub fn build(self) -> Vec<MessageSegment> {
        self.segments
    }

    /// 构建为 OneBot 线上 JSON 数组
    pub fn build_json(self) -> crate::NuonoResult<String> {
        let wire: Vec<Value> = self.segments.iter().map(MessageSegment::to_json).collect();
        serde_json::to_string(&wire).map_err(|e| crate::NuonoError::Serialization(e.to_string()))
    }
}

impl Default for MessageBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl From<String> for MessageSegment {
    fn from(s: String) -> Self {
        MessageSegment::text(s)
    }
}

impl From<&str> for MessageSegment {
    fn from(s: &str) -> Self {
        MessageSegment::text(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_segment_creation() {
        let text = MessageSegment::text("Hello");
        assert_eq!(text.get_text(), Some("Hello"));

        let image = MessageSegment::image("file://test.jpg");
        assert!(matches!(image, MessageSegment::Image { .. }));

        let at = MessageSegment::at(123456);
        assert!(matches!(at, MessageSegment::At { qq: 123456 }));
    }

    #[test]
    fn test_message_builder() {
        let msg = MessageBuilder::new()
            .text("你好")
            .image("test.jpg")
            .at(123456)
            .build();

        assert_eq!(msg.len(), 3);
    }

    #[test]
    fn native_user_mentions_preserve_identity_in_wire_storage_and_nested_nodes() {
        for id in ["ou_native_openid", "18446744073709551615", "00123456"] {
            let segment = MessageSegment::at_user(id);
            let restored: MessageSegment =
                serde_json::from_value(serde_json::to_value(&segment).unwrap()).unwrap();
            let expected = json!({"type":"at","data":{"qq":id}});
            assert_eq!(restored.to_json(), expected);
            let built = MessageBuilder::new().text("hello").at_user(id).build();
            assert_eq!(built[1].to_json(), expected);
            let node = MessageSegment::Node {
                name: None,
                uin: None,
                content: built,
            };
            assert_eq!(node.to_json()["data"]["content"][1], expected);
        }
        assert_eq!(
            MessageSegment::at(123).to_json(),
            json!({"type":"at","data":{"qq":123}})
        );
    }

    #[test]
    fn test_from_string() {
        let seg: MessageSegment = "test".into();
        assert_eq!(seg.get_text(), Some("test"));
    }

    /// 线上形状逐段钉死：字段名错一个（如 cover vs thumb）实现就会静默丢弃。
    #[test]
    fn wire_shapes_match_onebot_v11() {
        assert_eq!(
            MessageSegment::text("hi").to_json(),
            json!({"type": "text", "data": {"text": "hi"}})
        );
        assert_eq!(
            MessageSegment::at_all().to_json(),
            json!({"type": "at", "data": {"qq": "all"}})
        );
        assert_eq!(
            MessageSegment::reply(-2147483000).to_json(),
            json!({"type": "reply", "data": {"id": -2147483000}})
        );
        assert_eq!(
            MessageSegment::markdown("**粗体**").to_json(),
            json!({"type": "markdown", "data": {"content": "**粗体**"}})
        );
        assert_eq!(
            MessageSegment::dice().to_json(),
            json!({"type": "dice", "data": {}})
        );
        assert_eq!(
            MessageSegment::rps().to_json(),
            json!({"type": "rps", "data": {}})
        );
        assert_eq!(
            MessageSegment::poke(42).to_json(),
            json!({"type": "poke", "data": {"qq": 42}})
        );
        assert_eq!(
            MessageSegment::contact("group", 777).to_json(),
            json!({"type": "contact", "data": {"type": "group", "id": 777}})
        );
        assert_eq!(
            MessageSegment::location(39.9, 116.4, "北京").to_json(),
            json!({"type": "location", "data": {"lat": 39.9, "lon": 116.4, "title": "北京"}})
        );
    }

    #[test]
    fn node_encodes_nested_segments_recursively() {
        let node = MessageSegment::node_with_info(
            "某人",
            10086,
            vec![MessageSegment::text("内部文本"), MessageSegment::at_all()],
        )
        .to_json();
        assert_eq!(node["type"], "node");
        assert_eq!(node["data"]["name"], "某人");
        assert_eq!(node["data"]["uin"], 10086);
        assert_eq!(node["data"]["content"][0]["data"]["text"], "内部文本");
        // 嵌套的 @全体成员同样要编码成 at + qq:"all"
        assert_eq!(node["data"]["content"][1]["data"]["qq"], "all");
    }

    #[test]
    fn music_segment_supports_platform_ids_and_custom_cards() {
        let platform = MessageSegment::music("163", "186016").to_json();
        assert_eq!(platform["type"], "music");
        assert_eq!(platform["data"]["type"], "163");
        assert_eq!(platform["data"]["id"], "186016");

        let custom = MessageSegment::custom_music(
            "https://example.com/song",
            "https://example.com/song.mp3",
            "歌名",
        )
        .to_json();
        assert_eq!(custom["data"]["type"], "custom");
        assert_eq!(custom["data"]["audio"], "https://example.com/song.mp3");
        assert!(custom["data"].get("id").is_none());
    }

    #[test]
    fn json_card_serializes_payload_as_string() {
        let card = MessageSegment::json_from_value(&json!({"app": "com.tencent.music"})).to_json();
        assert_eq!(card["type"], "json");
        // v11 的 json 段 data.data 是转义后的 JSON 字符串，不是嵌套对象
        assert!(card["data"]["data"].is_string());
        assert!(
            card["data"]["data"]
                .as_str()
                .unwrap()
                .contains("com.tencent.music")
        );
    }

    #[test]
    fn derived_serde_uses_snake_case_type_names() {
        // 存储路径的形状钉死，避免再退回 "Text"/"AtAll" 这类 Rust 命名
        let stored = serde_json::to_value(MessageSegment::text("x")).unwrap();
        assert_eq!(stored["type"], "text");
        let round: MessageSegment = serde_json::from_value(
            json!({"type": "share", "data": {"url": "https://e", "title": "t"}}),
        )
        .unwrap();
        assert!(matches!(round, MessageSegment::Share { .. }));
    }
}
