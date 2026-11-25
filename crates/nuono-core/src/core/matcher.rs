use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::trace;

use crate::api::bot::Bot;
use crate::core::platform::UnifiedEvent;
use crate::error::Result;

fn command_boundary_ok(rest: &str) -> bool {
    rest.is_empty()
        || rest
            .chars()
            .next()
            .is_some_and(|ch| ch.is_whitespace() || matches!(ch, '#' | ':' | ',' | '，' | '：'))
}

// Event Context — 事件上下文，类似 NoneBot2 的 Event 对象

#[derive(Clone)]
pub struct EventContext {
    pub event: Arc<UnifiedEvent>,
    pub bot: Option<Arc<Bot>>,
    pub state: HashMap<String, Value>,
}

impl EventContext {
    pub fn new(event: UnifiedEvent) -> Self {
        Self {
            event: Arc::new(event),
            bot: None,
            state: HashMap::new(),
        }
    }

    pub fn with_bot(mut self, bot: Arc<Bot>) -> Self {
        self.bot = Some(bot);
        self
    }

    pub fn insert_state(mut self, key: impl Into<String>, value: Value) -> Self {
        self.state.insert(key.into(), value);
        self
    }

    pub fn get_state(&self, key: &str) -> Option<&Value> {
        self.state.get(key)
    }

    pub fn user_id(&self) -> Option<i64> {
        self.event.user_id()
    }

    pub fn group_id(&self) -> Option<i64> {
        self.event.group_id()
    }

    pub fn self_id(&self) -> Option<i64> {
        self.event.self_id()
    }

    pub fn content(&self) -> Option<&str> {
        self.event.as_message().map(|m| m.content.as_str())
    }
}

// 处理器特征

pub type HandlerFuture = Pin<Box<dyn Future<Output = HandlerResult> + Send>>;

pub enum HandlerResult {
    Continue,
    Stop,
    Reply(String),
}

pub trait Handler: Send + Sync {
    fn handle(&self, ctx: EventContext) -> HandlerFuture;
}

impl<F, Fut> Handler for F
where
    F: Fn(EventContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = HandlerResult> + Send + 'static,
{
    fn handle(&self, ctx: EventContext) -> HandlerFuture {
        Box::pin(self(ctx))
    }
}

// Matcher — 事件匹配器，类似 NoneBot2 的 Matcher

pub struct Matcher {
    pub name: String,
    pub priority: i32,
    pub block: bool,
    pub handlers: Vec<Arc<dyn Handler>>,
    pub rule: MatcherRule,
    pub temp: bool,
    pub available: bool,
}

impl Matcher {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            priority: 1,
            block: true,
            handlers: Vec::new(),
            rule: MatcherRule::default(),
            temp: false,
            available: true,
        }
    }

    pub fn priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    pub fn block(mut self, block: bool) -> Self {
        self.block = block;
        self
    }

    pub fn temp(mut self) -> Self {
        self.temp = true;
        self
    }

    pub fn command(mut self, cmd: impl Into<String>) -> Self {
        self.rule.commands.push(cmd.into());
        self
    }

    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.rule.prefixes.push(prefix.into());
        self
    }

    pub fn keyword(mut self, keyword: impl Into<String>) -> Self {
        self.rule.keywords.push(keyword.into());
        self
    }

    pub fn regex(mut self, pattern: impl Into<String>) -> Self {
        self.rule.regexes.push(pattern.into());
        self
    }

    pub fn to_me(mut self) -> Self {
        self.rule.to_me = true;
        self
    }

    pub fn group_only(mut self) -> Self {
        self.rule.group_only = true;
        self
    }

    pub fn private_only(mut self) -> Self {
        self.rule.private_only = true;
        self
    }

    pub fn handle<H: Handler + 'static>(mut self, handler: H) -> Self {
        self.handlers.push(Arc::new(handler));
        self
    }

    pub fn handle_fn<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(EventContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HandlerResult> + Send + 'static,
    {
        self.handlers.push(Arc::new(f));
        self
    }

    /// 判断消息是否满足静态规则。
    ///
    /// 不同规则类别之间是 AND，同一类别中的多个候选是 OR。无效正则按不匹配
    /// 处理。`to_me` 当前只被记录，尚未参与匹配，这是调用方不能依赖的过滤条件。
    pub fn matches(&self, event: &UnifiedEvent) -> bool {
        if !self.available {
            return false;
        }

        let Some(msg) = event.as_message() else {
            return false;
        };

        if self.rule.group_only && msg.group_id.is_none() {
            return false;
        }
        if self.rule.private_only && msg.group_id.is_some() {
            return false;
        }

        let text = &msg.content;

        if !self.rule.commands.is_empty() {
            let matched = self.rule.commands.iter().any(|cmd| {
                text.starts_with(cmd) && text.get(cmd.len()..).is_some_and(command_boundary_ok)
            });
            if !matched {
                return false;
            }
        }

        if !self.rule.prefixes.is_empty() {
            let matched = self.rule.prefixes.iter().any(|pfx| text.starts_with(pfx));
            if !matched {
                return false;
            }
        }

        if !self.rule.keywords.is_empty() {
            let matched = self.rule.keywords.iter().any(|kw| text.contains(kw));
            if !matched {
                return false;
            }
        }

        if !self.rule.regexes.is_empty() {
            let matched = self.rule.regexes.iter().any(|re| {
                regex::Regex::new(re)
                    .map(|r| r.is_match(text))
                    .unwrap_or(false)
            });
            if !matched {
                return false;
            }
        }

        true
    }
}

