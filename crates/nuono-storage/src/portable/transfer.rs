use std::collections::{BTreeMap, BTreeSet};

use base64::{Engine, engine::general_purpose::STANDARD};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, QueryResult,
    SqliteTransactionMode, Statement, TransactionOptions, TransactionTrait, Value,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Column, LogicalType, MAX_CELL_BYTES, Schema, Table, invalid, quoted};
use crate::error::Result;

const FORMAT: u32 = 1;
const PAGE: usize = 256;
const MAX_IMAGE_BYTES: usize = 128 * 1024 * 1024;

/// 跨数据库可移植快照中的强类型单元格值（使用显式类型标签与字符串编码，避免 JavaScript 64 位整数精度丢失以及 NULL 与空字符串混淆）。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Cell {
    Null,
    Integer(String),
    Boolean(bool),
    Text(String),
    Binary(String),
    Float(String),
}

/// 自增序列的历史已分配上界水位（high-water mark，取表内最大 ID 与底层序列当前值的较大者）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sequence {
    pub column: String,
    pub high_water: String,
}

/// 单张已登记表的数据快照（包含表结构定义、按规范字节排序的行数据、行数据 SHA-256 校验和及自增序列水位）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableImage {
    pub definition: Table,
    pub rows: Vec<Vec<Cell>>,
    pub sha256: String,
    pub sequences: Vec<Sequence>,
}

/// 跨数据库可移植的完整数据库逻辑快照（受总大小上限约束，附带已校验的部署身份与插件存储归属元数据）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseImage {
    pub format: u32,
    pub schema_fingerprint: String,
    pub deployment: Option<DeploymentImage>,
    pub tables: Vec<TableImage>,
}

/// 随数据库快照一并导出的部署环境身份、已启用可选模块、全部已安装/保留数据的插件清单及外置资产声明。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentImage {
    pub deployment_id: String,
    pub framework_version: String,
    pub source_backend: String,
    #[serde(default)]
    pub modules: Vec<String>,
    pub plugins: Vec<super::RegisteredPlugin>,
    pub assets: Vec<nuono_app::plugin_manifest::PersistentAsset>,
}

/// 在一致性只读事务快照内导出所有已登记表的完整数据与序列水位（绝不修改源库中的任何记录）。
///
/// # Errors
/// 若源库中存在未登记的表/视图/触发器/函数、实际表结构与声明发生漂移、单格数据超限或含不可移植的值（如 NUL 字符或非有限浮点数），则拒绝导出。
/// 调用方在调用前必须先停止所有应用层及外部的文件与数据库写入。
pub async fn export(connection: &DatabaseConnection, schema: &Schema) -> Result<DatabaseImage> {
    let tx = transaction(connection).await?;
    inventory(&tx, schema, false).await?;
    if tx.get_database_backend() == DbBackend::Postgres {
        let names = schema
            .tables
            .keys()
            .map(|name| quoted(name))
            .collect::<Result<Vec<_>>>()?
            .join(",");
        tx.execute_unprepared(&format!("LOCK TABLE {names} IN SHARE MODE"))
            .await?;
    }
    let image = snapshot(&tx, schema).await?;
    tx.rollback().await?;
    Ok(image)
}

