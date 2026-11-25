use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use obscura_browser::page::PendingNavigationOutcome;
use obscura_browser::{BrowserContext, Page, WaitUntil};
use obscura_js::runtime::{ObscuraJsRuntime, spawn_watchdog};
use serde_json::Value;

use super::{BrowserContent, BrowserPage, BrowserRequest, BrowserWaitUntil};

static INITIALIZED: OnceLock<()> = OnceLock::new();

/// 宿主在派发任何插件任务前，先在主线程上调用一次此函数。
/// 上游 V8 要求首个 Isolate 必须在主线程完成构造；仅靠内部的 Isolate 构造互斥锁无法保证在工作线程首次初始化的安全性。
pub(super) fn initialize() -> Result<()> {
    INITIALIZED.get_or_init(|| drop(ObscuraJsRuntime::new()));
    Ok(())
}

pub(super) fn is_initialized() -> bool {
    INITIALIZED.get().is_some()
}

/// 在 SDK 受配额限制的阻塞工作线程内部调用。
/// 任何 V8 对象都不会跨出当前线程，且线程内局部的 Tokio 运行时生命周期覆盖整个 `Page` 与网络客户端。
pub(super) fn fetch(
    request: BrowserRequest,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> Result<BrowserPage> {
    ensure!(is_initialized(), "browser engine has not been initialized");
    remaining(deadline, &cancelled)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not create browser runtime")?;
    // 上游库在 debug 日志级别下会打印完整的 URL 和代理凭据。
    // 因此在当前工作线程内局部禁用 tracing 订阅器，仅由 SDK 返回明确阶段的错误描述，避免在日志中回显页面脚本或连接密钥。
    tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
        runtime.block_on(fetch_page(request, deadline, &cancelled))
    })
}

