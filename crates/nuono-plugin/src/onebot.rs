use serde::{Deserialize, Serialize};
use serde_json::Value;

// 消息段 (MessageSegment) — OneBot v11 标准格式

/// OneBot v11 消息段（接收与发送共用）。
///
/// 这是旧强类型 API 的已知段集合，不承担未知扩展段的无损保存；宽松兼容发生在
/// [`parse_message`]，未知段会降级为文本。需要原始扩展数据的代码应在调用此模块前
/// 保存原始 JSON。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum MessageSegment {
    #[serde(rename = "text")]
    Text { data: TextData },
    #[serde(rename = "at")]
    At { data: AtData },
    #[serde(rename = "image")]
    Image { data: ImageData },
    #[serde(rename = "record")]
    Record { data: RecordData },
    #[serde(rename = "video")]
    Video { data: VideoData },
    #[serde(rename = "face")]
    Face { data: FaceData },
    #[serde(rename = "reply")]
    Reply { data: ReplyData },
    #[serde(rename = "forward")]
    Forward { data: ForwardData },
    #[serde(rename = "node")]
    Node { data: NodeData },
    #[serde(rename = "file")]
    File { data: FileData },
    #[serde(rename = "poke")]
    Poke { data: PokeData },
    #[serde(rename = "xml")]
    Xml { data: XmlData },
    #[serde(rename = "json")]
    Json { data: JsonData },
    #[serde(rename = "music")]
    Music { data: MusicData },
    #[serde(rename = "share")]
    Share { data: ShareData },
    #[serde(rename = "contact")]
    Contact { data: ContactData },
    #[serde(rename = "location")]
    Location { data: LocationData },
    #[serde(rename = "rps")]
    Rps { data: RpsData },
    #[serde(rename = "dice")]
    Dice { data: DiceData },
    #[serde(rename = "shake")]
    Shake { data: ShakeData },
    #[serde(rename = "anonymous")]
    Anonymous { data: AnonymousData },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TextData {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtData {
    pub qq: AtType,
}

/// @ 目标。
///
/// OneBot 在线上通常把 `qq` 编码为字符串；`"all"` 是协议 sentinel，其余内容按
/// 不透明 ID 保留，不在反序列化阶段强制转为整数。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AtType {
    All,
    Id(String),
}

impl Serialize for AtType {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            AtType::All => serializer.serialize_str("all"),
            AtType::Id(s) => serializer.serialize_str(s),
        }
    }
}