/// 将已校验的跨数据库可移植快照（`DatabaseImage`）原子导入到一个完全空白、隔离的目标数据库中。
///
/// # Errors
/// 任何字段值、校验和、表结构、主外键约束或自增序列不匹配，都会立即回滚整个目标库事务。
/// 导入过程纯属数据搬迁，不会触发任何插件生命周期、默认种子账号创建或支付回调。
pub async fn import(
    connection: &DatabaseConnection,
    schema: &Schema,
    image: &DatabaseImage,
) -> Result<()> {
    validate_image(schema, image)?;
    let tx = transaction(connection).await?;
    inventory(&tx, schema, true).await?;
    for sql in schema.statements(tx.get_database_backend() == DbBackend::Postgres)? {
        tx.execute_unprepared(&sql).await?;
    }
    let foreign_keys = defer_foreign_keys(&tx).await?;
    for table in &image.tables {
        let name = quoted(&table.definition.name)?;
        let columns = table
            .definition
            .columns
            .iter()
            .map(|c| quoted(&c.name))
            .collect::<Result<Vec<_>>>()?
            .join(",");
        let placeholders = (1..=table.definition.columns.len())
            .map(|i| format!("${i}"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("INSERT INTO {name} ({columns}) VALUES ({placeholders})");
        for row in &table.rows {
            let values = row
                .iter()
                .zip(&table.definition.columns)
                .map(|(value, column)| sql_value(value, column, tx.get_database_backend()))
                .collect::<Result<Vec<_>>>()?;
            tx.execute_raw(Statement::from_sql_and_values(
                tx.get_database_backend(),
                &sql,
                values,
            ))
            .await?;
        }
        restore_sequences(&tx, table).await?;
    }
    check_foreign_keys(&tx, &foreign_keys).await?;
    verify_on(&tx, schema, image).await?;
    tx.commit().await?;
    Ok(())
}

/// 在向目标环境暂存任何部署资产文件之前，预先校验数据库快照格式以及目标数据库是否完全为空。
///
/// # Errors
/// 若快照与当前表结构定义不匹配，或目标数据库中已存在任何表、视图、序列等对象，则报错拒绝。
pub async fn preflight_import(
    connection: &DatabaseConnection,
    schema: &Schema,
    image: &DatabaseImage,
) -> Result<()> {
    validate_image(schema, image)?;
    inventory(connection, schema, true).await
}

/// 将目标数据库中的每一张表、每一行数据、约束结构及自增序列水位与离线快照（`DatabaseImage`）逐一比对。
///
/// # Errors
/// 若发现任何不一致直接返回错误；本函数只做严格校验，绝不自动修补或静默忽略差异记录。
pub async fn verify(
    connection: &DatabaseConnection,
    schema: &Schema,
    image: &DatabaseImage,
) -> Result<()> {
    validate_image(schema, image)?;
    let tx = transaction(connection).await?;
    verify_on(&tx, schema, image).await?;
    tx.rollback().await?;
    Ok(())
}

/// 在维护恢复期间校验数据库是否除了 `nuono_deployment.maintenance_operation` 被置为当前维护操作 ID 之外，其余所有业务表和记录均与离线快照完全一致。
///
/// # Errors
/// 若任何业务数据发生变化，或 `maintenance_operation` 与预期维护操作 ID 不符，则校验失败。
pub async fn verify_maintenance(
    connection: &DatabaseConnection,
    schema: &Schema,
    image: &DatabaseImage,
    operation: &str,
) -> Result<()> {
    let mut expected = image.clone();
    let table = expected
        .tables
        .iter_mut()
        .find(|t| t.definition.name == "nuono_deployment")
        .ok_or_else(|| invalid("包缺少部署状态"))?;
    let column = table
        .definition
        .columns
        .iter()
        .position(|c| c.name == "maintenance_operation")
        .ok_or_else(|| invalid("包缺少维护控制列"))?;
    if table.rows.len() != 1 {
        return Err(invalid("部署身份必须唯一"));
    }
    table.rows[0][column] = Cell::Text(operation.into());
    table.sha256 = digest(&table.rows)?;
    verify(connection, schema, &expected).await
}

/// 在持有专用数据库维护锁期间，以 CAS（Compare-And-Set）方式原子设置或清除 `nuono_deployment.maintenance_operation` 持久化维护屏障。
///
/// # Errors
/// 若数据库中当前记录的维护操作标识与 `expected` 不一致，则报错拒绝，绝不覆盖或误清其他未完成的维护操作。
pub async fn set_maintenance(
    connection: &DatabaseConnection,
    expected: Option<&str>,
    next: Option<&str>,
) -> Result<()> {
    let result=connection.execute_raw(Statement::from_sql_and_values(connection.get_database_backend(),
        "UPDATE nuono_deployment SET maintenance_operation=$1 WHERE id=1 AND (maintenance_operation=$2 OR (maintenance_operation IS NULL AND $2 IS NULL))",
        [next.map(str::to_owned).into(),expected.map(str::to_owned).into()])).await?;
    if result.rows_affected() != 1 {
        return Err(invalid("数据库维护标识与当前操作不一致"));
    }
    Ok(())
}

async fn transaction(connection: &DatabaseConnection) -> Result<DatabaseTransaction> {
    Ok(match connection.get_database_backend() {
        DbBackend::Sqlite => {
            connection
                .begin_with_options(TransactionOptions {
                    isolation_level: None,
                    access_mode: None,
                    sqlite_transaction_mode: Some(SqliteTransactionMode::Immediate),
                })
                .await?
        }
        DbBackend::Postgres => connection.begin().await?,
        _ => return Err(invalid("只支持 SQLite 与 PostgreSQL")),
    })
}

pub(super) async fn inventory<C: ConnectionTrait>(
    conn: &C,
    schema: &Schema,
    empty: bool,
) -> Result<()> {
    if conn.get_database_backend() == DbBackend::Postgres {
        check_postgres_scope(conn).await?;
    } else if conn.get_database_backend() == DbBackend::Sqlite {
        let unsupported = conn
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT type || ':' || name AS object FROM sqlite_schema \
                 WHERE type IN ('view','trigger') ORDER BY type,name",
            ))
            .await?
            .into_iter()
            .map(|row| row.try_get::<String>("", "object"))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if !unsupported.is_empty() {
            return Err(invalid(format!(
                "含未支持的 SQLite 对象：{}",
                unsupported.join(", ")
            )));
        }
    }
    let sql = match conn.get_database_backend() {
        DbBackend::Sqlite => {
            "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*' ORDER BY name"
        }
        DbBackend::Postgres => {
            "SELECT c.relname AS name FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relkind IN ('r','p','v','m','f') UNION ALL SELECT t.tgname AS name FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND NOT t.tgisinternal ORDER BY name"
        }
        _ => return Err(invalid("不支持的后端")),
    };
    let names = conn
        .query_all_raw(Statement::from_string(conn.get_database_backend(), sql))
        .await?
        .into_iter()
        .map(|r| r.try_get::<String>("", "name"))
        .collect::<std::result::Result<BTreeSet<_>, _>>()?;
    if empty {
        if !names.is_empty() {
            return Err(invalid("目标数据库必须为空，拒绝覆盖已有表或视图"));
        }
    } else {
        let expected: BTreeSet<_> = schema.tables.keys().cloned().collect();
        if names != expected {
            return Err(invalid(format!(
                "结构登记不完整；额外对象 {:?}；缺少表 {:?}",
                names.difference(&expected).collect::<Vec<_>>(),
                expected.difference(&names).collect::<Vec<_>>()
            )));
        }
    }
    if conn.get_database_backend() == DbBackend::Postgres {
        let actual=conn.query_all_raw(Statement::from_string(DbBackend::Postgres,"SELECT c.relname AS name FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relkind='S' ORDER BY c.relname")).await?.into_iter().map(|r|r.try_get::<String>("","name")).collect::<std::result::Result<BTreeSet<_>,_>>()?;
        let mut expected = BTreeSet::new();
        if !empty {
            for table in schema.tables.values() {
                for column in table.columns.iter().filter(|c| c.generated_id) {
                    let name = sequence_name(conn, table, &column.name).await?;
                    let leaf = name
                        .rsplit('.')
                        .next()
                        .ok_or_else(|| invalid("无效序列名"))?
                        .trim_matches('"')
                        .to_owned();
                    expected.insert(leaf);
                }
            }
        }
        if actual != expected {
            return Err(invalid("目标非空或含未登记的独立序列"));
        }
        let routines=conn.query_one_raw(Statement::from_string(DbBackend::Postgres,"SELECT COUNT(*) AS n FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='public'")).await?.ok_or_else(||invalid("无法读取例程清单"))?.try_get::<i64>("","n")?;
        if routines != 0 {
            return Err(invalid("公开 schema 含未支持的函数或存储过程"));
        }
    }
    Ok(())
}

