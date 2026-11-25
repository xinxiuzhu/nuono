//! 面向源码编译插件的可选进程内 Obscura 浏览器引擎。
//!
//! 启用 `nuono-plugin` 的 `obscura` feature 即可链接该引擎。Nuono 宿主会在主线程完成 V8 初始化；
//! 独立运行的二进制或测试程序在发起浏览器任务前，必须在主线程调用一次 [`initialize`]。
//! 每次页面抓取都会创建独立的浏览器上下文；构建要求与取消语义的限制详见 `docs/plugins/OBSCURA.md`。

use std::{sync::Arc, time::Duration};
#[cfg(any(feature = "obscura", test))]
use std::{
    sync::{
        LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use anyhow::{Result, anyhow, ensure};
#[cfg(any(feature = "obscura", test))]
use tokio::sync::{Semaphore, oneshot};

use crate::PluginTaskScope;

#[cfg(feature = "obscura")]
mod backend;

/// 当前编译产物是否已启用并链接了 Obscura 引擎（此函数不会触发 V8 初始化）。
pub const fn is_available() -> bool {
    cfg!(feature = "obscura")
}

/// 在发起任何浏览器任务前，必须在进程主线程上调用一次以预热初始化 V8。
///
/// Nuono 宿主在启动异步运行时前会自动调用此函数；未启用 `obscura` feature 时为空操作。
/// 切勿在插件回调或工作线程中首次调用——上游 V8 引擎要求首次初始化必须发生在主线程。
pub fn initialize() -> Result<()> {
    #[cfg(feature = "obscura")]
    backend::initialize()?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BrowserWaitUntil {
    Load,
    DomContentLoaded,
    NetworkIdle0,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BrowserContent {
    Html,
    Text,
    None,
}

/// 单次相互隔离的页面抓取请求（Cookie 绝不会在多次请求之间共享）。
///
/// 为防止日志泄露敏感信息，本类型的 `Debug` 输出会对 URL、Cookie、代理凭据与 JS 表达式做脱敏处理。
#[derive(Clone)]
pub struct BrowserRequest {
    pub url: String,
    pub wait_until: BrowserWaitUntil,
    /// 调用方的总截止时间（含排队等待时间），取值范围为 `0 < timeout <= 120s`。
    /// 注意：底层的同步页面导航可能会略微超出该截止时间；工作线程在实际退出前会一直占用并发配额，插件停用关闭时也会等待其彻底退出。
    pub timeout: Duration,
    /// 页面导航完成后额外留给 JS 事件循环稳定 DOM 的等待时间（上限 10 秒）。
    pub settle: Duration,
    pub content: BrowserContent,
    /// 需要同步求值的 JavaScript 表达式，其序列化后的 JSON 结果会单独存放在响应的 `evaluation` 字段中。
    /// 若页面存在异步 Promise，必须在导航或 `settle` 阶段内完成并将结果写入 DOM/全局变量后再读取。
    pub evaluation: Option<String>,
    pub user_agent: Option<String>,
    pub proxy: Option<String>,
    /// 在导航前针对目标 `url` 预先写入的 `Set-Cookie` 字符串列表。
    /// 采用 Obscura 内部的 Cookie Jar 管理域名/路径作用域与重定向，而非简单拼接全局 `Cookie` 请求头。
    pub cookies: Vec<String>,
    /// 是否允许请求本机或内网地址（仅供本地开发调试使用）。
    /// 上游引擎同样会检查进程级的 `OBSCURA_ALLOW_PRIVATE_NETWORK` 环境变量。
    pub allow_private_network: bool,
    /// 返回结果的总 UTF-8 字节数上限（含最终 URL、页面标题、正文内容与序列化后的 JS 表达式返回值），最大 8 MiB。
    /// 超出该上限会直接报错拒绝，绝不静默截断。
    pub max_output_bytes: usize,
}

impl std::fmt::Debug for BrowserRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserRequest")
            .field("url", &"[REDACTED]")
            .field("wait_until", &self.wait_until)
            .field("timeout", &self.timeout)
            .field("settle", &self.settle)
            .field("content", &self.content)
            .field("has_evaluation", &self.evaluation.is_some())
            .field("has_user_agent", &self.user_agent.is_some())
            .field("has_proxy", &self.proxy.is_some())
            .field("cookie_count", &self.cookies.len())
            .field("allow_private_network", &self.allow_private_network)
            .field("max_output_bytes", &self.max_output_bytes)
            .finish()
    }
}

impl BrowserRequest {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            wait_until: BrowserWaitUntil::Load,
            timeout: Duration::from_secs(30),
            settle: Duration::from_millis(250),
            content: BrowserContent::Html,
            evaluation: None,
            user_agent: None,
            proxy: None,
            cookies: Vec::new(),
            allow_private_network: false,
            max_output_bytes: 2 * 1024 * 1024,
        }
    }

    fn validate(&self) -> Result<()> {
        let url = url::Url::parse(&self.url).map_err(|_| anyhow!("浏览器 URL 无效"))?;
        ensure!(
            matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
            "浏览器只接受 HTTP/HTTPS URL"
        );
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "浏览器 URL 不能包含凭据，请使用 cookies"
        );
        ensure!(
            !self.timeout.is_zero() && self.timeout <= Duration::from_secs(120),
            "浏览器超时必须大于 0 且不超过 120 秒"
        );
        ensure!(
            self.settle <= Duration::from_secs(10),
            "浏览器等待时间不能超过 10 秒"
        );
        ensure!(
            (1..=8 * 1024 * 1024).contains(&self.max_output_bytes),
            "浏览器结果预算必须为 1 到 8388608 字节"
        );
        if let Some(proxy) = &self.proxy {
            let proxy = url::Url::parse(proxy).map_err(|_| anyhow!("浏览器代理 URL 无效"))?;
            ensure!(
                matches!(proxy.scheme(), "http" | "https" | "socks5" | "socks5h")
                    && proxy.host_str().is_some(),
                "浏览器代理协议必须是 HTTP、HTTPS 或 SOCKS5"
            );
        }
        ensure!(
            self.cookies.len() <= 64,
            "浏览器初始 cookie 数量不能超过 64"
        );
        for cookie in &self.cookies {
            let pair = cookie.split(';').next().unwrap_or_default();
            ensure!(
                cookie.len() <= 8192
                    && !cookie.contains(['\r', '\n', '\0'])
                    && pair
                        .split_once('=')
                        .is_some_and(|(name, _)| !name.trim().is_empty()),
                "浏览器初始 cookie 必须为有效的 Set-Cookie 字符串"
            );
        }
        ensure!(
            self.url.len() <= 16 * 1024
                && self
                    .evaluation
                    .as_ref()
                    .is_none_or(|s| s.len() <= 64 * 1024)
                && self
                    .user_agent
                    .as_ref()
                    .is_none_or(|s| s.len() <= 8192 && !s.contains(['\r', '\n', '\0']))
                && self.proxy.as_ref().is_none_or(|s| s.len() <= 8192),
            "浏览器请求参数过大或包含无效字符"
        );
        Ok(())
    }
}

