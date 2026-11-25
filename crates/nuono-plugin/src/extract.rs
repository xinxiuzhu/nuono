//! 统一指令提取器（`FromCommandContext`）与声明式指令分组（`CommandGroup`）。
//!
//! 解决大型插件手写 `enum Action` + 元组表分发与重复前置鉴权的样板问题。
//! 任何普通 `async fn` 只要参数实现了 [`FromCommandContext`]，即可直接注册为指令处理器，
//! 无需手写 `#[async_trait]` 或生命周期标注。

use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::api::Bot;
use crate::core::command::{Command, CommandHandler, PermissionLevel};
use crate::core::trigger::{MatchData, Trigger, TriggerType};
use crate::domain::DomainEvent;
use crate::plugin::Event;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 单次指令调用的只读上下文（用于 [`FromCommandContext`] 提取器和 [`CommandGuard`] 守卫）。
#[derive(Debug)]
pub struct CommandContext<'a> {
    bot: &'a Bot,
    event: &'a Event<'a>,
    match_data: &'a MatchData,
}

impl<'a> CommandContext<'a> {
    /// 构造指令提取上下文。
    pub fn new(bot: &'a Bot, event: &'a Event<'a>, match_data: &'a MatchData) -> Self {
        Self {
            bot,
            event,
            match_data,
        }
    }

    /// 绑定当前会话目标的 Bot 动作句柄。
    pub fn bot(&self) -> &'a Bot {
        self.bot
    }

    /// 原始入站事件包装器。
    pub fn event(&self) -> &'a Event<'a> {
        self.event
    }

    /// 跨平台强类型领域事件（若宿主已归一化提供）。
    pub fn domain_event(&self) -> Option<&'a DomainEvent> {
        self.event.domain_event()
    }

    /// 本次触发匹配的捕获结果与原始参数文本。
    pub fn match_data(&self) -> &'a MatchData {
        self.match_data
    }

    /// 提取已去除首尾空白的参数字符串。
    pub fn args_str(&self) -> &'a str {
        self.match_data.args.trim()
    }
}

/// 提取器或前置守卫拒绝执行时的处置结果。
#[derive(Debug)]
#[non_exhaustive]
pub enum CommandRejection {
    /// 向当前会话回复用户友好提示并正常结束本次指令（不记为系统错误）。
    Reply(String),
    /// 静默终止本次指令执行（不回复也不报错）。
    Silent,
    /// 作为内部错误向上抛给宿主诊断与日志。
    Error(anyhow::Error),
}

impl CommandRejection {
    /// 构造一条直接回复给用户的拒绝提示。
    pub fn reply(message: impl Into<String>) -> Self {
        Self::Reply(message.into())
    }
}

impl From<anyhow::Error> for CommandRejection {
    fn from(error: anyhow::Error) -> Self {
        Self::Error(error)
    }
}

impl From<crate::NuonoError> for CommandRejection {
    fn from(error: crate::NuonoError) -> Self {
        Self::Error(error.into())
    }
}

/// 指令参数提取器 Trait。
#[async_trait]
pub trait FromCommandContext<S: Send + Sync>: Sized + Send {
    async fn from_command_context(
        ctx: &CommandContext<'_>,
        state: &S,
    ) -> std::result::Result<Self, CommandRejection>;
}

/// 注入插件共享状态 `S`。
#[derive(Debug, Clone)]
pub struct CmdState<S>(pub S);