/// 扫描 PostgreSQL 系统目录，拒绝任何无法仅通过 `public` 下已登记普通表完整重建的数据库对象（如其他用户 schema、视图、自定义函数、触发器、自定义类型或行级安全策略）。
/// 直接查询 `pg_catalog` 而非依赖当前连接的 `search_path`，确保对当前会话不可见的隐藏对象也无处遁形。
async fn check_postgres_scope<C: ConnectionTrait>(conn: &C) -> Result<()> {
    let namespaces = conn
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT nspname AS name FROM pg_namespace \
             WHERE nspname NOT IN ('public','pg_catalog','information_schema','pg_toast') \
             AND nspname !~ '^pg_(temp|toast_temp)_[0-9]+$' ORDER BY nspname",
        ))
        .await?
        .into_iter()
        .map(|row| row.try_get::<String>("", "name"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if !namespaces.is_empty() {
        return Err(invalid(format!(
            "当前仅支持 public；拒绝遗漏其他用户 schema：{}",
            namespaces.join(", ")
        )));
    }

    // PostgreSQL 执行 CREATE TABLE 时会自动生成与表同名的复合行类型及其数组类型（随建表自动重建，无需拦截）；
    // 但用户自定义的 domain、enum 或独立 composite 类型无法通过建表语句重建，必须排查拦截。
    let sql = r#"
        SELECT object FROM (
            SELECT 'relation public.' || c.relname AS object
            FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
            WHERE n.nspname='public' AND c.relkind NOT IN ('r','i','S')
            UNION ALL
            SELECT 'routine ' || p.oid::regprocedure::text
            FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace
            WHERE n.nspname='public'
            UNION ALL
            SELECT 'type public.' || t.typname
            FROM pg_type t JOIN pg_namespace n ON n.oid=t.typnamespace
            LEFT JOIN pg_class c ON c.oid=t.typrelid
            LEFT JOIN pg_type element ON element.oid=t.typelem
            LEFT JOIN pg_class element_relation ON element_relation.oid=element.typrelid
            WHERE n.nspname='public'
            AND NOT (t.typtype='c' AND COALESCE(c.relkind='r',false))
            AND NOT (t.typcategory='A' AND element.typarray=t.oid
                     AND element.typtype='c' AND COALESCE(element_relation.relkind='r',false))
            UNION ALL
            SELECT 'collation public.' || x.collname
            FROM pg_collation x JOIN pg_namespace n ON n.oid=x.collnamespace
            WHERE n.nspname='public'
            UNION ALL
            SELECT 'conversion public.' || x.conname
            FROM pg_conversion x JOIN pg_namespace n ON n.oid=x.connamespace
            WHERE n.nspname='public'
            UNION ALL
            SELECT 'operator public.' || x.oprname
            FROM pg_operator x JOIN pg_namespace n ON n.oid=x.oprnamespace
            WHERE n.nspname='public'
            UNION ALL
            SELECT 'operator class public.' || x.opcname
            FROM pg_opclass x JOIN pg_namespace n ON n.oid=x.opcnamespace
            WHERE n.nspname='public'
            UNION ALL
            SELECT 'operator family public.' || x.opfname
            FROM pg_opfamily x JOIN pg_namespace n ON n.oid=x.opfnamespace
            WHERE n.nspname='public'
            UNION ALL
            SELECT 'text search configuration public.' || x.cfgname
            FROM pg_ts_config x JOIN pg_namespace n ON n.oid=x.cfgnamespace
            WHERE n.nspname='public'
            UNION ALL
            SELECT 'text search dictionary public.' || x.dictname
            FROM pg_ts_dict x JOIN pg_namespace n ON n.oid=x.dictnamespace
            WHERE n.nspname='public'
            UNION ALL
            SELECT 'text search parser public.' || x.prsname
            FROM pg_ts_parser x JOIN pg_namespace n ON n.oid=x.prsnamespace
            WHERE n.nspname='public'
            UNION ALL
            SELECT 'text search template public.' || x.tmplname
            FROM pg_ts_template x JOIN pg_namespace n ON n.oid=x.tmplnamespace
            WHERE n.nspname='public'
            UNION ALL
            SELECT 'extended statistics public.' || x.stxname
            FROM pg_statistic_ext x JOIN pg_namespace n ON n.oid=x.stxnamespace
            WHERE n.nspname='public'
            UNION ALL
            SELECT 'rule public.' || c.relname || '.' || x.rulename
            FROM pg_rewrite x JOIN pg_class c ON c.oid=x.ev_class
            JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public'
            UNION ALL
            SELECT 'trigger public.' || c.relname || '.' || x.tgname
            FROM pg_trigger x JOIN pg_class c ON c.oid=x.tgrelid
            JOIN pg_namespace n ON n.oid=c.relnamespace
            WHERE n.nspname='public' AND NOT x.tgisinternal
            UNION ALL
            SELECT 'row security policy public.' || c.relname || '.' || x.polname
            FROM pg_policy x JOIN pg_class c ON c.oid=x.polrelid
            JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public'
            UNION ALL
            SELECT 'row security public.' || c.relname
            FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
            WHERE n.nspname='public' AND (c.relrowsecurity OR c.relforcerowsecurity)
            UNION ALL
            SELECT 'event trigger ' || evtname FROM pg_event_trigger
            UNION ALL
            SELECT 'large object ' || oid::text FROM pg_largeobject_metadata
            UNION ALL
            SELECT 'extension ' || extname FROM pg_extension WHERE extname<>'plpgsql'
            UNION ALL
            SELECT 'foreign server ' || srvname FROM pg_foreign_server
            UNION ALL
            SELECT 'foreign data wrapper ' || fdwname FROM pg_foreign_data_wrapper
        ) unsupported ORDER BY object LIMIT 100
    "#;
    let unsupported = conn
        .query_all_raw(Statement::from_string(DbBackend::Postgres, sql))
        .await?
        .into_iter()
        .map(|row| row.try_get::<String>("", "object"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if !unsupported.is_empty() {
        return Err(invalid(format!(
            "含未支持的 PostgreSQL 对象（最多列出 100 项）：{}",
            unsupported.join(", ")
        )));
    }
    Ok(())
}

async fn snapshot<C: ConnectionTrait>(conn: &C, schema: &Schema) -> Result<DatabaseImage> {
    inventory(conn, schema, false).await?;
    super::structure::verify_structure(conn, schema).await?;
    validate_source_foreign_keys(conn).await?;
    let mut tables = Vec::new();
    let mut bytes = 0_usize;
    for table in schema.tables.values() {
        check_columns(conn, table).await?;
        let projection = table
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| projection(c, i, conn.get_database_backend()))
            .collect::<Result<Vec<_>>>()?
            .join(",");
        let order = table
            .primary_key
            .iter()
            .map(|key| quoted(key))
            .collect::<Result<Vec<_>>>()?
            .join(",");
        let mut offset = 0_usize;
        let mut rows = Vec::new();
        loop {
            let sql = format!(
                "SELECT {projection} FROM {} ORDER BY {order} LIMIT {PAGE} OFFSET {offset}",
                quoted(&table.name)?
            );
            let page = conn
                .query_all_raw(Statement::from_string(conn.get_database_backend(), sql))
                .await?;
            let count = page.len();
            for raw in page {
                let row = table
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| cell(&raw, c, i, conn.get_database_backend()))
                    .collect::<Result<Vec<_>>>()?;
                bytes = bytes
                    .checked_add(
                        serde_json::to_vec(&row)
                            .map_err(|e| invalid(e.to_string()))?
                            .len(),
                    )
                    .ok_or_else(|| invalid("数据包过大"))?;
                if bytes > MAX_IMAGE_BYTES {
                    return Err(invalid(
                        "逻辑行数据超过 128 MiB；当前离线工具不截断导出，请分配支持流式搬迁的版本",
                    ));
                }
                rows.push(row);
            }
            offset += count;
            if count < PAGE {
                break;
            }
        }
        // 不同数据库引擎的默认字符排序规则（collation）不同；因此在 Rust 内存中对导出的强类型行按统一规范顺序重排，确保两端生成的快照与校验和完全一致。
        rows.sort();
        let sha256 = digest(&rows)?;
        tables.push(TableImage {
            definition: table.clone(),
            rows,
            sha256,
            sequences: sequences(conn, table).await?,
        });
    }
    Ok(DatabaseImage {
        format: FORMAT,
        schema_fingerprint: schema.fingerprint.clone(),
        deployment: if schema.tables.contains_key("nuono_deployment") {
            let row = conn
                .query_one_raw(Statement::from_string(
                    conn.get_database_backend(),
                    "SELECT deployment_id,assets_json FROM nuono_deployment WHERE id=1",
                ))
                .await?
                .ok_or_else(|| invalid("部署身份未初始化"))?;
            let modules = conn
                .query_all_raw(Statement::from_string(
                    conn.get_database_backend(),
                    "SELECT DISTINCT module FROM nuono_schema_migrations WHERE module <> 'core' ORDER BY module",
                ))
                .await?
                .into_iter()
                .map(|r| r.try_get::<String>("", "module"))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Some(DeploymentImage {
                deployment_id: row.try_get("", "deployment_id")?,
                framework_version: env!("CARGO_PKG_VERSION").into(),
                source_backend: match conn.get_database_backend() {
                    DbBackend::Postgres => "postgres",
                    DbBackend::Sqlite => "sqlite",
                    _ => return Err(invalid("不支持的源后端")),
                }
                .into(),
                modules,
                plugins: super::registered_plugins(conn).await?,
                assets: super::parse_document(
                    row.try_get::<String>("", "assets_json")?.as_bytes(),
                )?,
            })
        } else {
            None
        },
        tables,
    })
}

async fn check_columns<C: ConnectionTrait>(conn: &C, table: &Table) -> Result<()> {
    let sql = match conn.get_database_backend() {
        DbBackend::Sqlite => format!("PRAGMA table_info({})", quoted(&table.name)?),
        DbBackend::Postgres => format!(
            "SELECT column_name AS name FROM information_schema.columns WHERE table_schema='public' AND table_name='{}' ORDER BY ordinal_position",
            table.name
        ),
        _ => return Err(invalid("不支持的后端")),
    };
    let actual = conn
        .query_all_raw(Statement::from_string(conn.get_database_backend(), sql))
        .await?
        .into_iter()
        .map(|r| r.try_get::<String>("", "name"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if actual
        != table
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect::<Vec<_>>()
    {
        return Err(invalid(format!("{}：实际列与登记不一致", table.name)));
    }
    for column in &table.columns {
        let column_name = quoted(&column.name)?;
        let length = if conn.get_database_backend() == DbBackend::Postgres {
            if column.kind == LogicalType::Binary {
                format!("octet_length({column_name})")
            } else {
                format!("octet_length({column_name}::text)")
            }
        } else {
            format!("length(CAST({column_name} AS BLOB))")
        };
        let sql = format!(
            "SELECT COALESCE(MAX({length}),0) AS bytes FROM {}",
            quoted(&table.name)?
        );
        let row = conn
            .query_one_raw(Statement::from_string(conn.get_database_backend(), sql))
            .await?
            .ok_or_else(|| invalid("长度检查无结果"))?;
        // PostgreSQL 的 MAX(octet_length(...)) 返回的仍是 int4（i32），因此按 i32 读取后再转为 i64。
        let size = if conn.get_database_backend() == DbBackend::Postgres {
            i64::from(row.try_get::<i32>("", "bytes")?)
        } else {
            row.try_get::<i64>("", "bytes")?
        };
        let size = usize::try_from(size).map_err(|_| invalid("单值长度无效"))?;
        if size > MAX_CELL_BYTES {
            return Err(invalid(format!(
                "{}.{} 超过单值 8 MiB 限制",
                table.name, column.name
            )));
        }
    }
    Ok(())
}

fn projection(column: &Column, i: usize, backend: DbBackend) -> Result<String> {
    let name = quoted(&column.name)?;
    let value = match (column.kind, backend) {
        (LogicalType::Integer, _) => format!("CAST({name} AS BIGINT)"),
        (LogicalType::Boolean, DbBackend::Sqlite) => format!("CAST({name} AS INTEGER)"),
        (LogicalType::Float, DbBackend::Postgres) => format!("CAST({name} AS DOUBLE PRECISION)"),
        _ => name.clone(),
    };
    Ok(if backend == DbBackend::Sqlite {
        format!("{value} AS c{i}, typeof({name}) AS t{i}")
    } else {
        format!("{value} AS c{i}")
    })
}

fn cell(raw: &QueryResult, column: &Column, i: usize, backend: DbBackend) -> Result<Cell> {
    let name = format!("c{i}");
    if backend == DbBackend::Sqlite {
        let physical: String = raw.try_get("", &format!("t{i}"))?;
        let valid = physical == "null"
            || match column.kind {
                LogicalType::Integer | LogicalType::Boolean => physical == "integer",
                LogicalType::Text => physical == "text",
                LogicalType::Binary => physical == "blob",
                LogicalType::Float => physical == "real" || physical == "integer",
            };
        if !valid {
            return Err(invalid(format!(
                "{}：SQLite 存储类型 {physical} 不匹配",
                column.name
            )));
        }
    }
    let result = match column.kind {
        LogicalType::Integer => raw
            .try_get::<Option<i64>>("", &name)?
            .map_or(Cell::Null, |v| Cell::Integer(v.to_string())),
        LogicalType::Boolean => {
            let value = if backend == DbBackend::Sqlite {
                match raw.try_get::<Option<i64>>("", &name)? {
                    None => None,
                    Some(0) => Some(false),
                    Some(1) => Some(true),
                    _ => return Err(invalid("布尔值必须为 0/1")),
                }
            } else {
                raw.try_get::<Option<bool>>("", &name)?
            };
            value.map_or(Cell::Null, Cell::Boolean)
        }
        LogicalType::Text => match raw.try_get::<Option<String>>("", &name)? {
            None => Cell::Null,
            Some(v) => {
                if v.contains('\0') {
                    return Err(invalid("文本含 NUL，PG 无法保持"));
                }
                Cell::Text(v)
            }
        },
        LogicalType::Binary => raw
            .try_get::<Option<Vec<u8>>>("", &name)?
            .map_or(Cell::Null, |v| Cell::Binary(STANDARD.encode(v))),
        LogicalType::Float => match raw.try_get::<Option<f64>>("", &name)? {
            None => Cell::Null,
            Some(v) => {
                if !v.is_finite() || v.to_bits() == (-0.0_f64).to_bits() {
                    return Err(invalid("不支持非有限浮点或负零"));
                }
                Cell::Float(format!("{:016x}", v.to_bits()))
            }
        },
    };
    if let Cell::Integer(value) = &result {
        validate_integer(value, column)?;
    }
    Ok(result)
}

fn sql_value(cell: &Cell, column: &Column, backend: DbBackend) -> Result<Value> {
    let kind = column.kind;
    if *cell == Cell::Null {
        return Ok(match kind {
            LogicalType::Integer => Value::BigInt(None),
            LogicalType::Boolean if backend == DbBackend::Postgres => Value::Bool(None),
            LogicalType::Boolean => Value::BigInt(None),
            LogicalType::Binary => Value::Bytes(None),
            LogicalType::Float => Value::Double(None),
            _ => Value::String(None),
        });
    }
    Ok(match (cell, kind) {
        (Cell::Integer(v), LogicalType::Integer) => validate_integer(v, column)?.into(),
        (Cell::Boolean(v), LogicalType::Boolean) => {
            if backend == DbBackend::Postgres {
                (*v).into()
            } else {
                i64::from(*v).into()
            }
        }
        (Cell::Text(v), LogicalType::Text) if !v.contains('\0') => v.clone().into(),
        (Cell::Binary(v), LogicalType::Binary) => STANDARD
            .decode(v)
            .map_err(|_| invalid("非法 Base64"))?
            .into(),
        (Cell::Float(v), LogicalType::Float) => {
            let bits = u64::from_str_radix(v, 16).map_err(|_| invalid("非法浮点编码"))?;
            let number = f64::from_bits(bits);
            if v.len() != 16 || !number.is_finite() || bits == (-0.0_f64).to_bits() {
                return Err(invalid("不可移植浮点值"));
            }
            number.into()
        }
        _ => return Err(invalid("单元格类型与清单不匹配")),
    })
}

fn digest(rows: &[Vec<Cell>]) -> Result<String> {
    Ok(hex::encode(Sha256::digest(
        serde_json::to_vec(rows).map_err(|e| invalid(e.to_string()))?,
    )))
}

fn validate_image(schema: &Schema, image: &DatabaseImage) -> Result<()> {
    if image.format != FORMAT || image.schema_fingerprint != schema.fingerprint {
        return Err(invalid("迁移包格式或两端结构指纹不匹配"));
    }
    if image.tables.len() != schema.tables.len() {
        return Err(invalid("迁移包表数不匹配"));
    }
    if schema.tables.contains_key("nuono_deployment") {
        let metadata = image
            .deployment
            .as_ref()
            .ok_or_else(|| invalid("包缺少部署和插件完整清单"))?;
        uuid::Uuid::parse_str(&metadata.deployment_id).map_err(|_| invalid("部署身份不是 UUID"))?;
        if metadata.framework_version != env!("CARGO_PKG_VERSION")
            || !matches!(metadata.source_backend.as_str(), "sqlite" | "postgres")
        {
            return Err(invalid("迁移包源版本或后端无效"));
        }
        let reconstructed = Schema::from_image(image)?;
        if reconstructed.fingerprint() != schema.fingerprint() {
            return Err(invalid("包插件清单与结构指纹不一致"));
        }
    }
    let mut seen = BTreeSet::new();
    let mut bytes = 0_usize;
    for table in &image.tables {
        if !seen.insert(&table.definition.name)
            || schema.tables.get(&table.definition.name) != Some(&table.definition)
        {
            return Err(invalid("迁移包表结构或唯一性不匹配"));
        }
        if table.sha256 != digest(&table.rows)? {
            return Err(invalid(format!(
                "{}：内容摘要不一致",
                table.definition.name
            )));
        }
        let expected: BTreeSet<_> = table
            .definition
            .columns
            .iter()
            .filter(|c| c.generated_id)
            .map(|c| c.name.as_str())
            .collect();
        let actual: BTreeSet<_> = table.sequences.iter().map(|s| s.column.as_str()).collect();
        if expected != actual || actual.len() != table.sequences.len() {
            return Err(invalid("序列清单不完整或重复"));
        }
        for sequence in &table.sequences {
            if sequence
                .high_water
                .parse::<i64>()
                .map_err(|_| invalid("非法序列上界"))?
                < 0
            {
                return Err(invalid("序列上界不得为负数"));
            }
        }
        for row in &table.rows {
            if row.len() != table.definition.columns.len() {
                return Err(invalid("行宽不匹配"));
            }
            for (value, column) in row.iter().zip(&table.definition.columns) {
                sql_value(value, column, DbBackend::Postgres)?;
            }
            bytes += serde_json::to_vec(row)
                .map_err(|e| invalid(e.to_string()))?
                .len();
            if bytes > MAX_IMAGE_BYTES {
                return Err(invalid("逻辑行数据超过大小上限"));
            }
        }
    }
    Ok(())
}

async fn sequences<C: ConnectionTrait>(conn: &C, table: &Table) -> Result<Vec<Sequence>> {
    let mut result = Vec::new();
    for column in table.columns.iter().filter(|c| c.generated_id) {
        let sql = format!(
            "SELECT COALESCE(MAX(CAST({} AS BIGINT)),0) AS value FROM {}",
            quoted(&column.name)?,
            quoted(&table.name)?
        );
        let maximum = conn
            .query_one_raw(Statement::from_string(conn.get_database_backend(), sql))
            .await?
            .ok_or_else(|| invalid("缺少序列最大值"))?
            .try_get::<i64>("", "value")?;
        let allocated = match conn.get_database_backend() {
            DbBackend::Sqlite => conn
                .query_one_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "SELECT seq AS value FROM sqlite_sequence WHERE name=$1",
                    [table.name.clone().into()],
                ))
                .await?
                .map(|r| r.try_get::<i64>("", "value"))
                .transpose()?
                .unwrap_or(0),
            DbBackend::Postgres => {
                let sequence = sequence_name(conn, table, &column.name).await?;
                let row = conn
                    .query_one_raw(Statement::from_string(
                        DbBackend::Postgres,
                        format!("SELECT last_value AS value,is_called FROM {sequence}"),
                    ))
                    .await?
                    .ok_or_else(|| invalid("缺少序列状态"))?;
                let value = row.try_get::<i64>("", "value")?;
                if row.try_get::<bool>("", "is_called")? {
                    value
                } else {
                    // 尚未调用过 nextval 的序列（is_called = false）在下一次分配时会直接返回 last_value 本身；
                    // 若曾通过 RESTART 或 setval(value, false) 预留了大于 1 的起始值，则其已分配上界水位应为 last_value - 1。
                    value
                        .checked_sub(1)
                        .ok_or_else(|| invalid("非法的未取值序列状态"))?
                }
            }
            _ => return Err(invalid("不支持的后端")),
        };
        result.push(Sequence {
            column: column.name.clone(),
            high_water: maximum.max(allocated).max(0).to_string(),
        });
    }
    Ok(result)
}

async fn sequence_name<C: ConnectionTrait>(
    conn: &C,
    table: &Table,
    column: &str,
) -> Result<String> {
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT pg_get_serial_sequence($1,$2) AS name",
            [quoted(&table.name)?.into(), column.to_owned().into()],
        ))
        .await?
        .ok_or_else(|| invalid("缺少 identity 序列"))?;
    let name = row
        .try_get::<Option<String>>("", "name")?
        .ok_or_else(|| invalid("已登记自增列没有序列"))?;
    // 即便序列名由 PostgreSQL 的 pg_get_serial_sequence 返回，在拼入 SQL 前仍对每个点号分段做严格的标识符校验与双引号转义。
    name.split('.')
        .map(|part| quoted(part.trim_matches('"')))
        .collect::<Result<Vec<_>>>()
        .map(|v| v.join("."))
}