/// 渲染后的页面 DOM 内容与可选的 JS 表达式求值结果（不向外部暴露底层 V8 对象）。
#[derive(Debug, Clone)]
pub struct BrowserPage {
    pub url: String,
    pub title: String,
    pub content: String,
    pub evaluation: Option<serde_json::Value>,
}

/// 绑定到插件任务作用域的可克隆浏览器句柄。
#[derive(Clone)]
pub struct PluginBrowser {
    scope: Arc<PluginTaskScope>,
}

impl std::fmt::Debug for PluginBrowser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginBrowser")
            .field("plugin", &self.scope.plugin_name())
            .finish_non_exhaustive()
    }
}

impl PluginBrowser {
    /// 通常通过 `ctx.browser()` 获取；独立测试也可自行传入 `PluginTaskScope`，并在测试运行时结束前调用 `scope.shutdown().await`。
    pub fn for_scope(scope: Arc<PluginTaskScope>) -> Self {
        Self { scope }
    }

    pub async fn fetch(&self, request: BrowserRequest) -> Result<BrowserPage> {
        request.validate()?;
        ensure!(!self.scope.is_closed(), "插件已停用，不能抓取页面");
        #[cfg(not(feature = "obscura"))]
        return Err(anyhow!(
            "未链接 Obscura，请为 nuono-plugin 启用 obscura feature 并重新构建应用"
        ));
        #[cfg(feature = "obscura")]
        {
            ensure!(
                backend::is_initialized(),
                "Obscura 尚未初始化，独立程序需在主线程先调用 browser::initialize()"
            );
            let deadline = Instant::now() + request.timeout;
            self.run(deadline, move |cancelled| {
                backend::fetch(request, deadline, cancelled)
            })
            .await
        }
    }

