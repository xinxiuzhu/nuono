//! 插件配置声明与值校验；宿主、SDK 解码和插件测试共用。
use crate::config_schema::{ConfigFieldType, ConfigFieldUi, PluginConfigField};
use serde_json::Value;
use std::collections::HashSet;

/// 从字段声明提取默认值。加载前应调用 `validate_config_fields` 校验声明。
pub fn config_defaults(fields: &[PluginConfigField]) -> Value {
    Value::Object(
        fields
            .iter()
            .map(|f| (f.key.clone(), f.default.clone()))
            .collect(),
    )
}

/// 校验一个值，不在错误信息中回显用户配置。
pub fn validate_config_field_value(
    key: &str,
    value: &Value,
    field: &PluginConfigField,
) -> Result<(), String> {
    let ui = field.effective_ui();
    if ui.editor.is_some() && ui.encoding.as_deref() == Some("native") {
        return validate_ui_value(key, value, &ui);
    }
    let valid = match field.field_type {
        ConfigFieldType::Number => {
            if let Some(v) = value.as_i64() {
                if field.min.is_some_and(|m| v < m) || field.max.is_some_and(|m| v > m) {
                    return Err(format!(
                        "插件配置字段 {key} 超出允许数字范围 {}..={}",
                        field
                            .min
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "不限".into()),
                        field
                            .max
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "不限".into())
                    ));
                }
                true
            } else {
                false
            }
        }
        ConfigFieldType::Float => value.as_f64().is_some_and(|v| {
            v.is_finite()
                && field.min.is_none_or(|m| v >= m as f64)
                && field.max.is_none_or(|m| v <= m as f64)
        }),
        ConfigFieldType::Boolean | ConfigFieldType::Switch => value.is_boolean(),
        ConfigFieldType::Token => value
            .as_array()
            .is_some_and(|a| a.iter().all(Value::is_string)),
        ConfigFieldType::Text
        | ConfigFieldType::Textarea
        | ConfigFieldType::Password
        | ConfigFieldType::Select => value.is_string(),
        ConfigFieldType::IntegerList => false,
    };
    if !valid {
        return Err(format!("插件配置字段 {key} 类型不匹配或超出允许数字范围"));
    }
    if field.field_type == ConfigFieldType::Token && !allows_custom_list_options(&ui) {
        let options = ui
            .options
            .iter()
            .map(|o| o.value.clone())
            .collect::<Vec<_>>();
        for v in value.as_array().unwrap() {
            validate_options(key, v.as_str().unwrap(), &options)?;
        }
    }
    validate_ui_value(key, value, &ui)
}

/// 检查字段声明；子对象的必填空默认是编辑新条目的模板，实际条目仍严格校验。
pub fn validate_config_fields(fields: &[PluginConfigField]) -> Result<(), String> {
    validate_declarations(fields, fields, "", true)
}

/// 校验配置声明中的密码凭据字段：所有密码字段必须是可独立寻址的顶层字段，绝不能嵌套在对象列表或映射中。
/// 该校验同时供插件直接注册与宿主转换后的 Schema 注册路径共用。
pub fn validate_config_ui_credentials(key: &str, ui: &ConfigFieldUi) -> Result<(), String> {
    for child in &ui.fields {
        let child_key = format!("{key}.{}", child.key);
        if child.field_type == ConfigFieldType::Password {
            return Err(format!(
                "插件配置声明 {child_key}: 凭据不能嵌套在对象中，请声明为独立顶层密码字段"
            ));
        }
        validate_config_ui_credentials(&child_key, &child.effective_ui())?;
    }
    Ok(())
}

