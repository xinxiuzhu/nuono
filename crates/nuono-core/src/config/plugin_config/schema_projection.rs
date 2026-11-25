//! 插件配置表单 Schema 派生、数据库值合并与类型校验辅助函数。

use std::collections::HashMap;

use crate::config::schema::{
    ConfigField, ConfigSchema, ConfigValue, StructuredConfig, StructuredConfigField,
};
use crate::core::config_schema::{ConfigFieldType, PluginConfigField};
use crate::core::plugin_service::{PluginArea, Plugins};
use crate::error::{NuonoError, Result};

pub(super) fn toml_contains_datetime(value: &toml::Value) -> bool {
    match value {
        toml::Value::Datetime(_) => true,
        toml::Value::Array(values) => values.iter().any(toml_contains_datetime),
        toml::Value::Table(values) => values.values().any(toml_contains_datetime),
        _ => false,
    }
}

pub fn secret_fields(schema: &ConfigSchema) -> Vec<String> {
    schema.iter().filter(|(_,field)|matches!(field,ConfigField::String{widget:Some(widget),..} if widget=="password" || widget=="token")).map(|(key,_)|key.clone()).collect()
}

pub(super) fn database_values(
    schema: &ConfigSchema,
    saved: &nuono_storage::plugins::PluginConfiguration,
) -> Result<StructuredConfig> {
    let mut result: StructuredConfig = schema
        .iter()
        .map(|(key, field)| (key.clone(), StructuredConfigField::from_config_field(field)))
        .collect();
    for (key, raw) in saved.values.iter().chain(&saved.service) {
        let field = schema.get(key).ok_or_else(|| {
            NuonoError::ConfigValidation(format!("已保存配置字段 {key} 未声明，需要显式转换"))
        })?;
        let value: ConfigValue = serde_json::from_value(raw.clone())
            .map_err(|_| NuonoError::ConfigValidation(format!("插件配置字段 {key} 类型错误")))?;
        validate_config_value(key, &value, field).map_err(NuonoError::ConfigValidation)?;
        if let Some(entry) = result.get_mut(key) {
            set_entry_data(key, entry, value)?;
        }
    }
    for (key, field) in schema {
        if let Some(entry) = result.get(key) {
            validate_config_value(key, &entry.data_value(), field)
                .map_err(NuonoError::ConfigValidation)?;
        }
    }
    Ok(result)
}

/// 将插件公开的字段声明转换为管理端与持久化层共享的 schema。
///
/// `group_scoped` 从插件声明原样带过来 —— 这里是它进入授权判据的唯一入口，
/// 所以不做任何加工，插件没标就是 false。
pub fn plugin_fields_to_schema(fields: &[PluginConfigField]) -> Result<ConfigSchema> {
    nuono_plugin::config_validation::validate_config_fields(fields)
        .map_err(NuonoError::ConfigValidation)?;
    Ok(fields
        .iter()
        .map(|field| {
            let section = field.section.clone();
            let group_scoped = field.group_scoped;
            let user_scoped = field.user_scoped;
            let ui = field.effective_ui();
            let options: Vec<String> = ui
                .options
                .iter()
                .map(|option| option.value.clone())
                .collect();
            let schema = match &field.field_type {
                ConfigFieldType::Boolean | ConfigFieldType::Switch => ConfigField::Bool {
                    title: field.label.clone(),
                    desc: field.description.clone(),
                    default: field.default.as_bool().unwrap_or(false),
                    section,
                    group_scoped,
                    user_scoped,
                    ui,
                },
                ConfigFieldType::Number => ConfigField::Int {
                    title: field.label.clone(),
                    desc: field.description.clone(),
                    default: field.default.as_i64().unwrap_or(0),
                    min: field.min,
                    max: field.max,
                    section,
                    group_scoped,
                    user_scoped,
                    ui,
                },
                ConfigFieldType::Float => ConfigField::Float {
                    title: field.label.clone(),
                    desc: field.description.clone(),
                    default: field.default.as_f64().unwrap_or(0.0),
                    min: field.min.map(|value| value as f64),
                    max: field.max.map(|value| value as f64),
                    section,
                    group_scoped,
                    user_scoped,
                    ui,
                },
                ConfigFieldType::Select => ConfigField::Select {
                    title: field.label.clone(),
                    desc: field.description.clone(),
                    default: field.default.as_str().unwrap_or_default().to_string(),
                    options,
                    section,
                    group_scoped,
                    user_scoped,
                    ui,
                },
                ConfigFieldType::IntegerList | ConfigFieldType::Token => ConfigField::ListStr {
                    title: field.label.clone(),
                    desc: field.description.clone(),
                    default: field
                        .default
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                    options,
                    section,
                    group_scoped,
                    user_scoped,
                    ui,
                },
                ConfigFieldType::Text | ConfigFieldType::Textarea | ConfigFieldType::Password => {
                    ConfigField::String {
                        title: field.label.clone(),
                        desc: field.description.clone(),
                        default: field.default.as_str().unwrap_or_default().to_string(),
                        options: (!options.is_empty()).then_some(options),
                        section,
                        widget: match field.field_type {
                            ConfigFieldType::Textarea => Some("textarea".into()),
                            ConfigFieldType::Password => Some("password".into()),
                            _ => None,
                        },
                        group_scoped,
                        user_scoped,
                        ui,
                    }
                }
            };
            (field.key.clone(), schema)
        })
        .collect())
}

