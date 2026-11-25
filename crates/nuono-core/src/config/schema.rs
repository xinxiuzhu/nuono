//! Dashboard 与运行时配置文件共享的结构化配置表示。
//!
//! [`ConfigField`] 描述表单元数据和默认值，[`StructuredConfigField`] 则把默认值替换为
//! 持久化的 `data`。二者之间的转换只改变表示，不负责执行业务校验；写入端仍需调用
//! 对应配置管理器的校验逻辑。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub use nuono_plugin::ConfigFieldUi;

/// 单个配置字段的 Schema 定义
///
/// `group_scoped` 标记哪些字段允许客户在单个群里覆盖，默认 `false`（见
/// [`ConfigField::group_scoped`]）。它**只存在于内存中的活 schema**，不写进
/// `config.toml` —— [`StructuredConfigField`] 刻意没有这一项。`ui` 也只存在于活 schema，
/// 表单声明变更不会迁移用户值。原因是授权判据必须跟着插件
/// 当前的声明走：落盘之后，插件把某个字段收回员工专用，磁盘上那份陈旧的 `true` 还会继续
/// 放行客户写入，权限收回就成了假的。附带好处是存量配置文件一个字节都不用动。
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConfigField {
    String {
        title: String,
        desc: String,
        #[serde(default)]
        default: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        options: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        section: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        widget: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        group_scoped: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        user_scoped: bool,
        #[serde(default)]
        ui: ConfigFieldUi,
    },
    Bool {
        title: String,
        desc: String,
        #[serde(default)]
        default: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        section: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        group_scoped: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        user_scoped: bool,
        #[serde(default)]
        ui: ConfigFieldUi,
    },
    Int {
        title: String,
        desc: String,
        #[serde(default)]
        default: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        min: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        section: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        group_scoped: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        user_scoped: bool,
        #[serde(default)]
        ui: ConfigFieldUi,
    },
    Float {
        title: String,
        desc: String,
        #[serde(default)]
        default: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        section: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        group_scoped: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        user_scoped: bool,
        #[serde(default)]
        ui: ConfigFieldUi,
    },
    ListStr {
        title: String,
        desc: String,
        #[serde(default)]
        default: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        section: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        group_scoped: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        user_scoped: bool,
        #[serde(default)]
        ui: ConfigFieldUi,
    },
    Select {
        title: String,
        desc: String,
        #[serde(default)]
        default: String,
        options: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        section: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        group_scoped: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        user_scoped: bool,
        #[serde(default)]
        ui: ConfigFieldUi,
    },
}

/// 供 `skip_serializing_if` 使用：`false` 不落盘，序列化结果与改造前逐字节一致。
fn is_false(value: &bool) -> bool {
    !*value
}

impl ConfigField {
    pub fn title(&self) -> &str {
        match self {
            Self::String { title, .. }
            | Self::Bool { title, .. }
            | Self::Int { title, .. }
            | Self::Float { title, .. }
            | Self::ListStr { title, .. }
            | Self::Select { title, .. } => title,
        }
    }

    pub fn desc(&self) -> &str {
        match self {
            Self::String { desc, .. }
            | Self::Bool { desc, .. }
            | Self::Int { desc, .. }
            | Self::Float { desc, .. }
            | Self::ListStr { desc, .. }
            | Self::Select { desc, .. } => desc,
        }
    }

    pub fn ui(&self) -> &ConfigFieldUi {
        match self {
            Self::String { ui, .. }
            | Self::Bool { ui, .. }
            | Self::Int { ui, .. }
            | Self::Float { ui, .. }
            | Self::ListStr { ui, .. }
            | Self::Select { ui, .. } => ui,
        }
    }