impl<S> Deref for CmdState<S> {
    type Target = S;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[async_trait]
impl<S: Clone + Send + Sync> FromCommandContext<S> for CmdState<S> {
    async fn from_command_context(
        _ctx: &CommandContext<'_>,
        state: &S,
    ) -> std::result::Result<Self, CommandRejection> {
        Ok(Self(state.clone()))
    }
}

#[async_trait]
impl<S: Send + Sync> FromCommandContext<S> for Bot {
    async fn from_command_context(
        ctx: &CommandContext<'_>,
        _state: &S,
    ) -> std::result::Result<Self, CommandRejection> {
        Ok(ctx.bot().clone())
    }
}

#[async_trait]
impl<S: Send + Sync> FromCommandContext<S> for MatchData {
    async fn from_command_context(
        ctx: &CommandContext<'_>,
        _state: &S,
    ) -> std::result::Result<Self, CommandRejection> {
        Ok(ctx.match_data().clone())
    }
}

/// 去除首尾空白的原始参数文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawArgs(pub String);

impl Deref for RawArgs {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[async_trait]
impl<S: Send + Sync> FromCommandContext<S> for RawArgs {
    async fn from_command_context(
        ctx: &CommandContext<'_>,
        _state: &S,
    ) -> std::result::Result<Self, CommandRejection> {
        Ok(Self(ctx.args_str().to_owned()))
    }
}

/// 按空白拆分的指令参数列表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args(pub Vec<String>);

impl Deref for Args {
    type Target = [String];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[async_trait]
impl<S: Send + Sync> FromCommandContext<S> for Args {
    async fn from_command_context(
        ctx: &CommandContext<'_>,
        _state: &S,
    ) -> std::result::Result<Self, CommandRejection> {
        Ok(Self(
            ctx.args_str()
                .split_whitespace()
                .map(str::to_owned)
                .collect(),
        ))
    }
}

/// 解析整个参数文本为强类型 `T`（实现 `FromStr`）；解析失败时回复友好提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedArg<T>(pub T);

impl<T> Deref for ParsedArg<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[async_trait]
impl<S, T> FromCommandContext<S> for ParsedArg<T>
where
    S: Send + Sync,
    T: FromStr + Send,
{
    async fn from_command_context(
        ctx: &CommandContext<'_>,
        _state: &S,
    ) -> std::result::Result<Self, CommandRejection> {
        let raw = ctx.args_str();
        if raw.is_empty() {
            return Err(CommandRejection::reply("缺少必要参数"));
        }
        raw.parse::<T>()
            .map(Self)
            .map_err(|_| CommandRejection::reply(format!("参数格式无效: {raw}")))
    }
}

/// 跨平台发送者原生 ID（优先读取 `DomainEvent::actor`，兼容退回 JSON `user_id`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderId(pub String);

impl Deref for SenderId {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[async_trait]
impl<S: Send + Sync> FromCommandContext<S> for SenderId {
    async fn from_command_context(
        ctx: &CommandContext<'_>,
        _state: &S,
    ) -> std::result::Result<Self, CommandRejection> {
        if let Some(actor) = ctx.domain_event().and_then(|d| d.actor.as_ref()) {
            return Ok(Self(actor.value().as_str().to_owned()));
        }
        if let Some(raw) = ctx.event().raw().get("user_id") {
            let id = match raw {
                Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            };
            if let Some(id) = id {
                return Ok(Self(id));
            }
        }
        Err(CommandRejection::reply("无法识别当前发送者身份"))
    }
}

/// 可选群/频道会话 ID（私聊时为 `None`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionalGroupId(pub Option<String>);

#[async_trait]
impl<S: Send + Sync> FromCommandContext<S> for OptionalGroupId {
    async fn from_command_context(
        ctx: &CommandContext<'_>,
        _state: &S,
    ) -> std::result::Result<Self, CommandRejection> {
        if let Some(conv) = ctx.domain_event().and_then(|d| d.conversation.as_ref()) {
            let id = match conv {
                crate::domain::ConversationRef::Group { group } => {
                    Some(group.value().as_str().to_owned())
                }
                crate::domain::ConversationRef::Channel { channel, .. } => {
                    Some(channel.value().as_str().to_owned())
                }
                crate::domain::ConversationRef::Direct { .. } | _ => None,
            };
            return Ok(Self(id));
        }
        let id = ctx.event().raw().get("group_id").and_then(|v| match v {
            Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        });
        Ok(Self(id))
    }
}

/// 仅限群聊/频道执行的会话 ID 提取器；私聊触发时自动回复提示并拒绝执行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupId(pub String);

impl Deref for GroupId {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[async_trait]
impl<S: Send + Sync> FromCommandContext<S> for GroupId {
    async fn from_command_context(
        ctx: &CommandContext<'_>,
        state: &S,
    ) -> std::result::Result<Self, CommandRejection> {
        match OptionalGroupId::from_command_context(ctx, state).await?.0 {
            Some(id) => Ok(Self(id)),
            None => Err(CommandRejection::reply("该指令仅支持在群聊或频道中使用")),
        }
    }
}

/// 持有拥有所有权的事件快照（供需要读取语言、元数据或领域事件的处理器使用）。
#[derive(Debug, Clone)]
pub struct EventSnapshot {
    pub raw: Value,
    pub domain: Option<DomainEvent>,
    pub locale: crate::LocaleContext,
    pub command_prefix: String,
    pub command_text: String,
}

#[async_trait]
impl<S: Send + Sync> FromCommandContext<S> for EventSnapshot {
    async fn from_command_context(
        ctx: &CommandContext<'_>,
        _state: &S,
    ) -> std::result::Result<Self, CommandRejection> {
        Ok(Self {
            raw: ctx.event().raw().clone(),
            domain: ctx.domain_event().cloned(),
            locale: ctx.event().locale_context(),
            command_prefix: ctx.event().command_prefix().to_owned(),
            command_text: ctx.event().command_text().to_owned(),
        })
    }
}

/// 指令返回值转换 Trait（同时兼容 `anyhow::Result<()>`、`NuonoResult<()>` 与 `Result<(), CommandRejection>`）。
pub trait IntoCommandOutcome {
    fn into_command_outcome(self) -> std::result::Result<(), CommandRejection>;
}

impl IntoCommandOutcome for () {
    fn into_command_outcome(self) -> std::result::Result<(), CommandRejection> {
        Ok(())
    }
}

impl IntoCommandOutcome for anyhow::Result<()> {
    fn into_command_outcome(self) -> std::result::Result<(), CommandRejection> {
        self.map_err(CommandRejection::Error)
    }
}

impl IntoCommandOutcome for crate::NuonoResult<()> {
    fn into_command_outcome(self) -> std::result::Result<(), CommandRejection> {
        self.map_err(|e| CommandRejection::Error(e.into()))
    }
}

impl IntoCommandOutcome for std::result::Result<(), CommandRejection> {
    fn into_command_outcome(self) -> std::result::Result<(), CommandRejection> {
        self
    }
}

/// 指令前置守卫 Trait（可挂载在 [`CommandGroup`] 上，对该分组下全部指令生效）。
#[async_trait]
pub trait CommandGuard<S: Send + Sync>: Send + Sync {
    async fn check(
        &self,
        ctx: &CommandContext<'_>,
        state: &S,
    ) -> std::result::Result<(), CommandRejection>;
}

/// 将异步闭包包装为 [`CommandGuard`]。
pub struct FnGuard<F>(F);

impl<F> std::fmt::Debug for FnGuard<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FnGuard").finish_non_exhaustive()
    }
}

/// 从异步函数或闭包构造 [`CommandGuard`]。
pub fn guard_fn<S, F, Fut>(f: F) -> FnGuard<F>
where
    S: Send + Sync,
    F: for<'a> Fn(&'a CommandContext<'a>, &'a S) -> Fut + Send + Sync,
    Fut: Future<Output = std::result::Result<(), CommandRejection>> + Send,
{
    FnGuard(f)
}

#[async_trait]
impl<S, F, Fut> CommandGuard<S> for FnGuard<F>
where
    S: Send + Sync,
    F: for<'a> Fn(&'a CommandContext<'a>, &'a S) -> Fut + Send + Sync,
    Fut: Future<Output = std::result::Result<(), CommandRejection>> + Send,
{
    async fn check(
        &self,
        ctx: &CommandContext<'_>,
        state: &S,
    ) -> std::result::Result<(), CommandRejection> {
        (self.0)(ctx, state).await
    }
}

/// 类型安全指令处理器 Trait，自动为 0–6 个提取器参数的异步函数实现。
pub trait TypedCommandHandler<S: Send + Sync, Extractors>: Clone + Send + Sync + 'static {
    fn invoke<'a>(
        &'a self,
        ctx: &'a CommandContext<'a>,
        state: &'a S,
    ) -> BoxFuture<'a, std::result::Result<(), CommandRejection>>;
}

