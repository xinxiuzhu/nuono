use super::action_log::{self, SendState};
use anyhow::Result;
use dashmap::DashMap;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;
use uuid::Uuid;

type ResponseCallback = dyn FnOnce(SendState, Option<&str>, Option<&str>) + Send + Sync;

struct ResponseObserver(Box<ResponseCallback>);

impl std::fmt::Debug for ResponseObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResponseObserver")
    }
}

/// 一个已分配 echo 的在途 action。
///
/// `tx = Some` 表示调用方正在等待结果；`None` 是 fire-and-forget 发送，但仍登记 echo，
/// 以便回包到达时完成日志关联并立即清除条目。
#[derive(Debug)]
struct PendingRequest {
    action: String,
    summary: String,
    tx: Option<oneshot::Sender<Result<Value, String>>>,
    deadline: tokio::time::Instant,
    plugin: Option<String>,
    observer: Option<ResponseObserver>,
}

impl PendingRequest {
    fn report(&mut self, state: SendState, error: Option<&str>) {
        if let Some(observer) = self.observer.take() {
            (observer.0)(state, self.plugin.as_deref(), error);
        }
    }
}

/// 带相关 ID 的 OneBot action 请求。
///
/// echo 由框架生成 UUID，并原样随请求发给下游；它只在当前进程的 `ApiClient`
/// `pending` 表中有意义，不是业务消息 ID，也不应被持久化为平台标识。
#[derive(Debug, Clone)]
pub struct ApiRequest {
    pub action: String,
    pub params: Value,
    pub echo: String,
}

impl ApiRequest {
    pub fn new(action: impl Into<String>, params: Value) -> Self {
        Self {
            action: action.into(),
            params,
            echo: Uuid::new_v4().to_string(),
        }
    }

    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "action": self.action,
            "params": self.params,
            "echo": self.echo,
        })
    }

    /// 序列化为线上文本；`params` 按所有权移动，媒体字段不再复制一份。
    fn into_json_string(self) -> Result<String> {
        let mut frame = serde_json::Map::with_capacity(3);
        frame.insert("action".into(), Value::String(self.action));
        frame.insert("params".into(), self.params);
        frame.insert("echo".into(), Value::String(self.echo));
        serde_json::to_string(&Value::Object(frame)).map_err(Into::into)
    }
}

/// 进程内的 OneBot echo 相关器。
///
/// 请求经连接 writer 发送，回包由 `ConnectionManager` 在事件管线之前调用
/// [`Self::on_response`]。映射不跨重启，也不绑定 bot_id；echo 的全局 UUID 唯一性使一个
/// 共享 client 可服务多条连接。
pub struct ApiClient {
    pending: Arc<DashMap<String, PendingRequest>>,
    admission: Mutex<()>,
    maintenance: Mutex<Option<Box<dyn crate::runtime::RuntimeTask>>>,
    executor: Option<crate::runtime::RuntimeExecutor>,
    capacity: usize,
    timeout: Duration,
}

impl std::fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiClient")
            .field("pending_count", &self.pending.len())
            .field("capacity", &self.capacity)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

const MAX_PENDING_REQUESTS: usize = 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ACTION_BYTES: usize = 256;

/// 在请求完成或被取消时自动清理 `pending` 表中未发送或等待中的 echo 记录。
struct PendingCallGuard<'a> {
    pending: &'a DashMap<String, PendingRequest>,
    echo: Option<String>,
}
impl Drop for PendingCallGuard<'_> {
    fn drop(&mut self) {
        if let Some(echo) = self.echo.take()
            && let Some((_, mut request)) = self.pending.remove(&echo)
        {
            if request.deadline <= tokio::time::Instant::now() {
                expire_request(&mut request);
            } else {
                request.report(SendState::Cancelled, Some("等待已取消，平台可能已接受请求"));
            }
        }
    }
}

fn expire_request(pending: &mut PendingRequest) {
    pending.report(SendState::Timeout, Some("等待平台回执超时"));
    if let Some(sender) = pending.tx.take() {
        let _ = sender.send(Err("API call timeout".into()));
    }
    metrics::counter!("nuono_onebot_echo_expired_total").increment(1);
}

fn reap_expired(pending: &DashMap<String, PendingRequest>) {
    let now = tokio::time::Instant::now();
    pending.retain(|_, request| {
        if request.deadline <= now {
            expire_request(request);
            false
        } else {
            true
        }
    });
}