    /// 客户能否在单个群里覆盖这个字段。
    ///
    /// 全部分支都要显式写出，而不是 `_ => false`：新增字段类型时编译器会在这里报错，
    /// 逼人做一次决定；通配分支会让新类型默认「不可按群改」并且没人知道。
    /// 默认拒绝的方向是对的，但那也该是一次显式选择。
    pub fn user_scoped(&self) -> bool {
        match self {
            Self::String { user_scoped, .. }
            | Self::Bool { user_scoped, .. }
            | Self::Int { user_scoped, .. }
            | Self::Float { user_scoped, .. }
            | Self::ListStr { user_scoped, .. }
            | Self::Select { user_scoped, .. } => *user_scoped,
        }
    }

    pub fn group_scoped(&self) -> bool {
        match self {
            Self::String { group_scoped, .. }
            | Self::Bool { group_scoped, .. }
            | Self::Int { group_scoped, .. }
            | Self::Float { group_scoped, .. }
            | Self::ListStr { group_scoped, .. }
            | Self::Select { group_scoped, .. } => *group_scoped,
        }
    }
}

/// 一个配置文件的完整 schema。
///
/// 字段元数据供管理面渲染和基础范围校验使用，不代表配置已满足跨字段或部署安全约束。
pub type ConfigSchema = HashMap<String, ConfigField>;

/// 配置值
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum ConfigValue {
    String(String),
    Bool(bool),
    Int(i64),
    Float(f64),
    ListStr(Vec<String>),
}

/// 结构化配置文件中的单个字段（带 data，对应 config.toml 的文件格式）
///
/// 与 ConfigField 的区别：用 `data` 存当前值，而非 `default`。
/// 保留历史文件格式；完整表单描述来自插件当前声明的活 schema。
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StructuredConfigField {
    String {
        title: String,
        desc: String,
        data: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        options: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        section: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        widget: Option<String>,
    },
    Bool {
        title: String,
        desc: String,
        data: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        section: Option<String>,
    },
    Int {
        title: String,
        desc: String,
        data: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        min: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        section: Option<String>,
    },
    Float {
        title: String,
        desc: String,
        data: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        section: Option<String>,
    },
    ListStr {
        title: String,
        desc: String,
        data: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        section: Option<String>,
    },
    Select {
        title: String,
        desc: String,
        data: String,
        options: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        section: Option<String>,
    },
}

pub type StructuredConfig = HashMap<String, StructuredConfigField>;

impl StructuredConfigField {
    /// 从 ConfigField（Schema）+ 默认值 构造结构化字段
    ///
    /// `group_scoped` 一律丢弃：它是授权判据，只能来自插件当前的声明，落盘会让陈旧的值
    /// 继续放行写入（见 [`ConfigField`] 的说明）。
    pub fn from_config_field(field: &ConfigField) -> Self {
        match field {
            ConfigField::String {
                title,
                desc,
                default,
                options,
                section,
                widget,
                group_scoped: _,
                user_scoped: _,
                ui: _,
            } => Self::String {
                title: title.clone(),
                desc: desc.clone(),
                data: default.clone(),
                options: options.clone(),
                section: section.clone(),
                widget: widget.clone(),
            },
            ConfigField::Bool {
                title,
                desc,
                default,
                section,
                group_scoped: _,
                user_scoped: _,
                ui: _,
            } => Self::Bool {
                title: title.clone(),
                desc: desc.clone(),
                data: *default,
                section: section.clone(),
            },
            ConfigField::Int {
                title,
                desc,
                default,
                min,
                max,
                section,
                group_scoped: _,
                user_scoped: _,
                ui: _,
            } => Self::Int {
                title: title.clone(),
                desc: desc.clone(),
                data: *default,
                min: *min,
                max: *max,
                section: section.clone(),
            },
            ConfigField::Float {
                title,
                desc,
                default,
                min,
                max,
                section,
                group_scoped: _,
                user_scoped: _,
                ui: _,
            } => Self::Float {
                title: title.clone(),
                desc: desc.clone(),
                data: *default,
                min: *min,
                max: *max,
                section: section.clone(),
            },
            ConfigField::ListStr {
                title,
                desc,
                default,
                options,
                section,
                group_scoped: _,
                user_scoped: _,
                ui: _,
            } => Self::ListStr {
                title: title.clone(),
                desc: desc.clone(),
                data: default.clone(),
                options: options.clone(),
                section: section.clone(),
            },
            ConfigField::Select {
                title,
                desc,
                default,
                options,
                section,
                group_scoped: _,
                user_scoped: _,
                ui: _,
            } => Self::Select {
                title: title.clone(),
                desc: desc.clone(),
                data: default.clone(),
                options: options.clone(),
                section: section.clone(),
            },
        }
    }