impl<S, F, Fut, Out> TypedCommandHandler<S, ()> for F
where
    S: Send + Sync + 'static,
    F: Fn() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Out> + Send + 'static,
    Out: IntoCommandOutcome,
{
    fn invoke<'a>(
        &'a self,
        _ctx: &'a CommandContext<'a>,
        _state: &'a S,
    ) -> BoxFuture<'a, std::result::Result<(), CommandRejection>> {
        let fut = (self)();
        Box::pin(async move { fut.await.into_command_outcome() })
    }
}

macro_rules! impl_typed_command_handler {
    ($($ty:ident),+) => {
        impl<S, F, Fut, Out, $($ty),+> TypedCommandHandler<S, ($($ty,)+)> for F
        where
            S: Send + Sync + 'static,
            F: Fn($($ty),+) -> Fut + Clone + Send + Sync + 'static,
            Fut: Future<Output = Out> + Send + 'static,
            Out: IntoCommandOutcome,
            $($ty: FromCommandContext<S> + 'static,)+
        {
            #[expect(non_snake_case, reason = "宏复用泛型类型名作为提取后的局部变量名")]
            fn invoke<'a>(
                &'a self,
                ctx: &'a CommandContext<'a>,
                state: &'a S,
            ) -> BoxFuture<'a, std::result::Result<(), CommandRejection>> {
                Box::pin(async move {
                    $(
                        let $ty = $ty::from_command_context(ctx, state).await?;
                    )+
                    (self)($($ty),+).await.into_command_outcome()
                })
            }
        }
    };
}