async fn restore_sequences<C: ConnectionTrait>(conn: &C, table: &TableImage) -> Result<()> {
    for sequence in &table.sequences {
        let high = sequence
            .high_water
            .parse::<i64>()
            .map_err(|_| invalid("非法序列上界"))?;
        let column_index = table
            .definition
            .columns
            .iter()
            .position(|c| c.name == sequence.column)
            .ok_or_else(|| invalid("未知序列列"))?;
        let mut maximum = 0;
        for row in &table.rows {
            if let Cell::Integer(value) = &row[column_index] {
                maximum = maximum.max(value.parse::<i64>().map_err(|_| invalid("非法 ID"))?);
            }
        }
        if maximum > high {
            return Err(invalid("序列上界小于导入 ID"));
        }
        match conn.get_database_backend() {
            DbBackend::Sqlite => {
                conn.execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "DELETE FROM sqlite_sequence WHERE name=$1",
                    [table.definition.name.clone().into()],
                ))
                .await?;
                conn.execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "INSERT INTO sqlite_sequence(name,seq) VALUES($1,$2)",
                    [table.definition.name.clone().into(), high.into()],
                ))
                .await?;
            }
            DbBackend::Postgres => {
                let name = sequence_name(conn, &table.definition, &sequence.column).await?;
                conn.query_one_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT setval($1::regclass,$2,$3)",
                    [name.into(), high.max(1).into(), (high > 0).into()],
                ))
                .await?;
            }
            _ => return Err(invalid("不支持的后端")),
        }
    }
    Ok(())
}

