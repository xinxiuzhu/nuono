//! 定时任务定义与校验逻辑，供数据库持久化和运行时调度准入共用。
//!
//! 注意：静态校验无法检查 Bot 实例或订阅状态，因为这些状态在任务实际触发前可能会随时变化。

use croner::parser::{CronParser, Seconds};
use nuono_error::{NuonoError, Result};
use serde_json::Value;

pub const DEFAULT_TIMEZONE: &str = "UTC";
pub use crate::managed_messages::MAX_MESSAGE_CHARS;

/// 统一使用 IANA 时区名称，禁止依赖宿主机的本地时区偏移；历史存量任务默认保持为 UTC。
pub fn timezone(name: &str) -> Result<chrono_tz::Tz> {
    if name.len() > 64 {
        return Err(invalid("定时任务时区名称过长"));
    }
    name.parse()
        .map_err(|_| invalid("定时任务时区无效，请使用 IANA 时区名称（例如 Asia/Shanghai）"))
}

pub fn next_occurrences(
    expression: &str,
    zone: &str,
    after: chrono::DateTime<chrono::Utc>,
    count: usize,
) -> Result<Vec<chrono::DateTime<chrono::Utc>>> {
    let cron = parse_schedule(expression)?;
    let zone = timezone(zone)?;
    let mut cursor = after.with_timezone(&zone);
    let mut dates = Vec::new();
    for _ in 0..count.min(10) {
        let Ok(next) = cron.find_next_occurrence(&cursor, false) else {
            break;
        };
        dates.push(next.with_timezone(&chrono::Utc));
        cursor = next;
    }
    Ok(dates)
}

pub struct ZonedSchedule {
    cron: croner::Cron,
    zone: chrono_tz::Tz,
}

impl ZonedSchedule {
    pub fn parse(expression: &str, zone: &str) -> Result<Self> {
        Ok(Self {
            cron: parse_schedule(expression)?,
            zone: timezone(zone)?,
        })
    }
    pub fn next(
        &self,
        after: chrono::DateTime<chrono::Utc>,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        self.cron
            .find_next_occurrence(&after.with_timezone(&self.zone), false)
            .ok()
            .map(|date| date.with_timezone(&chrono::Utc))
    }
}

pub fn validate_schedule(expression: &str, zone: &str) -> Result<()> {
    if next_occurrences(expression, zone, chrono::Utc::now(), 1)?.is_empty() {
        return Err(invalid("定时任务 cron 表达式没有未来触发时间"));
    }
    Ok(())
}

pub const MAX_NAME_BYTES: usize = 256;
pub const MAX_CRON_BYTES: usize = 256;
pub const MAX_ACTION_TYPE_BYTES: usize = 64;
pub const MAX_BOT_ID_BYTES: usize = 64;
pub const MAX_ACTION_PARAMS_BYTES: usize = 64 * 1024;
// PostgreSQL 现有的 VARCHAR 列同样限制 Unicode 字符数。
const MAX_NAME_CHARS: usize = 100;
const MAX_CRON_CHARS: usize = 100;
const MAX_ACTION_TYPE_CHARS: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduledAction {
    SendNativeMessage {
        request: Box<crate::managed_messages::ManagedMessage>,
    },
    SendMessage {
        target_type: String,
        target_id: String,
        message: String,
    },
    PushSubscribe {
        topic: String,
        message: String,
    },
}

fn invalid(message: impl Into<String>) -> NuonoError {
    NuonoError::Validation(message.into())
}

fn required_string<'a>(params: &'a Value, field: &str) -> Result<&'a str> {
    params[field]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| invalid(format!("定时任务参数 {field} 必须是非空字符串")))
}