pub fn is_service_field(key: &str) -> bool {
    key == "auto_listen_enabled" || service_schema(None).contains_key(key)
}

pub(super) fn service_schema(service_config: Option<&Plugins>) -> ConfigSchema {
    let mut structured = StructuredConfig::new();
    // 注入 prefix（插件命令入口前缀，自动监听使用独立能力及开关）
    if !structured.contains_key("prefix") {
        let prefix = service_config
            .map(|cfg| cfg.prefix.clone())
            .filter(|v| !v.is_empty())
            .unwrap_or_default();
        structured.insert(
            "prefix".to_string(),
            StructuredConfigField::ListStr {
                title: "命令前缀".to_string(),
                desc: "限定插件命令和规则入口，命中后再匹配正文；自动监听独立管理。英文字母不区分大小写，多个前缀取最长匹配".to_string(),
                data: prefix,
                options: vec![],
                section: Some("服务配置".to_string()),
            },
        );
    }

    if !structured.contains_key("force_prefix") {
        let force_prefix = service_config
            .map(|cfg| cfg.force_prefix.clone())
            .unwrap_or_default();
        structured.insert(
            "force_prefix".to_string(),
            StructuredConfigField::ListStr {
                title: "额外前缀".to_string(),
                desc: "与插件前缀并列的额外入口；不是需要连续输入的第二层前缀".to_string(),
                data: force_prefix,
                options: vec![],
                section: Some("服务配置".to_string()),
            },
        );
    }

    if !structured.contains_key("allow_empty_prefix") {
        structured.insert(
            "allow_empty_prefix".to_string(),
            StructuredConfigField::Bool {
                title: "允许空前缀".to_string(),
                desc: "已停用，仅保留旧配置兼容：有非空有效前缀时必须命中；没有时直接匹配正文，此值不再影响路由".to_string(),
                data: service_config
                    .map(|cfg| cfg.allow_empty_prefix)
                    .unwrap_or(false),
                section: Some("服务配置".to_string()),
            },
        );
    }

    if !structured.contains_key("disable_force_prefix") {
        structured.insert(
            "disable_force_prefix".to_string(),
            StructuredConfigField::Bool {
                title: "禁用额外前缀".to_string(),
                desc: "开启后忽略 force_prefix，只保留插件前缀".to_string(),
                data: service_config
                    .map(|cfg| cfg.disable_force_prefix)
                    .unwrap_or(false),
                section: Some("服务配置".to_string()),
            },
        );
    }

    if !structured.contains_key("min_permission") {
        structured.insert(
            "min_permission".to_string(),
            StructuredConfigField::Select {
                title: "最低权限等级".to_string(),
                desc: "正序 5 档权限：Everyone < GroupAdmin < GroupOwner < BotAdmin < BotOwner"
                    .to_string(),
                data: service_config
                    .map(|cfg| cfg.min_permission.as_str().to_string())
                    .unwrap_or_else(|| {
                        crate::core::permission::PermissionLevel::Everyone
                            .as_str()
                            .to_string()
                    }),
                options: vec![
                    "Everyone".to_string(),
                    "GroupAdmin".to_string(),
                    "GroupOwner".to_string(),
                    "BotAdmin".to_string(),
                    "BotOwner".to_string(),
                ],
                section: Some("服务配置".to_string()),
            },
        );
    }

    if !structured.contains_key("area") {
        structured.insert(
            "area".to_string(),
            StructuredConfigField::Select {
                title: "生效区域".to_string(),
                desc: "限制插件在群聊、私聊或全部场景生效".to_string(),
                data: service_config
                    .map(|cfg| area_to_string(cfg.area))
                    .unwrap_or_else(|| "ALL".to_string()),
                options: vec!["ALL".to_string(), "GROUP".to_string(), "DIRECT".to_string()],
                section: Some("服务配置".to_string()),
            },
        );
    }

    if !structured.contains_key("black_list") {
        let black_list = service_config
            .map(|cfg| cfg.black_list.clone())
            .unwrap_or_default();
        structured.insert(
            "black_list".to_string(),
            StructuredConfigField::ListStr {
                title: "黑名单".to_string(),
                desc: "禁止使用该插件的用户 ID 或群 ID".to_string(),
                data: black_list,
                options: vec![],
                section: Some("服务配置".to_string()),
            },
        );
    }

    if !structured.contains_key("white_list") {
        let white_list = service_config
            .map(|cfg| cfg.white_list.clone())
            .unwrap_or_default();
        structured.insert(
            "white_list".to_string(),
            StructuredConfigField::ListStr {
                title: "白名单".to_string(),
                desc: "非空时，仅允许列表中的用户 ID 或群 ID 使用该插件".to_string(),
                data: white_list,
                options: vec![],
                section: Some("服务配置".to_string()),
            },
        );
    }

    if !structured.contains_key("alias") {
        let alias = service_config
            .map(|cfg| cfg.alias.clone())
            .unwrap_or_default();
        structured.insert(
            "alias".to_string(),
            StructuredConfigField::ListStr {
                title: "插件别名".to_string(),
                desc: "插件显示或业务识别使用的别名".to_string(),
                data: alias,
                options: vec![],
                section: Some("服务配置".to_string()),
            },
        );
    }

    if !structured.contains_key("disabled_commands") {
        structured.insert(
            "disabled_commands".to_string(),
            StructuredConfigField::ListStr {
                title: "禁用指令".to_string(),
                desc: "由指令管理自动维护，记录当前插件被禁用的指令".to_string(),
                data: vec![],
                options: vec![],
                section: Some("服务配置".to_string()),
            },
        );
    }

    let mut schema: ConfigSchema = structured
        .into_iter()
        .map(|(key, field)| (key, field.to_config_field()))
        .collect();
    if let Some(enabled) = service_config.and_then(|cfg| cfg.auto_listen_enabled) {
        schema.insert(
            "auto_listen_enabled".into(),
            ConfigField::Bool {
                title: "自动监听".into(),
                desc: "监听插件声明的普通消息；已被显式命令前缀认领的消息不会进入自动监听。可按群覆盖，关闭后仍可使用命令前缀".into(),
                default: enabled,
                section: Some("服务配置".into()),
                group_scoped: true,
                user_scoped: false,
                ui: Default::default(),
            },
        );
    }
    schema
}