impl<'de> Deserialize<'de> for AtType {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if s == "all" {
            Ok(AtType::All)
        } else {
            Ok(AtType::Id(s))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImageData {
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RecordData {
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub magic: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VideoData {
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FaceData {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReplyData {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ForwardData {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uin: Option<String>,
    pub content: Vec<MessageSegment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FileData {
    #[serde(default)]
    pub file: String,
    #[serde(default, alias = "file_name")]
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PokeData {
    #[serde(rename = "type")]
    pub poke_type: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct XmlData {
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct JsonData {
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MusicData {
    #[serde(rename = "type")]
    pub music_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ShareData {
    pub url: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContactData {
    #[serde(rename = "type")]
    pub contact_type: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LocationData {
    pub lat: String,
    pub lon: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RpsData {}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiceData {}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ShakeData {}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AnonymousData {}

/// 发送消息段（OneBot v11 发送与接收格式相同）
pub type SendSegment = MessageSegment;

// 消息类型

/// OneBot v11 消息顶层枚举
#[derive(Serialize, Debug, Clone, Deserialize)]
#[serde(tag = "message_type")]
#[non_exhaustive]
pub enum Message {
    #[serde(rename = "private")]
    PrivateMessage(PrivateMessage),
    #[serde(rename = "group")]
    GroupMessage(GroupMessage),
}

/// 私聊消息
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PrivateMessage {
    pub time: i64,
    pub self_id: i64,
    pub post_type: String,
    pub sub_type: String,
    pub message_id: i64,
    pub user_id: i64,
    pub message: Vec<MessageSegment>,
    pub raw_message: String,
    #[serde(default)]
    pub font: i32,
    #[serde(default)]
    pub sender: PrivateMessageSender,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct PrivateMessageSender {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub user_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub nickname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub age: Option<i32>,
}

/// 群消息
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GroupMessage {
    pub time: i64,
    pub self_id: i64,
    pub post_type: String,
    pub sub_type: String,
    pub message_id: i64,
    pub group_id: i64,
    pub user_id: i64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub anonymous: Option<Anonymous>,
    pub message: Vec<MessageSegment>,
    pub raw_message: String,
    #[serde(default)]
    pub font: i32,
    #[serde(default)]
    pub sender: GroupMessageSender,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct GroupMessageSender {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub user_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub nickname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub card: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub age: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub area: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub title: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Anonymous {
    pub id: i64,
    pub name: String,
    pub flag: String,
}

// 事件类型 (Event) — message / notice / request / meta_event

/// OneBot v11 事件顶层枚举
#[derive(Serialize, Debug, Clone, Deserialize)]
#[serde(tag = "post_type")]
#[non_exhaustive]
pub enum Event {
    #[serde(
        rename = "message",
        serialize_with = "serialize_event_message",
        deserialize_with = "deserialize_event_message"
    )]
    Message(Box<MessageEvent>),
    #[serde(rename = "notice")]
    Notice(NoticeEvent),
    #[serde(rename = "request")]
    Request(RequestEvent),
    #[serde(rename = "meta_event")]
    Meta(MetaEvent),
}

// 外层 `Event` 枚举已经以 `post_type` 作为 tag；而 `Message` 结构体为支持单独反序列化也带有 `post_type` 字段，
// 因此在嵌套序列化/反序列化时需要手动剥离和补回该字段，避免产生重复键。
fn serialize_event_message<S>(message: &MessageEvent, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let Value::Object(mut fields) =
        serde_json::to_value(message).map_err(serde::ser::Error::custom)?
    else {
        return Err(serde::ser::Error::custom("message event must be an object"));
    };
    fields.remove("post_type");
    Value::Object(fields).serialize(serializer)
}

fn deserialize_event_message<'de, D>(deserializer: D) -> Result<Box<MessageEvent>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut fields = serde_json::Map::<String, Value>::deserialize(deserializer)?;
    fields.insert(
        "post_type".to_string(),
        Value::String("message".to_string()),
    );
    serde_json::from_value(Value::Object(fields)).map_err(serde::de::Error::custom)
}

#[derive(Serialize, Debug, Clone, Deserialize)]
#[serde(tag = "notice_type")]
#[non_exhaustive]
pub enum NoticeEvent {
    #[serde(rename = "group_upload")]
    GroupUpload {
        time: i64,
        self_id: i64,
        group_id: i64,
        user_id: i64,
        file: GroupUploadFile,
    },
    #[serde(rename = "group_admin")]
    GroupAdmin {
        time: i64,
        self_id: i64,
        group_id: i64,
        user_id: i64,
        sub_type: String,
    },
    #[serde(rename = "group_decrease")]
    GroupDecrease {
        time: i64,
        self_id: i64,
        group_id: i64,
        user_id: i64,
        operator_id: i64,
        sub_type: String,
    },
    #[serde(rename = "group_increase")]
    GroupIncrease {
        time: i64,
        self_id: i64,
        group_id: i64,
        user_id: i64,
        operator_id: i64,
        sub_type: String,
    },
    #[serde(rename = "group_ban")]
    GroupBan {
        time: i64,
        self_id: i64,
        group_id: i64,
        user_id: i64,
        operator_id: i64,
        duration: i64,
        sub_type: String,
    },
    #[serde(rename = "friend_add")]
    FriendAdd {
        time: i64,
        self_id: i64,
        user_id: i64,
    },
    #[serde(rename = "group_recall")]
    GroupRecall {
        time: i64,
        self_id: i64,
        group_id: i64,
        user_id: i64,
        operator_id: i64,
        message_id: i64,
    },
    #[serde(rename = "friend_recall")]
    FriendRecall {
        time: i64,
        self_id: i64,
        user_id: i64,
        message_id: i64,
    },
    #[serde(rename = "poke")]
    Poke {
        time: i64,
        self_id: i64,
        group_id: Option<i64>,
        user_id: i64,
        target_id: i64,
    },
    #[serde(rename = "lucky_king")]
    LuckyKing {
        time: i64,
        self_id: i64,
        group_id: i64,
        user_id: i64,
        target_id: i64,
    },
    #[serde(rename = "honor")]
    Honor {
        time: i64,
        self_id: i64,
        group_id: i64,
        user_id: i64,
        honor_type: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct GroupUploadFile {
    pub id: String,
    pub name: String,
    pub size: i64,
    pub busid: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request_type")]
#[non_exhaustive]
pub enum RequestEvent {
    #[serde(rename = "friend")]
    Friend {
        time: i64,
        self_id: i64,
        user_id: i64,
        comment: String,
        flag: String,
    },
    #[serde(rename = "group")]
    Group {
        time: i64,
        self_id: i64,
        group_id: i64,
        user_id: i64,
        comment: String,
        flag: String,
        sub_type: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "meta_event_type")]
#[non_exhaustive]
pub enum MetaEvent {
    #[serde(rename = "lifecycle")]
    Lifecycle {
        time: i64,
        self_id: i64,
        sub_type: String,
    },
    #[serde(rename = "heartbeat")]
    Heartbeat {
        time: i64,
        self_id: i64,
        status: HeartbeatStatus,
        interval: i64,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub online: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub good: Option<bool>,
}

/// 兼容旧代码：MessageEvent 就是 Message
pub type MessageEvent = Message;

// API 调用 (Action / Response)

/// OneBot v11 API 请求。
///
/// `echo` 是调用方选择的相关值，协议允许任意 JSON。`None` 表示不要求相关回包；这与
/// `core::onebot_client::ApiRequest` 总是生成字符串 UUID 的运行时客户端是两套构造路径。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub action: String,
    pub params: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub echo: Option<Value>,
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.action, self.params)
    }
}

/// OneBot v11 API 响应
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: String,
    pub retcode: i64,
    pub data: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub echo: Option<Value>,
    pub msg: String,
    pub wording: String,
}

impl Response {
    pub fn ok(data: Value) -> Self {
        Self {
            status: "ok".into(),
            retcode: 0,
            data,
            echo: None,
            msg: "".into(),
            wording: "".into(),
        }
    }
    pub fn error(retcode: i64, msg: impl Into<String>) -> Self {
        let msg = msg.into();
        Self {
            status: "failed".into(),
            retcode,
            data: Value::Null,
            echo: None,
            msg: msg.clone(),
            wording: msg,
        }
    }
}

// 常用 Action 构造器

/// 根据 Message 类型构造发送消息的 Action
pub fn build_send_message(msg: &Message, message: Vec<SendSegment>) -> Action {
    match msg {
        Message::GroupMessage(gm) => action_send_group_msg(gm.group_id, message),
        Message::PrivateMessage(pm) => action_send_private_msg(pm.user_id, message),
    }
}

pub fn action_send_group_msg(group_id: i64, message: Vec<SendSegment>) -> Action {
    Action {
        action: "send_group_msg".into(),
        params: serde_json::json!({ "group_id": group_id, "message": message }),
        echo: None,
    }
}

pub fn action_send_private_msg(user_id: i64, message: Vec<SendSegment>) -> Action {
    Action {
        action: "send_private_msg".into(),
        params: serde_json::json!({ "user_id": user_id, "message": message }),
        echo: None,
    }
}

pub fn action_get_group_info(group_id: i64) -> Action {
    Action {
        action: "get_group_info".into(),
        params: serde_json::json!({ "group_id": group_id, "no_cache": false }),
        echo: None,
    }
}

pub fn action_get_stranger_info(user_id: i64) -> Action {
    Action {
        action: "get_stranger_info".into(),
        params: serde_json::json!({ "user_id": user_id, "no_cache": false }),
        echo: None,
    }
}

pub fn action_delete_msg(message_id: i64) -> Action {
    Action {
        action: "delete_msg".into(),
        params: serde_json::json!({ "message_id": message_id }),
        echo: None,
    }
}

pub fn action_get_login_info() -> Action {
    Action {
        action: "get_login_info".into(),
        params: Value::Null,
        echo: None,
    }
}

pub fn action_get_version_info() -> Action {
    Action {
        action: "get_version_info".into(),
        params: Value::Null,
        echo: None,
    }
}

// 辅助函数

/// 从 JSON 字符串解析 OneBot 事件
pub fn parse_event(msg: &str) -> crate::NuonoResult<Event> {
    serde_json::from_str(msg)
        .map_err(|e| crate::NuonoError::MessageParse(format!("解析事件失败: {e}")))
}

/// 从 JSON 字符串解析消息，兼容 NapCat、LLOneBot 等常见实现差异。
///
/// 该兼容过程是有损的：字符串消息被包装为单个文本段，未知段被替换为可读文本，
/// 字符串 `message_id` 解析失败时使用 `0` sentinel，缺失时间则填本机当前时间。它适合
/// 喂给旧强类型处理器，不适合作为协议归档或签名校验前的规范化。
pub fn parse_message(msg: &str) -> crate::NuonoResult<Message> {
    let mut value: serde_json::Value = serde_json::from_str(msg)
        .map_err(|e| crate::NuonoError::MessageParse(format!("JSON 解析失败: {e}")))?;

    // 规范化：处理各种 OneBot 实现的不规范格式
    if let Some(obj) = value.as_object_mut() {
        // 1. message 字段：支持字符串（CQ码）和数组 → 统一为数组
        // 同时过滤/转换 onebot-api 不支持的未知消息段类型
        if let Some(msg_val) = obj.remove("message") {
            if msg_val.is_string() {
                let text = msg_val.as_str().unwrap_or("").to_string();
                obj.insert(
                    "message".to_string(),
                    serde_json::json!([
                        {"type": "text", "data": {"text": text}}
                    ]),
                );
            } else if let Some(mut arr) = msg_val.as_array().cloned() {
                let known_types: &[&str] = &[
                    "text",
                    "face",
                    "image",
                    "record",
                    "video",
                    "file",
                    "at",
                    "rps",
                    "dice",
                    "shake",
                    "poke",
                    "anonymous",
                    "share",
                    "contact",
                    "location",
                    "music",
                    "reply",
                    "forward",
                    "node",
                    "xml",
                    "json",
                ];
                for seg in arr.iter_mut() {
                    let type_str = seg
                        .get("type")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string());
                    if let Some(type_str) = type_str {
                        let is_unknown = !known_types.contains(&type_str.as_str());
                        if let Some(seg_obj) = seg.as_object_mut() {
                            if is_unknown {
                                // 未知类型 → 转为 text 段，保留原始内容
                                let raw = seg_obj
                                    .get("data")
                                    .and_then(|d| d.get("text").or_else(|| d.get("file")))
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| format!("[{}]", type_str));
                                seg_obj.insert("type".to_string(), serde_json::json!("text"));
                                seg_obj
                                    .insert("data".to_string(), serde_json::json!({"text": raw}));
                            } else if !seg_obj.contains_key("data") {
                                seg_obj.insert("data".to_string(), serde_json::json!({}));
                            }
                        }
                    }
                }
                obj.insert("message".to_string(), serde_json::json!(arr));
            } else {
                obj.insert("message".to_string(), msg_val);
            }
        }

        // 2. message_id 字段：支持字符串和整数
        if let Some(id_val) = obj.get("message_id")
            && id_val.is_string()
        {
            let id_str = id_val.as_str().unwrap_or("0");
            let id_num: i32 = id_str.parse().unwrap_or(0);
            obj.insert("message_id".to_string(), serde_json::json!(id_num));
        }

        // 3. time / font 字段缺失时补默认值
        if !obj.contains_key("font") {
            obj.insert("font".to_string(), serde_json::json!(0));
        }
        if !obj.contains_key("time") {
            obj.insert(
                "time".to_string(),
                serde_json::json!(chrono::Local::now().timestamp()),
            );
        }
        if !obj.contains_key("sender") {
            obj.insert("sender".to_string(), serde_json::json!({}));
        }

        // 4. raw_message 字段缺失时从 message 提取
        if !obj.contains_key("raw_message")
            && let Some(msg_arr) = obj.get("message").and_then(|m| m.as_array())
        {
            let raw: String = msg_arr
                .iter()
                .filter_map(|seg| {
                    seg.get("data")
                        .and_then(|d| d.get("text"))
                        .and_then(|t| t.as_str())
                })
                .collect();
            obj.insert("raw_message".to_string(), serde_json::json!(raw));
        }
    }

    serde_json::from_value(value)
        .map_err(|e| crate::NuonoError::MessageParse(format!("消息反序列化失败: {e}")))
}

/// 从消息段中提取纯文本
pub fn extract_plain_text(message: &[MessageSegment]) -> String {
    message
        .iter()
        .filter_map(|seg| match seg {
            MessageSegment::Text { data } => Some(data.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// 构造纯文本发送段
pub fn text(text: impl Into<String>) -> SendSegment {
    SendSegment::Text {
        data: TextData { text: text.into() },
    }
}

/// 构造 @ 发送段
pub fn at(qq: impl Into<String>) -> SendSegment {
    let qq_str = qq.into();
    let qq_type = if qq_str == "all" {
        AtType::All
    } else {
        AtType::Id(qq_str)
    };
    SendSegment::At {
        data: AtData { qq: qq_type },
    }
}

/// 构造图片发送段
pub fn image(url: impl Into<String>) -> SendSegment {
    SendSegment::Image {
        data: ImageData {
            file: url.into(),
            url: None,
            image_type: None,
            cache: None,
            proxy: None,
            timeout: None,
        },
    }
}

/// 构造回复发送段
pub fn reply(message_id: i64) -> SendSegment {
    SendSegment::Reply {
        data: ReplyData {
            id: message_id.to_string(),
        },
    }
}

/// 获取消息的发送者 ID
pub fn get_user_id(msg: &Message) -> i64 {
    match msg {
        Message::GroupMessage(gm) => gm.user_id,
        Message::PrivateMessage(pm) => pm.user_id,
    }
}

/// 获取消息的群 ID
pub fn get_group_id(msg: &Message) -> Option<i64> {
    match msg {
        Message::GroupMessage(gm) => Some(gm.group_id),
        Message::PrivateMessage(_) => None,
    }
}

/// 判断是否是群消息
pub fn is_group_message(msg: &Message) -> bool {
    matches!(msg, Message::GroupMessage(_))
}

/// 判断是否是私聊消息
pub fn is_private_message(msg: &Message) -> bool {
    matches!(msg, Message::PrivateMessage(_))
}

/// 获取发送者昵称
pub fn get_sender_nickname(msg: &Message) -> String {
    match msg {
        Message::GroupMessage(gm) => gm.sender.card.clone().unwrap_or(
            gm.sender
                .nickname
                .clone()
                .unwrap_or_else(|| gm.user_id.to_string()),
        ),
        Message::PrivateMessage(pm) => pm
            .sender
            .nickname
            .clone()
            .unwrap_or_else(|| pm.user_id.to_string()),
    }
}

/// 根据 OneBot 形状事件构建等待会话键。
///
/// 群会话按 `bot:group:user` 隔离，其他类型均按私聊处理。跨平台兼容事件可使用
/// number 或 string 身份；缺失身份返回 `None`，避免把多个用户合并到同一个 sentinel。
pub fn try_build_session_id(bot_id: i64, event: &Value) -> Option<String> {
    fn identity(value: Option<&Value>) -> Option<String> {
        match value? {
            Value::String(value) if !value.trim().is_empty() => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        }
    }

    let user_id = identity(event.get("user_id"))?;
    let message_type = event
        .get("message_type")
        .and_then(|x| x.as_str())
        .unwrap_or("");

    if message_type == "group" || message_type == "channel" {
        let group_id = identity(event.get("group_id").or_else(|| event.get("channel_id")))?;
        Some(format!("{}:{}:{}", bot_id, group_id, user_id))
    } else {
        Some(format!("{}:private:{}", bot_id, user_id))
    }
}

/// 兼容旧调用方的包装函数：解析失败时回退到 `{bot_id}:invalid` 字符串键。
pub fn build_session_id(bot_id: i64, event: &Value) -> String {
    try_build_session_id(bot_id, event).unwrap_or_else(|| format!("{bot_id}:invalid"))
}

/// 从 WebSocket 消息中提取 self_id
pub fn extract_self_id(msg: &str) -> Option<i64> {
    serde_json::from_str::<Value>(msg)
        .ok()?
        .get("self_id")?
        .as_i64()
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn session_id_preserves_platform_string_identities() {
        let event = serde_json::json!({
            "message_type": "group",
            "user_id": "ou_user",
            "group_id": "oc_chat"
        });
        assert_eq!(
            try_build_session_id(7, &event).as_deref(),
            Some("7:oc_chat:ou_user")
        );
        assert!(try_build_session_id(7, &serde_json::json!({})).is_none());
    }

    #[test]
    fn event_json_shape_is_preserved_for_all_compatibility_variants() {
        let private = serde_json::json!({
            "post_type": "message", "message_type": "private",
            "time": 1, "self_id": 2, "sub_type": "friend", "message_id": 3,
            "user_id": 4, "message": [{"type": "text", "data": {"text": "hello"}}],
            "raw_message": "hello", "font": 0, "sender": {"nickname": "Alice"}
        });
        let group = serde_json::json!({
            "post_type": "message", "message_type": "group",
            "time": 1, "self_id": 2, "sub_type": "normal", "message_id": 3,
            "group_id": 5, "user_id": 4,
            "message": [{"type": "text", "data": {"text": "hello"}}],
            "raw_message": "hello", "font": 0, "sender": {"role": "member"}
        });
        for expected in [private, group] {
            let event = parse_event(&expected.to_string()).unwrap();
            assert!(matches!(&event, Event::Message(_)));
            assert_eq!(serde_json::to_value(&event).unwrap(), expected);
            let encoded = serde_json::to_string(&event).unwrap();
            assert_eq!(encoded.matches("\"post_type\"").count(), 1);
            let decoded = parse_event(&encoded).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
        }
        for expected in [
            serde_json::json!({
                "post_type": "notice", "notice_type": "friend_add",
                "time": 1, "self_id": 2, "user_id": 4
            }),
            serde_json::json!({
                "post_type": "request", "request_type": "group",
                "time": 1, "self_id": 2, "group_id": 5, "user_id": 4,
                "comment": "join", "flag": "token", "sub_type": "add"
            }),
            serde_json::json!({
                "post_type": "meta_event", "meta_event_type": "heartbeat",
                "time": 1, "self_id": 2,
                "status": {"online": true, "good": true}, "interval": 30000
            }),
        ] {
            let event = parse_event(&expected.to_string()).unwrap();
            assert_eq!(serde_json::to_value(event).unwrap(), expected);
        }
    }

    #[test]
    fn compatibility_event_inline_size_is_smaller_than_a_message() {
        let event_bytes = std::mem::size_of::<Event>();
        let message_bytes = std::mem::size_of::<Message>();
        eprintln!("OneBot Event: {event_bytes} bytes; Message: {message_bytes} bytes");
        assert!(event_bytes < message_bytes);
    }

    #[test]
    fn event_message_parsing_keeps_tag_and_required_field_validation() {
        let valid = serde_json::json!({
            "post_type": "message", "message_type": "private",
            "time": 1, "self_id": 2, "sub_type": "friend", "message_id": 3,
            "user_id": 4, "message": [], "raw_message": "", "font": 0, "sender": {}
        });
        for field in [
            "post_type",
            "message_type",
            "user_id",
            "message_id",
            "message",
        ] {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            let error = parse_event(&missing.to_string()).unwrap_err();
            assert!(matches!(error, crate::NuonoError::MessageParse(_)));
            assert!(error.to_string().contains("解析事件失败: "));
        }
        for (field, invalid) in [("post_type", "unknown"), ("message_type", "unknown")] {
            let mut wrong_tag = valid.clone();
            wrong_tag[field] = serde_json::json!(invalid);
            assert!(parse_event(&wrong_tag.to_string()).is_err());
        }
        // 单独反序列化 Message 时仍要求包含完整的原始 DTO 字段（如 post_type）。
        let mut standalone = valid;
        standalone.as_object_mut().unwrap().remove("post_type");
        assert!(serde_json::from_value::<Message>(standalone).is_err());
    }

    #[test]
    fn standard_message_segments_round_trip_without_an_extra_data_wrapper() {
        let fixtures = [
            serde_json::json!({"type": "text", "data": {"text": "hello"}}),
            serde_json::json!({"type": "at", "data": {"qq": "all"}}),
            serde_json::json!({"type": "at", "data": {"qq": "00123"}}),
            serde_json::json!({"type": "image", "data": {
                "file": "image-id", "url": "https://example.com/image.png"
            }}),
            serde_json::json!({"type": "record", "data": {"file": "record-id"}}),
            serde_json::json!({"type": "video", "data": {"file": "video-id"}}),
            serde_json::json!({"type": "face", "data": {"id": "1"}}),
            serde_json::json!({"type": "reply", "data": {"id": "123"}}),
            serde_json::json!({"type": "forward", "data": {"id": "forward-id"}}),
            serde_json::json!({"type": "node", "data": {
                "name": "Alice", "uin": "123", "content": [
                    {"type": "text", "data": {"text": "nested"}}
                ]
            }}),
            serde_json::json!({"type": "file", "data": {
                "file": "file-id", "name": "fixture.txt"
            }}),
            serde_json::json!({"type": "poke", "data": {"type": "1", "id": "123"}}),
            serde_json::json!({"type": "xml", "data": {"data": "<message/>"}}),
            serde_json::json!({"type": "json", "data": {"data": "{\"key\":42}"}}),
            serde_json::json!({"type": "music", "data": {"type": "qq", "id": "1"}}),
            serde_json::json!({"type": "share", "data": {
                "url": "https://example.com", "title": "fixture"
            }}),
            serde_json::json!({"type": "contact", "data": {"type": "qq", "id": "123"}}),
            serde_json::json!({"type": "location", "data": {"lat": "1", "lon": "2"}}),
            serde_json::json!({"type": "rps", "data": {}}),
            serde_json::json!({"type": "dice", "data": {}}),
            serde_json::json!({"type": "shake", "data": {}}),
            serde_json::json!({"type": "anonymous", "data": {}}),
        ];
        for expected in fixtures {
            let segment = serde_json::from_value::<MessageSegment>(expected.clone()).unwrap();
            assert_eq!(serde_json::to_value(&segment).unwrap(), expected);
            let encoded = serde_json::to_string(&segment).unwrap();
            let decoded = serde_json::from_str::<MessageSegment>(&encoded).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
        }
    }

    #[test]
    fn legacy_send_actions_emit_standard_segment_data() {
        let segments = vec![
            text("hello"),
            MessageSegment::At {
                data: AtData {
                    qq: AtType::Id("00123".to_string()),
                },
            },
        ];
        let expected = serde_json::json!([
            {"type": "text", "data": {"text": "hello"}},
            {"type": "at", "data": {"qq": "00123"}}
        ]);
        let group = action_send_group_msg(5, segments.clone());
        assert_eq!(group.action, "send_group_msg");
        assert_eq!(group.params["group_id"], 5);
        assert_eq!(group.params["message"], expected);
        let private = action_send_private_msg(4, segments);
        assert_eq!(private.action, "send_private_msg");
        assert_eq!(private.params["user_id"], 4);
        assert_eq!(private.params["message"], expected);
    }

    fn compatibility_private_input(message: Value) -> Value {
        serde_json::json!({
            "post_type": "message", "message_type": "private",
            "time": 1, "self_id": 2, "sub_type": "friend",
            "message_id": "3", "user_id": 4, "message": message
        })
    }

    #[test]
    fn string_message_compatibility_emits_standard_text_data() {
        let message = parse_message(
            &compatibility_private_input(serde_json::json!("hello [CQ:at,qq=123]")).to_string(),
        )
        .unwrap();
        let Message::PrivateMessage(message) = message else {
            panic!("expected private message");
        };
        assert_eq!(message.message_id, 3);
        assert_eq!(message.raw_message, "hello [CQ:at,qq=123]");
        assert_eq!(
            serde_json::to_value(message.message).unwrap(),
            serde_json::json!([
                {"type": "text", "data": {"text": "hello [CQ:at,qq=123]"}}
            ])
        );
    }

    #[test]
    fn array_message_compatibility_keeps_known_data_and_unknown_fallbacks() {
        let input = compatibility_private_input(serde_json::json!([
            {"type": "text", "data": {"text": "hello"}},
            {"type": "at", "data": {"qq": "00123"}},
            {"type": "image", "data": {"file": "image-id"}},
            {"type": "extension", "data": {"text": "custom"}},
            {"type": "attachment", "data": {"file": "file-id"}},
            {"type": "unsupported", "data": {}},
            {"type": "dice"}
        ]));
        let message = parse_message(&input.to_string()).unwrap();
        let Message::PrivateMessage(message) = message else {
            panic!("expected private message");
        };
        assert_eq!(message.raw_message, "hellocustomfile-id[unsupported]");
        assert_eq!(
            serde_json::to_value(message.message).unwrap(),
            serde_json::json!([
                {"type": "text", "data": {"text": "hello"}},
                {"type": "at", "data": {"qq": "00123"}},
                {"type": "image", "data": {"file": "image-id"}},
                {"type": "text", "data": {"text": "custom"}},
                {"type": "text", "data": {"text": "file-id"}},
                {"type": "text", "data": {"text": "[unsupported]"}},
                {"type": "dice", "data": {}}
            ])
        );
    }

    #[test]
    fn segment_compatibility_keeps_required_payload_validation() {
        for invalid in [
            serde_json::json!({"type": "text", "data": {}}),
            serde_json::json!({"type": "text"}),
            serde_json::json!({"type": "text", "data": {"text": 42}}),
            serde_json::json!({"type": "at", "data": {"qq": true}}),
        ] {
            let input = compatibility_private_input(serde_json::json!([invalid]));
            let error = parse_message(&input.to_string()).unwrap_err();
            assert!(matches!(error, crate::NuonoError::MessageParse(_)));
            assert!(error.to_string().contains("消息反序列化失败: "));
        }
    }
}
