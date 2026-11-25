//! 统一的消息内容分片与发送目标定义。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 业务核心层使用的标准会话目标类型。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum UserType {
    #[default]
    Group,
    Direct,
    Channel,
    SubChannel,
}

impl UserType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::Direct => "direct",
            Self::Channel => "channel",
            Self::SubChannel => "sub_channel",
        }
    }

    pub fn is_direct(self) -> bool {
        matches!(self, Self::Direct)
    }
}

/// 标准出站消息投递目标。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MessageTarget {
    Group {
        group_id: String,
    },
    Direct {
        user_id: String,
    },
    Channel {
        channel_id: String,
    },
    SubChannel {
        channel_id: String,
        sub_channel_id: String,
    },
}

/// 多媒体消息段的文件数据来源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
#[non_exhaustive]
pub enum FileSource {
    Url(String),
    Base64(String),
    Path(String),
    Id(String),
    Raw(String),
}

impl FileSource {
    pub fn from_wire(value: impl Into<String>) -> Self {
        let value = value.into();
        if value.starts_with("http://") || value.starts_with("https://") {
            Self::Url(value)
        } else if value.starts_with("base64://") {
            Self::Base64(value)
        } else if value.starts_with("file://") {
            Self::Path(value)
        } else {
            Self::Raw(value)
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Url(v) | Self::Base64(v) | Self::Path(v) | Self::Id(v) | Self::Raw(v) => v,
        }
    }
}

/// 与具体平台无关的统一消息段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MessageSegment {
    Text {
        text: String,
    },
    Image {
        source: FileSource,
        id: Option<String>,
    },
    At {
        user_id: String,
    },
    Reply {
        message_id: String,
    },
    File {
        name: String,
        source: FileSource,
    },
    Record {
        source: FileSource,
    },
    Video {
        source: FileSource,
    },
    Node {
        messages: Vec<MessageSegment>,
    },
    Markdown {
        content: String,
    },
    Buttons {
        buttons: Vec<Vec<crate::ButtonOne>>,
    },
    TemplateMarkdown {
        template_id: String,
        data: Value,
    },
    Button {
        id: String,
        label: String,
        data: Value,
    },
    Unknown {
        kind: String,
        data: Value,
    },
}

impl MessageSegment {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}