impl_typed_command_handler!(T1);
impl_typed_command_handler!(T1, T2);
impl_typed_command_handler!(T1, T2, T3);
impl_typed_command_handler!(T1, T2, T3, T4);
impl_typed_command_handler!(T1, T2, T3, T4, T5);
impl_typed_command_handler!(T1, T2, T3, T4, T5, T6);

struct ExtractedCommandHandler<S, H, E> {
    state: S,
    guards: Vec<Arc<dyn CommandGuard<S>>>,
    handler: H,
    _marker: std::marker::PhantomData<fn() -> E>,
}

#[async_trait]
impl<S, H, E> CommandHandler for ExtractedCommandHandler<S, H, E>
where
    S: Send + Sync + 'static,
    H: TypedCommandHandler<S, E>,
    E: Send + Sync + 'static,
{
    async fn handle(&self, bot: &Bot, event: &Event<'_>, data: MatchData) -> anyhow::Result<()> {
        let ctx = CommandContext::new(bot, event, &data);
        for guard in &self.guards {
            if let Err(rejection) = guard.check(&ctx, &self.state).await {
                return handle_rejection(bot, rejection).await;
            }
        }
        match self.handler.invoke(&ctx, &self.state).await {
            Ok(()) => Ok(()),
            Err(rejection) => handle_rejection(bot, rejection).await,
        }
    }
}

async fn handle_rejection(bot: &Bot, rejection: CommandRejection) -> anyhow::Result<()> {
    match rejection {
        CommandRejection::Reply(message) => {
            bot.send(message).await?;
            Ok(())
        }
        CommandRejection::Silent => Ok(()),
        CommandRejection::Error(error) => Err(error),
    }
}

/// 声明式指令与跨平台触发规则分组构造器：共享状态、服务分组、默认权限与前置守卫。
pub struct CommandGroup<S = ()> {
    state: S,
    sv_name: Option<String>,
    default_permission: PermissionLevel,
    guards: Vec<Arc<dyn CommandGuard<S>>>,
    commands: Vec<Command>,
    triggers: Vec<Trigger>,
}

impl<S> std::fmt::Debug for CommandGroup<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandGroup")
            .field("sv_name", &self.sv_name)
            .field("default_permission", &self.default_permission)
            .field("guard_count", &self.guards.len())
            .field("command_count", &self.commands.len())
            .field("trigger_count", &self.triggers.len())
            .finish_non_exhaustive()
    }
}

