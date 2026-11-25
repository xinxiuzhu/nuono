use serde::{Deserialize, Serialize};

/// 配置迁移目标作用域（迁移逻辑只接收用户显式修改过的配置项差集，不包含默认值副本）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigurationTarget {
    Global,
    User(i64),
    Group(i64),
}

pub trait ConfigurationMigration: Send + Sync {
    fn upgrade(
        &self,
        from_version: i64,
        to_version: i64,
        target: ConfigurationTarget,
        values: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Map<String, serde_json::Value>, String>;
}

/// 配置字段类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFieldType {
    Text,
    Number,
    Float,
    Boolean,
    Select,
    Textarea,
    Password,
    Switch,
    Token,
    IntegerList,
}

/// Select 字段的选项
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct ConfigFieldOption {
    pub label: String,
    pub value: String,
}

impl From<&str> for ConfigFieldOption {
    fn from(value: &str) -> Self {
        Self::from(value.to_owned())
    }
}

impl From<&&str> for ConfigFieldOption {
    fn from(value: &&str) -> Self {
        Self::from(*value)
    }
}

impl From<String> for ConfigFieldOption {
    fn from(value: String) -> Self {
        Self {
            label: value.clone(),
            value,
        }
    }
}

impl From<&String> for ConfigFieldOption {
    fn from(value: &String) -> Self {
        Self::from(value.clone())
    }
}

impl From<&ConfigFieldOption> for ConfigFieldOption {
    fn from(value: &ConfigFieldOption) -> Self {
        value.clone()
    }
}

impl<L: Into<String>, V: Into<String>> From<(L, V)> for ConfigFieldOption {
    fn from((label, value): (L, V)) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

impl<L: Clone + Into<String>, V: Clone + Into<String>> From<&(L, V)> for ConfigFieldOption {
    fn from((label, value): &(L, V)) -> Self {
        Self::from((label.clone(), value.clone()))
    }
}

/// 字段的声明式表单信息，只来自插件代码，不从用户配置文件推断。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(default)]
pub struct ConfigFieldUi {
    #[serde(skip_serializing_if = "is_zero")]
    pub order: i32,
    #[serde(skip_serializing_if = "is_false")]
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visible_when: Option<ConfigFieldVisibility>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<ConfigFieldOption>,
    /// tags / multiselect 是否允许选项列表以外的字符串，兼容历史值或可扩展名单。
    /// 不放宽字符串、Select 或数字字段的约束。
    #[serde(skip_serializing_if = "is_false")]
    pub allow_custom: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options_source: Option<ConfigFieldOptionsSource>,
    /// 标签 / 多选 / 对象列表 / 键值。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editor: Option<String>,
    /// json_string 表示外层配置值仍是字符串；native 表示实际 JSON 类型。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// object_list 的已知字段；key_value 可使用单个数字字段描述每个值。
    /// 未知对象键仍由插件保留。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(no_recursion)]
    pub fields: Vec<PluginConfigField>,
    /// object_list 内的唯一标识字段。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_label: Option<String>,
    /// 布局宽度提示：`"half"`（半宽两列）或 `"full"`（通栏整行）；留空由前端按控件类型自动决策。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<String>,
    /// 是否归入所在分组的「高级选项」折叠区，减少常用配置视觉噪音。
    #[serde(skip_serializing_if = "is_false")]
    pub advanced: bool,
    /// 可选的简短行内副说明；存在时行内直接展示，长说明保留在帮助提示中。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct ConfigFieldVisibility {
    pub key: String,
    pub value: serde_json::Value,
}

/// 从另一个声明为 JSON 数组的字段读取标签和值。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct ConfigFieldOptionsSource {
    pub key: String,
    pub value_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_key: Option<String>,
}

fn is_zero(value: &i32) -> bool {
    *value == 0
}

/// 插件配置字段声明 — 对应 gsuid_core 的 config_default.py
///
/// 每个插件通过 `config_fields()` 声明自己可配置的字段。
/// 前端根据这些声明自动生成配置表单，后端根据约束校验保存的值。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct PluginConfigField {
    /// 字段键名（TOML key）
    pub key: String,
    /// 显示标签
    pub label: String,
    /// 提示说明
    pub description: String,
    /// 字段类型
    pub field_type: ConfigFieldType,
    /// 默认值
    pub default: serde_json::Value,
    /// 是否必填
    #[serde(default)]
    pub required: bool,
    /// 占位文本
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Select / Text 类型的选项列表
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<ConfigFieldOption>>,
    /// Number 类型的最小值
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    pub min: Option<i64>,
    /// Number 类型的最大值
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    pub max: Option<i64>,
    /// 分组（用于 UI 分段显示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// 客户能否在自己的群里单独调这个字段。
    ///
    /// **默认 false，且必须由插件显式打开**。反过来（默认可调）意味着某个插件哪天加了一个
    /// API key 字段，就自动暴露给了所有客户 —— 安全边界的默认值只能往严的那边倒。
    /// 未标记的字段属于员工，客户改它会被写入路径直接拒掉。
    #[serde(default, skip_serializing_if = "is_false")]
    pub group_scoped: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub user_scoped: bool,
    /// 额外表单描述；老声明无需填写。
    #[serde(default)]
    pub ui: ConfigFieldUi,
}

