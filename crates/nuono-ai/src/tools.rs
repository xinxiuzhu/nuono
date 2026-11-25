//! 工具调用的参数与结构定义，不依赖 Agent 存储、长期记忆或具体的模型协议。
use nuono_domain::{BotIdentity, ConversationRef, IdentityRef};
use nuono_error::{NuonoError, Result};
use serde_json::Value;
use std::sync::Arc;

/// 由宿主注入的工具调用上下文身份信息，绝不由模型生成参数填充。
/// 插件自身的服务依赖仍由工具实现自行管理。
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub bot: BotIdentity,
    pub actor: IdentityRef,
    pub conversation: Option<ConversationRef>,
    pub message_id: Option<String>,
}

pub const MAX_TOOL_SCHEMA_BYTES: usize = 64 * 1024;
pub const MAX_TOOL_INPUT_BYTES: usize = 1024 * 1024;

/// 工具注册时编译好的参数 Schema 快照，供模型发现工具及执行前校验参数使用。
#[derive(Clone)]
pub struct ToolInputSchema {
    schema: Value,
    validator: Arc<jsonschema::Validator>,
}

struct NoExternalReferences;
impl jsonschema::Retrieve for NoExternalReferences {
    fn retrieve(
        &self,
        _: &jsonschema::Uri<String>,
    ) -> std::result::Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("external tool schema references are disabled".into())
    }
}

impl ToolInputSchema {
    pub fn compile(schema: Value) -> Result<Self> {
        if !schema.is_object()
            || schema.get("type").and_then(Value::as_str) != Some("object")
            || serde_json::to_vec(&schema)?.len() > MAX_TOOL_SCHEMA_BYTES
        {
            return Err(NuonoError::ConfigValidation(
                "工具 input_schema 必须是有界的 object JSON Schema".into(),
            ));
        }
        // 禁止通过 HTTP 或本地文件解析外部引用，仅支持 Schema 内部的 `$defs`/`$ref` 引用。
        let validator = jsonschema::options()
            .with_retriever(NoExternalReferences)
            .build(&schema)
            .map_err(|error| {
                NuonoError::ConfigValidation(format!(
                    "工具 JSON Schema 无效或引用不可用（{}）",
                    error.schema_path()
                ))
            })?;
        Ok(Self {
            schema,
            validator: Arc::new(validator),
        })
    }

    pub fn value(&self) -> &Value {
        &self.schema
    }

    pub fn validate(&self, input: &Value) -> Result<()> {
        if serde_json::to_vec(input)?.len() > MAX_TOOL_INPUT_BYTES {
            return Err(NuonoError::Validation("工具参数超过大小限制".into()));
        }
        self.validator.validate(input).map_err(|error| {
            // 切勿在错误信息中回显输入值（可能包含敏感凭据）。
            NuonoError::Validation(format!(
                "工具参数不符合 JSON Schema：字段 {}，规则 {}",
                error.instance_path(),
                error.schema_path()
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nested_constraints_and_local_references_are_enforced() {
        let schema = ToolInputSchema::compile(json!({
            "type":"object", "additionalProperties":false,
            "required":["items"],
            "properties":{"items":{"type":"array","minItems":1,
                "items":{"$ref":"#/$defs/Item"}}},
            "$defs":{"Item":{"type":"object","required":["kind","count"],
                "properties":{"kind":{"enum":["role","weapon"]},
                    "count":{"type":"integer","minimum":1,"maximum":5}}}}
        }))
        .unwrap();
        assert!(
            schema
                .validate(&json!({"items":[{"kind":"role","count":2}]}))
                .is_ok()
        );
        for input in [
            json!({}),
            json!({"items":[]}),
            json!({"items":[{"kind":"unknown","count":2}]}),
            json!({"items":[{"kind":"role","count":9}]}),
            json!({"items":[{"kind":"role","count":2}],"extra":true}),
        ] {
            assert!(schema.validate(&input).is_err());
        }
    }

    #[test]
    fn remote_and_file_references_are_not_resolved() {
        for reference in ["https://example.com/schema.json", "file:///etc/passwd"] {
            assert!(
                ToolInputSchema::compile(json!({
                    "type":"object", "properties":{"value":{"$ref":reference}}
                }))
                .is_err()
            );
        }
        assert!(ToolInputSchema::compile(json!({"type":"object","required":42})).is_err());
    }
}