async fn enqueue_before_deadline(
    sender: &tokio::sync::mpsc::Sender<nuono_app::transport::TransportFrame>,
    frame: nuono_app::transport::TransportFrame,
    deadline: tokio::time::Instant,
) -> Result<()> {
    let permit = tokio::time::timeout_at(deadline, sender.reserve())
        .await
        .map_err(|_| anyhow::anyhow!("API request queue timeout"))?
        .map_err(|_| anyhow::anyhow!("API request send failed: channel closed"))?;
    // `tokio::time::Timeout` 会优先轮询已就绪的内部 Future；如果通道容量是在超过截止时间之后、
    // 当前任务尚未被重新调度前才释放出来的，绝不能继续把这份已经过期的动作发出去。
    anyhow::ensure!(
        tokio::time::Instant::now() < deadline,
        "API request queue timeout"
    );
    permit.send(frame);
    Ok(())
}

impl ApiClient {
    pub fn new() -> Self {
        Self::with_limits(MAX_PENDING_REQUESTS, REQUEST_TIMEOUT)
    }

    fn with_limits(capacity: usize, timeout: Duration) -> Self {
        Self {
            pending: Arc::new(DashMap::new()),
            admission: Mutex::new(()),
            maintenance: Mutex::new(None),
            executor: crate::runtime::RuntimeExecutor::current(),
            capacity: capacity.max(1),
            timeout: timeout.max(Duration::from_millis(1)),
        }
    }

