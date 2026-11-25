//! 应用层与持久化层时钟类型重导出（应用层使用 chrono 时间点，ORM 持久化层统一使用毫秒时间戳）。
pub use nuono_app::clock::*;
pub use nuono_storage::clock::{UtcMillis, load_time, store_time};
