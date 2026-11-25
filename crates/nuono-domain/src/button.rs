use serde::{Deserialize, Serialize};

fn default_ttl() -> u32 {
    300
}

/// 按钮样式
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[repr(u8)]
#[non_exhaustive]
pub enum ButtonStyle {
    /// 灰色线框
    #[default]
    Gray = 0,
    /// 蓝色线框
    Blue = 1,
}

/// 按钮动作类型
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[repr(i8)]
#[non_exhaustive]
pub enum ButtonAction {
    /// 自适应（根据客户端能力选择合适的方式）
    #[default]
    Auto = -1,
    /// 跳转按钮
    Jump = 0,
    /// 回调按钮
    Callback = 1,
    /// 命令按钮
    Command = 2,
    /// 直接回复已注册的等待交互会话。
    Reply = 3,
}

/// 按钮权限控制
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[repr(u8)]
#[non_exhaustive]
pub enum ButtonPermission {
    /// 指定用户
    SpecifiedUser = 0,
    /// 管理者
    Manager = 1,
    /// 所有人可按
    #[default]
    All = 2,
    /// 指定身份组（仅频道）
    SpecifiedRole = 3,
}

/// 跨平台按钮定义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ButtonOne {
    /// 按钮显示文本
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default = "default_ttl")]
    pub ttl_secs: u32,
    /// 按钮数据（命令、回调数据、URL等）
    pub data: String,
    /// 按下之后显示的文本（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pressed_text: Option<String>,
    /// 按钮样式
    #[serde(default)]
    pub style: ButtonStyle,
    /// 按钮动作类型
    #[serde(default)]
    pub action: ButtonAction,
    /// 按钮权限
    #[serde(default)]
    pub permission: ButtonPermission,
    /// 指定身份组 ID 列表（仅频道可用）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub specify_role_ids: Vec<String>,
    /// 指定用户 ID 列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub specify_user_ids: Vec<String>,
    /// 客户端不支持时的提示文本
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unsupport_tips: String,
}

impl ButtonOne {
    /// 创建一个新按钮
    pub fn new(text: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            id: String::new(),
            token: None,
            ttl_secs: default_ttl(),
            data: data.into(),
            pressed_text: None,
            style: ButtonStyle::default(),
            action: ButtonAction::default(),
            permission: ButtonPermission::default(),
            specify_role_ids: Vec::new(),
            specify_user_ids: Vec::new(),
            unsupport_tips: String::new(),
        }
    }

    /// 设置按钮样式
    pub fn with_style(mut self, style: ButtonStyle) -> Self {
        self.style = style;
        self
    }

    /// 设置按钮动作类型
    pub fn with_action(mut self, action: ButtonAction) -> Self {
        self.action = action;
        self
    }

    /// 设置按钮权限
    pub fn with_permission(mut self, permission: ButtonPermission) -> Self {
        self.permission = permission;
        self
    }

    /// 设置按下后的显示文本
    pub fn with_pressed_text(mut self, text: impl Into<String>) -> Self {
        self.pressed_text = Some(text.into());
        self
    }

    /// 设置指定用户 ID 列表
    pub fn with_users(mut self, users: Vec<i64>) -> Self {
        self.specify_user_ids = users.into_iter().map(|id| id.to_string()).collect();
        self
    }

    /// 接收字符串形式的用户 ID 列表（保留平台原生 OpenID 或 Snowflake 大整数的精确字符串表示）。
    pub fn with_user_ids(mut self, users: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.specify_user_ids = users.into_iter().map(Into::into).collect();
        self.permission = ButtonPermission::SpecifiedUser;
        self
    }

    /// 设置指定身份组 ID 列表（仅频道）
    pub fn with_roles(mut self, roles: Vec<i64>) -> Self {
        self.specify_role_ids = roles.into_iter().map(|id| id.to_string()).collect();
        self
    }

    /// 设置客户端不支持时的提示文本
    pub fn with_unsupport_tips(mut self, tips: impl Into<String>) -> Self {
        self.unsupport_tips = tips.into();
        self
    }

    /// 创建快速命令按钮
    pub fn command(text: impl Into<String>, command: impl Into<String>) -> Self {
        Self::new(text, command).with_action(ButtonAction::Command)
    }

    pub fn reply(text: impl Into<String>, value: impl Into<String>) -> Self {
        Self::new(text, value).with_action(ButtonAction::Reply)
    }

    pub fn with_ttl(mut self, seconds: u32) -> Self {
        self.ttl_secs = seconds;
        self
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    pub fn callback_json(
        text: impl Into<String>,
        id: impl Into<String>,
        data: serde_json::Value,
    ) -> Self {
        Self::callback(text, data.to_string()).with_id(id)
    }

    /// 创建回调按钮
    pub fn callback(text: impl Into<String>, callback_data: impl Into<String>) -> Self {
        Self::new(text, callback_data).with_action(ButtonAction::Callback)
    }

    /// 创建跳转按钮
    pub fn jump(text: impl Into<String>, url: impl Into<String>) -> Self {
        Self::new(text, url).with_action(ButtonAction::Jump)
    }
}

/// 按钮构建器
#[derive(Debug, Clone)]
pub struct ButtonBuilder {
    text: String,
    data: String,
    pressed_text: Option<String>,
    style: ButtonStyle,
    action: ButtonAction,
    permission: ButtonPermission,
    specify_role_ids: Vec<String>,
    specify_user_ids: Vec<String>,
    unsupport_tips: String,
}

impl ButtonBuilder {
    pub fn new(text: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            data: data.into(),
            pressed_text: None,
            style: ButtonStyle::default(),
            action: ButtonAction::default(),
            permission: ButtonPermission::default(),
            specify_role_ids: Vec::new(),
            specify_user_ids: Vec::new(),
            unsupport_tips: String::new(),
        }
    }

    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    pub fn data(mut self, data: impl Into<String>) -> Self {
        self.data = data.into();
        self
    }

    pub fn style(mut self, style: ButtonStyle) -> Self {
        self.style = style;
        self
    }

    pub fn action(mut self, action: ButtonAction) -> Self {
        self.action = action;
        self
    }

    pub fn build(self) -> ButtonOne {
        ButtonOne {
            text: self.text,
            id: String::new(),
            token: None,
            ttl_secs: default_ttl(),
            data: self.data,
            pressed_text: self.pressed_text,
            style: self.style,
            action: self.action,
            permission: self.permission,
            specify_role_ids: self.specify_role_ids,
            specify_user_ids: self.specify_user_ids,
            unsupport_tips: self.unsupport_tips,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_button_creation() {
        let btn = ButtonOne::new("测试", "test_command");
        assert_eq!(btn.text, "测试");
        assert_eq!(btn.data, "test_command");
        assert_eq!(btn.style, ButtonStyle::Gray);
    }

    #[test]
    fn test_button_builder() {
        let btn = ButtonBuilder::new("测试", "test")
            .style(ButtonStyle::Blue)
            .action(ButtonAction::Command)
            .build();

        assert_eq!(btn.text, "测试");
        assert_eq!(btn.style, ButtonStyle::Blue);
        assert_eq!(btn.action, ButtonAction::Command);
    }

    #[test]
    fn test_command_button() {
        let btn = ButtonOne::command("天气", "天气");
        assert_eq!(btn.action, ButtonAction::Command);
    }
}
