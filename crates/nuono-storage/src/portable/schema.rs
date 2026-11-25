use std::collections::BTreeMap;

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{invalid, quoted};
use crate::error::Result;

/// 数据库列在跨数据库可移植备份（SQLite <-> PostgreSQL）中的统一逻辑值类型。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicalType {
    Integer,
    Boolean,
    Text,
    Binary,
    Float,
}

/// 跨数据库可移植表结构中显式登记的单个列定义（保留稳定列名、逻辑类型与自增属性）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    pub name: String,
    pub kind: LogicalType,
    pub generated_id: bool,
    /// 有符号整数列的逻辑位宽（16 / 32 / 64）；SQLite 侧通过 CHECK 约束对齐 PostgreSQL 较窄的 `SMALLINT` / `INTEGER` 范围。
    pub integer_bits: Option<u8>,
}

/// 跨数据库可移植表结构中登记的单张表定义（为保证导入导出与逐行比对的顺序确定性，每张表必须声明主键）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    pub primary_key: Vec<String>,
}

/// 由配套版本代码提供的受信任双后端（PostgreSQL 与 SQLite）配对建表 SQL 声明。
///
/// 可移植迁移包中仅保留这些纯声明式的 `CREATE TABLE` / `CREATE INDEX` 语句，解析器会拒绝任意动态 SQL；
/// 插件扩展须提供完整的当前表结构声明，而非生命周期回调或增量修补脚本。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Migration {
    pub name: String,
    pub postgres_sql: String,
    pub sqlite_sql: String,
}

/// 经过双端一致性校验、可供离线跨数据库迁移工具（SQLite <-> PostgreSQL）直接使用的完整表结构清单。
#[derive(Clone, Debug)]
pub struct Schema {
    pub(crate) migrations: Vec<Migration>,
    pub(crate) tables: BTreeMap<String, Table>,
    pub(crate) fingerprint: String,
}

impl Schema {
    /// 从已通过基础校验的可移植数据库快照（`DatabaseImage`）中重建包含所有保留插件表的完整表结构定义。
    ///
    /// # Errors
    /// 若快照缺少部署身份清单、框架版本与当前程序不一致，或插件表结构声明不合法，则返回错误。
    pub fn from_image(image: &super::DatabaseImage) -> Result<Self> {
        let deployment = image
            .deployment
            .as_ref()
            .ok_or_else(|| invalid("缺少部署身份清单"))?;
        if deployment.framework_version != env!("CARGO_PKG_VERSION") {
            return Err(invalid("必须使用相同框架版本搬迁"));
        }
        let mut plugins = deployment.plugins.iter().collect::<Vec<_>>();
        plugins.sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));
        let mut seen = std::collections::BTreeSet::new();
        let mut extensions = Vec::new();
        for plugin in plugins {
            if !seen.insert(&plugin.plugin_id) {
                return Err(invalid("插件清单重复"));
            }
            if !super::manifest_tables(&plugin.manifest)?.is_empty() {
                extensions.push(Migration {
                    name: format!(
                        "{}:{}",
                        plugin.plugin_id,
                        plugin
                            .version
                            .ok_or_else(|| invalid("非空业务清单缺少版本"))?
                    ),
                    postgres_sql: plugin.manifest.postgres_schema.clone(),
                    sqlite_sql: plugin.manifest.sqlite_schema.clone(),
                });
            }
        }
        Self::framework_with_modules(&deployment.modules, extensions)
    }

    /// 基于编译期内置的框架核心建表 SQL 与显式登记的插件扩展建表 SQL 构建可移植表结构。
    ///
    /// # Errors
    /// 若包含不支持的字段类型、重复表名、`ALTER` 增量语句或双端列定义不一致，则返回错误（插件扩展必须提供当前版本的完整 `CREATE` 声明）。
    pub fn framework(extensions: Vec<Migration>) -> Result<Self> {
        Self::framework_with_modules(&[], extensions)
    }

    /// 基于编译期内置的框架核心表结构、指定的可选框架模块（如 `"ai"`）以及插件扩展建表 SQL 构建完整的可移植表结构。
    ///
    /// # Errors
    /// 若模块名未知、模块重复、包含不支持的字段类型、表名冲突或双端列定义不一致，则返回错误。
    pub fn framework_with_modules(modules: &[String], extensions: Vec<Migration>) -> Result<Self> {
        let mut migrations: Vec<_> = crate::db::framework_migration_sources()
            .into_iter()
            .map(|(name, postgres_sql, sqlite_sql)| Migration {
                name: name.into(),
                postgres_sql: postgres_sql.into(),
                sqlite_sql: sqlite_sql.into(),
            })
            .collect();
        let mut seen_modules = std::collections::BTreeSet::new();
        for module in modules {
            if !seen_modules.insert(module.as_str()) {
                return Err(invalid("重复的框架模块清单"));
            }
            match module.as_str() {
                "ai" => {
                    migrations.extend(crate::db::ai_module_migration_sources().into_iter().map(
                        |(name, postgres_sql, sqlite_sql)| Migration {
                            name: name.into(),
                            postgres_sql: postgres_sql.into(),
                            sqlite_sql: sqlite_sql.into(),
                        },
                    ));
                }
                _ => return Err(invalid(format!("未知框架模块 {module}"))),
            }
        }
        migrations.extend(extensions);
        Self::new(migrations)
    }

    /// 离线静态校验一组配对的 PostgreSQL 与 SQLite 完整建表 SQL（无需连接真实数据库）。
    ///
    /// # Errors
    /// 若任何表或列无法在 SQLite 与 PostgreSQL 之间无损映射，或双端表结构不一致，则返回错误。
    pub fn new(migrations: Vec<Migration>) -> Result<Self> {
        let mut tables = BTreeMap::new();
        for migration in &migrations {
            let pg = parse(&migration.postgres_sql, true)?;
            let sqlite = parse(&migration.sqlite_sql, false)?;
            if pg.keys().collect::<Vec<_>>() != sqlite.keys().collect::<Vec<_>>() {
                return Err(invalid(format!("{}：两端表名不一致", migration.name)));
            }
            for (name, table) in pg {
                let counterpart = &sqlite[&name];
                if table.primary_key != counterpart.primary_key
                    || table.columns.len() != counterpart.columns.len()
                {
                    return Err(invalid(format!("{name}：两端主键或列不一致")));
                }
                for (a, b) in table.columns.iter().zip(&counterpart.columns) {
                    let compatible = a.kind == b.kind
                        || matches!(
                            (a.kind, b.kind),
                            (LogicalType::Boolean, LogicalType::Integer)
                        );
                    if a.name != b.name || !compatible || a.generated_id != b.generated_id {
                        return Err(invalid(format!(
                            "{name}.{}：两端类型或自增语义不一致",
                            a.name
                        )));
                    }
                }
                if tables.insert(name.clone(), table).is_some() {
                    return Err(invalid(format!("重复表 {name}")));
                }
            }
        }
        if tables.is_empty() {
            return Err(invalid("空的结构清单"));
        }
        for migration in &migrations {
            validate_declarative_functions(&migration.postgres_sql, &tables)?;
            validate_declarative_functions(&migration.sqlite_sql, &tables)?;
        }
        let fingerprint = hex::encode(Sha256::digest(
            serde_json::to_vec(&migrations).map_err(|e| invalid(e.to_string()))?,
        ));
        Ok(Self {
            migrations,
            tables,
            fingerprint,
        })
    }

    /// 双后端建表 SQL 定义及其登记顺序的稳定 SHA-256 结构指纹。
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

