use crate::db::Database;
use crate::error::Result;

pub use crate::telemetry::RuntimeLogEvent as LogRecord;

/// 获取最近 N 条日志（从内存缓存读取，不查询数据库）。
pub async fn get_logs(_db: &Database, limit: u64) -> Result<Vec<LogRecord>> {
    Ok(crate::telemetry::get_recent_logs(limit as usize))
}
