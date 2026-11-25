//! 数据库实时 DDL 结构校验：将当前数据库中的表、列、约束、索引及序列属性，与用声明的建表 SQL 全新生成的干净参考库结构逐项比对。

use std::collections::BTreeMap;

use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde_json::Value;

use super::{Schema, invalid, quoted};
use crate::error::Result;

/// 抓取当前数据库的完整 DDL 结构快照（仅包含表结构、约束、索引和序列定义，不包含表内行数据或自增序列当前水位值）。
pub(super) async fn snapshot<C: ConnectionTrait>(conn: &C) -> Result<BTreeMap<String, Value>> {
    match conn.get_database_backend() {
        DbBackend::Sqlite => sqlite_structure(conn).await,
        DbBackend::Postgres => postgres_structure(conn, "public").await,
        _ => Err(invalid("不支持的结构验证后端")),
    }
}

/// 校验数据库实时表结构是否与声明的 `Schema` 完全一致（必须在调用方持有的维护事务内执行）。
/// SQLite 使用独立内存库作为参考基准；PostgreSQL 则在当前事务的 `pg_temp` 临时命名空间中建参考表并在返回前全部清理，绝不改动正式业务表。
pub(crate) async fn verify_structure<C: ConnectionTrait>(conn: &C, schema: &Schema) -> Result<()> {
    match conn.get_database_backend() {
        DbBackend::Sqlite => {
            let reference = sea_orm::Database::connect("sqlite::memory:").await?;
            for sql in schema.statements(false)? {
                reference.execute_unprepared(&sql).await?;
            }
            let expected = sqlite_structure(&reference).await?;
            let actual = sqlite_structure(conn).await?;
            reference.close().await?;
            compare(&actual, &expected)
        }
        DbBackend::Postgres => {
            let actual = postgres_structure(conn, "public").await?;
            conn.execute_unprepared("SET LOCAL search_path TO pg_temp,pg_catalog")
                .await?;
            let table_pattern =
                regex::Regex::new(r"(?i)^CREATE\s+TABLE\s+").map_err(|e| invalid(e.to_string()))?;
            for statement in schema.statements(true)? {
                let sql = if table_pattern.is_match(&statement) {
                    format!(
                        "{} ON COMMIT DROP",
                        table_pattern.replace(&statement, "CREATE TEMP TABLE ")
                    )
                } else {
                    statement
                };
                conn.execute_unprepared(&sql).await?;
            }
            let row = conn
                .query_one_raw(Statement::from_string(
                    DbBackend::Postgres,
                    "SELECT nspname FROM pg_namespace WHERE oid=pg_my_temp_schema()",
                ))
                .await?
                .ok_or_else(|| invalid("无法取得参考结构 namespace"))?;
            let namespace = row.try_get::<String>("", "nspname")?;
            let expected = postgres_structure(conn, &namespace).await?;
            // 使用单条 DROP TABLE 批量删除所有临时参考表，避免表间外键引用顺序阻碍清理。
            let names = schema
                .tables
                .keys()
                .map(|name| Ok(format!("{}.{}", quoted(&namespace)?, quoted(name)?)))
                .collect::<Result<Vec<_>>>()?
                .join(",");
            conn.execute_unprepared(&format!("DROP TABLE {names}"))
                .await?;
            conn.execute_unprepared("SET LOCAL search_path TO public,pg_catalog")
                .await?;
            compare(&actual, &expected)
        }
        _ => Err(invalid("不支持的结构验证后端")),
    }
}

fn compare(actual: &BTreeMap<String, Value>, expected: &BTreeMap<String, Value>) -> Result<()> {
    let differing = actual
        .keys()
        .chain(expected.keys())
        .filter(|name| actual.get(*name) != expected.get(*name))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    if !differing.is_empty() {
        return Err(invalid(format!(
            "实际结构与完整声明不同：{}；包括列、主外键、CHECK、UNIQUE、默认值、索引和序列属性",
            differing
                .into_iter()
                .map(|name| format!(
                    "{name} [{}]",
                    difference_paths(actual.get(&name), expected.get(&name), "").join(",")
                ))
                .collect::<Vec<_>>()
                .join("; ")
        )));
    }
    Ok(())
}

async fn sqlite_structure<C: ConnectionTrait>(conn: &C) -> Result<BTreeMap<String, Value>> {
    let rows=conn.query_all_raw(Statement::from_string(DbBackend::Sqlite,
        "SELECT type,name,tbl_name,sql FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*' ORDER BY type,name")).await?;
    let mut result = BTreeMap::new();
    for row in rows {
        let kind = row.try_get::<String>("", "type")?;
        let name = row.try_get::<String>("", "name")?;
        let table = row.try_get::<String>("", "tbl_name")?;
        let sql = row.try_get::<Option<String>>("", "sql")?;
        result.insert(format!("{kind}:{name}"),serde_json::json!({"table":table,"sql":sql.map(|v|normalize_sql(&v,None)).transpose()?}));
    }
    Ok(result)
}