fn validate_declarative_functions(sql: &str, _tables: &BTreeMap<String, Table>) -> Result<()> {
    let clean = strip_line_comments(sql);
    let mut code = String::new();
    let mut chars = clean.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\'' {
            while let Some(next) = chars.next() {
                if next == '\'' {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            code.push(' ');
        } else if ch != '"' && ch != '`' {
            code.push(ch);
        }
    }
    let code = code.to_ascii_lowercase();
    let calls = Regex::new(r"([a-z_][a-z0-9_]*)\s*\(").map_err(|e| invalid(e.to_string()))?;
    for call in calls.captures_iter(&code) {
        let name = &call[1];
        let offset = call.get(0).ok_or_else(|| invalid("函数声明无效"))?.start();
        let prefix = code[..offset].trim_end();
        let preceding = prefix.split_whitespace().last().unwrap_or("");
        if matches!(preceding, "table" | "references" | "on")
            || prefix.ends_with("if not exists")
            || matches!(
                name,
                "check"
                    | "in"
                    | "key"
                    | "unique"
                    | "or"
                    | "and"
                    | "default"
                    | "where"
                    | "identity"
                    | "varchar"
                    | "varying"
                    | "btree"
                    | "instr"
                    | "char"
                    | "length"
                    | "octet_length"
                    | "convert_to"
                    | "lower"
                    | "upper"
                    | "trim"
                    | "substr"
                    | "substring"
                    | "replace"
                    | "coalesce"
                    | "nullif"
                    | "abs"
                    | "round"
                    | "floor"
                    | "ceil"
                    | "ceiling"
                    | "min"
                    | "max"
                    | "json_valid"
                    | "json_type"
                    | "json_typeof"
                    | "jsonb_typeof"
                    | "json_array_length"
                    | "cast"
                    | "extract"
                    | "now"
                    | "clock_timestamp"
                    | "strftime"
                    | "unixepoch"
            )
        {
            continue;
        }
        return Err(invalid(format!(
            "完整清单不允许调用未登记的非便携函数 {name}"
        )));
    }
    Ok(())
}

pub(super) fn parse(sql: &str, pg: bool) -> Result<BTreeMap<String, Table>> {
    let cleaned = strip_line_comments(sql);
    let qualified = Regex::new(r#"(?i)\b(?:REFERENCES|ON|TABLE)\s+\"?[a-z][a-z0-9_]*\"?\s*\."#)
        .map_err(|e| invalid(e.to_string()))?;
    if qualified.is_match(&cleaned) {
        return Err(invalid(
            "完整声明必须使用不带 schema 的准确表名，不能绕过同一登记空间",
        ));
    }
    let table_re = Regex::new(
        r#"(?i)^CREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?\"?([a-z][a-z0-9_]*)\"?\s*\("#,
    )
    .map_err(|e| invalid(e.to_string()))?;
    let index_re =
        Regex::new(r"(?i)^CREATE\s+(UNIQUE\s+)?INDEX\s").map_err(|e| invalid(e.to_string()))?;
    let pk_re =
        Regex::new(r"(?i)PRIMARY\s+KEY\s*\(([^)]*)\)").map_err(|e| invalid(e.to_string()))?;
    let mut tables = BTreeMap::new();
    for statement in split_sql(&cleaned, ';')? {
        let statement = statement.trim();
        if statement.is_empty() {
            continue;
        }
        if index_re.is_match(statement) {
            continue;
        }
        let caps = table_re.captures(statement).ok_or_else(|| {
            invalid("便携结构仅接受完整 CREATE TABLE/INDEX，不执行数据修复或任意 SQL")
        })?;
        let name = caps[1].to_owned();
        quoted(&name)?;
        let begin = caps.get(0).ok_or_else(|| invalid("缺少表定义"))?.end();
        let end = statement
            .rfind(')')
            .ok_or_else(|| invalid("表定义括号不完整"))?;
        let body = &statement[begin..end];
        let mut columns = Vec::new();
        let mut primary_key = Vec::new();
        for field in split_sql(body, ',')? {
            let field = field.trim();
            let upper = field.to_ascii_uppercase();
            if [
                "CONSTRAINT ",
                "PRIMARY ",
                "FOREIGN ",
                "UNIQUE ",
                "UNIQUE(",
                "CHECK ",
                "CHECK(",
            ]
            .iter()
            .any(|prefix| upper.starts_with(prefix))
            {
                if let Some(pk) = pk_re.captures(field) {
                    primary_key = names(&pk[1])?;
                }
                continue;
            }
            let (name, definition) = field
                .split_once(char::is_whitespace)
                .ok_or_else(|| invalid("缺少列类型"))?;
            let name = name.trim_matches('"').to_owned();
            quoted(&name)?;
            let definition = definition.trim().to_ascii_uppercase();
            let kind = if pg
                && (definition.starts_with("BIGINT")
                    || definition.starts_with("INTEGER")
                    || definition.starts_with("SMALLINT"))
                || !pg && definition.starts_with("INTEGER")
            {
                LogicalType::Integer
            } else if pg && definition.starts_with("BOOLEAN") {
                LogicalType::Boolean
            } else if definition.starts_with("TIMESTAMPTZ")
                || definition.starts_with("TIMESTAMP WITH TIME ZONE")
            {
                return Err(invalid(
                    "时间必须使用 UTC 毫秒 BIGINT/INTEGER；不接受 timestamptz/TEXT 隐式转换",
                ));
            } else if definition.starts_with("TEXT") {
                LogicalType::Text
            } else if pg && definition.starts_with("BYTEA") || !pg && definition.starts_with("BLOB")
            {
                LogicalType::Binary
            } else if pg && definition.starts_with("DOUBLE PRECISION")
                || !pg && definition.starts_with("REAL")
            {
                LogicalType::Float
            } else {
                return Err(invalid(format!(
                    "{}.{name}：未支持的列类型",
                    caps[1].to_owned()
                )));
            };
            if definition.contains("PRIMARY KEY") {
                primary_key.push(name.clone());
            }
            let generated_id = if pg {
                if definition.contains("IDENTITY")
                    && (!definition.contains("GENERATED BY DEFAULT AS IDENTITY")
                        || definition
                            .split("IDENTITY")
                            .nth(1)
                            .is_some_and(|tail| tail.trim_start().starts_with('(')))
                {
                    return Err(invalid(
                        "便携自增必须使用无参数的 BIGINT GENERATED BY DEFAULT AS IDENTITY",
                    ));
                }
                definition.contains("GENERATED BY DEFAULT AS IDENTITY")
            } else {
                definition.contains("AUTOINCREMENT")
            };
            columns.push(Column {
                name,
                kind,
                generated_id,
                integer_bits: if kind == LogicalType::Integer {
                    Some(if pg && definition.starts_with("INTEGER") {
                        32
                    } else if pg && definition.starts_with("SMALLINT") {
                        16
                    } else {
                        64
                    })
                } else {
                    None
                },
            });
        }
        if primary_key.is_empty()
            || primary_key
                .iter()
                .any(|key| !columns.iter().any(|c| &c.name == key))
        {
            return Err(invalid(format!("{name}：需要声明有效主键")));
        }
        if tables
            .insert(
                name.clone(),
                Table {
                    name: name.clone(),
                    columns,
                    primary_key,
                },
            )
            .is_some()
        {
            return Err(invalid(format!("重复表 {name}")));
        }
    }
    Ok(tables)
}

pub(super) fn strip_line_comments(sql: &str) -> String {
    let mut output = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut quote = None;
    while let Some(character) = chars.next() {
        if let Some(delimiter) = quote {
            output.push(character);
            if character == delimiter {
                if chars.peek() == Some(&delimiter) {
                    output.push(chars.next().expect("peeked quote exists"));
                } else {
                    quote = None;
                }
            }
        } else if character == '-' && chars.peek() == Some(&'-') {
            chars.next();
            for next in chars.by_ref() {
                if next == '\n' {
                    output.push('\n');
                    break;
                }
            }
        } else {
            if matches!(character, '\'' | '"') {
                quote = Some(character);
            }
            output.push(character);
        }
    }
    output
}

fn names(value: &str) -> Result<Vec<String>> {
    value
        .split(',')
        .map(|name| {
            let name = name.trim().trim_matches('"');
            quoted(name)?;
            Ok(name.to_owned())
        })
        .collect()
}

pub(super) fn split_sql(input: &str, separator: char) -> Result<Vec<&str>> {
    let mut depth = 0_i32;
    let mut quote = None;
    let mut start = 0;
    let mut result = Vec::new();
    let mut chars = input.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                if chars.peek().is_some_and(|(_, next)| *next == q) {
                    chars.next();
                } else {
                    quote = None;
                }
            }
        } else {
            match c {
                '\'' | '"' => quote = Some(c),
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth < 0 {
                        return Err(invalid("SQL 括号不匹配"));
                    }
                }
                c if c == separator && depth == 0 => {
                    result.push(&input[start..index]);
                    start = index + c.len_utf8();
                }
                _ => {}
            }
        }
    }
    if depth != 0 || quote.is_some() {
        return Err(invalid("SQL 字符串或括号不完整"));
    }
    result.push(&input[start..]);
    Ok(result)
}

