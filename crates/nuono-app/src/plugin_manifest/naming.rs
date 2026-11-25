//! 插件数据表前缀的命名转换与校验工具（供声明构造器与宿主存储层共用）。
use nuono_error::{NuonoError, Result};
use regex::Regex;
use std::sync::LazyLock;

static PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-z][a-z0-9]*(?:_[a-z0-9]+)*_$")
        .expect("the static table-prefix regex must compile")
});

/// 将 ASCII `CamelCase` 或 `kebab-case` 插件名转换为带末尾下划线的规范 `snake_case_` 表名前缀。
///
/// # Errors
/// 拒绝包含标点符号、非 ASCII 字符、以数字开头或超长的名称。
pub fn normalize_table_prefix(value: &str) -> Result<String> {
    if !value
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphabetic)
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(NuonoError::InvalidInput(
            "插件表前缀只能包含 ASCII 字母、数字、下划线和连字符，且以字母开头".into(),
        ));
    }
    let prefix = snake_case_prefix(value);
    validate_table_prefix(&prefix)?;
    Ok(prefix)
}

/// 校验表名前缀是否为规范的小写 `snake_case_`（必须以单下划线结尾，且长度不超过 62 字节以为具体表名留出空间）。
///
/// # Errors
/// 当表名前缀不符合规范正则或超过 62 字节时返回错误。
pub fn validate_table_prefix(value: &str) -> Result<()> {
    if value.len() > 62 || !PREFIX.is_match(value) {
        return Err(NuonoError::InvalidInput(
            "插件表前缀必须符合 ^[a-z][a-z0-9]*(?:_[a-z0-9]+)*_$，且不超过 62 字节".into(),
        ));
    }
    Ok(())
}

pub(super) fn snake_case_prefix(value: &str) -> String {
    let chars: Vec<_> = value.chars().collect();
    let mut result = String::new();
    for (index, ch) in chars.iter().copied().enumerate() {
        if matches!(ch, '-' | '_') {
            if !result.is_empty() && !result.ends_with('_') {
                result.push('_');
            }
            continue;
        }
        let previous = index.checked_sub(1).and_then(|i| chars.get(i));
        let next = chars.get(index + 1);
        if ch.is_ascii_uppercase()
            && !result.is_empty()
            && !result.ends_with('_')
            && (previous.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                || (previous.is_some_and(char::is_ascii_uppercase)
                    && next.is_some_and(char::is_ascii_lowercase)))
        {
            result.push('_');
        }
        result.push(ch.to_ascii_lowercase());
    }
    if !result.ends_with('_') {
        result.push('_');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prefixes_are_canonical_idempotent_and_preserve_acronym_boundaries() {
        for (raw, expected) in [
            ("SampleNN", "sample_nn_"),
            ("demo-kit", "demo_kit_"),
            ("XMLParser", "xml_parser_"),
            ("dm_", "dm_"),
            ("demo__kit--", "demo_kit_"),
        ] {
            assert_eq!(normalize_table_prefix(raw).unwrap(), expected, "{raw}");
            assert_eq!(normalize_table_prefix(expected).unwrap(), expected);
            validate_table_prefix(expected).unwrap();
        }
        for invalid in [
            "",
            "1plugin",
            "_private",
            "a.b",
            "a/b",
            "中文",
            "x; DROP TABLE users",
            "a\0b",
        ] {
            assert!(normalize_table_prefix(invalid).is_err(), "{invalid:?}");
        }
        assert!(normalize_table_prefix(&"a".repeat(63)).is_err());
        for invalid in ["SampleNN_", "demo-kit_", "a__b_", "missing_suffix"] {
            assert!(validate_table_prefix(invalid).is_err(), "{invalid}");
        }
    }
}
