use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// 按钮定义
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ButtonDef {
    pub label: String,
    pub data: String,
    #[serde(default)]
    pub visited_label: Option<String>,
}

/// 按钮模板（一组按钮行）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ButtonTemplate {
    pub id: String,
    pub name: String,
    pub buttons: Vec<Vec<ButtonDef>>,
}

/// Markdown 模板
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkdownTemplate {
    pub id: String,
    pub name: String,
    pub content: String,
    /// 模板参数名列表
    #[serde(default)]
    pub params: Vec<String>,
}

/// 模板管理器
pub struct TemplateManager {
    /// 按钮模板库: template_id -> ButtonTemplate
    pub button_templates: HashMap<String, ButtonTemplate>,
    /// Markdown 模板库: template_id -> MarkdownTemplate
    pub markdown_templates: HashMap<String, MarkdownTemplate>,
    /// 模板目录
    pub template_dir: PathBuf,
}

impl TemplateManager {
    pub fn new(template_dir: PathBuf) -> Self {
        Self {
            button_templates: HashMap::new(),
            markdown_templates: HashMap::new(),
            template_dir,
        }
    }

    /// 从目录加载所有模板
    pub fn load_from_dir(&mut self) -> Result<(), String> {
        let bt_dir = self.template_dir.join("buttons");
        let md_dir = self.template_dir.join("markdown");

        if bt_dir.exists()
            && let Ok(entries) = std::fs::read_dir(&bt_dir)
        {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                match self.load_button_template(&path) {
                    Ok(template) => {
                        info!(id = %template.id, "已加载按钮模板");
                        self.button_templates.insert(template.id.clone(), template);
                    }
                    Err(e) => warn!(path = %path.display(), "加载按钮模板失败: {}", e),
                }
            }
        }

        if md_dir.exists()
            && let Ok(entries) = std::fs::read_dir(&md_dir)
        {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                match self.load_markdown_template(&path) {
                    Ok(template) => {
                        info!(id = %template.id, "已加载 Markdown 模板");
                        self.markdown_templates
                            .insert(template.id.clone(), template);
                    }
                    Err(e) => warn!(path = %path.display(), "加载 Markdown 模板失败: {}", e),
                }
            }
        }

        info!(
            buttons = self.button_templates.len(),
            markdowns = self.markdown_templates.len(),
            "模板加载完成"
        );
        Ok(())
    }

    fn load_button_template(&self, path: &Path) -> Result<ButtonTemplate, String> {
        let content = std::fs::read_to_string(path).map_err(|e| format!("读取文件失败: {}", e))?;

        let value: serde_json::Value =
            serde_json::from_str(&content).map_err(|e| format!("JSON 解析失败: {}", e))?;

        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        let rows = value
            .get("rows")
            .and_then(|r| r.as_array())
            .map(|rows| {
                rows.iter()
                    .map(|row| {
                        row.get("buttons")
                            .and_then(|b| b.as_array())
                            .map(|buttons| {
                                buttons
                                    .iter()
                                    .map(|btn| {
                                        let label = btn
                                            .get("render_data")
                                            .and_then(|r| r.get("label"))
                                            .and_then(|l| l.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let data = btn
                                            .get("action")
                                            .and_then(|a| a.get("data"))
                                            .and_then(|d| d.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let visited_label = btn
                                            .get("render_data")
                                            .and_then(|r| r.get("visited_label"))
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.to_string());
                                        ButtonDef {
                                            label,
                                            data,
                                            visited_label,
                                        }
                                    })
                                    .collect()
                            })
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(ButtonTemplate {
            id: id.clone(),
            name: value
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or(&id)
                .to_string(),
            buttons: rows,
        })
    }

    fn load_markdown_template(&self, path: &Path) -> Result<MarkdownTemplate, String> {
        let content = std::fs::read_to_string(path).map_err(|e| format!("读取文件失败: {}", e))?;

        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        // 提取模板参数 `{{param_name}}`
        let re = regex::Regex::new(r"\{\{([^\n{}]+)\}\}").map_err(|e| e.to_string())?;

        let params: Vec<String> = re
            .captures_iter(&content)
            .map(|cap| cap[1].trim().to_string())
            .collect();

        Ok(MarkdownTemplate {
            id: id.clone(),
            name: id,
            content,
            params,
        })
    }

    /// 获取按钮模板
    pub fn get_button(&self, id: &str) -> Option<&ButtonTemplate> {
        self.button_templates.get(id)
    }

    /// 获取 Markdown 模板
    pub fn get_markdown(&self, id: &str) -> Option<&MarkdownTemplate> {
        self.markdown_templates.get(id)
    }

    /// 渲染 Markdown 模板
    pub fn render_markdown(
        &self,
        template_id: &str,
        params: &HashMap<String, String>,
    ) -> Option<String> {
        let template = self.markdown_templates.get(template_id)?;
        let mut result = template.content.clone();
        for (key, value) in params {
            result = result.replace(&format!("{{{{{}}}}}", key), value);
        }
        Some(result)
    }

    /// 列出所有按钮模板 ID
    pub fn list_buttons(&self) -> Vec<String> {
        self.button_templates.keys().cloned().collect()
    }

    /// 列出所有 Markdown 模板 ID
    pub fn list_markdowns(&self) -> Vec<String> {
        self.markdown_templates.keys().cloned().collect()
    }
}

impl Default for TemplateManager {
    fn default() -> Self {
        Self::new(PathBuf::from("templates"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_markdown() {
        let mut manager = TemplateManager::default();
        let template = MarkdownTemplate {
            id: "test".into(),
            name: "Test".into(),
            content: "Hello {{name}}!".into(),
            params: vec!["name".into()],
        };
        manager.markdown_templates.insert("test".into(), template);

        let mut params = HashMap::new();
        params.insert("name".into(), "World".into());
        let result = manager.render_markdown("test", &params);
        assert_eq!(result, Some("Hello World!".into()));
    }
}