fn validate_declarations(
    fields: &[PluginConfigField],
    root: &[PluginConfigField],
    prefix: &str,
    top: bool,
) -> Result<(), String> {
    let mut seen = HashSet::new();
    for field in fields {
        let key = format!("{prefix}{}", field.key);
        let fail = |reason: &str| format!("插件配置声明 {key}: {reason}");
        if field.key.trim().is_empty() || field.label.trim().is_empty() {
            return Err(fail("字段键名和显示名称不能为空"));
        }
        if !seen.insert(&field.key) {
            return Err(fail("字段键名重复"));
        }
        if field.field_type == ConfigFieldType::IntegerList {
            return Err(fail("不支持 IntegerList；请显式声明支持的配置类型"));
        }
        if field.min.zip(field.max).is_some_and(|(a, b)| a > b) {
            return Err(fail("最小值不能大于最大值"));
        }
        let ui = field.effective_ui();
        if top {
            validate_config_ui_credentials(&key, &ui)?;
        }
        if top
            && ui.encoding.as_deref() == Some("native")
            && ui.editor.is_some()
            && !(field.field_type == ConfigFieldType::Token
                && matches!(ui.editor.as_deref(), Some("tags" | "multiselect")))
        {
            return Err(fail("顶层原生列表组件必须使用 Token 字符串列表类型"));
        }
        if (field.min.is_some() || field.max.is_some())
            && !matches!(
                field.field_type,
                ConfigFieldType::Number | ConfigFieldType::Float
            )
        {
            return Err(fail("数字范围只能用于数字字段"));
        }
        if !ui.fields.is_empty()
            && !matches!(ui.editor.as_deref(), Some("object_list" | "key_value"))
        {
            return Err(fail("只有对象列表和数字映射可以声明子字段"));
        }
        if ui.step.is_some_and(|v| !v.is_finite() || v <= 0.0) {
            return Err(fail("步长必须是有限正数"));
        }
        if !matches!(
            ui.editor.as_deref(),
            None | Some("tags" | "multiselect" | "object_list" | "key_value")
        ) {
            return Err(fail("未知表单组件"));
        }
        if !matches!(
            ui.encoding.as_deref(),
            None | Some("native" | "json_string")
        ) {
            return Err(fail("未知值编码"));
        }
        if !matches!(ui.span.as_deref(), None | Some("half" | "full")) {
            return Err(fail("布局宽度提示仅支持 half 或 full"));
        }
        if ui.encoding.as_deref() == Some("json_string")
            && !matches!(
                field.field_type,
                ConfigFieldType::Text | ConfigFieldType::Textarea | ConfigFieldType::Password
            )
        {
            return Err(fail("JSON 字符串编码必须使用字符串字段"));
        }
        if top
            && ui.encoding.as_deref() == Some("native")
            && matches!(ui.editor.as_deref(), Some("object_list" | "key_value"))
        {
            return Err(fail("顶层对象容器须显式使用 JSON 字符串编码"));
        }
        let mut options = HashSet::new();
        for option in &ui.options {
            if !options.insert(&option.value) {
                return Err(fail("选项值重复"));
            }
        }
        if let Some(condition) = &ui.visible_when {
            let target = fields
                .iter()
                .chain(root)
                .find(|f| f.key == condition.key)
                .ok_or_else(|| fail("显示条件引用了未声明字段"))?;
            validate_config_field_value(&condition.key, &condition.value, target)?;
        }
        if let Some(source) = &ui.options_source {
            let target = root
                .iter()
                .find(|f| f.key == source.key)
                .ok_or_else(|| fail("选项来源引用了未声明字段"))?;
            if target.ui.editor.as_deref() != Some("object_list") {
                return Err(fail("动态选项来源必须是对象列表"));
            }
            for name in std::iter::once(&source.value_key).chain(source.label_key.iter()) {
                if !target.ui.fields.iter().any(|f| &f.key == name) {
                    return Err(fail("动态选项引用了未声明的对象子字段"));
                }
            }
        }
        if let Some(label) = &ui.item_label
            && !ui.fields.iter().any(|f| {
                &f.key == label
                    && matches!(
                        f.field_type,
                        ConfigFieldType::Text | ConfigFieldType::Select
                    )
            })
        {
            return Err(fail("条目标识必须引用字符串子字段"));
        }
        if ui.editor.as_deref() == Some("key_value")
            && (ui.fields.len() > 1
                || ui.fields.first().is_some_and(|f| {
                    !matches!(
                        f.field_type,
                        ConfigFieldType::Number | ConfigFieldType::Float
                    )
                }))
        {
            return Err(fail("数字映射只能声明一个数字值字段"));
        }
        validate_declarations(&ui.fields, root, &format!("{key}."), false)?;
        let mut default_field = field.clone();
        if !top {
            default_field.required = false;
            default_field.ui.required = false;
        }
        validate_config_field_value(&key, &field.default, &default_field)
            .map_err(|e| format!("{e}（声明默认值）"))?;
    }
    Ok(())
}
fn allows_custom_list_options(ui: &ConfigFieldUi) -> bool {
    ui.allow_custom && matches!(ui.editor.as_deref(), Some("tags" | "multiselect"))
}

pub fn validate_number(
    key: &str,
    value: f64,
    min: Option<f64>,
    max: Option<f64>,
) -> std::result::Result<(), String> {
    if !value.is_finite()
        || min.is_some_and(|min| value < min)
        || max.is_some_and(|max| value > max)
    {
        Err(format!("插件配置字段 {key} 超出允许数字范围"))
    } else {
        Ok(())
    }
}

fn validate_options(key: &str, value: &str, options: &[String]) -> std::result::Result<(), String> {
    if !options.is_empty() && !options.iter().any(|option| option == value) {
        Err(format!("插件配置字段 {key} 不在允许选项中"))
    } else {
        Ok(())
    }
}

fn is_empty_value(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::String(value) => value.trim().is_empty(),
        serde_json::Value::Array(value) => value.is_empty(),
        serde_json::Value::Object(value) => value.is_empty(),
        _ => false,
    }
}