async fn fetch_page(
    request: BrowserRequest,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> Result<BrowserPage> {
    let context = Arc::new(BrowserContext::with_storage_and_network(
        "nuono-sdk".to_owned(),
        request.proxy,
        false,
        request.user_agent,
        None,
        request.allow_private_network,
    ));
    let url = url::Url::parse(&request.url).context("invalid browser URL")?;
    for cookie in &request.cookies {
        context.cookie_jar.set_cookie(cookie, &url);
    }

    let mut page = Page::new("nuono-sdk-page".to_owned(), context);
    page.set_navigation_timeout(remaining(deadline, cancelled)?);
    let wait_until = match request.wait_until {
        BrowserWaitUntil::Load => WaitUntil::Load,
        BrowserWaitUntil::DomContentLoaded => WaitUntil::DomContentLoaded,
        BrowserWaitUntil::NetworkIdle0 => WaitUntil::NetworkIdle0,
    };
    // 页面导航会在内部创建或替换 V8 Isolate 实例；其自带的看门狗（watchdog）会限制同步脚本执行时长，
    // 但底层导航仍可能略晚于外层异步截止时间结束。调用方的工作线程会一直持有并发配额直至本函数真正返回。
    cancellable(
        page.navigate_with_wait(&request.url, wait_until),
        deadline,
        cancelled,
    )
    .await?
    .map_err(|_| anyhow::anyhow!("browser navigation failed"))?;

    finish_navigation(&mut page, request.settle, true, deadline, cancelled).await?;

    // 执行一次调用方传入的 JS 表达式并保存返回值；若该表达式触发了页面跳转，
    // 则在读取最终文档标题与正文前先跟随完成跳转，但不会在新页面上重复执行该表达式。
    let evaluation = if let Some(source) = &request.evaluation {
        let source = serde_json::to_string(source)?;
        let expression = format!(
            "(() => {{ const value = (0, eval)({source}); \
             if (value && typeof value.then === 'function') \
             throw new Error('async evaluation is unsupported'); \
             return value; }})()"
        );
        let value = evaluate_json(
            &mut page,
            &expression,
            request.max_output_bytes,
            deadline,
            cancelled,
        )?;
        finish_navigation(&mut page, request.settle, false, deadline, cancelled).await?;
        Some(value)
    } else {
        None
    };

    let content = match request.content {
        BrowserContent::Html => "document.documentElement?.outerHTML ?? ''",
        BrowserContent::Text => "document.body?.innerText ?? document.body?.textContent ?? ''",
        BrowserContent::None => "''",
    };
    let expression =
        format!("({{title: String(document.title ?? ''), content: String({content})}})");
    let data = evaluate_json(
        &mut page,
        &expression,
        request.max_output_bytes,
        deadline,
        cancelled,
    )?;
    let title = data["title"]
        .as_str()
        .context("browser returned an invalid title")?
        .to_owned();
    let content = data["content"]
        .as_str()
        .context("browser returned invalid content")?
        .to_owned();
    let result = BrowserPage {
        url: page.url_string(),
        title,
        content,
        evaluation,
    };
    let evaluation_bytes = result
        .evaluation
        .as_ref()
        .map(serde_json::to_vec)
        .transpose()?
        .map_or(0, |value| value.len());
    let output_bytes = result
        .url
        .len()
        .saturating_add(result.title.len())
        .saturating_add(result.content.len())
        .saturating_add(evaluation_bytes);
    ensure!(
        output_bytes <= request.max_output_bytes,
        "browser output exceeds max_output_bytes"
    );
    remaining(deadline, cancelled)?;
    Ok(result)
}

/// 对每个新加载的文档推进事件循环（`settle`），跟随脚本或表单触发的延迟跳转，
/// 并将 `history.pushState` / `replaceState` 的虚拟地址同步到页面的当前 URL。
/// 每次后续跳转同样受初始请求的截止时间与取消状态约束。
async fn finish_navigation(
    page: &mut Page,
    settle: Duration,
    mut settle_current: bool,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> Result<()> {
    const MAX_DEFERRED_NAVIGATIONS: usize = 5;
    for followed in 0..=MAX_DEFERRED_NAVIGATIONS {
        if settle_current && !settle.is_zero() {
            let budget = remaining(deadline, cancelled)?;
            let watchdog = page
                .isolate_handle()
                .map(|handle| spawn_watchdog(handle, budget));
            let settled =
                cancellable(page.settle(settle.as_millis() as u64), deadline, cancelled).await;
            let fired = watchdog.is_some_and(|token| token.stop());
            if fired {
                page.cancel_v8_termination();
                bail!("browser request timed out");
            }
            settled?;
        }
        ensure!(
            followed < MAX_DEFERRED_NAVIGATIONS || !page.has_pending_navigation(),
            "browser exceeded deferred navigation limit"
        );
        let budget = remaining(deadline, cancelled)?;
        page.set_navigation_timeout(budget);
        // `sync_virtual_url` 内部也会执行页面 JS，因此即使是同文档（same-document）分支也需要挂载看门狗；
        // 跨文档跳转则会替换 Isolate 并沿用上游自身的导航超时监管。
        let watchdog = page
            .isolate_handle()
            .map(|handle| spawn_watchdog(handle, budget));
        let outcome = cancellable(
            page.process_pending_navigation_outcome(),
            deadline,
            cancelled,
        )
        .await;
        let fired = watchdog.is_some_and(|token| token.stop());
        if fired {
            page.cancel_v8_termination();
            bail!("browser request timed out");
        }
        let outcome =
            outcome?.map_err(|_| anyhow::anyhow!("browser deferred navigation failed"))?;
        if outcome != PendingNavigationOutcome::Document {
            return Ok(());
        }
        settle_current = true;
    }
    bail!("browser exceeded deferred navigation limit")
}

fn evaluate_json(
    page: &mut Page,
    expression: &str,
    limit: usize,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> Result<Value> {
    let budget = remaining(deadline, cancelled)?;
    // JSON 转义最多可能把 1 个字节膨胀为 6 个字节（例如 `\u0000`）。
    // 这里放宽中间 JSON 字符串的长度上限，避免仅因标题或正文含转义字符就误拒合法的 UTF-8 内容；
    // 最终返回给调用方的 `BrowserPage` 各字段总字节数会在外层再做一次精确预算检查。
    let encoded_limit = limit.saturating_mul(6).saturating_add(128);
    // 上游会将普通 `eval` 抛出的异常静默转为 `null`，且其默认的对象序列化发生在撤销看门狗之后。
    // 因此这里在脚本内部直接调用 `JSON.stringify`，确保自定义的 getter / `toJSON` 同样受看门狗超时约束；
    // 同时绝不把页面脚本抛出的异常原文拼入宿主错误消息中，以免泄露页面内的凭据、Cookie 或私有 URL 参数。
    let script = format!(
        r#"(() => {{
            try {{
                const value = ({expression});
                const json = JSON.stringify(value) ?? 'null';
                if (json.length > {encoded_limit}) return '{{"ok":false,"limit":true}}';
                return '{{"ok":true,"value":' + json + '}}';
            }} catch (_) {{ return '{{"ok":false}}'; }}
        }})()"#
    );
    let value = page
        .js
        .as_mut()
        .context("browser JavaScript runtime is unavailable")?
        .evaluate_with_timeout(&script, budget)
        .map_err(|_| anyhow::anyhow!("browser JavaScript evaluation failed or timed out"))?;
    remaining(deadline, cancelled)?;
    let encoded = value
        .as_str()
        .context("browser returned an invalid evaluation result")?;
    // 外层封套只会增加固定长度的 JSON 包装字符；在反序列化前先校验 UTF-8 字节数。
    ensure!(
        encoded.len() <= encoded_limit.saturating_add(32),
        "browser output exceeds max_output_bytes"
    );
    let envelope: Value = serde_json::from_str(encoded)
        .map_err(|_| anyhow::anyhow!("browser returned invalid JSON"))?;
    if envelope["limit"].as_bool() == Some(true) {
        bail!("browser output exceeds max_output_bytes");
    }
    ensure!(
        envelope["ok"].as_bool() == Some(true),
        "browser JavaScript evaluation failed"
    );
    envelope
        .get("value")
        .cloned()
        .context("browser returned an incomplete evaluation result")
}

fn remaining(deadline: Instant, cancelled: &impl Fn() -> bool) -> Result<Duration> {
    ensure!(!cancelled(), "browser request was cancelled");
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .context("browser request timed out")
}

async fn cancellable<F: Future>(
    future: F,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> Result<F::Output> {
    tokio::pin!(future);
    loop {
        let delay = remaining(deadline, cancelled)?.min(Duration::from_millis(25));
        tokio::select! {
            output = &mut future => {
                remaining(deadline, cancelled)?;
                return Ok(output);
            }
            _ = tokio::time::sleep(delay) => {}
        }
    }
}