/// 供 `skip_serializing_if` 使用：`false` 不落盘，存量配置文件因此保持原样。
fn is_false(value: &bool) -> bool {
    !*value
}

impl PluginConfigField {
    pub fn text(key: &str, label: &str, description: &str, default: &str) -> Self {
        Self {
            key: key.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            field_type: ConfigFieldType::Text,
            default: serde_json::Value::String(default.to_string()),
            required: false,
            placeholder: None,
            options: None,
            min: None,
            max: None,
            section: None,
            group_scoped: false,
            user_scoped: false,
            ui: ConfigFieldUi::default(),
        }
    }

    pub fn number(key: &str, label: &str, description: &str, default: i64) -> Self {
        Self {
            key: key.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            field_type: ConfigFieldType::Number,
            default: serde_json::json!(default),
            required: false,
            placeholder: None,
            options: None,
            min: None,
            max: None,
            section: None,
            group_scoped: false,
            user_scoped: false,
            ui: ConfigFieldUi::default(),
        }
    }

    pub fn boolean(key: &str, label: &str, description: &str, default: bool) -> Self {
        Self {
            key: key.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            field_type: ConfigFieldType::Boolean,
            default: serde_json::Value::Bool(default),
            required: false,
            placeholder: None,
            options: None,
            min: None,
            max: None,
            section: None,
            group_scoped: false,
            user_scoped: false,
            ui: ConfigFieldUi::default(),
        }
    }

    pub fn switch(key: &str, label: &str, description: &str, default: bool) -> Self {
        Self {
            key: key.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            field_type: ConfigFieldType::Switch,
            default: serde_json::Value::Bool(default),
            required: false,
            placeholder: None,
            options: None,
            min: None,
            max: None,
            section: None,
            group_scoped: false,
            user_scoped: false,
            ui: ConfigFieldUi::default(),
        }
    }

    pub fn token(key: &str, label: &str, description: &str) -> Self {
        Self {
            key: key.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            field_type: ConfigFieldType::Token,
            default: serde_json::json!([]),
            required: false,
            placeholder: None,
            options: None,
            min: None,
            max: None,
            section: None,
            group_scoped: false,
            user_scoped: false,
            ui: ConfigFieldUi::default(),
        }
    }

    pub fn select(
        key: &str,
        label: &str,
        description: &str,
        default: &str,
        options: Vec<ConfigFieldOption>,
    ) -> Self {
        Self {
            key: key.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            field_type: ConfigFieldType::Select,
            default: serde_json::Value::String(default.to_string()),
            required: false,
            placeholder: None,
            options: Some(options),
            min: None,
            max: None,
            section: None,
            group_scoped: false,
            user_scoped: false,
            ui: ConfigFieldUi::default(),
        }
    }

    pub fn textarea(key: &str, label: &str, description: &str, default: &str) -> Self {
        Self {
            key: key.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            field_type: ConfigFieldType::Textarea,
            default: serde_json::Value::String(default.to_string()),
            required: false,
            placeholder: None,
            options: None,
            min: None,
            max: None,
            section: None,
            group_scoped: false,
            user_scoped: false,
            ui: ConfigFieldUi::default(),
        }
    }

    pub fn password(key: &str, label: &str, description: &str) -> Self {
        Self {
            key: key.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            field_type: ConfigFieldType::Password,
            default: serde_json::Value::String(String::new()),
            required: false,
            placeholder: None,
            options: None,
            min: None,
            max: None,
            section: None,
            group_scoped: false,
            user_scoped: false,
            ui: ConfigFieldUi::default(),
        }
    }

    /// 原生字符串列表，运行时和 TOML 都保留数组类型。
    pub fn string_list(key: &str, label: &str, description: &str, default: Vec<String>) -> Self {
        let mut field = Self::token(key, label, description);
        field.default = serde_json::json!(default);
        field.ui.editor = Some("tags".into());
        field.ui.encoding = Some("native".into());
        field
    }

    /// JSON 字符串编码的字符串列表，不改变插件既有字符串存储。
    pub fn json_string_list(key: &str, label: &str, description: &str, default: &str) -> Self {
        Self::text(key, label, description, default)
            .with_editor("tags")
            .with_encoding("json_string")
    }

    /// JSON 字符串编码的对象列表。子字段可以声明原生嵌套编辑器。
    pub fn json_object_list(
        key: &str,
        label: &str,
        description: &str,
        default: &str,
        fields: Vec<PluginConfigField>,
        item_label: &str,
    ) -> Self {
        let mut field = Self::text(key, label, description, default)
            .with_editor("object_list")
            .with_encoding("json_string");
        field.ui.fields = fields;
        field.ui.item_label = Some(item_label.into());
        field
    }