    // 一旦阻塞任务闭包被派发到工作线程，准入与执行配额（permit）就移交由该工作线程持有，
    // 确保即便调用方因超时或取消提前放弃等待，仍在后台运行的 V8 工作线程也不会脱离并发配额监管。
    #[cfg(any(feature = "obscura", test))]
    async fn run<T, F>(&self, deadline: Instant, job: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Box<dyn Fn() -> bool + Send>) -> Result<T> + Send + 'static,
    {
        let pool = &*BROWSER_POOL;
        let admitted = pool
            .admitted
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow!("浏览器任务较多，请稍后重试"))?;
        let slot = tokio::select! {
            biased;
            _ = self.scope.cancelled() => return Err(anyhow!("插件已停用，页面抓取已取消")),
            slot = tokio::time::timeout_at(deadline.into(), pool.slots.clone().acquire_owned()) =>
                slot.map_err(|_| anyhow!("页面抓取等待超时"))??,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelOnDrop(cancel.clone());
        let scope = Arc::downgrade(&self.scope);
        let (sender, receiver) = oneshot::channel();
        self.scope.spawn_browser_job(move || {
            let (_admitted, _slot) = (admitted, slot);
            let cancelled = Box::new(move || {
                cancel.load(Ordering::Acquire)
                    || scope.upgrade().is_none_or(|scope| scope.is_closed())
            });
            if sender.is_closed() || cancelled() {
                return;
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(cancelled)))
                .unwrap_or_else(|_| Err(anyhow!("浏览器工作线程发生 panic")));
            let _ = sender.send(result);
        })?;
        tokio::select! {
            biased;
            _ = self.scope.cancelled() => Err(anyhow!("插件已停用，页面抓取已取消")),
            result = tokio::time::timeout_at(deadline.into(), receiver) =>
                result.map_err(|_| anyhow!("页面抓取超时"))?
                    .map_err(|_| anyhow!("浏览器工作线程已退出"))?,
        }
    }
}

// 与 CPU 密集型图片渲染池相互独立：所有插件全局共享 1 个并发 V8 执行槽位与最多 8 个排队等待名额；
// 超出容量的请求立即快速失败，绝不无限制派生工作线程。
#[cfg(any(feature = "obscura", test))]
struct BrowserPool {
    slots: Arc<Semaphore>,
    admitted: Arc<Semaphore>,
}
#[cfg(any(feature = "obscura", test))]
static BROWSER_POOL: LazyLock<BrowserPool> = LazyLock::new(|| BrowserPool {
    slots: Arc::new(Semaphore::new(1)),
    admitted: Arc::new(Semaphore::new(9)),
});

#[cfg(any(feature = "obscura", test))]
struct CancelOnDrop(Arc<AtomicBool>);
#[cfg(any(feature = "obscura", test))]
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[cfg(test)]
#[path = "../tests/unit/browser/tests.rs"]
mod tests;