    fn ensure_maintenance(&self) -> Result<()> {
        let mut maintenance = self.maintenance.lock().expect("锁已中毒");
        if maintenance.as_ref().is_some_and(|task| task.is_finished()) {
            maintenance.take();
        }
        if maintenance.is_none() {
            let executor = self
                .executor
                .clone()
                .or_else(crate::runtime::RuntimeExecutor::current)
                .ok_or_else(|| anyhow::anyhow!("OneBot 请求需要运行中的宿主 runtime"))?;
            let pending = Arc::downgrade(&self.pending);
            let period = self.timeout.min(Duration::from_secs(1));
            *maintenance = Some(executor.spawn(Box::pin(async move {
                let mut interval = tokio::time::interval(period);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    let Some(pending) = pending.upgrade() else {
                        return;
                    };
                    reap_expired(&pending);
                }
            })));
        }
        Ok(())
    }

    fn register<'a>(
        &'a self,
        request: &ApiRequest,
        sender: Option<oneshot::Sender<Result<Value, String>>>,
        deadline: tokio::time::Instant,
    ) -> Result<PendingCallGuard<'a>> {
        anyhow::ensure!(
            !request.action.is_empty() && request.action.len() <= MAX_ACTION_BYTES,
            "OneBot action 必须为 1–{MAX_ACTION_BYTES} 字节"
        );
        self.ensure_maintenance()?;
        {
            // 只有准入插入操作需要互斥串行化；收到回包、取消或超时过期只需直接从 DashMap 中移除。
            let _admission = self.admission.lock().expect("锁已中毒");
            reap_expired(&self.pending);
            if self.pending.len() >= self.capacity {
                metrics::counter!("nuono_onebot_echo_rejected_total", "reason" => "full")
                    .increment(1);
                return Err(anyhow::anyhow!(
                    "OneBot 在途请求已达到 {} 项上限，请稍后重试",
                    self.capacity
                ));
            }
            self.pending.insert(
                request.echo.clone(),
                PendingRequest {
                    action: request.action.clone(),
                    summary: String::new(),
                    tx: sender,
                    deadline,
                    plugin: action_log::current_plugin(),
                    observer: None,
                },
            );
        }
        let guard = PendingCallGuard {
            pending: &self.pending,
            echo: Some(request.echo.clone()),
        };
        // 摘要只服务 DEBUG 日志。默认 INFO 级别下不遍历参数，媒体请求因此不会为了
        // 一份永远不会打印的摘要再复制/编码整棵参数树。
        if tracing::enabled!(tracing::Level::DEBUG)
            && let Some(mut pending) = self.pending.get_mut(&request.echo)
        {
            pending.summary = summarize_params(&request.params);
        }
        Ok(guard)
    }

    fn request_summary(&self, echo: &str) -> String {
        self.pending
            .get(echo)
            .map(|pending| pending.summary.clone())
            .unwrap_or_default()
    }

    /// 在写入连接之前，按精确 echo 绑定一次结果日志；不会保留消息负载。
    ///
    /// 返回 `false` 表示该请求不由此 client 跟踪，调用者只能报告写入状态。
    pub fn observe_response(
        &self,
        echo: &str,
        observer: impl FnOnce(SendState, Option<&str>, Option<&str>) + Send + Sync + 'static,
    ) -> bool {
        let Some(mut pending) = self.pending.get_mut(echo) else {
            return false;
        };
        if pending.observer.is_some() {
            return false;
        }
        pending.observer = Some(ResponseObserver(Box::new(observer)));
        true
    }

    /// 写入失败只结束日志观察，不改变请求等待和取消规则。
    pub fn report_write_failure(&self, echo: &str, error: &str) {
        if let Some(mut pending) = self.pending.get_mut(echo) {
            pending.report(SendState::Failed, Some(error));
        }
    }

    /// 入队和等待回包共用 30 秒期限，最多 1024 个已登记请求；满载立即拒绝。
    /// 取消会移除登记，迟到回包不重试，也不能据此判断平台是否已执行 action。
    pub async fn call(
        &self,
        action: impl Into<String>,
        params: Value,
        msg_tx: &tokio::sync::mpsc::Sender<nuono_app::transport::TransportFrame>,
    ) -> Result<Value> {
        let req = ApiRequest::new(action, params);
        let deadline = tokio::time::Instant::now() + self.timeout;
        let (tx, rx) = oneshot::channel();
        let _pending_guard = self.register(&req, Some(tx), deadline)?;
        tracing::debug!(target: "onebot_api", action = %req.action, echo = %req.echo,
            params = %self.request_summary(&req.echo), "[OneBot API] 请求发送");
        let json_str = req.into_json_string()?;
        enqueue_before_deadline(
            msg_tx,
            nuono_app::transport::TransportFrame::Text(json_str),
            deadline,
        )
        .await?;
        match tokio::time::timeout_at(deadline, rx)
            .await
            .map_err(|_| anyhow::anyhow!("API call timeout"))?
        {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(anyhow::anyhow!("API error: {error}")),
            Err(_) => Err(anyhow::anyhow!("API response channel closed")),
        }
    }

    /// 入队成功即返回；回包关联有效期 30 秒，由单个受管维护任务每秒清理。
    /// 队列等待也有同一期限。取消或入队失败立即移除登记；不隐式重试通知。
    pub async fn send(
        &self,
        action: impl Into<String>,
        params: Value,
        msg_tx: &tokio::sync::mpsc::Sender<nuono_app::transport::TransportFrame>,
    ) -> Result<()> {
        let req = ApiRequest::new(action, params);
        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut pending = self.register(&req, None, deadline)?;
        tracing::debug!(target: "onebot_api", action = %req.action, echo = %req.echo,
            params = %self.request_summary(&req.echo), "[OneBot API] 通知发送");
        let json_str = req.into_json_string()?;
        enqueue_before_deadline(
            msg_tx,
            nuono_app::transport::TransportFrame::Text(json_str),
            deadline,
        )
        .await?;
        // 成功入队的通知在收到响应或超时过期前会一直保留在 `pending` 表中（受容量与 TTL 上限约束），因此解除守卫的即时清理。
        pending.echo = None;
        Ok(())
    }

    /// 消费下游 action 回包。
    ///
    /// 条目在解释 `status` 前即从 pending 中移除，故同 echo 只有首个回包有效。明确的
    /// `failed`/`error` 转为等待方错误；未知格式为兼容不同 OneBot 实现，按成功返回
    /// 整个响应体，而不是擅自判失败。
    pub fn on_response(&self, response: &Value) {
        let echo = response.get("echo").and_then(|v| v.as_str()).unwrap_or("");

        if echo.is_empty() {
            return;
        }

        if let Some((_, mut pending)) = self.pending.remove(echo) {
            if pending.deadline <= tokio::time::Instant::now() {
                expire_request(&mut pending);
                return;
            }
            let status = response.get("status").and_then(|v| v.as_str());
            let data = response.get("data").cloned().unwrap_or(Value::Null);
            let retcode = response.get("retcode").and_then(|v| v.as_i64());
            let msg = response
                .get("msg")
                .or_else(|| response.get("wording"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            match status {
                Some("ok")
                    if response
                        .get("retcode")
                        .is_none_or(|code| code.as_i64() == Some(0)) =>
                {
                    pending.report(SendState::Confirmed, None);
                    tracing::debug!(
                        target: "onebot_api",
                        action = %pending.action,
                        echo = %echo,
                        retcode = retcode.unwrap_or_default(),
                        params = %pending.summary,
                        data = %summarize_value(&data),
                        "[OneBot API] 回包成功"
                    );
                    if let Some(tx) = pending.tx {
                        let _ = tx.send(Ok(data));
                    }
                }
                Some("failed") | Some("error") | Some("ok") => {
                    let err_msg = if msg.is_empty() { "unknown error" } else { msg };
                    let safe_err_msg = summarize_error_text(err_msg);
                    pending.report(SendState::Failed, Some(&safe_err_msg));
                    tracing::warn!(
                        target: "onebot_api",
                        action = %pending.action,
                        echo = %echo,
                        retcode = retcode.unwrap_or_default(),
                        error = %safe_err_msg,
                        params = %pending.summary,
                        response = %summarize_value(response),
                        "[OneBot API] 回包失败"
                    );
                    if let Some(tx) = pending.tx {
                        let _ = tx.send(Err(safe_err_msg));
                    }
                }
                _ => {
                    pending.report(SendState::Unconfirmed, Some("平台未返回明确的成功回执"));
                    // 不确定格式，直接返回整个响应体
                    tracing::warn!(
                        target: "onebot_api",
                        action = %pending.action,
                        echo = %echo,
                        params = %pending.summary,
                        response = %summarize_value(response),
                        "[OneBot API] 未知回包格式"
                    );
                    if let Some(tx) = pending.tx {
                        let _ = tx.send(Ok(response.clone()));
                    }
                }
            }
        } else {
            let status = response
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let retcode = response
                .get("retcode")
                .and_then(|v| v.as_i64())
                .unwrap_or_default();
            tracing::debug!(target: "onebot_api", echo = %echo, status = %status, retcode = retcode, "[OneBot API] 收到未登记回包");
        }
    }
}

fn summarize_params(params: &Value) -> String {
    summarize_value(params)
}

/// 错误描述按 token 脱敏，而不是整条替换。
///
/// 实现端把失败原因写在这里，是排查 retcode 的唯一线索；整条打掉等于自断诊断
/// 能力。逐 token 处理只挡住 URL、路径和 base64 片段，把「为什么失败」留下。
fn summarize_error_text(value: &str) -> String {
    let mut text = value
        .split_whitespace()
        .map(|token| {
            let stripped = token
                .trim_matches(|ch: char| matches!(ch, '"' | '\'' | '(' | ')' | ',' | '。' | '，'));
            if stripped.is_empty() || !should_redact_text(stripped) {
                return token.to_string();
            }
            token.replace(
                stripped,
                &format!("<{} chars redacted>", stripped.chars().count()),
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    const LIMIT: usize = 1200;
    if text.len() > LIMIT {
        let mut end = LIMIT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("...");
    }
    text
}

fn summarize_value(value: &Value) -> String {
    let mut value = value.clone();
    redact_large_fields(&mut value);
    let mut text = value.to_string();
    const LIMIT: usize = 1200;
    if text.len() > LIMIT {
        let mut end = LIMIT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("...");
    }
    text
}

fn should_redact_text(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("http://")
        || lower.contains("https://")
        || lower.contains("base64://")
        || lower.contains("data:")
        || lower.contains("file://")
        || value.starts_with("~/")
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with("\\\\")
        || (!value.starts_with('/') && value.contains('/'))
        || [
            "/etc/",
            "/home/",
            "/private/",
            "/root/",
            "/tmp/",
            "/usr/",
            "/Users/",
            "/var/",
            "/Volumes/",
        ]
        .iter()
        .any(|prefix| value.starts_with(prefix))
        || (value.len() >= 3
            && value.as_bytes()[1] == b':'
            && matches!(value.as_bytes()[2], b'\\' | b'/'))
        || contains_encoded_payload(value)
}

fn contains_encoded_payload(value: &str) -> bool {
    value.split_whitespace().any(|token| {
        let token = token.trim_matches(|ch: char| {
            !ch.is_ascii_alphanumeric() && !matches!(ch, '+' | '/' | '=' | '-' | '_')
        });
        looks_like_encoded_payload(token)
    })
}

fn looks_like_encoded_payload(value: &str) -> bool {
    let trimmed = value.trim_end_matches('=');
    let padding = value.len() - trimmed.len();
    let standard_alphabet = trimmed
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'));
    let url_safe_alphabet = trimmed
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    padding <= 2
        && trimmed.len() >= 24
        && (standard_alphabet || url_safe_alphabet)
        && value.len() % 4 != 1
        && (padding > 0
            || value.len() >= 48
            || value.chars().any(|ch| matches!(ch, '+' | '/' | '-' | '_')))
}

fn redact_large_fields(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if matches!(
                    key.to_ascii_lowercase().as_str(),
                    "file" | "url" | "base64" | "photo" | "document" | "voice" | "video"
                ) {
                    if let Some(value) = val.as_str() {
                        *val = Value::String(format!("<{} chars redacted>", value.chars().count()));
                    } else {
                        *val = Value::String("<redacted>".to_string());
                    }
                    continue;
                }
                redact_large_fields(val);
            }
        }
        Value::Array(arr) => {
            for val in arr {
                redact_large_fields(val);
            }
        }
        Value::String(text) if should_redact_text(text) || text.len() > 512 => {
            *value = Value::String(format!("<{} chars redacted>", text.chars().count()));
        }
        _ => {}
    }
}

impl Default for ApiClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "../../tests/unit/core/onebot_client/fault_tests.rs"]
mod fault_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn echo(frame: nuono_app::transport::TransportFrame) -> String {
        let nuono_app::transport::TransportFrame::Text(text) = frame else {
            panic!("expected text frame");
        };
        serde_json::from_str::<Value>(&text).unwrap()["echo"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[tokio::test]
    async fn response_observers_follow_echo_and_plugin_with_interleaved_replies() {
        let client = ApiClient::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let first = action_log::scope(
            Some("DemoPlugin".into()),
            client.send("send_msg", json!({"message":"first"}), &tx),
        );
        let second = action_log::scope(
            Some("BetaPlugin".into()),
            client.send("send_msg", json!({"message":"second"}), &tx),
        );
        let (a, b) = tokio::join!(first, second);
        a.unwrap();
        b.unwrap();
        let first_echo = echo(rx.recv().await.unwrap());
        let second_echo = echo(rx.recv().await.unwrap());
        let captured = Arc::new(Mutex::new(Vec::new()));
        for id in [&first_echo, &second_echo] {
            let captured = captured.clone();
            let request = id.to_owned();
            assert!(client.observe_response(id, move |state, plugin, error| {
                captured.lock().unwrap().push((
                    request,
                    state,
                    plugin.map(str::to_owned),
                    error.map(str::to_owned),
                ));
            }));
        }
        client.on_response(
            &json!({"echo": second_echo, "status":"failed", "retcode":100, "wording":"拒绝发送"}),
        );
        client.on_response(&json!({"echo": first_echo, "status":"ok", "retcode":0}));
        client.on_response(&json!({"echo": first_echo, "status":"ok", "retcode":0}));
        let rows = captured.lock().unwrap();
        assert_eq!(rows.len(), 2, "one final outcome per echo");
        assert_eq!(
            rows[0],
            (
                second_echo,
                SendState::Failed,
                Some("BetaPlugin".into()),
                Some("拒绝发送".into())
            )
        );
        assert_eq!(
            rows[1],
            (
                first_echo,
                SendState::Confirmed,
                Some("DemoPlugin".into()),
                None
            )
        );
    }

    #[tokio::test(start_paused = true)]
    async fn expiry_unknown_response_and_write_failure_never_report_confirmation() {
        let client = ApiClient::with_limits(4, Duration::from_secs(30));
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut echoes = Vec::new();
        for _ in 0..3 {
            client.send("send_msg", json!({}), &tx).await.unwrap();
            let id = echo(rx.recv().await.unwrap());
            let captured = captured.clone();
            assert!(
                client
                    .observe_response(&id, move |state, _, _| captured.lock().unwrap().push(state))
            );
            echoes.push(id);
        }
        client.on_response(&json!({"echo": echoes[0], "status":"async", "retcode":0}));
        client.report_write_failure(&echoes[1], "连接已关闭");
        tokio::time::advance(Duration::from_secs(31)).await;
        reap_expired(&client.pending);
        assert_eq!(
            *captured.lock().unwrap(),
            [
                SendState::Unconfirmed,
                SendState::Failed,
                SendState::Timeout
            ]
        );
        assert!(client.pending.is_empty());
    }

    #[tokio::test]
    async fn cancellation_reports_unknown_outcome_once_and_releases_observer() {
        let client = ApiClient::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let mut call = Box::pin(client.call("send_msg", json!({}), &tx));
        let frame = tokio::select! {
            _ = &mut call => panic!("receipt must be pending"),
            frame = rx.recv() => frame.unwrap(),
        };
        let id = echo(frame);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let observer = captured.clone();
        assert!(
            client.observe_response(&id, move |state, _, _| observer.lock().unwrap().push(state))
        );
        drop(call);
        client.on_response(&json!({"echo":id,"status":"ok"}));
        assert_eq!(*captured.lock().unwrap(), [SendState::Cancelled]);
        assert!(client.pending.is_empty());
    }

    #[tokio::test]
    async fn cancelling_queue_blocked_call_removes_pending_echo() {
        let client = ApiClient::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        tx.send(nuono_app::transport::TransportFrame::Text(
            "occupied".into(),
        ))
        .await
        .unwrap();
        let mut call = Box::pin(client.call("get_status", json!({}), &tx));
        tokio::select! {
            biased;
            _ = &mut call => panic!("full queue should block"),
            _ = tokio::task::yield_now() => {}
        }
        assert_eq!(client.pending.len(), 1);
        drop(call);
        assert!(client.pending.is_empty());
    }

    #[tokio::test]
    async fn cancelling_sent_call_removes_pending_echo() {
        let client = ApiClient::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let mut call = Box::pin(client.call("get_status", json!({}), &tx));
        tokio::select! {
            biased;
            _ = &mut call => panic!("response should be pending"),
            frame = rx.recv() => assert!(frame.is_some())
        }
        assert_eq!(client.pending.len(), 1);
        drop(call);
        assert!(client.pending.is_empty());
    }

    #[test]
    fn action_summaries_redact_resources_and_nested_response_values() {
        let value = json!({
            "message": [
                {"type": "image", "data": {"url": "https://secret.test/image.png"}},
                {"type": "file", "data": {"file": "/Users/private/key.txt"}}
            ],
            "response": {
                "download": "base64://secret",
                "relative": "./secrets",
                "encoded": "abcdefghijklmnopqrstuvwxyz0123456789_-AB"
            }
        });

        let summary = summarize_value(&value);
        for secret in [
            "secret.test",
            "/Users/private",
            "base64://",
            "./secrets",
            "abcdefghijklmnopqrstuvwxyz",
        ] {
            assert!(!summary.contains(secret));
        }
        assert!(summary.contains("redacted"));
    }

    #[test]
    fn error_text_keeps_the_reason_and_only_redacts_the_path() {
        // 实现端常把我们传的路径回显进错误里；整条打掉就查不出 retcode 的原因了。
        let safe = summarize_error_text(
            "send video failed: file not found /Users/example/media/clip.mp4 rich media transfer failed",
        );
        assert!(safe.contains("send video failed"));
        assert!(safe.contains("rich media transfer failed"));
        assert!(!safe.contains("/Users/example"));
        assert!(safe.contains("redacted"));
    }

    #[test]
    fn action_summaries_truncate_unicode_at_char_boundary() {
        let value = Value::Array(
            (0..200)
                .map(|_| Value::String("中文".to_string()))
                .collect(),
        );

        let summary = summarize_value(&value);
        assert!(summary.ends_with("..."));
        assert!(summary.len() <= 1203);
    }

    #[tokio::test]
    async fn contradictory_ok_response_is_failed_and_error_text_is_redacted() {
        let client = ApiClient::new();
        let (tx, rx) = oneshot::channel();
        client.pending.insert(
            "echo-1".to_string(),
            PendingRequest {
                action: "send_msg".to_string(),
                summary: "{}".to_string(),
                tx: Some(tx),
                deadline: tokio::time::Instant::now() + REQUEST_TIMEOUT,
                plugin: None,
                observer: None,
            },
        );

        client.on_response(&json!({
            "echo": "echo-1",
            "status": "ok",
            "retcode": 100,
            "msg": "upload failed: abcdefghijklmnopqrstuvwxyz0123456789_-AB",
            "data": null
        }));

        let error = rx.await.unwrap().unwrap_err();
        assert!(error.contains("redacted"));
        assert!(!error.contains("abcdefghijklmnopqrstuvwxyz"));
    }
}