    /// JSON 字符串编码的字符串键、数字值映射。
    pub fn json_number_map(key: &str, label: &str, description: &str, default: &str) -> Self {
        Self::text(key, label, description, default)
            .with_editor("key_value")
            .with_encoding("json_string")
    }

    pub fn decimal(key: &str, label: &str, description: &str, default: f64) -> Self {
        let mut field = Self::number(key, label, description, 0);
        field.field_type = ConfigFieldType::Float;
        field.default = serde_json::json!(default);
        field
    }

    // ── Builder 方法 ──

    pub fn required(mut self) -> Self {
        self.required = true;
        self.ui.required = true;
        self
    }

    pub fn with_order(mut self, order: i32) -> Self {
        self.ui.order = order;
        self
    }

    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.ui.unit = Some(unit.into());
        self
    }

    pub fn with_step(mut self, step: f64) -> Self {
        self.ui.step = Some(step);
        self
    }

    pub fn with_visible_when(mut self, key: &str, value: impl Into<serde_json::Value>) -> Self {
        self.ui.visible_when = Some(ConfigFieldVisibility {
            key: key.into(),
            value: value.into(),
        });
        self
    }

    pub fn with_custom_options(self) -> Self {
        self.with_allow_custom_options(true)
    }

    pub fn with_allow_custom_options(mut self, allow_custom: bool) -> Self {
        self.ui.allow_custom = allow_custom;
        self
    }

    pub fn with_options_source(
        mut self,
        key: &str,
        value_key: &str,
        label_key: Option<&str>,
    ) -> Self {
        self.ui.options_source = Some(ConfigFieldOptionsSource {
            key: key.into(),
            value_key: value_key.into(),
            label_key: label_key.map(String::from),
        });
        self
    }

    pub fn with_editor(mut self, editor: impl Into<String>) -> Self {
        self.ui.editor = Some(editor.into());
        self
    }

    pub fn with_encoding(mut self, encoding: impl Into<String>) -> Self {
        self.ui.encoding = Some(encoding.into());
        self
    }

    pub fn with_fields(mut self, fields: Vec<PluginConfigField>) -> Self {
        self.ui.fields = fields;
        self
    }

    pub fn with_item_label(mut self, item_label: impl Into<String>) -> Self {
        self.ui.item_label = Some(item_label.into());
        self
    }

    /// 建议前端将此字段以通栏整行展示（适合长 URL、路径或说明性输入）。
    pub fn full_width(mut self) -> Self {
        self.ui.span = Some("full".into());
        self
    }

    /// 建议前端将此字段以两列半宽展示。
    pub fn half_width(mut self) -> Self {
        self.ui.span = Some("half".into());
        self
    }

    /// 标记为低频高级项，前端自动收纳到当前分组的「高级选项」折叠区。
    pub fn advanced(mut self) -> Self {
        self.ui.advanced = true;
        self
    }

    /// 为字段提供简短的行内辅助说明。
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.ui.hint = Some(hint.into());
        self
    }

    /// 兼容原有顶层元数据，生成统一的活表单描述。
    pub fn effective_ui(&self) -> ConfigFieldUi {
        let mut ui = self.ui.clone();
        ui.required |= self.required;
        if ui.placeholder.is_none() {
            ui.placeholder = self.placeholder.clone();
        }
        if ui.options.is_empty() {
            ui.options = self.options.clone().unwrap_or_default();
        }
        ui.fields = ui
            .fields
            .into_iter()
            .map(|mut field| {
                field.ui = field.effective_ui();
                field
            })
            .collect();
        ui
    }

    pub fn with_default(mut self, value: impl Into<serde_json::Value>) -> Self {
        self.default = value.into();
        self
    }

    pub fn with_section(mut self, section: impl Into<String>) -> Self {
        self.section = Some(section.into());
        self
    }

    pub fn with_placeholder(mut self, placeholder: impl Into<String>) -> Self {
        let placeholder = placeholder.into();
        self.ui.placeholder = Some(placeholder.clone());
        self.placeholder = Some(placeholder);
        self
    }

    pub fn with_options(mut self, options: Vec<ConfigFieldOption>) -> Self {
        self.ui.options = options.clone();
        self.options = Some(options);
        self
    }

    pub fn with_min(mut self, min: i64) -> Self {
        self.min = Some(min);
        self
    }

    pub fn with_max(mut self, max: i64) -> Self {
        self.max = Some(max);
        self
    }

    /// 允许客户在单个群里覆盖这个字段。
    ///
    /// 只该用在业务口味上（欢迎语、冷却秒数之类）。凡是密钥、外部账号、成本相关的字段
    /// 一律不要加 —— 一旦标上，具有该群管理权限的账号都能改。
    pub fn group_scoped(mut self) -> Self {
        self.group_scoped = true;
        self
    }

    /// 允许普通账号在个人作用域覆盖该字段（前提是管理员开启了个人级配置覆盖策略）。
    pub fn user_scoped(mut self) -> Self {
        self.user_scoped = true;
        self
    }
}
