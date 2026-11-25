//! 插件自有数据库表的后台管理访问策略声明。
//! 这些策略均由受信任的插件源码静态声明，绝不会从可被修改的运行期数据中加载。
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DatabaseFieldType {
    Text,
    Boolean,
    Integer,
    Json,
}

impl DatabaseFieldType {
    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Boolean => "boolean",
            Self::Integer => "integer",
            Self::Json => "json",
        }
    }
}

/// 凡在此列出的字段均允许读取与导出。若额外指定了 `edit_type`，则在表开启新增/修改权限时允许编辑。
/// 主键仅可在新建记录时指定，自增/自动生成的主键始终由数据库维护。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseColumnPolicy {
    pub name: String,
    #[serde(default)]
    pub edit_type: Option<DatabaseFieldType>,
    #[serde(default)]
    pub max_length: Option<u64>,
}

impl DatabaseColumnPolicy {
    pub fn read(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            edit_type: None,
            max_length: None,
        }
    }

    pub fn edit(name: impl Into<String>, kind: DatabaseFieldType) -> Self {
        Self {
            edit_type: Some(kind),
            ..Self::read(name)
        }
    }

    pub fn max_length(mut self, limit: u64) -> Self {
        self.max_length = Some(limit);
        self
    }
}

/// 未在列表中声明的字段（包括后续新增的数据库列）默认受保护不可访问。
/// 表权限默认为只读；若表缺少必填列或字段类型不兼容，相关写入操作会直接报错拒绝。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseTablePolicy {
    pub table: String,
    pub columns: Vec<DatabaseColumnPolicy>,
    #[serde(default)]
    pub create: bool,
    #[serde(default)]
    pub update: bool,
    #[serde(default)]
    pub delete: bool,
}

impl DatabaseTablePolicy {
    pub fn read(table: impl Into<String>, columns: Vec<DatabaseColumnPolicy>) -> Self {
        Self {
            table: table.into(),
            columns,
            create: false,
            update: false,
            delete: false,
        }
    }

    pub fn manage(mut self) -> Self {
        self.create = true;
        self.update = true;
        self.delete = true;
        self
    }

    pub fn validate(&self) -> Result<(), String> {
        let valid_name = |name: &str| {
            !name.trim().is_empty() && name.len() <= 128 && !name.chars().any(char::is_control)
        };
        if !valid_name(&self.table) || self.columns.is_empty() || self.columns.len() > 256 {
            return Err("数据库表声明缺少名称或字段，或超过声明范围限制".into());
        }
        let mut names = std::collections::BTreeSet::new();
        for column in &self.columns {
            if !valid_name(&column.name)
                || !names.insert(&column.name)
                || column
                    .max_length
                    .is_some_and(|limit| limit == 0 || limit > 65_536)
            {
                return Err("数据库字段声明包含无效名称、重复字段或长度限制".into());
            }
        }
        if self.update && !self.columns.iter().any(|column| column.edit_type.is_some()) {
            return Err("可编辑的表必须声明可编辑字段".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn declarations_default_to_read_only_and_reject_ambiguous_fields() {
        let column = DatabaseColumnPolicy::read("id");
        let mut table = DatabaseTablePolicy::read("sample", vec![column.clone()]);
        assert!(table.validate().is_ok() && !table.create && !table.update && !table.delete);
        table.columns.push(column);
        assert!(table.validate().is_err());
        table.columns.pop();
        assert!(table.manage().validate().is_err());
        assert!(
            DatabaseTablePolicy::read(
                "sample",
                vec![DatabaseColumnPolicy::edit("name", DatabaseFieldType::Text).max_length(100)]
            )
            .manage()
            .validate()
            .is_ok()
        );
    }
}
