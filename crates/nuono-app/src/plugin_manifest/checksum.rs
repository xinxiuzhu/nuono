//! 插件数据库迁移脚本的跨方言统一摘要计算。
use nuono_error::{NuonoError, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// 将同一迁移版本的所有数据库方言脚本合并计算单个 SHA-256 校验和（结果与当前实际运行哪种数据库无关）。
///
/// 方言名称按字典序排列在固定结构的 JSON 文档内；SQL 原文字节（包括字符串字面量内的换行符）
/// 均原样参与哈希，防止已应用的迁移脚本被静默篡改。插件运行配置不参与此校验和。
///
/// # Errors
/// 当方言名称为空、重复、版本号无效或序列化失败时返回错误。
pub fn migration_checksum(
    version: i64,
    description: &str,
    sources: &[(&str, &str)],
) -> Result<String> {
    let mut variants = BTreeMap::new();
    for (dialect, sql) in sources {
        if dialect.is_empty() || variants.insert(*dialect, *sql).is_some() {
            return Err(NuonoError::InvalidInput("迁移来源标识为空或重复".into()));
        }
    }
    if version <= 0 || description.trim().is_empty() || variants.is_empty() {
        return Err(NuonoError::InvalidInput(
            "迁移清单缺少版本、说明或来源".into(),
        ));
    }
    #[derive(Serialize)]
    struct Document<'a> {
        format: &'static str,
        version: String,
        description: &'a str,
        variants: BTreeMap<&'a str, &'a str>,
    }
    let bytes = serde_json::to_vec(&Document {
        format: "nuono.migration.v1",
        version: version.to_string(),
        description,
        variants,
    })?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_digest_covers_all_variants_and_preserves_sql_literals() {
        let sources = [
            ("postgres", "CREATE TABLE a(id BIGINT);\r\n"),
            ("sqlite", "CREATE TABLE a(id INTEGER);"),
        ];
        let digest = migration_checksum(1, "create", &sources).unwrap();
        assert_eq!(
            digest,
            migration_checksum(
                1,
                "create",
                &[("sqlite", sources[1].1), ("postgres", sources[0].1)]
            )
            .unwrap()
        );
        assert_ne!(
            digest,
            migration_checksum(
                1,
                "create",
                &[
                    ("postgres", sources[0].1),
                    ("sqlite", "CREATE TABLE a(id TEXT);")
                ]
            )
            .unwrap()
        );
        assert_ne!(digest, migration_checksum(2, "create", &sources).unwrap());
        assert_ne!(
            migration_checksum(1, "literal", &[("sql", "SELECT 'a\r\nb'")]).unwrap(),
            migration_checksum(1, "literal", &[("sql", "SELECT 'a\nb'")]).unwrap()
        );
    }
}