async fn verify_on<C: ConnectionTrait>(
    conn: &C,
    schema: &Schema,
    image: &DatabaseImage,
) -> Result<()> {
    let actual = snapshot(conn, schema).await?;
    let mut expected_deployment = image.deployment.clone();
    if let (Some(expected), Some(actual)) = (&mut expected_deployment, &actual.deployment) {
        // 跨数据库迁移后 source_backend 会变为当前目标引擎（如 sqlite -> postgres），但其余所有持久化的归属信息、插件清单与资产声明必须完全一致。
        expected.source_backend = actual.source_backend.clone();
    }
    if expected_deployment != actual.deployment {
        return Err(invalid("部署或插件清单与实际账本记录不一致"));
    }
    let expected: BTreeMap<_, _> = image
        .tables
        .iter()
        .map(|t| (&t.definition.name, t))
        .collect();
    for table in &actual.tables {
        let original = expected
            .get(&table.definition.name)
            .ok_or_else(|| invalid("导入出现额外表"))?;
        if table.sha256 != original.sha256
            || table.rows.len() != original.rows.len()
            || table.sequences != original.sequences
        {
            return Err(invalid(format!(
                "{}：导入后的行、类型或自增上界不一致",
                table.definition.name
            )));
        }
    }
    Ok(())
}

