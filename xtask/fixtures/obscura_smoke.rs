//! 真实 V8/无头浏览器冒烟测试桩。所有页面均通过本地回环地址提供，无需访问外网或启动 Nuono 宿主/数据库。
//!
//! 需以 `harness = false` 模式运行：必须在主线程创建 Tokio 工作线程之前同步完成 V8 初始化。
use anyhow::{Context, Result, ensure};
use axum::{Router, extract::State, response::Html, routing::get};
use nuono_plugin::{
    PluginTaskScope,
    browser::{self, BrowserContent, BrowserRequest, BrowserWaitUntil, PluginBrowser},
    serde_json::json,
};
use std::{sync::Arc, time::Duration};
use tokio::{sync::Notify, time::timeout};

const PAGE: &str = r#"<!doctype html><html><head><title>initial</title></head>
<body><strong id="result">initial</strong><span id="timer">waiting</span>
<script>
document.title = 'obscura smoke';
document.getElementById('result').textContent = 'executed JavaScript';
setTimeout(() => { document.getElementById('timer').textContent = 'timer fired'; }, 20);
</script></body></html>"#;

#[derive(Clone, Default)]
struct HeldResponse {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

async fn held_response(State(state): State<HeldResponse>) -> Html<&'static str> {
    state.started.notify_one();
    state.release.notified().await;
    Html("<!doctype html><html><body>released</body></html>")
}

fn request(url: impl Into<String>) -> BrowserRequest {
    let mut request = BrowserRequest::new(url);
    // 仅在当前本地测试桩中显式开启私网访问，插件默认配置不允许访问私网。
    request.allow_private_network = true;
    request.timeout = Duration::from_secs(10);
    request.settle = Duration::ZERO;
    request
}

fn main() -> Result<()> {
    browser::initialize().context("initialize Obscura before Tokio")?;
    ensure!(browser::is_available(), "obscura feature is required");
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(smoke())
}

