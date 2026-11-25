//! 外部消息平台适配器的生命周期抽象。
//!
//! `AdapterManager` 只管理已由上层构造并注册的适配器；凭据读取、平台选择和事件管线
//! 接线留给可执行程序完成，以避免核心库隐式产生网络副作用。

pub mod manager;
pub mod supervisor;
pub mod types;

pub use manager::AdapterManager;
pub use supervisor::{BackoffPolicy, supervise};
pub use types::{AdapterConfig, AdapterInfo, AdapterStatus};