async fn defer_foreign_keys(tx: &DatabaseTransaction) -> Result<Vec<(String, String, bool, bool)>> {
    if tx.get_database_backend() == DbBackend::Sqlite {
        tx.execute_unprepared("PRAGMA defer_foreign_keys=ON")
            .await?;
        return Ok(Vec::new());
    }
    let rows=tx.query_all_raw(Statement::from_string(DbBackend::Postgres,"SELECT c.relname AS table_name,k.conname AS name,k.condeferrable AS deferred,k.condeferred AS initial FROM pg_constraint k JOIN pg_class c ON c.oid=k.conrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND k.contype='f'")).await?;
    let mut constraints = Vec::new();
    for row in rows {
        let table: String = row.try_get("", "table_name")?;
        let name: String = row.try_get("", "name")?;
        let deferred: bool = row.try_get("", "deferred")?;
        let initial: bool = row.try_get("", "initial")?;
        tx.execute_unprepared(&format!(
            "ALTER TABLE {} ALTER CONSTRAINT {} DEFERRABLE INITIALLY DEFERRED",
            quoted(&table)?,
            quoted(&name)?
        ))
        .await?;
        constraints.push((table, name, deferred, initial));
    }
    tx.execute_unprepared("SET CONSTRAINTS ALL DEFERRED")
        .await?;
    Ok(constraints)
}