impl Default for CommandGroup<()> {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandGroup<()> {
    /// 创建无状态指令分组。
    pub fn new() -> Self {
        Self::with_state(())
    }
}

impl<S: Clone + Send + Sync + 'static> CommandGroup<S> {
    /// 创建共享状态 `S` 的指令分组。
    pub fn with_state(state: S) -> Self {
        Self {
            state,
            sv_name: None,
            default_permission: PermissionLevel::Everyone,
            guards: Vec::new(),
            commands: Vec::new(),
            triggers: Vec::new(),
        }
    }

    /// 为本分组下所有后续指令设置服务分组名（SV）。
    pub fn sv(mut self, name: impl Into<String>) -> Self {
        self.sv_name = Some(name.into());
        self
    }

    /// 为本分组下所有后续指令设置默认权限等级。
    pub fn permission(mut self, level: PermissionLevel) -> Self {
        self.default_permission = level;
        self
    }

    /// 挂载前置守卫（按添加顺序依次执行）。
    pub fn guard(mut self, guard: impl CommandGuard<S> + 'static) -> Self {
        self.guards.push(Arc::new(guard));
        self
    }

    /// 注册一条指令。
    pub fn command<H, E>(self, pattern: impl Into<String>, handler: H) -> Self
    where
        H: TypedCommandHandler<S, E>,
        E: Send + Sync + 'static,
    {
        self.command_with(pattern, handler, |cmd| cmd)
    }

    /// 注册一条指令并通过回调自定义别名、描述、用法、权限或匹配方式。
    pub fn command_with<H, E>(
        mut self,
        pattern: impl Into<String>,
        handler: H,
        configure: impl FnOnce(Command) -> Command,
    ) -> Self
    where
        H: TypedCommandHandler<S, E>,
        E: Send + Sync + 'static,
    {
        let wrapped = Arc::new(ExtractedCommandHandler {
            state: self.state.clone(),
            guards: self.guards.clone(),
            handler,
            _marker: std::marker::PhantomData,
        });
        let mut command = Command::new(pattern, wrapped).with_permission(self.default_permission);
        if let Some(sv) = &self.sv_name {
            command = command.with_sv(sv.clone());
        }
        self.commands.push(configure(command));
        self
    }

    /// 注册跨平台非指令触发规则（支持关键词、正则、前缀、后缀、完全匹配、文件或任意消息）。
    pub fn trigger_with<H, E>(
        mut self,
        trigger_type: TriggerType,
        pattern: impl Into<String>,
        handler: H,
        configure: impl FnOnce(Trigger) -> Trigger,
    ) -> Self
    where
        H: TypedCommandHandler<S, E>,
        E: Send + Sync + 'static,
    {
        let wrapped = Arc::new(ExtractedCommandHandler {
            state: self.state.clone(),
            guards: self.guards.clone(),
            handler,
            _marker: std::marker::PhantomData,
        });
        let mut trigger = Trigger::new_event(trigger_type, pattern, false, false, wrapped)
            .with_permission(self.default_permission);
        if let Some(sv) = &self.sv_name {
            trigger = trigger.with_sv_name(sv.clone());
        }
        self.triggers.push(configure(trigger));
        self
    }

    /// 注册跨平台关键词触发规则（默认不阻断）。
    pub fn keyword<H, E>(self, pattern: impl Into<String>, handler: H) -> Self
    where
        H: TypedCommandHandler<S, E>,
        E: Send + Sync + 'static,
    {
        self.trigger_with(TriggerType::Keyword, pattern, handler, |t| t)
    }

    /// 注册跨平台正则触发规则（捕获组可通过 `MatchData` 读取，默认不阻断）。
    pub fn regex<H, E>(self, pattern: impl Into<String>, handler: H) -> Self
    where
        H: TypedCommandHandler<S, E>,
        E: Send + Sync + 'static,
    {
        self.trigger_with(TriggerType::Regex, pattern, handler, |t| t)
    }

    /// 注册跨平台前缀触发规则。
    pub fn prefix<H, E>(self, pattern: impl Into<String>, handler: H) -> Self
    where
        H: TypedCommandHandler<S, E>,
        E: Send + Sync + 'static,
    {
        self.trigger_with(TriggerType::Prefix, pattern, handler, |t| t)
    }

