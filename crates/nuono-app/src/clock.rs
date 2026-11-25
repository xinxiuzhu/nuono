//! 应用层统一 UTC 时钟以及展示层/传输层的时间戳转换工具。
//! 数据库持久化层通过时钟适配器统一存储有符号 UTC 毫秒时间戳。

use chrono::{DateTime, Local, TimeZone, Utc};

/// 获取当前 UTC 时间（存储层保留毫秒级精度）。
pub fn db_now() -> DateTime<Utc> {
    Utc::now()
}

/// 数据库时间戳的展示格式（本地时区 `%Y-%m-%d %H:%M:%S`），仅用于日志 / 面板展示。
pub fn format_db_ts(dt: impl Into<DateTime<Utc>>) -> String {
    dt.into()
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// 生成与服务器本地时区无关的无损 API 毫秒时间戳字符串。
pub fn format_wire_ts(value: impl Into<DateTime<Utc>>) -> String {
    value.into().timestamp_millis().to_string()
}

/// 解析毫秒时间戳或历史约定的 `%Y-%m-%d %H:%M:%S` 字符串（视为本地时区），供 API 输入校验与解析使用。
pub fn parse_db_ts(value: &str) -> Option<DateTime<Utc>> {
    if let Ok(milliseconds) = value.parse::<i64>() {
        return DateTime::from_timestamp_millis(milliseconds);
    }
    let naive = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S").ok()?;
    Local
        .from_local_datetime(&naive)
        .single()
        .map(|dt| dt.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_now_is_utc_and_formattable() {
        let now = db_now();
        assert_eq!(now.timezone(), Utc);
        // 展示格式仍保持历史约定的 `%Y-%m-%d %H:%M:%S` 定宽。
        assert_eq!(format_db_ts(now).len(), "YYYY-MM-DD HH:MM:SS".len());
    }
}