async fn smoke() -> Result<()> {
    let held = HeldResponse::default();
    let app = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route(
            "/history",
            get(|| async {
                Html(
                    r#"<!doctype html><html><head><title>history page</title></head>
<body><p id="history">history DOM preserved</p>
<script>history.replaceState({}, '', '/resolved');</script></body></html>"#,
                )
            }),
        )
        .route(
            "/delayed-redirect",
            get(|| async {
                Html(
                    r#"<!doctype html><html><head><title>redirect origin</title></head>
<body><p>redirect origin content</p>
<script>setTimeout(() => { location.href = '/final'; }, 20);</script></body></html>"#,
                )
            }),
        )
        .route(
            "/final",
            get(|| async {
                Html(
                    r#"<!doctype html><html><head><title>redirect final</title></head>
<body><strong id="final">final DOM loaded</strong></body></html>"#,
                )
            }),
        )
        .route(
            "/cookie",
            get(|| async { Html("<!doctype html><html><body>cookies</body></html>") }),
        )
        .route(
            "/large",
            get(|| async { Html(format!("<html><body>{}</body></html>", "x".repeat(4096))) }),
        )
        .route("/hold", get(held_response))
        .with_state(held.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let scope = Arc::new(PluginTaskScope::new("obscura-smoke"));
    let browser = PluginBrowser::for_scope(scope.clone());

    let result = async {
        let mut html = request(format!("{base}/"));
        html.settle = Duration::from_millis(100);
        html.evaluation = Some(
            "({answer: 42, nested: {ok: true}, values: [1, 'two'], timer: document.getElementById('timer').textContent})".into(),
        );
        let page = browser.fetch(html).await.context("HTML and JavaScript")?;
        ensure!(page.url == format!("{base}/"), "page URL was lost");
        ensure!(page.title == "obscura smoke", "JS title mutation was lost");
        ensure!(page.content.contains("executed JavaScript"), "JS DOM mutation was lost");
        ensure!(page.content.contains("<strong"), "HTML output lost markup");
        ensure!(
            page.evaluation == Some(json!({"answer": 42, "nested": {"ok": true}, "values": [1, "two"], "timer": "timer fired"})),
            "evaluation must preserve JSON types and observe settled timers"
        );

        let mut text = request(format!("{base}/"));
        text.content = BrowserContent::Text;
        text.wait_until = BrowserWaitUntil::DomContentLoaded;
        let page = browser.fetch(text).await.context("text extraction")?;
        ensure!(page.content.contains("executed JavaScript"), "text output lost DOM content");
        ensure!(!page.content.contains("<strong"), "text output contains HTML tags");
        ensure!(page.evaluation.is_none(), "unrequested evaluation must be None");

        // 服务端并未注册 /resolved 路由：replaceState 必须只更新页面上报的 URL，
        // 而不触发网络请求替换当前 DOM 文档。
        let mut history = request(format!("{base}/history"));
        history.evaluation = Some("document.getElementById('history').textContent".into());
        let page = browser.fetch(history).await.context("same-document history URL")?;
        ensure!(page.url == format!("{base}/resolved"), "history.replaceState URL was lost");
        ensure!(page.title == "history page", "same-document history replaced the page title");
        ensure!(page.evaluation == Some(json!("history DOM preserved")), "same-document history replaced the DOM");

        let mut redirect = request(format!("{base}/delayed-redirect"));
        redirect.settle = Duration::from_millis(100);
        redirect.evaluation = Some("document.getElementById('final')?.textContent ?? null".into());
        let page = browser.fetch(redirect).await.context("navigation requested during settle")?;
        ensure!(page.url == format!("{base}/final"), "settle did not follow the delayed redirect");
        ensure!(page.title == "redirect final", "redirect returned the original page title");
        ensure!(page.evaluation == Some(json!("final DOM loaded")), "redirect returned the original DOM");
        ensure!(!page.content.contains("redirect origin content"), "redirect retained stale HTML");

        let mut only_eval = request(format!("{base}/cookie"));
        only_eval.content = BrowserContent::None;
        only_eval.wait_until = BrowserWaitUntil::NetworkIdle0;
        only_eval.evaluation = Some("null".into());
        let page = browser.fetch(only_eval).await.context("evaluation without content")?;
        ensure!(page.content.is_empty(), "None content must be empty");
        ensure!(page.evaluation == Some(json!(null)), "successful JS null is not an absent result");

        let mut throws = request(format!("{base}/cookie"));
        let mut eval_redirect = request(format!("{base}/cookie"));
        eval_redirect.evaluation = Some("location.href = '/final'; 42".into());
        let page = browser.fetch(eval_redirect).await.context("evaluation navigation")?;
        ensure!(page.url == format!("{base}/final"), "evaluation navigation URL was lost");
        ensure!(page.evaluation == Some(json!(42)), "navigation must not rerun evaluation");
        ensure!(page.content.contains("final"), "evaluation navigation returned old content");

        throws.evaluation = Some("(() => { throw new Error('smoke-evaluation-error'); })()".into());
        ensure!(browser.fetch(throws).await.is_err(), "JS exceptions must not become successful null results");

        let mut large = request(format!("{base}/large"));
        large.max_output_bytes = 1024;
        ensure!(browser.fetch(large).await.is_err(), "oversized page output must fail");

        let mut large_eval = request(format!("{base}/cookie"));
        large_eval.content = BrowserContent::None;
        large_eval.evaluation = Some("'x'.repeat(4096)".into());
        large_eval.max_output_bytes = 1024;
        ensure!(browser.fetch(large_eval).await.is_err(), "oversized evaluation output must fail");

        let mut seeded = request(format!("{base}/cookie"));
        seeded.cookies = vec!["session=seeded; Path=/".into()];
        seeded.evaluation = Some("document.cookie".into());
        let page = browser.fetch(seeded).await.context("seeded cookie")?;
        ensure!(page.evaluation == Some(json!("session=seeded")), "request cookie was not seeded");
        let mut clean = request(format!("{base}/cookie"));
        clean.evaluation = Some("document.cookie".into());
        let page = browser.fetch(clean).await.context("fresh cookie jar")?;
        ensure!(page.evaluation == Some(json!("")), "cookies leaked between fetches");

        let jobs = (0..4).map(|index| {
            let browser = &browser;
            let mut job = request(format!("{base}/cookie"));
            job.cookies = vec![format!("session=parallel-{index}; Path=/")];
            job.evaluation = Some("document.cookie".into());
            async move {
                let page = browser.fetch(job).await?;
                ensure!(page.evaluation == Some(json!(format!("session=parallel-{index}"))), "concurrent cookie jars leaked");
                Ok::<_, anyhow::Error>(())
            }
        });
        futures::future::try_join_all(jobs).await.context("concurrent requests")?;

        let mut loops = request(format!("{base}/cookie"));
        loops.timeout = Duration::from_secs(1);
        loops.evaluation = Some("(() => { for (;;) {} })()".into());
        let result = timeout(Duration::from_secs(5), browser.fetch(loops))
            .await.context("infinite evaluation failed to return by the caller deadline")?;
        ensure!(result.is_err(), "infinite evaluation must fail");

        cancellation(&base, held).await?;
        Ok::<_, anyhow::Error>(())
    }.await;

    // 在已注册的浏览器工作任务完成清理前保持本地 HTTP 服务运行。
    let shutdown = timeout(Duration::from_secs(10), scope.shutdown()).await;
    server.abort();
    let _ = server.await;
    result?;
    shutdown.context("browser workers did not leave the task scope")?;
    ensure!(
        browser.fetch(request(format!("{base}/"))).await.is_err(),
        "closed scope accepted new work"
    );
    println!("obscura smoke passed (local HTTP, real V8, no database)");
    Ok(())
}

async fn cancellation(base: &str, held: HeldResponse) -> Result<()> {
    let scope = Arc::new(PluginTaskScope::new("obscura-smoke-cancellation"));
    let browser = PluginBrowser::for_scope(scope.clone());
    let caller_browser = browser.clone();
    let held_request = request(format!("{base}/hold"));
    let caller = tokio::spawn(async move { caller_browser.fetch(held_request).await });
    timeout(Duration::from_secs(10), held.started.notified())
        .await
        .context("browser did not enter held navigation")?;
    caller.abort();
    let _ = caller.await;

    // 异步页面导航可在服务端挂起响应时被取消，作用域关闭必须等待取消流程彻底完成。
    let shutdown = timeout(Duration::from_secs(5), scope.shutdown()).await;
    // 无论成功或失败都必须唤醒放行服务端，避免请求处理协程永久挂起。
    held.release.notify_one();
    shutdown.context("cancelled async navigation prevented scope shutdown")?;
    ensure!(
        browser
            .fetch(request(format!("{base}/cookie")))
            .await
            .is_err(),
        "cancelled scope accepted new work"
    );
    Ok(())
}