async fn check_foreign_keys(
    tx: &DatabaseTransaction,
    constraints: &[(String, String, bool, bool)],
) -> Result<()> {
    if tx.get_database_backend() == DbBackend::Sqlite {
        if !tx
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "PRAGMA foreign_key_check",
            ))
            .await?
            .is_empty()
        {
            return Err(invalid("外键校验失败"));
        }
        return Ok(());
    }
    tx.execute_unprepared("SET CONSTRAINTS ALL IMMEDIATE")
        .await?;
    for (table, name, deferred, initial) in constraints {
        let state = if *deferred {
            if *initial {
                "DEFERRABLE INITIALLY DEFERRED"
            } else {
                "DEFERRABLE INITIALLY IMMEDIATE"
            }
        } else {
            "NOT DEFERRABLE"
        };
        tx.execute_unprepared(&format!(
            "ALTER TABLE {} ALTER CONSTRAINT {} {state}",
            quoted(table)?,
            quoted(name)?
        ))
        .await?;
    }
    Ok(())
}

async fn validate_source_foreign_keys<C: ConnectionTrait>(conn: &C) -> Result<()> {
    if conn.get_database_backend() == DbBackend::Sqlite {
        if !conn
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "PRAGMA foreign_key_check",
            ))
            .await?
            .is_empty()
        {
            return Err(invalid("源数据外键校验失败"));
        }
        return Ok(());
    }
    let rows=conn.query_all_raw(Statement::from_string(DbBackend::Postgres,r#"
        SELECT origin.relname AS child,parent.relname AS parent,k.conname,k.confmatchtype::text AS match_type,
          (SELECT json_agg(a.attname ORDER BY x.ordinality)::text FROM unnest(k.conkey) WITH ORDINALITY x(attnum,ordinality) JOIN pg_attribute a ON a.attrelid=k.conrelid AND a.attnum=x.attnum) AS child_columns,
          (SELECT json_agg(a.attname ORDER BY x.ordinality)::text FROM unnest(k.confkey) WITH ORDINALITY x(attnum,ordinality) JOIN pg_attribute a ON a.attrelid=k.confrelid AND a.attnum=x.attnum) AS parent_columns
        FROM pg_constraint k JOIN pg_class origin ON origin.oid=k.conrelid JOIN pg_namespace n ON n.oid=origin.relnamespace JOIN pg_class parent ON parent.oid=k.confrelid
        WHERE k.contype='f' AND n.nspname='public'
    "#)).await?;
    for row in rows {
        let child = row.try_get::<String>("", "child")?;
        let parent = row.try_get::<String>("", "parent")?;
        let columns: Vec<String> =
            super::parse_document(row.try_get::<String>("", "child_columns")?.as_bytes())?;
        let referenced: Vec<String> =
            super::parse_document(row.try_get::<String>("", "parent_columns")?.as_bytes())?;
        if columns.is_empty() || columns.len() != referenced.len() {
            return Err(invalid("外键列清单无效"));
        }
        let columns = columns
            .iter()
            .map(|v| quoted(v))
            .collect::<Result<Vec<_>>>()?;
        let referenced = referenced
            .iter()
            .map(|v| quoted(v))
            .collect::<Result<Vec<_>>>()?;
        let nonnull = columns
            .iter()
            .map(|c| format!("c.{c} IS NOT NULL"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let equality = columns
            .iter()
            .zip(&referenced)
            .map(|(a, b)| format!("c.{a}=p.{b}"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let mut predicate = format!(
            "({nonnull}) AND NOT EXISTS(SELECT 1 FROM {} p WHERE {equality})",
            quoted(&parent)?
        );
        match row.try_get::<String>("", "match_type")?.as_str() {
            "s" => {}
            "f" => {
                let any_null = columns
                    .iter()
                    .map(|c| format!("c.{c} IS NULL"))
                    .collect::<Vec<_>>()
                    .join(" OR ");
                let any_nonnull = columns
                    .iter()
                    .map(|c| format!("c.{c} IS NOT NULL"))
                    .collect::<Vec<_>>()
                    .join(" OR ");
                predicate = format!("({predicate}) OR (({any_null}) AND ({any_nonnull}))");
            }
            _ => return Err(invalid("不支持的外键匹配规则")),
        }
        if conn
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                format!(
                    "SELECT 1 AS invalid FROM {} c WHERE {predicate} LIMIT 1",
                    quoted(&child)?
                ),
            ))
            .await?
            .is_some()
        {
            return Err(invalid(format!(
                "{child}.{} 存在无效外键",
                row.try_get::<String>("", "conname")?
            )));
        }
    }
    Ok(())
}

fn validate_integer(value: &str, column: &Column) -> Result<i64> {
    let value = value.parse::<i64>().map_err(|_| invalid("非法 i64"))?;
    let valid = match column.integer_bits {
        Some(16) => i16::try_from(value).is_ok(),
        Some(32) => i32::try_from(value).is_ok(),
        Some(64) => true,
        _ => false,
    };
    if !valid {
        return Err(invalid(format!("{} 超出声明的有符号整数范围", column.name)));
    }
    Ok(value)
}
