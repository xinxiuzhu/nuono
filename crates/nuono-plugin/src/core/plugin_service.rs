use crate::core::{
    ActorPermission, PermissionLevel, PluginConfigField, Trigger, TriggerHandler, TriggerRegistry,
    TriggerType,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

/// 插件所在区域 — 对齐 gsuid_core 的 area 配置
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PluginArea {
    /// 仅群聊
    Group,
    /// 仅私聊
    Direct,
    /// 所有类型
    #[default]
    All,
    /// 跟随插件级别设置
    FollowPlugin,
}

impl PluginArea {
    /// 解析宽松的旧配置值；未识别值视为跟随插件级别设置。
    pub fn parse_lossy(s: &str) -> Self {
        match s.to_uppercase().as_str() {
            "GROUP" => Self::Group,
            "DIRECT" | "PRIVATE" => Self::Direct,
            "ALL" => Self::All,
            _ => Self::FollowPlugin,
        }
    }
}

impl std::str::FromStr for PluginArea {
    type Err = !;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::parse_lossy(s))
    }
}

/// Plugins — 插件级服务设置容器。
///
/// 统一管理插件级别的命令前缀、最低权限门槛、作用区域与黑白名单。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plugins {
    pub name: String,
    /// 调用本插件所需的最低聊天权限等级（默认 `Everyone`）。
    #[serde(default)]
    pub min_permission: PermissionLevel,
    /// 是否启用
    pub enabled: bool,
    /// 作用区域
    pub area: PluginArea,
    /// 黑名单（用户ID或群组ID）
    pub black_list: Vec<String>,
    /// 白名单（如果非空，则只允许白名单中的用户/群组）
    pub white_list: Vec<String>,
    /// 命令入口前缀；命令、规则和 on_event 共用，自动监听独立声明。
    pub prefix: Vec<String>,
    /// 自动消息监听默认开关；None 表示没有声明监听能力，由宿主校准。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_listen_enabled: Option<bool>,
    /// 强制前缀列表（无论用户输入什么前缀，都使用这些前缀）
    pub force_prefix: Vec<String>,
    /// 是否禁用强制前缀
    pub disable_force_prefix: bool,
    /// 旧配置兼容字段；不能绕过非空插件前缀。
    pub allow_empty_prefix: bool,
    /// 插件别名
    pub alias: Vec<String>,
    /// 配置字段定义
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub config_fields: Vec<PluginConfigField>,
}

impl Default for Plugins {
    fn default() -> Self {
        Self {
            name: String::new(),
            min_permission: PermissionLevel::Everyone,
            enabled: true,
            area: PluginArea::FollowPlugin,
            black_list: Vec::new(),
            white_list: Vec::new(),
            prefix: Vec::new(),
            auto_listen_enabled: None,
            force_prefix: Vec::new(),
            disable_force_prefix: false,
            allow_empty_prefix: false,
            alias: Vec::new(),
            config_fields: Vec::new(),
        }
    }
}