pub(super) fn schema_to_defaults(schema: &ConfigSchema) -> HashMap<String, ConfigValue> {
    schema
        .iter()
        .map(|(k, v)| (k.clone(), field_default(v)))
        .collect()
}

fn field_default(field: &ConfigField) -> ConfigValue {
    match field {
        ConfigField::String { default, .. } => ConfigValue::String(default.clone()),
        ConfigField::Bool { default, .. } => ConfigValue::Bool(*default),
        ConfigField::Int { default, .. } => ConfigValue::Int(*default),
        ConfigField::Float { default, .. } => ConfigValue::Float(*default),
        ConfigField::ListStr { default, .. } => ConfigValue::ListStr(default.clone()),
        ConfigField::Select { default, .. } => ConfigValue::String(default.clone()),
    }
}

pub(crate) fn set_entry_data(
    key: &str,
    entry: &mut StructuredConfigField,
    value: ConfigValue,
) -> Result<()> {
    match (entry, value) {
        (StructuredConfigField::String { data, .. }, ConfigValue::String(v)) => *data = v,
        (StructuredConfigField::Bool { data, .. }, ConfigValue::Bool(v)) => *data = v,
        (StructuredConfigField::Int { data, .. }, ConfigValue::Int(v)) => *data = v,
        (StructuredConfigField::Float { data, .. }, ConfigValue::Float(v)) => *data = v,
        (StructuredConfigField::Float { data, .. }, ConfigValue::Int(v)) => *data = v as f64,
        (StructuredConfigField::ListStr { data, .. }, ConfigValue::ListStr(v)) => *data = v,
        (StructuredConfigField::Select { data, .. }, ConfigValue::String(v)) => *data = v,
        _ => {
            return Err(NuonoError::ConfigValidation(format!(
                "插件配置字段 {key} 的持久化类型不兼容，请显式迁移后保存"
            )));
        }
    }
    Ok(())
}