#[derive(Default, Clone)]
pub struct MatcherRule {
    pub commands: Vec<String>,
    pub prefixes: Vec<String>,
    pub keywords: Vec<String>,
    pub regexes: Vec<String>,
    pub to_me: bool,
    pub group_only: bool,
    pub private_only: bool,
}

// Session — 多轮对话状态机

pub struct Session {
    pub id: String,
    pub state: HashMap<String, Value>,
    pub created_at: std::time::Instant,
    pub expires_at: Option<std::time::Instant>,
    pub current_step: u32,
    pub total_steps: u32,
}

impl Session {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            state: HashMap::new(),
            created_at: std::time::Instant::now(),
            expires_at: None,
            current_step: 0,
            total_steps: 0,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.expires_at = Some(std::time::Instant::now() + timeout);
        self
    }

    pub fn with_steps(mut self, steps: u32) -> Self {
        self.total_steps = steps;
        self
    }

    pub fn set<K: Into<String>>(&mut self, key: K, value: Value) {
        self.state.insert(key.into(), value);
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.state.get(key)
    }

    pub fn is_expired(&self) -> bool {
        self.expires_at
            .is_some_and(|t| std::time::Instant::now() > t)
    }

    pub fn advance(&mut self) -> bool {
        self.current_step += 1;
        self.current_step < self.total_steps
    }
}

// SessionManager — 管理所有活跃的会话

