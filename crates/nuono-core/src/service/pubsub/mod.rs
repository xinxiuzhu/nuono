//! 事件发布/订阅（topic 推送）相关服务。
//!
//! 单独划出这个命名空间，是因为顶层的 `subscriptions` 一词已经被两种领域争用：这里的事件
//! 订阅，以及别的域里同名的「订阅」概念。放进 `pubsub` 后，调用方从
//! 路径本身就能看出面对的是哪一个领域，不必靠上下文猜。
//!
//! 这里不做 `pub use subscriptions::*` 的再导出：`Subscription`、`list`、`create` 这些名字太
//! 通用，摊平到 `pubsub::` 一层会把刚划开的边界重新糊上，调用方请写全 `pubsub::subscriptions::`。

pub mod subscriptions;