    /// 注册跨平台后缀触发规则。
    pub fn suffix<H, E>(self, pattern: impl Into<String>, handler: H) -> Self
    where
        H: TypedCommandHandler<S, E>,
        E: Send + Sync + 'static,
    {
        self.trigger_with(TriggerType::Suffix, pattern, handler, |t| t)
    }

    /// 注册跨平台完全匹配触发规则。
    pub fn fullmatch<H, E>(self, pattern: impl Into<String>, handler: H) -> Self
    where
        H: TypedCommandHandler<S, E>,
        E: Send + Sync + 'static,
    {
        self.trigger_with(TriggerType::FullMatch, pattern, handler, |t| t)
    }

    /// 注册跨平台任意消息触发规则。
    pub fn on_message<H, E>(self, handler: H) -> Self
    where
        H: TypedCommandHandler<S, E>,
        E: Send + Sync + 'static,
    {
        self.trigger_with(TriggerType::Message, "", handler, |t| t)
    }

    /// 合并另一个分组生成的指令列表。
    pub fn extend(mut self, commands: impl IntoIterator<Item = Command>) -> Self {
        self.commands.extend(commands);
        self
    }

    /// 合并另一个分组生成的跨平台触发规则列表。
    pub fn extend_triggers(mut self, triggers: impl IntoIterator<Item = Trigger>) -> Self {
        self.triggers.extend(triggers);
        self
    }

    /// 产出所有注册完成的 [`Command`]。
    pub fn build(self) -> Vec<Command> {
        self.commands
    }

    /// 产出所有注册完成的跨平台 [`Trigger`]。
    pub fn build_triggers(self) -> Vec<Trigger> {
        self.triggers
    }

    /// 同时产出全部指令与跨平台触发规则（便于在 `PluginFactory::mount` 中一次性挂载）。
    pub fn build_parts(self) -> (Vec<Command>, Vec<Trigger>) {
        (self.commands, self.triggers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::WaitManager;
    use crate::core::event_bus::EventBus;
    use crate::core::onebot_client::ApiClient;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Default)]
    struct TestState {
        calls: Arc<AtomicUsize>,
    }

    struct MustHaveArg;

    #[async_trait]
    impl FromCommandContext<TestState> for MustHaveArg {
        async fn from_command_context(
            ctx: &CommandContext<'_>,
            _state: &TestState,
        ) -> std::result::Result<Self, CommandRejection> {
            if ctx.args_str().is_empty() {
                Err(CommandRejection::Silent)
            } else {
                Ok(Self)
            }
        }
    }

    #[tokio::test]
    async fn command_group_invokes_extractors_and_guards() {
        async fn handler(
            CmdState(state): CmdState<TestState>,
            _must: MustHaveArg,
            SenderId(sender): SenderId,
            ParsedArg(num): ParsedArg<i64>,
        ) -> anyhow::Result<()> {
            assert_eq!(sender, "10001");
            assert_eq!(num, 42);
            state.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        let state = TestState::default();
        let commands = CommandGroup::with_state(state.clone())
            .sv("demo")
            .command_with("calc", handler, |cmd| cmd.with_aliases(&["c"]))
            .build();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].sv_name.as_deref(), Some("demo"));
        assert_eq!(commands[0].aliases, vec!["c"]);

        let bot = Bot::new(
            1,
            Arc::new(EventBus::new()),
            Arc::new(WaitManager::new()),
            Arc::new(ApiClient::new()),
            None,
            None,
        );
        let raw = json!({
            "post_type": "message",
            "message_type": "private",
            "user_id": 10001,
            "raw_message": "calc 42"
        });
        let event = Event::new(&raw);

        // 参数为空时静默拒绝执行
        commands[0]
            .handler
            .as_ref()
            .expect("command handler")
            .handle(&bot, &event, MatchData::new("calc".into()))
            .await
            .unwrap();
        assert_eq!(state.calls.load(Ordering::SeqCst), 0);

        // 参数为 "42" 时正常执行
        commands[0]
            .handler
            .as_ref()
            .expect("command handler")
            .handle(
                &bot,
                &event,
                MatchData::new("calc 42".into()).with_args("42".into()),
            )
            .await
            .unwrap();
        assert_eq!(state.calls.load(Ordering::SeqCst), 1);
    }
}
