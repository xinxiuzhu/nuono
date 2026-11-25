//! 插件 Web 路由的模式匹配。
//!
//! 声明的路径由字面段和参数段组成，参数段写作 `:name`，只匹配**恰好一个**非空段：
//! `/groups/:group_id` 命中 `/groups/123`，不命中 `/groups`、`/groups/123/payments`。
//! 不做前缀匹配、也不跨段捕获 —— 插件自己按 `req.path` 解析段，宿主只负责
//! 「这条请求归哪条声明」。
//!
//! # 为什么字面量优先
//!
//! `/groups/plans` 与 `/groups/:group_id` 可以同时存在。请求 `/groups/plans` 必须归
//! 前者，否则「用参数名当资源名」会变成无法声明的路径。选择规则只有一条：字面段越多
//! 越优先；数量相同时先声明的优先（同形模式已在加载阶段被拒绝）。
//!
//! # 归一化去重
//!
//! `/groups/:id` 和 `/groups/:gid` 是同一个模式。参数名只进插件自己的解析代码，
//! 对宿主没有意义，所以加载阶段按「参数位替换成 `:`」归一化后查重，否则两条声明会
//! 争同一条请求，结果取决于声明顺序。

/// 把路径切成段。根路径 `/` 是零段；首尾与重复斜杠由加载校验挡在前面。
pub(super) fn segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|part| !part.is_empty()).collect()
}

fn is_parameter(segment: &str) -> bool {
    segment.starts_with(':')
}

/// 声明路径里的参数名。空名、非法字符、重复名都算错误。
///
/// 字面段里出现 `:`（例如 `foo:bar`）同样拒绝：那多半是想写参数却写错了位置，
/// 静默当字面量会让请求永远 404。
pub(super) fn parameter_names(path: &str) -> Result<Vec<&str>, String> {
    let mut names = Vec::new();
    for segment in segments(path) {
        let Some(name) = segment.strip_prefix(':') else {
            if segment.contains(':') {
                return Err(format!("段 {segment:?} 里的 ':' 只能出现在参数段开头"));
            }
            continue;
        };
        if name.is_empty() {
            return Err("参数段缺少名字".into());
        }
        let mut chars = name.chars();
        let valid_start = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
        if !valid_start || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!(
                "参数名 {name:?} 只能包含字母、数字和下划线，且不能以数字开头"
            ));
        }
        if names.contains(&name) {
            return Err(format!("参数名 {name:?} 在同一条路径里出现两次"));
        }
        names.push(name);
    }
    Ok(names)
}

/// 归一化形式：参数段一律替换成 `:`。只用于查重，不参与匹配。
pub(super) fn normalized(path: &str) -> String {
    let parts: Vec<&str> = segments(path)
        .into_iter()
        .map(|segment| if is_parameter(segment) { ":" } else { segment })
        .collect();
    format!("/{}", parts.join("/"))
}

/// 字面段数量，匹配优先级用。
pub(super) fn literal_count(path: &str) -> usize {
    segments(path)
        .into_iter()
        .filter(|segment| !is_parameter(segment))
        .count()
}

/// 声明的模式是否命中实际路径。
pub(super) fn matches(declared: &str, actual: &str) -> bool {
    let declared = segments(declared);
    let actual = segments(actual);
    declared.len() == actual.len()
        && declared.iter().zip(&actual).all(|(pattern, value)| {
            if is_parameter(pattern) {
                !value.is_empty()
            } else {
                pattern == value
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_match_exactly_one_non_empty_segment() {
        assert!(matches("/groups/:group_id", "/groups/123"));
        assert!(matches(
            "/groups/:group_id/payments",
            "/groups/123/payments"
        ));
        assert!(matches("/", "/"));
        assert!(!matches("/groups/:group_id", "/groups"));
        assert!(!matches("/groups/:group_id", "/groups/123/payments"));
        assert!(!matches("/groups/:group_id", "/groups/"));
        assert!(!matches("/groups", "/groups/123"));
    }

    #[test]
    fn literal_segments_are_case_sensitive() {
        assert!(!matches("/Groups", "/groups"));
        assert!(matches("/groups/:id", "/groups/ABC"));
    }

    #[test]
    fn literal_count_orders_specificity() {
        assert_eq!(literal_count("/groups/plans"), 2);
        assert_eq!(literal_count("/groups/:group_id"), 1);
        assert_eq!(literal_count("/"), 0);
    }

    #[test]
    fn normalization_ignores_parameter_names() {
        assert_eq!(normalized("/groups/:id"), "/groups/:");
        assert_eq!(normalized("/groups/:gid"), "/groups/:");
        assert_eq!(normalized("/groups/plans"), "/groups/plans");
        assert_eq!(normalized("/"), "/");
    }

    #[test]
    fn parameter_names_are_validated() {
        assert_eq!(
            parameter_names("/groups/:group_id/payments/:payment_id"),
            Ok(vec!["group_id", "payment_id"])
        );
        for path in [
            "/groups/:",
            "/groups/:1id",
            "/groups/:a-b",
            "/groups/:id/:id",
        ] {
            assert!(parameter_names(path).is_err(), "{path} 应被拒绝");
        }
        assert!(
            parameter_names("/groups/foo:bar").is_err(),
            "字面段里的冒号多半是写错了参数位置"
        );
    }
}
