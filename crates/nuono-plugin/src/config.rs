//! 供插件 handler 使用的类型化配置辅助。

use crate::{PluginConfigField, PluginContext, Result, Value};
pub use nuono_app::config_schema::{ConfigurationMigration, ConfigurationTarget};
use serde::de::DeserializeOwned;
use std::{
    any::Any,
    marker::PhantomData,
    sync::{Arc, RwLock},
};

mod updates;
pub use updates::{PluginConfigSnapshot, PluginConfigUpdatePort, PluginConfigUpdates};

mod encoded_lists;
#[doc(hidden)]
pub use encoded_lists::{decode_json_string_lists, normalize_json_string_lists};

/// 配置字段、默认值与解码，通常由 `#[derive(ConfigSchema)]` 生成。
///
/// derive 同时保留固有的 `get_schema`、`default_value` 与 `from_value` 方法。
/// 派生字段默认以从 0 起的声明位置作为 UI 顺序；
/// 显式 `#[config(order = ...)]` 会覆盖该字段位置。schema
/// 向量本身仍保留声明顺序，即使存在覆盖也是如此。
pub trait ConfigSchema: DeserializeOwned {
    fn get_schema() -> Vec<PluginConfigField>;
    fn default_value() -> Value {
        crate::config_validation::config_defaults(&Self::get_schema())
    }
    /// 把可接受输入规整为存储 schema 声明的类型。
    ///
    /// 带显式编码列表的 derive 在字段校验前调用它。
    /// 默认实现兼容既有 schema 与手写实现。
    fn normalize_value(value: Value) -> Result<Value> {
        Ok(value)
    }
    /// 把已校验的存储值解码为配置的 Rust 字段类型。
    ///
    /// 默认使用普通 Serde 解码。derive 只适配带
    /// 显式编码的字段；纯字符串保持原意。
    fn decode_value(value: Value) -> Result<Self> {
        crate::serde_json::from_value(value).map_err(|e| crate::anyhow!("配置解析失败: {e}"))
    }
    fn from_value(value: &Value) -> Result<Self> {
        let fields = Self::get_schema();
        crate::config_validation::validate_config_fields(&fields).map_err(anyhow::Error::msg)?;
        let incoming = value
            .as_object()
            .ok_or_else(|| crate::anyhow!("配置必须是 JSON 对象"))?;
        let mut merged = crate::config_validation::config_defaults(&fields);
        for (key, value) in incoming {
            merged[key] = value.clone();
        }
        let merged = Self::normalize_value(merged)?;
        for field in &fields {
            crate::config_validation::validate_config_field_value(
                &field.key,
                &merged[&field.key],
                field,
            )
            .map_err(anyhow::Error::msg)?;
        }
        Self::decode_value(merged)
    }
}

/// 从宿主最新的有效快照读取类型化配置。
///
/// 存入 `Arc` 字段供命令状态注入。在 `on_load` 期间调用 [`Self::bind`]；
/// 每次 [`Self::get`] 解码一份一致快照。绑定前使用
/// 配置类型声明的默认值。派生客户端仍需实现 `on_config_changed`。
/// [`Self::snapshot`] 还会在宿主发布新快照前共享一份已解码值。
///
/// ```
/// use nuono_plugin::{ConfigSchema, LiveConfig, serde::Deserialize};
///
/// #[derive(Deserialize, ConfigSchema)]
/// #[serde(crate = "nuono_plugin::serde")]
/// struct MyConfig {
///     #[config(default = "pong")]
///     greeting: String,
/// }
/// let config = LiveConfig::<MyConfig>::default();
/// assert_eq!(config.get().unwrap().greeting, "pong");
/// ```
pub struct LiveConfig<T> {
    state: RwLock<LiveConfigState>,
    marker: PhantomData<fn() -> T>,
}

impl<T> std::fmt::Debug for LiveConfig<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveConfig").finish_non_exhaustive()
    }
}

struct LiveConfigState {
    handle: PluginConfigHandle,
    cached: Option<TypedSnapshot>,
}

struct TypedSnapshot {
    source: Arc<Value>,
    // 保持 LiveConfig<T> 既有的 get() Send/Sync 行为，即使
    // 拥有的 T 非 Send。只有 snapshot() 要求 T: Send + Sync。
    value: Arc<dyn Any + Send + Sync>,
}

impl LiveConfigState {
    fn cached<T: Send + Sync + 'static>(&self, source: &Arc<Value>) -> Option<Arc<T>> {
        self.cached
            .as_ref()
            .filter(|cached| Arc::ptr_eq(&cached.source, source))
            .map(|cached| {
                cached
                    .value
                    .clone()
                    .downcast::<T>()
                    .expect("a LiveConfig only caches its own configuration type")
            })
    }
}

impl<T: ConfigSchema> Default for LiveConfig<T> {
    fn default() -> Self {
        Self {
            state: RwLock::new(LiveConfigState {
                handle: PluginConfigHandle::new(T::default_value()),
                cached: None,
            }),
            marker: PhantomData,
        }
    }
}

impl<T: ConfigSchema> LiveConfig<T> {
    /// 校验并绑定宿主句柄，不复制其当前配置。
    pub fn bind(&self, ctx: &PluginContext) -> Result<()> {
        self.bind_handle(ctx.config_handle.clone())
    }

