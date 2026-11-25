//! 插件提供的按需 Agent 工作流。正文在模型选择对应读取工具后才进入会话。

/// 与插件命令、工具一起启停的业务工作流。
///
/// 宿主将其注册为只读工具 `plugin.<插件名>.skill.<id>`。`description` 用于选择工作流，
/// `instructions` 是调用后的正文；不会自动写入系统提示或授予工具权限。
/// `tools` 只能引用同一插件 `Plugin::tools()` 返回的本地工具名，宿主提供完整名称和风险等级。
/// ID 最多 64 个 ASCII 字母、数字、下划线或连字符；正文最多 32 KiB。
#[derive(Debug, Clone)]
pub struct PluginAgentSkill {
    pub id: String,
    pub title: String,
    pub description: String,
    pub instructions: String,
    pub tools: Vec<String>,
}

impl PluginAgentSkill {
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        description: impl Into<String>,
        instructions: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            description: description.into(),
            instructions: instructions.into(),
            tools: Vec::new(),
        }
    }

    pub fn with_tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.tools = tools.into_iter().map(Into::into).collect();
        self
    }
}