impl Plugins {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }

    /// 设置调用本插件所需的最低聊天权限等级。
    pub fn with_min_permission(mut self, level: PermissionLevel) -> Self {
        self.min_permission = level;
        self
    }
    pub fn with_prefix(mut self, prefix: Vec<String>) -> Self {
        self.prefix = prefix;
        self
    }
    /// 设置已声明自动监听入口的默认开关；不授予额外权限或绕过插件总开关。
    pub fn with_auto_listen_enabled(mut self, enabled: bool) -> Self {
        self.auto_listen_enabled = Some(enabled);
        self
    }
    pub fn with_force_prefix(mut self, fp: Vec<String>) -> Self {
        self.force_prefix = fp;
        self
    }
    pub fn with_area(mut self, area: PluginArea) -> Self {
        self.area = area;
        self
    }
    pub fn with_black_list(mut self, list: Vec<String>) -> Self {
        self.black_list = list;
        self
    }
    pub fn with_white_list(mut self, list: Vec<String>) -> Self {
        self.white_list = list;
        self
    }
    pub fn with_alias(mut self, alias: Vec<String>) -> Self {
        self.alias = alias;
        self
    }

    /// 获取所有生效的 prefix（包含 force_prefix）
    pub fn effective_prefixes(&self) -> Vec<String> {
        let mut prefixes = self.prefix.clone();
        if !self.disable_force_prefix {
            prefixes.extend(self.force_prefix.clone());
        }
        if self.allow_empty_prefix {
            prefixes.push(String::new());
        }
        prefixes.dedup();
        prefixes
    }

    /// 匹配插件消息入口，返回去掉前缀的正文。
    ///
    /// 非空前缀存在时必须命中，旧 `allow_empty_prefix` 不再提供旁路。
    /// 无非空前缀的插件可以接收普通消息；跨插件归属由宿主在调用前统一决定。
    /// 前缀按最长优先、ASCII 大小写不敏感匹配，正文大小写原样保留。
    pub fn message_text<'a>(&self, text: &'a str) -> Option<&'a str> {
        let text = text.trim();
        let prefixes = self.effective_prefixes();
        let mut has_prefix = false;
        let mut matched_len = None;
        for prefix in prefixes.iter().filter(|prefix| !prefix.is_empty()) {
            has_prefix = true;
            if text
                .get(..prefix.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
            {
                matched_len = Some(matched_len.unwrap_or(0).max(prefix.len()));
            }
        }
        match matched_len {
            Some(len) => Some(text[len..].trim()),
            None if !has_prefix => Some(text),
            None => None,
        }
    }

    /// 检查参与者是否满足此插件的最低权限门槛（封禁态一律拒绝）。
    pub fn check_permission(&self, actor: ActorPermission) -> bool {
        actor.has_permission(self.min_permission)
    }

    /// 检查用户/群组是否在黑白名单中
    pub fn check_access(&self, user_id: &str, group_id: Option<&str>) -> bool {
        let contains_user_or_group = |list: &[String]| {
            list.iter()
                .any(|id| id == user_id || group_id == Some(id.as_str()))
        };
        if contains_user_or_group(&self.black_list) {
            return false;
        }
        self.white_list.is_empty() || contains_user_or_group(&self.white_list)
    }

    /// 检查区域是否匹配
    pub fn check_area(&self, msg_type: &str) -> bool {
        match self.area {
            PluginArea::Group => msg_type == "group",
            PluginArea::Direct => msg_type == "private",
            PluginArea::All => true,
            PluginArea::FollowPlugin => true, // 由上层决定
        }
    }
}

/// Service — 服务容器，一组相关触发器的集合
///
/// 对应 gsuid_core 的 SV 类。一个 SV 代表一个插件服务，
/// 可以注册多个触发器（命令/前缀/关键词/正则/完全匹配/任意消息）。
///
/// # 示例
/// 插件入口前缀在 `Plugins` 中声明；SV 只声明去掉入口前缀后的业务命令。
///
/// ```rust
/// use nuono_plugin::{async_trait, Bot, MatchData, Message, Plugins, Result, SV,
///     TriggerHandler, TriggerRegistry, TriggerType};
/// use std::sync::Arc;
/// # struct WeatherHandler;
/// # #[async_trait]
/// # impl TriggerHandler for WeatherHandler {
/// #     async fn handle(&self, _bot: &Bot, _event: &Message, _data: MatchData) -> Result<()> {
/// #         Ok(())
/// #     }
/// # }
/// let plugin = Arc::new(Plugins::new("Weather").with_prefix(vec!["weather".into()]));
/// let registry = TriggerRegistry::new();
/// let handler = Arc::new(WeatherHandler);
/// let sv = SV::new("天气")
///     .with_plugin(plugin.clone())
///     .command("天气", handler.clone())
///     .command("weather", handler.clone())
///     .keyword("天气", handler);
/// sv.register(&registry);
/// # assert_eq!(plugin.message_text("weather天气"), Some("天气"));
/// # assert_eq!(registry.count(), 3);
/// # assert_eq!(registry.count_by_type(TriggerType::Command), 2);
/// # assert_eq!(registry.count_by_type(TriggerType::Keyword), 1);
/// # assert!(registry.get_all_triggers().iter().all(|trigger| {
/// #     trigger.plugin_name.as_deref() == Some("Weather")
/// #         && trigger.sv_name.as_deref() == Some("天气")
/// # }));
/// ```
#[derive(Debug)]
pub struct SV {
    pub name: String,
    pub triggers: Vec<Trigger>,
    pub prefix: String,
    pub to_me: bool,
    pub block: bool,
    pub permission_level: Option<PermissionLevel>,
    /// 所属插件（关联到 Plugins）
    pub plugin: Option<Arc<Plugins>>,
    /// 服务内规则的优先级；带前缀规则优先，再比较此值，数字越小越先执行。
    pub priority: Option<i32>,
    /// SV 级别的区域（覆盖插件级别）
    pub area: Option<PluginArea>,
    /// SV 级别的黑名单
    pub black_list: Vec<String>,
    /// SV 级别的白名单
    pub white_list: Vec<String>,
}