    /// 提取当前值
    pub fn data_value(&self) -> ConfigValue {
        match self {
            Self::String { data, .. } | Self::Select { data, .. } => {
                ConfigValue::String(data.clone())
            }
            Self::Bool { data, .. } => ConfigValue::Bool(*data),
            Self::Int { data, .. } => ConfigValue::Int(*data),
            Self::Float { data, .. } => ConfigValue::Float(*data),
            Self::ListStr { data, .. } => ConfigValue::ListStr(data.clone()),
        }
    }

    /// 提取为 ConfigField（用于前端 schema 渲染）
    ///
    /// `group_scoped` 恒为 `false`：文件里没有这一项，从磁盘反推出来的 schema 不能凭空
    /// 授权。`ui` 同样为空。完整表单描述和按群授权都必须用插件声明的活 schema。
    pub fn to_config_field(&self) -> ConfigField {
        match self {
            Self::String {
                title,
                desc,
                data,
                options,
                section,
                widget,
            } => ConfigField::String {
                title: title.clone(),
                desc: desc.clone(),
                default: data.clone(),
                options: options.clone(),
                section: section.clone(),
                widget: widget.clone(),
                group_scoped: false,
                user_scoped: false,
                ui: ConfigFieldUi::default(),
            },
            Self::Bool {
                title,
                desc,
                data,
                section,
            } => ConfigField::Bool {
                title: title.clone(),
                desc: desc.clone(),
                default: *data,
                section: section.clone(),
                group_scoped: false,
                user_scoped: false,
                ui: ConfigFieldUi::default(),
            },
            Self::Int {
                title,
                desc,
                data,
                min,
                max,
                section,
            } => ConfigField::Int {
                title: title.clone(),
                desc: desc.clone(),
                default: *data,
                min: *min,
                max: *max,
                section: section.clone(),
                group_scoped: false,
                user_scoped: false,
                ui: ConfigFieldUi::default(),
            },
            Self::Float {
                title,
                desc,
                data,
                min,
                max,
                section,
            } => ConfigField::Float {
                title: title.clone(),
                desc: desc.clone(),
                default: *data,
                min: *min,
                max: *max,
                section: section.clone(),
                group_scoped: false,
                user_scoped: false,
                ui: ConfigFieldUi::default(),
            },
            Self::ListStr {
                title,
                desc,
                data,
                options,
                section,
            } => ConfigField::ListStr {
                title: title.clone(),
                desc: desc.clone(),
                default: data.clone(),
                options: options.clone(),
                section: section.clone(),
                group_scoped: false,
                user_scoped: false,
                ui: ConfigFieldUi::default(),
            },
            Self::Select {
                title,
                desc,
                data,
                options,
                section,
            } => ConfigField::Select {
                title: title.clone(),
                desc: desc.clone(),
                default: data.clone(),
                options: options.clone(),
                section: section.clone(),
                group_scoped: false,
                user_scoped: false,
                ui: ConfigFieldUi::default(),
            },
        }
    }
}

pub use nuono_app::preferences::ThemeConfig;

/// 兼容旧结构的 Entry，但我们现在尽量简化
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ConfigEntry {
    #[serde(rename = "type")]
    pub type_name: String,
    pub data: ConfigValue,
}