async fn postgres_structure<C: ConnectionTrait>(
    conn: &C,
    namespace: &str,
) -> Result<BTreeMap<String, Value>> {
    let sql = r#"
        SELECT c.relname AS name,json_build_object(
            'kind',c.relkind,'persistence',CASE WHEN n.nspname=$1 AND n.nspname<>'public' THEN 'p' ELSE c.relpersistence END,
            'options',c.reloptions,'row_security',c.relrowsecurity,'force_row_security',c.relforcerowsecurity,
            'columns',(SELECT json_agg(json_build_object(
                'name',a.attname,'type',format_type(a.atttypid,a.atttypmod),'not_null',a.attnotnull,
                'identity',a.attidentity,'generated',a.attgenerated,'default',pg_get_expr(d.adbin,d.adrelid),
                'collation',CASE WHEN a.attcollation=0 THEN NULL ELSE a.attcollation::regcollation::text END
            ) ORDER BY a.attnum) FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum
             WHERE a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped),
            'constraints',(SELECT json_agg(json_build_object(
                'name',k.conname,'type',k.contype,'definition',pg_get_constraintdef(k.oid,true),
                'validated',k.convalidated,'deferrable',k.condeferrable,'deferred',k.condeferred
            ) ORDER BY k.conname) FROM pg_constraint k WHERE k.conrelid=c.oid),
            'indexes',(SELECT json_agg(json_build_object(
                'name',idx.relname,'definition',pg_get_indexdef(i.indexrelid),'valid',i.indisvalid,
                'ready',i.indisready,'unique',i.indisunique,'primary',i.indisprimary
            ) ORDER BY idx.relname) FROM pg_index i JOIN pg_class idx ON idx.oid=i.indexrelid WHERE i.indrelid=c.oid),
            'sequences',(SELECT json_agg(json_build_object(
                'column',a.attname,'start',s.seqstart,'increment',s.seqincrement,'min',s.seqmin,'max',s.seqmax,'cache',s.seqcache,'cycle',s.seqcycle
            ) ORDER BY a.attname) FROM pg_attribute a JOIN pg_sequence s ON s.seqrelid=pg_get_serial_sequence(format('%I.%I',n.nspname,c.relname),a.attname)::regclass
             WHERE a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped AND a.attidentity<>'')
        )::text AS structure
        FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
        WHERE n.nspname=$1 AND c.relkind IN ('r','p','v','m','f') ORDER BY c.relname
    "#;
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            [namespace.into()],
        ))
        .await?;
    let mut result = BTreeMap::new();
    for row in rows {
        let mut value: Value = serde_json::from_str(&row.try_get::<String>("", "structure")?)?;
        normalize_definitions(&mut value, namespace)?;
        result.insert(row.try_get("", "name")?, value);
    }
    Ok(result)
}

fn normalize_definitions(value: &mut Value, namespace: &str) -> Result<()> {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if matches!(key.as_str(), "definition" | "default") {
                    if let Value::String(sql) = value {
                        *sql = normalize_sql(sql, Some(namespace))?;
                    }
                } else {
                    normalize_definitions(value, namespace)?;
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                normalize_definitions(value, namespace)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// 对 SQL 表达式做词法归一化：原样保留单引号字符串字面量，仅剥离标识符上的 schema 前缀（如 `public.` 或 `pg_temp_N.`），
/// 从而确保默认值里诸如 `'public.x'` 的字符串字面量若被篡改仍能被检测出来。
fn normalize_sql(sql: &str, namespace: Option<&str>) -> Result<String> {
    let clean = super::schema::strip_line_comments(sql);
    let mut tokens = Vec::new();
    let mut chars = clean.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_whitespace() || ch == ';' {
            continue;
        }
        if ch == '\'' || ch == '"' || ch == '`' {
            let mut token = String::new();
            let mut closed = false;
            while let Some(next) = chars.next() {
                if next == ch {
                    if chars.peek() == Some(&ch) {
                        token.push(next);
                        token.push(chars.next().ok_or_else(|| invalid("非法引号"))?);
                    } else {
                        closed = true;
                        break;
                    }
                } else {
                    token.push(next);
                }
            }
            if !closed {
                return Err(invalid("结构 SQL 引号不完整"));
            }
            tokens.push(if ch == '\'' {
                format!("'{token}'")
            } else {
                token.to_ascii_lowercase()
            });
        } else if ch.is_ascii_alphanumeric() || ch == '_' {
            let mut token = ch.to_string();
            while chars
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
            {
                token.push(chars.next().ok_or_else(|| invalid("非法标识"))?);
            }
            tokens.push(token.to_ascii_lowercase());
        } else {
            tokens.push(ch.to_string());
        }
    }
    let mut canonical = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if i + 2 < tokens.len() && tokens[i..i + 3] == ["if", "not", "exists"] {
            i += 3;
            continue;
        }
        if namespace
            .is_some_and(|n| tokens[i] == n || n.starts_with("pg_temp_") && tokens[i] == "pg_temp")
            && tokens.get(i + 1).is_some_and(|t| t == ".")
        {
            i += 2;
            continue;
        }
        canonical.push(tokens[i].clone());
        i += 1;
    }
    Ok(canonical.join(" "))
}

fn difference_paths(actual: Option<&Value>, expected: Option<&Value>, prefix: &str) -> Vec<String> {
    if actual == expected {
        return Vec::new();
    }
    match (actual, expected) {
        (Some(Value::Object(a)), Some(Value::Object(b))) => a
            .keys()
            .chain(b.keys())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .flat_map(|key| difference_paths(a.get(key), b.get(key), &format!("{prefix}.{key}")))
            .take(20)
            .collect(),
        (Some(Value::Array(a)), Some(Value::Array(b))) if a.len() == b.len() => (0..a.len())
            .flat_map(|i| difference_paths(a.get(i), b.get(i), &format!("{prefix}[{i}]")))
            .take(20)
            .collect(),
        _ => vec![prefix.into()],
    }
}
