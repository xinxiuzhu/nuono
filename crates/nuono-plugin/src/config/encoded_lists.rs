use crate::{Result, Value};

/// 将显式声明为 `encoding = "json_string"` 的字符串数组字段编码为存储层所需的 JSON 字符串格式。
#[doc(hidden)]
pub fn normalize_json_string_lists(value: &mut Value, keys: &[&str]) -> Result<()> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| crate::anyhow!("配置必须是 JSON 对象"))?;
    for key in keys {
        let Some(value) = object.get_mut(*key) else {
            continue;
        };
        if let Value::Array(items) = value {
            if items.iter().any(|item| !item.is_string()) {
                return Err(crate::anyhow!("配置字段 {key} 必须是字符串列表"));
            }
            let encoded = crate::serde_json::to_string(items)
                .map_err(|_| crate::anyhow!("配置字段 {key} 的字符串列表编码失败"))?;
            *value = Value::String(encoded);
        }
    }
    Ok(())
}

/// 将显式声明为 `encoding = "json_string"` 的字段从 JSON 字符串还原为字符串数组，以供 Serde 反序列化到强类型结构体。
#[doc(hidden)]
pub fn decode_json_string_lists(value: &mut Value, keys: &[&str]) -> Result<()> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| crate::anyhow!("配置必须是 JSON 对象"))?;
    for key in keys {
        let Some(value) = object.get_mut(*key) else {
            continue;
        };
        if let Value::String(encoded) = value {
            let items: Vec<String> = crate::serde_json::from_str(encoded)
                .map_err(|_| crate::anyhow!("配置字段 {key} 必须是 JSON 编码的字符串列表"))?;
            *value = Value::Array(items.into_iter().map(Value::String).collect());
        }
    }
    Ok(())
}