fn area_to_string(area: PluginArea) -> String {
    match area {
        PluginArea::Group => "GROUP",
        PluginArea::Direct => "DIRECT",
        PluginArea::All | PluginArea::FollowPlugin | _ => "ALL",
    }
    .to_string()
}

pub(super) fn sanitize_component(name: &str) -> String {
    let sanitized = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect::<String>();
    let trimmed = sanitized.trim();
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 所有文件和 HTTP 保存入口共用的校验。错误只描述字段和约束，不包含配置值。
pub fn validate_config_value(
    key: &str,
    value: &ConfigValue,
    field: &ConfigField,
) -> std::result::Result<(), String> {
    let mut declared = match field {
        ConfigField::String { options, .. } => {
            PluginConfigField::text(key, field.title(), field.desc(), "").with_options(
                options
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|value| nuono_plugin::ConfigFieldOption {
                        label: value.clone(),
                        value,
                    })
                    .collect(),
            )
        }
        ConfigField::Select { options, .. } => PluginConfigField::select(
            key,
            field.title(),
            field.desc(),
            "",
            options
                .iter()
                .map(|value| nuono_plugin::ConfigFieldOption {
                    label: value.clone(),
                    value: value.clone(),
                })
                .collect(),
        ),
        ConfigField::Bool { .. } => {
            PluginConfigField::boolean(key, field.title(), field.desc(), false)
        }
        ConfigField::Int { min, max, .. } => {
            let mut field = PluginConfigField::number(key, field.title(), field.desc(), 0);
            field.min = *min;
            field.max = *max;
            field
        }
        ConfigField::Float { min, max, .. } => {
            let number = match value {
                ConfigValue::Float(v) => *v,
                ConfigValue::Int(v) => *v as f64,
                _ => return Err(format!("插件配置字段 {key} 类型不匹配")),
            };
            nuono_plugin::config_validation::validate_number(key, number, *min, *max)?;
            PluginConfigField::decimal(key, field.title(), field.desc(), 0.0)
        }
        ConfigField::ListStr { options, .. } => {
            PluginConfigField::string_list(key, field.title(), field.desc(), vec![]).with_options(
                options
                    .iter()
                    .map(|value| nuono_plugin::ConfigFieldOption {
                        label: value.clone(),
                        value: value.clone(),
                    })
                    .collect(),
            )
        }
    };
    declared.ui = field.ui().clone();
    let json = serde_json::to_value(value).map_err(|_| format!("插件配置字段 {key} 类型不匹配"))?;
    nuono_plugin::config_validation::validate_config_field_value(key, &json, &declared)
}