impl SV {
    /// 创建新的服务
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            triggers: Vec::new(),
            prefix: String::new(),
            to_me: false,
            block: false,
            permission_level: None,
            plugin: None,
            priority: None,
            area: None,
            black_list: Vec::new(),
            white_list: Vec::new(),
        }
    }

    /// 设置所属插件
    pub fn with_plugin(mut self, plugin: Arc<Plugins>) -> Self {
        self.plugin = Some(plugin);
        self
    }

    /// 设置统一前缀（如 "/"）
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// 设置是否需要 @机器人
    pub fn to_me(mut self, to_me: bool) -> Self {
        self.to_me = to_me;
        self
    }

    /// 设置是否阻断后续触发器
    pub fn block(mut self, block: bool) -> Self {
        self.block = block;
        self
    }

    /// 设置最低权限等级
    pub fn permission(mut self, level: PermissionLevel) -> Self {
        self.permission_level = Some(level);
        self
    }

    /// 设置服务内规则的优先级（带前缀规则优先，数字越小越先执行）。
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = Some(priority);
        self
    }

    /// 设置 SV 级别的区域
    pub fn with_area(mut self, area: PluginArea) -> Self {
        self.area = Some(area);
        self
    }

    /// 设置 SV 级别的黑名单
    pub fn with_black_list(mut self, list: Vec<String>) -> Self {
        self.black_list = list;
        self
    }

    /// 设置 SV 级别的白名单
    pub fn with_white_list(mut self, list: Vec<String>) -> Self {
        self.white_list = list;
        self
    }

    /// 注册命令触发器（前缀+命令）
    pub fn command(mut self, cmd: impl Into<String>, handler: Arc<dyn TriggerHandler>) -> Self {
        let mut trigger = Trigger::new(
            TriggerType::Command,
            cmd,
            &self.prefix,
            self.block,
            self.to_me,
            handler,
        );
        trigger.priority = self.priority.unwrap_or(5);
        self.triggers.push(trigger);
        self
    }

    /// 注册前缀触发器
    pub fn prefix_trigger(
        mut self,
        pfx: impl Into<String>,
        handler: Arc<dyn TriggerHandler>,
    ) -> Self {
        let mut trigger = Trigger::new(
            TriggerType::Prefix,
            pfx,
            &self.prefix,
            self.block,
            self.to_me,
            handler,
        );
        trigger.priority = self.priority.unwrap_or(5);
        self.triggers.push(trigger);
        self
    }

    /// 注册关键词触发器
    pub fn keyword(mut self, kw: impl Into<String>, handler: Arc<dyn TriggerHandler>) -> Self {
        let mut trigger = Trigger::new(
            TriggerType::Keyword,
            kw,
            &self.prefix,
            self.block,
            self.to_me,
            handler,
        );
        trigger.priority = self.priority.unwrap_or(5);
        self.triggers.push(trigger);
        self
    }

    /// 注册正则触发器
    pub fn regex(mut self, pattern: impl Into<String>, handler: Arc<dyn TriggerHandler>) -> Self {
        let mut trigger = Trigger::new(
            TriggerType::Regex,
            pattern,
            &self.prefix,
            self.block,
            self.to_me,
            handler,
        );
        trigger.priority = self.priority.unwrap_or(5);
        self.triggers.push(trigger);
        self
    }

    /// 注册完全匹配触发器
    pub fn fullmatch(
        mut self,
        pattern: impl Into<String>,
        handler: Arc<dyn TriggerHandler>,
    ) -> Self {
        let mut trigger = Trigger::new(
            TriggerType::FullMatch,
            pattern,
            &self.prefix,
            self.block,
            self.to_me,
            handler,
        );
        trigger.priority = self.priority.unwrap_or(5);
        self.triggers.push(trigger);
        self
    }

    /// 注册任意消息触发器（匹配所有消息）
    pub fn on_message(mut self, handler: Arc<dyn TriggerHandler>) -> Self {
        let mut trigger = Trigger::new(
            TriggerType::Message,
            "",
            &self.prefix,
            self.block,
            self.to_me,
            handler,
        );
        trigger.priority = self.priority.unwrap_or(5);
        self.triggers.push(trigger);
        self
    }

    /// 注册后缀触发器
    pub fn suffix(mut self, sfx: impl Into<String>, handler: Arc<dyn TriggerHandler>) -> Self {
        let mut trigger = Trigger::new(
            TriggerType::Suffix,
            sfx,
            &self.prefix,
            self.block,
            self.to_me,
            handler,
        );
        trigger.priority = self.priority.unwrap_or(5);
        self.triggers.push(trigger);
        self
    }

    /// 注册文件类型触发器
    pub fn file(mut self, file_type: impl Into<String>, handler: Arc<dyn TriggerHandler>) -> Self {
        let mut trigger = Trigger::new(
            TriggerType::File,
            file_type,
            &self.prefix,
            self.block,
            self.to_me,
            handler,
        );
        trigger.priority = self.priority.unwrap_or(5);
        self.triggers.push(trigger);
        self
    }

    /// 将 SV 声明转换为触发器列表，由宿主统一注册。
    pub fn into_triggers(self) -> Vec<Trigger> {
        let plugin_name = self.plugin.as_ref().map(|p| p.name.clone());
        let sv_name = self.name.clone();
        self.triggers
            .into_iter()
            .map(|mut trigger| {
                trigger.plugin_name = plugin_name.clone();
                trigger.sv_name = Some(sv_name.clone());
                trigger
            })
            .collect()
    }

    /// 将所有触发器注册到 TriggerRegistry。
    ///
    /// 新插件更推荐在 `Plugin::triggers()` 中返回 `into_triggers()` 的结果，
    /// 这样动态插件只声明业务触发器，注册过程由宿主统一完成。
    pub fn register(self, registry: &TriggerRegistry) {
        info!(sv = %self.name, triggers = self.triggers.len(), "注册服务");
        for trigger in self.into_triggers() {
            registry.register(trigger);
        }
    }

    /// 返回触发器数量
    pub fn len(&self) -> usize {
        self.triggers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.triggers.is_empty()
    }
}

