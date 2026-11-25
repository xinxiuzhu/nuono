//! 将插件声明的工作流技能（Skill）封装为生命周期与该插件绑定的只读 Agent 工具。
use super::*;
use crate::error::NuonoError;
use crate::plugin::PluginAgentSkill;
use std::collections::HashSet;

#[cfg(test)]
#[path = "../../../tests/unit/core/plugin_host/tool_schema_tests.rs"]
mod tool_schema_tests;

struct PluginSkillTool {
    name: String,
    description: String,
    document: Value,
}

#[async_trait::async_trait]
impl AgentTool for PluginSkillTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn call(&self, _ctx: AgentToolContext, input: Value) -> Result<Value> {
        if !input.as_object().is_some_and(|fields| fields.is_empty()) {
            return Err(NuonoError::ConfigValidation(
                "Plugin skill reader accepts an empty object".into(),
            ));
        }
        Ok(self.document.clone())
    }
}

fn invalid(plugin: &str, reason: &str) -> NuonoError {
    NuonoError::ConfigValidation(format!("插件 {plugin} 的 Agent 声明无效：{reason}"))
}

fn plain_metadata(value: &str, limit: usize) -> bool {
    !value.trim().is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}

pub(super) fn scoped_tools(
    plugin: &str,
    tools: Vec<Arc<dyn AgentTool>>,
    skills: Vec<PluginAgentSkill>,
) -> Result<Vec<Arc<dyn AgentTool>>> {
    let mut declared = HashMap::new();
    let mut compiled: Vec<Arc<dyn AgentTool>> = Vec::with_capacity(tools.len() + skills.len());
    for tool in tools {
        let local_name = tool.name().to_string();
        if local_name.trim().is_empty()
            || local_name.starts_with("skill.")
            || declared.contains_key(&local_name)
        {
            return Err(invalid(plugin, "工具名为空、重复或占用了 skill. 命名空间"));
        }
        let scoped_name = format!("plugin.{plugin}.{local_name}");
        declared.insert(local_name, (scoped_name.clone(), tool.risk_level()));
        compiled.push(Arc::new(PluginScopedTool {
            scoped_name,
            inner: tool,
            diagnostics: None,
            epoch: None,
        }));
    }

    if skills.len() > 32 {
        return Err(invalid(plugin, "工作流数量超过 32"));
    }
    let mut ids = HashSet::new();
    let mut total_body_bytes = 0usize;
    for skill in skills {
        if skill.id.is_empty()
            || skill.id.len() > 64
            || !skill
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            || !ids.insert(skill.id.clone())
        {
            return Err(invalid(plugin, "工作流 ID 为空、非法或重复"));
        }
        if !plain_metadata(&skill.title, 128) || !plain_metadata(&skill.description, 2048) {
            return Err(invalid(plugin, "工作流标题或描述无效"));
        }
        total_body_bytes = total_body_bytes.saturating_add(skill.instructions.len());
        if skill.instructions.trim().is_empty()
            || skill.instructions.len() > 32 * 1024
            || total_body_bytes > 256 * 1024
            || skill
                .instructions
                .chars()
                .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
            || skill.tools.len() > 64
        {
            return Err(invalid(plugin, "工作流正文、总字节数或工具引用超过预算"));
        }
        let mut references = Vec::with_capacity(skill.tools.len());
        let mut seen = HashSet::new();
        for local_name in &skill.tools {
            let Some((name, risk)) = declared.get(local_name) else {
                return Err(invalid(plugin, "工作流引用了未声明或其它作用域的工具"));
            };
            if !seen.insert(local_name) {
                return Err(invalid(plugin, "工作流重复引用工具"));
            }
            references.push(serde_json::json!({
                "local_name": local_name,
                "name": name,
                "risk_level": risk,
            }));
        }
        compiled.push(Arc::new(PluginSkillTool {
            name: format!("plugin.{plugin}.skill.{}", skill.id),
            description: format!("读取{}的工作流。{}", skill.title, skill.description),
            document: serde_json::json!({
                "plugin": plugin,
                "id": skill.id,
                "title": skill.title,
                "instructions": skill.instructions,
                "tools": references,
            }),
        }));
    }
    Ok(compiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct WriteTool;
    #[async_trait::async_trait]
    impl AgentTool for WriteTool {
        fn name(&self) -> &str {
            "save"
        }
        fn description(&self) -> &str {
            "Save an explicitly selected item"
        }
        fn risk_level(&self) -> ToolRiskLevel {
            ToolRiskLevel::LowRisk
        }
        async fn call(&self, _: AgentToolContext, input: Value) -> Result<Value> {
            Ok(input)
        }
    }

    fn context() -> AgentToolContext {
        AgentToolContext::new(
            AgentContext::from_runtime_event(
                1,
                &serde_json::json!({"post_type":"message", "user_id":42}),
            )
            .unwrap(),
        )
    }

    fn skill() -> PluginAgentSkill {
        PluginAgentSkill::new(
            "guide",
            "Item guide",
            "Read the item workflow",
            "Ask before saving.",
        )
        .with_tools(["save"])
    }

    #[tokio::test]
    async fn skill_reader_preserves_referenced_tool_risk_and_rejects_arguments() {
        let tools = scoped_tools("example", vec![Arc::new(WriteTool)], vec![skill()]).unwrap();
        assert_eq!(tools[0].risk_level(), ToolRiskLevel::LowRisk);
        let reader = &tools[1];
        assert_eq!(reader.name(), "plugin.example.skill.guide");
        assert_eq!(reader.risk_level(), ToolRiskLevel::ReadOnly);
        let document = reader.call(context(), serde_json::json!({})).await.unwrap();
        assert_eq!(document["instructions"], "Ask before saving.");
        assert_eq!(document["tools"][0]["name"], "plugin.example.save");
        assert_eq!(document["tools"][0]["risk_level"], "LowRisk");
        assert!(reader.call(context(), Value::Null).await.is_err());
        assert!(
            reader
                .call(context(), serde_json::json!({"run":"save"}))
                .await
                .is_err()
        );
    }

    #[test]
    fn skill_references_cannot_escape_plugin_scope_or_repeat() {
        for references in [vec!["plugin.other.save"], vec!["save", "save"]] {
            let declaration = PluginAgentSkill::new("guide", "Guide", "Read guide", "Instructions")
                .with_tools(references);
            assert!(scoped_tools("example", vec![Arc::new(WriteTool)], vec![declaration]).is_err());
        }
        assert!(
            scoped_tools("example", vec![Arc::new(WriteTool)], vec![skill(), skill()]).is_err()
        );
        let mut oversized = skill();
        oversized.instructions = "x".repeat(32 * 1024 + 1);
        assert!(scoped_tools("example", vec![Arc::new(WriteTool)], vec![oversized]).is_err());
    }

    #[test]
    fn workflow_and_business_tools_share_the_existing_plugin_cleanup_boundary() {
        let registry = ToolRegistry::new();
        for plugin in ["example", "other"] {
            for tool in scoped_tools(plugin, vec![Arc::new(WriteTool)], vec![skill()]).unwrap() {
                registry.register_arc(tool).unwrap();
            }
        }
        assert_eq!(registry.unregister_by_prefix("plugin.example."), 2);
        assert!(registry.get("plugin.example.skill.guide").is_none());
        assert!(registry.get("plugin.example.save").is_none());
        assert!(registry.get("plugin.other.skill.guide").is_some());
        assert!(registry.get("plugin.other.save").is_some());
    }
}