/// 解析受支持的定时任务动作；如果字段缺失或格式有误则直接报错，绝不静默替换为默认值。
pub fn parse_action(action_type: &str, params: Option<&str>) -> Result<ScheduledAction> {
    if action_type == "send_native_message" {
        let request: crate::managed_messages::ManagedMessage =
            serde_json::from_str(params.ok_or_else(|| invalid("定时任务必须提供动作参数"))?)
                .map_err(|_| invalid("定时消息的路由、目标或消息格式无效"))?;
        request.validate()?;
        return Ok(ScheduledAction::SendNativeMessage {
            request: Box::new(request),
        });
    }
    if !matches!(action_type, "send_message" | "push_subscribe") {
        return Err(invalid(format!("不支持的定时任务动作类型: {action_type}")));
    }
    let params: Value =
        serde_json::from_str(params.ok_or_else(|| invalid("定时任务必须提供动作参数"))?)
            .map_err(|_| invalid("定时任务动作参数必须是 JSON 对象"))?;
    if !params.is_object() {
        return Err(invalid("定时任务动作参数必须是 JSON 对象"));
    }
    let message = required_string(&params, "message")?.to_owned();
    crate::managed_messages::validate_text(&message)?;
    match action_type {
        "send_message" => {
            let target_type = required_string(&params, "target_type")?;
            if !matches!(target_type, "group" | "user" | "private") {
                return Err(invalid(
                    "定时任务 target_type 必须是 group、user 或 private",
                ));
            }
            let target_id = required_string(&params, "target_id")?;
            positive_id(target_id, "target_id")?;
            Ok(ScheduledAction::SendMessage {
                target_type: target_type.to_owned(),
                target_id: target_id.to_owned(),
                message,
            })
        }
        "push_subscribe" => Ok(ScheduledAction::PushSubscribe {
            topic: required_string(&params, "topic")?.to_owned(),
            message,
        }),
        _ => unreachable!("supported actions were checked above"),
    }
}

fn positive_id(value: &str, field: &str) -> Result<()> {
    if value.parse::<i64>().is_ok_and(|id| id > 0) {
        Ok(())
    } else {
        Err(invalid(format!("定时任务 {field} 必须是有效的正整数 ID")))
    }
}

/// 解析系统约定的 6 字段或 7 字段（含秒）cron 表达式，包括已过期的历史表达式。
pub fn parse_schedule(cron_expr: &str) -> Result<croner::Cron> {
    if cron_expr.len() > MAX_CRON_BYTES {
        return Err(invalid("Cron 表达式过长"));
    }
    CronParser::builder()
        .seconds(Seconds::Required)
        .dom_and_dow(true)
        // croner 4.x 默认要求严格的步进范围语法；这里开启宽松范围模式，
        // 以兼容 3.x 时期已落库的 `5/5`、`0/30` 等写法，无需重写存量表达式。
        .sloppy_ranges(true)
        .build()
        .parse(cron_expr)
        .map_err(|_| invalid("定时任务 cron 表达式无效（必须包含秒字段）"))
}