impl Default for SV {
    fn default() -> Self {
        Self::new("default")
    }
}

/// SVList — 全局服务注册表，对应 gsuid_core 的 SVList
///
/// 管理所有已注册的 Plugins 和 SV 实例，
/// 提供统一的查询和管理接口。
#[derive(Debug)]
pub struct SVList {
    /// 所有已注册的 SV 实例
    pub lst: RwLock<HashMap<String, Arc<SV>>>,
    /// 所有已注册的 Plugins 实例
    pub plugins: RwLock<HashMap<String, Arc<Plugins>>>,
    /// Plugins -> SV 的映射关系
    pub detail_lst: RwLock<HashMap<String, Vec<String>>>,
}

impl SVList {
    pub fn new() -> Self {
        Self {
            lst: RwLock::new(HashMap::new()),
            plugins: RwLock::new(HashMap::new()),
            detail_lst: RwLock::new(HashMap::new()),
        }
    }

    /// 注册一个 SV 实例
    pub async fn register_sv(&self, sv: SV) {
        let name = sv.name.clone();
        let plugin_name = sv.plugin.as_ref().map(|p| p.name.clone());
        let sv_arc = Arc::new(sv);

        self.lst.write().await.insert(name.clone(), sv_arc);

        if let Some(pname) = plugin_name {
            let mut detail = self.detail_lst.write().await;
            let sv_list = detail.entry(pname).or_insert_with(Vec::new);
            if !sv_list.contains(&name) {
                sv_list.push(name);
            }
        }
    }