impl Schema {
    /// 按跨表/跨插件的外键拓扑依赖顺序排列所有 `CREATE TABLE` 语句，最后追加所有 `CREATE INDEX` 语句。
    pub(super) fn statements(&self, postgres: bool) -> Result<Vec<String>> {
        let table = Regex::new(
            r#"(?i)^CREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?\"?([a-z][a-z0-9_]*)\"?\s*\("#,
        )
        .map_err(|e| invalid(e.to_string()))?;
        let references = Regex::new(r#"(?i)REFERENCES\s+\"?([a-z][a-z0-9_]*)\"?\s*\("#)
            .map_err(|e| invalid(e.to_string()))?;
        let mut pending = BTreeMap::new();
        let mut indexes = Vec::new();
        for migration in &self.migrations {
            let script = strip_line_comments(if postgres {
                &migration.postgres_sql
            } else {
                &migration.sqlite_sql
            });
            for statement in split_sql(&script, ';')? {
                let statement = statement.trim();
                if statement.is_empty() {
                    continue;
                }
                if let Some(capture) = table.captures(statement) {
                    let name = capture[1].to_string();
                    let dependencies = references
                        .captures_iter(statement)
                        .map(|c| c[1].to_string())
                        .filter(|parent| *parent != name)
                        .collect::<std::collections::BTreeSet<_>>();
                    pending.insert(name, (statement.to_string(), dependencies));
                } else {
                    indexes.push(statement.to_string());
                }
            }
        }
        let mut created = std::collections::BTreeSet::new();
        let mut statements = Vec::new();
        while !pending.is_empty() {
            let ready = pending
                .iter()
                .filter(|(_, (_, refs))| refs.is_subset(&created))
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            if ready.is_empty() {
                return Err(invalid(format!(
                    "完整 CREATE 清单存在循环或未登记的外键依赖：{}",
                    pending.keys().cloned().collect::<Vec<_>>().join(",")
                )));
            }
            for name in ready {
                let (statement, _) = pending
                    .remove(&name)
                    .ok_or_else(|| invalid("结构依赖顺序变化"))?;
                created.insert(name);
                statements.push(statement);
            }
        }
        statements.extend(indexes);
        Ok(statements)
    }
}