pub fn validate_ui_value(
    key: &str,
    value: &serde_json::Value,
    ui: &ConfigFieldUi,
) -> std::result::Result<(), String> {
    let decoded;
    let value = if ui.encoding.as_deref() == Some("json_string") {
        let encoded = value
            .as_str()
            .ok_or_else(|| format!("插件配置字段 {key} 必须使用 JSON 字符串编码"))?;
        decoded = serde_json::from_str::<serde_json::Value>(encoded)
            .map_err(|_| format!("插件配置字段 {key} JSON 格式无效"))?;
        &decoded
    } else {
        value
    };
    if ui.required && is_empty_value(value) {
        return Err(format!("插件配置字段 {key} 为必填项"));
    }
    let options: Vec<String> = ui
        .options
        .iter()
        .map(|option| option.value.clone())
        .collect();
    match ui.editor.as_deref() {
        Some("tags" | "multiselect") => {
            let items = value
                .as_array()
                .ok_or_else(|| format!("插件配置字段 {key} 必须是字符串列表"))?;
            for item in items {
                let item = item
                    .as_str()
                    .ok_or_else(|| format!("插件配置字段 {key} 列表项必须是字符串"))?;
                if !ui.allow_custom {
                    validate_options(key, item, &options)?;
                }
            }
        }
        Some("object_list") => {
            let items = value
                .as_array()
                .ok_or_else(|| format!("插件配置字段 {key} 必须是对象列表"))?;
            let mut labels = std::collections::HashSet::new();
            for (index, item) in items.iter().enumerate() {
                let object = item
                    .as_object()
                    .ok_or_else(|| format!("插件配置字段 {key} 列表项必须是对象"))?;
                if let Some(label_key) = &ui.item_label {
                    let label = object
                        .get(label_key)
                        .and_then(serde_json::Value::as_str)
                        .filter(|label| !label.trim().is_empty())
                        .ok_or_else(|| format!("插件配置字段 {key} 列表项缺少有效标识"))?;
                    if !labels.insert(label) {
                        return Err(format!("插件配置字段 {key} 列表项标识重复"));
                    }
                }
                for field in &ui.fields {
                    let nested_key = format!("{key}[{index}].{}", field.key);
                    match object.get(&field.key) {
                        Some(value) => validate_config_field_value(&nested_key, value, field)?,
                        None if field.effective_ui().required => {
                            return Err(format!("插件配置字段 {nested_key} 为必填项"));
                        }
                        None => {}
                    }
                }
            }
        }
        Some("key_value") => {
            let object = value
                .as_object()
                .ok_or_else(|| format!("插件配置字段 {key} 必须是数字映射"))?;
            // 可选的单个数字字段描述映射中的每个值；不通过用户键名推断字段。
            let value_field = ui.fields.first();
            for (index, (map_key, item)) in object.iter().enumerate() {
                if map_key.trim().is_empty() {
                    return Err(format!("插件配置字段 {key} 映射键不能为空"));
                }
                let number = item
                    .as_f64()
                    .ok_or_else(|| format!("插件配置字段 {key} 映射值必须是数字"))?;
                validate_number(key, number, None, None)?;
                if let Some(field) = value_field {
                    validate_config_field_value(&format!("{key}[{index}]"), item, field)?;
                }
            }
        }
        _ => {
            if let Some(value) = value.as_str() {
                validate_options(key, value, &options)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod credential_scope_tests {
    use super::*;

    fn objects(children: Vec<PluginConfigField>) -> PluginConfigField {
        PluginConfigField::json_object_list("services", "Services", "", "[]", children, "name")
    }

    #[test]
    fn nested_credentials_are_rejected_with_the_full_field_path() {
        let name = PluginConfigField::text("name", "Name", "", "");
        for credential in [
            PluginConfigField::password("api_key", "API key", ""),
            objects(vec![
                name.clone(),
                PluginConfigField::password("api_key", "API key", ""),
            ])
            .with_encoding("native")
            .with_default(serde_json::json!([])),
        ] {
            let error =
                validate_config_fields(&[objects(vec![name.clone(), credential])]).unwrap_err();
            assert!(error.contains("services."));
            assert!(error.contains("api_key"));
            assert!(error.contains("独立顶层密码字段"));
        }
    }

    #[test]
    fn top_level_password_and_nested_token_tag_lists_remain_supported() {
        let fields = vec![
            PluginConfigField::password("api_key", "API key", ""),
            PluginConfigField::token("tags", "Tags", ""),
            objects(vec![
                PluginConfigField::text("name", "Name", "", ""),
                PluginConfigField::string_list("tags", "Tags", "", Vec::new()),
            ]),
        ];
        validate_config_fields(&fields).unwrap();
    }

    #[test]
    fn layout_metadata_span_advanced_and_hint_are_validated() {
        let valid = vec![
            PluginConfigField::text("endpoint", "Endpoint", "Long help", "")
                .full_width()
                .advanced()
                .with_hint("Short inline summary"),
            PluginConfigField::number("timeout", "Timeout", "", 30).half_width(),
        ];
        validate_config_fields(&valid).unwrap();
        assert_eq!(valid[0].ui.span.as_deref(), Some("full"));
        assert!(valid[0].ui.advanced);
        assert_eq!(valid[0].ui.hint.as_deref(), Some("Short inline summary"));
        assert_eq!(valid[1].ui.span.as_deref(), Some("half"));

        let mut invalid = PluginConfigField::text("bad_span", "Bad span", "", "");
        invalid.ui.span = Some("third".into());
        let err = validate_config_fields(&[invalid]).unwrap_err();
        assert!(err.contains("布局宽度提示"));
    }
}