    /// 绑定共享句柄，便于测试或已有配置句柄的插件。
    /// 候选配置无效时保留原句柄。
    pub fn bind_handle(&self, handle: PluginConfigHandle) -> Result<()> {
        T::from_value(handle.snapshot().as_ref())?;
        let previous = {
            let mut state = self.state.write().expect("锁已中毒");
            std::mem::replace(
                &mut *state,
                LiveConfigState {
                    handle,
                    cached: None,
                },
            )
        };
        // 自定义配置可能实现 Drop；在锁外执行。
        drop(previous);
        Ok(())
    }

    /// 解码一份当前快照；解析错误返回给 handler。
    pub fn get(&self) -> Result<T> {
        let snapshot = self.state.read().expect("锁已中毒").handle.snapshot();
        T::from_value(snapshot.as_ref())
    }

    /// 在源快照变化前共享一份已解码配置。
    ///
    /// 读取与 get() 相同的宿主已发布值，无文件系统 I/O。
    /// 缓存未命中则校验并解码；未变快照复用 `Arc<T>`。
    /// 返回的快照在更新后仍有效，应视为不可变。
    /// 无效值返回错误，而不是回退到更旧的缓存值。
    ///
    /// 当 schema/默认值/解码只依赖已发布配置时使用。
    /// 会查阅变化中外部状态的自定义实现应继续使用 get()。
    pub fn snapshot(&self) -> Result<Arc<T>>
    where
        T: Send + Sync + 'static,
    {
        {
            let state = self.state.read().expect("锁已中毒");
            let source = state.handle.snapshot();
            if let Some(value) = state.cached(&source) {
                return Ok(value);
            }
        }
        // 拿到写锁后再检查：其他读者可能已填入
        // 缓存，或 bind_handle 已替换配置源。
        let mut state = self.state.write().expect("锁已中毒");
        let source = state.handle.snapshot();
        if let Some(value) = state.cached(&source) {
            return Ok(value);
        }
        let value = Arc::new(T::from_value(source.as_ref())?);
        let previous = state.cached.replace(TypedSnapshot {
            source,
            value: value.clone(),
        });
        drop(state);
        drop(previous);
        Ok(value)
    }
}

#[cfg(test)]
#[path = "../tests/unit/config/live_config.rs"]
mod tests;

#[derive(Clone, Debug)]
pub struct RuntimePluginConfiguration {
    pub values: Arc<Value>,
    pub enabled: bool,
}

tokio::task_local! {
    static RUNTIME_CONFIGURATIONS: std::cell::RefCell<std::collections::HashMap<String, RuntimePluginConfiguration>>;
}

/// 由宿主为单次事件注入的只读插件配置作用域（并发事件之间互不共享）。
pub async fn with_runtime_configurations<F: std::future::Future>(
    values: std::collections::HashMap<String, RuntimePluginConfiguration>,
    future: F,
) -> F::Output {
    RUNTIME_CONFIGURATIONS
        .scope(std::cell::RefCell::new(values), future)
        .await
}

pub fn runtime_configuration(plugin_id: &str) -> Option<RuntimePluginConfiguration> {
    RUNTIME_CONFIGURATIONS
        .try_with(|values| values.borrow().get(plugin_id).cloned())
        .ok()
        .flatten()
}

pub(crate) fn capture_runtime_configurations()
-> Option<std::collections::HashMap<String, RuntimePluginConfiguration>> {
    RUNTIME_CONFIGURATIONS
        .try_with(|values| values.borrow().clone())
        .ok()
}

pub(crate) async fn inherit_runtime_configurations<F: std::future::Future>(
    values: Option<std::collections::HashMap<String, RuntimePluginConfiguration>>,
    future: F,
) -> F::Output {
    match values {
        Some(values) => with_runtime_configurations(values, future).await,
        None => future.await,
    }
}

#[derive(Clone, Debug)]
pub struct PluginConfigHandle {
    value: Arc<RwLock<Arc<Value>>>,
    plugin_id: Option<Arc<str>>,
}
impl PluginConfigHandle {
    pub fn new(value: Value) -> Self {
        Self {
            value: Arc::new(RwLock::new(Arc::new(value))),
            plugin_id: None,
        }
    }
    pub fn for_plugin(plugin_id: &str, value: Value) -> Self {
        Self {
            plugin_id: Some(Arc::from(plugin_id)),
            ..Self::new(value)
        }
    }
    /// 每次处理先取得一次快照；已取得的快照不会随热更新变化。
    pub fn snapshot(&self) -> Arc<Value> {
        if let Some(plugin) = self.plugin_id.as_deref()
            && let Some(configuration) = runtime_configuration(plugin)
        {
            return configuration.values;
        }
        self.global_snapshot()
    }
    /// 宿主持久化读取始终直接访问全局配置快照，不受当前事件局部作用域覆盖的影响。
    pub fn global_snapshot(&self) -> Arc<Value> {
        self.value.read().expect("锁已中毒").clone()
    }
    pub fn publish(&self, value: Value) {
        *self.value.write().expect("锁已中毒") = Arc::new(value);
    }
}

/// 宿主在校验完按钮交互令牌后，用经过认证的原始会话配置替换合成交互事件上的临时配置。
pub fn replace_runtime_configurations(
    values: std::collections::HashMap<String, RuntimePluginConfiguration>,
) -> bool {
    RUNTIME_CONFIGURATIONS
        .try_with(|current| *current.borrow_mut() = values)
        .is_ok()
}
pub(crate) fn inherit_blocking_configurations<T>(
    values: Option<std::collections::HashMap<String, RuntimePluginConfiguration>>,
    job: impl FnOnce() -> T,
) -> T {
    match values {
        Some(values) => RUNTIME_CONFIGURATIONS.sync_scope(std::cell::RefCell::new(values), job),
        None => job(),
    }
}