pub fn validate_cron(cron_expr: &str) -> Result<()> {
    validate_schedule(cron_expr, DEFAULT_TIMEZONE)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct TaskDefinition {
    pub name: String,
    pub cron_expr: String,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    pub action_type: String,
    pub action_params: Option<String>,
    pub bot_id: Option<String>,
}

pub fn default_timezone() -> String {
    DEFAULT_TIMEZONE.into()
}

impl TaskDefinition {
    pub fn validate(&self) -> Result<ScheduledAction> {
        validate_definition_size(
            &self.name,
            &self.cron_expr,
            &self.action_type,
            self.action_params.as_deref(),
            self.bot_id.as_deref(),
        )?;
        if self.name.trim().is_empty() {
            return Err(invalid("定时任务名称不能为空"));
        }
        validate_schedule(&self.cron_expr, &self.timezone)?;
        validate_bot_id(self.bot_id.as_deref())?;
        parse_action(&self.action_type, self.action_params.as_deref())
    }
}

/// 空白的 `bot_id` 保持旧版未绑定行为；但如果填写了格式错误的 `bot_id` 必须直接报错，绝不能回退成全员广播。
pub fn validate_bot_id(bot_id: Option<&str>) -> Result<()> {
    if let Some(bot_id) = bot_id.map(str::trim).filter(|value| !value.is_empty()) {
        positive_id(bot_id, "bot_id")?;
    }
    Ok(())
}

pub fn validate_task(
    name: &str,
    cron_expr: &str,
    action_type: &str,
    params: Option<&str>,
    bot_id: Option<&str>,
) -> Result<ScheduledAction> {
    validate_definition_size(name, cron_expr, action_type, params, bot_id)?;
    if name.trim().is_empty() {
        return Err(invalid("定时任务名称不能为空"));
    }
    validate_cron(cron_expr)?;
    validate_bot_id(bot_id)?;
    parse_action(action_type, params)
}

/// 在解析或克隆任务定义前先检查字节数与字符数上限。
/// 存储层在执行带长度限制的 SQL 查询后也会调用此校验，防止被截断的历史脏数据被误执行。
pub fn validate_definition_size(
    name: &str,
    cron_expr: &str,
    action_type: &str,
    params: Option<&str>,
    bot_id: Option<&str>,
) -> Result<()> {
    for (field, value, max_bytes) in [
        ("name", name, MAX_NAME_BYTES),
        ("cron_expr", cron_expr, MAX_CRON_BYTES),
        ("action_type", action_type, MAX_ACTION_TYPE_BYTES),
        (
            "action_params",
            params.unwrap_or_default(),
            MAX_ACTION_PARAMS_BYTES,
        ),
        ("bot_id", bot_id.unwrap_or_default(), MAX_BOT_ID_BYTES),
    ] {
        if value.len() > max_bytes {
            return Err(invalid(format!(
                "定时任务 {field} 不能超过 {max_bytes} 字节"
            )));
        }
    }
    for (field, value, max_chars) in [
        ("name", name, MAX_NAME_CHARS),
        ("cron_expr", cron_expr, MAX_CRON_CHARS),
        ("action_type", action_type, MAX_ACTION_TYPE_CHARS),
    ] {
        if value.chars().count() > max_chars {
            return Err(invalid(format!(
                "定时任务 {field} 不能超过 {max_chars} 个字符"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn croner_upgrade_preserves_existing_single_value_steps() {
        use chrono::TimeZone;
        let start = chrono::Utc.with_ymd_and_hms(2026, 10, 20, 0, 0, 0).unwrap();
        for (expression, seconds) in [
            ("5/5 * * * * *", vec![5, 10, 15]),
            ("0/30 * * * * *", vec![30, 60, 90]),
        ] {
            let cron = parse_schedule(expression).unwrap();
            let mut current = start;
            for second in seconds {
                current = cron.find_next_occurrence(&current, false).unwrap();
                assert_eq!(current, start + chrono::Duration::seconds(second));
            }
        }
        assert!(parse_schedule("5/0 * * * * *").is_err());
    }

    #[test]
    fn definition_size_limits_count_utf8_bytes_and_accept_exact_boundaries() {
        let name = format!("{}a", "好".repeat(MAX_NAME_BYTES / 3));
        let cron = format!("{}a", "好".repeat(MAX_CRON_BYTES / 3));
        let action = format!("{}a", "好".repeat(MAX_ACTION_TYPE_BYTES / 3));
        let params = "a".repeat(MAX_ACTION_PARAMS_BYTES);
        let bot = "a".repeat(MAX_BOT_ID_BYTES);
        validate_definition_size(&name, &cron, &action, Some(&params), Some(&bot)).unwrap();
        for (name, cron, action, params, bot) in [
            (
                format!("{name}a"),
                cron.clone(),
                action.clone(),
                params.clone(),
                bot.clone(),
            ),
            (
                name.clone(),
                format!("{cron}a"),
                action.clone(),
                params.clone(),
                bot.clone(),
            ),
            (
                name.clone(),
                cron.clone(),
                format!("{action}a"),
                params.clone(),
                bot.clone(),
            ),
            (
                name.clone(),
                cron.clone(),
                action.clone(),
                format!("{params}a"),
                bot.clone(),
            ),
            (
                name,
                cron.clone(),
                action.clone(),
                params.clone(),
                format!("{bot}a"),
            ),
            ("好".repeat(86), cron, action, params, bot),
        ] {
            assert!(matches!(
                validate_task(&name, &cron, &action, Some(&params), Some(&bot)),
                Err(NuonoError::Validation(message)) if message.contains("字节")
            ));
        }
        assert!(matches!(
            validate_definition_size(&"a".repeat(101), "0 0 * * * *", "send_message", None, None),
            Err(NuonoError::Validation(message)) if message.contains("100 个字符")
        ));
    }

    #[test]
    fn action_parser_rejects_missing_malformed_or_unexecutable_parameters() {
        for params in [
            None,
            Some("not json"),
            Some("null"),
            Some("[]"),
            Some("{}"),
            Some(r#"{"target_type":"other","target_id":"1","message":"hello"}"#),
            Some(r#"{"target_type":"group","target_id":1,"message":"hello"}"#),
            Some(r#"{"target_type":"group","target_id":"0","message":"hello"}"#),
            Some(r#"{"target_type":"group","target_id":"-1","message":"hello"}"#),
            Some(r#"{"target_type":"group","target_id":"9223372036854775808","message":"hello"}"#),
            Some(r#"{"target_type":"group","target_id":" 1 ","message":"hello"}"#),
            Some(r#"{"target_type":"group","target_id":"1","message":"  "}"#),
        ] {
            assert!(
                matches!(
                    parse_action("send_message", params),
                    Err(NuonoError::Validation(_))
                ),
                "{params:?}"
            );
        }
        assert!(parse_action("run_sop", Some("{}")).is_err());
        assert!(parse_action("push_subscribe", Some(r#"{"topic":"","message":"hi"}"#)).is_err());
    }

    #[test]
    fn action_parser_preserves_supported_values_and_extension_fields() {
        for target_type in ["group", "user", "private"] {
            let params = serde_json::json!({
                "target_type": target_type, "target_id": "42", "message": " hello ",
                "extension": true
            });
            assert_eq!(
                parse_action("send_message", Some(&params.to_string())).unwrap(),
                ScheduledAction::SendMessage {
                    target_type: target_type.into(),
                    target_id: "42".into(),
                    message: " hello ".into()
                }
            );
        }
        assert_eq!(
            parse_action(
                "push_subscribe",
                Some(r#"{"topic":"news","message":"hello"}"#)
            )
            .unwrap(),
            ScheduledAction::PushSubscribe {
                topic: "news".into(),
                message: "hello".into()
            }
        );
    }

    #[test]
    fn cron_validator_requires_scheduler_dialect_and_future_occurrence() {
        assert!(parse_schedule("0 0 0 * * * 2000").is_ok());
        for expression in ["0 0 * * * *", "@daily", "0 0 0 * * * 2099"] {
            assert!(validate_cron(expression).is_ok(), "{expression}");
        }
        for expression in ["not cron", "0 * * * *", "60 * * * * *", "0 0 0 * * * 2000"] {
            assert!(
                matches!(validate_cron(expression), Err(NuonoError::Validation(_))),
                "{expression}"
            );
        }
    }

    #[test]
    fn bot_binding_rejects_values_that_would_fall_back_to_broadcast() {
        for bot_id in [None, Some(""), Some("  "), Some("12345"), Some(" 12345 ")] {
            validate_bot_id(bot_id).unwrap();
        }
        for bot_id in ["abc", "0", "-1", "9223372036854775808"] {
            assert!(matches!(
                validate_bot_id(Some(bot_id)),
                Err(NuonoError::Validation(_))
            ));
        }
    }
}

#[cfg(test)]
mod timezone_tests {
    use super::*;
    fn instant(s: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }
    #[test]
    fn dashboard_presets_execute_at_local_wall_time() {
        let start = instant("2026-10-23T16:00:00Z");
        for (cron, expected) in [
            ("0 0 8 * * *", "2026-10-24T00:00:00Z"),
            ("0 0 12 * * *", "2026-10-24T04:00:00Z"),
            ("0 0 * * * *", "2026-10-23T17:00:00Z"),
            ("0 0 9 * * 1", "2026-10-26T01:00:00Z"),
            ("0 0 0 1 * *", "2026-10-31T16:00:00Z"),
        ] {
            assert_eq!(
                next_occurrences(cron, "Asia/Shanghai", start, 1).unwrap(),
                vec![instant(expected)]
            );
        }
        assert_eq!(
            next_occurrences("0 0 8 * * *", "UTC", start, 1).unwrap(),
            vec![instant("2026-10-24T08:00:00Z")]
        );
    }
    #[test]
    fn daylight_saving_keeps_local_hour_with_changed_offset() {
        let next = next_occurrences(
            "0 0 8 * * *",
            "America/New_York",
            instant("2026-03-07T00:00:00Z"),
            3,
        )
        .unwrap();
        assert_eq!(
            next,
            vec![
                instant("2026-03-07T13:00:00Z"),
                instant("2026-03-08T12:00:00Z"),
                instant("2026-03-09T12:00:00Z")
            ]
        );
        assert!(
            next_occurrences("0 0 8 * * *", "bogus", instant("2026-03-07T00:00:00Z"), 1).is_err()
        );
    }
}