pub struct SessionManager {
    sessions: DashMap<String, Session>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
        }
    }

    pub fn create(&self, session: Session) {
        let id = session.id.clone();
        self.sessions.insert(id, session);
    }

    pub fn get(&self, session_id: &str) -> Option<dashmap::mapref::one::Ref<'_, String, Session>> {
        self.sessions.get(session_id)
    }

    pub fn get_mut(
        &self,
        session_id: &str,
    ) -> Option<dashmap::mapref::one::RefMut<'_, String, Session>> {
        self.sessions.get_mut(session_id)
    }

    pub fn remove(&self, session_id: &str) -> Option<Session> {
        self.sessions.remove(session_id).map(|(_, v)| v)
    }

    pub fn has_session(&self, session_id: &str) -> bool {
        self.sessions.contains_key(session_id)
    }

    pub fn cleanup_expired(&self) -> usize {
        let expired_keys: Vec<String> = self
            .sessions
            .iter()
            .filter(|entry| entry.value().is_expired())
            .map(|entry| entry.key().clone())
            .collect();

        let count = expired_keys.len();
        for key in expired_keys {
            self.sessions.remove(&key);
        }
        count
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

// MatcherRegistry — Matcher 注册中心

pub struct MatcherRegistry {
    matchers: RwLock<Vec<Arc<Matcher>>>,
    session_manager: Arc<SessionManager>,
}

pub struct MatcherResult {
    pub replies: Vec<String>,
    pub blocked: bool,
}

impl MatcherRegistry {
    pub fn new(session_manager: Arc<SessionManager>) -> Self {
        Self {
            matchers: RwLock::new(Vec::new()),
            session_manager,
        }
    }

    pub async fn register(&self, matcher: Matcher) {
        let name = matcher.name.clone();
        let priority = matcher.priority;
        let block = matcher.block;
        trace!(
            "注册 Matcher: {} (priority={}, block={})",
            name, priority, block
        );
        let mut matchers = self.matchers.write().await;
        matchers.push(Arc::new(matcher));
        matchers.sort_by_key(|m| m.priority);
    }

    /// 按升序优先级串行执行匹配器和 handler。
    ///
    /// handler 的 `Continue` 仅继续当前 matcher；`Reply`/`Stop` 结束当前 handler 链。
    /// 只有产生 `Reply` 或 `Stop` 才算命中，随后 `block=true` 才会阻止更低优先级
    /// matcher。读锁跨越所有异步 handler，事件处理期间注册新 matcher 会等待。
    pub async fn handle_event(&self, event: UnifiedEvent) -> MatcherResult {
        let matchers = self.matchers.read().await;
        let mut replies = Vec::new();
        let mut blocked = false;

        for matcher in matchers.iter() {
            if !matcher.matches(&event) {
                continue;
            }

            let ctx = EventContext::new(event.clone());
            let mut matched = false;
            for handler in &matcher.handlers {
                match handler.handle(ctx.clone()).await {
                    HandlerResult::Continue => continue,
                    HandlerResult::Reply(text) => {
                        replies.push(text);
                        matched = true;
                        break;
                    }
                    HandlerResult::Stop => {
                        matched = true;
                        break;
                    }
                }
            }

            if matched && matcher.block {
                blocked = true;
                break;
            }
        }

        MatcherResult { replies, blocked }
    }

    pub async fn matcher_count(&self) -> usize {
        self.matchers.read().await.len()
    }

    pub fn session_manager(&self) -> Arc<SessionManager> {
        self.session_manager.clone()
    }
}

impl Default for MatcherRegistry {
    fn default() -> Self {
        Self::new(Arc::new(SessionManager::new()))
    }
}

// 便捷会话构造器

pub struct SessionBuilder {
    session_id: String,
    timeout: Duration,
    steps: u32,
}

impl SessionBuilder {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            timeout: Duration::from_secs(300),
            steps: 0,
        }
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn steps(mut self, steps: u32) -> Self {
        self.steps = steps;
        self
    }

    pub fn build(self) -> Session {
        Session::new(self.session_id)
            .with_timeout(self.timeout)
            .with_steps(self.steps)
    }
}

// 会话流程辅助

pub struct SessionFlow;

impl SessionFlow {
    pub async fn wait_for_reply(
        bot: &Bot,
        session_id: &str,
        timeout: Duration,
        session_manager: &SessionManager,
    ) -> Result<Value> {
        match bot.wait_for_reply(timeout).await {
            Ok(reply) => {
                if let Some(mut session) = session_manager.remove(session_id) {
                    session.set("last_reply", reply.clone());
                    session.advance();
                    session_manager.create(session);
                }
                Ok(reply)
            }
            Err(e) => {
                session_manager.remove(session_id);
                Err(e)
            }
        }
    }

    pub async fn conversation<F, Fut>(
        bot: &Bot,
        session_id: &str,
        session_manager: &SessionManager,
        handler: F,
    ) -> Result<()>
    where
        F: FnOnce(&mut Session, &Bot) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let session = Session::new(session_id)
            .with_timeout(Duration::from_secs(300))
            .with_steps(1);
        session_manager.create(session);

        if let Some(mut session) = session_manager.get_mut(session_id) {
            handler(&mut session, bot).await?;
        }

        session_manager.remove(session_id);
        Ok(())
    }
}