    /// 注册一个 Plugins 实例
    pub async fn register_plugin(&self, plugin: Plugins) {
        let name = plugin.name.clone();
        let plugin_arc = Arc::new(plugin);
        self.plugins.write().await.insert(name, plugin_arc);
    }

    /// 获取 SV 实例
    pub async fn get_sv(&self, name: &str) -> Option<Arc<SV>> {
        self.lst.read().await.get(name).cloned()
    }

    /// 获取 Plugins 实例
    pub async fn get_plugin(&self, name: &str) -> Option<Arc<Plugins>> {
        self.plugins.read().await.get(name).cloned()
    }

    /// 获取插件下的所有 SV 名称
    pub async fn get_plugin_svs(&self, plugin_name: &str) -> Vec<String> {
        self.detail_lst
            .read()
            .await
            .get(plugin_name)
            .cloned()
            .unwrap_or_default()
    }

    /// 获取所有插件名称
    pub async fn list_plugins(&self) -> Vec<String> {
        self.plugins.read().await.keys().cloned().collect()
    }

    /// 获取所有 SV 名称
    pub async fn list_svs(&self) -> Vec<String> {
        self.lst.read().await.keys().cloned().collect()
    }

    /// 检查 SV 是否存在
    pub async fn has_sv(&self, name: &str) -> bool {
        self.lst.read().await.contains_key(name)
    }

    /// 检查 Plugin 是否存在
    pub async fn has_plugin(&self, name: &str) -> bool {
        self.plugins.read().await.contains_key(name)
    }

    /// 移除 SV
    pub async fn remove_sv(&self, name: &str) -> Option<Arc<SV>> {
        let sv = self.lst.write().await.remove(name);
        if let Some(ref sv) = sv
            && let Some(ref plugin_name) = sv.plugin
        {
            let mut detail = self.detail_lst.write().await;
            if let Some(svs) = detail.get_mut(&plugin_name.name) {
                svs.retain(|n| n != name);
            }
        }
        sv
    }

    /// 移除 Plugin（同时移除其下的所有 SV）
    pub async fn remove_plugin(&self, name: &str) -> Option<Arc<Plugins>> {
        let plugin = self.plugins.write().await.remove(name);
        if plugin.is_some() {
            let sv_names = self.get_plugin_svs(name).await;
            let mut lst = self.lst.write().await;
            for sv_name in &sv_names {
                lst.remove(sv_name);
            }
            self.detail_lst.write().await.remove(name);
        }
        plugin
    }

    /// 统计信息
    pub async fn stats(&self) -> (usize, usize) {
        let plugins = self.plugins.read().await.len();
        let svs = self.lst.read().await.len();
        (plugins, svs)
    }
}

impl Default for SVList {
    fn default() -> Self {
        Self::new()
    }
}

/// 全局 SVList 单例
static SVLIST: once_cell::sync::Lazy<Arc<SVList>> =
    once_cell::sync::Lazy::new(|| Arc::new(SVList::new()));

/// 获取全局 SVList 实例
pub fn global_svlist() -> Arc<SVList> {
    SVLIST.clone()
}

#[cfg(test)]
mod tests {
    use super::{PluginArea, Plugins};

    #[test]
    fn access_lists_match_opaque_user_and_group_ids_with_blacklist_precedence() {
        for (black, white, user, group, allowed) in [
            (vec![], vec![], "user", None, true),
            (vec!["user"], vec![], "user", None, false),
            (vec!["group"], vec![], "user", Some("group"), false),
            (vec![], vec!["user"], "user", None, true),
            (vec![], vec!["group"], "user", Some("group"), true),
            (vec![], vec!["group"], "user", None, false),
            (vec![], vec!["other"], "user", Some("group"), false),
            (vec!["user"], vec!["group"], "user", Some("group"), false),
            (vec!["group"], vec!["user"], "user", Some("group"), false),
            (vec!["user"], vec!["user"], "user", None, false),
            (vec!["001"], vec![], "1", None, true),
            (vec![], vec!["001"], "1", None, false),
            (vec![], vec!["001"], "001", None, true),
            (vec!["group"], vec!["user"], "user", None, true),
        ] {
            let plugin = Plugins::new("access")
                .with_black_list(black.into_iter().map(str::to_owned).collect())
                .with_white_list(white.into_iter().map(str::to_owned).collect());
            assert_eq!(
                plugin.check_access(user, group),
                allowed,
                "black={:?}, white={:?}, user={user:?}, group={group:?}",
                plugin.black_list,
                plugin.white_list,
            );
        }
    }

    #[test]
    fn plugin_namespace_requires_longest_prefix_and_preserves_body() {
        let mut plugin =
            Plugins::new("links").with_prefix(vec!["u".into(), "url".into(), "".into()]);
        plugin.allow_empty_prefix = true;
        assert_eq!(
            plugin.message_text(" URLqq音乐登录 TokenAbC "),
            Some("qq音乐登录 TokenAbC")
        );
        assert_eq!(plugin.message_text("QQ音乐登录"), None);
        assert_eq!(plugin.message_text("url"), Some(""));
        assert_eq!(plugin.message_text("你好"), None);
        plugin.prefix = vec!["示例".into()];
        assert_eq!(plugin.message_text("示例查询 123"), Some("查询 123"));
        assert_eq!(plugin.message_text("示查询"), None);
    }

    #[test]
    fn additional_prefixes_and_unprefixed_plugins_share_the_same_gate() {
        let mut plugin = Plugins::new("p").with_force_prefix(vec!["ww".into()]);
        assert_eq!(plugin.message_text("查询"), None);
        assert_eq!(plugin.message_text("Ww查询"), Some("查询"));
        plugin.disable_force_prefix = true;
        assert_eq!(plugin.message_text(" 查询 "), Some("查询"));
        assert_eq!(plugin.message_text(""), Some(""));
    }

    #[test]
    fn standard_parsing_preserves_legacy_config_semantics() {
        for (value, expected) in [
            ("GROUP", PluginArea::Group),
            ("group", PluginArea::Group),
            ("GrOuP", PluginArea::Group),
            ("DIRECT", PluginArea::Direct),
            ("direct", PluginArea::Direct),
            ("PRIVATE", PluginArea::Direct),
            ("private", PluginArea::Direct),
            ("All", PluginArea::All),
            ("FOLLOW_PLUGIN", PluginArea::FollowPlugin),
            ("unknown", PluginArea::FollowPlugin),
            ("", PluginArea::FollowPlugin),
            (" group ", PluginArea::FollowPlugin),
            ("跟随插件", PluginArea::FollowPlugin),
        ] {
            assert_eq!(PluginArea::parse_lossy(value), expected);
            assert_eq!(value.parse::<PluginArea>().unwrap(), expected);
        }
    }
}
